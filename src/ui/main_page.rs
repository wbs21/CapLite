//! 主页面（录制页）：采集范围、内容与音源、输出位置、开始/停止、实时状态与结果。
//!
//! 界面层只负责两件事：把用户意图写进 [`Config`]，以及把 [`Recorder`] 的状态画出来。
//! 真正的采集/编码/封装都在 [`crate::recorder`] 的四个线程里，所以这里：
//! * 任何改动都通过返回值告诉外壳（外壳统一 `sanitized()` + `save()`），
//! * 任何失败都落成界面可见的提示，绝不 `unwrap()` / `expect()`。
//!
//! 框选区域用一个**独立 viewport**（原生窗口）做遮罩：只有它能覆盖整个桌面、
//! 常驻最前，而主窗口在框选期间会被 egui 暂停。坐标换算见 `OverlayMapping`。
//!
//! 遮罩的实现方式（历史上踩过坑，见 `capture_desktop` 的注释）：
//! **进入框选时先用 GDI 抓一张当前桌面的静态截图**，遮罩窗口是**不透明**的，
//! 把截图按 1:1 铺满显示，然后**只把选区之外压暗**。这样用户看到的就是"刚才的桌面"，
//! 选区始终是原亮度。早期版本用"透明窗口 + 四条压暗带"想直接透出真桌面，
//! 但 eframe 的 immediate viewport 在某些 GL/合成配置下拿不到真正的逐像素透明，
//! 用户实测看到的是**整屏被遮挡**——那正是本文件改成冻结帧方案的原因。

use crate::audio::devices::{list_capture_devices, AudioDevice};
use crate::config::{CaptureMode, Config, ContentMode, Region};
use crate::error::AppError;
use crate::i18n::I18n;
use crate::recorder::{Recorder, RecorderState, RecorderStats, RecordingResult};
use crate::ui::layout::LayoutProbe;
use crate::ui::startup as startup_marks;
use crate::video::capture::{list_monitors, MonitorEntry};

use super::theme;
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Stroke, StrokeKind, Vec2};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, EnumDisplayMonitors, GetDC, GetDIBits,
    GetMonitorInfoW, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HDC, HGDIOBJ,
    HMONITOR, MONITORINFOEXW, SRCCOPY,
};

/// `MONITORINFO::dwFlags` 里的"主显示器"位（windows crate 没生成这个常量，值是 winuser.h 里的 1）
const MONITORINFOF_PRIMARY: u32 = 1;

/// 框选遮罩的 viewport id（同一时刻只允许一个）
const PICKER_SALT: &str = "cap-region-picker";

/// 选区最小边（物理像素）。小于它 H.264 基本没有意义，`Region::is_valid` 也会判为非法。
const MIN_REGION_SIDE: i64 = 16;

// ---------------------------------------------------------------------------
// 逐部件高度探针（**只在测试构建里存在**）
// ---------------------------------------------------------------------------

/// 录制页**逐部件高度**的实测量（逻辑点）。
///
/// 为什么要这个结构：用户要一份"录制页每个部件的实际高度表"，而**所有数字必须来自真布局**。
/// 唯一的办法是在真正画界面的那几行里顺手记下 `Response::rect`（`ui.horizontal` /
/// `ui.label` / `cards` 的实测矩形），而不是在测试里另画一份"看起来一样"的近似布局
/// —— 后者量的是测试自己，改真代码时那些数字不会跟着动，也就证明不了任何事。
///
/// 因此这里的每个数字都是**同一份绘制代码**在无窗口 `egui::Context::run_ui` 里跑出来的。
/// 只在 `#[cfg(test)]` 下存在：发布构建里这些字段与赋值全部消失，没有任何运行期代价。
///
/// 数据源单测：`main_page::tests::dump_record_page_heights`（`#[ignore]`，**不参与常规测试**）。
/// 类型本身在所有构建里都存在（`pub`，作为那几个 `*_section` 的参数类型）；
/// **只有录制页的那个字段与全部赋值是 `#[cfg(test)]`**，发布构建里没有任何运行期代价。
#[derive(Debug, Default, Clone, Copy)]
pub struct RecordProbe {
    /// 页面留给内容的宽度（`ui.max_rect().width()`）
    pub page_avail_w: f32,
    /// 页面拿到的可用高度（`ui.max_rect().height()`，已扣掉外壳）
    pub page_avail_h: f32,
    /// 页面内容区顶边（`ui.max_rect().top()`，用于算首尾留白）
    pub page_top: f32,
    /// 滚动区**内容**的顶边
    pub content_top: f32,
    /// 滚动区**内容**的底边（最后一张卡片的底）
    pub content_bottom: f32,

    /// 状态卡片：整卡高度（含内边距 + 描边）
    pub status_card_h: f32,
    /// 状态卡片：内容高度（不含内边距 + 描边）
    pub status_card_inner_h: f32,
    /// 状态卡片那一行（状态点 + 状态文字 + 时间码 + 主按钮）的高度
    pub status_row_h: f32,
    /// 状态点直径
    pub status_dot_h: f32,
    /// 状态文字（"就绪"）自身的高度
    pub status_text_h: f32,
    /// 时间码文本自身的高度（**大号等宽**）
    pub timer_text_h: f32,
    /// 主操作按钮高度（"开始录制"）
    pub start_btn_h: f32,
    /// 主操作按钮宽度
    pub start_btn_w: f32,
    /// 状态卡片那一行里，顶部/底部"空白"（= 行高 − 最高部件高）
    pub status_row_stack_h: f32,

    /// 采集卡片：整卡 / 内容 / 「显示器 + 录制范围」合并行（P38）
    pub capture_card_h: f32,
    pub capture_card_inner_h: f32,
    /// 「显示器 + 录制范围」合并行行高（P38 起采集卡只有这一行）
    pub capture_row_h: f32,
    /// 该行**最后一个控件**的右缘 x（不换行/不被裁切的凭证，见
    /// `record_page_capture_and_audio_rows_fit`；行尾还剩一段空隙是正常的）
    pub capture_row_last_x: f32,
    /// 采集卡内两行之间的间距。P38 起合并成一行 → **0**（历史：曾有
    /// 「录制范围」/「显示器 + 信息」两行，各相距 `GAP_ROW_TIGHT`）
    pub capture_gap_h: f32,

    /// 录制内容卡片：整卡 / 内容 / 「录制内容」行 / 「音源」标签与复选框同行（P38）
    pub content_card_h: f32,
    pub content_card_inner_h: f32,
    pub rec_mode_row_h: f32,
    /// 「音源（可多选）」标签行高（P38 起标签与复选框**同行**，标签自身仍约 16 高）
    pub audio_label_h: f32,
    /// 复选框行高（与标签同行的那一行）
    pub audio_row_h: f32,
    /// 该行**最后一个控件**的右缘 x（同 `capture_row_last_x`）
    pub audio_row_last_x: f32,
    /// 「录制内容」行与「音源」行之间的间距（`GAP_ROW_TIGHT`）
    pub content_gap_h: f32,
    /// 「音源（可多选）」标签与第一个复选框之间的**行内**小间距（P38 起 10.0；
    /// 历史：标签曾独占一行，此处是 2.0 的"小标题行距"）
    pub audio_label_gap_h: f32,

    /// 存储位置卡片：整卡 / 内容 / 「存储位置」那一行
    pub output_card_h: f32,
    pub output_card_inner_h: f32,
    pub output_row_h: f32,

    /// 卡片之间的间距（`GAP_CARD_TIGHT`，共 3 处）
    pub card_gap_h: f32,
    /// 卡片内边距（`PAD_CARD_TIGHT`，上下各这么多）
    pub card_pad: f32,

    /// 合计：页面内容高度（末卡底 − 内容顶）
    pub total_content_h: f32,
    /// 合计：首留白（内容顶 − 页面可用区顶）
    pub lead_blank_h: f32,
    /// 合计：尾留白（页面可用区底 − 末卡底）；`None` = 没量到（页面可用区底未知）
    pub tail_blank_h: Option<f32>,
    /// 滚动区 `content_size.y`（探针核对用）
    pub scroll_content_h: f32,
    /// 滚动区视口高度
    pub scroll_viewport_h: f32,
    /// **滚动区自己的口径**下"内容底"= 内容层 `Ui::min_rect().bottom()`。
    ///
    /// 它**不等于**各卡片高度之和：`Ui::add_space` 既推进游标、又把 `min_rect` 撑大，
    /// 所以滚动区的 `content_size` **把行距重复计了一次**（标定见
    /// `tests::calibrate_scroll_content_size`：同一份卡片、只换最后一张的高度，
    /// 差值恒定 = 间距之和）。差额记在 [`Self::nested_layout_residual_h`]。
    pub scroll_min_rect_bottom: f32,
    /// 差额：`content_size.y` − （四张卡片 + 三个卡片间距）＝ 嵌套布局（每张卡片内部的
    /// 行距/小标题间距）在滚动区口径里被重复计入的部分。它是**恒定项**，与卡片自身高度无关。
    pub nested_layout_residual_h: f32,
}

/// 主页面需要的"外壳状态"。录制会话与结果由外壳持有，页面只借用。
pub struct MainCtx<'a> {
    pub cfg: &'a mut Config,
    pub i18n: &'a I18n,
    pub recorder: &'a mut Option<Recorder>,
    pub result: &'a mut Option<RecordingResult>,
}

/// 页面画完后要告诉外壳的事
#[derive(Default)]
pub struct MainOutcome {
    /// 配置被用户改过（外壳负责 `sanitized()` + `save()`）
    pub cfg_changed: bool,
    /// 需要在剪辑页打开的文件：**优先导出产物**（用户在输出目录里的那个普通 MP4），
    /// 只有导出失败时才回退到 **Cache 里保留的流文件**（两者**同名同基名**，分处两个目录）
    pub open_in_editor: Option<PathBuf>,
    /// 上面那个文件是不是"导出失败后的分片回退"（界面要如实说明）
    pub open_in_editor_fallback: bool,
    /// 真错误（启动/停止录制失败、目录不可写…）→ 外壳的底部状态栏用红色显示
    pub error: Option<String>,
    /// 警告（本次录制没有声音、输出目录被回退…）→ 底部状态栏用琥珀色显示
    pub warning: Option<String>,
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

pub struct MainPage {
    /// 显示器列表（`list_monitors`，顺序与 [`MonitorGeo`] 一致）
    monitors: Vec<MonitorEntry>,
    /// 各显示器的物理矩形（EnumDisplayMonitors 顺序 == winit `available_monitors` 顺序）
    geo: Vec<MonitorGeo>,
    monitors_err: Option<String>,
    monitors_loaded: bool,
    /// 上一次枚举显示器的时刻（约每 5 秒刷新一次，**不每帧枚举**）
    last_monitor_refresh: Instant,
    /// **延后初始化**的剩余帧数（见 [`MainPage::flush_deferred_init`]）。
    ///
    /// 为什么要有它：显示器/DXGI、WASAPI 枚举与"目录可写性探测（真写文件）"都在启动路径上，
    /// 加起来是几十到几百毫秒的**纯等待**，用户看到的是"双击后窗口迟迟不出现"。
    /// 现在改成"窗口先出来，枚举在首帧之后马上补上"，`0` 表示已经执行过（不重复）。
    defer_ticks: u8,
    /// 延迟预热的最早时刻：启动后先让纯剪辑/查看用户保持低占用，
    /// 真正准备录制（或过了空闲窗口）再建 OpenH264 实例。
    prewarm_at: Instant,
    /// 是否已经触发过首次预热（之后配置变化才立即重建）。
    prewarm_started: bool,
    /// 启动阶段打点用的日志落点（由外壳注入；`None` = 不记，单测就是这种）
    startup_log: Option<fn(&str)>,
    /// 区域模式下换屏后的提示（"请重新框选"之类）：走外壳底部栏的 warn 通道
    region_hint: Option<String>,

    /// 麦克风列表（麦克风可能被禁用/没有，所以是 Option）
    mic_devices: Option<Vec<AudioDevice>>,
    mic_err: Option<String>,

    /// 框选遮罩的状态（None = 遮罩没打开）
    picker: Option<Picker>,
    /// 开始/停止/设备枚举失败的可见提示
    error: Option<String>,
    /// 输出目录探测的缓存：上次探测时用的目录值（避免每帧都真写一个临时文件）
    output_probe: Option<String>,
    /// 防抖中的待探测目录：`(目录值, 最早探测时刻)`。
    output_probe_pending: Option<(String, Instant)>,
    /// 输出目录探测的请求代际：目录一变就 +1，旧线程的结果直接丢弃。
    output_probe_epoch: u64,
    /// 后台目录探测结果（不阻塞 UI）。
    output_probe_rx: Option<Receiver<(u64, String)>>,
    /// 当前探测任务的失效令牌（结果代际之外再加一层，防止旧任务继续跑）。
    output_probe_cancel: Option<Arc<AtomicBool>>,
    /// 外壳已经解析好的 Cache 根目录；`None` = 还没注入（测试/旧路径）。
    /// 有它之后开始录制不再自己 `ConfigStore::detect(None)`（那会重复探测，
    /// 并且会忽略 `--config-dir`）。
    cache_dir: Option<PathBuf>,

    // ---- 异步开始/停止 ----
    /// 正在后台初始化采集/编码/音频（`Recorder::start` 可能要 0.5–6 s）。
    starting: bool,
    start_rx: Option<Receiver<Result<Recorder, AppError>>>,
    start_handle: Option<JoinHandle<()>>,
    /// 当前启动任务的协作取消令牌。
    start_cancel: Option<Arc<AtomicBool>>,
    /// 正在后台 join + 导出（`Recorder::stop` 可能几分钟）。
    stopping: bool,
    stop_rx: Option<Receiver<Result<RecordingResult, AppError>>>,
    stop_handle: Option<JoinHandle<()>>,
    /// 当前收尾任务的协作取消令牌。
    stop_cancel: Option<Arc<AtomicBool>>,
    /// 看门狗：超过这些时刻就报超时并放弃等待（后台线程可能仍在收尾）。
    start_deadline: Option<Instant>,
    stop_deadline: Option<Instant>,
    /// 本次看门狗是否已经触发；退出路径据此返回 false。
    watchdog_fired: bool,
    /// 本次停止完成后要不要继续关闭窗口（托盘 Quit / 用户点 X）。
    pending_quit: bool,
    /// “启动完成后立刻停止”：用户在启动阶段按停止/系统锁屏时用。
    stop_after_start: bool,
    /// 输出目录的提示（"原目录不可写，已改用 X" / "没有任何可写的输出目录：…"）
    output_dir_note: Option<String>,
    /// 首帧标记：默认配置里的选区是占位值，需要用真实主屏尺寸替换
    first_frame: bool,
    /// 本帧需要打开的框选遮罩（"选择区域"那一刻自动弹出，见 [`Self::capture_section`]）。
    /// 为什么不在 `capture_section` 里直接开：那需要同时可变借用 `self` 的两部分，
    /// 而它在 `self.monitors` 的迭代里。延后一帧处理最省事，也没有可见延迟。
    pending_pick: bool,

    /// 本帧的布局测量（内容高度 / 视口高度 / 可用高度）。
    ///
    /// 每帧都记（不是 `cfg(test)`）：最小窗口尺寸的**整窗**验证在 `CapLite` 二进制里，
    /// 那里读不到库的 `cfg(test)` 符号，所以这是常规接口（理由见 [`LayoutProbe`]）。
    probe: LayoutProbe,

    /// 本帧的**逐部件高度**实测量（见 [`RecordProbe`]）。
    ///
    /// 与 [`Self::probe`] 不同，这一项**只在测试构建里编译**：它服务的是"逐部件高度表"
    /// 那个 `#[ignore]` 探针单测，发布构建不需要、也不该为它付出任何代价。
    #[cfg(test)]
    probe_rows: RecordProbe,
}

/// 启动时**延后几帧**再做设备枚举/目录探测（见 [`MainPage::flush_deferred_init`]）。
///
/// 为什么是 2 而不是 1：第 1 帧是"窗口刚出现"的那一帧，egui 还要在这一帧建字体图集、
/// 提交第一张画面；把枚举压到第 2 帧，能保证**首帧渲染**不被这些系统调用拖慢。
/// 2 帧在 60Hz 下约 33 ms，用户感知不到"列表晚了一点出来"。
const DEFER_INIT_FRAMES: u8 = 2;

/// 输出目录输入防抖：用户连续键入路径时只保留最后一次探测，
/// 避免每个字符都起一个网络/磁盘探测线程。
const OUTPUT_PROBE_DEBOUNCE: Duration = Duration::from_millis(350);

/// 启动看门狗：正常 0.5–6 s；超过 15 s 认为引擎卡住，UI 不再无限显示 Starting。
const START_WATCHDOG: Duration = Duration::from_secs(15);
/// 停止/导出看门狗：长录制 remux 可能较久，但不应无限；超过 2 min 交给用户决定。
const STOP_WATCHDOG: Duration = Duration::from_secs(120);
/// 退出时最多等录制收尾多久（超时就保留分片并退出）。
const EXIT_WATCHDOG: Duration = Duration::from_secs(15);

impl Default for MainPage {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MainPage {
    fn drop(&mut self) {
        if let Some(cancel) = &self.start_cancel {
            cancel.store(true, Ordering::Release);
        }
        if let Some(cancel) = &self.stop_cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}

impl MainPage {
    pub fn new() -> Self {
        Self {
            monitors: Vec::new(),
            geo: Vec::new(),
            monitors_err: None,
            monitors_loaded: false,
            last_monitor_refresh: Instant::now(),
            defer_ticks: DEFER_INIT_FRAMES,
            prewarm_at: Instant::now() + Duration::from_secs(8),
            prewarm_started: false,
            startup_log: None,
            region_hint: None,
            mic_devices: None,
            mic_err: None,
            picker: None,
            error: None,
            output_probe: None,
            output_probe_pending: None,
            output_probe_epoch: 0,
            output_probe_rx: None,
            output_probe_cancel: None,
            cache_dir: None,
            starting: false,
            start_rx: None,
            start_handle: None,
            start_cancel: None,
            stopping: false,
            stop_rx: None,
            stop_handle: None,
            stop_cancel: None,
            start_deadline: None,
            stop_deadline: None,
            watchdog_fired: false,
            pending_quit: false,
            stop_after_start: false,
            output_dir_note: None,
            first_frame: true,
            pending_pick: false,
            probe: LayoutProbe::default(),
            #[cfg(test)]
            probe_rows: RecordProbe::default(),
        }
    }

    /// 本帧的**逐部件高度**实测量（见 [`RecordProbe`]）；只在测试构建里存在。
    ///
    /// 探针单测 `tests::dump_record_page_heights` 用它打印"改前/改后"两列高度表。
    #[cfg(test)]
    pub fn record_probe(&self) -> RecordProbe {
        self.probe_rows
    }

    /// 本帧的布局测量（内容高度 / 视口高度 / 可用高度）。
    ///
    /// 常规接口而不是 `cfg(test)`：最小窗口尺寸的**整窗**验证在 `CapLite` 二进制里，
    /// 那里读不到库的 `cfg(test)` 符号（见 [`LayoutProbe`] 的说明）。
    pub fn layout_probe(&self) -> LayoutProbe {
        self.probe
    }

    /// 注入启动打点的日志落点（外壳在构造后立刻调用；不注入就完全不记）。
    ///
    /// 用函数指针而不是 `&StartupLog`：页面生命周期比日志长，且页面可能被放到后台线程
    /// （延后枚举）；函数指针是 `'static`，不牵扯借用。传 `None` 等价于"不记"。
    pub fn set_startup_log(&mut self, log: Option<fn(&str)>) {
        self.startup_log = log;
    }

    /// 注入外壳已经解析好的 Cache 根目录。
    ///
    /// 之后 `start_recording` 不再自己 `ConfigStore::detect(None)`：那会重复探测目录、
    /// 并且会忽略 `--config-dir`，导致流文件落到和配置不同的回退位置。
    pub fn set_cache_dir(&mut self, dir: PathBuf) {
        self.cache_dir = Some(dir);
    }

