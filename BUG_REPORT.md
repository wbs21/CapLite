# 代码审查报告 — rust-screen-recorder（CapLite）

审查对象：`CapLite/`（Rust + egui/eframe + Windows Graphics Capture + OpenH264 + libyuv + muxide）
审查方式：**逐行阅读 + 二进制级验证**。本机没有 Rust 工具链（无 `cargo`/`rustc`），因此无法 `cargo check`；
所有"编译/链接错误"结论均来自源码阅读，并对随包二进制做了 PE 导出表 / COFF 导入库成员 / 静态库机器类型解析来交叉验证。

---

## 0. 总体结论

这个项目**当前状态不可能编译通过**，而且不是零星笔误，而是**两套互不兼容的半成品设计被放在同一个 src/ 里**：

| 体系 | 文件 | 框架 | 日志 | 状态 |
|---|---|---|---|---|
| A 套（egui 单体程序） | `main.rs` `recorder.rs` `wgc_capture.rs` `av_backend.rs` `frame_ring.rs` `config.rs` `region_picker.rs` `app_error.rs` | eframe/egui + windows-rs | `tracing` | main.rs 引用了 A 套，但**没有 `mod` 声明** |
| B 套（托盘 + 后台线程 + 心跳 + i18n） | `app_state.rs` `i18n.rs` `constants.rs`（部分） | tray-icon + crossbeam-channel | `log`/`simplelog` | 引用的类型**全部不存在**，依赖**全部没写进 Cargo.toml** |

另外存在一条**必然的链接失败**（OpenH264 的 FFI 声明里有两个符号根本不存在）和一条**必然的运行时启动失败**（导入库要的 DLL 名与随包 DLL 名不一致）。

修复建议优先级见第 5 节。

---

## 1. 编译级错误（阻塞，全项目必现）

### 1.1 `main.rs` 没有模块声明
`CapLite/src/main.rs:2-6` 直接 `use crate::config::...`、`crate::recorder::...`、`crate::region_picker`、`crate::app_error::AppError`，
但全项目**没有任何 `mod xxx;` 语句**（全仓 grep `^\s*(pub )?mod\s+\w+` → 0 命中）。
→ 除 `main.rs` 外的所有 `.rs` 文件都不参与编译，`crate::config` 等全部 `unresolved import` (E0432)。

> 反过来说：一旦补上 `mod`，第 1.12 节的 B 套文件会立刻把 B 套的几十个错误一起暴露出来。

### 1.2 `Cargo.toml` 缺少代码真正用到的 windows feature
`wgc_capture.rs:6-7` 使用 `windows::Win32::Graphics::Direct3D11::*` 和 `windows::Win32::Graphics::Dxgi::*`，
但 `Cargo.toml:9-16` 的 features 只有 `Windows::*` 系列，**没有 `Win32_Graphics_Direct3D11`、`Win32_Graphics_Dxgi`**。
（用到：`D3D11CreateDevice`、`ID3D11Device`、`ID3D11Texture2D`、`ID3D11DeviceContext`、`D3D11_TEXTURE2D_DESC`、`D3D11_MAP_READ`、`IDXGIDevice`）
→ windows-rs 里未启用的命名空间不生成，模块路径 `unresolved`。

### 1.3 使用了不存在的变量 `out_path`
`av_backend.rs:152`：`let file = File::create(out_path)?;`
`out_path` 只是 `AvRecorder::new` 的参数（`av_backend.rs:103`），**从未存进结构体**（结构体定义 `av_backend.rs:87-100` 无此字段）
→ E0425 cannot find value `out_path`。后果：即使编译过，MP4 writer 也永远建不起来。

### 1.4 `FrameRing` 没有实现 `Default`
`recorder.rs:72`：`let mut ring = std::mem::take(&mut self.ring);` → 需要 `FrameRing: Default`，但 `frame_ring.rs` 没有实现 → E0277。

