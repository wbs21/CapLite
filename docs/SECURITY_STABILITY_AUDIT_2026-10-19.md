# CapLite 安全 / 稳定性复审（2026-10-19 第二轮）

> 复审对象：`D:\deepseek-harness\default-workspace\CapLite`（含上一轮 8+3 项实现）。
> 方法：静态代码审查 + 自动化 GUI 实测（真实 DXGI/OpenH264/WASAPI/托盘消息）。
> **本轮只做审核，没有改动任何代码。**
>
> 上一轮实现记录见 [`IMPLEMENTATION_2026-10-19.md`](IMPLEMENTATION_2026-10-19.md)；
> 原始方案见 [`../../CapLite-安全稳定与系统占用优化方案-2026-10-19.md`](../../CapLite-安全稳定与系统占用优化方案-2026-10-19.md)。
>
> **2026-10-19 后续修复**：本文 P0-1~P0-4 与第二批 8 项已实现，见
> [`IMPLEMENTATION_2026-10-19_BATCH2.md`](IMPLEMENTATION_2026-10-19_BATCH2.md)。
> 本文保留为复审时的原始证据，下面的“未修复”描述是当时的快照。

---

## 0. 结论摘要

上一轮的实现是有效的：单实例、异步起停、失败可见、磁盘检查、锁屏/挂起停录、
Cache 分层、DLL 搜索收紧、解码边界、release 日志都通过了真实链路验证。

但复审仍然找到 **6 个高危**、**12 个中危**和一批低危/加固项。最需要优先处理的是：

1. **开始/停止/收尾没有超时与取消**：`cap-start` / `cap-stop` / `on_exit` 都可能无限等待，
   用户会遇到“永远 Starting…”“关不掉”“进程卡死”。
2. **超长静音补齐被“跳过”会造成音轨永久变短/漂移**：修掉 OOM 的同时引入了 A/V 时长不一致。
3. **Cache 在 exe 目录 + `prune_derived` 跟随 junction**：便携版被管理员运行时，
   攻击者可预置 `Cache\derived` junction，让启动清理删除目标目录里的文件。
4. **全局热键可以静默开始录制，且没有录制指示**：同一会话的任何进程都能
   `PostMessage(WM_HOTKEY)` 触发；默认还会隐藏窗口。
5. **不可信视频仍在主进程内解码**：`max_pixels`/probesize 只是资源边界，不是安全隔离。
6. **DLL 可被同名替换且没有哈希/版本留痕**：exe 目录是用户可写时，管理员运行会放大风险。

---

## 1. 已验证通过的部分（避免误判）

| 场景 | 结果 |
|---|---|
| 真实 GUI 热键开始/停止 | 通过：生成普通 MP4，进程继续运行 |
| 开始阶段立刻停止 | 通过：`stop_after_start` 生效，生成短文件，不卡死 |
| 录制中模拟 WTS 锁屏（`WM_WTSSESSION_CHANGE`） | 通过：正常停止并导出 |
| 录制中点 X 关闭 | 通过：进程退出，产物存在 |
| 开始阶段点 X 关闭 | 通过：等启动完成后停止、导出、退出 |
| 最小化后热键开始/停止 | 通过：仍能生成文件 |
| 双实例 | 通过：第二个进程退出，系统仅 1 个进程 |
| release 启动 | 首帧约 254 ms；FFmpeg 预加载 5/5 |
| `cargo test --lib` | 389 passed / 0 failed / 5 ignored |
| `cargo test --bins` / `--test e2e` | 6 / 7 passed |
| `cargo clippy --all-targets` | 0 warning |

> 这些实测说明“异步状态机”和“锁屏/关闭收尾”主路径是能工作的；下面的问题主要是
> **边界条件、异常路径和攻击面**。

---

## 2. 高危问题（P0）

### P0-1 · 开始/停止/收尾没有超时、没有取消，`on_exit` 可能永久挂起