    /// **延后初始化**：显示器 / 麦克风枚举 + 输出目录可写性探测（真写一个临时文件）。
    ///
    /// 这些都在启动路径上、且都要等系统（DXGI/WASAPI/磁盘），实测合计几十到几百毫秒；
    /// 全部挪到首帧之后执行，窗口就会先出现。失败语义**与改动前完全一致**：
    /// 枚举失败仍然只记 `monitors_err` / `mic_err`（界面用琥珀小字说明），
    /// 输出目录探测失败仍然只给一条 warn 提示条，**绝不 panic、绝不假装有数据**。
    ///
    /// 只执行一次（`defer_ticks` 归零后由 `flush_deferred_init` 一次性做完）。
    fn flush_deferred_init(&mut self, cfg: &Config, t: &dyn Fn(&str) -> String) {
        let log = self.startup_log;
        let t0 = Instant::now();
        self.refresh_monitors();
        let monitors_ms = t0.elapsed().as_millis();
        // 输出目录探测：与 `output_section` 里那套逻辑同一份判断（缓存键置空 → 这次真探测一遍）
        self.output_probe = None;
        let probed = self.probe_output_dir(cfg, t, None);
        // 诊断串**只进日志**（`cap.log`），所以按日志惯例写 ASCII：
        // 它不上界面，也就不该被当成"界面文案"（`ui::i18n_guard` 会拦硬编码中文）。
        let extra = format!(
            "[cap] monitors {} / enum {}ms / output-dir probe {}",
            self.monitors.len(),
            monitors_ms,
            if probed {
                "ran"
            } else {
                "skipped (cache valid)"
            }
        );
        startup_marks::mark_dur_extra_log(log, "deferred_enum_done", "app", t0.elapsed(), extra);
        // 麦克风：只有"这次录制真的要麦克风"或用户展开下拉时才枚举（保持改动前语义）。
        // 具体由 `mic_combo` 那一帧触发的 `refresh_mic_devices` 完成，这里不主动枚举。
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: MainCtx<'_>) -> MainOutcome {
        let mut out = MainOutcome::default();
        let i18n = cx.i18n;
        // 后台开始/停止的结果先收（绝不阻塞）；这样下面的状态卡片/结果卡片
        // 拿到的是本帧最新状态。
        self.poll_recording_jobs(cx.cfg, cx.recorder, cx.result, i18n);
        // 只捕获 &I18n（Copy），这样下面借用 cfg 时不会打架
        let t = |k: &str| i18n.t(k).to_string();
        // 目录可写性探测在后台线程里跑；这里只收结果，绝不在绘制路径上碰网络盘。
        self.poll_output_probe(ui.ctx(), &t);

        // ---- 逐部件高度探针：每帧清零后重记（见 [`RecordProbe`]）----
        // 页面级几何（可用宽度/高度、可用区顶边）先落下来，卡片几何在下面逐个填。
        // 记在一份**局部副本**里（`RecordProbe: Copy`）而不是直接写 `self.probe_rows`：
        // 下面还要调 `self.status_card(...)` / `self.capture_section(...)`（都要 `&mut self`），
        // 持着 `&mut self.probe_rows` 会与它们打架。帧末一次性写回。
        let mut probe = RecordProbe {
            page_avail_w: ui.max_rect().width(),
            page_avail_h: ui.max_rect().height(),
            page_top: ui.max_rect().top(),
            card_gap_h: theme::GAP_CARD_TIGHT,
            card_pad: theme::PAD_CARD_TIGHT,
            ..RecordProbe::default()
        };

        // ---- 延后初始化：窗口先出来，枚举/探测在首帧之后补齐（见 `flush_deferred_init`）----
        // `defer_ticks` 每帧减一；减到 0 的那一帧（= 第 2 帧）一次性做完显示器枚举与目录探测。
        // 期间界面显示"加载中…"、不显示任何设备数据，也**不会**因为没有数据而报错。
        if self.defer_ticks > 0 {
            self.defer_ticks -= 1;
            if self.defer_ticks == 0 {
                self.flush_deferred_init(cx.cfg, &t);
            }
        }
        if self.first_frame {
            self.first_frame = false;
            // 默认配置里 region 是 1280x720 的占位值（`Region::default`）：
            // 换成主显示器真实矩形，"区域"模式的兜底输入框一开始就是有意义的数字
            if cx.cfg.region == Region::default() {
                if let Some(m) = self.primary_geo() {
                    cx.cfg.region = m.region();
                    out.cfg_changed = true;
                }
            }
        }

        // 主操作按钮**挪进状态卡片**（用户要求："将录制按钮放到时间码右侧"）：
        // 所以这里**不再有底部固定操作条**（那块 `Panel::bottom("cap_record_bar")` 已删除，
        // 也就不会有"卡片下方那条空框"）。按钮与状态点/时间码同一行、左对齐。
        // 错误/警告交给外壳的底部状态栏
        out.error = self.error.clone();
        out.warning = self
            .audio_warning_text(cx.recorder, i18n)
            .or_else(|| self.output_dir_note.clone().filter(|n| !n.is_empty()))
            .or_else(|| self.region_hint.clone().filter(|n| !n.is_empty()));

        let mut record_action: Option<RecordAction> = None;
        // **纵向滚动条永远不显示**（用户要求："取消上下滚动条"）：
        // * `AlwaysHidden` —— 滚动条一个像素都不画；
        // * `auto_shrink([false, false])` —— 滚动区占满可用空间，内容自顶部开始排。
        //
        // 为什么不像剪辑页那样彻底删掉滚动区：这一页的高度**不是常量** ——
        // 录制中会多出「实时统计」网格、录完还会多出结果卡片，
        // 两张卡片出现时内容可能高过最小窗口。那时**滚轮仍能看到全部内容**，
        // 而不是被裁掉；而滚动条始终不可见，所以"没有上下滚动条"这条依然成立。
        // 静止态（就绪 + 无结果，也就是 `min_inner_size` 的推算口径）**根本不需要滚动**：
        // 单测 `headless_record_required_height_fits_the_min_window` 断言 `content_h <= viewport_h`。
        let _scroll = egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 滚动区**内容**的顶边（页面可用区顶 → 内容顶 = 首留白，见 `lead_blank_h`）
                #[cfg(test)]
                {
                    probe.content_top = ui.min_rect().top();
                }
                // 顶部**状态卡片**：状态点 + 状态文字 + 大号等宽时长 + 主操作按钮（同一行）
                // + 按钮左侧的分辨率信息（P38 从采集卡挪来）
                record_action =
                    self.status_card(ui, cx.recorder, cx.result, cx.cfg, i18n, &t, &mut probe);
                // 卡片间距也用**紧凑**那一档（`GAP_CARD_TIGHT`）：这一页的高度直接决定
                // 程序最小窗口高度（见 `ui::layout`），4 张卡片之间共省 12px。
                ui.add_space(theme::GAP_CARD_TIGHT);
                // ---- 录制中：参数控件全部置灰 ----
                // 本次录制的参数在 `Recorder::start` 里就**快照**好了：帧率、导出目录等都是那时的
                // 局部变量，编码器/封装器也已经建好并持有这些值，
                // 中途改配置不会影响正在进行的这一次；所以录制期间直接禁用参数控件，
                // 并给一行说明，避免用户以为"改了没生效是 bug"。
                // 停止按钮在**状态卡片**里（不在这个 `add_enabled_ui` 作用域内），所以始终可用。
                let recording = cx.recorder.is_some();
                if recording {
                    ui.label(
                        egui::RichText::new(t("recording_locked"))
                            .size(theme::FS_SMALL)
                            .color(theme::warn_fg()),
                    );
                    ui.add_space(theme::GAP_ROW_TIGHT);
                }
                ui.add_enabled_ui(!recording, |ui| {
                    // `card` 只在测试构建里被读（逐部件高度探针）；发布构建编译时"未使用"是预期的
                    #[cfg_attr(not(test), allow(unused_variables))]
                    let (_, card) = theme::card_tight(ui, |ui| {
                        out.cfg_changed |= self.capture_section(ui, cx.cfg, &t, &mut probe);
                    });
                    #[cfg(test)]
                    {
                        probe.capture_card_h = card.outer_h;
                        probe.capture_card_inner_h = card.inner_h;
                    }
                    ui.add_space(theme::GAP_CARD_TIGHT);
                    #[cfg_attr(not(test), allow(unused_variables))]
                    let (_, card) = theme::card_tight(ui, |ui| {
                        out.cfg_changed |= self.content_section(ui, cx.cfg, &t, &mut probe);
                    });
                    #[cfg(test)]
                    {
                        probe.content_card_h = card.outer_h;
                        probe.content_card_inner_h = card.inner_h;
                    }
                    ui.add_space(theme::GAP_CARD_TIGHT);
                    #[cfg_attr(not(test), allow(unused_variables))]
                    let (_, card) = theme::card_tight(ui, |ui| {
                        out.cfg_changed |= self.output_section(ui, cx.cfg, &t, &mut probe);
                    });
                    #[cfg(test)]
                    {
                        probe.output_card_h = card.outer_h;
                        probe.output_card_inner_h = card.inner_h;
                    }
                });
                ui.add_space(theme::GAP_CARD_TIGHT);
                if let Some(path) = self.result_card(ui, cx.result, &t) {
                    out.open_in_editor = Some(path.0);
                    out.open_in_editor_fallback = path.1;
                }
                // 滚动区**内容**的底边：不在这里量 —— `auto_shrink([false, false])` 下滚动区的
                // `ui` 按**视口**高度扩张，且 `add_space` 会把 `min_rect` 重复撑大。
                // 真口径由各张卡片的实测高度 + 间距 + 恒定差额在帧末拼出（见 `ui()` 的帧末段）。
            });
        // 本帧布局测量（最小窗口高度 `ui::layout::MIN_INNER_H` 的推导数据源）
        self.probe = LayoutProbe {
            content_h: _scroll.content_size.y,
            viewport_h: _scroll.inner_rect.height(),
            page_avail_h: ui.max_rect().height(),
        };

        // ---- 逐部件高度探针：合计 + 首尾留白，然后把局部副本写回页面 ----
        #[cfg(test)]
        {
            probe.scroll_content_h = _scroll.content_size.y;
            probe.scroll_viewport_h = _scroll.inner_rect.height();
            probe.scroll_min_rect_bottom = probe.scroll_content_h;
            // 内容底 = 四张卡片 + 三个卡片间距 + 恒定差额（滚动区把行距重复计了一次）。
            // 三个间距出现在 status→capture、capture→content、content→output 之间；
            // 恒定额 = 滚动区口径里被重复计入的**卡片内部**行距（status 卡内录制中的
            // 统计行留白、content 卡内「录制内容」行与「音源」行之间的行距）——
            // 它是**派生量**，随卡内结构变化（P38 合并行后变大），由探针打印出来核对。
            probe.nested_layout_residual_h = (probe.scroll_content_h
                - (probe.status_card_h
                    + probe.capture_card_h
                    + probe.content_card_h
                    + probe.output_card_h
                    + probe.card_gap_h * 3.0))
                .max(0.0);
            probe.content_bottom = probe.content_top
                + probe.status_card_h
                + probe.card_gap_h
                + probe.capture_card_h
                + probe.card_gap_h
                + probe.content_card_h
                + probe.card_gap_h
                + probe.output_card_h
                + probe.nested_layout_residual_h;
            probe.total_content_h = (probe.content_bottom - probe.content_top).max(0.0);
            probe.lead_blank_h = (probe.content_top - probe.page_top).max(0.0);
            let page_bottom = probe.page_top + probe.page_avail_h;
            probe.tail_blank_h = Some((page_bottom - probe.content_bottom).max(0.0));
            self.probe_rows = probe;
        }

        // 状态卡片里的主操作按钮：在这里统一执行（`ui()` 借得到 `&mut recorder/cfg`）
        match record_action {
            Some(RecordAction::Start) => {
                let _ = self.start_recording(cx.cfg, cx.recorder, cx.result, i18n);
            }
            Some(RecordAction::Stop) => {
                let _ = self.stop_recording(cx.cfg, cx.recorder, cx.result, i18n);
            }
            None => {}
        }

        // 框选遮罩：独立窗口，必须在主窗口的 ui 里每帧调用一次
        if let Some(mut pk) = self.picker.take() {
            match run_picker(ui.ctx(), &mut pk, i18n) {
                PickerEnd::Keep => self.picker = Some(pk),
                PickerEnd::Cancelled => {}
                PickerEnd::Picked(region) => {
                    self.apply_picked_region(cx.cfg, region);
                    out.cfg_changed = true;
                }
            }
        } else if self.pending_pick {
            // 用户刚点选"区域录制"（或点了"重新框选"）：**立刻**进入框选，不需要再点一次按钮
            self.open_picker(cx.cfg, i18n, &t);
        }
        self.pending_pick = false;

        // ---- FFmpeg 运行期 DLL 预加载：**显示器与音频枚举完成之后**（用户要求）----
        //
        // 时机：本帧稍前，`flush_deferred_init` 已经枚举完显示器并探测完输出目录
        // （`defer_ticks` 归零的那一帧）；音频设备（麦克风）也在**同一帧**的
        // `content_section → mic_combo` 里枚举完（那里的 `defer_ticks == 0` 判断正是同一条）。
        // 所以"两处都完成"在本帧末尾成立，这里触发一次即可。
        //
        // 做什么：后台线程里把 5 个 DLL 主动 `LoadLibraryW` 一遍（见
        // `startup::preload_ffmpeg_dlls_once`）。这样"按下开始录制"不再有映射 110 MB 的等待，
        // 而"启动瞬间不映射"（延迟加载的红利）也没丢；不阻塞 UI 帧，也不阻塞枚举本身。
        // 每帧调用是安全的：内部一次性标志，只有第一次真的起线程；失败只进 `cap.log`。
        if self.defer_ticks == 0 && self.audio_enum_attempted() {
            startup_marks::preload_ffmpeg_dlls_once(self.startup_log);
            // ---- OpenH264 编码器预热：枚举完成后再等一个空闲窗口 ----
            //
            // 为什么在这里：`Recorder::start` 里 `Capture::open` + `OpenH264 初始化` 是
            // "按下开始录制后一帧画面都没有"的那几秒，实测编码器初始化占 0.46~6.0 s。
            // 启动 8 s 后仍在录制页、且用户没有开始录制，再在**后台线程**按当前配置
            // 建好实例。这样只剪辑/查看的用户不必为编码器常驻约 25 MB 工作集；
            // 真按下开始时仍按缓存键取用，键不匹配/没有/失败就照旧同步初始化。
            if cx.recorder.is_none() && !self.prewarm_started && Instant::now() >= self.prewarm_at {
                startup_marks::prewarm_encoder_once(cx.cfg, self.startup_log);
                self.prewarm_started = true;
            } else if !self.prewarm_started {
                // 没有输入时 egui 不出帧：给预热时刻排一次唤醒。
                ui.ctx().request_repaint_after(
                    self.prewarm_at.saturating_duration_since(Instant::now()),
                );
            }
        }
        // ---- 配置改了（分辨率/帧率/码率/区域…）→ 缓存键变了就重新后台预热 ----
        // 键没变时 `prewarm_encoder_once` 是空操作（一次互斥锁 + 一次 `GetMonitorInfoW`）。
        // 录制中不预热：本次录制的参数在 `Recorder::start` 里就快照好了，再建实例没有意义。
        if out.cfg_changed && self.defer_ticks == 0 && self.prewarm_started && cx.recorder.is_none()
        {
            startup_marks::prewarm_encoder_once(cx.cfg, self.startup_log);
        }
        out
    }

    /// 音频设备是否已经**尝试过**枚举（成功、为空、失败都算"尝试过"，见 [`Self::mic_combo`]）。
    ///
    /// 为什么不是"枚举成功"：没有麦克风的机器上 `mic_devices` 是 `Some(空列表)`、设备服务
    /// 不可用时是 `mic_err = Some(..)`；要求"成功"会让预加载在这两种机器上永远不触发。
    fn audio_enum_attempted(&self) -> bool {
        self.mic_devices.is_some() || self.mic_err.is_some()
    }

    // -----------------------------------------------------------------------
    // 采集范围
    // -----------------------------------------------------------------------