### 1.5 `self` 被 move 进 `'static` 线程闭包
`recorder.rs:75-94`（编码线程）在 `move` 闭包里访问 `self.av_recorder`、`self.start_time`（`recorder.rs:76,93`），
而 `start(&mut self)` 的 `self` 生命周期不是 `'static` → E0521/E0597。
正确做法：**进线程之前**先 `let mut av = self.av_recorder.take().unwrap();` 和 `let start_time = Arc::clone(&self.start_time);`，闭包只捕获这些拥有所有权的值。

### 1.6 `ring` 被 move 两次
`recorder.rs:72` 把 `self.ring` take 出来后，`recorder.rs:75` 的编码闭包 move 了它，`recorder.rs:114` 的采集闭包又用 `ring.push_frame(...)`
→ E0382 use of moved value。而且 `FrameRing` 内部状态根本不是线程安全的（见 3.6），不能靠"各持一份"绕过。

### 1.7 `GraphicsCaptureItem::GetForPrimaryDisplay()` 不存在
`wgc_capture.rs:20`。该类型只有 `CreateFromVisual` / `TryCreateFromDisplayId` / `TryCreateFromWindowId`（以及 `Size` / `DisplayName` / `Closed`）。
参考：[windows-docs-rs GraphicsCaptureItem](https://microsoft.github.io/windows-docs-rs/doc/windows/Graphics/Capture/struct.GraphicsCaptureItem.html)
→ 应改用 `TryCreateFromDisplayId(DisplayId(...))`（需要先从 DXGI/WinRT 取目标显示器的 DisplayId）。

### 1.8 `IsCursorCaptureEnabled(true)` 参数错误
`wgc_capture.rs:48`。`IsCursorCaptureEnabled()` 是**无参 getter**，setter 是 `SetIsCursorCaptureEnabled(bool)`。
参考：[windows-docs-rs GraphicsCaptureSession](https://microsoft.github.io/windows-docs-rs/doc/windows/Graphics/Capture/struct.GraphicsCaptureSession.html)

### 1.9 flag 字段被赋整数
`wgc_capture.rs:83-84`：`staging_desc.BindFlags = 0;` / `staging_desc.MiscFlags = 0;`
windows-rs 的 Win32 flag 类型是 newtype（同文件 `wgc_capture.rs:82` 写 `D3D11_CPU_ACCESS_READ.0 as u32` 正好说明这一点）
→ 类型不匹配 E0308，应写 `D3D11_BIND_FLAG(0)` / `D3D11_RESOURCE_MISC_FLAG(0)`（或 `Default::default()`）。

### 1.10 D3D11 立即上下文的获取代码类型错乱
`wgc_capture.rs:87-92`：
- `tex2d.GetDevice(None)` —— 第二参应为 `*mut Option<ID3D11Device>`，传 `None` 不成立；
- `let mut tmp_ctx: *mut std::ffi::c_void = ...; ...GetImmediateContext(&mut tmp_ctx)` —— 形参应为 `*mut Option<ID3D11DeviceContext>`；
- 随后 `ID3D11DeviceContext::from_raw(tmp_ctx as _)` 手工接管引用计数：如果指针来源不是"移交所有权"，就会变成泄漏或双重释放。

### 1.11 `D3D11CreateTexture2D` 不存在
`wgc_capture.rs:94`。创建纹理是 **`ID3D11Device::CreateTexture2D`**，d3d11.h 里没有 `D3D11CreateTexture2D` 这个自由函数
（有的是 `D3D11CreateDevice` / `D3D11CreateDeviceAndSwapChain`）。
参考：[ID3D11Device::CreateTexture2D](https://learn.microsoft.com/zh-cn/windows/win32/api/d3d11/nf-d3d11-id3d11device-createtexture2d)
→ 应该把 `ID3D11Device` 存到 `WgcCapture` 里，然后 `device.CreateTexture2D(&desc, None)?`。

### 1.12 B 套残留文件引用的东西全部不存在
`app_state.rs` 引用了：`crate::recorder::RecorderHandle`、`crate::region_picker::{start_region_picker, SelectOutcome}`、
`crate::av_backend::BackendEvent`、`config::{RuntimeState, RecordMode, load_config}`、`CaptureRegion::is_valid()`、
`crossbeam_channel`、`tray_icon`、`simplelog`、`winapi`、`log` —— **这些类型/依赖在项目里全部不存在**
（`recorder.rs` 里只有 `ScreenRecorder`；`region_picker.rs` 里只有 `pick_region`；`av_backend.rs` 里没有 `BackendEvent`；`config.rs` 里没有 `RuntimeState`/`RecordMode`/`load_config`；`Cargo.toml` 里没有那 5 个 crate）。
另外 `i18n.rs:23,30,49` 用 `log::info!`，而其他文件用 `tracing::*` —— 同一个 crate 里两套日志门面。
→ `app_state.rs`、`i18n.rs` 是另一个版本的程序残留，应当整体删除或整体迁移，不要试图"修好它再编译"。

---

## 2. 链接 / 构建 / 部署（即使编译通过也跑不起来）

### 2.1 【必然链接失败】`WelsSetEncoderParam`、`WelsEncodeFrame` 在 OpenH264 中不存在
`av_backend.rs:11-12` 声明并调用了这两个函数。我解析了随包二进制的导出表：

- `src/include/openh264.dll` **只导出 6 个函数**：
  `WelsCreateSVCEncoder`、`WelsDestroySVCEncoder`、`WelsCreateDecoder`、`WelsDestroyDecoder`、`WelsGetCodecVersion`、`WelsGetCodecVersionEx`
- `src/lib/openh264.lib` 里能搜到 `WelsCreateSVCEncoder`，**搜不到 `WelsEncodeFrame` / `WelsSetEncoderParam`**

→ **LNK2019 unresolved external symbol**。
OpenH264 的正确用法是 C++ 接口 `ISVCEncoder`：`WelsCreateSVCEncoder(&mut *mut ISVCEncoder)`，然后通过**虚表**调用
`Initialize(&SEncParamBase)` / `EncodeFrame(&SSourcePicture, &mut SFrameBSInfo)` / `Uninitialize()`。
在 Rust 里必须手工声明 `#[repr(C)] struct ISVCEncoderVtbl { ... }`（这是社区通行做法），不能直接 `extern "C"` 平铺调用。

顺带：`SEncParamBase`（`av_backend.rs:15-28`）结构体布局也是自造的 —— 真实定义只有 6 个字段
（`iUsageType, iPicWidth, iPicHeight, iTargetBitrate, iRCMode, fMaxFrameRate`），
这里写成了 10 个字段 + `[SSpatialLayerConfig; 4]`。首字段偏移 0 会被 OpenH264 当成 `iUsageType`（传进去的是 1920/1080），参数整体错位 → 配置必然失败或 UB。

### 2.2 【必然启动失败】导入库要的 DLL 名与随包 DLL 名不一致
`src/lib/openh264.lib` 的成员名是 `openh264-7.dll/`，即它是 **`openh264-7.dll` 的导入库**；
而 `src/include/` 里放的 DLL 叫 **`openh264.dll`**。
→ 进程启动时加载器找不到 `openh264-7.dll`，直接 `0xc0000135`。修法：把 DLL 改名为 `openh264-7.dll`，或用 `dumpbin/lib` 按实际 DLL 重新生成导入库。

### 2.3 `build.rs` 的库搜索路径指错
`build.rs:3`：`println!("cargo:rustc-link-search=native=./");` —— 相对路径解析到项目根，
但 `.lib` 全都在 `src/lib/` → 链接器找不到 `openh264.lib` / `yuv.lib`。应写 `native=src/lib`（或用 `CARGO_MANIFEST_DIR` 拼绝对路径）。
另外 `ap4.lib`（Bento4）**从未被链接**，而实现用的是 `muxide` —— 5.4 MB 静态库纯属死重量。

### 2.4 运行时 DLL 未部署
`build.rs` 只负责链接，**没有把 `openh264.dll` 拷到 `target/<profile>/`**（也没有 `cargo:rerun-if-changed`）。
→ 即便链接成功，`cargo run` 出来的 exe 也会因为找不到 DLL 而启动失败。

### 2.5 `src/include/libyuv.dll` 是空壳
我解析了它的导出表：**0 个导出函数**（文件仅 18 KB）。真正生效的是静态库 `src/lib/yuv.lib`
（44 个 COFF 成员，其中 41 个 x64 目标文件，符号 `ARGBToI420` 存在）。
→ 该 DLL 是死文件；同时 `include/` 里放 dll、`lib/` 里放 .lib 的命名也在误导维护者。

### 2.6 缺 `#![windows_subsystem = "windows"]`
`main.rs` 没有该属性 → release 构建会附带一个黑框控制台窗口。

### 2.7 日志初始化可能 panic
`main.rs:12-16` `RollingFileAppender::new(Rotation::Daily, "logs", ...)` 内部是 `expect`，
当 CWD 不可写（例如从 `C:\Program Files\` 启动、或 UAC 保护目录）时会 **panic 崩溃启动**。
应改用 `tracing_appender::rolling::Builder::new().build()` 返回的 `Result` 并降级到 stdout。

### 2.8 日志格式未关闭 ANSI
`main.rs:18-23` 没有 `.with_ansi(false)` → 写进 `logs/recorder.log` 的文本会夹带颜色转义码。

---

## 3. 运行期逻辑 / 并发 bug（把编译问题都修好后依然会错）

### 3.1 【首帧即失败】SPS/PPS 提取逻辑恒失败
`av_backend.rs:166-182`：
```rust
let start_code = &[0x00,0x00,0x01];              // 3 字节
if let Some(p) = nalu[pos..].windows(4).position(|x| x == start_code)  // 窗口是 4 字节
```
`windows(4)` 产出的切片长度是 4，与长度 3 的 `start_code` 比较**永远是 false** → 循环立即 `break` → `sps/pps` 永远 `None`
→ `extract_sps_pps` 返回 `NoSpsPps` → `feed_bgra_frame` 第一帧就报错 → **MP4 文件根本不会被创建，录制拿不到任何输出**。
即使把长度对齐，`av_backend.rs:171` 的 `end` 计算和 `av_backend.rs:173` 的 `slice[0]`（空 NALU 时 panic）仍需重写。
建议直接匹配 3 字节序列 `00 00 01`（可自动兼容 4 字节起始码），并对 `slice` 做非空判断。

### 3.2 【关键帧标志永远是 false】
`av_backend.rs:159`：`mux.write_h264_frame(nalu_data, self.timestamp, false)?;` —— 第三参是 `is_keyframe`，硬编码 `false`
→ 生成的 MP4 里没有关键帧标记，播放器可能完全解不出来。应按 `SFrameBSInfo.eFrameType == videoFrameTypeIDR` 判断。

### 3.3 时间戳按整数毫秒递增会漂移
`av_backend.rs:161`：`self.timestamp += 1000/self.fps as u64;` —— 30 fps 时每帧 +33 ms，1 秒只推进 990 ms。
录制 1 小时视频时长会短约 36 秒。应累加 `frame_index * 1000 / fps`（或直接用微秒/`TimeBase`）。

### 3.4 输出缓冲区大小与用法都不对
`av_backend.rs:142`：`let mut nalu_buf = vec![0u8; (w*h) as usize];` —— 每帧分配并清零 `w*h` 字节（1080p = 2 MB/帧，30 fps ≈ 60 MB/s），
且把 `nalu_buf` 当作 `WelsEncodeFrame` 的输出缓冲；按真实 API，输出应由 `SFrameBSInfo` 描述、且**单帧 H.264 数据完全可能超过 `w*h`**。改为复用缓冲并走 `SFrameBSInfo`。

### 3.5 编码线程是无 sleep 忙等
`recorder.rs:87`：`None => continue,` —— 配合 `frame_ring.rs:42-44` 里 `pop_frame` 的**阻塞式** `Condvar::wait`，
两者语义正好相反：要么 `pop_frame` 阻塞导致 `None` 分支是死代码，要么改成非阻塞后这里把 CPU 打满（单核 100% 空转）。
另外 `recorder.rs:78` 每轮循环都 `state.lock()`。

### 3.6 `FrameRing` 是"假线程安全"
`frame_ring.rs`：真正的数据（`buf` / `write_pos` / `read_pos` / `count`）都是普通字段，
`Mutex<()>`（`frame_ring.rs:10`）只保护了一个零大小值；而 `push_frame` / `pop_frame` 都要求 `&mut self`（`frame_ring.rs:27,40`）。
→ 要么它根本不能被两个线程同时用（当前就是这样，见 1.6），要么就得包进 `Arc<Mutex<FrameRing>>`（那内部 Mutex 又是多余的）。
现状既不可能编译，也不构成正确的同步原语。

### 3.7 采集节流算法写错 + 每帧重建全屏资源
- `recorder.rs:110-111`：`if now - last >= frame_interval { last = now; ... }` —— 应写 `last += frame_interval`，
  否则每轮的处理耗时都会累积成漂移，实际帧率明显低于目标。
- `wgc_capture.rs:86-104`：**每帧**都新建一张全屏 staging 纹理、每帧都 `GetDevice`/`GetImmediateContext`、每帧都 `CopyResource` 整屏再在 CPU 端裁剪。
  1080p@30 下这是纯粹的带宽与分配浪费。正确做法：在 `new()` 里保存 device/context 并复用一张**目标选区大小**的 staging 纹理，用 `CopySubresourceRegion` 只搬需要的区域。

### 3.8 【多显示器必崩】负坐标被 `as usize` 变成天文数字
`wgc_capture.rs:69-78`：
```rust
let crop_x = self.region.x as usize;   // x = -1920（副屏在主屏左侧）→ 18446744073709549696
...
if crop_x + crop_w > tex_width { ... } // usize 加法溢出
```
- debug：加法溢出直接 **panic**；
- release：溢出回绕，边界检查被绕过 → 随后 `src_ptr.add((crop_y+y)*row_pitch + crop_x*4)` **越界读取 GPU 映射内存** → 崩溃或花屏。

多显示器且副屏位于主屏左侧/上方时 100% 触发。修法：转换前校验 `x >= 0 && y >= 0`，用 `i64`/`u32` 做加法。

### 3.9 只抓主显示器，选区却可能来自别的屏
`wgc_capture.rs:20` 拿的是主显示器纹理，但选区坐标来自 `region_picker`（虚拟桌面坐标系）。
只要用户在副屏上框选，`wgc_capture.rs:76` 就会返回"选区超出采集画面范围" → 见 3.11，采集线程静默退出。

### 3.10 不校验采集像素格式
帧池按 `B8G8R8A8UIntNormalized` 申请（`wgc_capture.rs:42`），但 HDR/广色域显示器上 WGC 可能给出 `R16G16B16A16Float`，
代码从不检查 `desc.Format`（`wgc_capture.rs:66-67` 只读了 desc），一律按 4 字节 BGRA 解读 → 花屏/错位。

### 3.11 采集线程的错误不上报，UI 会一直"假装在录"
- `recorder.rs:102` `WgcCapture::new` 失败 → 只 `error!` 然后 `return`；
- `recorder.rs:117` 采集出错 → `error!` + `break`。

两种情况都**不改变 `RecorderState`**（仍是 `Recording`）→ 编码线程在 `Recording` 状态下永远忙等（见 3.5），
UI 继续显示"录制时长/停止录制"，用户以为录着，实际一个字节都没写。`start()` 本身返回 `Result`，应该把 `WgcCapture` 的创建挪到 `start()` 里，失败直接回给 UI。

### 3.12 `TryGetNextFrame` 的错误被当成"暂无新帧"
`wgc_capture.rs:59-62`：`Err(_) => return Ok(None)` —— 无帧和设备丢失/失效（切分辨率、独占全屏、显示器热插拔）无法区分，后者会被永久静默。

### 3.13 帧与会话没有 `Close()`
`Direct3D11CaptureFrame` 用完未 `Close()`，`GraphicsCaptureSession` 也未 `Close()`（`WgcCapture.session` 字段除构造外从未使用）。
WGC 的帧池只有 2 帧，官方要求用完释放以便复用，否则在高帧率下持续新建帧、显存增长。

### 3.14 UI 不会自动重绘
`main.rs:50-55` 用 `rec.get_record_duration()` 显示时长，但 `update()` 里没有 `ctx.request_repaint()` / `request_repaint_after(..)`
→ egui 只在有输入事件时重绘，"录制时长"基本不走字，状态切换也可能迟迟不刷新。

### 3.15 "停止录制"会冻结界面
`main.rs:90-95` → `recorder.rs:132-133`：在 UI 线程里 `join` 采集线程和编码线程，
编码线程还要把环形缓冲里剩下的帧编完、并调用 `AvRecorder::stop()`（`mux.finish()` 写 moov）。
→ 停止一次可能卡住几百毫秒到数秒。应改为异步停止（置 `Stopping` 后由后台线程收尾）。

### 3.16 保存路径逻辑错
`main.rs:68-73`：用户选完文件后把**完整文件路径**写进 `cfg.last_output_dir`（字段语义是目录），
下次 `FileDialog::set_directory(&self.cfg.last_output_dir)` 收到的是文件路径 → 目录记忆失效或行为异常。应存 `path.parent()`。
另外这里不强制 `.mp4` 扩展名（B 套的 `app_state.rs:216-218` 反而补了 `.mp4`，但那是死代码）。

### 3.17 配置读写太"沉默"
- `config.rs:45`：`serde_json::from_str(&text).unwrap_or_default()` —— **JSON 里少任何一个字段（旧版配置）就会把整份配置静默重置成默认值**（没有 `#[serde(default)]`）。
- `config.rs:51`：`let _ = fs::write(path, json);` —— 保存失败无任何提示。
- `config.rs:37` / `config.rs:49`：`fs::create_dir_all(&p).ok()` 同样吞错。
- 默认值不一致：`config.rs:27` `default_bitrate: 4000000`（4 Mbps） vs `constants.rs:16` `DEFAULT_BITRATE_KBPS = 5000`（5 Mbps）；
  `constants.rs` 里的 `FPS_MIN/MAX`、`BITRATE_MIN/MAX`、`BACKEND_HEARTBEAT_TIMEOUT` **从未被使用**，`config.rs` 也不引用 `constants`。
- 默认选区硬编码 `1920x1080 @ (0,0)`（`config.rs:25`）：在 2560×1440 / 4K 屏、或主屏不在虚拟桌面原点时会直接命中 3.9 的越界错误。
  `CaptureRegion::default()`（0×0）与 `PersistentConfig::default()`（1920×1080）也不一致。
- 无任何 fps/码率范围校验 → 手改 `config.json` 成 `fps: 100000` 会让采集线程疯狂轮询（`recorder.rs:104` 的间隔趋近 0）。

### 3.18 错误类型滥用
`recorder.rs:52`：`if *state != RecorderState::Idle { return Err(AvError::EncodeFail(-1)); }`
—— "已经在录制中"被报成"编码帧失败 ret=-1"，UI 上会显示 `启动录制失败: 编码帧失败 ret=-1`，完全误导。

### 3.19 `finish()` 的错误被吞掉，退出路径会产生损坏 MP4
- `av_backend.rs:194-196`：`impl Drop for AvRecorder { fn drop(&mut self) { let _ = self.stop(); } }`
  —— `stop()` 里 `mux.finish()?` 的错误（写 moov box 失败、磁盘满）被丢弃，用户拿到一个无法播放的文件却毫无提示。
- `ScreenRecorder` **没有 `Drop`**，`main.rs` 也没实现 `on_close_event`/关窗停止录制：
  直接关窗口/杀进程 → 编码线程被终止 → MP4 没有 moov，文件损坏。

### 3.20 小问题
- `main.rs:80` 在 `CentralPanel::show` 的闭包内用 `return` 提前结束整帧绘制（会跳过末尾的许可证标签），可读性差且有隐患。
- `wgc_capture.rs:10` `use tracing::{error};`、`recorder.rs:2` `warn`、`main.rs:6` `AppError`、`av_backend.rs:6` `warn/error` 等多处导入未使用（`cargo build` 一堆 warning）。
- `region_picker.rs` 的 `Rect::new(x, y, width, height)` 参数语义、以及它返回的坐标是**逻辑像素还是物理像素**，需要按 `interactive_screenshot` 0.4 的文档核对；
  高 DPI（125%/150%）下若与 WGC 的物理像素坐标不一致，选区会整体偏移/缩放。
- `av_backend.rs:119` 的 `uv_size = (width/2)*(height/2)`、`stride = w/2`：`recorder.rs:57-58` 把宽高向下取偶后是正确的，
  但 `AvRecorder::new` 自身不校验奇偶 → 作为独立 API 被复用时 UV 平面会少一列/一行（越界写）。

---

## 4. 没问题的部分（避免误改）

- `ARGBToI420`（`av_backend.rs:73-85,133`）用于 BGRA 数据是**正确的**：libyuv 的 `ARGBToI420` 在内存里就是 B,G,R,A 字节序，`ABGRToI420` 才是给 RGBA 用的。函数名容易误会，但用法没错。
- `recorder.rs:57-59` 把宽高向下取偶符合 H.264 4:2:0 的要求。
- `AvRecorder::stop` 里有 `is_null` 检查（`av_backend.rs:189`），不会重复销毁编码器。
- `src/lib/yuv.lib`（41 个 x64 COFF 目标文件，含 `ARGBToI420`）与 x64 目标匹配，静态链接可用。

---

## 5. 建议的修复顺序

**P0 — 先让它能编译、能定位**
1. 决定保留哪一套设计（建议保留 A 套 egui 单体，**删除** `app_state.rs`、`i18n.rs`，或反过来整体迁移），不要混编。
2. 在 `main.rs` 加 `mod` 声明；补齐 `Cargo.toml` 的 `Win32_Graphics_Direct3D11` / `Win32_Graphics_Dxgi`。
3. 修 1.3（`out_path` 存字段）、1.4（`FrameRing: Default`）、1.5/1.6（线程闭包与 ring 的所有权）。

**P1 — 打通采集与封装链路**
4. 按真实 `ISVCEncoder` 虚表重写 `av_backend.rs`（含正确的 `SEncParamBase`/`SSourcePicture`/`SFrameBSInfo`），
   或直接换成熟的 Rust 编码封装（如 `openh264` / `openh264-sys2` crate），不要再手写错误的 `extern "C"`。
5. 修 2.2（DLL 名 `openh264-7.dll`）、2.3（`link-search=native=src/lib`）、2.4（构建后拷 DLL 到输出目录）。
6. 修 3.1（SPS/PPS 起始码长度）、3.2（关键帧标志）、3.3（时间戳漂移）、3.19（Drop/关闭时 finish）。

**P2 — 正确性与体验**
7. 3.8（负坐标）、3.9/3.10（多显示器与像素格式）、3.4/3.5/3.6/3.7（缓冲复用、忙等、真正的线程安全环形缓冲、按 `last += interval` 节流）。
8. 3.11（错误上报到 UI）、3.14（`request_repaint`）、3.15（异步停止）、3.16（路径记忆）、3.17（配置健壮性）。
