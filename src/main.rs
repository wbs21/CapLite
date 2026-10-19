// 只在 **release** 下切到 Windows 子系统：GUI 程序不该带控制台窗口（双击 exe 时多一个黑窗）；
// debug 保留控制台，方便直接看日志与 panic 回溯。必须是**文件最前面**的内部属性，
// 排在 crate 文档注释前面也一样合法（两者都是内部属性，只要都在任何 item 之前）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 屏幕录制程序入口：`录制 / 设置 / 剪辑` 三标签页外壳。
//!
//! 外壳只做四件事：
//! * 持有 `Config` / `I18n` 与**录制状态**（`Recorder` 与最近一次结果），
//! * 配置一改就 `sanitized()` + `save()`，失败在界面顶部可见（绝不 panic），
//! * 给 egui 装一个中文字体（内置字体没有汉字字形，否则中文全是方框），
//! * 在"主页面 → 剪辑页"之间做交接（剪辑页只按构造时的 `Config` 取视频）。
//!
//! 业务逻辑都在 `cap` 库里，便于集成测试与 `src/bin/` 下的烟雾测试复用。

use caplite::config::{plan_output_dir, Config, ConfigStore};
use caplite::i18n::I18n;
use caplite::recorder::{Recorder, RecordingResult};
use caplite::ui::editor::EditorPage;
use caplite::ui::main_page::{MainCtx, MainPage};
use caplite::ui::settings::SettingsPage;
use caplite::ui::startup as startup_marks;

use eframe::egui;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// **运行时窗口图标**的裸 RGBA 字节（256×256×4 = 262 144 B，逐行、**非预乘**）。
///
/// 与 exe 的 **PE 资源图标**（`build.rs` 用 `rc.exe` 把 `assets/caplite.ico` 嵌进资源节）
/// **完全是两件事**：
/// * PE 资源图标 = 文件在资源管理器/文件属性里显示的那个，由 shell 读取 exe 资源节；
/// * **窗口图标**（标题栏左上角 + 任务栏按钮）= 进程运行时由窗口库调 `WM_SETICON` 现场设置的，
///   系统**不会**拿 PE 资源图标去补 —— 所以只嵌资源不设 `ViewportBuilder::with_icon` 时，
///   窗口显示的是 winit/系统的默认图标（历史症状）。见 `docs/DESIGN.md` §11.26。
///
/// 字节来源：`python scripts/make-icon.py`（幂等，设计参数在脚本 docstring）。
/// `include_bytes!` 的路径相对**当前源文件**（`src/`），所以是 `../assets/…`；
/// 这个文件缺失/尺寸变了会**编译期**报错（比运行时拿到错尺寸的缓冲区好）。
const WINDOW_ICON_RGBA: &[u8] = include_bytes!("../assets/caplite-256.rgba");

/// 与 [`WINDOW_ICON_RGBA`] 必须一致（脚本里叫 `RGBA_SIZES`）。egui 不校验 `rgba.len()`，
/// 对不上时会静默画出错位的图，所以这里用一条编译期断言把两个数字钉在一起。
const WINDOW_ICON_PX: u32 = 256;

const _: () = assert!(
    WINDOW_ICON_RGBA.len() == (WINDOW_ICON_PX * WINDOW_ICON_PX * 4) as usize,
    "assets/caplite-256.rgba 的字节数与 WINDOW_ICON_PX 对不上：重跑 scripts/make-icon.py"
);

/// 关闭提醒弹窗的**固定自动关闭时长**（P13，用户明确要求 6 秒、且不做悬停重置）。
///
/// 为什么写死而不可配：它只是"防止用户离开电脑时窗口一直挂着"的兜底，
/// 不是一个功能开关；写死之后"到点会发生什么"只有一种可能，行为最好推理。
const CLOSE_DIALOG_AUTO_SECS: std::time::Duration = std::time::Duration::from_secs(6);

/// 配置落盘的**最小间隔**（P45，健壮性/资源审查）：滑条拖动期间最多每 200 ms 写一次盘。
///
/// 为什么是 200ms：人眼对"松手后立刻生效"的容忍度远大于 200ms，而这个间隔足以把
/// "拖一次滑条写上百次盘"压到 5 次/秒以下；再长就会让"关窗口前的最后一次改动"
/// 更容易落在节流窗口里（虽然 `on_exit` 会补写，但少一次补救总是好的）。
const CFG_SAVE_MIN_GAP: std::time::Duration = std::time::Duration::from_millis(200);