    fn capture_section(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut Config,
        t: &dyn Fn(&str) -> String,
        probe: &mut RecordProbe,
    ) -> bool {
        let _ = probe;
        let mut changed = false;
        // 卡片标题"采集"已按用户要求删除：卡片直接以「显示器 + 录制范围」行开始
        // 用户点选"区域录制"的那一刻就要进入框选（不需要再点一次"拾取"）。
        // 这里只记一个标记，真正的遮罩在 `ui()` 里本帧稍后打开。
        let mut enter_region = false;
        let mut reselect = false;
        // ---- 显示器一行 + 右侧「录制范围」（P38，用户附图口径）----
        //
        // 原来是**两行**（「录制范围」单选行 / 「显示器 + 信息」行），用户要求：
        // ① 分辨率信息（`全屏录制：1920x1080` 等）挪到状态卡片「开始录制」**左侧**；
        // ② 「录制范围」单选 + 「重新框选」并到**显示器下拉右侧**，同一行。
        // 于是采集卡从两行压成一行（−44px），录制页整体再省一截。
        //
        // 与 `theme::field_row` 同一套排法（标签固定宽 + 右侧控件左对齐），
        // 这里**直接用 `ui.horizontal`**，因为逐部件高度探针要那一行的实测矩形
        //（`field_row` 只把闭包结果返回出来，拿不到行矩形）。
        //
        // **不换行的宽度账**（`MIN_INNER_W` = 760，卡片内容区右缘 751）：
        // 显示器标签 72 + 下拉 200 + 间距 6 + 录制范围标签（自然宽 ~75）+ 两个单选
        // + 4 + 重新框选 ≈ 650（中）/ ~730（英，"Capture Area"/"Custom Region"），
        // 上限 759 —— 有余量；真账由 `record_page_capture_and_audio_rows_fit`
        // 两种语言各断言一次（拿最后一个控件右缘量的，实测 zh 607 / en 726）。
        const MONITOR_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);
        if self.monitors_loaded && self.last_monitor_refresh.elapsed() >= MONITOR_REFRESH {
            self.last_monitor_refresh = Instant::now();
            self.refresh_monitors();
        }
        let mut rank = rank_of_cfg(cfg.monitor_index);
        if rank >= self.geo.len() {
            rank = if self.geo.is_empty() {
                0
            } else {
                self.geo.len() - 1
            };
        }
        let mut monitor_picked = false;
        // 下拉是否**本帧真的展开**了（`show_ui` 内层闭包只有在展开时才执行）
        let mut combo_open = false;
        // **还没枚举过**（启动后前两帧）：明确告诉用户"正在加载"，而不是让空下拉看起来像"没有显示器"。
        // 这也是"不假装有数据"的一半：没有数据时就说没有数据的原因（加载中）。
        let loading = self.defer_ticks > 0 && self.monitors.is_empty();
        // 采集卡唯一一行：显示器（左）+ 录制范围（右）
        let mut row_last_x = 0.0f32; // 探针：本行最后一个控件的右缘（不换行/不裁切的凭证）
        let capture_row = ui.horizontal(|ui| {
            // 标签列 72（不是 `FIELD_LABEL_W` 的 88）：「显示器 / Monitor」两个词都短，
            // 88px 纯属浪费——这一行右边要排「录制范围」整组控件，宽度账见函数头注释。
            ui.add_sized(
                Vec2::new(theme::FIELD_LABEL_W_MONITOR, theme::H_CONTROL),
                egui::Label::new(RichText::new(t("monitor")).color(theme::muted()))
                    .halign(egui::Align::LEFT),
            );
            let selected = if loading {
                t("loading_devices")
            } else {
                self.monitors
                    .get(rank)
                    .map(|m| m.label_short())
                    .unwrap_or_else(|| t("no_monitor"))
            };
            egui::ComboBox::from_id_salt("cap_monitor")
                .width(200.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    // 内层闭包**只在展开时**执行 → 这就是"下拉被打开"的判据。
                    // 这里只登记标记，真正的枚举在闭包外做（闭包里还借着 `self.monitors`）。
                    combo_open = true;
                    if loading {
                        // 加载中：下拉里也不放任何"假设备"，只放一行说明
                        ui.label(RichText::new(t("loading_devices")).color(theme::muted()));
                    }
                    for (i, m) in self.monitors.iter().enumerate() {
                        if ui.selectable_value(&mut rank, i, m.label()).clicked() {
                            monitor_picked = true;
                        }
                    }
                });
            ui.add_space(6.0);
            // 「录制范围」标签：**不用** `FIELD_LABEL_W` 固定宽（88）—— 这一行右边已经排了
            // 两个单选 + 按钮，英文 "Capture Area" 塞 88px 纯属浪费，还会把整行顶宽。
            // 自然宽度 + 次要文字色，与显示器/录制内容行的标签**同色同字号**，只是不对齐成列
            // （对齐成列的代价是宽度，P38 选了宽度）。
            ui.label(RichText::new(t("record_mode")).color(theme::muted()));
            changed |= ui
                .radio_value(
                    &mut cfg.capture_mode,
                    CaptureMode::FullScreen,
                    t("mode_fullscreen"),
                )
                .changed();
            let region_clicked = ui
                .radio_value(&mut cfg.capture_mode, CaptureMode::Region, t("mode_region"))
                .changed();
            changed |= region_clicked;
            enter_region = region_clicked && cfg.capture_mode == CaptureMode::Region;
            // 「重新框选」紧挨"区域录制"右边（用户要求）。
            // 全屏模式下**置灰**（不是隐藏）：按钮位置稳定，用户也一眼知道"切到区域才能重选"。
            let in_region = cfg.capture_mode == CaptureMode::Region;
            ui.add_space(4.0);
            let btn = ui
                .add_enabled(in_region, egui::Button::new(t("pick_region_again")))
                .on_disabled_hover_text(t("mode_region"));
            row_last_x = btn.rect.right();
            reselect = btn.clicked();
        });
        #[cfg(test)]
        {
            probe.capture_row_h = capture_row.response.rect.height();
            probe.capture_row_last_x = row_last_x;
            probe.capture_gap_h = 0.0; // P38 起采集卡只有一行，卡内没有第二行行距
        }
        #[cfg(not(test))]
        let _ = (capture_row, row_last_x);
        if reselect {
            self.pending_pick = true;
        }
        // **下拉被打开 = 立刻真枚举一次**（用户拔插显示器后打开就能看到最新列表）。
        // 仍然**不每帧枚举**：只在"本帧展开"且距上次枚举 ≥ `MONITOR_REFRESH` 时才做，
        // 展开着不放手也不会反复枚举。失败时 `refresh_monitors` 只记 warning、不清列表、不 panic。
        if combo_open && self.last_monitor_refresh.elapsed() >= MONITOR_REFRESH {
            self.last_monitor_refresh = Instant::now();
            self.refresh_monitors();
        }
        if !self.geo.is_empty() && rank != rank_of_cfg(cfg.monitor_index) {
            let old = self.selected_geo(cfg);
            cfg.monitor_index = cfg_of_rank(rank);
            let new = self.selected_geo(cfg);
            changed = true;
            // **区域模式下换屏**：旧选区可能不在新显示器上 → 夹紧；夹不下就清空并提示重选。
            // 绝不出现"切了屏但框选仍开在旧屏上/坐标按旧屏换算"的情况。
            if cfg.capture_mode == CaptureMode::Region {
                if let (Some(old), Some(new)) = (old, new) {
                    match region_after_monitor_switch(cfg.region, &old, &new) {
                        Some(r) => cfg.region = r,
                        None => {
                            cfg.region.width = 0;
                            cfg.region.height = 0;
                        }
                    }
                    self.region_hint = Some(t("region_reselect_hint"));
                }
            }
        }
        if monitor_picked {
            // 点选了一项就等于"用户刚改过显示器"：立刻再枚举一次，保证列表与选择一致
            self.last_monitor_refresh = Instant::now();
            self.refresh_monitors();
        }
        if let Some(e) = &self.monitors_err {
            ui.colored_label(
                // P40：显示器枚举失败是"警告"语义，颜色跟当前主题走
                crate::ui::theme::warn_fg(),
                format!("{} {e}", t("no_monitor")),
            );
        }
        // 用户明确从别的模式切到区域：本帧就弹框选
        if enter_region {
            self.pending_pick = true;
        }
        changed
    }

    /// 遮罩里框选完成后写回配置。
    /// 显示器序号按"选区落在哪块屏"重新对齐：遮罩可能被系统放到别的屏幕上，
    /// 而对齐后至少保证 `region` 和 `monitor_index` 指的是同一块屏（否则录制时会报"选区在显示器之外"）。
    fn apply_picked_region(&mut self, cfg: &mut Config, region: Region) {
        cfg.region = region;
        cfg.capture_mode = CaptureMode::Region;
        let (cx, cy) = (
            region.x as i64 + region.width as i64 / 2,
            region.y as i64 + region.height as i64 / 2,
        );
        if let Some((rank, _)) = self.geo.iter().enumerate().find(|(_, m)| {
            let (l, t, r, b) = m.bounds();
            cx >= l && cx < r && cy >= t && cy < b
        }) {
            cfg.monitor_index = cfg_of_rank(rank);
        }
    }

    // -----------------------------------------------------------------------
    // 内容与音源
    // -----------------------------------------------------------------------

    fn content_section(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut Config,
        t: &dyn Fn(&str) -> String,
        probe: &mut RecordProbe,
    ) -> bool {
        let _ = probe;
        let mut changed = false;
        // 卡片标题"录制内容"与下面第一行的行标签"录制内容"完全重复 →
        // 按用户要求**只保留行标签**（与"采集"标题同样的处理）
        //
        // **紧凑化（竖向）**：这一页的高度直接决定程序最小窗口高度（见 `ui::layout`），
        // 所以「录制鼠标指针」不再独占一行（约省 38px），而是并到「录制内容」那一行的右边
        // —— 它本来就是"视频内容要不要画指针"，与内容模式是同一件事的两面；
        // 只录声音（`video_on == false`）时它置灰，但**位置不变**（不会左右跳）。
        let video_on = cfg.content_mode.has_video();
        // 同「录制范围」行：直接用 `ui.horizontal` 以拿到行矩形（逐部件高度探针要它）
        let rec_mode_row = ui.horizontal(|ui| {
            ui.add_sized(
                Vec2::new(theme::FIELD_LABEL_W, theme::H_CONTROL),
                egui::Label::new(RichText::new(t("rec_mode")).color(theme::muted()))
                    .halign(egui::Align::LEFT),
            );
            changed |= ui
                .radio_value(
                    &mut cfg.content_mode,
                    ContentMode::VideoAndAudio,
                    t("rmode_both"),
                )
                .changed();
            changed |= ui
                .radio_value(
                    &mut cfg.content_mode,
                    ContentMode::VideoOnly,
                    t("rmode_video"),
                )
                .changed();
            changed |= ui
                .radio_value(
                    &mut cfg.content_mode,
                    ContentMode::AudioOnly,
                    t("rmode_audio"),
                )
                .changed();
            // 与内容模式同排：只录声音时置灰（没有画面可画指针），但**位置稳定**
            ui.add_space(14.0);
            ui.add_enabled_ui(video_on, |ui| {
                changed |= ui
                    .checkbox(&mut cfg.draw_cursor, t("draw_cursor"))
                    .changed();
            });
        });
        #[cfg(test)]
        {
            probe.rec_mode_row_h = rec_mode_row.response.rect.height();
        }
        #[cfg(not(test))]
        let _ = rec_mode_row;

        let audio_on = cfg.content_mode.has_audio();
        ui.add_space(theme::GAP_ROW_TIGHT);
        #[cfg(test)]
        {
            probe.content_gap_h = theme::GAP_ROW_TIGHT;
        }
        #[cfg(test)]
        let mut audio_label_h = 0.0f32;
        #[cfg(test)]
        let mut audio_row_h = 0.0f32;
        #[cfg(test)]
        let mut audio_row_last_x = 0.0f32;
        ui.add_enabled_ui(audio_on, |ui| {
            // ---- 「音源（可多选）」标签 + 复选框同一行（P38，用户附图口径）----
            //
            // 原来是标签独占一行、复选框另起一行。用户要求把系统声音/麦克风等
            // 选项移到「音源（可多选）」**右侧**，于是两行并一行（内容卡再省 34px）。
            //
            // 标签**不并进** `field_row` 的 88px 标签框：英文语言包里它是
            // "Audio sources (multi-select)"，塞进 88px 会被裁掉/换行，
            // 而"文字不被裁切"是硬要求（最小尺寸下的正确性）。
            // 两个复选框并排：省纵向空间，也让"可多选"一眼看出来。
            #[cfg_attr(not(test), allow(unused_variables))]
            let audio_row = ui.horizontal(|ui| {
                #[cfg_attr(not(test), allow(unused_variables))]
                let label_resp = ui.label(
                    egui::RichText::new(t("audio_setting"))
                        .size(theme::FS_SMALL)
                        .color(theme::muted()),
                );
                #[cfg(test)]
                {
                    audio_label_h = label_resp.rect.height();
                }
                ui.add_space(10.0);
                changed |= ui
                    .checkbox(&mut cfg.audio_system, t("system_audio"))
                    .changed();
                ui.add_space(18.0);
                changed |= ui.checkbox(&mut cfg.audio_mic, t("mic")).changed();
                ui.add_enabled_ui(cfg.audio_mic, |ui| {
                    changed |= self.mic_combo(ui, cfg, t);
                });
                // 本行最后一个部件的右缘（下拉 + 刷新按钮都由 `mic_device_combo` 画出来，
                // 返回里没有矩形，所以用水平布局的游标左缘来近似最后一个部件的右缘；
                // 探针 `record_page_capture_and_audio_rows_fit` 用它断"不换行/不裁切"）。
                #[cfg(test)]
                {
                    audio_row_last_x = ui.cursor().left();
                }
            });
            #[cfg(test)]
            {
                audio_row_h = audio_row.response.rect.height();
            }
            #[cfg(not(test))]
            let _ = audio_row;
            if !cfg.audio_system && !cfg.audio_mic {
                ui.add_space(4.0);
                theme::banner(ui, theme::Banner::Warn, &t("audio_required"));
            }
        });
        #[cfg(test)]
        {
            probe.audio_label_h = audio_label_h;
            probe.audio_row_h = audio_row_h;
            probe.audio_row_last_x = audio_row_last_x;
            probe.audio_label_gap_h = 10.0;
        }
        changed
    }

    /// 麦克风下拉：None = 系统默认设备
    fn mic_combo(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut Config,
        t: &dyn Fn(&str) -> String,
    ) -> bool {
        if self.mic_devices.is_none() && self.mic_err.is_none() && self.defer_ticks == 0 {
            // 首次枚举麦克风（WASAPI）：**同样延后到首帧之后**（`defer_ticks` 归零才做），
            // 并记一条阶段打点（枚举耗时是"启动路径上要不要推迟它"的依据）。
            let log = self.startup_log;
            let t0 = Instant::now();
            self.refresh_mic_devices();
            // 同上：只进日志的诊断串，按日志惯例写 ASCII
            let extra = format!(
                "[cap] mics {}",
                self.mic_devices.as_ref().map(Vec::len).unwrap_or(0)
            );
            startup_marks::mark_dur_extra_log(log, "enum_audio_done", "app", t0.elapsed(), extra);
        }
        // 220：音源行左边有标签 + 两个复选框（P38 并行），窄一档才装得下 760 最小宽度
        let (picked, refresh) = mic_device_combo(
            ui,
            self.mic_devices.as_deref(),
            cfg.mic_device_id.as_deref(),
            220.0,
            t,
        );
        let mut changed = false;
        if let Some(p) = picked {
            cfg.mic_device_id = p;
            changed = true;
        }
        if refresh {
            self.mic_devices = None;
            self.mic_err = None;
            self.refresh_mic_devices();
        }
        changed
    }

    // -----------------------------------------------------------------------
    // 输出位置
    // -----------------------------------------------------------------------

    fn output_section(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut Config,
        t: &dyn Fn(&str) -> String,
        probe: &mut RecordProbe,
    ) -> bool {
        let _ = probe;
        let mut changed = false;
        // 卡片标题"输出"也删掉：卡片里就是「保存位置 + 路径 + 选择目录」这一行，自明。
        //（与"采集/录制内容"两处保持同一种处理：卡片内不再有与行标签重复的标题）
        let out_row = ui.horizontal(|ui| {
            ui.add_sized(
                [theme::FIELD_LABEL_W, theme::H_CONTROL],
                egui::Label::new(
                    egui::RichText::new(t("storage_dir"))
                        .size(theme::FS_BODY)
                        .color(theme::muted()),
                )
                .halign(egui::Align::LEFT),
            );
            // 路径（等宽、按剩余宽度省略中段）+ 两个小按钮：先算好按钮要占多宽，
            // 剩下的才给路径，避免按钮被挤出右边界。
            let buttons_w = 214.0;
            let path_w = (ui.available_width() - buttons_w).max(140.0);
            let max_chars = ((path_w / 7.2) as usize).max(16);
            ui.add_sized(
                [path_w, theme::H_CONTROL],
                egui::Label::new(theme::mono_muted(&elide(&cfg.output_dir, max_chars)))
                    .halign(egui::Align::LEFT)
                    .truncate(),
            );
            if theme::ghost_button(ui, &t("btn_pick_output")).clicked() {
                // 起始目录用绝对路径：rfd 对相对路径的"当前目录"理解可能和 exe 不同
                let start = crate::config::resolve_against_exe_dir(&cfg.output_dir)
                    .unwrap_or_else(|_| std::env::temp_dir());
                match pick_folder(&start) {
                    Ok(Some(dir)) => {
                        cfg.output_dir = dir.to_string_lossy().to_string();
                        // 用户换了目录，之前那条"已改用 X"的提示就不成立了
                        self.output_dir_note = None;
                        changed = true;
                    }
                    Ok(None) => {}
                    Err(e) => self.error = Some(e),
                }
            }
        });
        #[cfg(test)]
        {
            probe.output_row_h = out_row.response.rect.height();
        }
        #[cfg(not(test))]
        let _ = out_row;
        // 这里只做"提示"：真正的回退在启动时由 plan_output_dir 决定，界面如实显示结论，
        // 用户就能一眼看到"我以为在用的目录"和"实际写入的目录"是否一致。
        // 探测要真写一个临时文件，所以按目录值缓存结果，避免每帧都打一次磁盘。
        self.probe_output_dir(cfg, t, Some(ui.ctx()));
        if let Some(note) = self.output_dir_note.as_ref().filter(|n| !n.is_empty()) {
            ui.add_space(theme::GAP_ROW_TIGHT);
            theme::banner(ui, theme::Banner::Warn, note);
        }
        changed
    }

    /// 输出目录可写性探测（**真写一个临时文件**）→ 结果缓存进 `output_dir_note`。
    ///
    /// 抽成独立方法是为了让"延后初始化"（[`Self::flush_deferred_init`]）能在首帧之后
    /// 也调用同一份判断：启动时探测被推迟，但**判断逻辑只有一处**，不会出现两套结论。
    ///
    /// 按 `cfg.output_dir` 的值缓存：值没变就不重复探测（每帧都真写一次文件是不可接受的）。
    /// 返回本次是否真的探测过。
    fn probe_output_dir(
        &mut self,
        cfg: &Config,
        t: &dyn Fn(&str) -> String,
        ctx: Option<&egui::Context>,
    ) -> bool {
        let key = cfg.output_dir.trim().to_string();
        if self.output_probe.as_deref() == Some(key.as_str()) {
            self.output_probe_pending = None;
            return false;
        }
        let now = Instant::now();
        if let Some((pending, due)) = &self.output_probe_pending {
            if pending == &key && now < *due {
                if let Some(ctx) = ctx {
                    ctx.request_repaint_after(due.saturating_duration_since(now));
                }
                return false;
            }
            if pending != &key {
                // 用户还在连续输入：作废旧请求，等输入停下来再做一次。
                if let Some(cancel) = self.output_probe_cancel.take() {
                    cancel.store(true, Ordering::Release);
                }
                self.output_probe_rx = None;
                self.output_probe_epoch = self.output_probe_epoch.wrapping_add(1);
                self.output_probe_pending = Some((key, now + OUTPUT_PROBE_DEBOUNCE));
                self.output_dir_note = None;
                if let Some(ctx) = ctx {
                    ctx.request_repaint_after(OUTPUT_PROBE_DEBOUNCE);
                }
                return false;
            }
        } else {
            // 新目录：先作废旧任务，再进入防抖窗口。
            if let Some(cancel) = self.output_probe_cancel.take() {
                cancel.store(true, Ordering::Release);
            }
            self.output_probe = None;
            self.output_probe_rx = None;
            self.output_probe_epoch = self.output_probe_epoch.wrapping_add(1);
            self.output_probe_pending = Some((key, now + OUTPUT_PROBE_DEBOUNCE));
            self.output_dir_note = None;
            if let Some(ctx) = ctx {
                ctx.request_repaint_after(OUTPUT_PROBE_DEBOUNCE);
            }
            return false;
        }

        self.output_probe = Some(key.clone());
        self.output_probe_pending = None;
        self.output_dir_note = None;
        self.output_probe_epoch = self.output_probe_epoch.wrapping_add(1);
        let epoch = self.output_probe_epoch;
        let configured = key;
        let storage = t("storage_dir");
        let fallback = t("warn_fallback_to");
        let no_dir = t("err_no_output_dir");
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        self.output_probe_cancel = Some(cancel);
        let spawned = std::thread::Builder::new()
            .name("output-dir-probe".into())
            .spawn(move || {
                if cancel_thread.load(Ordering::Relaxed) {
                    return;
                }
                let note = match crate::config::resolve_against_exe_dir(&configured) {
                    Err(e) => format!("{storage}: {e}"),
                    Ok(dir) => {
                        // 每个候选目录都带 1.5 s 超时；线程本身也跑在 UI 之外，
                        // 即使驱动层卡住也不会冻结窗口。
                        let plan = crate::config::plan_output_dir(
                            &dir.to_string_lossy(),
                            &crate::config::probe_dir_writable_timeout,
                        );
                        if let Some(chosen) = plan.chosen_path() {
                            if plan.fell_back() {
                                format!("{fallback} {}", chosen.display())
                            } else {
                                String::new()
                            }
                        } else {
                            plan.error.unwrap_or(no_dir)
                        }
                    }
                };
                if !cancel_thread.load(Ordering::Relaxed) {
                    let _ = tx.send((epoch, note));
                }
            });
        match spawned {
            Ok(_) => self.output_probe_rx = Some(rx),
            Err(e) => {
                self.output_probe_cancel = None;
                self.output_dir_note = Some(format!("{}: {e}", t("storage_dir")));
            }
        }
        true
    }

    /// 收后台目录探测结果（非阻塞）。旧代际结果直接丢弃，避免快速改目录时
    /// 一个慢返回把新目录的提示盖掉。
    fn poll_output_probe(&mut self, ctx: &egui::Context, t: &dyn Fn(&str) -> String) {
        if self
            .output_probe_cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            self.output_probe_rx = None;
            self.output_probe_cancel = None;
            return;
        }
        let msg = match self.output_probe_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match msg {
            Ok((epoch, note)) => {
                self.output_probe_rx = None;
                self.output_probe_cancel = None;
                if epoch == self.output_probe_epoch {
                    self.output_dir_note = Some(note);
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(TryRecvError::Disconnected) => {
                self.output_probe_rx = None;
                self.output_probe_cancel = None;
                // worker 崩了：保留当前缓存键，避免每帧重新起线程；用户改目录会重新探测。
                if self.output_dir_note.is_none() {
                    self.output_dir_note = Some(t("err_no_output_dir").to_string());
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // 开始 / 停止
    // -----------------------------------------------------------------------

    /// 开始录制。
    ///
    /// **`pub` 是刻意的**：全局快捷键与托盘菜单的「开始」必须和状态卡片上那个主按钮走
    /// **同一个函数**（导出收尾、预热补建、错误文案才可能一致），而不是另写一套逻辑。
    /// 返回值 = 这次动作产生的可见错误（`None` = 成功），供外壳走底部状态栏那条通道 ——
    /// 因为触发时用户可能不在录制页，等不到页面自己上报。
    pub fn start_recording(
        &mut self,
        cfg: &Config,
        recorder: &mut Option<Recorder>,
        result: &mut Option<RecordingResult>,
        i18n: &I18n,
    ) -> Option<String> {
        self.reap_finished_jobs();
        if self.has_active_recording_path(recorder.is_some()) {
            self.error = Some(i18n.t("err_recording_busy").to_string());
            return self.error.clone();
        }
        self.error = None;
        self.watchdog_fired = false;
        // 两个落点：流文件进 Cache、导出产物进用户设置的输出目录（同名）
        //
        // 失败时**技术原文只进日志**：`record_paths_checked` 给的是"配置值原文 + 系统错误"
        // 那种诊断串，直接当界面文案会漏翻（P1：语言 = English 时仍显示中文）。
        // 界面那一句走 i18n 的 `err_no_output_dir`（含具体配置值，便于用户自己修）。
        let (streaming, _exported) = match record_paths_checked(cfg, self.cache_dir.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "录制落点解析失败（输出目录不可用）");
                self.error = Some(i18n.t("err_no_output_dir").to_string());
                return self.error.clone();
            }
        };
        // **不能在 UI 线程调 `Recorder::start`**：它要开 DXGI、初始化 OpenH264
        //（忙机 0.46–6 s）、开 WASAPI；这几秒窗口会“未响应”，托盘/热键也一起失效。
        // 这里只起线程，结果由 `poll_recording_jobs` 在后续帧收。
        let cfg_thread = cfg.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("cap-start".into())
            .spawn(move || {
                let r = Recorder::start_cancellable(&cfg_thread, &streaming, cancel_thread);
                let _ = tx.send(r);
            });
        match spawned {
            Ok(h) => {
                self.start_rx = Some(rx);
                self.start_handle = Some(h);
                self.start_cancel = Some(cancel);
                self.starting = true;
                self.start_deadline = Some(Instant::now() + START_WATCHDOG);
                *result = None;
                None
            }
            Err(e) => {
                self.error = Some(i18n.tf("err_start_failed", &[&e.to_string()]));
                self.error.clone()
            }
        }
    }

    /// 停止录制（`pub` 的理由同 [`Self::start_recording`]：托盘/快捷键必须复用这一条路径）。
    ///
    /// **后台 stop**：join 四个线程 + `+faststart` 重封装可能几十秒到几分钟，
    /// 不能在 UI 线程做。这里把 `Recorder` 移交给线程，UI 进入 `stopping`，
    /// 结果由 `poll_recording_jobs` 收。
    pub fn stop_recording(
        &mut self,
        cfg: &Config,
        recorder: &mut Option<Recorder>,
        _result: &mut Option<RecordingResult>,
        i18n: &I18n,
    ) -> Option<String> {
        if self.stopping {
            return self.error.clone();
        }
        if self.starting {
            // 启动还没结束：不阻塞、也不报错，登记“启动完成后立刻停”。
            self.stop_after_start = true;
            return None;
        }
        let Some(mut r) = recorder.take() else {
            return self.error.clone();
        };
        let _ = cfg; // 预热在结果回来后做；此处留着参数是为了调用点签名不变
        self.stop_cancel = Some(r.cancel_token());
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("cap-stop".into())
            .spawn(move || {
                let res = r.stop();
                let _ = tx.send(res);
            });
        match spawned {
            Ok(h) => {
                self.stop_rx = Some(rx);
                self.stop_handle = Some(h);
                self.stopping = true;
                self.watchdog_fired = false;
                self.stop_deadline = Some(Instant::now() + STOP_WATCHDOG);
                None
            }
            Err(e) => {
                // 线程都起不来：闭包被 drop → `Recorder::Drop` 会在本线程同步收尾。
                // 极端资源压力下这可能短暂阻塞，但比留下一个没人 join 的录制会话安全。
                self.stopping = false;
                self.stop_rx = None;
                self.stop_handle = None;
                self.stop_cancel = None;
                self.error = Some(i18n.tf("err_stop_failed", &[&e.to_string()]));
                self.error.clone()
            }
        }
    }

    /// 本页是否正在做“开始/停止”的耗时工作（UI 只显示进度，不阻塞）。
    pub fn is_busy(&self) -> bool {
        self.starting || self.stopping
    }

    /// 录制子系统是否"全忙"：正在 start/stop 中 / 有活跃 Recorder / 后台 job 未 finish。
    ///
    /// 用于"能不能开始新录制"、"要不要拦截窗口关闭"、"要不要持续排帧"等场景。
    /// 参数 `recorder_active` = 调用方传入的 `recorder.is_some()`（Recorder 被 &mut 限制
    /// 无法从 `MainPage::&self` 直接访问，故由调用方显式传入）。
    pub fn has_active_recording_path(&self, recorder_active: bool) -> bool {
        self.starting || self.stopping || recorder_active || self.jobs_still_running()
    }

    fn jobs_still_running(&self) -> bool {
        self.start_handle.as_ref().is_some_and(|h| !h.is_finished())
            || self.stop_handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// 回收已经结束的后台句柄；看门狗超时后句柄会保留到这里，避免旧任务未退出时
    /// UI 又启动第二个录制会话。
    fn reap_finished_jobs(&mut self) {
        if self.start_handle.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = self.start_handle.take() {
                let _ = h.join();
            }
            self.start_cancel = None;
        }
        if self.stop_handle.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = self.stop_handle.take() {
                let _ = h.join();
            }
            self.stop_cancel = None;
        }
    }

    /// 请求“当前收尾完成后关闭窗口”（托盘 Quit / 用户点 X）。
    pub fn request_quit_after_finish(&mut self) {
        self.pending_quit = true;
    }

    /// 请求“启动完成后立即停止”（锁屏/挂起时正处于 `starting`，或用户连按停止）。
    pub fn request_stop_after_start(&mut self) {
        // 只有“正在启动”才需要这个标志；已经在停止时置位会误伤下一次录制。
        if self.starting {
            self.stop_after_start = true;
        }
    }

    /// 取走“完成后关闭”标志（外壳在 `poll_recording_jobs` 之后调用）。
    pub fn take_pending_quit(&mut self) -> bool {
        std::mem::take(&mut self.pending_quit)
    }

    /// 收后台开始/停止的结果（每帧调；绝不阻塞）。
    pub fn poll_recording_jobs(
        &mut self,
        cfg: &Config,
        recorder: &mut Option<Recorder>,
        result: &mut Option<RecordingResult>,
        i18n: &I18n,
    ) {
        self.reap_finished_jobs();
        if let Some(rx) = self.start_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(r)) => {
                    self.starting = false;
                    self.start_rx = None;
                    self.start_deadline = None;
                    self.start_cancel = None;
                    self.log_start_phases(&r);
                    *recorder = Some(r);
                    *result = None;
                }
                Ok(Err(e)) => {
                    self.starting = false;
                    self.start_rx = None;
                    self.start_deadline = None;
                    self.start_cancel = None;
                    self.stop_after_start = false;
                    self.error = Some(i18n.tf("err_start_failed", &[&e.to_string()]));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.starting = false;
                    self.start_rx = None;
                    self.start_deadline = None;
                    self.start_cancel = None;
                    self.stop_after_start = false;
                    self.error = Some(i18n.tf(
                        "err_start_failed",
                        &[&"start thread exited unexpectedly".to_string()],
                    ));
                }
            }
        }
        if let Some(rx) = self.stop_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(res)) => {
                    self.stopping = false;
                    self.stop_rx = None;
                    self.stop_deadline = None;
                    self.stop_cancel = None;
                    if let Some(issue) = res.session_issue.as_deref() {
                        self.error = Some(i18n.tf("session_issue_warning", &[&issue]));
                    } else {
                        self.error = None;
                    }
                    *result = Some(res);
                    // 停完补一次预热：本次录制把预热实例取走了，紧接着录第二段不应回到同步初始化。
                    startup_marks::prewarm_encoder_once(cfg, self.startup_log);
                }
                Ok(Err(e)) => {
                    self.stopping = false;
                    self.stop_rx = None;
                    self.stop_deadline = None;
                    self.stop_cancel = None;
                    self.error = Some(i18n.tf("err_stop_failed", &[&e.to_string()]));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.stopping = false;
                    self.stop_rx = None;
                    self.stop_deadline = None;
                    self.stop_cancel = None;
                    self.error = Some(i18n.tf(
                        "err_stop_failed",
                        &[&"stop thread exited unexpectedly".to_string()],
                    ));
                }
            }
        }
        if let Some(deadline) = self.start_deadline {
            if self.starting && Instant::now() >= deadline {
                self.starting = false;
                self.start_rx = None;
                if let Some(cancel) = &self.start_cancel {
                    cancel.store(true, Ordering::Release);
                }
                self.start_cancel = None;
                // 句柄刻意保留：超时不等于线程已经退出。reap_finished_jobs()
                // 会在它真正结束后回收，期间 start_recording 拒绝叠加新会话。
                self.start_deadline = None;
                self.stop_after_start = false;
                self.watchdog_fired = true;
                self.error = Some(i18n.t("err_start_timeout").to_string());
                tracing::error!(
                    timeout_s = START_WATCHDOG.as_secs(),
                    "启动看门狗触发：录制引擎未在超时内就绪，已放弃等待"
                );
            }
        }
        if let Some(deadline) = self.stop_deadline {
            if self.stopping && Instant::now() >= deadline {
                self.stopping = false;
                self.stop_rx = None;
                if let Some(cancel) = &self.stop_cancel {
                    cancel.store(true, Ordering::Release);
                }
                self.stop_cancel = None;
                // 同理：保留句柄直到线程真正结束；取消令牌会让 remux 在包边界退出。
                self.stop_deadline = None;
                self.watchdog_fired = true;
                self.error = Some(i18n.t("err_stop_timeout").to_string());
                tracing::error!(
                    timeout_s = STOP_WATCHDOG.as_secs(),
                    "停止/导出看门狗触发：收尾未在超时内完成，已放弃 UI 等待（分片保留）"
                );
            }
        }
        // 启动阶段被要求停止（锁屏/挂起/用户按停止）：启动完成后立刻停止。
        if self.stop_after_start && !self.starting && !self.stopping && recorder.is_some() {
            self.stop_after_start = false;
            let _ = self.stop_recording(cfg, recorder, result, i18n);
        }
        // 用户已经请求退出，但当时还在“启动中”：启动完成后立刻停止，
        // 不能因为 pending_quit 就带着一个正在录制的会话退出。
        if self.pending_quit && !self.starting && !self.stopping && recorder.is_some() {
            let _ = self.stop_recording(cfg, recorder, result, i18n);
        }
    }

    /// 退出前把在途的开始/停止等完；**最多等 [`EXIT_WATCHDOG`]**。
    ///
    /// 返回 `false` = 超时/看门狗触发：后台线程可能仍在收尾，调用方应记日志并退出，
    /// 不要为了 join 一个卡死的 FFmpeg/DXGI 调用而永久挂住进程。
    pub fn wait_for_recording_jobs(
        &mut self,
        cfg: &Config,
        recorder: &mut Option<Recorder>,
        result: &mut Option<RecordingResult>,
        i18n: &I18n,
    ) -> bool {
        let deadline = Instant::now() + EXIT_WATCHDOG;
        loop {
            self.poll_recording_jobs(cfg, recorder, result, i18n);
            if !self.has_active_recording_path(recorder.is_some()) {
                return !self.watchdog_fired;
            }
            if Instant::now() >= deadline {
                self.watchdog_fired = true;
                self.error = Some(i18n.t("err_stop_timeout").to_string());
                tracing::error!(
                    timeout_s = EXIT_WATCHDOG.as_secs(),
                    "退出看门狗触发：录制收尾未完成，保留 Cache 分片并退出"
                );
                return false;
            }
            if recorder.is_some() && !self.is_busy() {
                let _ = self.stop_recording(cfg, recorder, result, i18n);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn log_start_phases(&self, r: &Recorder) {
        let ph = r.start_phases();
        startup_marks::note(
            self.startup_log,
            &format!(
                "[cap] recording started: session ready {}ms (DXGI capture {}ms / \
                 OpenH264 {}ms{} / audio {}ms / AAC {}ms); first frame encoded {}ms",
                ph.total_ms,
                ph.capture_open_ms,
                ph.encoder_init_ms,
                if ph.encoder_prewarmed {
                    ", prewarm hit"
                } else {
                    ", prewarm miss"
                },
                ph.audio_open_ms,
                ph.aac_init_ms,
                ph.first_frame_ms
            ),
        );
    }

    /// 本页**当前如实上报**给外壳的可见错误（托盘/快捷键触发时用）。
    ///
    /// 为什么需要它：那两条入口运行时用户可能根本不在录制页，
    /// `MainPage::ui` 这一帧不会被调用，页面自己没机会把 `error` 交给外壳。
    pub fn reportable_error(&self) -> Option<String> {
        self.error.clone()
    }

    // -----------------------------------------------------------------------
    // 状态与结果
    // -----------------------------------------------------------------------

    /// 音频降级提示的文案（没有声音时）。以前它是录制页里的一行红字，
    /// 现在交给外壳的底部状态栏统一显示。
    fn audio_warning_text(&self, recorder: &Option<Recorder>, i18n: &I18n) -> Option<String> {
        let rec = recorder.as_ref()?;
        if rec.audio_warnings().is_empty() {
            return None;
        }
        Some(format!(
            "{} {}",
            i18n.tf(
                "warn_no_audio_track",
                &[&rec.audio_warnings().join(i18n.t("list_sep"))],
            ),
            i18n.t("warn_no_audio_track_hint")
        ))
    }

    /// 顶部状态卡片：状态点 + 状态文字 + **大号等宽时长** + **主操作按钮**
    ///（用户要求："将录制按钮放到时间码右侧"）。
    ///
    /// * 三者同一行、**左对齐**（按钮不再被推到卡片最右侧）；
    /// * 按钮 [`theme::START_BTN_H`] = 34px 高、[`theme::START_BTN_W`] = 132 宽，与时间码协调；
    ///   录制中变成危险色的「停止录制」时**位置与尺寸完全不变**（同一行同一 x，避免点按时跳）；
    /// * 那一行的高度**由行内最高的部件决定**。本轮（用户要求"把这一行压低 + 时间码字号调小一档"）
    ///   把计时器字号从 34 降到 29（[`theme::FS_TIMER`]），时间码行高 40 → 34，
    ///   与按钮**刚好齐平** → 行高 40 → 34、状态卡片 62 → 56、整页 402 → 396。
    ///   按钮尺寸一个像素都没动（它是主操作），状态文字仍是 [`theme::FS_STATUS`]；
    /// * 返回值是本帧要执行的动作（Start/Stop），由 `ui()` 统一执行 —— 这样
    ///   `status_card` 里只需要 `&recorder`，不用同时可变借用 `recorder`/`cfg`。
    ///
    /// `probe`：逐部件高度探针（只在 `#[cfg(test)]` 下有内容，见 [`RecordProbe`]）。
    /// 发布构建里它不参与任何布局，只是被忽略的参数。
    // 绘制函数：每个参数都是本帧要读的一份外部状态，没有可以内聚掉的。
    #[allow(clippy::too_many_arguments)]
    fn status_card(
        &mut self,
        ui: &mut egui::Ui,
        recorder: &Option<Recorder>,
        result: &Option<RecordingResult>,
        cfg: &Config,
        i18n: &I18n,
        t: &dyn Fn(&str) -> String,
        probe: &mut RecordProbe,
    ) -> Option<RecordAction> {
        let _ = probe;
        let mut action = None;
        // 这一行的高度实测（状态点 / 状态文字 / 时间码 / 主按钮各自多高）
        #[cfg(test)]
        let mut m_dot = 0.0f32;
        #[cfg(test)]
        let mut m_text = 0.0f32;
        #[cfg(test)]
        let mut m_timer = 0.0f32;
        #[cfg(test)]
        let mut m_btn = (0.0f32, 0.0f32);
        #[cfg_attr(not(test), allow(unused_variables))]
        let (_, card) = theme::card_tight(ui, |ui| {
            let row = ui.horizontal(|ui| {
                let (text, color) = if self.starting {
                    (t("state_starting"), theme::warn_fg())
                } else if self.stopping {
                    (t("state_stopping"), theme::warn_fg())
                } else {
                    match recorder {
                        Some(rec) => state_text(i18n, &rec.state()),
                        None => (t("state_idle"), theme::muted()),
                    }
                };
                #[cfg_attr(not(test), allow(unused_variables))]
                let dot = theme::status_dot(ui, color);
                #[cfg_attr(not(test), allow(unused_variables))]
                let text_resp = ui.label(
                    egui::RichText::new(text)
                        .size(theme::FS_STATUS)
                        .strong()
                        .color(color),
                );
                ui.add_space(16.0);
                let elapsed = match (recorder, result) {
                    (Some(rec), _) => Some(rec.elapsed()),
                    (None, Some(res)) => Some(res.duration),
                    _ => None,
                };
                // 时间码：**一个 `ui.label` 出口**（以前是两个分支各写一次），
                // 字号来自 `theme::mono_big`（= `theme::FS_TIMER`，单一来源），
                // 字体族仍是等宽、`RichText` 不做额外对齐 → 录制中数字不会跳动。
                let timer_text = match elapsed {
                    Some(d) => fmt_duration(d),
                    None => "00:00".to_string(),
                };
                #[cfg_attr(not(test), allow(unused_variables))]
                let timer = ui.label(theme::mono_big(&timer_text));
                #[cfg(test)]
                {
                    m_dot = dot.rect.height();
                    m_text = text_resp.rect.height();
                    m_timer = timer.rect.height();
                }
                // 主操作按钮：**卡片右侧、右对齐**（用户要求：不要紧贴时间码），
                // 尺寸：高 [`theme::START_BTN_H`]（34）、宽 [`theme::START_BTN_W`]（132）、
                // 字号 13.5 —— 仍用主操作色保持醒目；
                // 录制中换成危险色的「停止录制」时**位置与尺寸完全不变**（不会跳动）。
                //
                // **本轮"压缩状态行"明确不动它**（用户要求：主操作别动）——所以那一行只有在
                // 时间码降到与它齐平（[`theme::FS_TIMER`] = 29 → 行高 34）时才真正压下来；
                // 再降字号行高也不会变了（按钮就是下限），这也是选 29 而不是更小的理由。
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let recording = recorder.is_some();
                    let busy = self.starting || self.stopping;
                    let (label, fill) = if self.starting {
                        (t("state_starting"), theme::muted())
                    } else if self.stopping {
                        (t("state_finalizing"), theme::muted())
                    } else if recording {
                        (t("btn_stop"), theme::danger())
                    } else {
                        (t("btn_start"), theme::accent())
                    };
                    let btn = egui::Button::new(
                        egui::RichText::new(label)
                            .size(13.5)
                            .strong()
                            .color(egui::Color32::WHITE),
                    )
                    .fill(fill)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::same(6));
                    let resp = ui.add_sized([theme::START_BTN_W, theme::START_BTN_H], btn);
                    #[cfg(test)]
                    {
                        m_btn = (resp.rect.width(), resp.rect.height());
                    }
                    if resp.clicked() && !busy {
                        action = Some(if recording {
                            RecordAction::Stop
                        } else {
                            RecordAction::Start
                        });
                    }
                    // ---- 分辨率信息（P38，用户附图口径）：挪到「开始录制」**左侧** ----
                    //
                    // 原来它在采集卡「显示器」行右侧（`全屏录制：1920x1080` /
                    // `区域录制：800x600` / `区域录制：未选择`）。采集卡那一行现在要让给
                    // 「录制范围」单选，所以分辨率信息上移到状态卡片 —— 与"录制内容"
                    // 同属一眼要确认的信息，挨着主按钮也最顺。
                    // `right_to_left` 布局：先加按钮（最右），后加这段文字 → 文字落在按钮左边。
                    //
                    // 复用 `region_info_text`（纯函数，三种分支有单测），**不新增 i18n 键**。
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(region_info_text(
                            cfg.capture_mode,
                            self.selected_geo(cfg).map(|m| (m.w, m.h)),
                            cfg.region,
                            &t("mode_fullscreen"),
                            &t("mode_region"),
                            &t("region_not_chosen"),
                            &t("label_sep"),
                        ))
                        .size(theme::FS_SMALL)
                        .color(theme::muted()),
                    );
                });
            });
            #[cfg(test)]
            {
                // 这一行的高度 = `ui.horizontal` 量到的行矩形高；
                // 行内的"撑高"余量 = 行高 − 行内最高部件（状态点/文字/时间码/按钮）
                let highest = m_dot.max(m_text).max(m_timer).max(m_btn.1);
                probe.status_row_h = row.response.rect.height();
                probe.status_row_stack_h = (probe.status_row_h - highest).max(0.0);
                probe.status_dot_h = m_dot;
                probe.status_text_h = m_text;
                probe.timer_text_h = m_timer;
                probe.start_btn_h = m_btn.1;
                probe.start_btn_w = m_btn.0;
            }
            #[cfg(not(test))]
            let _ = row;
            let Some(rec) = recorder else {
                return;
            };
            // 音频降级提示不再画在这里：它走外壳的底部状态栏（`MainOutcome::warning`），
            // 全程序只有一条消息通道，避免同一件事在两处重复出现。
            ui.add_space(theme::GAP_ROW_TIGHT + 3.0);
            ui.label(
                egui::RichText::new(t("stats_title"))
                    .size(theme::FS_SECTION)
                    .strong()
                    .color(theme::text()),
            );
            ui.add_space(6.0);
            stats_grid(ui, &rec.stats(), t);
            // 时长/统计要持续刷新
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        });
        #[cfg(test)]
        {
            probe.status_card_h = card.outer_h;
            probe.status_card_inner_h = card.inner_h;
        }
        action
    }
    /// 结果卡片：最近一次录制的产物信息 + "在剪辑页打开"
    ///
    /// 返回 **(要打开的文件, 是否导出失败后的回退)**：**优先导出产物**（用户输出目录里的
    /// 普通 MP4），只有导出失败（`exported` 为空或文件不存在）时才回退到 **Cache 里的流文件**。
    /// 注意：这里只按 `res.exported` 判断；剪辑页自己的"打开最近录制"另有对遗留
    /// `*.plain.mp4` 的兼容读取（`ui::editor::newest_recording`），那条路径与本函数无关。
    fn result_card(
        &mut self,
        ui: &mut egui::Ui,
        result: &Option<RecordingResult>,
        t: &dyn Fn(&str) -> String,
    ) -> Option<(PathBuf, bool)> {
        let mut open_in_editor = None;
        let Some(res) = result else {
            return None;
        };
        let (_, _card) = theme::card_tight(ui, |ui| {
            ui.horizontal(|ui| {
                theme::status_dot(ui, theme::ok_fg());
                ui.label(
                    egui::RichText::new(t("result_title"))
                        .size(theme::FS_SECTION)
                        .strong()
                        .color(theme::text()),
                );
                ui.add_space(12.0);
                ui.label(theme::mono(&fmt_duration(res.duration)));
            });
            ui.add_space(theme::GAP_ROW_TIGHT);
            // 录制过程本身有问题（采集/编码提前失败、线程 panic、磁盘不足）：
            // 即使导出完整也要常驻显示，不能因为红色提示条被关掉就变成“成功”。
            if let Some(issue) = &res.session_issue {
                theme::banner(
                    ui,
                    theme::Banner::Warn,
                    &t("session_issue_warning").replace("{}", issue),
                );
                ui.add_space(6.0);
            }
            // 导出产物在**用户设置的输出目录**，流文件在 **Cache**（导出后仍保留，可找回）。
            // 结果区**优先显示导出产物**，并说明临时副本在 Cache 里。
            let exported_ok = !res.exported.as_os_str().is_empty() && res.exported.is_file();
            if exported_ok {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [92.0, theme::H_CONTROL],
                        egui::Label::new(
                            egui::RichText::new(t("exported_file"))
                                .size(theme::FS_SMALL)
                                .color(theme::muted()),
                        )
                        .halign(egui::Align::LEFT),
                    );
                    ui.label(theme::mono(&elide(&res.exported.to_string_lossy(), 72)));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [92.0, theme::H_CONTROL],
                        egui::Label::new(
                            egui::RichText::new(t("cache_file"))
                                .size(theme::FS_SMALL)
                                .color(theme::muted()),
                        )
                        .halign(egui::Align::LEFT),
                    );
                    ui.label(theme::mono_muted(&elide(
                        &res.streaming.to_string_lossy(),
                        72,
                    )));
                });
            } else {
                // 导出失败：只有 Cache 里的流文件（它就写在 Cache 目录里，可直接播放/剪辑）
                theme::banner(ui, theme::Banner::Warn, &t("streaming_kept"));
                // **导出被判为"不完整"时的具体原因**（例如"源 5000 ms / 产物 759 ms"）：
                // 必须说清楚 —— 否则用户拿到的是一个"看不出问题的短文件"。
                // 这条文案就是本轮"绝不静默产出残缺文件"在界面上的落点。
                if let Some(issue) = &res.export_issue {
                    ui.add_space(4.0);
                    ui.label(theme::mono_muted(&elide(
                        &t("export_incomplete").replace("{}", issue),
                        96,
                    )));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [92.0, theme::H_CONTROL],
                        egui::Label::new(
                            egui::RichText::new(t("cache_file"))
                                .size(theme::FS_SMALL)
                                .color(theme::muted()),
                        )
                        .halign(egui::Align::LEFT),
                    );
                    ui.label(theme::mono(&elide(&res.streaming.to_string_lossy(), 72)));
                });
            }
            // 交给剪辑页的文件：**导出产物优先**，导出失败才回退 Cache 里的流文件
            let fallback = !exported_ok;
            let target = if exported_ok {
                res.exported.clone()
            } else {
                res.streaming.clone()
            };
            // 录制页**不再提供"打开所在文件夹"**（用户要求去掉）：产物路径已经等宽显示，
            // 需要打开时用系统的文件管理器即可；这里只留"在剪辑页打开"这个主操作。
            ui.add_space(theme::GAP_ROW_TIGHT);
            if theme::primary_button(ui, &t("btn_open_in_editor"), 180.0).clicked() {
                open_in_editor = Some((target, fallback));
            }
        });
        open_in_editor
    }

    // -----------------------------------------------------------------------
    // 显示器枚举
    // -----------------------------------------------------------------------

    /// 重新枚举显示器：**这是唯一一处枚举入口**（进页面一次 / 约 5 秒定时 / 打开下拉 / 点选后）。
    ///
    /// 失败时**保留上一次的列表**（绝不清空、绝不 panic）：界面继续显示并可用旧的列表，
    /// 只把原因记成 warning（`monitors_err` → 页面那行琥珀小字；同时进 tracing 日志）。
    fn refresh_monitors(&mut self) {
        // 每次真的枚举都留一条 debug 日志（`RUST_LOG=cap=debug` 可观察频率：
        // 进页面一次 + 每 ≥5 秒最多一次 + 打开下拉时一次，**绝不是每帧一次**）
        tracing::debug!("枚举显示器（进页面 / ≥5s 周期 / 打开下拉 / 点选后）");
        self.monitors_loaded = true;
        let before = self.monitors.clone();
        self.geo = enumerate_monitor_geo().unwrap_or_default();
        match list_monitors() {
            Ok(list) => {
                self.monitors = list;
                self.monitors_err = None;
            }
            Err(e) => {
                // 枚举失败：旧列表原样保留，用户的选择与下拉内容都不受影响
                self.monitors = before;
                self.monitors_err = Some(e.to_string());
                tracing::warn!(error = %e, "枚举显示器失败，继续沿用上一次的列表");
            }
        }
        if !self.geo.is_empty() && self.monitors.len() < self.geo.len() {
            // `list_monitors` 拿不到名字时用几何信息补一个能看的标签
            for (i, g) in self.geo.iter().enumerate().skip(self.monitors.len()) {
                self.monitors.push(MonitorEntry {
                    index: i + 1,
                    name: format!("Display {}", i + 1),
                    width: g.w,
                    height: g.h,
                    primary: g.primary,
                });
            }
        }
    }

    fn refresh_mic_devices(&mut self) {
        refresh_mic_devices(&mut self.mic_devices, &mut self.mic_err);
    }

    fn primary_geo(&self) -> Option<MonitorGeo> {
        self.geo
            .iter()
            .find(|m| m.primary)
            .or_else(|| self.geo.first())
            .copied()
    }

    fn selected_geo(&self, cfg: &Config) -> Option<MonitorGeo> {
        let rank = rank_of_cfg(cfg.monitor_index);
        self.geo.get(rank).or_else(|| self.geo.first()).copied()
    }

    /// 打开框选遮罩：**先抓一张桌面冻结帧**，再开一个不透明的全屏窗口把帧铺上去。
    ///
    /// 截图失败不算致命：遮罩照开，只是少了"桌面背景"（界面会显示具体原因），
    /// 这样用户至少能框选、而不是对着一片黑屏猜发生了什么。
    fn open_picker(&mut self, cfg: &Config, i18n: &I18n, t: &dyn Fn(&str) -> String) {
        if self.geo.is_empty() {
            self.refresh_monitors();
        }
        let want = rank_of_cfg(cfg.monitor_index);
        let rank = if want < self.geo.len() { want } else { 0 };
        let Some(monitor) = self.geo.get(rank).copied() else {
            self.error = Some(t("err_no_monitor"));
            return;
        };
        let label = self
            .monitors
            .get(rank)
            .map(|m| m.label())
            .unwrap_or_else(|| monitor.label(&t("monitor")));

        // 冻结帧：只抓**目标显示器**（遮罩只铺在这一块屏上；整张虚拟桌面没必要）。
        let (frame, capture_err) = match capture_monitor(&monitor) {
            Ok(f) => (Some(f), None),
            Err(e) => {
                tracing::warn!("框选冻结帧抓取失败: {e}");
                (None, Some(e))
            }
        };
        self.error = capture_err.map(|e| i18n.tf("err_pick_capture_failed", &[&e]));

        self.picker = Some(Picker {
            rank,
            monitor,
            label,
            anchor: None,
            cursor: None,
            dragging: false,
            drag_mode: None,
            finished: false,
            press_on_confirm: false,
            too_small: false,
            frame,
            texture: None,
        });
    }
}

