# CapLite 两批修复实现记录（2026-10-19，第二轮）

> 对应复审报告里的 **P0-1 ~ P0-4** 与第二批 8 项。
> 代码在 `D:\deepseek-harness\default-workspace\CapLite`；`D:\WorkbuddyWW\CapLite` 未改动。

## 一、P0-1：超时 / 看门狗

- `MainPage` 增加 `start_deadline` / `stop_deadline` / `watchdog_fired`：
  - 启动 **15 s**；超时后退出 `Starting…`，报 `err_start_timeout`，不 join 卡住的线程；
  - 停止/导出 **120 s**；超时后报 `err_stop_timeout`，后台线程 detach，Cache 分片保留；
  - 退出路径最多等 **15 s**（`EXIT_WATCHDOG`），超时写 `cap.log` 后继续退出；
  - 新增两条单测覆盖 start/stop 看门狗。
- 看门狗只能“停止等待”，不能中断 DXGI/OpenH264/FFmpeg 的不可取消调用；这是有意的：
  进程退出时由操作系统回收线程，Cache 分片仍可恢复。

## 二、P0-2：静音漂移

- `MAX_SILENCE_PAD_FRAMES` 从 0.2 s 提升到 **10 s**；10 s 内按墙钟补静音，不再跳过。
- 超过 10 s：记 `audio_timeline_jumps`、写 `session_issue`、置 `Failed`/停止标志，
  由封装线程正常收尾。这样不会再出现“音频永久少一段、后续全部漂移”。
- 代价：超过 10 s 的停顿会直接结束本次录制（结果卡有明确警告）。

## 三、P0-3：Cache / junction

- 新增 `cache::is_plain_dir` / `ensure_plain_dir`：检查 `FILE_ATTRIBUTE_REPARSE_POINT`，
  拒绝 junction/symlink 目录。
- `Recorder::start` 先检查 `Cache` 根，再检查 `recordings` 子目录；
  `derived` 由主程序启动时检查，失败则本次不落盘。
- `prune_derived` 同时检查 `derived` 与其父 `Cache`；父目录是 junction 时直接拒绝删除。
- `resolve_cache_dir` 在探测前后都检查目录；不可写/被重定向时优先退到
  `%LOCALAPPDATA%\CapLite\Cache`，再退临时目录。
- 仍然**不自动删除** `Cache\recordings`。

## 四、P0-4：录制指示 / 热键隐私

- 托盘图标新增红点版本（`paint_recording_badge`），录制中 `NIM_MODIFY` 切换图标；
  tooltip 显示 `CapLite · Recording MM:SS`，每秒刷新。
- 开始/停止时推一条界面可见提示（`recording_started_notice` / `recording_stopped_notice`）。
- `hide_to_tray_on_record_start` 默认从 **true 改为 false**；老配置缺键也回落 false。
  用户显式开启后才隐藏，避免热键启动时静默隐藏窗口。
- 托盘图标暂时挂不上时仍记住当前 HICON/tooltip，Explorer 重启后 `TaskbarCreated`
  会按当前录制状态重新挂图标。

## 五、第二批 8 项

| 项 | 实现 |
|---|---|
| UI 文件 I/O 后台化 | `ConfigSaver` 专用 `cfg-save` 线程；配置探测加 1.5 s 超时；`record_paths` 不再做最多 1000 次 `exists()`，改用 pid+纳秒+进程内原子计数 |
| 线程 panic 立即上报 | `spawn_guarded` 包住 mux/video/audio/reader；panic 立即写 `session_issue`、置 `Failed`/停止，不再等 join |
| editor 取消失 join | 波形取消改为 `wave-reap` 后台线程回收句柄；`set_active(false)`/换文件不再在 UI 线程 join |
| prewarm 锁外释放 | `sweep_prewarm` / `enqueue_prewarm` / `discard_prewarm` / `take_prewarmed_encoder` 都在锁外 drop 旧编码器 |
| 导出空间预检 | 录制导出：剩余空间 < 源文件 + 8 MB 时直接保留分片并报 `export_issue`；`faststart` 仍按源文件+256 MB 降级。编辑器 copy 导出也加同样预检 |
| config/locale 大小上限 | `config.json` 1 MB；外部 locale 256 KB；超限拒绝并记日志 |
| 日志脱敏/轮转 | `C:\...` / `\\server\...` 替换为 `<path>`；2 MB 轮转为 `cap.log.1`，保留一份；tracing sink 按行脱敏 |
| 独立会话通知窗口 | 新增 `session.rs`：独立隐藏顶层窗口注册 WTS，处理锁屏/挂起/`WM_DISPLAYCHANGE`，不再依赖托盘窗口是否创建成功 |

## 六、验证

| 项 | 结果 |
|---|---|
| `cargo test --lib -- --test-threads=1` | **396 passed / 0 failed / 5 ignored** |
| `cargo test --bins` | 9 passed |
| `cargo test --test e2e` | 7 passed |
| `cargo clippy --all-targets` | 0 warning |
| GUI 热键开始/停止 | 通过，产出 MP4 |
| GUI 独立 session 窗口 + 模拟锁屏 | 通过，录制停止并导出 |
| 新增单测 | 启动/停止看门狗、panic 立即上报、日志脱敏、日志轮转、配置/locale 超限、托盘红点、后台配置保存 |

## 七、仍不在本轮范围

- 不可信媒体的**独立解码 helper 进程**（本轮仍只有 `max_pixels`/probesize 边界）。
- 正式 OV/EV 代码签名（本轮仍是开发证书；`dist`/`manual-test` 需重打包后另签）。
- 真实锁屏/合盖休眠/Explorer 重启/低磁盘 VHD/网络断开的人工复测。
- 看门狗只保证“UI/退出不无限等”，不能中断底层不可取消的 FFmpeg/DXGI 调用。