**证据**
- `cap-start` / `cap-stop` 线程：[main_page.rs](../src/ui/main_page.rs#L1109)、
  [main_page.rs](../src/ui/main_page.rs#L1155)。
- `wait_for_recording_jobs` 直接 `join`：[main_page.rs](../src/ui/main_page.rs#L1290)。
- `App::on_exit` 调它：[main.rs](../src/main.rs#L965)。
- `Recorder::stop` 无超时 `join`：[recorder.rs](../src/recorder.rs#L1191)。
- `StartGuard::drop` 无超时 `join`：[recorder.rs](../src/recorder.rs#L390)。

**风险**
- `Recorder::start` 卡在 DXGI / OpenH264 / WASAPI / 网络输出目录时，UI 永远停在
  `Starting…`；用户点关闭，`on_exit` 也会一起卡住。
- `Recorder::stop` 卡在 `av_interleaved_write_frame`（慢盘、网络盘、杀毒软件）或
  `+faststart` 回拷时，UI 永远停在 `Exporting…`；关闭流程同样卡死。
- `cap-start` / `cap-stop` 没有取消令牌：`stop_after_start` 只能“等它启动完再停”，
  如果启动本身卡死，这个标志永远不生效。

**建议**
1. 给 start/stop/finalize 增加**超时**（例如 start 15 s、stop 5 min，按分辨率/码率可配），
   超时后记录 `session_issue`，把 UI 切到可操作的失败态。
2. 把 `join` 从 UI/退出路径移走：用完成 channel + 看门狗线程；退出时最多等 N 秒，
   超时就保留 Cache 分片并强制退出（明确提示“未完成导出，分片已保留”）。
3. 给 `cap-start` / `cap-stop` 增加取消令牌；取消时不再等待不可中断的 FFmpeg 调用，
   而是让看门狗负责最终退出。
4. 对 `on_exit` 的 `wait_for_recording_jobs` 增加**总超时**和日志；不要无限阻塞消息循环。

---

### P0-2 · 超长静音“跳过”会让音轨永久变短、音画漂移

**证据**
- [recorder.rs](../src/recorder.rs#L1003)：
  ```rust
  if gap > MAX_SILENCE_PAD_FRAMES {
      stats.audio_timeline_jumps += 1;
      // 只记日志，不补静音
  }
  consumed[i] = target.max(have);
  ```
- AAC 的 pts 来自 `aac_index * FRAME_SAMPLES`，不是墙钟；跳过的静音不会进入
  `aac_index`。

**风险**
- 一次 1 秒的调度停顿/调试器暂停/高负载，会让音频时间轴永久少 1 秒，
  而视频继续按墙钟走；结果是**音轨比视频短、后续音画错位**。
- 锁屏/挂起时 WTS 会停录，但“CPU 卡顿 1 秒”不会触发 WTS；这正是该分支会命中的场景。
- 当前实现把 OOM 风险换成了 A/V 同步错误，属于“修一个坏一个”。

**建议**（三选一，按产品取舍）
1. 每轮最多补 `MAX_SILENCE_PAD_FRAMES`，但**不推进 `consumed`**，让后续循环慢慢追上；
   同时限制追赶期间的单帧总工作量。
2. 检测到超长 gap 时同时重锚定音频和视频时间轴（两边都跳过），并写一条
   `session_issue`；不要让只有音频变短。
3. 把“超长 gap”视为会话失败：正常停止并导出，明确告诉用户“录制因系统停顿而停止”。

---

### P0-3 · Cache 在 exe 目录 + `prune_derived` 跟随 junction：管理员运行时可被重定向删除

**证据**
- Cache 优先 `<exe>\Cache`：[cache.rs](../src/cache.rs#L19-L37)、
  [main.rs](../src/main.rs#L374)。
- `prune_derived` 对 `Cache\derived` 直接 `read_dir` + `remove_file`：
  [cache.rs](../src/cache.rs#L60)。
- 没有检查 `FILE_ATTRIBUTE_REPARSE_POINT`，也没有拒绝 junction。
- 便携版常放在 `Downloads` 等用户可写目录；`app.manifest` 是 `asInvoker`，但用户完全可能
  “以管理员身份运行”。

**风险**
- 普通用户可预先在 exe 目录创建 `Cache\derived` junction，指向任意目录；管理员运行
  CapLite 时，启动清理会删除目标目录中“超过 30 天/超过 64 个”的文件。
- 同样的问题也适用于 `Cache\recordings`（写入重定向）和配置目录（`.bak`/临时文件写入）。
- 这是**本地权限提升/任意文件删除**类问题，不是“隐私”级别。

**建议**
1. Cache/日志/派生缓存默认放 `%LOCALAPPDATA%\CapLite\`（每用户 ACL），
   不要把“程序目录可写”作为首选。
2. 打开/创建 `Cache`、`recordings`、`derived` 前检查 reparse point；发现 junction/symlink
   直接拒绝并把缓存退到 LocalAppData。
3. **不要在启动时自动删除派生缓存**；改成用户显式“清理派生缓存”，或至少只在
   确认是真实目录、且路径在 LocalAppData 下时修剪。
4. 如果检测到进程已提升且 exe 目录对普通用户可写，启动时给出明确警告并拒绝自动清理。

---

### P0-4 · 全局热键静默开录，且没有录制指示

**证据**
- `WM_HOTKEY` 直接 `push_command(ToggleRecording)`：
  [tray.rs](../src/tray.rs#L709-L713)。
- 热键启动后默认隐藏到托盘：`hide_to_tray_on_record_start = true`：
  [config.rs](../src/config.rs#L280)。
- 托盘提示文本固定为 `APP_NAME`，不区分录制状态：
  [tray.rs](../src/tray.rs#L1086-L1098)。
- 本轮实测：用 `PostMessage(WM_HOTKEY, 0x0CA1)` 直接向 `CapLiteTrayMessageWindow`
  投递即可开始录制。

**风险**
- 同一桌面会话的任何程序（或任何能模拟按键的程序）都可以让 CapLite 开始录屏，
  并默认隐藏窗口；用户看不到任何“正在录制”的提示。
- 与 Windows 的摄像头/麦克风隐私指示不同，屏幕录制没有系统级指示。
- 这是产品级隐私/安全问题，不是“快捷键太灵敏”。

**建议**
1. 录制中托盘图标必须变化（红点/边框），tooltip 显示 `Recording 00:12:34`；
   开始/停止时发一次 balloon/toast。
2. 热键启动的录制**默认不隐藏窗口**；只有用户显式勾选“热键启动也隐藏”才隐藏。
3. 可选：任务栏覆盖图标（`ITaskbarList3::SetOverlayIcon`）作为第二指示。
4. README/设置页明确写“全局热键 + 隐藏 = 无可见指示”，把选择权交给用户。

---

### P0-5 · 不可信视频仍在主进程内解码，资源上限不等于安全隔离

**证据**
- `max_pixels = 40 MP`、`probesize = 8 MB`、`max_analyze_duration = 2 s`：
  [edit/mod.rs](../src/edit/mod.rs#L61-L86)。
- 剪辑/播放/体检/波形/音频输出全部在进程内调用 FFmpeg 解码，没有 helper 进程。
- 没有限制 `nb_streams`、附件/元数据大小、解码总时长。

**风险**
- FFmpeg 解码器漏洞会直接变成 CapLite 进程内的代码执行/崩溃。
- 恶意构造的多流/超大元数据文件仍可在 `avformat_find_stream_info` 阶段消耗大量内存。
- 进程崩溃会连带正在进行的录制（虽然 Cache 有分片）。

**建议**
1. 把“不可信文件解码”移入受限 helper 进程：低完整性令牌 + Job Object
   （内存/CPU/子进程限制）+ 无网络；主进程只收 RGBA/I420 和波形峰值。
2. 若短期不做进程隔离，至少补 `nb_streams` 上限（如 8）、单文件大小/时长上限、
   `avformat` 的 `max_streams`/协议白名单，并建立 fuzz/ASan 流程。
3. 继续保持 FFmpeg 版本与 SHA256 固定；CVE 跟踪要有明确责任人。

---

### P0-6 · DLL 可被同名替换，且没有哈希/版本留痕；管理员运行会放大风险

**证据**
- FFmpeg 预加载用 exe 同级绝对路径：
  [startup.rs](../src/ui/startup.rs#L416-L437)。
- `SetDefaultDllDirectories` 只收紧搜索路径：
  [main.rs](../src/main.rs#L2127)。
- 加载后只记“成功/失败”，不记 `GetModuleFileNameW`/SHA-256/`av_version_info`。
- `dist` 里的 7 个运行期 DLL 均未签名（`NotSigned`）。
- 便携版位于用户可写目录；用户可能右键“以管理员身份运行”。

**风险**
- 本地攻击者（或恶意安装包）可替换 exe 同级的 DLL；管理员运行时即获得管理员代码执行。
- 无法从日志判断“用户实际加载的是不是随包版本”。
- 注意：LGPLv3 第 4 条要求允许替换共享库，所以**不能默认用哈希拒绝加载**；
  但可以记录、警告、提供显式覆盖开关。

**建议**
1. 加载后记录每个 DLL 的完整路径、大小、SHA-256、`av_version_info()`；
   发现与随包版本不一致时记 warning。
2. 启动时检测“进程已提升 + exe 目录对普通用户可写”，明确警告并建议安装到 Program Files。
3. 正式分发使用 OV/EV 证书 + 时间戳；DLL 可由构建方签名（不阻止 LGPL 替换，
   只提供可验证来源）。
4. 用 `dumpbin /loadconfig` 检查 CFG/CET；评估 `/guard:cf` 与 CETCOMPAT。

---

## 3. 中危问题（P1）

### P1-1 · UI 线程仍有多处文件系统 I/O，网络/慢盘会卡首帧或卡设置

**证据**
- `ConfigStore::detect` 在窗口创建前探测 4 个目录（真写文件）：
  [main.rs](../src/main.rs#L2155)、[config.rs](../src/config.rs#L517)。
- `App::new` 里 `plan_output_dir` 探测输出目录：
  [main.rs](../src/main.rs#L271)。
- `resolve_cache_dir` 探测 Cache： [main.rs](../src/main.rs#L374)。
- `Config::save_to_impl` 在 UI 线程做 `sync_all` + `.bak` 拷贝 + rename：
  [config.rs](../src/config.rs#L386)、[main.rs](../src/main.rs#L623)。
- `record_paths` 在 UI 线程做最多 1000 次 `exists()`：
  [main_page.rs](../src/ui/main_page.rs#L2989)。

**风险**
- 配置/输出目录在断开的 UNC、休眠的移动硬盘、被 OneDrive 占位的路径上时，
  启动可能长时间不出窗口；拖设置滑条可能卡 UI。
- 这些操作没有超时，和 P0-1 是同一类“UI 等 I/O”问题。

**建议**
- 启动探测放后台线程 + 超时；首帧先用安全的临时默认值。
- 配置保存改成后台写队列；UI 只更新内存，退出时等待最后一次写。
- `record_paths` 用唯一随机名直接创建，不做 1000 次 `exists()` 轮询。

### P1-2 · 录制线程 panic 不会立即上报，只有用户停止时才被发现

**证据**
- 视频/音频线程没有 `catch_unwind`；状态只在正常错误分支设置。
- `stop()` 在全部 `join` 后才统计 panic：
  [recorder.rs](../src/recorder.rs#L1199-L1215)。
- `mux_thread` 看到 channel 断开只把 `video_done/audio_done` 置真，不立即把会话判失败。

**风险**
- 视频线程 panic 后，mux 可能继续录音频；UI 仍显示 `Recording`，直到用户停止才出现
  `session_issue`。用户会以为整段都录到了。
- 音频线程 panic 同理，视频继续，音轨尾部缺失。

**建议**
- 给每个 producer 线程体加 `catch_unwind(AssertUnwindSafe(...))`，panic 时立即设置
  `state=Failed`、`note_issue`、`stop=true`。
- mux 线程在收到 channel 断开时也设置 `session_issue`（“视频/音频线程异常退出”），
  不要静默当成正常结束。

### P1-3 · `EditorPage::set_active(false)` / `cancel_current` 在 UI 线程 join

**证据**
- `cancel_current` 直接 `join` 细化线程和 4 个基准 worker：
  [editor.rs](../src/ui/editor.rs#L768)。
- `set_active` 在切页/开始录制时调用它：
  [editor.rs](../src/ui/editor.rs#L2552)。

**风险**
- 源文件在慢速网络盘/坏盘上时，`av_read_frame` 可能长时间不返回；切页/开始录制会卡 UI。
- 这正是上一轮报告里“UI 绝不 join 后台线程”的遗留问题。

**建议**：取消标志 + 后台 reaper/非阻塞句柄回收；UI 只置取消，不等待。

### P1-4 · `clip-open` 线程不可取消、句柄被丢弃

**证据**
- `EditorPage::open` 起 `clip-open` 后只保留 receiver，`Ok(_)` 丢弃 `JoinHandle`：
  [editor.rs](../src/ui/editor.rs#L1992-L2008)。
- 旧线程仍会继续 `open_streaming` + 关键帧/健康全片扫描；epoch 只丢弃消息，不停止读盘。

**风险**：连续打开多个大文件会叠加扫描线程，抢 CPU/磁盘；在录制时尤其明显。

**建议**：加取消令牌/`JoinHandle`，或只保留最新任务；扫描线程检查取消。

### P1-5 · 预热编码器在 `PREWARM` 锁内析构，可能卡 UI

**证据**
- `sweep_prewarm` 在锁内 `st.slot = None`： [encode.rs](../src/video/encode.rs#L506)。
- `enqueue_prewarm` 在锁内丢弃旧 slot： [encode.rs](../src/video/encode.rs#L545)。
- `take_if_match` 在锁内 `slot.take()`： [encode.rs](../src/video/encode.rs#L427)。

**风险**：`VideoEncoder::Drop` 会调用 OpenH264 `Uninitialize`/`Destroy`，可能 join 线程；
在 UI 每帧调用 `sweep_prewarm` 的路径上，过期回收会造成一次卡顿。

**建议**：把旧实例 `take()` 到局部变量，释放锁后再 drop。

### P1-6 · 导出前空间检查在 `faststart=false` 时仍可能把盘写满

**证据**
- `src_bytes` + `MIN_FREE_BYTES/2` 只决定是否 faststart：
  [recorder.rs](../src/recorder.rs#L1502-L1516)。
- 如果 `free < src_bytes`，仍然会调用 remux 写一份完整产物，只是不带 faststart。

**风险**：输出盘会在 remux 中途写满；虽然最终删除半成品，但期间可能影响系统。

**建议**：`free < src_bytes * 1.05` 时直接跳过导出、保留 Cache 分片并提示；
编辑器导出也加同样的预检。

### P1-7 · `config.json` / locale overlay 读取无大小上限

**证据**
- `Config::load_from` 直接 `read_to_string`： [config.rs](../src/config.rs#L361)。
- `I18n::overlay_dir` 直接读任意 `*.json`： [i18n.rs](../src/i18n.rs#L47-L56)。

**风险**：损坏/恶意的大文件可在启动时 OOM；locale overlay 还可改写 UI 文案（钓鱼）。

**建议**：限制 1 MB（config）/256 KB（locale），超限拒绝并提示；locale 只接受已知 schema。

### P1-8 · 日志隐私、无运行期轮转、`RUST_LOG` 可放大

**证据**
- `init_tracing` 直接写原始 `tracing` 消息： [main.rs](../src/main.rs#L2064)。
- 路径/设备名/文件名不脱敏；`RUST_LOG=debug` 可输出大量细节。
- 只在启动时截断 1 MB，运行期无轮转。

**风险**：日志泄露用户目录/文件名；debug 日志可能写满磁盘；共享机器上 `cap.log` 可读。

**建议**：默认脱敏路径；2 MB × 2 轮转；限制 `RUST_LOG` 上限；日志文件 ACL 收紧。

### P1-9 · WTS/电源/显示通知依赖托盘窗口；窗口建不出来就没有锁屏停录

**证据**
- WTS 注册在 `Tray::install` 里，只有 `create_message_window` 成功才执行：
  [tray.rs](../src/tray.rs#L975)。
- `WM_POWERBROADCAST`/`WM_DISPLAYCHANGE` 也由这个窗口接收：
  [tray.rs](../src/tray.rs#L692-L745)。

**风险**：如果托盘窗口创建失败（受限环境/Explorer 异常），录制仍可开始，但锁屏/挂起
不会自动停录，可能录到黑屏/安全桌面并产生错误文件。

**建议**：把“会话/电源通知窗口”独立于托盘（即使托盘失败也创建），或在主窗口注册。

### P1-10 · Cache 录制分片仍是明文、无启动恢复、无总量上限

**证据**
- `Cache\recordings` 永不自动删除： [cache.rs](../src/cache.rs#L22)。
- 关闭时才提醒；进程崩溃/强杀后下次启动没有恢复入口。
- 分片是屏幕内容，明文保存在 exe 目录/文档目录。

**风险**：敏感屏幕内容长期残留；崩溃后用户不知道可以恢复；磁盘无上限增长。

**建议**：启动时后台统计并提示“发现未导出分片”；提供打开/移动到输出目录；
可选“超过 N 天/超过 M GB 提醒”（不自动删）；Cache 默认 LocalAppData + ACL。

### P1-11 · 退出收尾失败不可见；`on_exit` 没有日志/进度

**证据**
- `wait_for_recording_jobs` 失败只写 `self.error`：
  [main_page.rs](../src/ui/main_page.rs#L1290)。
- `App::on_exit` 没有把错误写 `cap.log`，也没有 UI：
  [main.rs](../src/main.rs#L965)。

**风险**：退出时导出失败，用户只看到进程消失；Cache 虽保留，但无提示。

**建议**：`on_exit` 把 `session_issue`/error 写 `cap.log`，并在关闭前尽量显示一次结果；
至少用 `MessageBox`/toast 告知“导出失败，分片保留”。

### P1-12 · `stop_recording` 线程创建失败时退回 UI 线程同步析构 `Recorder`

**证据**
- 线程 `spawn` 失败分支： [main_page.rs](../src/ui/main_page.rs#L1155-L1175)。
- 闭包被 drop 时 `Recorder::Drop` 调 `stop()`，在 UI 线程 join + 导出。

**风险**：极端资源压力下，本来应该“失败快”的路径反而变成 UI 长阻塞。

**建议**：spawn 失败时只设置错误/保留分片，不要在主线程同步 finalize；用 `StartGuard` 式清理。

---

## 4. 低危 / 加固项（P2）

1. **`SystemStop` 空闲时 `continue` 会丢掉同一批后续托盘命令**：
   [main.rs](../src/main.rs#L720-L727)。应只跳过本条，不要把 `ShowWindow/Quit` 一起丢。
2. **单实例互斥体可被同会话进程抢占**：攻击者可先创建 `Local\CapLite.SingleInstance`
   让程序直接退出（DoS）。低危，但可以在失败时给 MessageBox/日志。
3. **`find_main_window` 缓存 HWND 只用 `IsWindow` 校验**：句柄复用可能指向别的窗口；
   建议缓存时重新校验类名/PID/标题前缀。
4. **`i18n` overlay 可被放置任意 JSON 改写 UI 文案**（钓鱼/误导）；配合大小限制与 schema 校验。
5. **`TRAY_ICON_PRESENT` 是进程级全局**：多 `Tray` 实例/测试会互相影响；自有窗口场景低危。
6. **`record_paths` 最多 1000 次 `exists()`**：慢盘/网络 Cache 会阻塞 UI（同 P1-1）。
7. **非 UTF-8 路径经 `to_string_lossy` 传给 FFmpeg**：可能打开错文件或失败；
   建议用宽字符接口/显式错误。
8. **`longPathAware` 只保证 Win32，FFmpeg 的 `avio_open` 是否支持长路径未验证**；
   超长路径应显式拒绝或提示。
9. **没有 UI 顶层 `catch_unwind`、没有 minidump**：UI 线程 panic 仍会直接结束进程；
   panic hook 只有一行。建议 catch + crash dump。
10. **CFG/CET 未配置/未验证**：建议 `dumpbin /loadconfig` 检查，评估 `/guard:cf`。
11. **`run-dev.ps1` 仍引用不存在的 `_rust\env.ps1`**：本副本的便捷开发路径会退化；
    README 已改 `_build-env`，脚本也应同步。
12. **`dist\` / `manual-test\` 未用本次 release 重签/重打包**：用户若运行 dist 里的旧 exe，
    不会包含本轮修复；正式发布前必须重签、重打包、更新 SHA256。

---

## 5. 建议整改顺序

### 第一批（P0，先做）
1. start/stop/finalize 看门狗 + 超时 + 强制退出路径。
2. 静音 gap 的 A/V 同步修复（不要再“只跳音频”）。
3. Cache 默认 LocalAppData + 拒绝 reparse point + 停止启动时自动删除派生缓存。
4. 录制指示：动态托盘图标/tooltip/开始 toast；热键启动默认不隐藏。
5. 解码 helper 进程（或至少 `max_streams`/大小/时长上限 + fuzz）。
6. DLL 哈希/版本留痕 + 提升+可写目录警告；正式签名。

### 第二批（P1）
- UI 文件 I/O 后台化/超时；配置异步写。
- producer `catch_unwind` + channel 断开即失败。
- editor cancel 不 join；clip-open 可取消。
- prewarm 锁外 drop。
- 导出空间预检（含 faststart=false）。
- config/locale 大小上限。
- 日志脱敏/轮转/ACL。
- 独立会话通知窗口。

### 第三批（P2）
- SystemStop 命令队列、find_main_window 校验、i18n schema、长路径/非 UTF-8、
  UI catch_unwind/minidump、CFG/CET、run-dev/dist 收尾。

---

## 6. 建议的验证用例（下一轮应跑）

1. **卡死注入**：在 `av_interleaved_write_frame`、`avformat_open_input`、DXGI `grab` 前
   注入 10 分钟延迟；断言 UI 仍可交互、关闭有超时、Cache 分片可播。
2. **A/V 漂移**：人为让音频消费线程停顿 1 s / 3 s；断言产物音轨时长与视频差在容差内，
   或明确停止并提示。
3. **junction 攻击**：在临时目录建 `Cache\derived` junction 指向测试目录；以普通权限
   运行启动清理，断言不删除目标文件；再以提升权限验证警告/拒绝。
4. **热键隐私**：从另一个进程 `PostMessage(WM_HOTKEY)`；断言有可见录制指示，
   且默认不隐藏窗口；再验证用户显式允许隐藏的路径。
5. **畸形媒体**：构造多流/超大 SPS/损坏 moov/超大 metadata 的样本，监控
   主进程内存/CPU；断言拒绝或 helper 隔离。
6. **网络路径**：把 `output_dir`/config dir 指向断开的 UNC，断言启动/保存超时且 UI 不冻结。
7. **DLL 替换**：替换一个开头的 DLL 为同名不同版本，断言日志记录实际路径/哈希/版本，
   并在提升+可写目录时给出警告。
8. **签名/清单**：`Get-AuthenticodeSignature`、`mt.exe -inputresource`、`dumpbin /loadconfig`。

---

## 7. 复审边界

- 本轮**没有改动代码**；上述均为静态审查/自动化实测结论。
- 未在真机验证：真实锁屏/合盖休眠、显示器热插拔、Explorer 重启、
  低磁盘 VHD、网络断开、管理员+junction 组合、长时间 4K 录制。
- 部分结论是“按代码路径推断”（如 junction 删除、超时缺失），建议按第 6 节用例复现后再修。
- `D:\WorkbuddyWW\CapLite` 是另一份工作副本，其 `config.rs` 有独立 P56 改动；
  不要把本目录整体覆盖过去，先 diff/合并。
