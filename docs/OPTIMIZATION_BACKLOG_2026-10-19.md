# CapLite 后续优化空间（2026-10-19 第三轮复审）

> 范围：两批修复落地后的全代码复审。
> 目标不是“再加功能”，而是继续降低**录像热路径 CPU/内存带宽**、**编辑页打开/预览开销**、
> **空闲唤醒**和**构建/发布不一致**。
>
> **2026-10-19 后续落地**：本文第 1 项（光标隐藏时不拷贝整帧）、第 3 项（`Sws` 复用 +
> H.264 `lowres` 预览）、第 4 项（音频空闲等待 2/5 ms → 10 ms）已实现并通过
> `cargo test --lib` / `cargo clippy --all-targets`。第 2、5、6 项以及 Cache 默认目录、
> 文件拆分、硬件编码仍作为后续项保留。

## 一、优先级总表

| 优先级 | 优化点 | 预期收益 | 风险/成本 |
|---|---|---|---|
| P1 | 录像光标路径少一次全帧拷贝 | 4K 下每帧省 ~33 MB memcpy；1080p ~8 MB/帧 | 低，改 `capture.rs` 分支 |
| P1 | 编辑页“打开即全片扫描/波形”按需化 | 长视频打开/切页 CPU、磁盘明显下降 | 中，涉及交互语义 |
| P1 | 播放解码复用 `Sws` 上下文 / 预览低分辨率解码 | 预览 CPU 下降；4K 最明显 | 中，需验证画质/兼容性 |
| P2 | 音频空闲等待从 2/5 ms 放宽到 10–20 ms | 静音时唤醒次数降 2–10 倍 | 低，条件变量有数据即唤醒 |
| P2 | 预览帧队列 64 MB 预算自适应下调 | 4K 预览内存下降 | 低-中，需测卡顿 |
| P2 | 启动探测并行化/进一步延后 | 死 UNC 下启动最坏耗时下降 | 中，改启动编排 |
| P2 | 构建用 `ffmpeg-min` 与发布 DLL 对齐 | target/release 从“完整 DLL”变“5 个精简 DLL” | 中，需处理 `.lib` 位置并回归 |
| P3 | Cache 默认改 `%LOCALAPPDATA%` | 去掉 exe 目录 junction/ACL 风险 | 中，涉及旧 Cache 迁移 |
| P3 | 拆分 `main.rs` / `editor.rs` 巨型文件 | 增量编译、审查成本下降 | 低风险但工作量大 |
| 战略 | 硬件 H.264 编码 / 解码 helper 进程 | CPU 与安全性的量级改善 | 大，独立项目 |

---

## 已实施（2026-10-19 本轮）

- **录像光标路径**：`Capture::grab` 先读光标并判断是否与采集区域相交；隐藏/选区外时
  直接对 DXGI 的 `raw` 做 BGRA→I420，不再先拷贝整帧到 `scratch`。
- **音频空闲唤醒**：`TRANSPORT_IDLE_WAIT` 2 ms → 10 ms；消费线程静音等待 5 ms → 10 ms。
  数据到达仍由条件变量立即唤醒，只降低静音/空闲时的轮询。
- **预览 `Sws` 复用**：`convert_to_rgba` 用线程本地 `SWS_CACHE` 按
  `(像素格式, 源宽高, 目标宽高)` 复用 `SwsContext`，不再每帧 `sws_getContext`。
- **预览 lowres**：H.264 源在预览尺寸的 2×/4× 以上时设置 `AVCodecContext.lowres`
  （只影响 Trim 预览，不影响导出）；不支持的解码器会忽略。
- **验证**：`cargo test --lib` 396 passed；`cargo clippy --all-targets` 0 warning；
  debug GUI 热键开始/停止通过；release exe 重新签名并重打包（17 条目）。
- **未做性能对比基准**：以上是代码路径优化，仍需按第五节用 4K/1h 素材量 CPU/内存；
  第 2、5、6 项（编辑页按需扫描、预览队列、启动探测/FFMPEG_DIR 对齐）保持待办。

## 二、最值得先做的 6 项

### 1. 录像光标路径：没有光标时不要复制整帧