// ---------------------------------------------------------------------------
// 内部数据结构
// ---------------------------------------------------------------------------

/// 显示器几何（虚拟桌面物理像素）
#[derive(Debug, Clone, Copy)]
struct MonitorGeo {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    primary: bool,
}

impl MonitorGeo {
    /// 物理像素边界 `(left, top, right, bottom)`，全程 `i64`（副屏在主屏左侧时是负数）
    fn bounds(&self) -> (i64, i64, i64, i64) {
        (
            self.x as i64,
            self.y as i64,
            self.x as i64 + self.w as i64,
            self.y as i64 + self.h as i64,
        )
    }

    /// 显示器整屏矩形（物理像素，宽高取偶）
    fn region(&self) -> Region {
        Region {
            x: self.x,
            y: self.y,
            width: self.w & !1,
            height: self.h & !1,
        }
    }

    /// 供界面显示的标签：`显示器 2 [1920, 0 2560×1440]`。
    /// 带上虚拟桌面坐标，多屏时用户才能判断"这块屏到底是哪块"。
    fn label(&self, monitor_word: &str) -> String {
        let (l, t, r, b) = self.bounds();
        format!("{monitor_word} ({l},{t})-({r},{b}) [{}×{}]", self.w, self.h)
    }
}

/// 框选遮罩的状态
struct Picker {
    /// 目标显示器在 EnumDisplayMonitors（= winit `available_monitors`）里的序号
    rank: usize,
    monitor: MonitorGeo,
    label: String,
    anchor: Option<Pos2>,
    cursor: Option<Pos2>,
    /// 指针当前是否按着左键（决定 Idle / Dragging / Adjusting 三态与手柄是否显示）
    dragging: bool,
    /// 这一次按下在拖什么（手柄缩放 / 整体移动 / 重新点选）
    drag_mode: Option<PickDrag>,
    /// 是否已经通过「确定」提交（可测的"已确认"标志；提交后遮罩会关闭）
    finished: bool,
    /// 这一次按下是否落在「确定」按钮上（松开时还要再确认一次指针仍在按钮内）
    press_on_confirm: bool,
    /// 上一次松开时选区太小
    too_small: bool,
    /// 进入框选那一刻的桌面冻结帧（整张虚拟桌面，桌面物理像素原点在左上角）
    frame: Option<Screenshot>,
    /// 冻结帧上传到 GPU 后的纹理句柄（只在遮罩窗口第一帧创建一次）
    texture: Option<egui::TextureHandle>,
}

/// 状态卡片里主操作按钮的动作（在 `ui()` 里统一执行）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordAction {
    Start,
    Stop,
}

enum PickerEnd {
    Keep,
    Picked(Region),
    Cancelled,
}

/// 这一次按下在拖什么
#[derive(Debug, Clone, Copy, PartialEq)]
enum PickDrag {
    /// 重新点选（矩形外按下，或还没有选区）
    NewSelection,
    /// 拖动某个手柄缩放（`orig` 是按下那一刻的矩形）
    Handle { h: PickHandle, orig: Option<Rect> },
    /// 在矩形内部拖动 = 整体移动（`grab` 是按下点，`orig` 是按下那一刻的矩形）
    Move { grab: Pos2, orig: Option<Rect> },
}

// ---------------------------------------------------------------------------
// 纯逻辑：压暗、坐标换算、矩形规范化
// ---------------------------------------------------------------------------

/// **选区内部**的半透明深色填充（用户要求："鼠标开始选取时，把选取的部分变深"）。
/// 约 38% 黑：既能明显看出"这块被选中了"，又不会看不清里面的内容。
const SELECT_DIM_ALPHA: u8 = 96;

/// 缩放手柄的边长（点）；手柄命中优先级高于"内部=移动 / 外部=重选"
const HANDLE_SIZE: f32 = 10.0;
/// 「确定」按钮尺寸（用户要求高度 ~28–32px）
const CONFIRM_SIZE: Vec2 = Vec2::new(72.0, 30.0);
/// 小于这个尺寸不显示「确定」（避免误点一个 1×1 的选区）
const CONFIRM_MIN_SIDE: f32 = 8.0;

/// 框选遮罩的交互状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickState {
    /// 还没有选区：**整屏正常亮度，不压暗**
    Idle,
    /// 正在拖出新矩形
    Dragging,
    /// 已经松开：**留在遮罩里继续调整**（缩放/移动/重选），点「确定」才返回
    Adjusting,
}

/// 8 个缩放手柄（四角 + 四边中点）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickHandle {
    N,
    S,
    W,
    E,
    Nw,
    Ne,
    Sw,
    Se,
}

/// 一次按下/拖动命中的目标（**优先级从高到低**）：
/// 确定按钮 → 缩放手柄 → 矩形内部（移动）→ 矩形外部（重新点选）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickHit {
    Confirm,
    Handle(PickHandle),
    Move,
    Reselect,
}

/// 选区填充的透明度：**没有选区就是 0**（不压暗），有选区才是 [`SELECT_DIM_ALPHA`]。
fn dim_alpha_for(sel: Option<Rect>) -> u8 {
    match sel {
        Some(r) if r.width() >= 1.0 && r.height() >= 1.0 => SELECT_DIM_ALPHA,
        _ => 0,
    }
}

/// 由"锚点 + 当前点"得到选区矩形：支持**反向拖拽**（从右下往左上），并**夹紧到屏幕内**。
/// 1×1 的退化矩形也不会 panic（`Rect::from_two_pos` 允许零宽高）。
fn selection_from_drag(anchor: Pos2, current: Pos2, bounds: Rect) -> Rect {
    Rect::from_two_pos(anchor, current).intersect(bounds)
}