/// 节流判定（纯函数，便于单测）：距上次落盘不足 [`CFG_SAVE_MIN_GAP`] 就先不写。
///
/// `last == None`（本次运行还没存过）永远为真 —— 第一次改动立刻落盘，
/// 不能让"启动后第一次拖滑条"也被推迟。
fn cfg_save_due(last: Option<Instant>, now: Instant) -> bool {
    match last {
        None => true,
        Some(t) => now.saturating_duration_since(t) >= CFG_SAVE_MIN_GAP,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Home,
    Settings,
    Editor,
}

/// 顶部提示条。按**严重度分级**（见 [`caplite::ui::theme::Banner`]）：
/// * `Info` —— 正常信息：目录**自动回退**、设置已保存……是预期行为，绝不能用红色；
/// * `Warn` —— 能继续但结果可能受影响（本次录制没有声音等）；
/// * `Error` —— 真失败：配置完全存不下、加载失败、保存失败……红色 + 加粗 + 不自动消失。
struct Notice {
    /// 提示类别（同类只显示一次；关闭后本会话不再出现）
    id: &'static str,
    kind: caplite::ui::theme::Banner,
    text: String,
    /// true = 只能手动关闭（真失败）；false = 问题自行消失时（例如保存成功）由代码撤下
    sticky: bool,
}

struct App {
    cfg: Config,
    i18n: I18n,

    /// 配置文件位置（已探测过可用性；不可写时也不会阻塞界面）
    store: ConfigStore,

    tab: Tab,
    /// 上一帧的标签页：用来识别"刚刚切到剪辑页"
    prev_tab: Tab,
    /// 已经交给剪辑页的输出目录值：避免每帧都 `set_output_dir`（纯冗余调用，没必要）
    editor_dir_synced: Option<String>,
    main: MainPage,
    settings: SettingsPage,
    editor: EditorPage,

    /// 录制会话（录制中为 `Some`）
    recorder: Option<Recorder>,
    /// 最近一次录制的结果（停止后填充）
    last_result: Option<RecordingResult>,

    /// 顶部提示条（可同时存在多条，例如"配置目录回退" + "输出目录回退"）
    notices: Vec<Notice>,
    /// 本会话已经关掉的提示类别：关掉之后同类不再冒出来
    dismissed: Vec<&'static str>,
    /// 启动时若"输出目录"被回退改写过：等首帧给用户提示之后，再把新目录落盘一次
    output_dir_dirty: bool,

    /// 流文件所在的 Cache 目录（程序所在目录优先，不可写时退到配置目录）
    cache_dir: std::path::PathBuf,
    /// **本次运行**是否已经弹过"请清理 Cache"的关闭提醒（内存标志，不写配置）
    cache_warned: bool,
    /// 正在显示关闭提醒窗口
    show_cache_close_dialog: bool,
    /// 关闭提醒窗口的**自动关闭截止时刻**（P13：固定 6 秒倒计时）。
    ///
    /// 只在弹窗期间是 `Some`；弹窗一关（点按钮或倒计时到点）就设回 `None` ——
    /// 所以**绝不会留着一个还活着的定时器**在弹窗消失后又去关窗口。
    /// 用户明确要求"固定 6 秒、不做悬停重置"，所以这里没有"鼠标移入就续期"的逻辑。
    close_dialog_deadline: Option<std::time::Instant>,
    /// 用户点了「知道了」：下一次关闭请求直接放行
    close_confirmed: bool,

    // ---- 托盘图标 + 全局快捷键（纯 Win32，见 `caplite::tray`）----
    /// 托盘 + 热键的持有者；`None` = 这台机器上没装成（**只影响托盘/热键，绝不影响录制**）。
    ///
    /// `None` 时**不会隐藏窗口**（否则窗口藏起来了却没有图标能把它叫回来）。
    tray: Option<caplite::tray::Tray>,
    /// 独立的会话/电源/显示通知窗口（不依赖托盘是否安装成功）。
    _session_watcher: Option<caplite::session::SessionWatcher>,
    /// "谁隐藏谁恢复"的状态机（见 [`caplite::tray::HideState`]）
    hide: caplite::tray::HideState,
    /// 上一次推给托盘的录制状态（`None` = 还没推过）。
    tray_recording: Option<bool>,
    /// 上一次刷新 tooltip 的录制秒数（每秒最多 NIM_MODIFY 一次）。
    tray_recording_secs: u64,
    /// 已推给 Win32 菜单文案的语言：语言一变就把三项文案重新推过去
    tray_menu_lang: String,
    /// 当前**真的注册着**的热键（`None` = 没注册）；换键失败时它还是旧的那个（回滚的落点）
    hotkey_applied: Option<caplite::tray::Hotkey>,
    /// 设置页正在捕获自定义组合 → 全局热键被**临时注销**（否则按下的就是当前组合，
    /// Windows 会直接触发开始/停止录制，根本到不了输入框）
    hotkey_suspended: bool,
    /// 上一帧的"捕获中"状态（用来识别边沿，而不是每帧重复注销/注册）
    hotkey_capture_prev: bool,
    /// "想隐藏但没有路回来"这条日志只记一次（否则每帧刷屏）
    hide_blocked_logged: bool,

    // ---- 启动阶段打点 / 延后初始化（见 `docs/DESIGN.md` 的启动优化一节）----
    /// 已经渲染过的帧数（从 1 开始）：首个非零值代表"窗口已经出画面了"
    frame_index: u32,
    /// 启动日志落点（`StartupLog::log_fn()`；`None` = 不记）
    startup_log: Option<fn(&str)>,
    /// 系统完整中文字体**还没**追加进 egui（延后到首帧之后，见 [`install_full_cjk_font`]）
    pending_full_font: bool,
    /// 系统完整中文字体是否已经追加过（避免重复追加、重复重建图集）
    full_font_done: bool,
    // P46：这里原来还有一个 `pending_editor_open`（"首帧之后才自动载入的上次视频"）。
    // 用户要求剪辑页**只认显式打开**，那条自动载入连同这个字段一起删了 ——
    // 于是"剪辑页上看到的永远是用户自己刚才指定的那一条"，启动时也不再走 FFmpeg。

    /// 上一次真正把配置写进磁盘的时刻（P45：滑条拖动时按 [`CFG_SAVE_MIN_GAP`] 节流）。
    cfg_saved_at: Option<Instant>,
    /// 有改动被节流推迟、还没落盘：等窗口过去（或松开鼠标 / 退出）时补一次。
    cfg_save_pending: bool,
    /// 后台配置保存器（UI 线程不做 sync_all/rename）。
    cfg_saver: Option<ConfigSaver>,
    /// 最近一次已入队的配置保存 id；`None` = 没有在等结果。
    cfg_save_inflight: Option<u64>,

    /// 仅测试用：本帧实测的**外壳高度**（窗口高 − 页面可用高：标签栏 + 状态栏 + 中央面板的
    /// 内边距都算在内）。`caplite::ui::layout::CHROME_H` 就是它量出来的，
    /// `main.rs` 的单测会重新量一遍并断言一致。
    #[cfg(test)]
    test_chrome_h: Option<f32>,
    /// 仅测试用：本帧**页面可用高度**（中央面板内层 `Ui::max_rect().height()`）。
    #[cfg(test)]
    test_page_avail_h: Option<f32>,
}

impl App {
    /// 构造整个应用。
    ///
    /// 参数是 `&egui::Context` 而不是 `&eframe::CreationContext`：
    /// 这里唯一用到 CreationContext 的地方就是它怀里的 `egui_ctx`（装字体 + 应用主题），
    /// 而**无窗口单测**需要一个能构造的入口（`CreationContext` 在测试里造不出来）。
    /// 收窄到 `Context` 之后，真机与单测走的是**同一个构造函数**，
    /// 布局测量才可能和真机一致（见 `ui::layout`）。
    fn new(
        ctx: &egui::Context,
        store: ConfigStore,
        log: StartupLog,
        explicit_config_dir_arg: bool,
        // 是否安装托盘图标 + 注册全局快捷键。
        // 只有**真机**（`main()`）传 true：无窗口单测里建 message-only 窗口、
        // 往通知区域塞图标、抢全局热键都属于"副作用"，测试不该做。
        enable_tray: bool,
    ) -> Self {
        // `run_native` 内部先建窗口/GL(DX12) 上下文，再回调这里 —— 所以"到这里"就代表
        // eframe/wgpu 初始化已经完成，这一步是启动路径上最大的一块（见 docs 实测表）。
        startup_marks::mark_dur_log(
            log.log_fn(),
            "eframe_backend_ready",
            "process",
            startup_marks::start().elapsed(),
        );
        let app_t0 = Instant::now();
        install_cjk_font(ctx, &log);
        // 主题（浅色/深色）在**配置读出来之后**才应用 —— 应用哪一种由
        // `cfg.dark_mode` 决定（P40），所以这里不能再"启动时无条件应用一次浅色"。
        // 首次应用前的空档里没有任何控件被画过（下面才开始建 `CentralPanel`），
        // 因此不存在"先按默认主题画一帧、再闪一下换肤"的问题。
        startup_marks::mark_dur_log(log.log_fn(), "font_subset_installed", "app", app_t0.elapsed());

        // ① 配置目录：已经在 `main()` 里探测好（`store`：显式指定 → 用户\Documents\cap
        //    → exe 同级 → 临时目录，探测方式只创建目录并写一个探测文件）。
        //    为什么挪到 `main()`：**启动日志的落点要用同一个探测结果** —— 不然日志可能落到
        //    不可写的目录，或者和配置分处两地。
        // ② 读配置：读不出来就回落默认值，并把原因留给提示条。
        //    若配置目录本身不可写，界面已经会显示"设置不会保留"，就不再叠加第二条错误。
        let cfg_t0 = Instant::now();
        let (cfg, load_err) = match store.load() {
            Ok(c) => (c, None),
            Err(e) => {
                log.line(&format!("[cap] 配置加载失败，已回落默认值: {e}"));
                if store.writable() {
                    (Config::default(), Some(e.to_string()))
                } else {
                    (Config::default(), None)
                }
            }
        };
        startup_marks::mark_dur_log(log.log_fn(), "config_loaded", "app", cfg_t0.elapsed());

        // ③ 主题：配置里存着"深色模式"与否（P40）。**深色是默认**（P42：用户要求
        //    "设置默认主题为黑色"）—— 老配置没有 `dark_mode` 键 → `true`；
        //    显式写了 `false` 的用户保持浅色（选择赢过默认）。
        //    `apply_kind` 同时做三件事：设置线程局部 [`ThemeKind`]（`theme::*` 取色函数的
        //    唯一依据）、把 egui 的 `ThemePreference` 钉成对应值（**不跟随系统**）、
        //    并把同一套样式写到 Light 与 Dark 两个主题键下。
        let theme_t0 = Instant::now();
        caplite::ui::theme::apply_kind(
            ctx,
            caplite::ui::theme::ThemeKind::from_dark_mode(cfg.dark_mode),
        );
        startup_marks::mark_dur_log(log.log_fn(), "theme_applied", "app", theme_t0.elapsed());

        // ③ 输出目录：`配置里的值` → 用户\视频\cap → 用户\文档\cap → exe 同级\cap-recordings
        //    → 临时目录，逐个"真写一个临时文件"探测，避免用户在录制时才没权限。
        //
        // **注意**：配置里的那个目录**必须**在这里验证（否则第一帧就会显示"目录不可写"的
        // 错误提示，而那是假错误）；所以这里只探测**配置值这一个目录**，
        // 完整回退链（视频/文档/exe 同级/临时目录，最多 4 次真写文件）交给
        // `MainPage::flush_deferred_init` 在首帧之后做。探测结果一致：`plan_output_dir`
        // 以配置值开头，配置值可用时它按定义就是 `chosen`。
        let dir_t0 = Instant::now();
        let mut cfg = cfg;
        let mut output_dir_dirty = false;
        let configured_ok = cfg.output_dir.trim().is_empty()
            || caplite::config::resolve_against_exe_dir(&cfg.output_dir)
                .map(|d| caplite::config::probe_dir_writable(&d).is_ok())
                .unwrap_or(false);
        let plan_deferred = !configured_ok;
        if plan_deferred {
            // 配置值不可用：这一种情况必须**立刻**走完整回退链 —— 不能让首帧显示
            // "输出目录不可写"，那对用户是恐慌，而不是事实（程序自己会换个能写的位置）。
            let plan = plan_output_dir(&cfg.output_dir, &caplite::config::probe_dir_writable_timeout);
            if let Some(chosen) = plan.chosen_path() {
                let chosen_text = chosen.to_string_lossy().to_string();
                if cfg.output_dir.trim() != chosen_text {
                    cfg.output_dir = chosen_text;
                    output_dir_dirty = true;
                }
                if let Some(from) = &plan.fell_back_from {
                    log.line(&format!(
                        "[cap] 输出目录不可写（{}），已自动改用 {}",
                        from.display(),
                        cfg.output_dir
                    ));
                }
            }
        }
        startup_marks::mark_dur_extra_log(
            log.log_fn(),
            "output_dir_checked",
            "app",
            dir_t0.elapsed(),
            if plan_deferred { "配置值不可用，已走完整回退链" } else { "配置值可用，回退链延后" },
        );
        // ④ 语言与外部语言包（`locales/` 以 exe 目录为基准，双击 exe 时不依赖 CWD）
        let i18n_t0 = Instant::now();
        let mut i18n = I18n::new(&cfg.language);
        if let Some(dir) = exe_side_locales() {
            i18n.overlay_dir(&dir);
        }
        startup_marks::mark_dur_log(log.log_fn(), "i18n_loaded", "app", i18n_t0.elapsed());

        // ⑤ **每次启动都默认"全屏录制"**（产品决策，用户明确要求）：
        // 不沿用上次保存的"区域"模式 —— 区域框选是一次性的定位动作，用户下次打开
        // 十有八九只是想整屏录。区域坐标**仍然保留在配置里**（`cfg.region` 不动），
        // 切到"区域录制"时可以直接复用上次的框；只是**启动模式固定为全屏**。
        cfg.capture_mode = caplite::config::CaptureMode::FullScreen;

        if let Some(p) = store.path().parent() {
            log.line(&format!("[cap] 配置文件目录: {}", p.display()));
        }
        log.line(&format!("[cap] 输出目录: {}", cfg.output_dir));

        // 提示条：**按严重度分级**，不再把所有东西揉成一条红字。
        //
        // 关键判断：目录回退是**预期行为**（程序自己挑了一个能写的位置），属于"正常信息"，
        // 用 info（中性蓝灰）；只有"一个能写的目录都没有 / 配置读不出来"才是真失败。
        //
        // 频率控制（用户连续两次反馈"这条提示还在，太打扰"）：
        // **自动回退全程不在界面提示** —— 目录是程序自己挑的，属于预期行为，
        // 只在**日志文件**里记（release 无控制台，所以落盘；debug 打控制台）：
        // 上面的 `[cap] 配置文件目录/输出目录` 两行，
        // 界面始终在底部状态栏左侧显示**实际生效的输出目录**，这就是用户需要的信息。
        // 只有"**用户显式指定**过目录却不可用"才算真错误：红条、不自动消失。
        let explicit_config_dir = explicit_config_dir_arg
            || std::env::var("CAP_CONFIG_DIR")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            || cfg
                .config_dir
                .as_deref()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false);
        let mut notices: Vec<Notice> = Vec::new();
        if !store.writable() {
            // 真失败：设置根本存不下（内存运行）——必须可见，且不自动消失
            let reason = store.notice.clone().unwrap_or_default();
            notices.push(Notice {
                id: "config_dir_unwritable",
                kind: caplite::ui::theme::Banner::Error,
                text: i18n.tf("err_config_memory_only", &[&reason]),
                sticky: true,
            });
        } else if store.notice.is_some() && explicit_config_dir {
            // 用户显式指定的目录不可写，程序**替他**改用了别处：这是真错误，必须红条
            let to = store
                .path()
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            notices.push(Notice {
                id: "config_dir_explicit_unwritable",
                kind: caplite::ui::theme::Banner::Error,
                text: i18n.tf("err_config_dir_explicit_fallback", &[&elide_middle(&to, 42)]),
                sticky: true,
            });
        }
        if let Some(e) = load_err {
            notices.push(Notice {
                id: "config_load_failed",
                kind: caplite::ui::theme::Banner::Error,
                text: i18n.tf("err_load_failed", &[&e]),
                sticky: true,
            });
        }
        // 输出目录的自动回退同样**只进日志**（录制页那一行显示的就是实际生效的目录）

        // 流文件的 Cache 目录：程序所在目录优先（`<exe>\Cache`），不可写时退配置目录。
        // 只在**关闭软件**时用来判断"要不要提醒用户清理"。
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let cfg_dir = store.path().parent().map(|d| d.to_path_buf());
        let cache_t0 = Instant::now();
        let cache_dir = caplite::cache::resolve_cache_dir(
            exe_dir.as_deref(),
            cfg_dir.as_deref(),
            &|p| caplite::config::probe_dir_writable_timeout(p).is_ok(),
        );
        log.line(&format!("[cap] 临时文件目录(Cache): {}", cache_dir.display()));
        startup_marks::mark_dur_log(log.log_fn(), "cache_dir_ready", "app", cache_t0.elapsed());
        // 派生缓存（波形峰值）可以安全修剪；`recordings` 子目录永远不在这里被碰。
        // 放后台线程：启动路径不该为几十个文件的 stat/delete 等待。
        {
            let derived = caplite::cache::derived_dir(&cache_dir);
            let _ = std::thread::Builder::new()
                .name("cache-prune".into())
                .spawn(move || {
                    let removed = caplite::cache::prune_derived(
                        &derived,
                        64,
                        std::time::Duration::from_secs(30 * 24 * 3600),
                    );
                    if removed > 0 {
                        tracing::info!(removed, dir = %derived.display(), "已修剪派生缓存");
                    }
                });
        }

        let mut main = MainPage::new();
        main.set_cache_dir(cache_dir.clone());
        let settings = SettingsPage::new();
        // **P46：剪辑页构造时只初始化状态，不打开任何文件。**
        //
        // 改造前这里会先记下 `cfg.last_video_path`，等首帧之后再去自动载入"上次的视频"
        // （为了不拖慢首帧，那一步被推迟到 [`App::flush_deferred_init`]）。用户要求改成
        // "剪辑页只认显式打开"，于是整条链都删了：现在只有录制页底部的「在剪辑页打开」
        // 和剪辑页自己的「打开视频」/「重新载入」会把文件交给它。
        let editor_t0 = Instant::now();
        let mut editor = EditorPage::new(&cfg);
        // 波形基准层的落盘缓存也放这个 `Cache\`（沿用同一条回退链，**不写用户文档目录**）：
        // 缓存目录只在这里解析一次，剪辑页不再自己算一遍（避免两份"同一个目录"的推导）。
        // 波形峰值是**可再生成**的派生数据：单独放 `Cache\derived`，允许按年龄/数量
        // 自动修剪；录制分片在 `Cache\recordings`，永不自动删除。
        let derived_dir = caplite::cache::derived_dir(&cache_dir);
        match caplite::cache::ensure_plain_dir(&derived_dir) {
            Ok(()) => editor.set_wave_cache_dir(Some(derived_dir)),
            Err(e) => {
                tracing::warn!(dir = %derived_dir.display(), error = %e, "波形缓存目录不安全，本次不落盘");
                editor.set_wave_cache_dir(None);
            }
        }
        startup_marks::mark_dur_log(
            log.log_fn(),
            "editor_page_ready",
            "app",
            editor_t0.elapsed(),
        );

        let ui_build_t0 = Instant::now();
        let mut main = main;
        main.set_startup_log(log.log_fn());
        startup_marks::mark_dur_extra_log(
            log.log_fn(),
            "ui_pages_ready",
            "app",
            ui_build_t0.elapsed(),
            format!("app_new 总耗时 {}ms", app_t0.elapsed().as_millis()),
        );

        // ⑥ **托盘图标 + 全局快捷键**（见 `docs/DESIGN.md` 的托盘一节）。
        //
        // 为什么在这里建：`App::new` 就是 eframe 的事件循环线程（= 主线程）在窗口
        // 与 GPU 上下文就绪后回调的地方，而 Win32 的消息**只被创建窗口的那条线程**的
        // 消息循环取走 —— 建在后台线程上，托盘菜单与 `WM_HOTKEY` 就永远不会被派发。
        //
        // 失败分级（硬要求）：
        // * **托盘整个装不上**（窗口/图标/`Shell_NotifyIconW`）→ 只记 `cap.log` 一行，
        //   `tray = None`，**不隐藏窗口**，录制等一切照常；
        // * **只是热键被占用** → 托盘照常可用，另外给一条界面 warn（文案走 i18n），
        //   并把 `GetLastError` 与候选建议一起记进 `cap.log`。
        let mut tray: Option<caplite::tray::Tray> = None;
        let mut hotkey_applied: Option<caplite::tray::Hotkey> = None;
        if enable_tray {
            // 有效组合的**唯一算法**在这里被调用；设置页显示、告警文案也复用它
            let want = if cfg.global_hotkey_enabled {
                Some(caplite::tray::effective_hotkey(
                    cfg.hotkey_preset,
                    &cfg.hotkey_custom,
                ))
            } else {
                None
            };
            let tip = i18n.t("tray_tip").to_string();
            match caplite::tray::Tray::install(
                ctx,
                WINDOW_ICON_RGBA,
                WINDOW_ICON_PX,
                &tip,
                [
                    i18n.t("tray_menu_show"),
                    i18n.t("tray_menu_stop"),
                    i18n.t("tray_menu_quit"),
                ],
                want,
            ) {
                Ok(t) => {
                    hotkey_applied = t.current_hotkey();
                    let failure = t.hotkey_error().cloned();
                    let ready = t.current_hotkey();
                    // 图标挂不上（例如**进程完整性级别与 shell 不同**导致
                    // `Shell_NotifyIconW` 返回 `GetLastError=5 拒绝访问`）：只进日志，
                    // 消息窗口与全局热键照常工作（它们不需要跨进程消息）。
                    let icon_err = t.icon_error().map(str::to_string);
                    tray = Some(t);
                    if let Some(e) = icon_err {
                        log.line(&format!(
                            "[cap] 托盘图标未能挂到通知区域（只影响图标/菜单，不影响录制与全局快捷键）：{e}"
                        ));
                    }
                    match failure {
                        Some(e) => {
                            // 注册失败：记日志（含 1409 与候选建议）+ 界面 warn 一条
                            log.line(&format!("[cap] {}", hotkey_log_line(&e)));
                            notices.push(hotkey_warning_notice(&i18n, &e));
                        }
                        None => log.line(&format!(
                            "[cap] 托盘已就绪（通知区域图标={}）；全局快捷键{}",
                            if tray.as_ref().is_some_and(|t| t.icon_ok()) {
                                "已挂上"
                            } else {
                                "未挂上"
                            },
                            match ready {
                                Some(hk) => format!(" = {}", hk.label()),
                                None => "已关闭".to_string(),
                            }
                        )),
                    }
                }
                Err(e) => {
                    // 连消息窗口都建不出来（托盘与热键都用不了）：只进日志
                    log.line(&format!("[cap] {}", i18n.tf("tray_unavailable", &[&e])));
                }
            }
        }
        let tray_menu_lang = i18n.language().to_string();
        // 与托盘解耦：即使 `Tray::install` 失败，锁屏/挂起/分辨率变化也要能停录。
        let session_watcher = if enable_tray {
            match caplite::session::SessionWatcher::install(ctx) {
                Ok(w) => Some(w),
                Err(e) => {
                    log.line(&format!("[cap] 会话通知窗口创建失败：{e}"));
                    None
                }
            }
        } else {
            None
        };

        Self {
            cfg,
            i18n,
            store: store.clone(),
            tab: Tab::Home,
            prev_tab: Tab::Home,
            editor_dir_synced: None,
            main,
            settings,
            editor,
            recorder: None,
            last_result: None,
            notices,
            dismissed: Vec::new(),
            output_dir_dirty,
            cache_dir,
            cache_warned: false,
            show_cache_close_dialog: false,
            close_dialog_deadline: None,
            close_confirmed: false,
            tray,
            _session_watcher: session_watcher,
            hide: caplite::tray::HideState::default(),
            tray_recording: Some(false),
            tray_recording_secs: 0,
            tray_menu_lang,
            hotkey_applied,
            hotkey_suspended: false,
            hotkey_capture_prev: false,
            hide_blocked_logged: false,
            frame_index: 0,
            startup_log: log.log_fn(),
            pending_full_font: SUBSET_FONT_ACTIVE,
            full_font_done: false,
            cfg_saved_at: None,
            cfg_save_pending: false,
            cfg_saver: Some(ConfigSaver::new(store.clone())),
            cfg_save_inflight: None,
            #[cfg(test)]
            test_chrome_h: None,
            #[cfg(test)]
            test_page_avail_h: None,
        }
    }

    /// **延后初始化**（首帧之后立刻执行一次）：追加系统完整中文字体
    /// （兜底生僻字，见 [`install_full_cjk_font`]）。
    ///
    /// **P46**：这里原来还有一步"自动载入上次的视频"（`pending_editor_open` →
    /// `EditorPage::open`）。用户要求剪辑页只认显式打开，那一步已经删除 ——
    /// 现在本函数只剩字体一件事，剪辑页在启动阶段完全不动。
    ///
    /// 显示器/音频枚举与目录回退链由 [`MainPage`] 自己在其第 2 帧完成。
    /// 全程**不阻塞首帧**：调用点在 `ui()` 的最后，此时首帧的绘制命令已经提交。
    fn flush_deferred_init(&mut self, ctx: &egui::Context) {
        if self.pending_full_font && !self.full_font_done {
            self.pending_full_font = false;
            self.full_font_done = true;
            // 完整 CJK 字体 15–25 MB + 一次字体图集重建。英文界面不需要它：
            // 内嵌子集已经覆盖 UI 文案，文件名里的生僻字再按需处理。
            if !self.i18n.language().starts_with("zh") {
                startup_marks::note(
                    self.startup_log,
                    "[cap] UI language is not zh; skip the 15-25 MB full CJK font fallback",
                );
                return;
            }
            let t0 = Instant::now();
            install_full_cjk_font(ctx);
            startup_marks::mark_dur_log(
                self.startup_log,
                "full_font_deferred_loaded",
                "app",
                t0.elapsed(),
            );
        }
    }

    /// 加一条提示：**同一类别只留一条**，且"用户已经关掉过的类别"不再出现
    /// （目录回退这类提示不会因为刷新/重绘反复冒出来）。
    fn push_notice(&mut self, notice: Notice) {
        if self.dismissed.contains(&notice.id) {
            return;
        }
        if let Some(slot) = self.notices.iter_mut().find(|n| n.id == notice.id) {
            *slot = notice; // 同类更新文案，而不是再堆一条
            return;
        }
        self.notices.push(notice);
    }

    /// 配置改动后的统一收口：夹紧非法值 → **节流**落盘 → 失败写进可见提示。
    ///
    /// ## 为什么要节流（P45 健壮性/资源审查）
    ///
    /// 设置页的滑条一拖就是几十帧，而"调整即保存"意味着每帧都要落盘。
    /// 改造前更糟：设置页自己存一次、外壳拿到 `cfg_changed` 又存一次 —— **每帧两次**
    /// "序列化 + 临时文件 + rename"。一次拖拽 ≈ 上百次写盘（还都在 UI 线程上）。
    ///
    /// 现在：**内存立刻更新**（用户看到的就是最新值），落盘最多每 [`CFG_SAVE_MIN_GAP`]
    /// 一次；被推迟的那一次由 [`App::flush_cfg_save`] 在窗口过去、松开鼠标、退出时补上。
    /// 也就是说"最坏情况"只是磁盘上的值晚 200ms，**不会丢改动**。
    fn commit_cfg(&mut self) {
        self.cfg = self.cfg.clone().sanitized();
        let now = Instant::now();
        if !cfg_save_due(self.cfg_saved_at, now) {
            // 节流窗口内：先记账，等 `ui_root` 安排的那一帧（或松手/退出）再写
            self.cfg_save_pending = true;
            return;
        }
        self.save_cfg_now(now);
    }

    /// 真的写一次盘（`commit_cfg` 的"到期"分支，以及松手/退出时的补写）。
    ///
    /// 失败也记 `cfg_saved_at`：配置目录真的不可写时，不该每 200ms 重试一次
    /// （那只会刷屏 + 空转磁盘）；用户下一次改动会再试，提示条已经在那儿了。
    fn save_cfg_now(&mut self, now: Instant) {
        self.cfg_saved_at = Some(now);
        self.cfg_save_pending = false;
        let id = self
            .cfg_saver
            .as_mut()
            .and_then(|saver| saver.enqueue(&self.cfg));
        match id {
            Some(id) => self.cfg_save_inflight = Some(id),
            None => {
                // 保存线程已经不可用：退回同步保存（极罕见），至少不让改动丢。
                match self.store.save(&self.cfg) {
                    Ok(()) => self.notices.retain(|n| n.sticky),
                    Err(e) => self.push_cfg_save_error(&e.to_string()),
                }
            }
        }
    }

    fn push_cfg_save_error(&mut self, err: &str) {
        let text = self.i18n.tf("err_save_failed", &[&err]);
        self.push_notice(Notice {
            id: "config_save_failed",
            kind: caplite::ui::theme::Banner::Error,
            text,
            sticky: false,
        });
    }

    /// 收后台保存结果（每帧/隐藏时调用；绝不等待）。
    fn pump_cfg_saves(&mut self) {
        let results = self
            .cfg_saver
            .as_mut()
            .map(|saver| saver.drain())
            .unwrap_or_default();
        if results.is_empty() {
            return;
        }
        for (id, result) in results {
            if self.cfg_save_inflight == Some(id) {
                self.cfg_save_inflight = None;
            }
            match result {
                Ok(()) => {
                    // 保存成功了 → 撤下"能自愈"的提示（就是"保存失败"那条）；
                    // sticky（目录回退/加载失败）留到用户手动关闭。
                    self.notices.retain(|n| n.sticky);
                }
                Err(e) => self.push_cfg_save_error(&e),
            }
        }
    }

    /// 有被节流推迟的改动时补一次落盘（松手 / 退出 / 节流窗口到期时调用）。
    fn flush_cfg_save(&mut self) {
        if self.cfg_save_pending {
            self.save_cfg_now(Instant::now());
        }
    }

    /// 把录制产物交给剪辑页 —— **录制页底部「在剪辑页打开」按钮的唯一落点**（P46）。
    ///
    /// 三件事：
    /// ① 写 `cfg.last_video_path` 并落盘。它现在**只用来定位目录**（下次「打开视频」的
    ///    文件对话框从录制目录开始），**不再**驱动任何自动载入 —— 剪辑页启动时是空的；
    /// ② **显式**调用 [`EditorPage::open`] 让剪辑页立刻载入该文件
    ///    （剪辑页只在 `new()` 时读一次配置，不显式调用就不会刷新）；
    /// ③ 切到剪辑页（`self.tab = Tab::Editor`）。
    ///
    /// `streaming_fallback = true` 表示**导出失败**，这次交过去的是分片流文件
    /// （唯一副本，仍可正常播放/剪辑）：界面必须如实说明，不能让人以为那是最终产物。
    fn open_in_editor(&mut self, path: &Path, streaming_fallback: bool) {
        self.cfg.last_video_path = Some(path.to_string_lossy().to_string());
        self.commit_cfg();
        // 刚录完 → 输出目录里多了一条新录像：把最新目录交给剪辑页，
        // 这样「打开视频」的对话框默认落在录制文件夹。
        self.editor.set_output_dir(Path::new(&self.cfg.output_dir));
        self.editor_dir_synced = Some(self.cfg.output_dir.clone());
        self.editor.open(path);
        if streaming_fallback {
            let text = self.i18n.t("warn_editor_streaming_fallback").to_string();
            self.push_notice(Notice {
                id: "editor_streaming_fallback",
                kind: caplite::ui::theme::Banner::Warn,
                text,
                sticky: true,
            });
        }
        self.tab = Tab::Editor;
    }

    // -----------------------------------------------------------------------
    // 托盘 / 全局快捷键
    // -----------------------------------------------------------------------

    /// 处理托盘菜单与全局快捷键送来的命令。
    ///
    /// **必须同时被 `App::ui_root` 与 `App::logic` 调用**：窗口隐藏
    /// （`ViewportCommand::Visible(false)`）之后 eframe 不再调 `App::ui`，只调 `App::logic`
    /// （`update_logic_only`）。少了 `logic` 这一处，"隐藏到托盘之后按热键停止录制"
    /// 就永远不生效 —— 这正是本需求最容易踩空的地方。
    fn pump_tray(&mut self, ctx: &egui::Context) {
        for cmd in caplite::tray::take_commands() {
            match cmd {
                caplite::tray::TrayCommand::ShowWindow => {
                    self.restore_window_now(ctx, "托盘菜单「显示窗口」");
                    // 需求口径：置前/聚焦
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                // 全局快捷键：**同一个键切换**（未录制 → 开始；录制中 → 停止）
                caplite::tray::TrayCommand::ToggleRecording => self.recording_from_tray(),
                // 托盘菜单「停止录制」：窗口藏起来之后这是唯一够得着的停止入口之一
                caplite::tray::TrayCommand::StopRecording => {
                    if self.recorder.is_some() || self.main.is_busy() {
                        self.recording_from_tray();
                    }
                }
                // 托盘菜单「退出」：**先停止录制 / 导出收尾**（`stop()` 是同步的），再关窗。
                // 之后走的是本程序本来就有的关闭流程（Cache 非空时还会弹一次提醒）。
                caplite::tray::TrayCommand::Quit => {
                    self.main.request_quit_after_finish();
                    if self.recorder.is_some() {
                        self.recording_from_tray();
                    } else if !self.main.is_busy() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                // 锁屏/挂起/显示模式变化：正常停止并导出，再告诉用户为什么停了。
                caplite::tray::TrayCommand::SystemStop(reason) => {
                    if self.recorder.is_some() {
                        self.recording_from_tray();
                    } else if self.main.is_busy() {
                        // 还在 Starting：登记“启动完成后立刻停”，不要漏掉锁屏/挂起。
                        self.main.request_stop_after_start();
                    } else {
                        continue;
                    }
                    let text = self.i18n.tf("system_stop_warning", &[&reason]);
                    self.push_notice(Notice {
                        id: "system_stop",
                        kind: caplite::ui::theme::Banner::Warn,
                        text,
                        sticky: false,
                    });
                }
            }
        }

        // 录制中、或者窗口是我们藏起来的 → 持续要帧（时长/统计要刷新；
        // 也让"万一还有没落地的 viewport 命令"有机会在下一帧被处理）。
        if self.recorder.is_some() || self.main.is_busy() || self.hide.hidden_by_us() {
            // `main.is_busy()` 包括后台 start/stop：这段时间没有 recorder 可读，
            // 必须自己排帧去 `poll_recording_jobs`，否则结果到了界面也不会更新。
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // 语言换了 → 把菜单三项文案重新推给 Win32（托盘菜单拿不到 `I18n`）
        if self.tray.is_some() && self.tray_menu_lang != self.i18n.language() {
            self.tray_menu_lang = self.i18n.language().to_string();
            caplite::tray::set_menu_labels([
                self.i18n.t("tray_menu_show"),
                self.i18n.t("tray_menu_stop"),
                self.i18n.t("tray_menu_quit"),
            ]);
        }
    }

    /// 全局快捷键 / 托盘菜单触发的"开始或停止录制"。
    ///
    /// **复用录制页主按钮那一对函数**（`MainPage::start_recording` / `stop_recording`），
    /// 所以导出收尾、预热补建、错误文案与点击按钮**完全一致** —— 这里没有第二套逻辑。
    /// 失败同样走底部状态栏那条消息通道（用户可能不在录制页，看不到页面里的红字）。
    fn recording_from_tray(&mut self) {
        // 启动阶段按热键/托盘「停止」：不弹“忙”错误，登记启动完成后立刻停止。
        if self.main.is_busy() {
            self.main.request_stop_after_start();
            return;
        }
        if self.recorder.is_some() {
            let _ = self.main.stop_recording(
                &self.cfg,
                &mut self.recorder,
                &mut self.last_result,
                &self.i18n,
            );
        } else {
            let _ = self.main.start_recording(
                &self.cfg,
                &mut self.recorder,
                &mut self.last_result,
                &self.i18n,
            );
        }
        if let Some(e) = self.main.reportable_error() {
            self.push_notice(Notice {
                id: "record_error",
                kind: caplite::ui::theme::Banner::Error,
                text: e,
                sticky: false,
            });
        }
    }

    /// **立刻**把主窗口恢复可见并置前（"谁隐藏谁恢复"的恢复侧）。
    ///
    /// 两条路一起走（谁生效都不冲突）：
    /// * `ViewportCommand::Visible(true)` —— 规范路径，egui/winit 的状态机走这条；
    /// * 原始 Win32 `ShowWindow(SW_SHOW)` + `SetForegroundWindow` —— **立刻生效**的兜底。
    ///
    /// 为什么必须有兜底（本轮实测）：窗口隐藏之后 eframe 只跑 `App::logic`，而
    /// **隐藏期间不会再有新的帧**（`logic` 里持续 `request_repaint_after` 也一样）——
    /// 于是"把 `Visible(true)` 留到下一帧去应用"这条设计会把窗口**永久留在隐藏状态**
    /// （实测 `IsWindowVisible` 连续 5 秒为 false，且负责第二拍的 `logic` 再也没被调用）。
    /// 反过来，隐藏侧也用原始 Win32 同步隐藏，所以 winit 的 `VISIBLE` 标志与真实状态
    /// 无论命令有没有被应用都不会错位（顶多多一次幂等调用）。
    fn restore_window_now(&mut self, ctx: &egui::Context, why: &str) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        let (before, front, after) = caplite::tray::show_main_window_now();
        startup_marks::note(
            self.startup_log,
            &format!(
                "[cap] {why} → 发送 Visible(true) 并同步 Win32 恢复：\
                 置前={front}，主窗口 IsWindowVisible {before:?} → {after:?}"
            ),
        );
    }

    /// 录制状态迁移 → 隐藏/恢复窗口。**按钮、托盘菜单、全局快捷键三条入口共用**。
    ///
    /// 规则是"**谁隐藏谁恢复**"（见 [`caplite::tray::HideState`]）：
    /// * 录制**真正开始**（`Recorder::start` 成功 → `recorder` 是 `Some`）且开关为真 → 隐藏；
    /// * 录制停止/失败/导出完成后，**只有"是我们隐藏的"**才恢复可见 ——
    ///   绝不覆盖用户自己隐藏窗口的意图（所以"恢复"永远不受下面那个前提限制）；
    /// * 开关关闭时**一个 viewport 命令都不发**；
    /// * **进入隐藏还有一个前提：用户得有办法把窗口叫回来** —— 要么通知区域图标挂上了
    ///   （点菜单「显示窗口」），要么全局热键注册成功（按一下停止录制就恢复可见）。
    ///   两个都没有时**绝不隐藏**，否则窗口藏起来就再也回不来了（这是本环境实测到的
    ///   真实场景：沙箱里进程是低完整性级别，`Shell_NotifyIconW` 直接被拒）。
    fn sync_tray_visibility(&mut self, ctx: &egui::Context) {
        let recording = self.recorder.is_some();
        let can_come_back = self
            .tray
            .as_ref()
            .is_some_and(|t| t.icon_ok() || t.hotkey_registered());
        let want_hide = self.cfg.hide_to_tray_on_record_start && can_come_back;
        // 只在"用户想藏、但确实没路回来"时记一次日志，避免每帧刷屏
        if recording
            && self.cfg.hide_to_tray_on_record_start
            && !can_come_back
            && !self.hide_blocked_logged
        {
            self.hide_blocked_logged = true;
            startup_marks::note(
                self.startup_log,
                "[cap] 配置要求录制开始时隐藏窗口，但通知区域图标与全局快捷键都不可用 → \
                 本次不隐藏（否则窗口无法恢复）",
            );
        }
        match self.hide.poll(recording, want_hide) {
            caplite::tray::VisibilityAction::Nothing => {}
            caplite::tray::VisibilityAction::Hide => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                // 同步兜底：隐藏期间不会再有帧，命令有没有被应用都要**这一帧**就真的藏起来
                let raw = caplite::tray::hide_main_window_now();
                // 隐藏/恢复都留一行审计（这是"谁隐藏谁恢复"唯一的运行期证据）
                startup_marks::note(
                    self.startup_log,
                    &format!(
                        "[cap] 录制已开始（配置开关=开，且存在恢复入口）→ 发送 Visible(false) \
                         并同步 Win32 隐藏：ShowWindow(SW_HIDE)={raw}，\
                         主窗口 IsWindowVisible={:?}",
                        caplite::tray::main_window_visible()
                    ),
                );
                // 隐藏之后要靠 `App::logic` 继续跑：这里补一次是让"刚藏起来就立刻按热键停止"
                // 也能马上被处理（`WM_HOTKEY` 那条路本身也会 `request_repaint`）。
                ctx.request_repaint();
            }
            caplite::tray::VisibilityAction::Show => {
                self.restore_window_now(ctx, "录制已结束（停止/失败/导出收尾完成）");
            }
        }
    }

    /// 托盘录制指示：红点图标 + 每秒刷新的 tooltip；开始/停止各推一条可见提示。
    fn sync_tray_recording(&mut self) {
        let recording = self.recorder.is_some();
        let secs = self
            .recorder
            .as_ref()
            .map(|r| r.elapsed().as_secs())
            .unwrap_or(0);
        let changed = self.tray_recording != Some(recording);
        if !changed && !(recording && secs != self.tray_recording_secs) {
            return;
        }
        if let Some(tray) = self.tray.as_mut() {
            tray.set_recording(recording, std::time::Duration::from_secs(secs));
        }
        self.tray_recording = Some(recording);
        self.tray_recording_secs = secs;
        if changed {
            let (key, kind) = if recording {
                ("recording_started_notice", caplite::ui::theme::Banner::Warn)
            } else {
                ("recording_stopped_notice", caplite::ui::theme::Banner::Info)
            };
            self.push_notice(Notice {
                id: "record_indicator",
                kind,
                text: self.i18n.t(key).to_string(),
                sticky: false,
            });
        }
    }

    /// 按当前配置重新注册全局快捷键（设置页改了 开关/预设/自定义 之后调用）。
    ///
    /// 顺序就是需求要求的那一条：`UnregisterHotKey` 旧的 → 注册新的 →
    /// **新的失败就回滚到旧的**（旧组合仍然可用）→ 候选探测给出建议 →
    /// 可见 warn（i18n）+ `cap.log`（含 `GetLastError`，1409 = 已被占用）。
    fn apply_hotkey(&mut self) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        let want = if self.cfg.global_hotkey_enabled {
            Some(caplite::tray::effective_hotkey(
                self.cfg.hotkey_preset,
                &self.cfg.hotkey_custom,
            ))
        } else {
            None
        };
        let fallback = self.hotkey_applied;
        match tray.set_hotkey(want, fallback) {
            Ok(()) => {
                self.hotkey_applied = want;
                // 成功了就把那条告警撤下来（它不会自愈，所以是 sticky，只能由代码撤）
                self.notices.retain(|n| n.id != "hotkey_register_failed");
                let msg = match want {
                    Some(hk) => format!("[cap] 全局快捷键已注册：{}", hk.label()),
                    None => "[cap] 全局快捷键已关闭（设置里关掉了开关）".to_string(),
                };
                startup_marks::note(self.startup_log, &msg);
            }
            Err(e) => {
                // `tray` 内部已经回滚；这里把外壳的状态对齐到"实际注册着的那个"
                self.hotkey_applied = tray.current_hotkey();
                let text = hotkey_error_text(&self.i18n, &e);
                startup_marks::note(self.startup_log, &format!("[cap] {}", hotkey_log_line(&e)));
                self.push_notice(Notice {
                    id: "hotkey_register_failed",
                    kind: caplite::ui::theme::Banner::Warn,
                    text,
                    sticky: true,
                });
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 真正的界面全在 [`App::ui_root`] 里。把它抽出来只有一个目的：
        // **无窗口单测能驱动同一份布局** —— 最小窗口尺寸（`ui::layout`）是实测出来的，
        // 而"实测"必须量真布局，不能另写一份复制品（复制品会随代码漂移而失真）。
        self.ui_root(ui);
    }

    /// **窗口隐藏时 eframe 只调这里**（不调 [`Self::ui`]）。
    ///
    /// 这不是可选优化：`ViewportCommand::Visible(false)` 之后 eframe 走
    /// `update_logic_only(app)`（`egui::Context::run_logic`），`App::ui` 整个不会被调用。
    /// 托盘菜单与 `WM_HOTKEY` 的命令、以及"录制结束该恢复可见"的判定，
    /// 因此都必须在这条路径上也走一遍，否则"隐藏到托盘之后就没法停/没法回来"。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 闲置的 OpenH264 预热实例要主动回收（隐藏到托盘期间也要走这条路径）。
        caplite::video::encode::sweep_prewarm();
        self.pump_tray(ctx);
        self.pump_cfg_saves();
        // 窗口隐藏时 `App::ui` 不跑；后台 start/stop 的结果必须在这里也能收。
        self.main.poll_recording_jobs(
            &self.cfg,
            &mut self.recorder,
            &mut self.last_result,
            &self.i18n,
        );
        self.sync_tray_recording();
        // 窗口隐藏时 `MainPage::ui` 不跑：停止后的 session_issue / 错误必须在这里
        // 也能进入底部提示通道，否则用户只看到“录完了”却不知道文件不完整。
        if let Some(e) = self.main.reportable_error() {
            self.push_notice(Notice {
                id: "record_error",
                kind: caplite::ui::theme::Banner::Error,
                text: e,
                sticky: false,
            });
        }
        if !self.main.is_busy() && self.main.take_pending_quit() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.sync_tray_visibility(ctx);
    }

    /// 退出前注销托盘图标（`NIM_DELETE`）与全局热键（`UnregisterHotKey`），
    /// 避免"幽灵图标"与热键残留。`Tray` 的 `Drop` 做同一件事（幂等），
    /// 这里显式一次是因为 `on_exit` 是 eframe 保证会走到的那一个点。
    fn on_exit(&mut self) {
        // 关键：`Recorder` 可能已经被移到 `cap-stop` 后台线程；这里等它完成导出，
        // 绝不能让进程先退出、把半成品/未导出的分片留在 Cache 里。
        let finished = self.main.wait_for_recording_jobs(
            &self.cfg,
            &mut self.recorder,
            &mut self.last_result,
            &self.i18n,
        );
        if !finished {
            tracing::error!("退出：录制收尾未在超时内完成，已保留 Cache 分片并继续退出");
        }
        self.tray.take();
        // P45：退出前把"被节流推迟的最后一次改动"补写下去 ——
        // 否则"拖完滑条 100ms 内直接关窗口"会丢掉最后一次改动（虽然窗口很小，但会真的发生）。
        self.flush_cfg_save();
        // 后台配置保存最多等 5 s；超时只记日志，不阻塞关闭。
        if let Some(saver) = self.cfg_saver.take() {
            if !saver.shutdown(&self.cfg, std::time::Duration::from_secs(5)) {
                tracing::warn!("退出：后台配置保存超时，最后一次改动可能未落盘");
            }
        }
    }
}

impl App {
    /// 窗口内的全部内容：顶部标签栏 + 中央页面 + 底部状态栏（+ 关闭提醒窗口）。
    ///
    /// 布局顺序即优先级：`Panel::top`/`Panel::bottom` **先占掉**自己那块高度，
    /// 中央内容区拿到的 `available_height()` 天然是"扣掉两条栏之后"的剩余空间，
    /// 所以三页的内容永远不会被两条栏压住（不是浮层覆盖，而是版面分割）。
    fn ui_root(&mut self, ui: &mut egui::Ui) {
        // 预热 TTL 主动回收：一次锁 + 一次 Instant 比较，成本可忽略。
        caplite::video::encode::sweep_prewarm();
        // 仅测试用：窗口的完整高度（根 `Ui` 的 `max_rect` 就是整个视口，不随面板收缩）
        #[cfg(test)]
        let window_h = ui.max_rect().height();
        // ---- 启动阶段打点：第 1 帧的开始/结束（首帧之后的打点都以此为参照）----
        self.frame_index = self.frame_index.saturating_add(1);
        let frame_t0 = Instant::now();
        if self.frame_index == 1 {
            startup_marks::mark_opt(self.startup_log, "ui_first_frame_begin", "frame");
        }

        // ---- 托盘菜单 / 全局快捷键：先把上一帧之后到达的命令处理掉 ----
        // 放在最前面：这样"点了托盘菜单的停止"在同一帧就会走完停止路径，
        // 下面的绘制拿到的就是停止后的状态。
        self.pump_tray(ui.ctx());
        self.pump_cfg_saves();

        // 录制中要持续刷新（时长、统计、状态）；后台 start/stop 期间也要排帧收结果。
        if self.recorder.is_some() || self.main.is_busy() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }

        // ---- 关闭软件时的 Cache 提醒（用户最终版要求）----
        //
        // 拦截关闭请求 → 如果 Cache 里还有文件就弹一次提示，用户点「知道了」后**正常关闭**。
        // 为什么用 egui 的 viewport 命令而不是 `rfd` 同步弹窗：`rfd` 会阻塞渲染线程
        //（Windows 上还会卡住消息循环），而 `CancelClose` + 一个 `egui::Window` 是纯 UI 流程。
        //
        // "是否有文件"用 `cache::cache_has_files`（只看第一个条目，**不遍历求和**）；
        // 空目录 / 已提醒过 → 直接放行关闭（`should_warn_cache` 是纯函数，有单测）。
        let close_requested = ui.ctx().input(|i| i.viewport().close_requested());
        if close_requested && !self.close_confirmed {
            // 正在开始/停止/录制：先取消这次关闭，等在途工作完成后再自动关闭。
            // 不能在这里直接放行：`Recorder` 可能已经被移到后台线程，进程退出会丢掉导出。
            if self.main.is_busy() || self.recorder.is_some() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.main.request_quit_after_finish();
                if self.recorder.is_some() {
                    let _ = self.main.stop_recording(
                        &self.cfg,
                        &mut self.recorder,
                        &mut self.last_result,
                        &self.i18n,
                    );
                }
                return;
            }
            // 只对**录制分片**提醒；`Cache\derived` 里的波形缓存可以自动修剪，不打扰用户。
            let nonempty = caplite::cache::cache_has_recordings(&self.cache_dir);
            if caplite::cache::should_warn_cache(nonempty, self.cache_warned) {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.cache_warned = true;
                self.show_cache_close_dialog = true;
                // 倒计时在**弹窗出现的那一刻**起算（P13），固定 6 秒
                self.close_dialog_deadline =
                    Some(std::time::Instant::now() + CLOSE_DIALOG_AUTO_SECS);
            }
        }
        if self.show_cache_close_dialog {
            let dir = self.cache_dir.display().to_string();
            // P51：这里**只传一个参数**。模板 `close_cache_hint` 只有 1 个 `{}`，
            // 原先还多传了 `&dir` —— `I18n::tf` 对多出来的参数是**静默丢弃**的
            //（`rest` 里找不到 `{}` 就 `break`），于是那个参数纯属摆设，
            // 还会让读代码的人误以为路径由模板负责显示。
            // 路径真正的显示位置在下面 `mono_muted(&dir)` 那一行。
            let text = self
                .i18n
                .tf("close_cache_hint", &[&caplite::ui::theme::APP_NAME]);
            let ok = self.i18n.t("btn_got_it").to_string();
            let mut confirmed = false;

            // ---- P13：固定 6 秒自动关闭（**不做悬停重置**）----
            //
            // 用户要求：倒计时只在弹窗出现期间走；到点等同于点「知道了」，
            // 即继续原来的关闭流程（当前是 CancelClose + 弹窗，确认后才真正退出）。
            // 倒计时一关就把 `close_dialog_deadline` 设回 None —— 不留存活着的定时器。
            let remaining = self
                .close_dialog_deadline
                .map(|d| d.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_default();
            if remaining.is_zero() {
                confirmed = true;
            } else {
                // 没有输入时界面不会自己重绘 —— 倒计时要真的走，就得自己安排节拍
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
            // 显示"还剩几秒"：向上取整，于是从 6 数到 1，绝不显示 0
            let secs = {
                let ms = remaining.as_millis() as i64;
                ((ms + 999) / 1000).clamp(1, 6) as u32
            };
            // `&secs` 直接当 `&dyn Display` 用，不必先 `to_string()` —— `tf` 内部本来就会
            // 对每个参数调一次 `to_string()`，先转一次等于多分配一个马上被丢掉的 String。
            let countdown = self.i18n.tf("close_auto_secs", &[&secs]);

            // ---- P12：先铺一层**半透明遮罩**把主界面压暗（弹窗必须一眼看出是浮层）----
            //
            // 遮罩自己 `interactable(true)` 且不画任何控件：它会把落在上面的点击**吃掉**，
            // 于是弹窗期间后面的按钮点不动（这也正是模态该有的行为）。
            // 整个窗口矩形（这个 egui 版本的 `Context` 只有 `content_rect`）：
            // 遮罩必须盖住包括顶栏/底部状态栏在内的整个界面，否则"压暗"会漏边。
            let mut screen = ui.ctx().content_rect();
            if !screen.is_positive() {
                // 极端布局（首帧/被压成 0）时给一个不可能盖住任何东西的矩形，
                // 而不是让后续绘制拿到 NaN 坐标
                screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(0.0, 0.0));
            }
            egui::Area::new(egui::Id::new("close_warn_scrim"))
                .order(egui::Order::Middle)
                .fixed_pos(screen.min)
                .interactable(true)
                .show(ui.ctx(), |ui| {
                    ui.allocate_response(screen.size(), egui::Sense::click_and_drag());
                    ui.painter()
                        .rect_filled(screen, 0.0, caplite::ui::theme::scrim());
                });

            let frame = egui::Frame::new()
                // 浅灰底 + 发丝边框 + 阴影（P12：与主界面明显不同色）
                .fill(caplite::ui::theme::bg())
                .stroke(caplite::ui::theme::hairline())
                .shadow(egui::epaint::Shadow {
                    offset: [0, 6],
                    blur: 18,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(70),
                })
                .corner_radius(caplite::ui::theme::radius())
                .inner_margin(egui::Margin::symmetric(16, 14));
            egui::Window::new(caplite::ui::theme::WINDOW_TITLE)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .frame(frame)
                .show(ui.ctx(), |ui| {
                    ui.set_max_width(420.0);
                    ui.label(egui::RichText::new(text).size(caplite::ui::theme::FS_BODY));
                    ui.add_space(6.0);
                    ui.label(caplite::ui::theme::mono_muted(&dir));
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| {
                        if caplite::ui::theme::primary_button(ui, &ok, 120.0).clicked() {
                            confirmed = true;
                        }
                        ui.add_space(6.0);
                        // 二选一里选的是"按钮旁一行小字"（不是按钮上的进度线）：
                        // 小字更直白地说明"会自己关"，而进度线容易被误读成"正在保存"。
                        ui.label(
                            egui::RichText::new(countdown)
                                .size(caplite::ui::theme::FS_SMALL)
                                .color(caplite::ui::theme::muted()),
                        );
                    });
                });
            if confirmed {
                // 记住"这次真的确认过了"，然后**再次发起关闭**，这次直接放行
                self.show_cache_close_dialog = false;
                // 弹窗一关就把定时器清掉：绝不留一个还活着的倒计时
                self.close_dialog_deadline = None;
                self.close_confirmed = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        } else {
            // 弹窗不在显示状态：定时器必须是空的（双保险，避免将来多一条关闭路径忘了清）
            self.close_dialog_deadline = None;
        }

        egui::Panel::top("cap_tabs").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                // 内容区**不再重复应用名**：系统窗口标题栏左上角已经写着 `CapLite v1.0.0`
                //（`ViewportBuilder::with_title(theme::WINDOW_TITLE)`，P44 加了版本号），
                // 这里再画一遍就是重复的 logo。
                // 三个标签页共用这个外壳，所以三个页面都不再有内容区标题。
                // 标签页做成"胶囊"：选中项用强调色浅底 + 强调色文字（不再用默认的框线样式）
                // 标签顺序（用户要求）：**录制 | 剪辑 | 设置**（设置放最后）
                for (tab, key) in [
                    (Tab::Home, "tab_home"),
                    (Tab::Editor, "tab_editor"),
                    (Tab::Settings, "tab_settings"),
                ] {
                    let selected = self.tab == tab;
                    let text = self.i18n.t(key);
                    let (fill, color) = if selected {
                        (caplite::ui::theme::accent_soft(), caplite::ui::theme::accent())
                    } else {
                        (egui::Color32::TRANSPARENT, caplite::ui::theme::muted())
                    };
                    let btn = egui::Button::new(
                        egui::RichText::new(text)
                            .size(14.0)
                            .color(color)
                            .strong(),
                    )
                    .fill(fill)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(8));
                    if ui.add_sized([76.0, 30.0], btn).clicked() {
                        self.tab = tab;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.recorder.is_some() {
                        ui.label(
                            egui::RichText::new(self.i18n.t("state_recording"))
                                .color(caplite::ui::theme::err_fg())
                                .strong(),
                        );
                        caplite::ui::theme::status_dot(ui, caplite::ui::theme::err_fg());
                    }
                });
            });
            ui.add_space(6.0);
            let sep = ui.available_rect_before_wrap();
            ui.painter().hline(
                sep.x_range(),
                sep.top(),
                egui::Stroke::new(1.0, caplite::ui::theme::line()),
            );
            ui.add_space(2.0);
        });

        let mut cfg_changed = false;
        let mut editor_handoff: Option<(std::path::PathBuf, bool)> = None;
        let mut page_error: Option<String> = None;
        let mut page_warning: Option<String> = None;
        // 剪辑页报上来的"波形不可用"（没有音轨 / 没编入该编码的解码器）：
        // 与其它页共用一个通道，最终统一汇到底部状态栏的 Warn 档（不弹窗、不阻塞）。
        let mut page_wave_error: Option<String> = None;
        // 本帧设置页的托盘/热键信号（见 `SettingsOutcome`）：
        // * `capturing` —— 正在捕获自定义组合，外壳**必须临时注销**全局热键；
        // * `hotkey_changed` —— 快捷键相关配置改过，要重新注册。
        // 不在设置页时这两个都是 false：切走标签页就等于停止捕获。
        let mut page_hotkey_capturing = false;
        let mut page_hotkey_changed = false;
        // "刚刚切到剪辑页"：只有在**切换那一刻**才把输出目录交给剪辑页。
        // P46 之后 `set_output_dir` 只是记一个字符串，但每帧调一次仍是纯冗余。
        let entering_editor = self.tab == Tab::Editor && self.prev_tab != Tab::Editor;
        // 先把后台 start/stop 的结果落地，再决定剪辑页是否 active：
        // 否则“热键开始录制”那一帧会先按 idle 启动波形线程，下一帧才停。
        self.main.poll_recording_jobs(
            &self.cfg,
            &mut self.recorder,
            &mut self.last_result,
            &self.i18n,
        );
        self.sync_tray_recording();
        // 剪辑页的重后台任务（波形/深度检查/播放）只在“页面可见且没在录制”时跑：
        // 在首页开始录制后继续 4 线程整片解码，会直接和视频编码抢核。
        self.editor.set_active(
            self.tab == Tab::Editor && self.recorder.is_none() && !self.main.is_busy(),
        );
        // **离开剪辑页 = 停止播放**：切走之后 `editor.ui()` 不再被调用，
        // 播放时钟停了，但解码线程还在按实时速度往下解 —— 用户看不见却一直吃 CPU。
        if self.prev_tab == Tab::Editor && self.tab != Tab::Editor {
            self.editor.stop_playback();
        }

        // ---- 底部状态栏（Windows 信息栏那种形态）----
        //
        // 为什么放底部：用户反馈"顶部一整块横幅太打扰"。`Panel::bottom` 会**先占掉**
        // 这块高度，中央内容区自动收窄，所以永远不会和内容重叠（不是悬浮覆盖层）。
        //
        // 栏内分工：
        // * 左：状态点 + 状态文字（录制中还会显示已录时长）+ 当前输出目录（只读、中段省略）；
        //   **没有消息时右半边什么都不画**（不留空白条占视线）；
        // * 右：**消息区**，一次显示**一条**（按严重度 Error > Warn > Info，同级取最新）：
        //   - error：红底 + 加粗 + 不自动消失（手动「知道了」才会消失）；
        //   - warn：琥珀底色，可「知道了」。
        //   **目录自动回退不再进这条通道**（用户明确要求：只进日志），
        //   所以正常启动时右半边是空的；出现 warn/error 时才占位。
        //   为什么一次只显示一条：栏高必须固定（消息出现时不能把内容顶得跳动），
        //   单行省略也不会换行撑高；还有排队的消息就在右边显示"+N"。
        // 取舍：致命错误也走这条通道（红底 + 状态点变红 + 悬停可看全文），
        // 不再额外占用顶部一整块横幅 —— 三页共用一条通道，行为一致。
        egui::Panel::bottom("cap_status_bar")
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    // 左：状态
                    let (state_text, state_color) = status_line(&self.i18n, &self.recorder, &self.last_result);
                    caplite::ui::theme::status_dot(ui, state_color);
                    ui.label(
                        egui::RichText::new(state_text)
                            .size(caplite::ui::theme::FS_SMALL)
                            .strong()
                            .color(state_color),
                    );
                    if let Some(rec) = &self.recorder {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(fmt_clock(rec.elapsed()))
                                .monospace()
                                .color(caplite::ui::theme::text()),
                        );
                    }
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(self.i18n.t("output_dir_label"))
                            .size(caplite::ui::theme::FS_SMALL)
                            .color(caplite::ui::theme::muted()),
                    );
                    ui.label(caplite::ui::theme::mono_muted(&elide_middle(
                        &self.cfg.output_dir,
                        46,
                    )))
                    .on_hover_text(&self.cfg.output_dir);

                    // 右：消息区（单行、省略、固定栏高）
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let queued = self.notices.len();
                        if let Some(idx) = pick_notice(&self.notices) {
                            let n = &self.notices[idx];
                            let text = n.text.clone();
                            let kind = n.kind;
                            let mut dismissed = false;
                            // 红底错误最醒目；info/warn 用各自的浅底
                            let (bg, fg) = match kind {
                                caplite::ui::theme::Banner::Error => {
                                    (caplite::ui::theme::err_bg(), caplite::ui::theme::err_fg())
                                }
                                caplite::ui::theme::Banner::Warn => {
                                    (caplite::ui::theme::warn_bg(), caplite::ui::theme::warn_fg())
                                }
                                caplite::ui::theme::Banner::Info => {
                                    (caplite::ui::theme::info_bg(), caplite::ui::theme::info_fg())
                                }
                            };
                            egui::Frame::NONE
                                .fill(bg)
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    match kind {
                                        caplite::ui::theme::Banner::Error => caplite::ui::theme::err_line(),
                                        caplite::ui::theme::Banner::Warn => caplite::ui::theme::warn_line(),
                                        caplite::ui::theme::Banner::Info => caplite::ui::theme::info_line(),
                                    },
                                ))
                                .corner_radius(egui::CornerRadius::same(4))
                                .inner_margin(egui::Margin::symmetric(8, 2))
                                .show(ui, |ui| {
                                    if ui
                                        .small_button(self.i18n.t("btn_got_it"))
                                        .clicked()
                                    {
                                        dismissed = true;
                                    }
                                    let width = (ui.available_width() - 4.0).max(80.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(width, 0.0),
                                        egui::Layout::top_down(egui::Align::LEFT),
                                        |ui| {
                                            let rich = if kind == caplite::ui::theme::Banner::Error {
                                                egui::RichText::new(&text)
                                                    .size(caplite::ui::theme::FS_SMALL)
                                                    .color(fg)
                                                    .strong()
                                            } else {
                                                egui::RichText::new(&text)
                                                    .size(caplite::ui::theme::FS_SMALL)
                                                    .color(fg)
                                            };
                                            ui.add(egui::Label::new(rich).truncate())
                                                .on_hover_text(&text);
                                        },
                                    );
                                });
                            if queued > 1 {
                                ui.label(
                                    egui::RichText::new(format!("+{}", queued - 1))
                                        .size(caplite::ui::theme::FS_SMALL)
                                        .color(caplite::ui::theme::muted()),
                                );
                            }
                            if dismissed {
                                let id = n.id;
                                self.dismissed.push(id);
                                self.notices.remove(idx);
                            }
                        }
                    });
                });
                ui.add_space(5.0);
            });

        // 仅测试用：量出**外壳**占掉的固定高度 = 窗口高 − 页面可用高。
        // 它把"标签栏 + 状态栏 + 中央面板的内边距"一并算进去 —— 这正是最小窗口高度
        // 推导里需要的那一项（见 `caplite::ui::layout`）。
        #[cfg(test)]
        {
            self.test_chrome_h = None;
            self.test_page_avail_h = None;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            #[cfg(test)]
            {
                let page_h = ui.max_rect().height();
                self.test_page_avail_h = Some(page_h);
                self.test_chrome_h = Some(window_h - page_h);
            }
            match self.tab {
                Tab::Home => {
                    // 不在设置页 = 捕获结束（切走标签页就该把热键装回去）
                    self.settings.stop_capture();
                    let outcome = self.main.ui(
                        ui,
                        MainCtx {
                            cfg: &mut self.cfg,
                            i18n: &self.i18n,
                            recorder: &mut self.recorder,
                            result: &mut self.last_result,
                        },
                    );
                    cfg_changed = outcome.cfg_changed;
                    editor_handoff = outcome
                        .open_in_editor
                        .map(|p| (p, outcome.open_in_editor_fallback));
                    page_error = outcome.error;
                    page_warning = outcome.warning;
                }
                Tab::Settings => {
                    // 录制中（Recording/Stopping）设置项全部置灰：本次录制的参数在开始时
                    // 就已经快照，改了也不会生效 —— 与其让人改了个没用的值，不如直接禁用。
                    let recording = self
                        .recorder
                        .as_ref()
                        .map(|r| {
                            matches!(
                                r.state(),
                                caplite::recorder::RecorderState::Recording
                                    | caplite::recorder::RecorderState::Stopping
                            )
                        })
                        .unwrap_or(false);
                    let outcome = self.settings.ui(
                        ui,
                        &mut self.cfg,
                        &mut self.i18n,
                        recording,
                    );
                    cfg_changed = outcome.cfg_changed;
                    // 保存失败的可见错误 → 底部栏红条（页面里不再画）
                    if let Some(e) = outcome.error {
                        page_error = Some(e);
                    }
                    // 托盘/热键：捕获中要临时注销，改过组合要重新注册
                    page_hotkey_capturing = outcome.hotkey_capturing;
                    page_hotkey_changed = outcome.hotkey_changed;
                }
                Tab::Editor => {
                    // 不在设置页 = 捕获结束（切走标签页就该把热键装回去）
                    self.settings.stop_capture();
                    // 切到剪辑页 / 输出目录变了 → 把最新录制目录交给剪辑页
                    //（「打开视频」的文件对话框起始位置看它）
                    if entering_editor
                        || self.editor_dir_synced.as_deref() != Some(self.cfg.output_dir.as_str())
                    {
                        self.editor.set_output_dir(Path::new(&self.cfg.output_dir));
                        self.editor_dir_synced = Some(self.cfg.output_dir.clone());
                    }
                    // 显示项与配置保持一致（设置页改过、或本页那个复选框改过都要同步）。
                    // **不要每帧都无条件同步**：`set_show_waveform` 在值真的变化时才动作
                    //（关掉要取消后台任务、打开要重起一个），所以这里的"每帧读一次配置"
                    // 是零成本的（值相同就直接 return）。
                    self.editor.set_show_waveform(self.cfg.show_waveform);
                    self.editor.ui(ui, &self.i18n);
                    // 本页那个「波形」复选框被动过 → 走与设置页**同一条**落盘路径
                    if self.editor.wave_cfg_changed() {
                        self.cfg.show_waveform = self.editor.show_waveform;
                        cfg_changed = true;
                    }
                    // "没有音轨 / 解码失败"如实提示（只走底部状态栏，不弹窗、不阻塞）
                    if let Some(w) = self.editor.waveform_notice() {
                        if page_wave_error.as_deref() != Some(w.as_str()) {
                            page_wave_error = Some(w);
                        }
                    } else {
                        page_wave_error = None;
                    }
                }
            }
        });

        if cfg_changed {
            self.commit_cfg();
        }
        // P45：被节流推迟的改动 —— 到点就补写、松手就补写；还没到点就安排一帧
        // （**不能只判断"松手"**：用户拖完停住不动时不会再有输入事件，
        //   不安排重绘的话这次改动会一直挂在内存里，直到下一次任何操作）。
        if self.cfg_save_pending {
            let waited = self
                .cfg_saved_at
                .map(|t| t.elapsed())
                .unwrap_or(CFG_SAVE_MIN_GAP);
            if waited >= CFG_SAVE_MIN_GAP || ui.ctx().input(|i| i.pointer.any_released()) {
                self.flush_cfg_save();
            } else {
                ui.ctx().request_repaint_after(CFG_SAVE_MIN_GAP - waited);
            }
        }
        if std::mem::take(&mut self.output_dir_dirty) {
            // 首帧：用户已经看到"原输出目录不可写，已改用 X"的提示，这时才把新目录落盘。
            // 放在首帧而不是构造时，是为了保证"提示"和"落盘"对用户是同一件事。
            self.commit_cfg();
        }
        // 录制页报上来的错误/警告 → 同一套底部状态栏通道（红/琥珀），不再各画一套横幅
        if let Some(e) = page_error {
            self.push_notice(Notice {
                id: "record_error",
                kind: caplite::ui::theme::Banner::Error,
                text: e,
                sticky: false,
            });
        }
        if let Some(w) = page_warning {
            self.push_notice(Notice {
                id: "record_warning",
                kind: caplite::ui::theme::Banner::Warn,
                text: w,
                sticky: false,
            });
        }
        // 波形不可用（没有音轨 / 该编码的解码器没编进来）：**如实告知但不吓人** ——
        // Warn 档（能继续，剪辑照常），文案由 i18n 给（中英各一份）。
        // 用 `Notice::id` 去重：同一条原因只显示一次，不会每帧刷屏。
        if let Some(w) = page_wave_error {
            let text = self
                .i18n
                .tf("waveform_unavailable", &[&w]);
            self.push_notice(Notice {
                id: "editor_waveform",
                kind: caplite::ui::theme::Banner::Warn,
                text,
                sticky: false,
            });
        }
        if let Some((path, fallback)) = editor_handoff {
            self.open_in_editor(&path, fallback);
        }
        self.prev_tab = self.tab;

        // 非录制页时 `MainPage::ui` 不跑，后台 start/stop 的结果仍要在这里收；
        // 本函数在录制页是幂等的（没有在途结果时两次 try_recv 都是 Empty）。
        self.main.poll_recording_jobs(
            &self.cfg,
            &mut self.recorder,
            &mut self.last_result,
            &self.i18n,
        );
        self.sync_tray_recording();
        // 非录制页时 `MainPage::ui` 不会把 `self.error` 交给外壳；这里补一条。
        if let Some(e) = self.main.reportable_error() {
            self.push_notice(Notice {
                id: "record_error",
                kind: caplite::ui::theme::Banner::Error,
                text: e,
                sticky: false,
            });
        }
        if !self.main.is_busy() && self.main.take_pending_quit() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // ---- 托盘/热键的两个收口（必须在**按钮那条路径走完之后**做）----
        //
        // ① 捕获态变化：进入捕获先 `UnregisterHotKey`（**必须**，否则用户按下的正是
        //    当前组合，Windows 会直接触发开始/停止录制、按键根本进不了输入框）；
        //    退出捕获按当前配置重新注册（组合可能刚被改过）。
        if page_hotkey_capturing != self.hotkey_capture_prev {
            self.hotkey_capture_prev = page_hotkey_capturing;
            self.hotkey_suspended = page_hotkey_capturing;
            if page_hotkey_capturing {
                if let Some(tray) = self.tray.as_mut() {
                    // 只是临时注销：`hotkey_applied` 仍记着原来那个，退出时用它回滚兜底
                    let _ = tray.set_hotkey(None, None);
                }
            } else {
                self.apply_hotkey();
            }
        } else if page_hotkey_changed && !self.hotkey_suspended {
            // ② 组合/开关改过 → `Unregister` 旧的 → 注册新的（失败会回滚 + 出可见提示）
            self.apply_hotkey();
        }
        // ③ 录制状态迁移 → 隐藏/恢复（按钮、托盘菜单、全局快捷键三条入口共用同一判据）
        self.sync_tray_visibility(ui.ctx());

        // ---- 首帧结束：这一刻界面已经画完（绘制命令已提交），可以安全地做"延后初始化"----
        // 打点顺序很重要：先记 `ui_first_frame_done`（用户能感知的"首帧渲染完"），
        // 再做字体追加 —— 它自己也会记一条阶段打点。
        if self.frame_index == 1 {
            startup_marks::mark_dur_log(
                self.startup_log,
                "ui_first_frame_done",
                "frame",
                frame_t0.elapsed(),
            );
        }
        // 先克隆一份 `Context`（Arc 句柄，克隆很便宜）：`flush_deferred_init` 里还要用
        // `ui` 之外的可变借用，直接传 `ui.ctx()` 会和 `&mut self` 打架。
        let ctx = ui.ctx().clone();
        self.flush_deferred_init(&ctx);
    }
}

