# CapLite 优化实现记录（2026-10-19）

> 对应上一轮《CapLite 安全 / 稳定 / 系统占用优化方案》中“最值得先处理的 8 件事”
> 与“安全侧 3 件事”。本文只记录**已落地**的代码与验证结果；未完成项在最后一节列出。
>
> **后续两批修复**（P0-1~P0-4 + 第二批 8 项）见
> [`IMPLEMENTATION_2026-10-19_BATCH2.md`](IMPLEMENTATION_2026-10-19_BATCH2.md)。

## 一、已落地

### 1. 单实例与文件唯一性（S3 / T7 / D1）

- 新增 `src/single_instance.rs`：`Local\CapLite.SingleInstance` 命名互斥体；
  第二个实例调用 `tray::show_main_window_now()` 激活已有窗口后退出。
- `main()` 在配置/日志/Cache 任何写入之前认领单实例。
- 配置保存：
  - 临时文件名唯一（`config.json.<pid>.<nanos>.<n>.tmp`）+ `create_new`；
  - 写完 `sync_all` 再 rename；rename 失败重试（0/20/100/250 ms）；
  - 替换前保留 `config.json.bak`；主文件损坏时 `ConfigStore::load` 回退 `.bak`。
- 录制流文件改到 `Cache\recordings\rec-<stamp>-<pid>-<n>.mp4`，同秒第二次开始
  不会覆盖第一段；导出重名 10,000 次后不再回退覆盖，改用纳秒后缀。

### 2. UI 线程不再阻塞（T1 / T2 / T3 / T6）

- `MainPage::start_recording` 起 `cap-start` 后台线程，立即返回；
  `stop_recording` 把 `Recorder` 移交给 `cap-stop` 线程，立即返回。
- `poll_recording_jobs` 每帧收结果；状态卡片显示 `Starting…` / `Exporting…`；
  启动/停止期间持续 `request_repaint_after(100ms)`，隐藏到托盘时由 `App::logic` 收结果。
- 关闭路径：正在开始/停止/录制时先 `CancelClose`，请求“完成后关闭”；
  `on_exit` 用 `wait_for_recording_jobs` 等在途任务完成，避免进程先退、导出丢失。
- `Recorder::start` 增加 `StartGuard`：任一 `spawn` 失败时置停并 join 已启动线程，
  不再留下继续写 Cache 的孤儿录制。
- `Recorder::stop` 检查每个 `join()` 的 panic；`RecordingResult.session_issue`
  保留采集/编码/音频失败、线程 panic、磁盘不足等原因；结果卡片常驻 Warn，
  不会把“提前结束”显示成“成功”。

### 3. 磁盘空间（T5）

- 新增 `src/disk.rs`：`GetDiskFreeSpaceExW` 查询可用字节，`MIN_FREE_BYTES = 512 MB`。
- 录制开始前检查 Cache/输出卷；不足直接拒绝并给出可读错误。
- 封装线程每 5 s 检查一次；低于安全余量主动置停、保留 Cache 分片并报 `session_issue`。
- 停止导出前检查空间是否够 `+faststart` 的临时文件；不够时降级为普通 MP4
 （moov 在末尾，仍可播/可剪辑）。
- `FragmentedMp4Writer::flush` 现在检查 `AVIOContext.error`；`finish` 失败计入
  `session_issue`，不再只 warn。

### 4. 休眠 / 锁屏 / 显示变化（T4）

- `src/power.rs`：封装线程持有 `SetThreadExecutionState(ES_CONTINUOUS |
  ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)`，停止/panic 时恢复。
- 托盘窗口注册 `WTSRegisterSessionNotification`；WndProc 处理：
  - `WM_WTSSESSION_CHANGE` 锁屏 → `TrayCommand::SystemStop`；
  - `WM_POWERBROADCAST` 挂起 → `SystemStop`；
  - `WM_DISPLAYCHANGE` → `SystemStop`。
  统一走正常停止/导出，不自动恢复录制。
- 音频静音补齐单轮上限 0.2 s；超长跳变只记 `audio_timeline_jumps`，不再一次性
  `resize` 出几分钟静音（避免休眠/挂起后 OOM/卡死）。

### 5. 配置、Cache 与日志（S4 / R8 / R9 / S8）

- `ConfigStore::save` 走“已探测过”的 `save_to_impl(..., probe=false)`；
  不再每次保存都 create/write/delete 探测文件。
- 所有配置目录都不可写时，`ConfigStore::load` 直接返回默认配置，**不读 CWD**
  里的相对 `config.json`。