/// 8 个手柄的中心点（纯函数，便于测试与绘制共用一套几何）
fn handle_centers(sel: Rect) -> [(PickHandle, Pos2); 8] {
    let c = sel.center();
    [
        (PickHandle::Nw, sel.left_top()),
        (PickHandle::Ne, sel.right_top()),
        (PickHandle::Sw, sel.left_bottom()),
        (PickHandle::Se, sel.right_bottom()),
        (PickHandle::N, Pos2::new(c.x, sel.min.y)),
        (PickHandle::S, Pos2::new(c.x, sel.max.y)),
        (PickHandle::W, Pos2::new(sel.min.x, c.y)),
        (PickHandle::E, Pos2::new(sel.max.x, c.y)),
    ]
}

/// 「确定」按钮的矩形：**选区正下方、水平居中**（用户要求）。
/// 下方空间不够（贴着屏幕底边）时改为**贴在选区内侧底部居中**，始终可见可点。
fn confirm_button_rect(sel: Rect, area: Rect) -> Option<Rect> {
    if sel.width() < CONFIRM_MIN_SIDE || sel.height() < CONFIRM_MIN_SIDE {
        return None;
    }
    let gap = 10.0;
    let center_x = sel.center().x;
    let below = Rect::from_min_size(
        Pos2::new(center_x - CONFIRM_SIZE.x / 2.0, sel.max.y + gap),
        CONFIRM_SIZE,
    );
    if area.contains_rect(below) {
        return Some(below);
    }
    // 贴屏幕底边：改到**选区内侧**底部居中（同样水平居中）
    let inside = Rect::from_min_size(
        Pos2::new(
            (center_x - CONFIRM_SIZE.x / 2.0).clamp(area.min.x, area.max.x - CONFIRM_SIZE.x),
            (sel.max.y - CONFIRM_SIZE.y - gap).max(area.min.y),
        ),
        CONFIRM_SIZE,
    );
    Some(inside.intersect(area))
}

/// 命中判定（**优先级写死在这里**，测试与绘制共用）：
/// 1. 「确定」按钮（必须先判它，否则点击会被拖拽逻辑吞掉）；
/// 2. 8 个缩放手柄（含一点容差）；
/// 3. 矩形内部 → 移动；
/// 4. 其余 → 重新点选。
fn picker_hit(p: Pos2, sel: Option<Rect>, area: Rect) -> PickHit {
    let Some(sel) = sel else {
        return PickHit::Reselect;
    };
    if let Some(btn) = confirm_button_rect(sel, area) {
        if btn.expand(2.0).contains(p) {
            return PickHit::Confirm;
        }
    }
    let half = HANDLE_SIZE / 2.0;
    for (h, c) in handle_centers(sel) {
        if Rect::from_center_size(c, Vec2::splat(HANDLE_SIZE))
            .expand(half)
            .contains(p)
        {
            return PickHit::Handle(h);
        }
    }
    if sel.contains(p) {
        PickHit::Move
    } else {
        PickHit::Reselect
    }
}

/// 拖动某个手柄后的新矩形：反向拖拽也成立（例如拖左上角越过右下角），
/// 并夹紧到屏幕内；退化到 1×1 也不 panic。
fn resize_by_handle(orig: Rect, h: PickHandle, p: Pos2, bounds: Rect) -> Rect {
    let (mut min, mut max) = (orig.min, orig.max);
    match h {
        PickHandle::Nw => min = p,
        PickHandle::Ne => {
            min.y = p.y;
            max.x = p.x;
        }
        PickHandle::Sw => {
            min.x = p.x;
            max.y = p.y;
        }
        PickHandle::Se => max = p,
        PickHandle::N => min.y = p.y,
        PickHandle::S => max.y = p.y,
        PickHandle::W => min.x = p.x,
        PickHandle::E => max.x = p.x,
    }
    selection_from_drag(min, max, bounds)
}

/// 平移整个选区：保持尺寸不变，整体**夹紧在屏幕内**（贴边即停，不会把选区推出屏幕）。
fn move_selection(orig: Rect, delta: Vec2, bounds: Rect) -> Rect {
    let size = orig.size();
    let mut min = orig.min + delta;
    min.x = min
        .x
        .clamp(bounds.min.x, (bounds.max.x - size.x).max(bounds.min.x));
    min.y = min
        .y
        .clamp(bounds.min.y, (bounds.max.y - size.y).max(bounds.min.y));
    Rect::from_min_size(min, size)
}

/// 窗口内点坐标（逻辑点，左上角为原点）→ 虚拟桌面物理像素。
///
/// 遮罩窗口是"目标显示器上的无边框全屏"，所以窗口左上角就是显示器左上角：
/// 把"窗口内逻辑点"按 `显示器物理尺寸 / 窗口逻辑尺寸` 缩放，再平移显示器原点即得物理坐标。
/// 这个比例就是该显示器的 `pixels_per_point`（DPI 缩放），但用几何比例算出来更稳——
/// 它只依赖 [`MonitorGeo`] 这一份权威数据，窗口实际被系统放到哪里都不影响结果。
/// 全程 `i64`：副屏在主屏左侧时坐标是负的。
///
/// 为什么不用 `ViewportInfo::inner_rect` 拼原点：那个值在"窗口被系统挪到别的屏"
/// 或 DPI 不一致时会给出与 `EnumDisplayMonitors` 对不上的结果（旧版正是因此错位）。
#[derive(Debug, Clone, Copy)]
struct OverlayMapping {
    /// 目标显示器左上角在虚拟桌面中的物理像素坐标
    monitor: (i64, i64),
    /// 窗口内容的逻辑尺寸（点），用来把显示器物理宽高换算成点
    win_size: (f64, f64),
    /// 显示器物理像素尺寸
    monitor_size: (i64, i64),
}

impl OverlayMapping {
    fn new(monitor: &MonitorGeo, win_size: (f64, f64)) -> Self {
        Self {
            monitor: (monitor.x as i64, monitor.y as i64),
            win_size,
            monitor_size: (monitor.w as i64, monitor.h as i64),
        }
    }

    /// 每个逻辑点对应多少个物理像素（按 x/y 各自算，避免非等比缩放时错位）
    fn scale(&self) -> (f64, f64) {
        (
            self.monitor_size.0 as f64 / self.win_size.0.max(1.0),
            self.monitor_size.1 as f64 / self.win_size.1.max(1.0),
        )
    }

    /// 窗口内逻辑点 → 虚拟桌面物理像素
    fn to_physical(self, p: Pos2) -> (i64, i64) {
        let (sx, sy) = self.scale();
        (
            self.monitor.0 + (p.x as f64 * sx).round() as i64,
            self.monitor.1 + (p.y as f64 * sy).round() as i64,
        )
    }

    /// 虚拟桌面物理像素 → 窗口内逻辑点（显示二维码/十字线时用）
    fn to_local(self, x: i64, y: i64) -> Pos2 {
        let (sx, sy) = self.scale();
        let sx = if sx.abs() < 1e-9 { 1.0 } else { sx };
        let sy = if sy.abs() < 1e-9 { 1.0 } else { sy };
        Pos2::new(
            ((x - self.monitor.0) as f64 / sx) as f32,
            ((y - self.monitor.1) as f64 / sy) as f32,
        )
    }

    /// 显示器在虚拟桌面里的物理矩形（`(l, t, r, b)`）
    fn bounds(&self) -> (i64, i64, i64, i64) {
        (
            self.monitor.0,
            self.monitor.1,
            self.monitor.0 + self.monitor_size.0,
            self.monitor.1 + self.monitor_size.1,
        )
    }

    /// 冻结帧的 uv 映射。
    ///
    /// 冻结帧是**整张虚拟桌面**的位图，它的左上角对应虚拟桌面原点
    /// （多屏时通常**不是** `(0,0)`，可能是 `(-1920,0)`）。所以要用
    /// `显示器在桌面里的坐标 - 虚拟桌面原点` 才是帧内的像素偏移。
    /// 搞错这一步的后果是"截到了隔壁屏幕的那块画面"，在双屏下肉眼可见。
    fn frame_uv(&self, frame: &Screenshot) -> Rect {
        let (fw, fh) = (frame.width.max(1) as f64, frame.height.max(1) as f64);
        let (l, t, r, b) = self.bounds();
        // 桌面坐标 → 帧内像素
        let fx0 = l - frame.origin.0;
        let fy0 = t - frame.origin.1;
        let fx1 = r - frame.origin.0;
        let fy1 = b - frame.origin.1;
        // 夹紧到帧的范围，防止显示器矩形与截图范围不完全一致时 uv 越界
        let l = fx0.clamp(0, frame.width as i64) as f64 / fw;
        let t = fy0.clamp(0, frame.height as i64) as f64 / fh;
        let r = fx1.clamp(0, frame.width as i64) as f64 / fw;
        let b = fy1.clamp(0, frame.height as i64) as f64 / fh;
        Rect::from_min_max(Pos2::new(l as f32, t as f32), Pos2::new(r as f32, b as f32))
    }
}

/// 把矩形**夹紧到某台显示器**（并按 H.264 要求取偶宽高）。
/// 纯函数：显示器比最小选区还小、或夹不出合法矩形时返回 `None`。
fn clamp_region_to_monitor(region: Region, monitor: &MonitorGeo) -> Option<Region> {
    let (ml, mt, mr, mb) = monitor.bounds();
    if mr - ml < MIN_REGION_SIDE || mb - mt < MIN_REGION_SIDE {
        return None;
    }
    let x0 = (region.x as i64).clamp(ml, mr - MIN_REGION_SIDE);
    let y0 = (region.y as i64).clamp(mt, mb - MIN_REGION_SIDE);
    let x1 = (region.x as i64 + region.width as i64).clamp(x0 + MIN_REGION_SIDE, mr);
    let y1 = (region.y as i64 + region.height as i64).clamp(y0 + MIN_REGION_SIDE, mb);
    let w = ((x1 - x0) & !1) as u32;
    let h = ((y1 - y0) & !1) as u32;
    if w < MIN_REGION_SIDE as u32 || h < MIN_REGION_SIDE as u32 {
        return None;
    }
    Some(Region {
        x: x0 as i32,
        y: y0 as i32,
        width: w,
        height: h,
    })
}

/// **区域模式下换显示器**时的选区处理：把旧选区搬到新显示器坐标系再夹紧；
/// 搬不过来（新屏装不下 / 两屏尺寸不一致）就返回 `None`，调用方据此**清空选区**
/// 并提示"请重新框选"。这样绝不会出现"切了屏但选区还在旧屏上"。
fn region_after_monitor_switch(
    region: Region,
    old: &MonitorGeo,
    new: &MonitorGeo,
) -> Option<Region> {
    let dx = (new.x - old.x) as i64;
    let dy = (new.y - old.y) as i64;
    let moved = Region {
        x: (region.x as i64 + dx).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        y: (region.y as i64 + dy).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        width: region.width,
        height: region.height,
    };
    let (ml, mt, mr, mb) = new.bounds();
    let fits = moved.x as i64 >= ml
        && moved.y as i64 >= mt
        && moved.x as i64 + moved.width as i64 <= mr
        && moved.y as i64 + moved.height as i64 <= mb;
    if !fits {
        return None;
    }
    clamp_region_to_monitor(moved, new)
}

/// 显示器那一行右侧的**状态信息**（纯函数，便于单测）：
/// * 全屏 → `全屏录制：1920x1080`（所选显示器的实际分辨率）
/// * 区域 + 有效选区 → `区域录制：800x600`（`cfg.region` 取偶后的宽高）
/// * 区域 + 还没框选 → `区域录制：未选择`
fn region_info_text(
    mode: CaptureMode,
    monitor: Option<(u32, u32)>,
    region: Region,
    full_prefix: &str,
    region_prefix: &str,
    none_text: &str,
    sep: &str,
) -> String {
    match mode {
        CaptureMode::FullScreen => {
            let (w, h) = monitor.unwrap_or((0, 0));
            format!("{full_prefix}{sep}{w}x{h}")
        }
        CaptureMode::Region => {
            let w = region.width & !1;
            let h = region.height & !1;
            if w < MIN_REGION_SIDE as u32 || h < MIN_REGION_SIDE as u32 {
                format!("{region_prefix}{sep}{none_text}")
            } else {
                format!("{region_prefix}{sep}{w}x{h}")
            }
        }
    }
}

/// 把一次拖拽换算成虚拟桌面物理像素矩形；太小返回 `None`。
///
/// * 反向拖拽（右下 → 左上）自动规范化：取两端的 min/max，结果与正向一致；
/// * 矩形夹紧到显示器矩形，绝不会越出屏幕；
/// * 宽高取偶（H.264 4:2:0 要求），1×1 这种极小选区直接返回 `None` 而不是 panic。
fn region_from_selection(map: &OverlayMapping, a: Pos2, b: Pos2) -> Option<Region> {
    let (ax, ay) = map.to_physical(a);
    let (bx, by) = map.to_physical(b);
    let (min_x, min_y, max_x, max_y) = map.bounds();
    let x0 = ax.min(bx).clamp(min_x, max_x);
    let y0 = ay.min(by).clamp(min_y, max_y);
    let x1 = ax.max(bx).clamp(min_x, max_x);
    let y1 = ay.max(by).clamp(min_y, max_y);
    let w = (x1 - x0) & !1;
    let h = (y1 - y0) & !1;
    if w < MIN_REGION_SIDE || h < MIN_REGION_SIDE {
        return None;
    }
    if x0 < i32::MIN as i64 || x0 > i32::MAX as i64 || y0 < i32::MIN as i64 || y0 > i32::MAX as i64
    {
        return None;
    }
    Some(Region {
        x: x0 as i32,
        y: y0 as i32,
        width: w as u32,
        height: h as u32,
    })
}

// ---------------------------------------------------------------------------
// 桌面冻结帧（GDI）
// ---------------------------------------------------------------------------

/// 抓下来的桌面图像：RGBA8、顶朝下、尺寸 = 虚拟桌面物理像素。
///
/// `origin` = 这块位图左上角在虚拟桌面里的物理像素坐标（多屏时通常是负数），
/// 换 uv 时必须减掉它，否则会截到隔壁屏幕那块画面。
#[derive(Clone)]
struct Screenshot {
    origin: (i64, i64),
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl Screenshot {
    /// 转成 egui 可上传的纹理数据；长度不符时返回 `None`（宁可不显示，也不 panic）。
    fn to_color_image(&self) -> Option<egui::ColorImage> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        if self.rgba.len() != self.width * self.height * 4 {
            return None;
        }
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [self.width, self.height],
            &self.rgba,
        ))
    }
}

/// GDI 句柄的 RAII 守卫：**任何提前 return 都不会泄漏**，也不需要 `unwrap()`。
mod gdi_guard {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{DeleteDC, DeleteObject, ReleaseDC, HBITMAP, HDC, HGDIOBJ};

    /// 屏幕 DC（`GetDC(NULL)` 的产物，必须用 `ReleaseDC` 还）
    pub struct ScreenDc(pub HDC);
    impl Drop for ScreenDc {
        fn drop(&mut self) {
            unsafe {
                let _ = ReleaseDC(Some(HWND(std::ptr::null_mut())), self.0);
            }
        }
    }

    /// 内存 DC（`DeleteDC`）
    pub struct MemDc(pub HDC);
    impl Drop for MemDc {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }

    /// 位图 / 旧选入对象（`DeleteObject`）
    pub struct GdiObject(pub HGDIOBJ);
    impl Drop for GdiObject {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(self.0);
            }
        }
    }

    impl From<HBITMAP> for GdiObject {
        fn from(b: HBITMAP) -> Self {
            Self(HGDIOBJ(b.0))
        }
    }
}

/// 系统错误原文：`{what} failed: 0x… (说明)`。用 `GetLastError` 那套，带系统给的可读文本。
///
/// **诊断串，不上界面**：它只进日志（界面那一句走 i18n 的 `state_failed` / `err_*`），
/// 所以按日志惯例写 ASCII —— 这也是 `ui::i18n_guard` 要求 `main_page.rs` 零中文字面量的原因。
fn last_error_text(what: &str) -> String {
    let e = windows::core::Error::from_thread();
    format!(
        "{what} failed: {} (0x{:08X})",
        e.message(),
        e.code().0 as u32
    )
}

/// 抓取**目标显示器**的静态截图（GDI `BitBlt`，32bpp 顶朝下 BGRA → RGBA）。
///
/// 为什么只抓目标显示器：框选遮罩只铺在 `pk.rank` 指定的那块屏上。整张虚拟桌面
/// 在双 4K 下是 ~66 MB BGRA + 一份 RGBA + 一份 GPU 纹理；只抓本屏直接把峰值砍到 1/2~1/4。
///
/// 为什么走 GDI 而不是复用 DXGI：这里只要"一张静止的图"，GDI 不需要复制会话、
/// 不依赖 D3D 设备，也不引入 OpenH264/ffmpeg 链路；失败时能直接给出 Win32 错误原文。
fn capture_monitor(monitor: &MonitorGeo) -> Result<Screenshot, String> {
    capture_rect(monitor.x, monitor.y, monitor.w as i32, monitor.h as i32)
}

/// 测试/兼容入口：抓主显示器（主显示器在虚拟桌面的 0,0）。
#[cfg(test)]
fn capture_desktop() -> Result<Screenshot, String> {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    capture_rect(0, 0, w, h)
}

fn capture_rect(vx: i32, vy: i32, vw: i32, vh: i32) -> Result<Screenshot, String> {
    if vw <= 0 || vh <= 0 {
        return Err(format!(
            "invalid monitor size: {vw}x{vh} (origin {vx},{vy})"
        ));
    }

    unsafe {
        let screen = gdi_guard::ScreenDc(GetDC(None));
        if screen.0.is_invalid() {
            return Err(last_error_text("GetDC(NULL)"));
        }
        let mem = gdi_guard::MemDc(CreateCompatibleDC(Some(screen.0)));
        if mem.0.is_invalid() {
            return Err(last_error_text("CreateCompatibleDC"));
        }
        let bitmap = CreateCompatibleBitmap(screen.0, vw, vh);
        if bitmap.is_invalid() {
            return Err(last_error_text("CreateCompatibleBitmap"));
        }
        let _bitmap_guard = gdi_guard::GdiObject::from(bitmap);
        let old = SelectObject(mem.0, HGDIOBJ(bitmap.0));
        if old.is_invalid() {
            return Err(last_error_text("SelectObject"));
        }
        let _old_guard = gdi_guard::GdiObject(old);

        // 屏幕 DC 的原点是虚拟桌面左上角；源坐标用显示器在桌面里的物理坐标。
        BitBlt(mem.0, 0, 0, vw, vh, Some(screen.0), vx, vy, SRCCOPY).map_err(|e| {
            format!(
                "BitBlt failed: {} (0x{:08X})",
                e.message(),
                e.code().0 as u32
            )
        })?;

        // 取像素：32bpp、biHeight 为负 = 顶朝下（这样第 0 行就是屏幕最上面一行）
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: vw,
                biHeight: -vh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };
        let stride = (vw as usize) * 4;
        let mut bgra = vec![0u8; stride * vh as usize];
        let lines = GetDIBits(
            mem.0,
            bitmap,
            0,
            vh as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        if lines == 0 {
            return Err(last_error_text("GetDIBits"));
        }
        // 行数不足时只信前面的行（按 stride 截断），避免读到未初始化的尾部
        bgra.truncate(stride * (lines as usize).min(vh as usize));

        Ok(Screenshot {
            origin: (vx as i64, vy as i64),
            width: vw as usize,
            height: vh as usize,
            rgba: bgra_to_rgba(bgra),
        })
    }
}

/// BGRA（GDI 的 32bpp DIB 布局，B 在最低字节）→ RGBA，**原地交换**。
///
/// 以前会再分配一份 `Vec`。4K 下一份就是 33 MB，框选时白白多一次分配+拷贝；
/// 现在直接把 B/R 对调、第 4 字节置 255，返回同一个缓冲区。
fn bgra_to_rgba(mut bgra: Vec<u8>) -> Vec<u8> {
    let n = bgra.len() / 4 * 4;
    bgra.truncate(n);
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        // GDI 的 32bpp DIB 第 4 字节按定义是 0；置成不透明免得整屏透明
        px[3] = 255;
    }
    bgra
}

// ---------------------------------------------------------------------------
// 框选遮罩
// ---------------------------------------------------------------------------

/// 打开遮罩窗口、画出冻结帧并处理拖拽。返回 `Keep` 时下一帧继续画。
fn run_picker(ctx: &egui::Context, pk: &mut Picker, i18n: &I18n) -> PickerEnd {
    let id = egui::ViewportId::from_hash_of(PICKER_SALT);
    // 不透明 + 无边框 + 置顶：整块屏幕由我们自己画（冻结帧 + 压暗），
    // 不依赖系统合成器的逐像素透明——那正是旧版"整屏被遮挡"的根源。
    let builder = egui::ViewportBuilder::default()
        .with_title(i18n.t("pick_region"))
        // 只认 winit 的显示器序号：它会在目标屏幕上直接开成"无边框全屏"
        .with_monitor(pk.rank)
        .with_always_on_top()
        .with_decorations(false)
        .with_resizable(false)
        .with_taskbar(false);
    let mut end = PickerEnd::Keep;
    ctx.show_viewport_immediate(id, builder, |ui, _class| {
        end = draw_overlay(ui, pk, i18n, id);
    });
    end
}