/// exe 同级 `locales/` 目录（用于覆盖内嵌文案）。
///
/// 为什么不用相对路径 `"locales"`：双击 exe 时 CWD 可能是 `C:\Windows\System32`，
/// 相对路径会去找 `System32\locales`，外部语言包永远读不到。
fn exe_side_locales() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("locales"))
}

/// 底部状态栏左侧那一段"当前状态"（状态文字 + 状态点的颜色）。
fn status_line(
    i18n: &I18n,
    recorder: &Option<Recorder>,
    result: &Option<RecordingResult>,
) -> (String, egui::Color32) {
    use caplite::ui::theme as th;
    // P40：状态色全部走主题取色函数（深色模式下要提亮一档才读得清）。
    // 与 `ui::main_page::state_text` 同一套口径：红=录制中、琥珀=收尾中、
    // 绿=已完成、深色下 idle 用次要文字色而不是实心灰。
    match recorder {
        Some(rec) => match rec.state() {
            caplite::recorder::RecorderState::Recording => {
                (i18n.t("state_recording").to_string(), th::err_fg())
            }
            caplite::recorder::RecorderState::Stopping => {
                (i18n.t("state_stopping").to_string(), th::warn_fg())
            }
            caplite::recorder::RecorderState::Finalizing => {
                (i18n.t("state_finalizing").to_string(), th::warn_fg())
            }
            caplite::recorder::RecorderState::Finished => {
                (i18n.t("rec_finished").to_string(), th::ok_fg())
            }
            caplite::recorder::RecorderState::Failed(msg) => (
                format!("{}：{msg}", i18n.t("state_failed")),
                th::err_fg(),
            ),
        },
        None => {
            if result.is_some() {
                (i18n.t("rec_finished").to_string(), th::ok_fg())
            } else {
                (i18n.t("state_idle").to_string(), th::muted())
            }
        }
    }
}

