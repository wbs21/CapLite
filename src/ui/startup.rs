//! **启动阶段打点**：把"进程启动 → 首帧"的每一步都记成一行毫秒级日志。
//!
//! # 为什么放在 `ui` 库里
//! 打点要覆盖三段代码：`main.rs`（进程入口/日志/eframe 外壳）、[`super::main_page`]
//! （显示器与音频设备枚举）以及字体安装（`main.rs`）。三者分处 bin 与 lib，
//! 所以把"打点基础设施"放进库里，bin 侧只提供**日志落点**（[`enable`]），
//! 不让 lib 反向依赖 bin。
//!
//! # 统一的日志格式
//! ```text
//! [cap][t=123ms] phase=ui_first_frame_done scope=frame frame=1 dur=210.4ms extra="..."
//! ```
//! * `t=` —— **相对进程起点**的毫秒（见 `START`）；这是阶段之间的"累计时间轴"，
//!   直接相减就能得到每个阶段耗时，不必依赖外部测时工具。
//! * `phase=` —— 阶段名，稳定不变，便于脚本 `Select-String 'phase=...'` 解析。
//! * `scope=process|app|frame` —— 阶段归属（进程级 / 应用初始化 / 单帧）。
//! * `dur=` —— 该阶段**自身**耗时（毫秒，一位小数）；`t` 与 `dur` 同时给出来，
//!   是为了让"累计"与"自耗"都能一眼看到、且能互相校验（`t[子] - t[父]`）。
//! * `extra="..."` —— 可选附注（例如"枚举到 2 台显示器"）。
//!
//! # 什么时候会有输出
//! 只有 [`enable`] 被调用过（bin 在解析完命令行、确定日志落点后调用）才会产出日志。
//! 因此**单元测试**默认是关闭的：测试进程里 `enable()` 不会被调用，[`mark`] 只是一次
//! 原子读取 + 直接返回，不会污染测试输出，也不会跨测试串台。
//!
//! `START` 的取值方式：模块首次被访问时取一次 `Instant::now()`，放在 [`mark`]
//! 的调用点上就是"进程刚起步"（`main` 的第一行），误差是**首次访问该静态量之前的
//! 动态链接与 CRT 启动时间**——那段时间外部拿不到 `Instant`，只能由外部脚本测"墙钟"，
//! 见 `docs/DESIGN.md` 的实测章节。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 时间轴零点：本模块首次被访问的时刻（`main` 的第一行）。
pub(crate) static START: OnceLock<Instant> = OnceLock::new();

/// 取时间轴零点（首次调用即"现在"）。
#[inline]
pub fn start() -> Instant {
    *START.get_or_init(Instant::now)
}

/// 相对进程起点已经过了多少毫秒。
pub fn elapsed_ms() -> u128 {
    start().elapsed().as_millis()
}

// ---------------------------------------------------------------------------
// 内嵌的中文子集字体（启动优化第 3 项）
// ---------------------------------------------------------------------------
//
// 放在库里（而不是 `src/main.rs`）有两个理由：
// 1. `cargo test --lib` 能**直接测试**它（缺字、体积这两条回归），bin 的测试跑不到；
// 2. 与 `main.rs` 内嵌的是同一个文件，`include_bytes!` 在同一个 crate 里去重，
//    不会让 exe 变大。

/// 内嵌的中文子集字体（构建期由 `scripts/make-font-subset.ps1` 生成）。
///
/// 它只含"...界面真正会用到的字形"，体积 ~0.5 MB，装它几乎免费；
/// 完整系统字体（15~25 MB）由 `src/main.rs` 推迟到首帧之后再追加当兜底。
pub const SUBSET_FONT_BYTES: &[u8] = include_bytes!("../../assets/font-subset.ttf");

/// 子集字体在 `FontDefinitions` 里的名字（`src/main.rs` 装字体时用的就是它）。
pub const SUBSET_FONT_NAME: &str = "cjk-subset";

/// 打点是否已启用（[`enable`] 调用过）。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 日志落点：进程内最多设置一次的函数指针。
///
/// **为什么用 `OnceLock` 而不是可换出的原子指针**：打点可能与日志线程并发，
/// 若允许"换出并释放"，读方在"读到非空指针"与"解引用"之间就可能撞上释放
/// （use-after-free）。`OnceLock` 里的函数指针活到进程结束、永不被释放，
/// 读方拿到的引用因此始终有效；代价是它**不可换新**（重复 [`enable`] 会被忽略），
/// 而启动日志的落点在一次运行里本来就不需要换。
static SINK: OnceLock<fn(&str)> = OnceLock::new();

/// sink 未就绪之前最多缓冲多少条打点（进程入口那几步）。
///
/// 32 足够放下"进程启动/目录探测/日志就绪"这几步；溢出时**丢弃**（`marks_dropped` 计数），
/// 绝不 panic、绝不阻塞启动。
const BUFFER_CAP: usize = 32;

/// 已缓冲的原始打点行数。
static BUFFERED: AtomicUsize = AtomicUsize::new(0);
/// 因缓冲满而丢弃的打点条数（正常启动恒为 0）。
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// 缓冲槽：`(相对起点毫秒数, 已经格式化好的日志行)`。
///
/// 用"格式化好的字符串"而不是结构化字段，是为了让缓冲/直写两条路径产出**完全相同**的行；
/// 代价只是每次打点一次 `format!`（几十字节），相对启动路径上的其它开销可以忽略。
static BUFFER: [OnceLock<(u128, String)>; BUFFER_CAP] =
    [const { OnceLock::new() }; BUFFER_CAP];