/// 画出遮罩（冻结帧 + 压暗 + 提示）并处理拖拽。
///
/// 关键顺序：**先铺冻结帧**，再压暗选区之外。于是"选区里 = 原像素、选区外 = 暗一档"，
/// 而且不管窗口是不是真的盖在原桌面位置上，用户看到的都是**刚才那张桌面**，
/// 不存在"整屏黑掉/看不到内容"的情形。
fn draw_overlay(
    ui: &mut egui::Ui,
    pk: &mut Picker,
    i18n: &I18n,
    id: egui::ViewportId,
) -> PickerEnd {
    let resp = ui.allocate_response(ui.available_size(), egui::Sense::click_and_drag());
    let area = resp.rect;
    let map = OverlayMapping::new(&pk.monitor, (area.width() as f64, area.height() as f64));

    // 冻结帧：整张虚拟桌面的截图，按 uv 取出本显示器那块铺满窗口。
    // 纹理只在遮罩窗口的第一帧上传一次（之后复用 `TextureHandle`，避免每帧重传几十 MB）。
    if pk.texture.is_none() {
        pk.texture = pk
            .frame
            .as_ref()
            .and_then(|f| f.to_color_image())
            .map(|img| {
                ui.ctx()
                    .load_texture("cap-picker-frame", img, egui::TextureOptions::NEAREST)
            });
    }
    if let (Some(tex), Some(frame)) = (pk.texture.as_ref(), pk.frame.as_ref()) {
        ui.painter()
            .image(tex.id(), area, map.frame_uv(frame), Color32::WHITE);
    } else {
        // 没有截图（抓取失败或尺寸异常）：至少给一块中性底色，界面文字会说明原因
        ui.painter()
            .rect_filled(area, 0.0, Color32::from_rgb(38, 38, 42));
    }

    // 选区：由"锚点 + 当前点"得到（支持反向拖拽，且夹紧在屏幕内）
    let sel = match (pk.anchor, pk.cursor) {
        (Some(a), Some(b)) => Some(selection_from_drag(a, b, area)),
        _ => None,
    };
    let state = if pk.anchor.is_some() {
        if pk.dragging {
            PickState::Dragging
        } else {
            PickState::Adjusting
        }
    } else {
        PickState::Idle
    };

    // ---- 配色（用户要求：**选中的部分变深**，矩形之外保持正常亮度）----
    // Idle 时 alpha = 0：整屏原样显示，一个压暗像素都不画。
    let painter = ui.painter();
    if let Some(r) = sel {
        let alpha = dim_alpha_for(Some(r));
        if alpha > 0 {
            painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, alpha));
            // 1~2px 亮边框（浅色画面也看得清）
            painter.rect_stroke(r, 0.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside);
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(1.0, Color32::from_rgb(37, 99, 235)),
                StrokeKind::Outside,
            );
        }
        // Adjusting 时画出 8 个手柄（四角 + 四边中点）：提示"这里可以拖动缩放"
        if state == PickState::Adjusting {
            for (_, c) in handle_centers(r) {
                let hr = Rect::from_center_size(c, Vec2::splat(HANDLE_SIZE));
                painter.rect_filled(hr, 2.0, Color32::WHITE);
                painter.rect_stroke(
                    hr,
                    2.0,
                    Stroke::new(1.0, Color32::from_rgb(37, 99, 235)),
                    StrokeKind::Inside,
                );
            }
        }
    }

    // ---- 十字辅助线 + 坐标提示（跟随鼠标，并吸附到物理像素网格） ----
    if let Some(p) = pk.cursor {
        // 先把鼠标位置转成物理像素，再转回来：十字线因此落在**整数物理像素**上，
        // 用户看到的线就是最终会被写进 cfg.region 的那条边界。
        let (px, py) = map.to_physical(p);
        let g = map.to_local(px, py);
        let guide = Color32::from_rgba_unmultiplied(120, 220, 255, 110);
        painter.line_segment(
            [Pos2::new(area.min.x, g.y), Pos2::new(area.max.x, g.y)],
            Stroke::new(1.0, guide),
        );
        painter.line_segment(
            [Pos2::new(g.x, area.min.y), Pos2::new(g.x, area.max.y)],
            Stroke::new(1.0, guide),
        );
        // 交点画一个小方框，明确"这一个物理像素"的位置
        painter.rect_stroke(
            Rect::from_center_size(g, Vec2::splat(7.0)),
            0.0,
            Stroke::new(1.0, Color32::from_rgb(255, 120, 120)),
            StrokeKind::Inside,
        );
        painter.text(
            Pos2::new(g.x + 10.0, g.y + 8.0),
            Align2::LEFT_TOP,
            format!("{px}, {py}"),
            FontId::proportional(13.0),
            Color32::from_gray(240),
        );
    }

    // ---- 文字提示 ----
    let font = FontId::proportional(20.0);
    painter.text(
        Pos2::new(area.center().x, area.min.y + 28.0),
        Align2::CENTER_TOP,
        i18n.tf("pick_on_monitor", &[&pk.label]),
        font.clone(),
        Color32::WHITE,
    );
    painter.text(
        Pos2::new(area.center().x, area.min.y + 60.0),
        Align2::CENTER_TOP,
        i18n.t("pick_hint"),
        FontId::proportional(15.0),
        Color32::from_gray(230),
    );
    if pk.frame.is_none() {
        painter.text(
            Pos2::new(area.center().x, area.min.y + 90.0),
            Align2::CENTER_TOP,
            i18n.tf("pick_no_screenshot", &[&i18n.t("err_pick_capture_failed")]),
            FontId::proportional(14.0),
            Color32::from_rgb(255, 190, 120),
        );
    }
    if let Some(r) = sel {
        // 尺寸标签用**物理像素**（就是要写进 cfg.region 的值），用户看到的和录到的必须一致。
        // 位置：**选区正中间**（用户要求），带一层半透明底衬 —— 浅色/深色画面上都读得清。
        let (x0, y0) = map.to_physical(r.left_top());
        let (x1, y1) = map.to_physical(r.right_bottom());
        let (w, h) = ((x1 - x0).unsigned_abs(), (y1 - y0).unsigned_abs());
        let size_text = i18n.tf("pick_size", &[&w, &h]);
        let galley = painter.layout_no_wrap(size_text, font.clone(), Color32::WHITE);
        let pad = Vec2::new(10.0, 5.0);
        let chip = Rect::from_center_size(r.center(), galley.size() + pad * 2.0);
        painter.rect_filled(chip, 5.0, Color32::from_rgba_unmultiplied(0, 0, 0, 150));
        painter.galley(chip.center() - galley.size() / 2.0, galley, Color32::WHITE);
        painter.text(
            r.center() + Vec2::new(0.0, 22.0),
            Align2::CENTER_CENTER,
            format!("{x0}, {y0}"),
            FontId::proportional(13.0),
            Color32::from_gray(235),
        );
    }
    // 「确定」按钮：下面用**真正的 egui 控件**注册（不是纯绘制），否则点击会被整屏
    // 拖拽交互吞掉 —— 这正是用户报的"点确定无反应"。
    let confirm_rect = sel.and_then(|r| confirm_button_rect(r, area));
    if let Some(btn) = confirm_rect {
        let hovered = resp
            .hover_pos()
            .map(|p| btn.expand(2.0).contains(p))
            .unwrap_or(false);
        painter.rect_filled(
            btn,
            6.0,
            if hovered {
                Color32::from_rgb(29, 78, 216)
            } else {
                Color32::from_rgb(37, 99, 235)
            },
        );
        painter.text(
            btn.center(),
            Align2::CENTER_CENTER,
            i18n.t("pick_confirm"),
            FontId::proportional(15.0),
            Color32::WHITE,
        );
    }
    if pk.too_small {
        painter.text(
            Pos2::new(area.center().x, area.max.y - 44.0),
            Align2::CENTER_BOTTOM,
            i18n.t("err_region_too_small"),
            FontId::proportional(16.0),
            Color32::from_rgb(255, 150, 150),
        );
    }
    painter.text(
        Pos2::new(area.center().x, area.max.y - 16.0),
        Align2::CENTER_BOTTOM,
        i18n.t("pick_cancel_hint"),
        FontId::proportional(14.0),
        Color32::from_gray(210),
    );

    // ---- 交互：**按"按下/松开"事件驱动**，不依赖 egui 的 drag 启发式 ----
    // 用户报的 bug（点「确定」无反应）根因有两条，这里都修掉：
    // ① 按钮以前只被"画"出来，没注册成交互控件 → 点击被整屏 `click_and_drag` 吃掉；
    //    现在先 `ui.interact()` 注册一个**真正的按钮控件**，并且指针在按钮上时不启动任何拖拽。
    // ② 以前靠 `drag_started_by` / `dragged_by` 判断——**egui 只在指针移动后**才触发 drag 事件，
    //    所以"按下-松开不移动"的标准点击既不会更新 `cursor` 也拿不到按下标记，
    //    确认分支永远不成立（这正是"点了没反应、不返回"）。现在改用
    //    `pointer.primary_pressed()/primary_released()`：**按下那一刻**就定锚点/记标记。
    let mut confirmed = false;
    if let Some(btn) = confirm_rect {
        let btn_resp = ui.interact(btn, ui.id().with("pick-confirm"), egui::Sense::click());
        if btn_resp.clicked() {
            confirmed = true;
        }
    }
    let pointer_on_confirm = ui
        .ctx()
        .pointer_latest_pos()
        .or_else(|| resp.hover_pos())
        .map(|p| {
            confirm_rect
                .map(|b| b.expand(2.0).contains(p))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let (pressed, released) =
        ui.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_released()));
    let pointer_pos = resp
        .interact_pointer_pos()
        .or_else(|| ui.ctx().pointer_latest_pos());

    if pressed {
        if let Some(p) = pointer_pos {
            let p = p.clamp(area.min, area.max);
            if pointer_on_confirm {
                // 按在「确定」上：只记标记，**不启动任何拖拽/选区处理**
                pk.press_on_confirm = true;
            } else {
                match picker_hit(p, sel, area) {
                    PickHit::Confirm => {}
                    PickHit::Handle(h) => {
                        pk.drag_mode = Some(PickDrag::Handle { h, orig: sel });
                    }
                    PickHit::Move => {
                        if let Some(orig) = sel {
                            pk.drag_mode = Some(PickDrag::Move {
                                grab: p,
                                orig: Some(orig),
                            });
                        }
                    }
                    PickHit::Reselect => {
                        // 矩形外部按下 = **重新点选**（丢弃旧矩形，从新的锚点开始）
                        pk.anchor = Some(p);
                        pk.cursor = Some(p);
                        pk.too_small = false;
                        pk.drag_mode = Some(PickDrag::NewSelection);
                    }
                }
                pk.dragging = true;
            }
        }
    }
    // 拖动中：更新当前点（并让手柄缩放/整体移动生效）。
    // 用"指针是否按着"而不是 `dragged_by`：轻微抖动也算拖动，手感更稳。
    if pk.dragging && !pointer_on_confirm {
        if let Some(p) = ui.ctx().pointer_latest_pos() {
            let p = p.clamp(area.min, area.max);
            pk.cursor = Some(p);
            match pk.drag_mode {
                Some(PickDrag::NewSelection) => {}
                Some(PickDrag::Handle {
                    h,
                    orig: Some(orig),
                }) => {
                    let new = resize_by_handle(orig, h, p, area);
                    pk.anchor = Some(new.min);
                    pk.cursor = Some(new.max);
                }
                Some(PickDrag::Move {
                    grab,
                    orig: Some(orig),
                }) => {
                    let new = move_selection(orig, p - grab, area);
                    pk.anchor = Some(new.min);
                    pk.cursor = Some(new.max);
                }
                // `orig == None`（按下那一刻没有选区）与 `None`（没在拖）都什么也不做。
                Some(PickDrag::Handle { orig: None, .. })
                | Some(PickDrag::Move { orig: None, .. })
                | None => {}
            }
        }
    }
    if released {
        // 「确定」：按下与松开**都**落在按钮里才算点中
        if pk.press_on_confirm && pointer_on_confirm {
            confirmed = true;
        }
        pk.press_on_confirm = false;
        pk.dragging = false;
        pk.drag_mode = None;
    }
    if confirmed {
        let sel = sel.or_else(|| match (pk.anchor, pk.cursor) {
            (Some(a), Some(b)) => Some(selection_from_drag(a, b, area)),
            _ => None,
        });
        match sel.and_then(|r| region_from_selection(&map, r.min, r.max)) {
            Some(reg) => {
                pk.finished = true;
                ui.ctx()
                    .send_viewport_cmd_to(id, egui::ViewportCommand::Close);
                return PickerEnd::Picked(reg);
            }
            None => pk.too_small = true,
        }
    }
    if released {
        // 松开后**停留在 Adjusting**：用户可以继续拖手柄/移动/重选，不自动返回
        if let Some(r) = sel {
            pk.too_small = r.width() < 1.0 || r.height() < 1.0;
        }
    }
    // 右键 = 取消（保留原矩形），跟截图工具一致
    if resp.clicked_by(egui::PointerButton::Secondary) {
        ui.ctx()
            .send_viewport_cmd_to(id, egui::ViewportCommand::Close);
        return PickerEnd::Cancelled;
    }

    // Esc 或点窗口关闭 = 取消（保留原矩形）
    let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape))
        || ui.input(|i| i.viewport().close_requested());
    if cancel {
        ui.ctx()
            .send_viewport_cmd_to(id, egui::ViewportCommand::Close);
        return PickerEnd::Cancelled;
    }

    ui.ctx().request_repaint();
    PickerEnd::Keep
}

// ---------------------------------------------------------------------------
// 显示器枚举（Win32）
// ---------------------------------------------------------------------------

/// 用 `EnumDisplayMonitors` 枚举显示器物理矩形。
/// 顺序与 `windows-capture`、winit 的 `available_monitors()` 一致，因此可以直接当
/// `ViewportBuilder::with_monitor` 的序号用。
fn enumerate_monitor_geo() -> Result<Vec<MonitorGeo>, String> {
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<MonitorGeo>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            out.push(MonitorGeo {
                x: r.left,
                y: r.top,
                w: (r.right - r.left).max(0) as u32,
                h: (r.bottom - r.top).max(0) as u32,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        TRUE
    }

    let mut out: Vec<MonitorGeo> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(std::ptr::from_mut(&mut out) as isize),
        )
    };
    if !ok.as_bool() {
        return Err("EnumDisplayMonitors failed".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// cfg.monitor_index ↔ 显示器序号
// ---------------------------------------------------------------------------

/// `Recorder` 里的约定是"0 = 主显示器，其余 = `Monitor::from_index(n)` = 枚举顺序 [n-1]"。
/// 这里统一写成 `序号 + 1`，避免主显示器不在第一位时出现两个值指向同一块屏。
fn rank_of_cfg(index: usize) -> usize {
    index.saturating_sub(1)
}

fn cfg_of_rank(rank: usize) -> usize {
    rank + 1
}

// ---------------------------------------------------------------------------
// 与设置页共用的部件
// ---------------------------------------------------------------------------

/// 重新枚举麦克风。枚举失败是常态（台式机没插麦、被禁用），所以只把原因交给界面显示。
pub(crate) fn refresh_mic_devices(
    devices: &mut Option<Vec<AudioDevice>>,
    err: &mut Option<String>,
) {
    match list_capture_devices() {
        Ok(list) => {
            *devices = Some(list);
            *err = None;
        }
        Err(e) => {
            *devices = None;
            *err = Some(e.to_string());
        }
    }
}

/// 麦克风下拉（None = 系统默认设备）。
///
/// 返回 `(新选择, 是否点了刷新)`：`None` 表示没变——这样调用方不必把 `&mut Option<String>`
/// 借给闭包，避免嵌套 `show_ui` 里的可变借用打架。
///
/// `combo_w`：下拉宽度。录制页的音源行左边还排着「音源（可多选）」标签 + 两个复选框
/// （P38 并成一行），所以那一行用窄一档（220）；别处仍可用宽档（260）。
pub(crate) fn mic_device_combo(
    ui: &mut egui::Ui,
    devices: Option<&[AudioDevice]>,
    current: Option<&str>,
    combo_w: f32,
    t: &dyn Fn(&str) -> String,
) -> (Option<Option<String>>, bool) {
    let mut sel: Option<String> = current.map(str::to_string);
    let mut refresh = false;
    ui.horizontal(|ui| {
        let text = sel
            .as_ref()
            .and_then(|id| devices.and_then(|v| v.iter().find(|d| &d.id == id)))
            .map(|d| d.label())
            .unwrap_or_else(|| t("mic_default"));
        egui::ComboBox::from_id_salt("mic_device")
            .selected_text(text)
            .width(combo_w)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut sel, None, t("mic_default"));
                if let Some(list) = devices {
                    for d in list {
                        ui.selectable_value(&mut sel, Some(d.id.clone()), d.label());
                    }
                }
            });
        refresh = ui.small_button(t("btn_refresh")).clicked();
    });
    let changed = sel.as_deref() != current;
    (if changed { Some(sel) } else { None }, refresh)
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 录制状态 → 文案与颜色（失败必须醒目）
fn state_text(i18n: &I18n, state: &RecorderState) -> (String, Color32) {
    match state {
        RecorderState::Recording => (
            i18n.t("state_recording").to_string(),
            // P40：红/琥珀/绿不再写死字面量，跟当前主题走（深色下要提亮才读得清）
            crate::ui::theme::danger(),
        ),
        RecorderState::Stopping => (
            i18n.t("state_stopping").to_string(),
            crate::ui::theme::warn_fg(),
        ),
        RecorderState::Finalizing => (
            i18n.t("state_finalizing").to_string(),
            crate::ui::theme::warn_fg(),
        ),
        RecorderState::Finished => (
            i18n.t("rec_finished").to_string(),
            crate::ui::theme::ok_fg(),
        ),
        // 分隔符走 i18n（中文 `：`／英文 `: `）：这里**不能**写死全角冒号 ——
        // 它是**界面可见**文字，必须跟着语言走（P1 的 `ui::i18n_guard` 会当场拦下硬编码）。
        RecorderState::Failed(msg) => (
            format!("{}{}{msg}", i18n.t("state_failed"), i18n.t("label_sep")),
            crate::ui::theme::err_fg(),
        ),
    }
}

fn stats_grid(ui: &mut egui::Ui, s: &RecorderStats, t: &dyn Fn(&str) -> String) {
    // 两列"标签 : 值"，等宽数字，成组排列（不再挤成一行）
    //
    // P51：这里**每帧都在跑**（录制中刷新统计），所以两处分配都省掉了：
    // 1. 原来是 `Vec<(String, String)>` —— 7 行也要一次堆分配；改成定长数组，栈上就够。
    // 2. 原来还要 `.map(|(l, v)| (l.as_str(), v.clone()))` 再 `collect()` 一次 ——
    //    纯粹是为了把 `String` 退化成 `&str` 去凑 `stat_grid` 的老签名，
    //    等于每帧白白 clone 7 个 String + 再分配一个 Vec。
    //    `stat_grid` 已放宽成 `L: AsRef<str>`，直接传切片即可。
    let rows: [(String, String); 7] = [
        (t("stat_frames"), s.video_frames),
        (t("stat_dropped"), s.video_dropped),
        (t("stat_keyframes"), s.keyframes),
        (t("stat_audio_frames"), s.audio_frames),
        (t("stat_no_change"), s.no_change),
        (t("stat_access_lost"), s.access_lost),
        (t("stat_silence"), s.silence_padded_frames),
    ]
    .map(|(label, value)| (label, value.to_string()));
    theme::stat_grid(ui, &rows[..]);
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// 录制时的两个落点（纯函数，便于理解和测试）：
/// * **流文件**（分片 MP4，录制过程中持续写）→ `cache_dir`（`<exe>\Cache\`）；
/// * **导出产物**（停止时 remux 出来的普通 MP4）→ `export_dir`（用户设置的输出目录）。
///   两边**同名同基名**：分处两个目录不会冲突，用户一眼能对上"哪条流对应哪个导出"。
fn record_paths(cache_dir: &Path, export_dir: &Path, stamp: &str) -> (PathBuf, PathBuf) {
    // 唯一名：同一秒内第二次开始、或系统时钟回拨，都不能覆盖上一段的分片
    //（Cache 里的分片是导出失败时唯一的找回副本）。
    // 不再在 UI 线程对最多 1000 个路径做 `exists()`（网络盘会卡界面）；
    // 进程内原子计数 + 纳秒足以保证唯一性。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("rec-{stamp}-{}-{nanos}-{n}.mp4", std::process::id());
    let recordings = crate::cache::recordings_dir(cache_dir);
    (recordings.join(name.clone()), export_dir.join(name))
}

/// 从配置算出本次录制的两个落点（会解析目录、必要时建目录）。
fn record_paths_checked(
    cfg: &Config,
    cache_dir: Option<&Path>,
) -> Result<(PathBuf, PathBuf), String> {
    let export_dir = crate::config::resolve_against_exe_dir(&cfg.output_dir).map_err(|e| {
        format!(
            "cannot resolve the output directory (configured {:?} is invalid: {e})",
            cfg.output_dir
        )
    })?;
    // 优先用外壳注入的 Cache 根目录：已经探测过一次，且尊重 `--config-dir`。
    // 只有独立调用/测试没有注入时才走旧的探测回退链。
    let cache_dir = match cache_dir {
        Some(d) => d.to_path_buf(),
        None => {
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            let cfg_dir = crate::config::ConfigStore::detect(None)
                .path()
                .parent()
                .map(|d| d.to_path_buf());
            crate::cache::resolve_cache_dir(exe_dir.as_deref(), cfg_dir.as_deref(), &|p: &Path| {
                crate::config::probe_dir_writable_timeout(p).is_ok()
            })
        }
    };
    Ok(record_paths(&cache_dir, &export_dir, &utc_stamp()))
}

fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    stamp_from_secs(secs)
}

fn stamp_from_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Unix 天数 → (年, 月, 日)。Howard Hinnant 的 `civil_from_days`，全程整数、无时区依赖。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 路径太长时中间省略，避免把窗口撑变形
fn elide(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s.chars().skip(count - max / 2).collect();
    format!("{head}…{tail}")
}

fn pick_folder(start: &Path) -> Result<Option<PathBuf>, String> {
    let mut dlg = rfd::FileDialog::new();
    if start.is_dir() {
        dlg = dlg.set_directory(start);
    }
    Ok(dlg.pick_folder())
}