/// 消息区一次只显示一条：按严重度选（Error > Warn > Info），同级取**最新**的那条。
/// 这样栏高恒定、不会因为同时来两条消息而把内容顶得跳动。
fn pick_notice(notices: &[Notice]) -> Option<usize> {
    fn rank(kind: caplite::ui::theme::Banner) -> u8 {
        match kind {
            caplite::ui::theme::Banner::Error => 2,
            caplite::ui::theme::Banner::Warn => 1,
            caplite::ui::theme::Banner::Info => 0,
        }
    }
    notices
        .iter()
        .enumerate()
        .max_by_key(|(i, n)| (rank(n.kind), *i))
        .map(|(i, _)| i)
}

// ---------------------------------------------------------------------------
// 全局快捷键失败：界面文案 / 日志行（两条路径共用，见 `App::apply_hotkey` 与 `App::new`）
// ---------------------------------------------------------------------------

/// 注册失败的**界面文案**（走 i18n；被占用时优先用"已被占用 + 建议组合"那一条）。
///
/// 单独抽成函数是为了可测：`cargo test` 里能直接断言"1409 与建议组合确实进了用户看得见的字"。
fn hotkey_error_text(i18n: &I18n, e: &caplite::tray::HotkeyRegisterError) -> String {
    let label = e.hotkey.label();
    if e.already_registered() {
        return match &e.suggestion {
            Some(s) => i18n.tf("hotkey_taken_with_suggestion", &[&label, &s.label()]),
            None => i18n.tf("hotkey_taken", &[&label]),
        };
    }
    i18n.tf(
        "hotkey_register_failed",
        &[&label, &e.code, &e.system],
    )
}