- Cache 分层：
  - `Cache\recordings\`：录制分片，永不自动删除；
  - `Cache\derived\`：波形峰值，启动时后台修剪（最多 64 个/30 天）。
  关闭提醒只针对录制分片（兼容旧版根目录 `rec-*.mp4`）。
- release 安装 `tracing` 文件 sink（默认 WARN，`RUST_LOG` 可调）；此前 GUI
  没有订阅者，采集/音频/导出告警在用户机器上全部丢失。

### 6. 资源放大器（R1–R7）

- 预热：同一时刻最多一条 OpenH264 初始化线程；新配置合并为 `pending`；
  TTL 从 300 s 降到 60 s，并新增 `sweep_prewarm` 由 UI 每帧主动回收。
- 波形：`EditorPage::set_active` 在离开剪辑页/开始录制时取消波形、深度检查和播放；
  回到剪辑页再从缓存/进度恢复；不再无条件 `set_show_waveform(true)`，尊重设置。
- 播放帧队列按字节预算（64 MB）限制：1080p 仍 8 帧，4K 降到 2 帧。
- 框选冻结帧只抓目标显示器；`bgra_to_rgba` 原地交换，省掉第二份 33 MB 缓冲。
- 鼠标光标位图按句柄 + 100 ms 缓存；每帧只更新位置，不再每帧 `GetIconInfo` +
  两次 `GetDIBits`。增加采集 buffer pitch/length 防御。
- 完整中文字体只在 `zh-CN` 界面加载；英文界面跳过 15–25 MB 字体兜底。
- OpenH264 找不到 DLL 时 release 只从 exe 同级解析，不再搜 CWD、`CARGO_TARGET_DIR`、
  `OPENH264_DLL_DIR` 或编译期源码路径。

### 7. 安全侧

- `main()` 最早调用 `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS)`；
  FFmpeg 预加载改为 exe 同级绝对路径 + `LoadLibraryExW(DLL_LOAD_DIR | DEFAULT_DIRS)`。
- 剪辑/播放/体检/波形/音频输出所有 FFmpeg 解码器设置 `max_pixels = 40 MP`；
  输入上下文设置 `probesize = 8 MB`、`max_analyze_duration = 2 s`。
- `app.manifest` 增加 `asInvoker`、`PerMonitorV2`、`longPathAware`、Win10/11
  `supportedOS`；已用 `mt.exe -inputresource` 回读确认嵌入成功。
- `.gitignore` 忽略 `dev-signing/`；`dev-sign.ps1` 支持 `CAPLITE_PFX_PASSWORD`
  环境变量与可选 `-TimestampServer`（RFC3161），默认行为保持离线可用。
- `_build-env*.bat` 改用 `%~dp0`，不再写死 `D:/WorkbuddyWW/CapLite`；README 的
  构建说明同步改到这两个脚本。

## 二、验证结果

| 项 | 结果 |
|---|---|
| `cargo check --all-targets` | 通过，0 warning |
| `cargo clippy --all-targets` | 通过，0 warning |
| `cargo test --lib -- --test-threads=1` | **389 passed / 0 failed / 5 ignored** |
| `cargo test --bins` | 6 passed |
| `cargo test --test e2e` | 7 passed |
| `cargo build --release --bin CapLite` | 通过（1m 12s） |
| 真机 recorder smoke（video-only 2 s） | 45 帧 / 0 丢帧 / 导出 2046 ms，对账 0 ms |
| 真机 recorder smoke（audio+video 2 s） | 53 帧 + 96 AAC 包 / 导出 2047 ms，对账 0 ms |
| 异步起停探针 | `async_start_stop_probe` 通过；start/stop 均 < 500 ms 返回，1.87 s 内完成 |
| 单实例（release） | 第一次运行；第二次 4 s 内退出，系统仅 1 个进程 |
| release 启动 | 首帧约 254 ms；`cap.log` 有启动时间轴；FFmpeg 预加载 **5/5** |
| release 单实例 | 第二次启动 4 s 内退出，系统仅 1 个进程 |
| manifest | `mt.exe -inputresource` 回读：asInvoker / PerMonitorV2 / longPathAware / Win10 supportedOS 均已嵌入 |

> `dist\` 与 `manual-test\` **没有**用本次 release 重新签名/打包：签名要在能写用户证书库的
> 普通 PowerShell 里执行 `scripts\dev-sign.ps1`。本次产物是
> `target\release\CapLite.exe`（未签名）。

## 三、未完成 / 后续

1. **FFmpeg 日志回调**：目前只把 Rust `tracing` 接到 `cap.log`；FFmpeg 自身
   `av_log` 仍写 stderr（release 无控制台，等于丢弃）。需要 `av_log_set_callback`
   + `va_list` 格式化，留到下一轮。
2. **解码 helper 进程**：已加 `max_pixels`/probesize 边界，但协议层解析仍在主进程。
   把剪辑/波形/播放解码移入受限令牌 + Job Object 的 helper 是更大的架构改动。
3. **发布签名**：仍使用开发证书；正式分发需要 OV/EV 证书 + RFC3161 时间戳。
   本轮只做了不把 PFX 纳入版本库/发布包。
4. **日志轮转**：`tracing` sink 目前是单文件 append；运行期 2 MB 轮转尚未实现。
5. **真实场景验证**：锁屏/挂起/多屏热插拔、低磁盘 VHD、Explorer 重启后的托盘
   图标恢复需要人坐在真机前逐项确认（本环境无法可靠模拟）。
6. **`D:\WorkbuddyWW\CapLite` 是另一份工作副本**，且其 `src/config.rs` 已有一套
   独立的 P56 配置加固；不要把本目录的 `src/` 整体覆盖过去，先 diff/合并。