/// 启用打点并把日志落点交给本模块（`sink` 收到的是**完整的一行**，含换行前的内容）。
///
/// 落点由 bin 决定（release 落盘 `cap.log`、debug 打控制台），lib 不关心文件在哪。
/// 重复调用是安全的：只有第一次生效（`OnceLock` 语义），随后重放尚未落地的缓冲。
pub fn enable(sink: fn(&str)) {
    let _ = SINK.set(sink);
    ENABLED.store(true, Ordering::Release);
    flush_buffer();
}

/// 停止打点（进程退出前调用；**不释放**已经设好的落点，见 `SINK` 的说明）。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn clear_sink() {
    ENABLED.store(false, Ordering::Release);
}

/// 把缓冲里还没落地的打点逐条交给 sink。
fn flush_buffer() {
    let n = BUFFERED.swap(0, Ordering::AcqRel).min(BUFFER_CAP);
    for slot in BUFFER.iter().take(n) {
        if let Some((_, line)) = slot.get() {
            emit(line);
        }
    }
}

/// 真正写出：读当前落点，读不到就缓冲。
fn emit(line: &str) {
    match SINK.get() {
        Some(sink) => sink(line),
        None => {
            // 落点还没就绪：先缓冲（溢出就丢弃，只计数，不影响启动）
            let idx = BUFFERED.fetch_add(1, Ordering::AcqRel);
            if idx < BUFFER_CAP {
                let _ = BUFFER[idx].set((elapsed_ms(), line.to_string()));
            } else {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// 因缓冲满而丢弃的打点条数（正常启动为 0；异常时也**绝不 panic**）。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn dropped_marks() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// 一行打点的各个字段（与 [`mark`] 的日志格式一一对应）。
///
/// 单独抽成结构体 + [`Line::render`]，是为了让**格式本身可单测**：
/// 脚本按 `phase=` / `t=` 解析，格式一旦漂移必须立刻被测试挡住。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// 相对进程起点的毫秒数
    pub t_ms: u128,
    /// 阶段名（稳定标识）
    pub phase: &'static str,
    /// 阶段归属（`process` / `app` / `frame`）
    pub scope: &'static str,
    /// 该阶段自身耗时（毫秒 ×10，即"十分之一毫秒"整数，避免浮点格式抖动）
    pub dur_tenths_ms: u64,
    /// 可选附注
    pub extra: Option<String>,
}

impl Line {
    /// 渲染成最终写进日志的那一行（**唯一格式来源**）。
    pub fn render(&self) -> String {
        let mut out = format!(
            "[cap][t={}ms] phase={} scope={} dur={}.{}ms",
            self.t_ms,
            self.phase,
            self.scope,
            self.dur_tenths_ms / 10,
            self.dur_tenths_ms % 10
        );
        if let Some(extra) = self.extra.as_deref().filter(|e| !e.is_empty()) {
            out.push_str(" extra=\"");
            out.push_str(&extra.replace('"', "'"));
            out.push('"');
        }
        out
    }
}

/// 打一个"瞬时"阶段点：耗时由本次 `Instant::now()` 与上一次打点之差算出。
///
/// `duration` 传 `Some(..)` 时用调用方测得的耗时（更精确：例如字体安装内部的两小步）。
pub fn mark_with(phase: &'static str, scope: &'static str, duration: Option<Duration>, extra: Option<String>) {
    if !ENABLED.load(Ordering::Acquire) {
        return; // 未启用（单测/其它复用 lib 的程序）→ 一次原子读 + 返回
    }
    let t_ms = elapsed_ms();
    let dur_tenths_ms = match duration {
        Some(d) => (d.as_micros() / 100) as u64,
        None => {
            // 没有自带计时：用"上一条打点"的时间差（TLS 里存上次的 `t_ms`）
            let previous = TLS_LAST.with(|cell| cell.replace(t_ms));
            ((t_ms.saturating_sub(previous)) as u64) * 10
        }
    };
    emit(&Line { t_ms, phase, scope, dur_tenths_ms, extra }.render());
}

thread_local! {
    /// 上一次"瞬时打点"的相对毫秒数（只用于算相邻两个阶段的间隔）。
    static TLS_LAST: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
}

/// 打一个带**显式耗时**的阶段点（最常用；耗时由 `Instant` 差值给出）。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn mark_dur(phase: &'static str, scope: &'static str, d: Duration) {
    mark_with(phase, scope, Some(d), None);
}

/// 打一个带显式耗时 + 附注的阶段点。
pub fn mark_dur_extra(phase: &'static str, scope: &'static str, d: Duration, extra: impl Into<String>) {
    let extra = extra.into();
    let extra = if extra.is_empty() { None } else { Some(extra) };
    mark_with(phase, scope, Some(d), extra);
}

/// 打一个"瞬时"点（耗时 = 与上一条瞬时点之差）。
pub fn mark(phase: &'static str, scope: &'static str) {
    mark_with(phase, scope, None, None);
}

// ---------------------------------------------------------------------------
// 给"拿得到日志落点、但不想全局启用打点"的调用方用的一组包装
// ---------------------------------------------------------------------------
//
// 为什么需要：设备枚举发生在 UI 页面的 `ui()` 里（每帧都被调用），那里能拿到外壳注入的
// 日志落点，但**不应该**把全局打点开关打开（单测、以及任何复用 lib 的程序都不该被写日志）。
// 于是这几个函数收 `log: Option<fn(&str)>`：`None` 就是纯空转，`Some` 才真的走一遍计时。
// 判断"是否真的记"由调用方决定，本模块只管格式与计时。

/// 记一个"瞬时"阶段点（耗时 = 与上一条瞬时点之差；`log = None` 时空转）。
pub fn mark_opt(log: Option<fn(&str)>, phase: &'static str, scope: &'static str) {
    let Some(sink) = log else { return };
    let t_ms = elapsed_ms();
    let dur_tenths_ms = {
        let previous = TLS_LAST.with(|cell| cell.replace(t_ms));
        ((t_ms.saturating_sub(previous)) as u64) * 10
    };
    sink(&Line { t_ms, phase, scope, dur_tenths_ms, extra: None }.render());
}

/// 记一个带显式耗时 + 附注的阶段点（`log = None` 时只执行闭包、不记任何东西）。
///
/// 名字带 `_log` 后缀是为了与上面的 [`mark_dur_extra`]（全局开关版）区分开：
/// 两个版本**不能同名**（Rust 没有重载），否则调用点会解析到错的那个。
pub fn mark_dur_extra_log(
    log: Option<fn(&str)>,
    phase: &'static str,
    scope: &'static str,
    d: Duration,
    extra: impl Into<String>,
) {
    let Some(sink) = log else { return };
    let extra = extra.into();
    let extra = if extra.is_empty() { None } else { Some(extra) };
    sink(&Line {
        t_ms: elapsed_ms(),
        phase,
        scope,
        dur_tenths_ms: (d.as_micros() / 100) as u64,
        extra,
    }
    .render());
}

/// 记一个带显式耗时的阶段点（`log = None` 时空转）。
pub fn mark_dur_log(log: Option<fn(&str)>, phase: &'static str, scope: &'static str, d: Duration) {
    mark_dur_extra_log(log, phase, scope, d, String::new());
}

/// 给一个表达式计时并把结果打点，返回表达式的值。
///
/// 这是启动路径上最省事的用法：
/// ```ignore
/// let list = caplite::ui::startup::timed("enum_audio", "app", || list_capture_devices());
/// ```
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn timed<T>(phase: &'static str, scope: &'static str, f: impl FnOnce() -> T) -> T {
    if !ENABLED.load(Ordering::Acquire) {
        return f();
    }
    let t0 = Instant::now();
    let value = f();
    mark_dur(phase, scope, t0.elapsed());
    value
}

// ---------------------------------------------------------------------------
// FFmpeg 运行期 DLL 预加载（启动优化第 2 项）
// ---------------------------------------------------------------------------
//
// # 为什么需要它（延迟加载的副作用）
// 5 个 FFmpeg DLL 已经改成 `/DELAYLOAD`（见 `build.rs` 的文件头注释），好处是**进程启动
// 瞬间不映射**这 110 MB；副作用是"第一次真正调用 FFmpeg"时才去映射 —— 而那个时刻正是
// 用户按下「开始录制」的瞬间（按钮响应会顿一下）。
//
// 用户要求："**显示器、音频枚举完成之后，就加载**"。所以这里在"首帧之后、设备枚举完成
// 那一刻"起一个**后台线程**主动 `LoadLibraryW` 一遍这些 DLL：
// * 不占 UI 线程（枚举本身也不等它，`ui()` 不等线程结束）；
// * 不早于首帧（调试点在 `MainPage::ui` 的枚举完成之后，见 `src/ui/main_page.rs`）；
// * 不等用户按录制（那时 DLL 早已在内存里，`LoadLibraryW` 只是引用计数 +1）。
//
// # 失败语义
// 只记日志（**warning**，写进 `cap.log`），**绝不 panic、绝不影响录制**：
// 单次失败就跳过这一个，剩下的继续；延迟加载兜底 —— 真正的调用点仍会自己去加载。

/// FFmpeg 运行期 DLL 名清单（**唯一来源**：`build.rs` 从 `$FFMPEG_DIR/bin` 的实际文件名推导，
/// 与它发给链接器的 `/DELAYLOAD` 是同一份，见 `build.rs::emit_shared_dll_names`）。
///
/// 生成文件里只有一行 `pub const FFMPEG_RUNTIME_DLLS: &[&str] = &[...];`。
/// 清单为空（构建时没设 `FFMPEG_DIR`）时预加载会**什么都不做**，而不是报错。
pub(crate) mod ffmpeg_dlls {
    include!(concat!(env!("OUT_DIR"), "/ffmpeg_dll_names.rs"));
}

/// 预加载是否已经**启动过**（一次性；每帧调用也不会重复起线程）。
static PRELOAD_STARTED: AtomicBool = AtomicBool::new(false);

/// 一次性认领（抽出 [`AtomicBool`] 参数是为了能在单测里验证"第二次不再触发"）。
fn claim_once(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::AcqRel)
}

/// **显示器/音频枚举完成之后**主动预加载 FFmpeg 的运行期 DLL（后台线程，非阻塞）。
///
/// 返回 `true` 表示**本次真的起了线程**（第一次调用），`false` 表示之前已经触发过
/// （或线程创建失败，此时会重置标志，下一帧还能再试）。
///
/// `log` 就是启动日志落点（`StartupLog::log_fn()`）：成功与失败各写一行；落点为空
/// （单测、或 bin 还没装好 sink）时那两行就静默丢弃，不影响任何事。
pub fn preload_ffmpeg_dlls_once(log: Option<fn(&str)>) -> bool {
    if !claim_once(&PRELOAD_STARTED) {
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("ffmpeg-preload".to_string())
        .spawn(move || preload_ffmpeg_dlls_blocking(ffmpeg_dlls::FFMPEG_RUNTIME_DLLS, log))
        .is_ok();
    if !spawned {
        // 线程都起不来（极端内存压力）：放回标志，下一帧再试；只记一行，绝不 panic
        PRELOAD_STARTED.store(false, Ordering::Release);
        log_line(log, "[cap] FFmpeg 预加载：线程创建失败（不影响录制，仍由延迟加载兜底）");
    }
    spawned
}

/// 后台线程里真正干活的那一步：逐个 `LoadLibraryW`，把结果记进日志。
///
/// 抽成独立函数（而不是写在闭包里）是为了**可单测**：单测传一个不存在的 DLL 名列表，
/// 断言"失败也只是日志、不 panic"。
fn preload_ffmpeg_dlls_blocking(names: &[&str], log: Option<fn(&str)>) {
    if names.is_empty() {
        log_line(
            log,
            "[cap] FFmpeg 预加载跳过：DLL 名单为空（构建时 FFMPEG_DIR 未设置或 bin 读不到？）",
        );
        return;
    }
    let t0 = Instant::now();
    let mut loaded: Vec<&str> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for name in names {
        match load_one(name) {
            Ok(()) => loaded.push(name),
            Err(e) => failed.push(format!("{name}（{e}）")),
        }
    }
    let elapsed = t0.elapsed();

    // 打点（`phase=ffmpeg_preload_done`）：与其它阶段一样带 `t=` / `dur=`，脚本可解析，
    // 也是"预加载到底在启动后多久发生"的唯一权威时间轴。
    mark_dur_extra_log(
        log,
        "ffmpeg_preload_done",
        "app",
        elapsed,
        format!("{}/{} 成功", loaded.len(), names.len()),
    );
    if failed.is_empty() {
        log_line(
            log,
            &format!(
                "[cap] FFmpeg 预加载完成：{}/{}（耗时 {}ms）：{}",
                loaded.len(),
                names.len(),
                elapsed.as_millis(),
                loaded.join(", ")
            ),
        );
    } else {
        // 失败**只记 warning**：预加载是"提前做的好事"，不是正确性前提
        log_line(
            log,
            &format!(
                "[cap] FFmpeg 预加载警告：{}/{} 成功（不影响录制，延迟加载仍会兜底），失败：{}",
                loaded.len(),
                names.len(),
                failed.join(", ")
            ),
        );
    }
}

/// 加载单个 DLL（按**名字**加载；发布布局优先 `<exe>\runtime\`，旧布局回退 exe 同级）。
///
/// **故意不 `FreeLibrary`**：预加载的意义就是让它在进程里常驻；释放掉等于白做
/// （还会把延迟加载再拉回"按需映射"）。失败信息直接用 `windows` crate 的错误（含
/// `GetLastError` 的 HRESULT 与系统消息，例如"找不到指定的模块"）。
fn load_one(name: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::{
        LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    };
    // 优先用 exe 同级 runtime\ 的**绝对路径**；旧布局（DLL 直接放 exe 同级）
    // 继续兼容。这样即使进程的默认搜索顺序被改坏，预加载也不会从 CWD/其它目录
    // 拿到同名 DLL。找不到时退回裸名字。
    let full = std::env::current_exe().ok().and_then(|p| {
        let dir = p.parent()?;
        let runtime = dir.join("runtime").join(name);
        if runtime.is_file() {
            Some(runtime)
        } else {
            let beside = dir.join(name);
            beside.is_file().then_some(beside)
        }
    });
    let path = full
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` 是本函数内构造的、以 0 结尾的 UTF-16 缓冲，在调用期间一直存活；
    // `LoadLibraryExW` 只读它。返回值（模块句柄）被有意丢弃 = 让模块留在进程里。
    // `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` 让该 DLL 的依赖也从它所在目录解析。
    let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
    match unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, flags) } {
        Ok(_handle) => Ok(()),
        Err(e) => Err(format!("{e}")),
    }
}

/// 直接写一行到启动日志落点（`None` 时空转；写成什么样由调用方决定）。
fn log_line(log: Option<fn(&str)>, line: &str) {
    if let Some(sink) = log {
        sink(line);
    }
}

/// 把一行说明写进启动日志落点（`cap.log`）—— 给**其它模块**用（它们拿不到私有的 `log_line`）。
///
/// 为什么需要这个口子：产品（GUI）**没有安装 tracing 订阅者**，`tracing::info!/warn!`
/// 在用户机器上全部丢弃；只有走启动日志落点的行才真的落地。
pub fn note(log: Option<fn(&str)>, line: &str) {
    log_line(log, line);
}

// ---------------------------------------------------------------------------
// OpenH264 编码器预热（启动优化第 4 项）
// ---------------------------------------------------------------------------
//
// `Recorder::start` 里的 OpenH264 初始化（`WelsCreateSVCEncoder` + `InitializeExt`）在本机
// 实测 **0.46~6.0 s**（机器忙时更长），而这段时间**一帧画面都没有** —— 用户按了"开始录制"
// 却要等几秒才真的开录，那几秒还会把"要录 5 秒"的预算吃掉（实测表见 `docs/DESIGN.md`）。
//
// 这里与 FFmpeg DLL 预加载**同一个时机**（显示器/音频枚举完成 + 首帧之后）在**后台线程**
// 按当前配置把编码器建好存进进程级缓存，`Recorder::start` 直接取用（见
// `video::encode::take_prewarmed_encoder`）。预热是**纯优化**：任何失败都只记一行 warning，
// 录制侧照旧"按需同步初始化"（即改动前的行为），绝不 panic、绝不阻止录制。

/// 预热跳过原因**只记一次**（"内容模式不含画面"这种原因每帧都可能重复）。
static PREWARM_SKIP_LOGGED: AtomicBool = AtomicBool::new(false);

/// **按当前配置在后台预热 OpenH264 编码器**（非阻塞；同键重复调用是空操作）。
///
/// 返回 `true` 表示本次真的起了后台预热线程。
///
/// 调用时机（`src/ui/main_page.rs`）：
/// * 显示器/音频枚举完成、FFmpeg DLL 预加载的**同一帧**；
/// * 用户改了配置（分辨率/帧率/码率/区域…）的那一帧 —— 键变了就重新预热，
///   键没变则是空操作（一次互斥锁 + 一次 `GetMonitorInfoW`，不阻塞 UI 帧）。
pub fn prewarm_encoder_once(cfg: &crate::config::Config, log: Option<fn(&str)>) -> bool {
    use crate::video::encode::{prewarm_encoder_async_logged, PrewarmOutcome};
    // 把 sink 也交给后台线程：GUI 没有 tracing 订阅者，"预热完成/失败"只有写进
    // `cap.log` 才在用户机器上看得见（见 `prewarm_encoder_async_logged` 的说明）。
    let outcome = prewarm_encoder_async_logged(cfg, log);
    match &outcome {
        PrewarmOutcome::Spawned => {
            PREWARM_SKIP_LOGGED.store(false, Ordering::Relaxed);
            log_line(log, "[cap] OpenH264 编码器预热已启动（后台线程；按下开始录制时直接复用）");
        }
        PrewarmOutcome::Skipped(why) => {
            // 只记一次：这个原因在"每帧都调用"的场景下会重复出现，刷屏没有意义
            if !PREWARM_SKIP_LOGGED.swap(true, Ordering::Relaxed) {
                log_line(
                    log,
                    &format!("[cap] OpenH264 编码器预热跳过：{why}（不影响录制）"),
                );
            }
        }
        PrewarmOutcome::ThreadFailed => {
            log_line(
                log,
                "[cap] OpenH264 编码器预热警告：后台线程创建失败（不影响录制，按下开始时同步初始化）",
            );
        }
        // 已经有同键实例 / 已在预热 / 同键之前失败过：都不是新信息，不记日志
        PrewarmOutcome::AlreadyWarm
        | PrewarmOutcome::InFlight
        | PrewarmOutcome::FailedBefore => {}
    }
    outcome.spawned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 格式必须稳定：脚本按 `t=` / `phase=` / `dur=` 解析，字段顺序与单位都不能漂。
    #[test]
    fn line_render_is_stable_and_parseable() {
        let line = Line {
            t_ms: 123,
            phase: "ui_first_frame",
            scope: "frame",
            dur_tenths_ms: 2104,
            extra: None,
        };
        assert_eq!(
            line.render(),
            "[cap][t=123ms] phase=ui_first_frame scope=frame dur=210.4ms"
        );
    }

    /// `extra` 里的双引号必须被替换掉，否则一行日志会被拆成两段、把解析脚本带偏。
    #[test]
    fn line_render_escapes_quotes_in_extra() {
        let line = Line {
            t_ms: 5,
            phase: "p",
            scope: "app",
            dur_tenths_ms: 0,
            extra: Some("显示器 \"A\" 2 台".to_string()),
        };
        let rendered = line.render();
        assert!(rendered.ends_with("extra=\"显示器 'A' 2 台\""), "{rendered}");
        assert_eq!(rendered.matches('"').count(), 2, "整行只应有一对引号: {rendered}");
    }

    /// 空 `extra` 不应产出 `extra=""`（解析脚本会把它当成"有附注但为空"）。
    #[test]
    fn line_render_omits_empty_extra() {
        let line = Line {
            t_ms: 1,
            phase: "p",
            scope: "app",
            dur_tenths_ms: 9,
            extra: Some(String::new()),
        };
        assert_eq!(line.render(), "[cap][t=1ms] phase=p scope=app dur=0.9ms");
    }

    /// 未启用时打点必须是**纯空转**：不写任何东西、也不报错（单测里大量调用 mark）。
    #[test]
    fn marks_are_noop_when_disabled() {
        assert!(!ENABLED.load(Ordering::Acquire), "测试进程不应启用打点");
        mark("should_not_appear", "app");
        mark_dur("should_not_appear", "app", Duration::from_millis(1));
        let v = timed("should_not_appear", "app", || 41 + 1);
        assert_eq!(v, 42);
        assert_eq!(dropped_marks(), 0);
    }

    /// 启用 + 收集：打点必须经 sink 落地，且相对起点毫秒数单调不减。
    #[test]
    fn enabled_marks_reach_sink_in_order() {
        use std::sync::Mutex;
        static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn sink(line: &str) {
            SEEN.lock().unwrap().push(line.to_string());
        }
        enable(sink);
        mark_dur("unit_a", "app", Duration::from_millis(2));
        mark_dur_extra("unit_b", "app", Duration::from_millis(3), "2 个设备");
        clear_sink();
        let seen = SEEN.lock().unwrap().clone();
        assert!(seen.len() >= 2, "至少两条打点: {seen:?}");
        let a = seen.iter().find(|l| l.contains("phase=unit_a")).expect("unit_a");
        let b = seen.iter().find(|l| l.contains("phase=unit_b")).expect("unit_b");
        assert!(a.contains("scope=app"), "{a}");
        assert!(b.contains("extra=\"2 个设备\""), "{b}");
        // `unit_a` 记 2ms：`dur=` 是它自己的耗时（允许 ±1 个 0.1ms 的取整误差）
        let dur: f64 = a
            .split("dur=")
            .nth(1)
            .and_then(|s| s.split("ms").next())
            .and_then(|s| s.parse().ok())
            .expect("dur 字段应能解析成数字");
        assert!((dur - 2.0).abs() < 1.0, "unit_a dur 应约为 2ms，实际 {dur}");
    }

    /// `elapsed_ms` 不应倒流，且 `start()` 是幂等的（零点只取一次）。
    #[test]
    fn start_is_stable_and_elapsed_monotonic() {
        let a = start();
        let b = start();
        assert_eq!(a, b, "时间轴零点必须只取一次");
        let first = elapsed_ms();
        let second = elapsed_ms();
        assert!(second >= first, "相对起点毫秒数不应倒流: {first} → {second}");
    }

    // -----------------------------------------------------------------------
    // 内嵌中文子集字体（启动优化第 3 项）
    // -----------------------------------------------------------------------

    /// 内嵌的两个语言包（与 `crate::i18n::BUILTIN` 同一个文件）。
    const LOCALE_JSON: &[&str] = &[
        include_str!("../../locales/zh-CN.json"),
        include_str!("../../locales/en-US.json"),
    ];

    /// 从 TTF 二进制里读出 `cmap` 覆盖的码点集合。
    ///
    /// 为什么不引第三方解析库：`Cargo.toml` 里**不允许新增依赖**（本任务的红线），
    /// 而 sfnt 的表目录 + `cmap` format 4/12 的解析只有几十行 —— 自己解析一次，
    /// 换来"子集缺字"这个回归**能在 `cargo test` 里被挡住**。
    fn font_codepoints(ttf: &[u8]) -> Vec<u32> {
        fn u16_at(b: &[u8], at: usize) -> u16 {
            u16::from_be_bytes([b[at], b[at + 1]])
        }
        fn u32_at(b: &[u8], at: usize) -> u32 {
            u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
        }

        assert!(ttf.len() > 12, "字体文件太小，不像是 sfnt");
        // sfnt 版本：0x00010000（TrueType）或 'true'/'OTTO'
        let version = u32_at(ttf, 0);
        assert!(
            version == 0x0001_0000 || version == 0x7472_7565 || version == 0x4F54_544F,
            "不是合法的 sfnt/ttf：version=0x{version:08X}"
        );
        let num_tables = u16_at(ttf, 4) as usize;
        let mut cmap_off = None;
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            if &ttf[rec..rec + 4] == b"cmap" {
                cmap_off = Some(u32_at(ttf, rec + 8) as usize);
            }
        }
        let cmap = cmap_off.expect("字体里没有 cmap 表");
        let num_sub = u16_at(ttf, cmap + 2) as usize;
        let mut out: Vec<u32> = Vec::new();
        for i in 0..num_sub {
            let sub = cmap + 4 + i * 8;
            let off = cmap + u32_at(ttf, sub + 4) as usize;
            match u16_at(ttf, off) {
                // format 4：BMP 分段映射
                4 => {
                    let seg_x2 = u16_at(ttf, off + 6) as usize;
                    let segs = seg_x2 / 2;
                    let end_base = off + 14;
                    let start_base = end_base + seg_x2 + 2;
                    let delta_base = start_base + seg_x2;
                    let range_base = delta_base + seg_x2;
                    for s in 0..segs {
                        let end = u16_at(ttf, end_base + s * 2) as u32;
                        let start = u16_at(ttf, start_base + s * 2) as u32;
                        let delta = u16_at(ttf, delta_base + s * 2);
                        let range_off = u16_at(ttf, range_base + s * 2);
                        if start == 0xFFFF {
                            continue;
                        }
                        for cp in start..=end {
                            let gid = if range_off == 0 {
                                (cp + delta as u32) & 0xFFFF
                            } else {
                                let addr = range_base + s * 2 + range_off as usize + (cp - start) as usize * 2;
                                if addr + 1 >= ttf.len() {
                                    0
                                } else {
                                    let g = u16_at(ttf, addr);
                                    if g == 0 { 0 } else { (g as u32 + delta as u32) & 0xFFFF }
                                }
                            };
                            if gid != 0 {
                                out.push(cp);
                            }
                        }
                    }
                }
                // format 12：完整码点分段映射
                12 => {
                    let groups = u32_at(ttf, off + 12) as usize;
                    for g in 0..groups {
                        let base = off + 16 + g * 12;
                        let start = u32_at(ttf, base);
                        let end = u32_at(ttf, base + 4);
                        for cp in start..=end {
                            out.push(cp);
                        }
                    }
                }
                // 其它 format（0/6/13…）本字体用不到：跳过即可
                _ => {}
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// **回归防线**：语言包里的每一个**非 ASCII** 字符都必须在内嵌子集字体里有字形。
    ///
    /// 为什么专门测这一条：子集是**构建期**用脚本从"当时的文案"抽出来的；以后有人改了
    /// `locales/*.json`（加了一个新汉字）却忘了重跑 `scripts/make-font-subset.ps1`，
    /// 界面就会在"首帧之后、完整系统字体追加完成之前"的窗口期显示方框。
    /// 这条测试会让那种改动**当场编译期后第一次 `cargo test` 就失败**。
    ///
    /// 允许白名单里的字符缺席：它们不是中文字形，本来就由 egui 内置字体提供
    /// （ASCII 走内置拉丁字体；`⚠` 走内置 emoji 字体）。
    #[test]
    fn embedded_font_subset_covers_all_locale_glyphs() {
        let cps = font_codepoints(SUBSET_FONT_BYTES);
        assert!(!cps.is_empty(), "内嵌子集字体解析出来 0 个码位，解析器或字体有问题");
        let set: std::collections::HashSet<u32> = cps.iter().copied().collect();
        assert!(
            set.contains(&('显' as u32)) && set.contains(&('录' as u32)),
            "子集字体里必须有界面最常用的汉字（例如 显/录）"
        );

        // 允许缺席的字符：**只列 ASCII 之外的**（ASCII 由内置拉丁字体承担）
        const ALLOW_MISSING: &[char] = &['\u{26A0}', '\u{2194}', '\u{2713}', '\u{2714}', '\u{2717}', '\u{2718}'];

        let mut missing: Vec<char> = Vec::new();
        for json in LOCALE_JSON {
            for ch in json.chars() {
                if ch.is_ascii() || ALLOW_MISSING.contains(&ch) {
                    continue;
                }
                if !set.contains(&(ch as u32)) {
                    missing.push(ch);
                }
            }
        }
        missing.sort_unstable();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "语言包里有 {} 个字符不在内嵌子集字体里：{}\n\
             修法：重跑 `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\\make-font-subset.ps1`",
            missing.len(),
            missing.iter().collect::<String>()
        );
    }

    /// 子集字体必须**明显小于**系统完整字体：这是"启动时少读十几 MB"的全部依据。
    ///
    /// 系统 `msyh.ttc` 在本机是 19.7 MB；子集若超过 2 MB 就说明字形集合被放得太宽
    /// （曾经把 GB2312 一级字库整个塞进去 → 2.9 MB → 解析开销又回来了）。
    #[test]
    fn embedded_font_subset_is_actually_small() {
        let kb = SUBSET_FONT_BYTES.len() / 1024;
        assert!(
            (10..2048).contains(&kb),
            "子集字体 {kb} KiB 不在合理区间（10 KiB ~ 2 MiB）：太小可能是生成失败，太大就失去意义"
        );
        // 名字常量与 main.rs 一致（防止两侧漂移成两个 key，追加兜底字体时对不上）
        assert_eq!(SUBSET_FONT_NAME, "cjk-subset");
    }

    // -----------------------------------------------------------------------
    // FFmpeg DLL 预加载：名字清单必须与 build.rs **同源**
    // -----------------------------------------------------------------------

    /// 从 `build.rs` 的源码文本里读出 `FFMPEG_LIB_STEMS`（库名前缀，去掉版本号）。
    ///
    /// 为什么"读源码"而不是在测试里再抄一份前缀：本测试的**目的**就是"两侧不许各写一份
    /// 名字"，抄一份等于把被测对象搬进了测试里。
    fn build_rs_lib_stems() -> Vec<String> {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/build.rs"))
            .expect("build.rs 应可读");
        let line = text
            .lines()
            .find(|l| l.contains("FFMPEG_LIB_STEMS: &[&str]"))
            .expect("build.rs 里应有 FFMPEG_LIB_STEMS 定义");
        let rhs = line.split('=').nth(1).expect("FFMPEG_LIB_STEMS 应有 = 赋值");
        let inner = rhs
            .split('[')
            .nth(1)
            .and_then(|s| s.split(']').next())
            .expect("FFMPEG_LIB_STEMS 应是 &[\"a\", \"b\"] 形式");
        inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 按 build.rs 的同一条规则（后缀 `.dll` + 前缀命中）从 `$FFMPEG_DIR/bin` 独立推导一份清单。
    fn expected_dll_names_from(bin: &std::path::Path, stems: &[String]) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(bin)
            .expect("FFMPEG_DIR/bin 应可读")
            .flatten()
            .filter_map(|e| {
                e.path()
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase())
            })
            .filter(|n| n.ends_with(".dll"))
            .filter(|n| stems.iter().any(|stem| n.starts_with(stem.as_str())))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// **单一来源的回归防线**：运行期清单（`include!` 进来的 const）必须等于
    /// "用 `build.rs` 的规则、在 `$FFMPEG_DIR/bin` 上现算一遍"的结果。
    ///
    /// 这一条同时挡住三种漂移：
    /// ① 有人手工在生成文件里改了名字；② `build.rs` 的规则改了却没重新生成；
    /// ③ 生成文件被误接到别的清单上（例如写死一份 `avcodec-63.dll` 的旧名字）。
    #[test]
    fn runtime_dll_names_equal_build_rs_rule_applied_to_ffmpeg_dir() {
        let Ok(dir) = std::env::var("FFMPEG_DIR") else {
            // 没设 FFMPEG_DIR 时 `build.rs` 生成的就是**空清单**，无可比对（本机始终有设）
            assert!(
                ffmpeg_dlls::FFMPEG_RUNTIME_DLLS.is_empty(),
                "没设 FFMPEG_DIR 时清单必须是空的，实际 {:?}",
                ffmpeg_dlls::FFMPEG_RUNTIME_DLLS
            );
            return;
        };
        let bin = std::path::Path::new(&dir).join("bin");
        let stems = build_rs_lib_stems();
        assert!(!stems.is_empty(), "build.rs 的 FFMPEG_LIB_STEMS 不该为空");
        let expected = expected_dll_names_from(&bin, &stems);
        let mut actual: Vec<String> = ffmpeg_dlls::FFMPEG_RUNTIME_DLLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        actual.sort();
        assert_eq!(
            actual, expected,
            "运行期 DLL 清单与 $FFMPEG_DIR/bin 现算的结果不一致（build.rs 的生成步骤漏了？）"
        );
    }

    /// 清单里的每一项都必须是"命中 build.rs 前缀的 `.dll`"——**不许**出现手写死名字。
    ///
    /// 单列这一条是因为"名字对不上"在运行期是**静默失效**：`LoadLibraryW` 只记一条 warning，
    /// 而延迟加载会退回"按需映射"，用户看到的现象只是"按钮又卡一下"，极难定位。
    #[test]
    fn runtime_dll_names_are_dll_files_matching_build_stems() {
        let stems = build_rs_lib_stems();
        for name in ffmpeg_dlls::FFMPEG_RUNTIME_DLLS {
            assert!(name.ends_with(".dll"), "{name} 不是 .dll");
            assert_eq!(*name, name.to_ascii_lowercase(), "{name} 应为小写");
            assert!(
                stems.iter().any(|stem| name.starts_with(stem.as_str())),
                "{name} 不匹配 build.rs 的任何前缀 {stems:?}"
            );
        }
    }

    /// 版本号只应出现在 `$FFMPEG_DIR/bin` 的实际文件名里 —— `build.rs` 里**不许**再写一份
    /// `"avcodec-63.dll"` 这样的字面量（那正是"改版本号漏改一处"的来源）。
    ///
    /// 注意：`build.rs` 的**注释**里可以提这些名字（帮助阅读），这里查的是**带引号的字面量**。
    #[test]
    fn build_rs_does_not_hardcode_versioned_dll_names() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/build.rs"))
            .expect("build.rs 应可读");
        for name in ffmpeg_dlls::FFMPEG_RUNTIME_DLLS {
            let literal = format!("\"{name}\"");
            assert!(
                !text.contains(&literal),
                "build.rs 里出现了写死的版本化 DLL 名 {literal}：名字必须只在 $FFMPEG_DIR/bin 里有一份"
            );
        }
    }

    /// 一次性判定：第二次调用不再触发（`main_page.rs` 每帧都会调它）。
    #[test]
    fn preload_claim_is_one_shot() {
        let flag = AtomicBool::new(false);
        assert!(claim_once(&flag), "第一次应认领成功");
        assert!(!claim_once(&flag), "第二次不应再认领");
        assert!(!claim_once(&flag), "之后都不应再认领");
    }

    /// 预加载失败**只记日志、绝不 panic**：给一个不存在的 DLL 名，断言"成功 0 条 + 一条警告"。
    ///
    /// 这条覆盖的正是不许发生的形态：因为一个 DLL 加载不了就让录制路径出事。
    #[test]
    fn preload_failure_is_only_logged() {
        use std::sync::Mutex;
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn sink(line: &str) {
            LINES.lock().unwrap().push(line.to_string());
        }
        LINES.lock().unwrap().clear();
        // 名字故意不存在（也不匹配任何 build.rs 前缀，纯路径问题）
        preload_ffmpeg_dlls_blocking(&["cap-no-such-dll-0.dll", "cap-no-such-dll-1.dll"], Some(sink));
        let lines = LINES.lock().unwrap().clone();
        assert!(
            lines.iter().any(|l| l.contains("FFmpeg 预加载警告") && l.contains("0/2")),
            "应有一条 0/2 的警告：{lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.contains("预加载完成")),
            "有失败时不应报「完成」：{lines:?}"
        );
    }

    /// 空清单（构建时没设 `FFMPEG_DIR`）时预加载必须**安静跳过**，不能去 `LoadLibraryW`。
    #[test]
    fn preload_with_empty_list_is_a_noop() {
        use std::sync::Mutex;
        static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn sink(line: &str) {
            LINES.lock().unwrap().push(line.to_string());
        }
        LINES.lock().unwrap().clear();
        preload_ffmpeg_dlls_blocking(&[], Some(sink));
        let lines = LINES.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "只应有一条「跳过」说明：{lines:?}");
        assert!(lines[0].contains("跳过"), "{lines:?}");
    }
}