/// 注册失败的**日志行**（语言中立，进 `cap.log`；含 `GetLastError` 与候选建议）。
fn hotkey_log_line(e: &caplite::tray::HotkeyRegisterError) -> String {
    format!("全局快捷键 {}", e.describe())
}

/// 注册失败的界面提示条（warn 级）。
///
/// `sticky = true`：它是**真的降级**（热键没了），不能因为"下一次保存配置成功"
/// 就被 `commit_cfg` 顺手撤掉；只有真的注册成功（[`App::apply_hotkey`]）才由代码撤下。
fn hotkey_warning_notice(i18n: &I18n, e: &caplite::tray::HotkeyRegisterError) -> Notice {
    Notice {
        id: "hotkey_register_failed",
        kind: caplite::ui::theme::Banner::Warn,
        text: hotkey_error_text(i18n, e),
        sticky: true,
    }
}

/// 录制时长的 `MM:SS`（状态栏用），超过 1 小时显示 `HH:MM:SS`。
fn fmt_clock(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// 中段省略：`C:\very\long\path\name` → `C:\very\…\name`（状态栏那一行用）。
fn elide_middle(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars || max_chars < 8 {
        return s.to_string();
    }
    let keep_head = (max_chars - 5) / 2;
    let keep_tail = max_chars - 5 - keep_head;
    let head: String = chars[..keep_head].iter().collect();
    let tail: String = chars[chars.len() - keep_tail..].iter().collect();
    format!("{head}…{tail}")
}

/// 系统完整中文字体候选（**兜底**用；只放顺序与字面号，不装 bytes）。
///
/// 顺序与改动前完全一致：第一个能读到的就用它。
const SYSTEM_CJK_CANDIDATES: &[(&str, u32)] = &[
    ("C:\\Windows\\Fonts\\msyh.ttc", 0),   // 微软雅黑（ttc 的第 0 号字面）
    ("C:\\Windows\\Fonts\\msyh.ttf", 0),
    ("C:\\Windows\\Fonts\\Deng.ttf", 0),   // 等线
    ("C:\\Windows\\Fonts\\simhei.ttf", 0), // 黑体
    ("C:\\Windows\\Fonts\\simsun.ttc", 0), // 宋体
];

/// 内嵌的**中文子集字体**（构建期由 `scripts/make-font-subset.ps1` 生成）。
///
/// 它只含"界面真正会用到的字形"（语言包全部字符 ∪ 源码里的中文字面量 ∪ ASCII+常用符号），
/// 体积几百 KB，所以**首帧前**装它几乎是免费的；完整系统字体（15~25 MB）推迟到
/// 首帧之后追加（见 [`install_full_cjk_font`]）。
///
/// 数据本体放在 `caplite::ui::startup` 里（同一个 crate，`include_bytes!` 去重、不额外占体积），
/// 这样 `cargo test --lib` 就能测"子集是否覆盖全部语言包字形"。
const SUBSET_FONT_BYTES: &[u8] = caplite::ui::startup::SUBSET_FONT_BYTES;
/// 子集字体是否可用（且真的被当作主字体装上）。
const SUBSET_FONT_ACTIVE: bool = !SUBSET_FONT_BYTES.is_empty();

/// 首帧之前装字体：优先装**子集**，装不上就退回完整系统字体（见 [`SUBSET_FONT_ACTIVE`]）。
fn install_cjk_font(ctx: &egui::Context, log: &StartupLog) {
    if SUBSET_FONT_ACTIVE {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            SUBSET_FONT_NAME.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(SUBSET_FONT_BYTES)),
        );
        // 正文以子集为首选（汉字+拉丁都好看）；等宽族放到末尾，
        // 这样统计数字仍是等宽，缺字时再回落到中文字体
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, SUBSET_FONT_NAME.to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(SUBSET_FONT_NAME.to_owned());
        ctx.set_fonts(fonts);
        if let Some(p) = read_system_cjk() {
            log.line(&format!(
                "[cap] 已装入中文子集字体（{} KiB，内嵌）；完整系统字体 {} 推迟到首帧之后追加",
                SUBSET_FONT_BYTES.len() / 1024,
                p
            ));
        } else {
            log.line(&format!(
                "[cap] 已装入中文子集字体（{} KiB，内嵌）；本机未找到完整系统字体，\
                 子集之外的字会缺字，且没有兜底字体可追加",
                SUBSET_FONT_BYTES.len() / 1024
            ));
        }
        return;
    }
    // 退化路径：没有子集字体 → 与改动前一样，启动时直接装完整系统字体
    if let Some((name, data)) = read_system_cjk_data() {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(name.to_owned(), std::sync::Arc::new(data));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, name.to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(name.to_owned());
        ctx.set_fonts(fonts);
        return;
    }
    log.line("[cap] 未找到可用的中文字体，界面中文可能显示为方框");
}