**证据**：[capture.rs](../src/video/capture.rs#L318-L356)

```rust
if self.draw_cursor {
    let scratch = ...;
    for y in 0..h {
        scratch[..].copy_from_slice(raw[..]); // 整帧 BGRA 拷贝
    }
    match self.cursor.read() { ... composite ... }
    convert::bgra_to_i420_cropped(scratch, ...)
} else {
    convert::bgra_to_i420_cropped(raw, ...)
}
```

当前是**先整帧拷贝，再读光标**。而光标经常是：

- 全屏视频/游戏里自动隐藏；
- 框选录制时在选区外；
- `read()` 返回 `None`。

这些情况下整帧拷贝完全白做。1080p BGRA 约 8.3 MB/帧，4K 约 33 MB/帧；30 fps 下是
250 MB/s ～ 1 GB/s 的纯内存带宽。

**建议**：先 `cursor.read()` / 判断 `frame_pos` 是否落在采集区域；只有真的要合成光标时
才拷贝到 `scratch`，否则直接把 `raw` 交给 `bgra_to_i420_cropped`。需要补一条
“光标隐藏/选区外”单测，验证输出与旧路径逐字节一致。

---

### 2. 编辑页打开：全片关键帧/健康扫描 + 4 线程全片波形

**证据**：

- `open_streaming` 打开后立刻 `scan_pass`：[edit/mod.rs](../src/edit/mod.rs#L516-L527)
- `scan_pass` 一直 `av_read_frame` 到 EOF：[health.rs](../src/edit/health.rs#L319-L390)
- 波形默认 4 个基准层 worker，把整片按 8 分钟分段并行解满：
  [editor.rs](../src/ui/editor.rs#L887-L1000)

这不是“打开后再慢慢算”那么简单：长视频（1 小时 4K）打开时，关键帧/健康扫描要读完整文件，
波形还要再解一遍完整音轨。它们都在后台线程，但仍会：

- 抢占正在录制的 CPU/磁盘；
- 让编辑页的“可交互”时间变长；
- 让笔记本风扇转起来。

**建议分两步**：

1. **关键帧/健康索引按需化**：打开时只扫前 N 秒（例如 30 s）建立首屏索引，用户拖到未索引
   区间时再增量扫描；或者先只扫关键帧，不做完整健康检查。
2. **波形默认只算可见窗口**：首屏可见区间优先，滚动/缩放时再补；长文件先弹一次
   “是否为整片生成波形？”或默认关闭整片基准层，只保留可见窗口。
3. 把 `clip-open` 的 `JoinHandle` 保留下来，换文件时置取消标志，避免旧文件扫描继续读盘。

这一项的收益通常比“再省一次 memcpy”更大，尤其是编辑长视频。

---

### 3. 播放预览：复用 `Sws` 上下文 / 低分辨率解码

**证据**：

- `convert_to_rgba` 每帧 `sws_getContext` + 分配 `Vec`：
  [edit/mod.rs](../src/edit/mod.rs#L1197-L1253)
- `player` 每帧调用 `convert_to_rgba(frame, max_w, max_h)`：
  [player.rs](../src/edit/player.rs#L381)

`convert_to_rgba` 已经把输出缩到预览尺寸，但：

- **解码仍是源分辨率**（4K 仍解 4K）；
- 每帧重新创建 `SwsContext`（`sws_getContext` 有初始化成本）；
- 每帧 `vec![0u8; stride*dh]` 新分配。

**建议**：

1. 在 `Player` 里缓存 `SwsContext`，用 `sws_getCachedContext` 或按
   `(src_format, fw, fh, dw, dh)` 复用；缩略图/其他路径也复用。
2. 预览尺寸明显小于源尺寸时，评估 H.264 `AVCodecContext.lowres`（1/2、1/4）解码，
   失败就回退全分辨率；只用于预览，不影响导出。
3. 帧缓冲池化（`Vec` 复用）可进一步减少分配；但要注意 `FrameRgba` 会移交 UI，
   池化需要回收协议，复杂度较高，先做 1、2。

---

### 4. 音频空闲唤醒：2 ms / 5 ms 偏激进

**证据**：

- 搬运线程空转等待 2 ms：[ring.rs](../src/audio/ring.rs#L44)、[ring.rs](../src/audio/ring.rs#L292)
- 消费线程静音时等 5 ms：[recorder.rs](../src/recorder.rs#L1053)
- 但 `wait_data` 在 `push` 时会立即唤醒，超时只用于“完全没数据”的场景。

所以把空闲超时放宽到 **10–20 ms** 不会增加有数据时的延迟，只会减少静音/空闲时的
context switch。按 2 路音源算，2 ms 搬运 + 5 ms 消费约 700 次唤醒/秒；10 ms 可降到
约 200 次/秒。

**建议**：`TRANSPORT_IDLE_WAIT` 2 ms → 10 ms；消费线程静音等待 5 ms → 10 ms；
停止响应最多多 10 ms，可接受。用 Process Explorer/ETW 量一下静音 10 分钟的
CPU 时间与 context switch 再定。

---

### 5. 预览队列 64 MB 预算可自适应

**证据**：[player.rs](../src/edit/player.rs#L38-L53)

```rust
const QUEUE_CAP: usize = 8;
const QUEUE_BYTES_BUDGET: usize = 64 * 1024 * 1024;
```

1080p 仍是 8 帧（约 66 MB），4K 降到 2 帧（约 66 MB）。这保证了大文件播放平滑，
但空闲/低分辨率时也占着预算。

**建议**：

- 1080p 可降到 4 帧/32 MB，4K 保持 2 帧；用播放卡顿率/丢帧率做 A/B；
- 或按可用物理内存动态调整（低内存机器 32 MB，高内存 64 MB）；
- 不要小于 2 帧，否则 seek/解码抖动会直接黑帧。

---

### 6. 启动探测并行化 / 构建与发布 FFmpeg 对齐

**启动探测**：`ConfigStore::detect`、`plan_output_dir`、`cache::resolve_cache_dir`
现在都有 1.5 s 超时，但仍是**串行**探测多个候选。断开的 UNC 下最坏可能是
N × 1.5 s。可以把候选目录的探测并行（每个 1.5 s 上限），或者把输出/Cache 探测
延后到首帧之后；配置目录仍需最早确定。

**构建对齐**：当前 `_build-env*.bat` 的 `FFMPEG_DIR` 指向完整 `ffmpeg-n9`，但发布包
用的是 `ffmpeg-min` 的 5 个 DLL。结果是：

- `target/release` 会复制完整 FFmpeg DLL（体积大）；
- 发布包和本地构建实际加载的 DLL 不完全一致。

`ffmpeg-min` 的 include/库/5 个 DLL 已经覆盖本项目用到的 codec/format/swscale/
swresample/AAC，理论上可以作为构建 `FFMPEG_DIR`；但它的 MSVC `.lib` 在 `bin` 下，
`lib` 下只有 `.dll.a`，切换前需要把 `.lib` 放到 `lib`（或让构建脚本额外 `-L bin`），
并用回归脚本 + 真机启动验证。收益是 target 更小、发布/构建一致；不是运行时性能项。

---

## 三、其他值得记录的优化

### 7. Cache 默认目录

P0-3 已经拒绝 junction，并在不可用时退 `%LOCALAPPDATA%`；但默认仍是
`<exe>\Cache`（可写时）。如果要进一步降低“exe 目录 ACL / junction / 便携版被管理员运行”
的风险，可以把默认改成 `%LOCALAPPDATA%\CapLite\Cache`，把旧 Cache 作为只读迁移源。
这是安全/可维护性优化，不是 CPU 优化。

### 8. `main.rs` / `editor.rs` 拆分

`main.rs` 约 157 KB、`editor.rs` 约 1.3 万行。任何小改动都会重编译整个 crate 的最大模块，
增量构建时间被放大。按职责拆成 `app/`、`editor/` 子模块（waveform、picker、playback、
health、ui）可以降低审查和编译成本。风险低，但工作量大，适合单独一轮。

### 9. 录像线程的分配与队列

- `video_queue` 120 个编码包、`audio_queue` 400 个包；目前内存不大，不需要动。
- OpenH264 输出 `Nal` 到 AVCC 有一次二次拷贝；单帧只有几十 KB，收益有限，不建议优先做。
- `FrameRgba` 池化收益取决于预览队列大小，先做第 5 项再评估。

### 10. 硬件编码 / 解码 helper（战略项）

- 录像仍用 OpenH264 软件编码；4K/60 的 CPU 大头在这里。硬件 H.264（Media Foundation/
  NVENC/QSV）能显著降 CPU，但会引入 GPU 兼容、码控、LGPL/专利和回退复杂度，属于独立项目。
- 不可信媒体解码仍在主进程；helper 进程是安全项，不是性能项，但与低分辨率预览可以
  一起设计（helper 只回传预览尺寸帧）。

---

## 四、不建议再动的项

以下在之前已经测量/讨论过，不建议为了“优化”重新引入：

1. `strip = true`：之前实测体积收益很小，反而丢失符号；
2. 强 pin `WGPU_BACKEND=dx12`：本机实测更差；
3. Realtime 线程优先级：Windows 上有整机卡死风险；
4. 为了省内存把预览队列降到 1 帧：seek/解码抖动会直接黑帧；
5. 默认关闭波形：这是产品功能，应该让用户选择，而不是用性能理由硬关；
6. 为了快而在导出时跳过自校验：会把“偏短 MP4”重新放回来，违背之前的稳定性修复。

---

## 五、建议的测量方法

在做上面任何一项前，先固定一套可复现的测量：

1. **录像热路径**：4K30/4K60、光标显示/隐藏、区域/全屏各录 2 分钟；记录
   CPU 时间、峰值 WS、`GetProcessMemoryInfo`、ETW 的 memcpy 热点。
2. **编辑打开**：1 小时 4K 文件，记录“双击到可交互”、
   “波形完成”、“关键帧/健康扫描完成”三个时间点；分别在 `show_waveform=true/false` 下测。
3. **静音空闲**：录 10 分钟无系统声音，记录 context switch/s 与 CPU 时间。
4. **启动**：正常盘、断开的 UNC 输出目录、断开的 UNC 配置目录各 5 次，记录首帧时间。
5. **构建**：分别用 n9/ffmpeg-min 做 release，比较 target 体积、DLL 清单、回归 12/12。

---

## 六、建议顺序

1. 光标隐藏时不拷贝整帧（低风险，立即收益）；
2. 音频空闲等待放宽到 10 ms（低风险，静音 CPU）；
3. 编辑页波形/关键帧按需化（最大收益，需产品确认交互）；
4. `Sws` 复用 + 预览低分辨率解码（4K 预览收益）；
5. 预览队列自适应/32 MB（需 A/B）；
6. 启动探测并行 + FFMPEG_DIR 对齐（工程/发布收益）；
7. Cache 默认 LocalAppData、文件拆分、硬件编码（中期/战略）。