// ---------------------------------------------------------------------------
// 测试（纯逻辑部分：时间戳、压暗、选区换算、多屏/DPI 坐标）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个"显示器 1920×1080、窗口逻辑尺寸由 scale 决定"的映射
    fn map_of(monitor: (i64, i64), size: (i64, i64), win: (f64, f64)) -> OverlayMapping {
        OverlayMapping {
            monitor,
            win_size: win,
            monitor_size: size,
        }
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_675), (2023, 11, 14));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29), "闰年 2 月 29 日");
    }

    #[test]
    fn stamp_uses_utc_and_is_sortable() {
        assert_eq!(stamp_from_secs(0), "19700101-000000");
        assert_eq!(stamp_from_secs(1_700_000_000), "20231114-221320");
    }

    // ---- ① 无选区不压暗；有选区时**选中的部分变深** ----

    #[test]
    fn dim_alpha_is_zero_without_a_selection() {
        // 关键回归：**还没有选区时一个字都不压暗**（旧实现整屏半透明黑 →
        // 用户"看不到桌面、找不到第一点"）。None 与退化矩形都必须是 0。
        assert_eq!(dim_alpha_for(None), 0);
        assert_eq!(
            dim_alpha_for(Some(Rect::from_min_max(Pos2::ZERO, Pos2::ZERO))),
            0,
            "零尺寸矩形不算选区，也不压暗"
        );
        let sel = Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(300.0, 200.0));
        assert_eq!(dim_alpha_for(Some(sel)), SELECT_DIM_ALPHA);
        const _: () = assert!(SELECT_DIM_ALPHA > 0);
    }

    // ---- ①b 拖拽规范化：反向拖拽 / 越界夹紧 / 1×1 不 panic ----

    #[test]
    fn selection_from_drag_normalizes_clamps_and_never_panics() {
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(640.0, 480.0));
        // 反向拖拽（从右下往左上）与正向拖拽得到**同一个矩形**
        let fwd = selection_from_drag(Pos2::new(100.0, 120.0), Pos2::new(400.0, 300.0), area);
        let rev = selection_from_drag(Pos2::new(400.0, 300.0), Pos2::new(100.0, 120.0), area);
        assert_eq!(fwd, rev);
        assert_eq!(fwd.min, Pos2::new(100.0, 120.0));
        assert_eq!(fwd.max, Pos2::new(400.0, 300.0));
        // 越界夹紧到屏幕内
        let clamped = selection_from_drag(Pos2::new(-50.0, -50.0), Pos2::new(9999.0, 9999.0), area);
        assert_eq!(clamped, area);
        // 1×1（甚至 0×0）不 panic，也不产生非法矩形
        let dot = selection_from_drag(Pos2::new(10.0, 10.0), Pos2::new(10.0, 10.0), area);
        assert_eq!(
            dot,
            Rect::from_min_max(Pos2::new(10.0, 10.0), Pos2::new(10.0, 10.0))
        );
        assert_eq!(dim_alpha_for(Some(dot)), 0, "尚未拖出面积时也不压暗");
    }

    // ---- ①c 命中优先级：确定 > 手柄 > 内部(移动) > 外部(重选) ----

    #[test]
    fn picker_hit_priority_is_confirm_then_handle_then_inside_then_outside() {
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 800.0));
        let sel = Rect::from_min_max(Pos2::new(300.0, 200.0), Pos2::new(700.0, 500.0));

        // ① 还没选区：任何按下都是"重新点选"
        assert_eq!(
            picker_hit(Pos2::new(10.0, 10.0), None, area),
            PickHit::Reselect
        );

        // ② 确定按钮优先（它就贴在右下角外侧，必须比"外部=重选"先判）
        let btn = confirm_button_rect(sel, area).expect("有效矩形应显示确定按钮");
        assert_eq!(picker_hit(btn.center(), Some(sel), area), PickHit::Confirm);

        // ③ 8 个手柄都能命中（且**优先于 Move**：手柄点在矩形边界上，也落在内部）
        for (h, c) in handle_centers(sel) {
            assert_eq!(
                picker_hit(c, Some(sel), area),
                PickHit::Handle(h),
                "{h:?} 手柄应被命中"
            );
        }

        // ④ 矩形内部（远离手柄与按钮）→ 移动
        assert_eq!(
            picker_hit(Pos2::new(500.0, 350.0), Some(sel), area),
            PickHit::Move
        );

        // ⑤ 矩形外部 → 重新点选
        assert_eq!(
            picker_hit(Pos2::new(20.0, 20.0), Some(sel), area),
            PickHit::Reselect
        );

        // ⑥ 贴边的选区：确定按钮改到**内侧**右下角，仍然可点（不会跑出屏幕）
        let edge = Rect::from_min_max(Pos2::new(600.0, 500.0), Pos2::new(1000.0, 800.0));
        let btn2 = confirm_button_rect(edge, area).expect("贴边也要有确定按钮");
        assert!(area.contains_rect(btn2), "确定按钮必须留在屏幕内");
        assert_eq!(
            picker_hit(btn2.center(), Some(edge), area),
            PickHit::Confirm
        );

        // ⑦ 太小的矩形不显示确定按钮（避免误点）
        let tiny = Rect::from_min_max(Pos2::new(10.0, 10.0), Pos2::new(13.0, 12.0));
        assert!(confirm_button_rect(tiny, area).is_none());
    }

    /// 手柄缩放与整体移动：反向拖过头、贴边都不会产生非法矩形。
    #[test]
    fn resize_and_move_keep_the_selection_legal() {
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 800.0));
        let sel = Rect::from_min_max(Pos2::new(300.0, 200.0), Pos2::new(700.0, 500.0));

        // 拖右下角到左上角另一侧 → 矩形**翻转**（拖过对角点后仍取两个角的包围盒）
        let flipped = resize_by_handle(sel, PickHandle::Se, Pos2::new(200.0, 100.0), area);
        assert_eq!(
            flipped.min,
            Pos2::new(200.0, 100.0),
            "被拖到的那一点成为新的角"
        );
        assert_eq!(flipped.max, Pos2::new(300.0, 200.0), "未动的对角点保持不变");
        assert!(flipped.width() > 0.0 && flipped.height() > 0.0);

        // 拖左上角到屏幕外 → 夹紧到屏幕内
        let clamped = resize_by_handle(sel, PickHandle::Nw, Pos2::new(-99.0, -99.0), area);
        assert_eq!(clamped.min, area.min);

        // 整体移动：尺寸不变，且贴边即停（不会被推出屏幕）
        let moved = move_selection(sel, Vec2::new(50.0, -30.0), area);
        assert_eq!(moved.size(), sel.size());
        assert_eq!(moved.min, Pos2::new(350.0, 170.0));
        let pushed = move_selection(sel, Vec2::new(9999.0, 9999.0), area);
        assert_eq!(pushed.size(), sel.size());
        assert!(
            area.contains_rect(pushed),
            "移动后必须仍在屏幕内：{pushed:?}"
        );
    }

    // ---- ② 反向拖拽规范化 + ④ 夹紧 + ⑤ 1×1 不 panic ----

    #[test]
    fn region_from_selection_normalizes_and_clamps() {
        let map = map_of((0, 0), (1920, 1080), (1920.0, 1080.0));
        // 正向拖拽
        let fwd = region_from_selection(&map, Pos2::new(100.0, 100.0), Pos2::new(400.0, 300.0))
            .expect("正向拖拽应有效");
        assert_eq!((fwd.x, fwd.y, fwd.width, fwd.height), (100, 100, 300, 200));
        // 反向拖拽（右下 → 左上）必须得到同一个矩形
        let back = region_from_selection(&map, Pos2::new(400.0, 300.0), Pos2::new(100.0, 100.0))
            .expect("反向拖拽也应有效");
        assert_eq!(back, fwd, "反向拖拽必须规范化成同一个矩形");

        // 超出屏幕：夹紧到显示器矩形，绝不会出现"选区在屏幕之外"
        let big =
            region_from_selection(&map, Pos2::new(-5000.0, -5000.0), Pos2::new(9000.0, 9000.0))
                .expect("应夹到屏幕范围内");
        assert_eq!((big.x, big.y), (0, 0));
        assert_eq!((big.width, big.height), (1920, 1080));

        // 1×1 这种极小选区：返回 None（界面显示"选区太小"），绝不 panic
        assert!(
            region_from_selection(&map, Pos2::new(10.0, 10.0), Pos2::new(11.0, 11.0)).is_none()
        );
        assert!(
            region_from_selection(&map, Pos2::new(10.0, 10.0), Pos2::new(10.0, 10.0)).is_none()
        );
        // 恰好到最小边（16）：奇数宽会被取偶到 16，仍应有效
        let min = region_from_selection(&map, Pos2::new(10.0, 10.0), Pos2::new(26.0, 26.0))
            .expect("16×16 应有效");
        assert_eq!((min.width, min.height), (16, 16));
        // 宽高一律取偶（H.264 4:2:0 要求）
        let odd = region_from_selection(&map, Pos2::new(0.0, 0.0), Pos2::new(101.0, 101.0))
            .expect("101×101 应有效并取偶");
        assert_eq!((odd.width, odd.height), (100, 100));
    }

    // ---- ③b 点「确定」必须真的提交并关闭遮罩（用户报的"点了没反应"回归测试）----

    /// 造一个可测的遮罩状态（无截图 → 走中性底色那条安全路径）。
    fn test_picker() -> Picker {
        Picker {
            rank: 0,
            monitor: MonitorGeo {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
                primary: true,
            },
            label: "测试显示器".to_string(),
            anchor: None,
            cursor: None,
            dragging: false,
            drag_mode: None,
            finished: false,
            press_on_confirm: false,
            too_small: false,
            frame: None,
            texture: None,
        }
    }

    /// 跑一帧遮罩绘制 + 交互（headless：直接 `Context::run_ui` 驱动 `draw_overlay`）。
    fn picker_frame(
        ctx: &egui::Context,
        pk: &mut Picker,
        i18n: &I18n,
        events: Vec<egui::Event>,
    ) -> PickerEnd {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut end = PickerEnd::Keep;
        // `drop_without_applying_deltas()` 与 editor 的 headless Harness 一致：
        // 没有渲染器时不要把纹理增量真的应用/释放（否则 epaint 的 TextureManager
        // 会因为"释放了从未分配过的纹理"而 panic）。
        ctx.run_ui(raw, |ui| {
            end = draw_overlay(ui, pk, i18n, egui::ViewportId::ROOT);
        })
        .drop_without_applying_deltas();
        end
    }

    fn pointer_moved(p: Pos2) -> egui::Event {
        egui::Event::PointerMoved(p)
    }

    fn pointer_button(p: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// **用户报的 bug**：框好区域后点「确定」没反应、遮罩不返回。
    ///
    /// 这条测试**完整走一遍事件序列**（拖出矩形 → 松开 → 指针移到确定按钮 → 按下 → 松开），
    /// 断言 ① 命中判定认出"确定" ② 返回的 `Region` 就是选区换算出来的那个
    ///（主页面拿到它就会写回 `cfg.region`，见 `apply_picked_region`）
    /// ③ 遮罩状态机进入 `finished`（`PickerEnd::Picked` → 关闭 viewport）。
    ///
    /// 复现旧 bug 的方式（已实测）：把「确定」改回"纯绘制 + 靠 `cursor` 判断松开点"
    /// 的老逻辑后，这一条会失败在 ③（返回 `Keep`，`finished == false`）。
    #[test]
    fn headless_click_on_confirm_finishes_the_picker() {
        let ctx = egui::Context::default();
        let i18n = I18n::new("zh-CN");
        let mut pk = test_picker();
        // 显示器 800×600 物理像素，窗口也是 800×600 逻辑点 → 1:1 映射，换算好算
        let a = Pos2::new(100.0, 80.0);
        let b = Pos2::new(500.0, 400.0);

        // ① 拖出矩形：按下 A → 移到 B → 松开（松开**不返回**，停在 Adjusting）
        picker_frame(&ctx, &mut pk, &i18n, vec![pointer_moved(a)]);
        picker_frame(&ctx, &mut pk, &i18n, vec![pointer_button(a, true)]);
        picker_frame(&ctx, &mut pk, &i18n, vec![pointer_moved(b)]);
        let end = picker_frame(&ctx, &mut pk, &i18n, vec![pointer_button(b, false)]);
        assert!(
            matches!(end, PickerEnd::Keep),
            "松开鼠标**不能**立刻返回（用户要求继续可调整）"
        );
        assert!(!pk.finished, "松开时还没确认");
        let sel = Rect::from_min_max(a, b);

        // ② 确定按钮在选区**下方水平居中**
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let btn = confirm_button_rect(sel, area).expect("有效选区应显示确定按钮");
        assert!(
            (btn.center().x - sel.center().x).abs() <= 0.5,
            "确定按钮必须水平居中于选区：按钮中心 {:?} vs 选区中心 {:?}",
            btn.center().x,
            sel.center().x
        );
        assert!(btn.min.y > sel.max.y, "确定按钮必须在选区下方");
        assert_eq!(
            picker_hit(btn.center(), Some(sel), area),
            PickHit::Confirm,
            "命中判定要认出确定按钮（且**优先于**手柄/移动/重选）"
        );

        // ③ 指针移到按钮上 → 按下 → 松开：这一下必须返回 Picked
        let c = btn.center();
        picker_frame(&ctx, &mut pk, &i18n, vec![pointer_moved(c)]);
        picker_frame(&ctx, &mut pk, &i18n, vec![pointer_button(c, true)]);
        let end = picker_frame(&ctx, &mut pk, &i18n, vec![pointer_button(c, false)]);
        match end {
            PickerEnd::Picked(reg) => {
                // 1:1 映射 + 宽高取偶：期望就是 (100,80)-(500,400)
                assert_eq!((reg.x, reg.y), (100, 80));
                assert_eq!((reg.width, reg.height), (400, 320));
            }
            other => panic!(
                "点「确定」应当提交并关闭遮罩（PickerEnd::Picked），实际 {:?}（finished={}）",
                match other {
                    PickerEnd::Keep => "Keep",
                    PickerEnd::Cancelled => "Cancelled",
                    PickerEnd::Picked(_) => "Picked",
                },
                pk.finished
            ),
        }
        assert!(pk.finished, "确认后状态机必须标记为已确认");
    }

    // ---- ②b 显示器行：状态信息文案 + 换屏后的选区处理（用户要求重做的那一片）----

    /// `region_info_text`：三种分支的文案（含奇数宽高取偶）。
    ///
    /// 分隔符由调用方给（i18n 的 `label_sep`）：中文是 `：`、英文是 `: ` ——
    /// 这也是为什么源码里**不能**写死全角冒号（P1 的 i18n 防回归扫描器会当场拦下）。
    #[test]
    fn region_info_text_covers_full_region_and_unset() {
        let mut cfg = Config {
            region: Region::default(),
            ..Config::default()
        };
        // ① 全屏 → 用所选显示器的实际分辨率
        assert_eq!(
            region_info_text(
                CaptureMode::FullScreen,
                Some((1920, 1080)),
                cfg.region,
                "全屏录制",
                "区域录制",
                "未选择",
                "："
            ),
            "全屏录制：1920x1080"
        );
        // 显示器未知时的兜底（不让界面出现空洞）
        assert_eq!(
            region_info_text(
                CaptureMode::FullScreen,
                None,
                cfg.region,
                "全屏录制",
                "区域录制",
                "未选择",
                ": "
            ),
            "全屏录制: 0x0"
        );
        // ② 区域 + 有效选区：宽高**取偶**
        cfg.region = Region {
            x: 10,
            y: 20,
            width: 801,
            height: 601,
        };
        assert_eq!(
            region_info_text(
                CaptureMode::Region,
                Some((1920, 1080)),
                cfg.region,
                "Full Screen",
                "Custom Region",
                "not selected",
                ": "
            ),
            "Custom Region: 800x600"
        );
        // ③ 区域 + 还没框选（0 尺寸 / 小于最小边）
        cfg.region = Region {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(
            region_info_text(
                CaptureMode::Region,
                Some((1920, 1080)),
                cfg.region,
                "全屏录制",
                "区域录制",
                "未选择",
                "："
            ),
            "区域录制：未选择"
        );
        cfg.region = Region {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        assert_eq!(
            region_info_text(
                CaptureMode::Region,
                Some((1920, 1080)),
                cfg.region,
                "全屏录制",
                "区域录制",
                "未选择",
                "："
            ),
            "区域录制：未选择",
            "小于最小边（{MIN_REGION_SIDE}）也算未选择"
        );
    }

    /// 换显示器：装得下的选区跟着搬过去并夹紧；装不下就返回 `None`（调用方清空 + 提示重选）。
    #[test]
    fn monitor_switch_moves_or_clears_the_region() {
        let left = MonitorGeo {
            x: -1920,
            y: 0,
            w: 1920,
            h: 1080,
            primary: false,
        };
        let right = MonitorGeo {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            primary: true,
        };
        let small = MonitorGeo {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
            primary: false,
        };

        // ① 同尺寸两屏：选区整个搬过去，坐标平移到新屏坐标系
        //    （选区是在**旧屏**上框的，所以坐标落在旧屏范围内：左屏 x ∈ [-1920, 0)）
        let on_left = Region {
            x: -1820,
            y: 100,
            width: 640,
            height: 480,
        };
        let moved = region_after_monitor_switch(on_left, &left, &right).expect("同尺寸屏应能搬");
        assert_eq!(
            (moved.x, moved.y),
            (100, 100),
            "相对位置保持不变（左屏 → 右屏平移 +1920）"
        );
        assert_eq!((moved.width, moved.height), (640, 480));

        // ② 搬到更小的屏：装不下 → None（界面会清空选区并提示"请重新框选"）
        let wide_on_right = Region {
            x: 100,
            y: 100,
            width: 1200,
            height: 900,
        };
        assert_eq!(
            region_after_monitor_switch(wide_on_right, &right, &small),
            None,
            "新屏装不下时必须返回 None（宁可清空也不要留一个越界选区）"
        );
        // 小选区在小屏里装得下 → 正常保留
        let tiny = Region {
            x: 10,
            y: 10,
            width: 200,
            height: 120,
        };
        assert!(region_after_monitor_switch(tiny, &right, &small).is_some());

        // ③ 夹紧：选区部分越出显示器时被拉回来
        let out = Region {
            x: 1800,
            y: 1000,
            width: 400,
            height: 400,
        };
        let clamped = clamp_region_to_monitor(out, &right).expect("应能夹紧");
        assert!(clamped.x + clamped.width as i32 <= 1920);
        assert!(clamped.y + clamped.height as i32 <= 1080);
        // ④ 比最小边还小的显示器：直接返回 None（不 panic、不产生非法矩形）
        let dot = MonitorGeo {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
            primary: false,
        };
        assert_eq!(clamp_region_to_monitor(on_left, &dot), None);
    }

    // ---- ③ 负坐标副屏 + DPI 换算 ----

    #[test]
    fn mapping_handles_negative_monitor_and_dpi() {
        // 副屏在主屏左侧：显示器在虚拟桌面里是 (-1920, 0)，2560×1440，DPI 125%
        // （窗口逻辑尺寸 = 物理 / 1.25 = 2048×1152）
        let map = map_of((-1920, 0), (2560, 1440), (2048.0, 1152.0));
        assert_eq!(map.bounds(), (-1920, 0, 640, 1440));
        // 窗口左上角 = 显示器左上角
        assert_eq!(map.to_physical(Pos2::new(0.0, 0.0)), (-1920, 0));
        // 每个逻辑点 = 1.25 物理像素
        assert_eq!(map.to_physical(Pos2::new(160.0, 160.0)), (-1720, 200));
        // 来回换算必须闭合
        let (px, py) = map.to_physical(Pos2::new(777.0, 333.0));
        let back = map.to_local(px, py);
        assert!(
            (back.x - 777.0).abs() <= 1.0 && (back.y - 333.0).abs() <= 1.0,
            "物理→逻辑应可逆：{back:?}"
        );

        // 负坐标下框选：结果仍是负的（旧版正是因为把 x 当 usize 才错）
        let r = region_from_selection(&map, Pos2::new(0.0, 0.0), Pos2::new(160.0, 160.0))
            .expect("应能算出选区");
        assert_eq!((r.x, r.y), (-1920, 0), "负坐标必须保持为负");
        assert_eq!((r.width, r.height), (200, 200));

        // 主屏 + 100% DPI：宽高与逻辑点 1:1
        let main = map_of((0, 0), (1920, 1080), (1920.0, 1080.0));
        assert_eq!(main.to_physical(Pos2::new(1920.0, 1080.0)), (1920, 1080));
    }

    // ---- 冻结帧的 uv 裁剪（多屏时只取本显示器那块） ----

    #[test]
    fn frame_uv_picks_the_monitor_slice() {
        // 冻结帧覆盖整个虚拟桌面：左侧副屏 (-1920,0)-(2560 宽) + 主屏 (0,0)-(1920,1080)
        // → 虚拟桌面原点 (-1920, 0)，尺寸 4480×1440
        let frame = Screenshot {
            origin: (-1920, 0),
            width: 4480,
            height: 1440,
            rgba: Vec::new(),
        };
        // 副屏 (-1920,0)-(640,1440)：帧内 x 偏移 0 → u 从 0 到 2560/4480
        let map = map_of((-1920, 0), (2560, 1440), (2048.0, 1152.0));
        let uv = map.frame_uv(&frame);
        assert!((uv.min.x - 0.0).abs() < 1e-6, "最左侧显示器的 u 应为 0");
        assert!((uv.max.x - 2560.0 / 4480.0).abs() < 1e-6, "uv={uv:?}");
        // 主屏 (0,0)-(1920,1080)：帧内 x 偏移 1920 → u 从 1920/4480 到 3840/4480
        let main = map_of((0, 0), (1920, 1080), (1920.0, 1080.0));
        let uv2 = main.frame_uv(&frame);
        assert!(
            (uv2.min.x - 1920.0 / 4480.0).abs() < 1e-6,
            "主屏应截到帧的右半部分：{uv2:?}"
        );
        assert!((uv2.max.x - 3840.0 / 4480.0).abs() < 1e-6);
        // 显示器矩形超出帧范围时 uv 被夹在 0..1，不会越界采样
        let outside = map_of((100_000, 0), (1920, 1080), (1920.0, 1080.0));
        let uv3 = outside.frame_uv(&frame);
        assert!(uv3.min.x >= 0.0 && uv3.max.x <= 1.0);
    }

    // ---- BGRA → RGBA（GDI 冻结帧的颜色通道顺序） ----

    #[test]
    fn bgra_to_rgba_swaps_channels_and_forces_opaque() {
        // GDI 32bpp DIB：B,G,R,0
        let bgra = [10u8, 20, 30, 0, 40, 50, 60, 0];
        let rgba = bgra_to_rgba(bgra.to_vec());
        assert_eq!(rgba, vec![30, 20, 10, 255, 60, 50, 40, 255]);
        // 尾部不足一个像素的字节被丢弃，不越界
        assert_eq!(bgra_to_rgba(vec![1, 2, 3]).len(), 0);
        assert_eq!(bgra_to_rgba(Vec::new()).len(), 0);
        // 尺寸不符时不上传纹理（宁可不显示也不 panic）
        let bad = Screenshot {
            origin: (0, 0),
            width: 2,
            height: 2,
            rgba: vec![0; 4],
        };
        assert!(bad.to_color_image().is_none());
        let empty = Screenshot {
            origin: (0, 0),
            width: 0,
            height: 0,
            rgba: Vec::new(),
        };
        assert!(empty.to_color_image().is_none());
    }

    /// 真机验证 GDI 冻结帧：必须真抓到一张合法的 RGBA 图（本机有显示器所以不会跳过）。
    /// 这条测试是"遮罩不再整屏黑"的第一道保险——如果 BitBlt/GetDIBits 挂了，这里先红。
    #[test]
    fn desktop_frame_captures_and_decodes() {
        let s = capture_desktop().expect("GDI 截屏应成功");
        println!(
            "冻结帧 origin={:?} {}x{} 字节={}",
            s.origin,
            s.width,
            s.height,
            s.rgba.len()
        );
        assert_eq!(s.rgba.len(), s.width * s.height * 4);
        assert!(s.width > 0 && s.height > 0);
        assert!(s.to_color_image().is_some(), "应能转成纹理数据");
    }

    #[test]
    fn monitor_index_roundtrip_matches_recorder_convention() {
        // recorder: 0 = 主显示器，n = 枚举顺序 [n-1]；界面统一写 序号+1
        assert_eq!(cfg_of_rank(0), 1);
        assert_eq!(rank_of_cfg(1), 0);
        assert_eq!(rank_of_cfg(cfg_of_rank(2)), 2);
    }

    #[test]
    fn elide_keeps_short_paths_intact() {
        assert_eq!(elide("C:\\tmp", 32), "C:\\tmp");
        let long = "C:\\very\\long\\path\\with\\many\\segments\\rec-20240101-000000.mp4";
        let e = elide(long, 20);
        assert!(e.chars().count() <= 21, "省略后不该更长: {e}");
        assert!(e.contains('…'));
    }

    // ---- 最小窗口尺寸的**实测**（见 `crate::ui::layout`）----

    /// 装**和真机同一套**字体：内置中文子集（含界面全部汉字）+ egui 自带拉丁/符号字体。
    ///
    /// 绝对尺寸的断言必须用它：没有汉字字形时中文会被当成缺字，量出来的行高/行宽都不作数。
    fn install_measure_fonts(ctx: &egui::Context) {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            crate::ui::startup::SUBSET_FONT_NAME.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(
                crate::ui::startup::SUBSET_FONT_BYTES,
            )),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, crate::ui::startup::SUBSET_FONT_NAME.to_owned());
        ctx.set_fonts(fonts);
    }

    /// 录制页的无窗口测试台：真字体 + 真主题 + 指定语言，跑 `MainPage::ui` 真实布局。
    ///
    /// 唯一被替换掉的是"输出目录"：指到临时目录，免得 `MainPage` 的**可写性探测**
    ///（它会真写一个临时文件）往用户的"视频"目录里写东西。其余全部走真代码路径
    ///（显示器/WASAPI 枚举、目录探测都在这里真实发生，失败也只当"加载失败"处理，不会 panic）。
    struct RecordHarness {
        ctx: egui::Context,
        /// 逐部件高度探针要读它（`MainPage::record_probe`）
        page: MainPage,
        cfg: Config,
        recorder: Option<Recorder>,
        result: Option<RecordingResult>,
        i18n: I18n,
        screen: Rect,
        time: f64,
    }

    impl RecordHarness {
        fn new(w: f32, h: f32, lang: &str) -> Self {
            let ctx = egui::Context::default();
            install_measure_fonts(&ctx);
            crate::ui::theme::apply(&ctx);
            let out = std::env::temp_dir().join("caplite-layout-probe");
            let _ = std::fs::create_dir_all(&out);
            let cfg = Config {
                output_dir: out.to_string_lossy().to_string(),
                ..Config::default()
            };
            Self {
                ctx,
                page: MainPage::new(),
                cfg,
                recorder: None,
                result: None,
                i18n: I18n::new(lang),
                screen: Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h)),
                time: 0.0,
            }
        }

        /// 跑一帧并返回本页的布局测量。
        ///
        /// 跑 3 帧才是稳态：`MainPage` 的"延后初始化"（显示器/麦克风枚举 + 目录探测）
        /// 发生在第 2 帧，第 3 帧显示的才是用户真正会看到的列表与信息。
        fn frame(&mut self) -> LayoutProbe {
            let Self {
                ctx,
                page,
                cfg,
                recorder,
                result,
                i18n,
                screen,
                time,
            } = self;
            let mut input = egui::RawInput {
                screen_rect: Some(*screen),
                ..Default::default()
            };
            input.time = Some(*time);
            *time += 0.5;
            let mut probe = LayoutProbe::default();
            ctx.run_ui(input, |ui| {
                let _ = page.ui(
                    ui,
                    MainCtx {
                        cfg,
                        i18n,
                        recorder,
                        result,
                    },
                );
                probe = page.layout_probe();
            })
            .drop_without_applying_deltas();
            probe
        }

        fn steady(&mut self) -> LayoutProbe {
            let mut p = self.frame();
            for _ in 0..2 {
                p = self.frame();
            }
            p
        }
    }

    /// 录制页在 `w × h` 下的稳态布局测量。
    fn measure_record(w: f32, h: f32, lang: &str) -> LayoutProbe {
        RecordHarness::new(w, h, lang).steady()
    }

    /// **标定**：`ScrollArea::content_size.y` 与"自己相加的内容高"之间的关系。
    ///
    /// 为什么要标定：逐部件探针要能解释 `RECORD_REQ_H` 那 366 是怎么来的。
    /// `ScrollArea::show` 的 `content_size` 是滚动**内容层** `Ui::min_size()`，
    /// 而 `auto_shrink([false, false])` 下那一层的 `max_rect` 被撑到视口大小 ——
    /// 到底它报的是"内容高"还是别的口径，只能真量。
    ///
    /// 量法：同一份卡片序列，只换"最后一张卡片"的高度（40 → 140），
    /// 同时记下自己相加的和与 `content_size.y`。
    fn calibrate_scroll_content_size() {
        let h = 499.0f32;
        println!();
        println!("[calib] ScrollArea content_size 口径标定（视口高 {h}）");
        for last in [40.0f32, 140.0] {
            let ctx = egui::Context::default();
            install_measure_fonts(&ctx);
            crate::ui::theme::apply(&ctx);
            let mut size = 0.0f32;
            let mut fixed = 0.0f32;
            let mut min_rect_bottom = 0.0f32;
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(760.0, h))),
                    ..Default::default()
                },
                |ui| {
                    let out = egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            // 固定 52 + 92 + 118，最后一张可变（P37 内边距/行距压缩后的实测值）
                            for (i, card_h) in [52.0f32, 92.0, 118.0, last].iter().enumerate() {
                                if i > 0 {
                                    ui.add_space(theme::GAP_CARD_TIGHT);
                                }
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), *card_h),
                                    egui::Sense::hover(),
                                );
                                fixed += rect.height();
                            }
                            fixed += theme::GAP_CARD_TIGHT * 3.0;
                            min_rect_bottom = ui.min_rect().bottom();
                        });
                    size = out.content_size.y;
                },
            )
            .drop_without_applying_deltas();
            println!(
                "  最后一张 {last:>5.0} → 相加 {fixed:>6.1}；内容层 min_rect 底 {min_rect_bottom:>6.1}；content_size.y {size:>6.1}；差 {:.1}",
                size - fixed
            );
            let _ = size;
        }
    }

    /// **时间码字号 → 实测行高**的扫描（一次性探针，`#[ignore]`，不参与常规测试）。
    ///
    /// 为什么需要它：状态卡片那一行的高度**就是时间码自己的行高**（实测 34 号 → 40px，
    /// 而右侧"开始录制"按钮只有 34px），所以"把这一行压低"只能靠**调小时间码字号**。
    /// 但字号到行高不是 1:1（egui 的 `RowHeight` 会向上取整到整像素），
    /// 因此候选字号必须**逐个真量**，不能按比例估。
    ///
    /// 跑法：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture probe_timer_font_heights
    /// ```
    ///
    /// 同时把"等宽数字不跳动"这一条也量出来：`00:00` / `00:01` / `09:59` / `59:59`
    /// 四个串在候选字号下的**宽度必须完全相同**（字体族是 `Monospace`）。
    #[test]
    #[ignore = "计时器字号扫描：打印用，不参与常规测试（见文档注释）"]
    fn probe_timer_font_heights() {
        let ctx = egui::Context::default();
        install_measure_fonts(&ctx);
        crate::ui::theme::apply(&ctx);
        println!();
        println!(
            "[timer] 时间码字号扫描（等宽 + `layout_no_wrap` 的 galley 高度；按钮高 {:.0}）",
            theme::START_BTN_H
        );
        println!("[timer] 当前 FS_TIMER = {}", theme::FS_TIMER);
        println!(
            "{:>6}  {:>8}  {:>8}  {:>8}  {:>8}  整行高 = max(行高, 按钮 34)",
            "字号", "高(px)", "行高(px)", "00:00宽", "59:59宽"
        );
        // `layout_no_wrap` 需要 `&mut FontsView`，而字体视图只在**帧内**可用 —— 所以整段量测
        // 跑在 `run_ui` 里（与上面那条逐部件探针同一套驱动方式，量到的就是同一套字体）。
        ctx.run_ui(egui::RawInput::default(), |ui| {
        ui.ctx().fonts_mut(|fonts| {
            for size in [34.0f32, 32.0, 31.0, 30.0, 29.0, 28.0, 27.0, 26.0, 24.0] {
                let g = fonts.layout_no_wrap(
                    "00:00".to_owned(),
                    egui::FontId::monospace(size),
                    crate::ui::theme::text(),
                );
                let h = g.size().y;
                let row = fonts.row_height(&egui::FontId::monospace(size));
                let mut w = |s: &str| {
                    fonts
                        .layout_no_wrap(
                            s.to_owned(),
                            egui::FontId::monospace(size),
                            crate::ui::theme::text(),
                        )
                        .size()
                        .x
                };
                let (w0, w1, w2, w3) = (w("00:00"), w("00:01"), w("09:59"), w("59:59"));
                let widths_identical =
                    (w0 - w1).abs() < 0.01 && (w0 - w2).abs() < 0.01 && (w0 - w3).abs() < 0.01;
                println!(
                    "{size:>6.1}  {h:>8.1}  {row:>8.1}  {w0:>8.1}  {w3:>8.1}  用 max(行高, {:.0}) = {:.0}；宽度一致(00:00/00:01/09:59/59:59)={widths_identical}",
                    theme::START_BTN_H,
                    row.max(theme::START_BTN_H)
                );
            }
        });
        })
        .drop_without_applying_deltas();
        println!();
    }

    /// **录制页必需高度 ≤ 最小窗口给它的高度**（用户要求里录制页那一半）。
    ///
    /// 录制页没有"流式"内容：卡片高度只由文字与控件决定，所以 `ScrollArea` 的
    /// `content_size.y` 直接就是必需高度（与窗口高度无关）。
    #[test]
    fn headless_record_required_height_fits_the_min_window() {
        use crate::ui::layout::{CHROME_H, MIN_INNER_H, MIN_INNER_W, RECORD_REQ_H};
        let page_avail_at_min = MIN_INNER_H - CHROME_H;

        // ① 必需高度：两种内置语言的较大者
        let mut req = 0.0f32;
        for lang in ["zh-CN", "en-US"] {
            let p = measure_record(MIN_INNER_W, page_avail_at_min, lang);
            println!(
                "[layout] 录制页必需高度（{lang}，宽 {MIN_INNER_W}）= {:.1}（视口 {:.1}，页面可用 {:.1}）",
                p.required_page_h(),
                p.viewport_h,
                p.page_avail_h
            );
            req = req.max(p.required_page_h());
        }
        assert!(
            (req - RECORD_REQ_H).abs() <= 0.5,
            "实测录制页必需高度 {req:.1} 与常量 RECORD_REQ_H={RECORD_REQ_H} 不一致"
        );
        assert!(
            req <= page_avail_at_min + 0.5,
            "录制页必需高度 {req:.1} 超过最小窗口给它的 {page_avail_at_min:.1}"
        );

        // ② 必需高度与窗口高度无关（卡片没有流式部分）：窗口高一倍，内容高度一模一样
        let base = measure_record(MIN_INNER_W, page_avail_at_min, "zh-CN").required_page_h();
        let tall = measure_record(MIN_INNER_W, page_avail_at_min + 200.0, "zh-CN");
        assert!(
            (tall.required_page_h() - base).abs() <= 0.5,
            "录制页内容高度不该随窗口高度变化：{:.1} vs {:.1}",
            tall.required_page_h(),
            base
        );

        // ③ 最小宽度已经够宽：再宽也不会变矮（= 没有行在换行，最宽那一行放得下）
        let wide = measure_record(1600.0, page_avail_at_min, "zh-CN").required_page_h();
        assert!(
            (wide - base).abs() <= 0.5,
            "最小宽度下录制页仍有行在换行：宽 {MIN_INNER_W} → {base:.1}，宽 1600 → {wide:.1}"
        );

        // ④ 最小尺寸下**不需要纵向滚动**（滚动条恒不显示）
        for lang in ["zh-CN", "en-US"] {
            let p = measure_record(MIN_INNER_W, page_avail_at_min, lang);
            assert!(
                p.no_vertical_scroll(),
                "[{lang}] 最小窗口下录制页内容不得溢出滚动区：内容 {:.1} > 视口 {:.1}",
                p.content_h,
                p.viewport_h
            );
            assert!(
                p.page_avail_h >= page_avail_at_min - 0.5,
                "[{lang}] 录制页可用高度 {:.1} 小于最小窗口承诺的 {page_avail_at_min:.1}",
                p.page_avail_h
            );
        }
    }

    /// **P38 合并行的"不换行 / 不被裁切"守门测试**：采集卡「显示器 + 录制范围」一行、
    /// 内容卡「音源（可多选）+ 复选框 + 麦克风下拉」一行，都是把原来的两行并成一行。
    ///
    /// 合行最怕的就是**宽度不够**：宽度不够时英文界面要么把控件挤出卡片内容区（被裁切），
    /// 要么换行（行高变化 = 又变回两行）。这里拿**每行最后一个控件的右缘**来量：
    /// 它必须落在卡片内容区右缘之内（`页面可用宽 − 内边距 8 − 描边 1`）。
    ///
    /// 为什么不是"高度不变"就够（③）：高度不变只说明没换行；控件被 `max_rect` 裁掉时
    /// 高度同样不变。两个方向都要守，缺一个就是"看起来一样、其实少了半个按钮"。
    #[test]
    fn record_page_capture_and_audio_rows_fit() {
        use crate::ui::layout::{CHROME_H, MIN_INNER_H, MIN_INNER_W};
        let page_h = MIN_INNER_H - CHROME_H;
        // 卡片内容区右缘：页面从 x=0 起排，卡片整幅占满，内容区 = 内边距 8 + 描边 1 之内
        let content_right = MIN_INNER_W - crate::ui::theme::PAD_CARD_TIGHT - 1.0;
        // `audio_row_last_x` 用的是水平布局游标左缘（最后一个部件右缘 + 一个 item_spacing），
        // 所以给一个 item_spacing 量级的容差；采集行量的是按钮真实右缘，容差同样放宽。
        let slack = crate::ui::theme::ITEM_SPACING_SLACK;
        for lang in ["zh-CN", "en-US"] {
            let mut h = RecordHarness::new(MIN_INNER_W, page_h, lang);
            h.steady();
            let p = h.page.record_probe();
            assert!(
                p.capture_row_last_x > 0.0 && p.capture_row_last_x <= content_right + slack,
                "[{lang}] 采集行「显示器 + 录制范围」溢出卡片内容区：\
                 末控件右缘 {:.1} > 右缘上限 {:.1}（宽 {MIN_INNER_W}）",
                p.capture_row_last_x,
                content_right + slack
            );
            assert!(
                p.audio_row_last_x > 0.0 && p.audio_row_last_x <= content_right + slack,
                "[{lang}] 音源行「复选框 + 麦克风下拉」溢出卡片内容区：\
                 末控件右缘 {:.1} > 右缘上限 {:.1}（宽 {MIN_INNER_W}）",
                p.audio_row_last_x,
                content_right + slack
            );
        }
    }

    // -----------------------------------------------------------------------
    // 逐部件高度探针（用户要的"录制页逐部件高度表"）
    // -----------------------------------------------------------------------

    /// 真机探针：真实 DXGI/OpenH264，验证 `start_recording/stop_recording`
    /// 立刻返回（耗时工作都在后台），并且结果能通过 `poll_recording_jobs` 收回。
    ///
    /// ```text
    #[test]
    fn start_watchdog_fires_and_reports_timeout() {
        let mut page = MainPage::new();
        let (_tx, rx) = std::sync::mpsc::channel();
        page.start_rx = Some(rx);
        page.starting = true;
        page.start_deadline = Some(Instant::now() - Duration::from_secs(1));
        let cfg = Config::default();
        let i18n = I18n::new("en-US");
        let mut recorder = None;
        let mut result = None;
        page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
        assert!(!page.starting, "看门狗触发后不能永远 Starting");
        assert!(page.watchdog_fired);
        assert_eq!(page.error, Some(i18n.t("err_start_timeout").to_string()));
    }

    #[test]
    fn stop_watchdog_detaches_and_reports_timeout() {
        let mut page = MainPage::new();
        let (_tx, rx) = std::sync::mpsc::channel();
        page.stop_rx = Some(rx);
        page.stopping = true;
        page.stop_deadline = Some(Instant::now() - Duration::from_secs(1));
        let cfg = Config::default();
        let i18n = I18n::new("en-US");
        let mut recorder = None;
        let mut result = None;
        page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
        assert!(!page.stopping, "看门狗触发后不能永远 Exporting");
        assert!(page.watchdog_fired);
        assert_eq!(page.error, Some(i18n.t("err_stop_timeout").to_string()));
    }

    /// cargo test --lib async_start_stop_probe -- --ignored --nocapture
    /// ```
    #[test]
    fn watchdog_signals_and_reaps_a_stuck_start_job() {
        let mut page = MainPage::new();
        let (_tx, rx) = std::sync::mpsc::channel();
        page.start_rx = Some(rx);
        page.starting = true;
        page.start_deadline = Some(Instant::now() - Duration::from_secs(1));
        let cancel = Arc::new(AtomicBool::new(false));
        page.start_cancel = Some(Arc::clone(&cancel));
        page.start_handle = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(80));
        }));
        let cfg = Config::default();
        let i18n = I18n::new("en-US");
        let mut recorder = None;
        let mut result = None;
        page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
        assert!(cancel.load(Ordering::Acquire), "看门狗必须发取消信号");
        assert!(page.start_handle.is_some(), "线程未退出前必须保留句柄");
        assert!(
            page.jobs_still_running(),
            "旧任务未退出时不能允许叠加新会话"
        );
        std::thread::sleep(Duration::from_millis(120));
        page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
        assert!(page.start_handle.is_none(), "线程退出后应回收句柄");
        assert!(!page.jobs_still_running());
    }

    #[test]
    fn watchdog_signals_and_reaps_a_stuck_stop_job() {
        let mut page = MainPage::new();
        let (_tx, rx) = std::sync::mpsc::channel();
        page.stop_rx = Some(rx);
        page.stopping = true;
        page.stop_deadline = Some(Instant::now() - Duration::from_secs(1));
        let cancel = Arc::new(AtomicBool::new(false));
        page.stop_cancel = Some(Arc::clone(&cancel));
        page.stop_handle = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(80));
        }));
        let cfg = Config::default();
        let i18n = I18n::new("en-US");
        let mut recorder = None;
        let mut result = None;
        page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
        assert!(cancel.load(Ordering::Acquire), "停止超时后必须请求取消导出");
        assert!(page.stop_handle.is_some(), "收尾线程未退出前必须保留句柄");
        std::thread::sleep(Duration::from_millis(120));
        page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
        assert!(page.stop_handle.is_none(), "收尾线程退出后应回收句柄");
    }
    #[test]
    #[ignore = "真机异步起停探针（需要显示器/OpenH264），用 --ignored 手动运行"]
    fn async_start_stop_probe() {
        let root = std::env::temp_dir().join(format!("cap-async-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Config {
            output_dir: root.join("out").to_string_lossy().into_owned(),
            content_mode: ContentMode::VideoOnly,
            ..Config::default()
        };
        let i18n = I18n::new("en-US");
        let mut page = MainPage::new();
        page.set_cache_dir(root.join("Cache"));
        let mut recorder = None;
        let mut result = None;

        let t = Instant::now();
        let _ = page.start_recording(&cfg, &mut recorder, &mut result, &i18n);
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "start 不能阻塞 UI"
        );
        assert!(page.is_busy());
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && recorder.is_none() && page.is_busy() {
            page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(recorder.is_some(), "启动应在 15 s 内完成：{:?}", page.error);
        assert!(!page.is_busy());

        let t = Instant::now();
        let _ = page.stop_recording(&cfg, &mut recorder, &mut result, &i18n);
        assert!(t.elapsed() < Duration::from_millis(500), "stop 不能阻塞 UI");
        assert!(page.is_busy());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && page.is_busy() {
            page.poll_recording_jobs(&cfg, &mut recorder, &mut result, &i18n);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!page.is_busy(), "停止应在 30 s 内完成：{:?}", page.error);
        assert!(result.is_some(), "停止后必须有结果");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 录制落点：新文件进 `Cache\recordings`，同一秒第二次开始不能覆盖第一次。
    #[test]
    fn record_paths_uses_recordings_subdir_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("cap-record-paths-{}", std::process::id()));
        let out = root.join("out");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(cap_lite_recordings(&root)).unwrap();
        let (s1, e1) = record_paths(&root, &out, "20260101-000000");
        assert_eq!(s1.parent(), Some(cap_lite_recordings(&root).as_path()));
        assert_eq!(s1.file_name(), e1.file_name());
        std::fs::write(&s1, b"x").unwrap();
        let (s2, _) = record_paths(&root, &out, "20260101-000000");
        assert_ne!(s1, s2, "同秒第二次必须换名，不能覆盖已有分片");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn cap_lite_recordings(root: &Path) -> PathBuf {
        crate::cache::recordings_dir(root)
    }

    /// 打印录制页的**逐部件高度表**（中英各一份）。
    ///
    /// **它刻意是 `#[ignore]`，不参与常规测试**：
    /// * 它不是断言，而是"把实测数字打在屏幕上"的一次性探针；常规 `cargo test` 不该多跑它；
    /// * 它依赖真字体与真布局（[`RecordHarness`]），跑起来比普通单测慢，而输出只对人有用。
    ///
    /// 跑法：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture dump_record_page_heights
    /// ```
    ///
    /// 所有数字都来自**真布局**（`egui::Context::run_ui` + 固定视口，见 [`RecordProbe`]），
    /// 没有一个是估的。视口取最小窗口给录制页的高度（`MIN_INNER_H − CHROME_H`），
    /// 宽度取 `MIN_INNER_W`（= 程序允许的最窄窗口，也就是"最挤"的那一档）。
    #[test]
    #[ignore = "逐部件高度探针：打印用，不参与常规测试（见文档注释）"]
    fn dump_record_page_heights() {
        // 先做一次"滚动区 content_size 口径"的标定：真按当前配置画一遍状态行，
        // 看 `content_size.y` 与"自己加起来的内容高"差多少（这一步只打印、不参与断言）。
        calibrate_scroll_content_size();
        use crate::ui::layout::{CHROME_H, MIN_INNER_H, MIN_INNER_W, RECORD_REQ_H};
        let page_h = MIN_INNER_H - CHROME_H;
        println!();
        println!(
            "[probe] 录制页逐部件高度（逻辑点）—— 视口 {MIN_INNER_W}×{page_h}（= 最小窗口 {MIN_INNER_W}×{MIN_INNER_H} 扣掉外壳 {CHROME_H}）"
        );
        println!(
            "[probe] 计时器字号 FS_TIMER={} / 状态文字 FS_STATUS={} / 主按钮 {:.0}×{:.0}（固定不动）",
            theme::FS_TIMER,
            theme::FS_STATUS,
            theme::START_BTN_W,
            theme::START_BTN_H
        );
        for lang in ["zh-CN", "en-US"] {
            let mut h = RecordHarness::new(MIN_INNER_W, page_h, lang);
            let layout = h.steady();
            let p = h.page.record_probe();
            println!();
            println!("---- {lang} ----");
            println!(
                "页面可用区      : {:.1} 宽 × {:.1} 高（顶边 y={:.1}）",
                p.page_avail_w, p.page_avail_h, p.page_top
            );
            println!(
                "滚动区内容      : 顶 y={:.1} / 底 y={:.1} / 高 {:.1}（`content_size.y` = {:.1}；视口 {:.1}）",
                p.content_top,
                p.content_bottom,
                p.total_content_h,
                p.scroll_content_h,
                p.scroll_viewport_h
            );
            println!(
                "滚动区口径差额  : {:.1}（嵌套布局的行距被重复计入；见 `calibrate_scroll_content_size`）",
                p.nested_layout_residual_h
            );
            println!(
                "页面首留白      : {:.1}（页面可用区顶 → 内容顶）；尾留白: {}",
                p.lead_blank_h,
                match p.tail_blank_h {
                    Some(v) => format!("{v:.1}"),
                    None => "未量到".to_string(),
                }
            );
            println!(
                "必需页面高度    : {:.1}（常量 RECORD_REQ_H = {RECORD_REQ_H}）",
                layout.required_page_h()
            );
            println!();
            println!("① 状态卡片（含内边距与 1px 描边）");
            println!(
                "   整卡高度            {:.1}   （内容 {:.1} + 上下内边距 {:.1} + 描边 {:.1}）",
                p.status_card_h,
                p.status_card_inner_h,
                p.card_pad * 2.0,
                p.status_card_h - p.status_card_inner_h - p.card_pad * 2.0
            );
            println!(
                "   「就绪 + 时间码」行  {:.1}   ← 行内最高部件决定",
                p.status_row_h
            );
            println!("     状态点            {:.1}", p.status_dot_h);
            println!("     状态文字          {:.1}", p.status_text_h);
            println!("     时间码文本自身    {:.1}", p.timer_text_h);
            println!(
                "     右侧「开始录制」  {:.1} 高 × {:.1} 宽",
                p.start_btn_h, p.start_btn_w
            );
            println!(
                "     行内撑高余量      {:.1}（行高 − 最高部件）",
                p.status_row_stack_h
            );
            println!();
            println!("② 采集卡片");
            println!(
                "   整卡高度            {:.1}   （内容 {:.1} + 内边距 {:.1}）",
                p.capture_card_h,
                p.capture_card_inner_h,
                p.card_pad * 2.0
            );
            println!(
                "   「显示器 + 录制范围」行 {:.1}（P38 合并成一行，卡内行距 {:.1}）",
                p.capture_row_h, p.capture_gap_h
            );
            println!(
                "   行末控件右缘        {:.1}（卡片内容区右缘 = 页面宽 − 9；不得超）",
                p.capture_row_last_x
            );
            println!();
            println!("③ 录制内容卡片");
            println!(
                "   整卡高度            {:.1}   （内容 {:.1} + 内边距 {:.1}）",
                p.content_card_h,
                p.content_card_inner_h,
                p.card_pad * 2.0
            );
            println!("   「录制内容」行      {:.1}", p.rec_mode_row_h);
            println!("   行间距              {:.1}", p.content_gap_h);
            println!(
                "   「音源」标签行      {:.1}（与复选框同行，P38）；行内间距 {:.1}",
                p.audio_label_h, p.audio_label_gap_h
            );
            println!("   复选框行            {:.1}", p.audio_row_h);
            println!(
                "   行末控件右缘        {:.1}（同左，不得超卡片内容区右缘）",
                p.audio_row_last_x
            );
            println!();
            println!("④ 存储位置卡片");
            println!(
                "   整卡高度            {:.1}   （内容 {:.1} + 内边距 {:.1}）",
                p.output_card_h,
                p.output_card_inner_h,
                p.card_pad * 2.0
            );
            println!("   存储位置行          {:.1}", p.output_row_h);
            println!();
            println!("⑤ 间距 / 内边距 / 留白");
            println!(
                "   卡片间距（×3）      {:.1} 各 = {:.1} 合计",
                p.card_gap_h,
                p.card_gap_h * 3.0
            );
            println!(
                "   卡片内边距（上下各）{:.1}（4 张 ×2 = 合计 {:.1}）",
                p.card_pad,
                p.card_pad * 8.0
            );
            println!("   页面首留白          {:.1}", p.lead_blank_h);
            let tail = p.tail_blank_h.unwrap_or(0.0);
            println!("   页面尾留白          {tail:.1}");
            println!();
            // 合计核对：四张卡 + 三个卡片间距 + 首留白 + 尾留白
            let cards = p.status_card_h + p.capture_card_h + p.content_card_h + p.output_card_h;
            let gaps = p.card_gap_h * 3.0;
            let residual = p.nested_layout_residual_h;
            println!("⑥ 合计核对");
            println!("   卡片合计            {cards:.1}");
            println!("   卡片间距合计        {gaps:.1}");
            println!("   滚动区口径差额      {residual:.1}");
            println!("   首留白              {:.1}", p.lead_blank_h);
            println!("   尾留白              {tail:.1}");
            println!(
                "   相加                {:.1}   （实测内容高 {:.1}，差 {:.1}）",
                cards + gaps + residual,
                p.total_content_h,
                (cards + gaps + residual) - p.total_content_h
            );
            println!(
                "   内容高 + 首留白 + 尾留白 = {:.1}（页面可用高 {:.1}）",
                p.total_content_h + p.lead_blank_h + tail,
                p.page_avail_h
            );
        }
        println!();
    }
}