/// 子集字体在 `FontDefinitions` 里的名字（**单一来源**，装/追加都用它）。
const SUBSET_FONT_NAME: &str = caplite::ui::startup::SUBSET_FONT_NAME;
/// 完整系统字体在 `FontDefinitions` 里的名字（追加时用）。
const FULL_FONT_NAME: &str = "cjk-full";

/// 追加系统完整中文字体作为**兜底**（首帧之后调用，见 `App::flush_deferred_init`）。
///
/// 用 `egui::Context::add_font` + `FontPriority::Lowest`：**追加**到字体族末尾，
/// 只在子集字体缺字时才被用到；这是"生僻字（设备名/文件名里的罕见汉字）最终也能显示"
/// 的保证，而代价（读 15~25 MB + 重建图集）被挪到了首帧之后。
///
/// 注意：`add_font` 会让 egui **在下一帧重建字体图集**（全量字形重新栅格化），
/// 那时窗口已经在屏幕上了，用户看到的是"文字立刻变全"，而不是"窗口迟迟不出来"。
fn install_full_cjk_font(ctx: &egui::Context) {
    let Some((name, data)) = read_system_cjk_data() else { return };
    // `FontInsert` 不在 egui 顶层 re-export 里（只有 `FontData`/`FontDefinitions` 在），
    // 所以走 `egui::epaint::text` —— 那是 **egui 自己 re-export 的 epaint**，
    // 不会引入任何新依赖（`Cargo.toml` 不需要动）。
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    ctx.add_font(FontInsert::new(
        FULL_FONT_NAME,
        data,
        vec![
            InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
        ],
    ));
    // 记一条：`add_font` 会让 egui 在**下一帧**重建字体图集，日志里能对上时间点
    tracing::debug!("已追加完整中文字体作为兜底: {name}");
}

/// 读第一个能读到的系统中文字体（返回路径，只用于日志）。
fn read_system_cjk() -> Option<String> {
    SYSTEM_CJK_CANDIDATES
        .iter()
        .map(|(p, _)| *p)
        .find(|p| std::fs::File::open(p).is_ok())
        .map(str::to_string)
}

/// 读第一个能读到的系统中文字体，返回 `(字体名, 数据)`。
fn read_system_cjk_data() -> Option<(&'static str, egui::FontData)> {
    for (path, index) in SYSTEM_CJK_CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = *index;
        return Some((FULL_FONT_NAME, data));
    }
    None
}

/// 解析命令行里的配置文件目录：`--config-dir <目录>` 或 `--config-dir=<目录>`。
///
/// 为什么支持它：受管环境里用户可能想把配置放到自己能控制的位置；
/// 有显式指定时它排在探测链最前面。解析失败只是"没指定"，不影响启动。
fn config_dir_from_args() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if let Some(rest) = text.strip_prefix("--config-dir=") {
            return caplite::config::resolve_config_dir(rest);
        }
        if text == "--config-dir" {
            return args
                .next()
                .and_then(|v| caplite::config::resolve_config_dir(&v.to_string_lossy()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 启动日志（release 没有控制台，所以落盘）
// ---------------------------------------------------------------------------

/// 日志文件名（落在**配置目录**里）。
const LOG_FILE_NAME: &str = "cap.log";

/// 单个日志文件上限；超过后轮转为 `cap.log.1`（运行期也会轮转）。
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// 轮转后的文件名后缀（`cap.log` → `cap.log.1`）。
const LOG_ROTATED_SUFFIX: &str = ".1";

/// 是否把日志写进文件：**只有 release**（见 [`StartupLog`] 的策略说明）。
const LOG_TO_FILE: bool = cfg!(not(debug_assertions));

/// 启动日志的落点与写法。
///
/// **为什么需要它**：release 版切到 Windows 子系统（`windows_subsystem = "windows"`）后进程
/// 不再有控制台，原来打在 stderr 的启动信息（配置目录 / 输出目录 / Cache 目录 / 目录回退原因）
/// 会**彻底看不见** —— 等于把诊断信息丢了。
///
/// **策略（二选一里选定的这一种）**：**release 只落盘、debug 只打控制台**。
/// * release：没有任何控制台可看，所以写 `<配置目录>\cap.log`；同时仍然保留 `eprintln!`
///   （用户把 stderr 重定向到文件时还有用，代价为零）。
/// * debug：**不落盘** —— 开发时每跑一次就多一个文件太脏，而调试时控制台信息本来就是直接可见的。
///
/// **绝不写死在 exe 同级**：日志目录沿用 [`ConfigStore`] 的**回退链结果**（那一步已经
/// "真写一个探测文件"验证过可写）；配置目录一个都写不了时退到
/// `%TEMP%\CapLite\cap.log`（与 [`caplite::cache`] 的临时兜底同一处）。
///
/// **写日志永远不能影响启动**：任何失败只 `eprintln!` 一条 warning，绝不 panic、绝不返回错误。
struct StartupLog {
    /// `None` = 连兜底目录都构造不出来（此时只打控制台，不落盘）
    path: Option<PathBuf>,
}

/// 后台配置保存器：UI 只把 `Config` 丢进 channel，真正的 `sync_all`/rename/杀毒软件
/// 重试都在 `cfg-save` 线程里，避免拖设置滑条时卡 UI。
struct ConfigSaver {
    tx: std::sync::mpsc::Sender<(u64, Config)>,
    rx: std::sync::mpsc::Receiver<(u64, Result<(), String>)>,
    next_id: u64,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ConfigSaver {
    fn new(store: ConfigStore) -> Self {
        let (tx, req_rx) = std::sync::mpsc::channel::<(u64, Config)>();
        let (res_tx, rx) = std::sync::mpsc::channel::<(u64, Result<(), String>)>();
        let handle = std::thread::Builder::new()
            .name("cfg-save".into())
            .spawn(move || {
                while let Ok((id, cfg)) = req_rx.recv() {
                    let r = store.save(&cfg).map_err(|e| e.to_string());
                    let _ = res_tx.send((id, r));
                }
            })
            .ok();
        Self {
            tx,
            rx,
            next_id: 0,
            handle,
        }
    }

    fn enqueue(&mut self, cfg: &Config) -> Option<u64> {
        self.next_id = self.next_id.wrapping_add(1);
        self.tx.send((self.next_id, cfg.clone())).ok()?;
        Some(self.next_id)
    }

    /// 收走所有已完成的结果（非阻塞）。
    fn drain(&mut self) -> Vec<(u64, Result<(), String>)> {
        let mut out = Vec::new();
        while let Ok(v) = self.rx.try_recv() {
            out.push(v);
        }
        out
    }

    /// 退出时等指定 id 的保存完成；超时返回 false，不阻塞进程退出。
    fn wait_for(&mut self, id: u64, timeout: std::time::Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(left.min(std::time::Duration::from_millis(100))) {
                Ok((got, _)) if got >= id => return true,
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn shutdown(mut self, cfg: &Config, timeout: std::time::Duration) -> bool {
        let id = self.enqueue(cfg);
        let ok = id.map(|id| self.wait_for(id, timeout)).unwrap_or(false);
        drop(self.tx);
        // 保存线程通常已经处理完；join 只在它真的结束时返回，避免退出路径再无限等。
        if let Some(h) = self.handle.take() {
            if h.is_finished() {
                let _ = h.join();
            }
        }
        ok
    }
}

/// 全局日志落点：给 [`append_mark_line`] 这个**无捕获**的函数指针用（见 `log_fn`）。
///
/// 只有 `StartupLog::new` 会设置它，一次运行里只设置一次（`OnceLock` 语义）；
/// 重复设置被忽略，所以"日志落点"在一次运行里是唯一且不可变的。
static LOG_PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// 把一行启动打点写进全局日志落点（`log_fn()` 返回的就是这个函数）。
///
/// 与 [`StartupLog::line`] 的差别只有一个：路径取自全局 [`LOG_PATH`] 而不是字段，
/// 这样才能做成 `fn(&str)` 到处传。落盘策略（release 写文件 / debug 只控制台）完全共用
/// [`append_log_line`]。
fn append_mark_line(msg: &str) {
    append_log_line(LOG_PATH.get().and_then(|p| p.as_deref()), msg);
}

impl StartupLog {
    /// `config_dir` / `config_writable` 来自 [`ConfigStore`] 的探测结果。
    fn new(config_dir: Option<&Path>, config_writable: bool) -> Self {
        // 配置目录不可写时 `ConfigStore::path()` 只是个裸文件名（父目录是空串），
        // 那种情况不能用它当日志目录，直接走兜底。
        let dir = if config_writable {
            config_dir.filter(|d| !d.as_os_str().is_empty())
        } else {
            None
        };
        let path = match dir {
            Some(d) => d.join(LOG_FILE_NAME),
            None => std::env::temp_dir().join("CapLite").join(LOG_FILE_NAME),
        };
        let log = Self { path: Some(path) };
        log.truncate_if_too_big();
        // 让 `log_fn()` 那个无捕获函数指针也能找到同一个文件（只第一次设置生效）
        let _ = LOG_PATH.set(log.path.clone());
        log
    }

    /// 日志文件路径（panic hook 与"启动失败"收尾都要用它，所以先取出来再交出所有权）。
    fn file_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    /// 一个**可以到处传的**日志落点：把一行字符串追加到同一个日志文件。
    ///
    /// 为什么需要它：启动打点发生在 `cap` 库里（设备枚举、字体装载），
    /// 而"日志写到哪里"是 bin 的决定。做成函数指针（而不是 `&StartupLog`）是为了
    /// 让它可 `Copy`、`'static`，能存进 `App`/`MainPage` 的字段里，也不牵扯借用。
    ///
    /// 落点本身仍然是 [`append_log_line`]：**release 才落盘、debug 只打控制台**，
    /// 与 [`StartupLog::line`] 完全同一条路径（绝不另开一套日志）。
    fn log_fn(&self) -> Option<fn(&str)> {
        Some(append_mark_line)
    }

    /// 启动时若日志已经超过 [`LOG_MAX_BYTES`]：轮转为 `cap.log.1`，保留一次历史。
    fn truncate_if_too_big(&self) {
        if !LOG_TO_FILE {
            return;
        }
        let Some(path) = self.path.as_deref() else { return };
        if std::fs::metadata(path)
            .map(|m| m.len() > LOG_MAX_BYTES)
            .unwrap_or(false)
        {
            rotate_log_file(path);
        }
    }

    /// 记一行启动信息：debug 打控制台，release 同时落盘。
    fn line(&self, msg: &str) {
        eprintln!("{msg}");
        append_log_line(self.path.as_deref(), msg);
    }
}

fn rotated_log_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_string_lossy().into_owned();
    s.push_str(LOG_ROTATED_SUFFIX);
    PathBuf::from(s)
}

/// 轮转失败（文件被占用/权限不足）不抛错：下一次再试，日志继续写旧文件。
fn rotate_log_file(path: &Path) -> bool {
    let rotated = rotated_log_path(path);
    let _ = std::fs::remove_file(&rotated);
    match std::fs::rename(path, &rotated) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[cap] 日志轮转失败（继续写当前文件）: {}（{e}）", path.display());
            false
        }
    }
}

/// 粗粒度脱敏：把绝对 Windows 路径（`C:\...`）与 UNC 路径（`\\server\...`）替换成
/// `<path>`，避免 `cap.log` 长期记录用户名、文件名、共享路径。
///
/// 只扫 ASCII 路径起点，UTF-8 文本按字符边界原样复制；路径在空白/引号/常见分隔符处结束。
fn redact_log_text(input: &str) -> String {
    fn path_start(b: &[u8], i: usize) -> bool {
        (i + 2 < b.len()
            && b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && (b[i + 2] == b'\\' || b[i + 2] == b'/'))
            || (i + 1 < b.len() && b[i] == b'\\' && b[i + 1] == b'\\')
    }
    fn path_end(b: &[u8], start: usize) -> usize {
        let mut i = start;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_whitespace()
                || matches!(c, b'"' | b'\'' | b')' | b']' | b'}' | b',' | b';' | b'|')
            {
                break;
            }
            i += 1;
        }
        i
    }

    let b = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < b.len() {
        if path_start(b, i) {
            out.push_str("<path>");
            i = path_end(b, i).max(i + 1);
        } else {
            let ch = input[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 按行写日志（带轮转）。`LogFile` 与 `append_log_line` 共用。
fn write_log_line(file: &mut LogFile, line: &[u8]) -> std::io::Result<()> {
    if file.size.saturating_add(line.len() as u64) > LOG_MAX_BYTES {
        // 先释放旧句柄，Windows 才允许 rename。
        file.file.take();
        if rotate_log_file(&file.path) {
            file.file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file.path)
                .ok();
            file.size = 0;
        } else if file.file.is_none() {
            file.file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file.path)
                .ok();
        }
    }
    let Some(f) = file.file.as_mut() else {
        return Err(std::io::Error::other("日志文件不可用"));
    };
    f.write_all(line)?;
    file.size = file.size.saturating_add(line.len() as u64);
    Ok(())
}

/// 追加一行到日志文件（**只有 release 真的写**；失败只 warn，绝不 panic）。
///
/// 独立成函数是给 panic hook 复用：hook 里只拿得到一个 `PathBuf`，拿不到 `StartupLog`
/// （hook 必须是 `'static`）。
fn append_log_line(path: Option<&Path>, msg: &str) {
    if !LOG_TO_FILE {
        return; // debug：控制台已经能看到，不落盘
    }
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut file = match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => LogFile {
            size: f.metadata().map(|m| m.len()).unwrap_or(0),
            file: Some(f),
            path: path.to_path_buf(),
        },
        // 日志写不进去（目录只读、磁盘满、被占用……）**不能影响启动**：只记一条 warning
        Err(e) => {
            eprintln!("[cap] 日志写入失败（不影响启动）: {}（{e}）", path.display());
            return;
        }
    };
    let line = format!("{}\n", redact_log_text(msg));
    if let Err(e) = write_log_line(&mut file, line.as_bytes()) {
        eprintln!("[cap] 日志写入失败（不影响启动）: {}（{e}）", path.display());
    }
}

/// `cap.log` 的当前句柄、路径与已写字节数（运行期轮转用）。
struct LogFile {
    file: Option<std::fs::File>,
    path: PathBuf,
    size: u64,
}

/// `tracing` 的文件 sink：一个进程级 `Mutex<LogFile>`，每条事件只锁一次、不重新 open。
#[derive(Clone)]
struct LogSink {
    file: std::sync::Arc<std::sync::Mutex<LogFile>>,
}

impl LogSink {
    fn new(path: &Path) -> Option<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Self {
            file: std::sync::Arc::new(std::sync::Mutex::new(LogFile {
                file: Some(file),
                path: path.to_path_buf(),
                size,
            })),
        })
    }
}

struct LogSinkWriter<'a> {
    file: &'a std::sync::Mutex<LogFile>,
    /// `tracing` 可能分多次 `write`；按行缓冲后统一脱敏/轮转。
    pending: Vec<u8>,
}

impl std::io::Write for LogSinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            let mut f = self
                .file
                .lock()
                .map_err(|_| std::io::Error::other("cap.log mutex poisoned"))?;
            let text = String::from_utf8_lossy(&line);
            let redacted = format!("{}\n", redact_log_text(text.trim_end_matches('\n')));
            write_log_line(&mut f, redacted.as_bytes())?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            let mut f = self
                .file
                .lock()
                .map_err(|_| std::io::Error::other("cap.log mutex poisoned"))?;
            let text = String::from_utf8_lossy(&line);
            let redacted = format!("{}\n", redact_log_text(text.trim_end_matches('\n')));
            write_log_line(&mut f, redacted.as_bytes())?;
        }
        let mut f = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("cap.log mutex poisoned"))?;
        if let Some(file) = f.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSinkWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogSinkWriter {
            file: &self.file,
            pending: Vec::new(),
        }
    }
}

/// release GUI 没有控制台，必须给 `tracing` 一个落盘 sink，否则所有 `warn!/error!`
/// 在用户机器上直接消失（这正是“采集失败/音频重建失败”最难定位的原因）。
///
/// 默认 `warn`；`RUST_LOG` 可覆盖（排障时用 `RUST_LOG=info`）。
fn init_tracing(log_path: Option<&Path>) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true);
    if LOG_TO_FILE {
        if let Some(sink) = log_path.and_then(LogSink::new) {
            let _ = builder.with_writer(sink).try_init();
            return;
        }
    }
    let _ = builder.try_init();
}

/// 把 panic 也写进同一个日志文件。
///
/// release 没有控制台，panic 默认只往 stderr 写 —— 那就是**静默消失、无从排查**。
/// 这里追加一行后**仍然调用原 hook**，所以 debug 的控制台输出、`RUST_BACKTRACE` 的行为都不变。
/// （`eframe::run_native` 返回 `Err` 的那条路径由 [`main`] 末尾单独记一行。）
fn install_panic_hook(path: Option<PathBuf>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        append_log_line(path.as_deref(), &format!("[cap] PANIC: {info}"));
        previous(info);
    }));
}

/// 当前 UTC 时间（`YYYY-MM-DD HH:MM:SSZ`），只用来给日志分次。
///
/// 手写日期换算（Howard Hinnant 的 civil_from_days）是为了**不引入新依赖** ——
/// 只为日志里一行时间不值得再加 chrono/time。
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

/// 收紧 DLL 搜索路径：只允许应用目录 + System32（+ 显式 AddDllDirectory），
/// 不再把**当前工作目录**放进搜索顺序。
///
/// 必须在任何 FFmpeg/OpenH264 `LoadLibrary` 之前调用（包括 delay-load helper）。
/// 失败开放：旧系统/受限环境拿不到这个 API 时保持默认行为，不让程序打不开。
fn harden_dll_search() {
    use windows::Win32::System::LibraryLoader::{
        AddDllDirectory, SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    };
    // SAFETY: 只改本进程的 DLL 搜索策略，不涉及指针/内存。
    let ok = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) };
    if ok.is_err() {
        eprintln!("[cap] SetDefaultDllDirectories 失败：保留默认 DLL 搜索顺序");
    }
    // 发布包把运行期 DLL 放在 <exe>\runtime。AddDllDirectory 加入的是
    // LOAD_LIBRARY_SEARCH_USER_DIRS；DEFAULT_DIRS 已包含用户目录，因此
    // 延迟加载/OpenH264 都能在这里解析。只在目录真实存在时添加，保留旧布局兼容。
    if let Some(runtime) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("runtime")))
        .filter(|p| p.is_dir())
    {
        let wide: Vec<u16> = runtime
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let cookie = unsafe { AddDllDirectory(windows::core::PCWSTR(wide.as_ptr())) };
        if cookie.is_null() {
            eprintln!("[cap] AddDllDirectory 失败：运行期 DLL 目录 {}", runtime.display());
        }
    }
}

fn main() -> eframe::Result<()> {
    // ---- 启动分阶段计时的"零点"：必须是 `main` 的**第一件事** ----
    // 这样 `t=` 就是"相对进程起点"的毫秒数（见 `caplite::ui::startup`）。动态链接/CRT 启动
    // 那一段在此之前、进程内拿不到 `Instant`，只能由外部脚本测墙钟（见 docs/DESIGN.md）。
    startup_marks::mark("process_start", "process");
    // 单实例：必须在任何配置/日志/Cache 写入之前认领。已有实例时把它恢复到前台，
    // 然后退出第二个进程，避免两个进程抢 config.json.tmp / rec-*.mp4 / 全局热键。
    if !caplite::single_instance::claim() {
        let _ = caplite::single_instance::activate_existing();
        return Ok(());
    }
    harden_dll_search();
    let config_dir = config_dir_from_args();
    // 是否"用户显式指定了配置目录"：探测会拿走 `config_dir`，所以先记下来。
    let explicit_config_dir_arg = config_dir.is_some();
    // ① 配置目录探测：放在这里是为了让**启动日志的落点**共用同一个结果（App::new 直接收下）。
    // ② 启动日志：release 落盘（没有控制台），debug 只打控制台。
    let store = ConfigStore::detect(config_dir);
    let log = StartupLog::new(store.path().parent(), store.writable());
    // 先装 tracing sink，后面所有 tracing::warn!/error! 才会落到同一个 cap.log。
    init_tracing(log.file_path().as_deref());
    // 打点从这一刻起真的落地：`enable` 会把"日志就绪之前"缓冲的那几笔（进程启动、
    // 配置目录探测）按原时间戳补写出来，所以下面这行 header 之后能立刻看到完整时间轴。
    startup_marks::enable(append_mark_line);
    startup_marks::mark_dur_extra_log(
        log.log_fn(),
        "log_ready",
        "process",
        startup_marks::start().elapsed(),
        format!("配置目录可用={}", store.writable()),
    );
    log.line(&format!(
        "[cap] ==== {} 启动 · {}{} ====",
        caplite::ui::theme::APP_NAME,
        utc_stamp(),
        if LOG_TO_FILE {
            " · release：无控制台，日志落盘"
        } else {
            " · debug：日志只打控制台，不落盘"
        }
    ));
    log.line("[cap] 启动计时起点 t=0 为 main() 第一行；此后每行 [cap][t=..ms] 都是相对它的毫秒数");
    if let Some(p) = log.file_path() {
        log.line(&format!("[cap] 日志文件: {}", p.display()));
    }
    // ③ release 的 panic 不能静默消失：写进同一个日志文件（debug 行为不变）。
    install_panic_hook(log.file_path());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1020.0, 720.0])
            // ---- 最小窗口尺寸：**实测推导**，不是拍脑袋 ----
            //
            // 需求：剪辑页的预览最小竖向尺寸是 200px（`editor::PREVIEW_MIN_H`），
            // 由此反推"整个程序最小能缩到多少"，并且**不允许出现上下滚动条**、
            // **不允许内容被裁掉/被两条栏盖住**。推导三步（数字都是 headless 量出来的，
            // 见 `caplite::ui::layout` 的模块注释与那里的三条单测）：
            //
            // ⚠️ **下面引用的数字必须与 `caplite::ui::layout` 的常量一致**。这里的代码
            //    引用的是**常量本身**（`MIN_INNER_W` / `MIN_INNER_H`），所以永远不会算错；
            //    容易过期的是括号里这些"当时量到多少"的说明文字 —— 改布局时**以常量为准**，
            //    并同步更新本段（`ui::layout` 的模块注释里有同样一份表格，两处一起改）。
            //
            // ① 宽度：把三页的宽度从 600 扫到 1100（再在 688~736 按 2 精扫），找"内容高度
            //    不再下降"的最小宽度（再窄就有行要换行 → 内容变高）：剪辑页在 718 以下会换行，
            //    720 起稳定；录制页从 600 起就不换行。
            //    所以"最宽一整行"≈719px → 取 `layout::MIN_INNER_W` = **760**
            //    （+40px 给字体版本差异留余量）。
            // ② 高度：把窗口压到"剪辑页预览已经触底"（预览下限 200px）时，
            //    `ScrollArea::content_size.y` 就是该页"再矮 1px 就得滚动"的必需高度
            //    （英文/中文各量一遍取大者）：
            //    * 剪辑页 `layout::EDITOR_REQ_H` = **515**（英文更高的一档）；
            //    * 录制页 `layout::RECORD_REQ_H` = **296**（P37 间距 + P38 两处并行之后，
            //      比剪辑页矮得多，正是用户要的方向）。
            //    再加上外壳 `layout::CHROME_H` = **126**（标签栏 + 状态栏 + 中央面板内边距）
            //    → `layout::MIN_INNER_H` = 126 + 4 + 26 + max(515, 366) = **671**。
            // ③ 两页冲突时**以录制页为准**（用户明确要求）：推导式写的就是
            //    `CHROME_H + max(EDITOR_REQ_H, RECORD_REQ_H)` —— 录制页更高就用录制页的
            //    （此时剪辑页预览反而比 200 更高，仍然满足"≥200"）；
            //    当前两个值分别是 515 / 296，所以最小高度由**剪辑页**决定。
            //
            // 结果：窗口不允许小于 `MIN_INNER_W × MIN_INNER_H`（当前 **760×671**），因此三页
            // **都不需要滚动**；三条 `ScrollArea` 也都设成
            // `ScrollBarVisibility::AlwaysHidden` + `auto_shrink([false, false])`
            // （内容是"刚好吃满"，滚动条永远不会被画出来，横向滚动则根本没开）。
            // 这些数字都写在 `caplite::ui::layout` 里（含逐项推导），并由三页的单测
            // + `main.rs` 的整窗单测**重新量一遍**，常量过期就会红。
            //
            // 待重新测量（P3，**本轮未做**）：波形重做与 P9 缩放锚点之后，剪辑页时间线
            // 那一带的内部构成变过（波形画在标记行内、片尾之后改画"无内容"底纹），
            // 但 `EDITOR_REQ_H` / `RECORD_REQ_H` 这**两个常量本身还没有重新量过** ——
            // 它们必须由 headless 测量（`cargo test` 的布局单测）重新推导，
            // 而本轮规则不允许跑测试，所以这里只标注"待重测"，**没有**改动常量。
            // 若重测后发现与常量不符，以重测值为准并同步 `ui::layout` 与本段。
            .with_min_inner_size([
                caplite::ui::layout::MIN_INNER_W,
                caplite::ui::layout::MIN_INNER_H,
            ])
            // 根窗口**不再**声明 transparent：框选遮罩已经改成"冻结帧 + 只压暗选区之外"，
            // 遮罩窗口本身是不透明的，不需要 GL 配置带 alpha 位。
            // （旧版为了让遮罩透出真桌面才开透明，实测在真机上不生效、屏幕被整块遮挡。）
            .with_title(caplite::ui::theme::WINDOW_TITLE)
            // ---- 窗口图标（标题栏左上角 + 任务栏）----
            //
            // 为什么必须在这里设：`build.rs` 嵌的是 **PE 资源图标**（文件图标），
            // 而**窗口/任务栏图标**由窗口库在**运行时**用 `WM_SETICON` 设置。eframe 0.36.2
            // 在 Windows 上走 `native/app_icon.rs`：把 `IconData` 缩到 `SM_CXICON`/`SM_CXSMICON`
            // → 编码 PNG → `CreateIconFromResourceEx` → 先 `ICON_BIG` 再 `ICON_SMALL` 两次 `WM_SETICON`；
            // 不调用 `with_icon` 时它退回自带的 egui logo（`load_default_egui_icon`），
            // 于是窗口/任务栏显示"默认图标"——这就是本次修的症状。
            //
            // 口径：`rgba` 是**非预乘** RGBA8（`IconData` 字段注释，`egui-0.36.2/src/viewport.rs`），
            // `width`/`height` 取 4 的倍数（256 ✔）。eframe 只把这张图当**单一源图**再自行
            // Lanczos 缩放，所以给 256 不会"发虚"：系统要 32/16 时是从 256 缩下去，
            // 比给 64 再被系统缩放更干净（取舍与实测见 `docs/DESIGN.md` §11.26）。
            .with_icon(eframe::egui::IconData {
                rgba: WINDOW_ICON_RGBA.to_vec(),
                width: WINDOW_ICON_PX,
                height: WINDOW_ICON_PX,
            }),
        ..Default::default()
    };
    // `run_native` 返回 Err 时（例如创建窗口/GL 上下文失败）默认只往 stderr 写，release 下
    // 同样看不见 —— 所以这里补记一行再原样返回错误（退出码交给调用方/系统）。
    let log_path = log.file_path();
    let result = eframe::run_native(
        caplite::ui::theme::APP_NAME,
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                &cc.egui_ctx,
                store,
                log,
                explicit_config_dir_arg,
                // 真机：装托盘图标 + 注册全局快捷键（见 `App::new` 的说明）
                true,
            )))
        }),
    );
    if let Err(e) = &result {
        append_log_line(
            log_path.as_deref(),
            &format!("[cap] 启动失败（eframe::run_native 返回错误）: {e}"),
        );
    }
    result
}

// ---------------------------------------------------------------------------
// 测试：**整窗**验证最小尺寸（外壳 + 三页一起量）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use caplite::ui::layout::{
        CHROME_H, EDITOR_REQ_H, MIN_INNER_H, MIN_INNER_W, RECORD_REQ_H,
    };

    #[test]
    fn config_saver_writes_on_a_worker() {
        let dir = std::env::temp_dir().join(format!("cap-cfg-saver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ConfigStore::detect(Some(dir.clone()));
        let cfg = Config::default();
        let mut saver = ConfigSaver::new(store.clone());
        let id = saver.enqueue(&cfg).expect("enqueue");
        assert!(saver.wait_for(id, std::time::Duration::from_secs(5)));
        assert!(store.path().is_file(), "后台保存应落盘");
        drop(saver);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_redaction_hides_drive_and_unc_paths() {
        let s = redact_log_text(
            "open C:\\Users\\Alice\\secret.mp4 and \\\\server\\share\\a b then ok",
        );
        assert!(!s.contains("Alice"), "{s}");
        assert!(!s.contains("server"), "{s}");
        assert!(s.contains("<path>"), "{s}");
        assert!(s.contains("then ok"), "普通文字不应被吃掉：{s}");
    }

    #[test]
    fn log_rotation_keeps_one_backup() {
        let dir = std::env::temp_dir().join(format!("cap-log-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.log");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut file = LogFile {
            file: Some(f),
            path: path.clone(),
            size: LOG_MAX_BYTES,
        };
        write_log_line(&mut file, b"hello\n").unwrap();
        assert!(rotated_log_path(&path).is_file(), "应生成 cap.log.1");
        assert!(path.is_file(), "新日志文件应重新创建");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **占用检测的"界面/日志确实给了告警"这一半**（另一半在 `caplite::tray` 里，
    /// 真机 `RegisterHotKey` 抢占用组合拿到 1409）。
    ///
    /// 这里用一条**真实的** `HotkeyRegisterError`（就是 `tray` 在 1409 时构造出来的那种）
    /// 走外壳的两条出口，断言：
    /// * 底部状态栏那条 warn 里带着 `1409` 与**建议组合**（用户看得见）；
    /// * `cap.log` 那行里带着同样的信息（排障看得见）；
    /// * 提示是 sticky 的 —— 不会被"下一次保存配置成功"顺手撤掉。
    #[test]
    fn hotkey_failure_produces_visible_warning_and_log_line() {
        let contested = caplite::tray::parse_hotkey("Ctrl+Shift+R").expect("合法组合");
        let suggestion = caplite::tray::parse_hotkey("Ctrl+Alt+F9").expect("合法组合");
        let err = caplite::tray::HotkeyRegisterError {
            hotkey: contested,
            code: caplite::tray::ERROR_HOTKEY_ALREADY_REGISTERED,
            system: "该组合已被其他程序占用".to_string(),
            suggestion: Some(suggestion),
        };
        assert_eq!(err.code, 1409, "ERROR_HOTKEY_ALREADY_REGISTERED");
        assert!(err.already_registered());

        for lang in ["zh-CN", "en-US"] {
            let i18n = I18n::new(lang);
            let text = hotkey_error_text(&i18n, &err);
            assert!(
                text.contains("Ctrl+Shift+R"),
                "[{lang}] 告警必须点名是哪个组合：{text}"
            );
            assert!(
                text.contains("Ctrl+Alt+F9"),
                "[{lang}] 告警必须给出建议组合：{text}"
            );
            assert!(
                !text.contains("hotkey_taken"),
                "[{lang}] 文案落到了 key 本身（漏翻）：{text}"
            );

            let notice = hotkey_warning_notice(&i18n, &err);
            assert_eq!(notice.kind, caplite::ui::theme::Banner::Warn);
            assert!(notice.sticky, "热键没了是真降级，提示不该被保存成功顺手撤掉");
            assert_eq!(notice.id, "hotkey_register_failed");
            assert_eq!(notice.text, text);

            // cap.log 那一行（语言中立）必须带错误码、组合名与建议
            let line = hotkey_log_line(&err);
            assert!(line.contains("1409"), "[{lang}] 日志缺错误码：{line}");
            assert!(line.contains("Ctrl+Shift+R"), "[{lang}] 日志缺组合名：{line}");
            assert!(line.contains("Ctrl+Alt+F9"), "[{lang}] 日志缺建议：{line}");
        }

        // 非 1409 的失败走另一条模板（也要有可见文案）
        let other = caplite::tray::HotkeyRegisterError {
            hotkey: contested,
            code: 87,
            system: "参数错误".to_string(),
            suggestion: None,
        };
        let i18n = I18n::new("zh-CN");
        let text = hotkey_error_text(&i18n, &other);
        assert!(text.contains("87") && text.contains("Ctrl+Shift+R"), "{text}");
        assert!(hotkey_log_line(&other).contains("87"));
    }

    /// 全局快捷键**切换**这条路径的边沿语义（纯状态机，不碰 Win32）：
    /// 捕获中不重注册、退出捕获按新配置注册一次、配置没变就不折腾。
    #[test]
    fn hotkey_capture_and_change_edges_are_well_defined() {
        // 复刻 `ui_root` 末尾那段判定，钉住"什么时候该调 apply_hotkey"。
        fn should_apply(capturing: bool, prev_capturing: bool, changed: bool) -> bool {
            if capturing != prev_capturing {
                !capturing // 退出捕获 → 重新注册；进入捕获 → 不注册（要的是注销）
            } else {
                changed && !capturing
            }
        }
        assert!(!should_apply(true, false, true), "进入捕获：不注册（正在注销）");
        assert!(should_apply(false, true, false), "退出捕获（没改配置）：按原组合装回去");
        assert!(should_apply(false, true, true), "退出捕获且改了配置：按新配置装");
        assert!(should_apply(false, false, true), "不在捕获、配置改了：重新注册");
        assert!(!should_apply(false, false, false), "什么都没变：一个 Win32 调用都不发");
        assert!(!should_apply(true, true, true), "捕获中：任何配置变化都等退出捕获再注册");
    }

    /// **设置页无窗口跑一遍**：新增的「常规」卡片（托盘开关 + 三选一 + 捕获控件）
    /// 必须在真实布局里画出来不 panic（egui 的重复 id / 非法尺寸都会在这里炸），
    /// 并且三个预设各画一帧（自定义那一支会走"捕获控件"的代码路径）。
    #[test]
    fn headless_settings_page_draws_every_hotkey_preset() {
        let ctx = measure_ctx();
        for lang in ["zh-CN", "en-US"] {
            for preset in [
                caplite::tray::HotkeyPreset::CtrlAltR,
                caplite::tray::HotkeyPreset::CtrlShiftR,
                caplite::tray::HotkeyPreset::Custom,
            ] {
                let mut app = test_app(&ctx, lang);
                app.tab = Tab::Settings;
                app.cfg.hotkey_preset = preset;
                app.cfg.hotkey_custom = "Ctrl+Shift+F9".to_string();
                let mut time = 0.0;
                for _ in 0..3 {
                    frame(&ctx, &mut app, MIN_INNER_W, MIN_INNER_H, time);
                    time += 0.5;
                }
                // 这一页不进最小高度推导，但必须"能画"：外壳读数正常就说明画完了
                assert!(
                    app.test_page_avail_h.is_some_and(|h| h > 100.0),
                    "[{lang}/{preset:?}] 设置页没画出来"
                );
            }
        }
    }

    /// 装**和真机同一套**字体（内置中文子集）+ 应用真主题。
    ///
    /// 绝对尺寸的断言必须这样做：没有汉字字形时中文会被当成缺字，
    /// 量出来的行高/行宽都不是真机上的值。
    fn measure_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            caplite::ui::startup::SUBSET_FONT_NAME.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(
                caplite::ui::startup::SUBSET_FONT_BYTES,
            )),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, caplite::ui::startup::SUBSET_FONT_NAME.to_owned());
        ctx.set_fonts(fonts);
        caplite::ui::theme::apply(&ctx);
        ctx
    }

    /// 造一个**不碰用户目录**的 `App`：配置目录与输出目录都落在临时目录里。
    ///
    /// 其余全部走真代码路径（字体/主题/三页构造/延后初始化），因为这里要量的就是真布局。
    fn test_app(ctx: &egui::Context, lang: &str) -> App {
        let dir = std::env::temp_dir().join(format!("caplite-layout-{lang}"));
        let _ = std::fs::create_dir_all(&dir);
        let store = ConfigStore::detect(Some(dir.clone()));
        // 先把语言与输出目录写进临时配置：`App::new` 会读它（这样 i18n 也走真路径）
        let cfg = Config {
            output_dir: dir.to_string_lossy().to_string(),
            language: lang.to_string(),
            ..Config::default()
        };
        let _ = store.save(&cfg);
        let log = StartupLog::new(store.path().parent(), store.writable());
        // 无窗口单测**不装托盘、不注册全局热键**：那是真机副作用（会往通知区域塞图标、
        // 占用一个系统级热键），测试进程不该做，也不需要（`enable_tray = false`）。
        App::new(ctx, store, log, false, false)
    }

    /// 无窗口跑一帧整个窗口（标签栏 + 页面 + 状态栏）。
    fn frame(ctx: &egui::Context, app: &mut App, w: f32, h: f32, time: f64) {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(w, h),
            )),
            ..Default::default()
        };
        input.time = Some(time);
        ctx.run_ui(input, |ui| app.ui_root(ui))
            .drop_without_applying_deltas();
    }

    /// **最小窗口尺寸的整窗验证**（`ui::layout` 里那些常量的守门人）。
    ///
    /// 单页的实测在库里有各自一条单测；这一条补的是**只有二进制才知道的两件事**：
    /// ① 外壳（标签栏 + 状态栏）到底占多少高度；② 在真实窗口尺寸下，
    /// 页面拿到的可用高度是不是 ≥ 那一页的必需高度（也就是"不会出现滚动条"）。
    #[test]
    fn headless_min_window_fits_every_page() {
        // ① 外壳高度：在"窗户够大"的尺寸下量（外壳高度与窗口尺寸无关，但页面要能正常布局）
        let ctx = measure_ctx();
        let mut app = test_app(&ctx, "zh-CN");
        let mut time = 0.0;
        for _ in 0..3 {
            frame(&ctx, &mut app, 1000.0, 700.0, time);
            time += 0.5;
        }
        let chrome = app.test_chrome_h.expect("应记录外壳高度");
        println!("[layout] 外壳（标签栏 + 状态栏）实测高度 = {chrome:.1}");
        assert!(
            (chrome - CHROME_H).abs() <= 0.5,
            "外壳实测高度 {chrome:.1} 与常量 CHROME_H={CHROME_H} 不一致"
        );
        // 外壳是**固定量**：换个窗口尺寸再量一次，必须一模一样，
        // 并且"页面可用高度 = 窗口高 − 外壳"这条关系要成立（推导链全靠它）。
        frame(&ctx, &mut app, 1000.0, 800.0, time);
        let chrome2 = app.test_chrome_h.expect("应记录外壳高度");
        let page2 = app.test_page_avail_h.expect("应记录页面可用高度");
        assert!(
            (chrome2 - chrome).abs() <= 0.5,
            "外壳高度不该随窗口尺寸变化：{chrome:.1}（700 高）vs {chrome2:.1}（800 高）"
        );
        assert!(
            (page2 - (800.0 - chrome)).abs() <= 0.5,
            "页面可用高度应等于窗口高 − 外壳：{page2:.1} vs {:.1}",
            800.0 - chrome
        );
        time += 0.5;

        // ② 录制页（默认标签页）：最小尺寸下"不需要滚动"
        app.tab = Tab::Home;
        for _ in 0..3 {
            frame(&ctx, &mut app, MIN_INNER_W, MIN_INNER_H, time);
            time += 0.5;
        }
        let rec = app.main.layout_probe();
        println!(
            "[layout] 最小窗口({MIN_INNER_W}×{MIN_INNER_H})下录制页：内容 {:.1} / 视口 {:.1} / 页面可用 {:.1}",
            rec.content_h, rec.viewport_h, rec.page_avail_h
        );
        assert!(
            rec.page_avail_h >= RECORD_REQ_H - 0.5,
            "最小窗口下录制页只拿到 {:.1}，少于它需要的 {RECORD_REQ_H}（外壳 {chrome:.1}）",
            rec.page_avail_h
        );
        assert!(
            rec.no_vertical_scroll(),
            "最小窗口下录制页内容 {:.1} 溢出视口 {:.1}（会出现滚动条）",
            rec.content_h,
            rec.viewport_h
        );

        // ③ 剪辑页：需要真素材（随包 ffmpeg 现场生成 2 秒 16:9 片）；环境不具备就跳过这一段
        let Some(path) = gen_test_clip() else {
            println!("[layout] 跳过剪辑页整窗验证：随包 ffmpeg 不可用");
            return;
        };
        app.editor.open(&path);
        app.tab = Tab::Editor;
        // **P34**：打开是异步的（探测 + 扫关键帧在后台线程）—— 这里跑真帧等它落地，
        // 走的正是生产路径（`ui()` 每帧 `poll_open`），而不是绕过它去直接读状态。
        {
            let t0 = std::time::Instant::now();
            while app.editor.loaded_path().is_none() {
                frame(&ctx, &mut app, MIN_INNER_W, MIN_INNER_H, time);
                time += 0.5;
                assert!(
                    t0.elapsed() < std::time::Duration::from_secs(30),
                    "剪辑页应在 30 s 内载入测试素材"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(app.editor.loaded_path().is_some(), "应成功载入测试素材");
        for _ in 0..3 {
            frame(&ctx, &mut app, MIN_INNER_W, MIN_INNER_H, time);
            time += 0.5;
        }
        let ed = app.editor.layout_probe();
        println!(
            "[layout] 最小窗口({MIN_INNER_W}×{MIN_INNER_H})下剪辑页：内容 {:.1} / 视口 {:.1} / 页面可用 {:.1}",
            ed.content_h, ed.viewport_h, ed.page_avail_h
        );
        println!(
            "[layout] 真机外壳：窗口高 {MIN_INNER_H} − 页面可用 {:.1} = {:.1}（常量 CHROME_H={CHROME_H}）",
            ed.page_avail_h,
            MIN_INNER_H - ed.page_avail_h
        );
        assert!(
            ed.page_avail_h >= EDITOR_REQ_H - 0.5,
            "最小窗口下剪辑页只拿到 {:.1}，少于它需要的 {EDITOR_REQ_H}（外壳 {:.1}，\
             窗口 {MIN_INNER_H}）",
            ed.page_avail_h,
            MIN_INNER_H - ed.page_avail_h
        );
        assert!(
            ed.no_vertical_scroll(),
            "最小窗口下剪辑页内容 {:.1} 溢出视口 {:.1}（会出现滚动条）",
            ed.content_h,
            ed.viewport_h
        );
    }

    /// 随包 ffmpeg 现场生成 2 秒 320×180（16:9）素材；环境不具备时返回 `None`（跳过而不是失败）。
    fn gen_test_clip() -> Option<PathBuf> {
        let ff = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
            .join("bin")
            .join("ffmpeg.exe");
        if !ff.is_file() {
            return None;
        }
        let out = std::env::temp_dir().join("caplite-layout-min.mp4");
        let _ = std::fs::remove_file(&out);
        let o = std::process::Command::new(&ff)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30",
                "-t",
                "2",
                "-c:v",
                "libopenh264",
                "-b:v",
                "500k",
                "-g",
                "30",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            Some(out)
        } else {
            println!(
                "[layout] 生成测试素材失败（跳过剪辑页整窗验证）：{}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
    }

    /// **P45 守门测试：配置落盘节流（滑条拖动不再每帧写盘），但改动一次都不许丢。**
    ///
    /// 三件事：
    /// ① 纯函数口径：第一次立刻写、200ms 窗口内不写、到点写；
    /// ② 行为：连改两次只落一次盘，第二次挂 `cfg_save_pending`（**磁盘还是旧值**）；
    /// ③ 补写：`flush_cfg_save()`（松手 / 退出 / 节流到点那条路）之后磁盘上是**最后**的值。
    ///
    /// 为什么值得一条测试：节流最容易出的 bug 是"最后一次改动被吞掉" ——
    /// 用户拖完滑条直接关窗口，配置回到旧值，且没有任何提示。
    #[test]
    fn p45_cfg_save_is_throttled_but_never_lost() {
        let t0 = Instant::now();
        assert!(cfg_save_due(None, t0), "本次运行第一次改动必须立刻落盘");
        assert!(
            !cfg_save_due(Some(t0), t0 + CFG_SAVE_MIN_GAP / 2),
            "节流窗口内不该写盘"
        );
        assert!(
            cfg_save_due(Some(t0), t0 + CFG_SAVE_MIN_GAP),
            "窗口到期就该写"
        );

        let ctx = egui::Context::default();
        let mut app = test_app(&ctx, "zh-CN");
        let read_disk = |app: &App| -> Config {
            let txt = std::fs::read_to_string(app.store.path()).expect("配置应已落盘");
            serde_json::from_str(&txt).expect("落盘内容应能读回")
        };

        // ② 第一次改动：立刻落盘
        app.cfg.fps = 60;
        app.commit_cfg();
        assert!(!app.cfg_save_pending, "第一次改动不该被推迟");
        // 保存现在是后台线程：等这一笔完成再读盘。
        if let Some(id) = app.cfg_save_inflight {
            assert!(app
                .cfg_saver
                .as_mut()
                .unwrap()
                .wait_for(id, std::time::Duration::from_secs(5)));
        }
        app.pump_cfg_saves();
        assert_eq!(read_disk(&app).fps, 60);

        // 紧接着再改一次：被节流 —— 内存是最新值，磁盘还是上一次的
        app.cfg.fps = 30;
        app.commit_cfg();
        assert!(app.cfg_save_pending, "200ms 内的第二次改动应被推迟");
        assert_eq!(app.cfg.fps, 30, "内存里必须立刻是最新值");
        assert_eq!(read_disk(&app).fps, 60, "节流窗口内磁盘不该被改写");

        // ③ 补写（松手 / 退出 / 到点）：磁盘追平内存
        app.flush_cfg_save();
        assert!(!app.cfg_save_pending);
        if let Some(id) = app.cfg_save_inflight {
            assert!(app
                .cfg_saver
                .as_mut()
                .unwrap()
                .wait_for(id, std::time::Duration::from_secs(5)));
        }
        app.pump_cfg_saves();
        assert_eq!(read_disk(&app).fps, 30, "补写之后磁盘必须是最后一次的值");
        // 没有待写内容时 flush 是空操作（不能因为多调一次就多写一次盘）
        app.flush_cfg_save();
        assert_eq!(read_disk(&app).fps, 30);
    }

    /// **P46 守门测试：启动时不打开任何视频，哪怕 `last_video_path` 指向一条真素材。**
    ///
    /// 这条测的是**外壳**那一侧（剪辑页自己的构造器由
    /// `ui::editor::tests::p46_new_never_autoloads_last_video_path` 守）。
    /// 改造前 `App::new` 会把 `last_video_path` 记进 `pending_editor_open`，
    /// 首帧之后由 `flush_deferred_init` 去 `editor.open()` —— 于是"启动就自动载入上次的视频"。
    ///
    /// **为什么必须用真素材，而不能用一个假的 `.mp4`**：老逻辑真的会去 `open()`，
    /// 而假文件在异步打开链里必然失败、`loaded_path()` 仍然是 `None` ——
    /// 那样这条测试在**改造前**也会通过，等于什么都没守住。用随包 ffmpeg 生成的真片
    /// 才能区分"没打开"和"打开了但失败"。
    #[test]
    fn p46_startup_never_opens_last_video() {
        let Some(clip) = gen_test_clip() else {
            println!("[p46] 跳过：随包 ffmpeg 不可用，无法生成真实素材");
            return;
        };
        let ctx = measure_ctx();
        let dir = std::env::temp_dir().join("caplite-p46-no-autoload");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        let store = ConfigStore::detect(Some(dir.clone()));
        let cfg = Config {
            output_dir: dir.to_string_lossy().to_string(),
            language: "zh-CN".to_string(),
            last_video_path: Some(clip.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let _ = store.save(&cfg);
        let log = StartupLog::new(store.path().parent(), store.writable());
        let mut app = App::new(&ctx, store, log, false, false);

        // 跑几帧：`flush_deferred_init` 在首帧之后执行，改造前正是在那里自动打开
        let mut time = 0.0;
        for _ in 0..4 {
            frame(&ctx, &mut app, 1000.0, 700.0, time);
            time += 0.5;
        }
        assert_eq!(
            app.editor.loaded_path(),
            None,
            "P46：启动后剪辑页不该载入任何视频（last_video_path 只用来定位目录）"
        );
        // 关键的一条：`open()` 是异步的，"在途"期间 `loaded_path()` 也是 `None`。
        // 只断言前者的话，"启动时悄悄起了个打开线程"会被漏掉。
        assert!(
            !app.editor.is_opening(),
            "P46：启动阶段连打开都不该起头（不该有在途的打开线程）"
        );
        assert!(app.tab == Tab::Home, "P46：启动仍停在录制页，不该被切走");
        assert_eq!(
            app.cfg.last_video_path.as_deref(),
            Some(clip.to_string_lossy().as_ref()),
            "配置里那条路径本身不该被清掉（它还是「打开视频」的目录线索）"
        );

        // 反向确认：**显式**交接照旧 —— 切页 + 真的把文件交给剪辑页
        app.open_in_editor(&clip, false);
        assert!(app.tab == Tab::Editor, "「在剪辑页打开」必须切到剪辑页");
        let t0 = Instant::now();
        while app.editor.loaded_path().is_none() {
            frame(&ctx, &mut app, 1000.0, 700.0, time);
            time += 0.5;
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(30),
                "「在剪辑页打开」应在 30 s 内把素材载入（异步打开链没被删）"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            app.editor.loaded_path(),
            Some(clip.as_path()),
            "「在剪辑页打开」载入的必须正是刚才交过去的那一条"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
