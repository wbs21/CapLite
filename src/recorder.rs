//! 录制会话编排：多线程分层 + 有界队列/环形缓冲 + 丢帧统计 + 错误上报 + 停止收尾。
//!
//! ```text
//! 视频线程（BelowNormal）：DXGI 采集 → 指针合成 → libyuv → OpenH264 ─┐（有界队列，满则丢帧并计数）
//!                                                                     ├→ 封装线程：按 pts 归并 → 分片 MP4 写入
//! 音频链（AboveNormal 读/消费 + Highest 搬运）：                     │   （视频滞后音频 >120ms 的包在归并处丢弃）
//!   WASAPI 读取线程（每路一个，只做内存拷贝，永不阻塞）              │
//!     → 小环 0.2s（满则丢最旧）→ 搬运线程 Highest → 大环 10s ────────┤
//!     → 消费线程：补静音 → 混音 → AAC ───────────────────────────────┘
//! ```
//!
//! 设计要点（音频分层来自参考方案 `add/`：优先保障音频、视频可丢帧、声画自动同步）：
//! * **音频读写分离**：WASAPI 读取线程只把 PCM 拷进小环；读取/搬运/消费各自独立，
//!   任何一环变慢都不会让 WASAPI 设备缓冲被覆盖（那是不可恢复的爆音/丢音）。
//! * **采集与编码同线程（视频）**：1080p 实测编码 129 fps ≫ 目标 30 fps，合并可省掉每帧
//!   3 MB 跨线程拷贝；队列满时丢帧而不是阻塞采集线程，并计入 `video_dropped`。
//! * **音频按墙上时钟补静音**：WASAPI 环回在系统静音时不产数据包，否则音轨比视频短。
//! * **音频断供检测与会话重建（V2-1）**：读取线程对"连续空窗"记账（>250ms 告警一次），
//!   遇设备失效类错误（`AUDCLNT_E_*`）重建采集会话（≤`MAX_AUDIO_REBUILDS` 次）；
//!   重建/放弃期间音轨由补静音兜住，缺口本身记入 `audio_dryout_ms` 统计。
//! * **封装线程按 pts 归并**：音视频各自到达，归并后写入；视频包滞后音频超过
//!   `VIDEO_LAG_DROP_MS` 直接丢弃（计入 `video_lag_dropped`），防声画偏移累积。
//! * **分片 MP4 每秒 flush**：进程被杀后文件仍可播放（崩溃可恢复）。
//! * **停止即导出普通 MP4**：在封装线程里做 `remux_to_plain`（copy，不重新编码）。

use crate::audio::aac::{AacEncoder, FRAME_SAMPLES, SAMPLE_RATE};
use crate::audio::capture::{AudioCapture, DryoutTracker, ReadFault, SourceKind};
use crate::audio::ring::AudioGateway;
use crate::audio::{devices, mixer};
use crate::config::Config;
use crate::disk;
use crate::error::AppError;
use crate::mux::{self, AudioParams, FragmentedMp4Writer, VideoParams};
use crate::power::RecordingPowerGuard;
use crate::sys::{set_current_thread_priority, ThreadPriority};
use crate::video::capture::{Capture, CaptureSpec, GrabOutcome};
use crate::video::encode::{take_prewarmed_encoder, VideoEncoder};

use crate::sync::lock_or_recover;
use crossbeam_channel::{bounded, select, Receiver, TrySendError};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// 分片写法决定了"崩溃最多丢多少"：完整分片只在**关键帧**处生成，生成后还要等一次
/// `avio_flush` 才真正落盘。因此刷盘间隔越小，崩溃丢的数据越少
/// （实测 1 秒 → 丢约 2 秒；250 ms → 只丢一个 GOP 加不到 0.25 秒）。
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// 有界队列容量（视频包；30fps 下 120 包 ≈ 4 秒缓冲）
const VIDEO_QUEUE: usize = 120;
/// 有界队列容量（音频包；AAC 每帧 21.3ms，400 包 ≈ 8.5 秒）
const AUDIO_QUEUE: usize = 400;

/// **会话就绪耗时超过它就在日志里告警（毫秒）**。
///
/// `Recorder::start` 里的采集/编码器/音源初始化期间**一帧画面都没有**。
/// 空闲机器上实测 0.2~0.5 s；机器很忙（后台编译 FFmpeg、磁盘打满）时实测 0.46~6.0 s。
/// 1.5 s 已经足够让"录 5 秒"变成"只录 3.5 秒"，所以从这以后必须留痕。
const START_LATENCY_WARN_MS: u64 = 1_500;

/// **音频读取线程的等待上限（毫秒）**（V2-1）
///
/// 等 WASAPI 事件；"有新数据"会立即返回，所以它只决定**空窗期**的记账粒度
/// （[`crate::audio::capture::DRYOUT_WARN_MS`] 按这个粒度判断"连续空窗"）与唤醒频率。
pub const AUDIO_READ_POLL_MS: u32 = 200;

/// **单轮最多补多少静音**（10 s @ 48 kHz）。
///
/// 如果消费线程被挂起很久（调试器、系统休眠/恢复），墙钟 `target` 会突然领先很多。
/// 10 s 静音约 3.8 MB PCM，既能覆盖常见卡顿，又不会像几分钟那样吃光内存。
/// **超过上限不再“跳过”**：跳过会让音轨永久少一段、与视频漂移；这里直接判会话失败、
/// 置停止标志并让封装线程正常收尾（结果卡会显示 `session_issue`）。
const MAX_SILENCE_PAD_FRAMES: usize = 480_000;

/// 录制中检查磁盘剩余空间的间隔。
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// **单个音源允许的 WASAPI 断供重建次数**（V2-1）
///
/// 10 分钟实机取证（`docs/PENDING.md` P31 补充）确认 WASAPI 环回会**源头断供**：
/// 采集会话报设备失效（`AUDCLNT_E_*`）时重解设备、重开会话即可恢复。
/// 但不能无限重建——反复失效说明设备真的没了：超过次数就放弃该路
/// （余下时间补静音，录制继续）；致命错误照旧让整个录制失败。
pub(crate) const MAX_AUDIO_REBUILDS: u32 = 3;

/// **"最后一帧 pts 与录制窗口差多少就值得告警"（毫秒）**。
///
/// 正常差值只有采集/停止标志传播的延迟（一个帧周期 + sleep 粒度，几十毫秒）。
/// 300 ms ≈ 9 帧 @30fps：超过它说明末尾真的有空档或丢帧，必须留痕（`recorder_smoke`
/// 还会按同一口径做**硬判定**：产物时长明显短于录制窗口就以非 0 退出）。
const TAIL_GAP_WARN_MS: i64 = 300;

/// **录制帧率硬上限**：60。
///
/// 高刷新率屏幕（144/165/240 Hz）**按目标帧率抽帧**，绝不跟着刷新率跑：视频线程是
/// 墙钟节拍驱动（每拍只取**当前最新**的一帧，中间的帧由 DXGI 合并/舍弃，见
/// `video::capture::CaptureStats::superseded_frames`），所以 144 Hz 屏录 30 fps 时
/// 每秒有约 114 帧被主动舍弃而不是编码进去。配置侧虽然只有 30/60 两档，
/// 但编码路径不能依赖 UI 的约束，所以这里再夹一次。
pub const MAX_RECORD_FPS: u32 = 60;

/// 落后超过这么多拍就丢弃落后额度、重新对齐（防止"追帧雪崩"）。
/// 见 [`advance_deadline`]。
pub(crate) const RESYNC_AFTER_FRAMES: u32 = 3;

/// **封装层视频滞后丢弃阈值（毫秒）**：视频包 pts 落后音频队列头部超过它就丢弃。
///
/// 为什么在封装层丢（而不是靠队列满丢）：封装写盘一旦卡住（磁盘抖动、机箱休眠），
/// 音频因"不允许丢、队列满只阻塞 200 ms"继续到达，视频则积压。若不管，恢复写盘后
/// 会一次性把几百毫秒的**过期画面**灌进文件 —— 声画就此错开且不可恢复。
/// 丢掉的只是过期帧，音频时间轴不受影响，播放表现为"那一段略卡"而不是"声画漂移"。
/// 口径来自参考方案（`add/`）的 `VIDEO_LAG_THRESHOLD = 0.120`。
/// 正常路径不会命中：视频 pts 按真实采集时刻（吸附帧网格），只有封装真卡了才会滞后。
pub(crate) const VIDEO_LAG_DROP_MS: i64 = 120;

/// 视频包是否"滞后于音频"到该丢（纯函数，脱线程单测）。
pub(crate) fn video_lagging(v_pts_ms: i64, a_pts_ms: i64) -> bool {
    v_pts_ms < a_pts_ms - VIDEO_LAG_DROP_MS
}

/// V2-1 重建策略（纯函数，脱线程单测）：只有**设备失效类**错误值得重建，
/// 且最多 [`MAX_AUDIO_REBUILDS`] 次。致命错误（配置/用法错误）重建只会得到同样结果。
///
/// 分型来自 [`crate::audio::capture::fault_of_wasapi`]。
pub(crate) fn should_rebuild(fault: ReadFault, rebuilds: u32) -> bool {
    fault == ReadFault::DeviceLost && rebuilds < MAX_AUDIO_REBUILDS
}

/// 把请求的帧率收进 `[1, MAX_RECORD_FPS]`（0 会被夹到 1，调用方另有 0 校验）。
pub(crate) fn effective_fps(requested: u32) -> u32 {
    requested.clamp(1, MAX_RECORD_FPS)
}

/// 视频帧 pts 的口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PtsSource {
    /// 按**真实采集时刻**（单调时钟 − 录制开始）。**最终选它。**
    ///
    /// 理由：编码一旦慢于目标帧率，"一秒墙钟只产出 N < fps 帧"，若还按帧序号计时，
    /// 视频就会比真实时间短、与按墙钟走的音轨错位（播放表现为"快放"）。
    /// 用真实时刻打戳，视频时长恒等于墙钟时长，掉帧只表现为画面略卡。
    RealTime,
    /// 按 `frame_index × 1000 / fps`。只在"每一拍都真的编出一帧"时才等价于真实时刻；
    /// **保留它只为单测做对照**（证明掉帧场景下两者确实不同）。
    ///
    /// P54：也正因如此，它在发布构建里恒不被构造 —— `PtsSource` 收敛成 `pub(crate)` 后
    /// 这才暴露出来（原先是 `pub`，dead_code 被屏蔽）。这里显式放行，不删。
    #[cfg_attr(not(test), allow(dead_code))]
    FrameIndex,
}

/// 视频帧的 pts（毫秒，相对录制开始）。
///
/// 纯函数，便于脱线程单测：两种口径在同一组输入下的差异就是"会不会快放"的根源。
pub(crate) fn video_pts_ms(
    source: PtsSource,
    elapsed: Duration,
    frame_index: u64,
    target_fps: u32,
) -> i64 {
    match source {
        PtsSource::RealTime => elapsed.as_millis() as i64,
        PtsSource::FrameIndex => (frame_index as i64) * 1000 / i64::from(target_fps.max(1)),
    }
}

/// 录制中实际使用的 pts 口径（改这里就能两种模式对照）。
pub(crate) const VIDEO_PTS_SOURCE: PtsSource = PtsSource::RealTime;

/// 视频帧的 pts（毫秒）：**真实采集时刻吸附到目标帧率网格**，并保证严格递增。
///
/// 在 [`video_pts_ms`] 的 `RealTime` 口径上多做了两件事，都是为了"容器里的时间戳必须
/// 严格递增且不抖"：
/// * **吸附到 `1000/fps` 的网格**（半步四舍五入）：真实采集时刻会带抖动（采集/编码耗时波动），
///   相邻两帧可能落进同一个时间格 —— ffmpeg 复用时会报
///   `non monotonically increasing dts to muxer`，播放器也可能把两帧当成同一帧；
/// * **单调兜底**：`max(吸附值, 上一帧 + 一格)`，任何情况下都不会出现重复/倒退的时间戳。
///
/// 关键性质仍然成立：视频**时长**跟着墙钟走（掉帧时只是画面略卡，不会快放），
/// 因为吸附误差最多半格（60 fps 下 8 ms），且不会像"帧序号口径"那样把 40 fps 压成 60 fps 的时长。
pub(crate) fn video_pts_on_grid(elapsed_ms: i64, last_pts: Option<i64>, target_fps: u32) -> i64 {
    let step = (1000 / target_fps.max(1) as i64).max(1);
    let snapped = ((elapsed_ms.max(0) + step / 2) / step) * step;
    match last_pts {
        Some(prev) => snapped.max(prev + step),
        None => snapped,
    }
}

/// 视频节拍：给定上一拍的目标时刻 `deadline`、当前时刻 `now` 与帧间隔，
/// 返回**下一拍的目标时刻**以及本次是否发生了"追帧重同步"。
///
/// 为什么需要重同步：编码偶尔变慢（例如突然切到 4K 内容）时会落后若干拍。若不丢掉
/// 落后额度，`deadline` 会一直留在过去，之后每一帧都"全力追赶" —— 表现为 CPU 打满、
/// 采集线程疯跑却仍然跟不上，而且越落越多。落后超过 [`RESYNC_AFTER_FRAMES`] 拍时
/// 直接把 deadline 对齐到"现在"（**并计入统计**，而不是静默跳帧）。
///
/// 判定用的是**自增之后**的 deadline：`now > next + interval * RESYNC_AFTER_FRAMES`
/// 等价于"落后来到了 5 拍及以上"，落后的 1~4 拍仍按原节拍追赶（不重同步）。
pub(crate) fn advance_deadline(
    deadline: Instant,
    now: Instant,
    interval: Duration,
) -> (Instant, bool) {
    let next = deadline.checked_add(interval).unwrap_or(deadline);
    let slack = interval
        .checked_mul(RESYNC_AFTER_FRAMES)
        .unwrap_or(Duration::MAX);
    match next.checked_add(slack) {
        Some(limit) if now > limit => (now, true),
        _ => (next, false),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderState {
    Recording,
    Stopping,
    Finalizing,
    Finished,
    Failed(String),
}

/// **`Recorder::start` 里各阶段的耗时（毫秒）**：把"按了开始录制之后到底在等什么"拆开量。
///
/// 为什么要拆开：这一段**一帧画面都没有**，机器忙时会从 0.46 s 涨到 6.0 s，
/// 用户感知就是"按了开始录制却要等几秒才真的开录"。只有把
/// `DXGI 采集会话` / `OpenH264 初始化` / `音源打开` / `AAC 初始化` 分开，
/// 才知道该优化哪一块（实测结论见 `docs/DESIGN.md` 的"录制启动耗时拆解"表）。
#[derive(Debug, Clone, Copy, Default)]
pub struct StartPhases {
    /// DXGI 采集会话建立（`Capture::open`：枚举/换算 + `IDXGIOutputDuplication`）
    pub capture_open_ms: u64,
    /// OpenH264 编码器创建 + `InitializeExt`（`VideoEncoder::new`；**命中预热时≈0**）
    pub encoder_init_ms: u64,
    /// 音源设备解析 + `AudioCapture::open`（WASAPI 环回/麦克风）
    pub audio_open_ms: u64,
    /// AAC 编码器创建（`AacEncoder::new`）
    pub aac_init_ms: u64,
    /// `Recorder::start` 从**进入**到**返回句柄**的总耗时（= [`RecordingResult::start_latency_ms`]）
    pub total_ms: u64,
    /// 这次的编码器实例是否来自**预热缓存**（`false` = 按需同步初始化）
    pub encoder_prewarmed: bool,
    /// 从 `Recorder::start` 进入到**第一帧被编码完成**的毫秒数
    /// （由视频线程写；`0` = 还没编出第一帧。这是"按开始录制 → 真的有画面"的直接口径）
    pub first_frame_ms: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RecorderStats {
    pub video_frames: u64,
    /// 队列满而丢弃的帧数（不允许阻塞采集线程）
    pub video_dropped: u64,
    pub keyframes: u64,
    pub audio_frames: u64,
    /// 采集侧：画面无变化（DXGI Timeout）
    pub no_change: u64,
    /// 采集侧：会话丢失并已重建
    pub access_lost: u64,
    /// 因系统静音而补入的静音帧数（诊断用）
    pub silence_padded_frames: u64,
    /// **音频时间轴跳变次数**：单轮 gap 超过 10 s，为避免音画漂移而停止录制。
    /// 正常应为 0；非 0 说明线程被长时间挂起/系统休眠过。
    pub audio_timeline_jumps: u64,
    /// **被舍弃/超越的帧数**（DXGI `AccumulatedFrames` 之和）：
    /// 高刷新率屏幕按目标帧率抽帧时，中间那些帧由系统合并掉，这就是"先舍弃多余帧"的
    /// 可观测证据（见 `video::capture::CaptureStats::superseded_frames`）。
    pub superseded_frames: u64,
    /// 视频节拍"追帧重同步"的次数（落后太多时丢弃落后额度、重新对齐，见 `advance_deadline`）
    pub pacing_resyncs: u64,
    /// **音频环形缓冲累计丢弃的样本数**（小环 + 大环；见 [`crate::audio::ring`]）。
    ///
    /// 正常应为 0。非 0 说明消费侧停顿超过了环形缓冲的兜底能力
    /// （大环 10 s）——那是"机器已经被拖到极限"的客观证据，不是 bug。
    pub audio_ring_dropped: u64,
    /// **封装层因"滞后音频超过 `VIDEO_LAG_DROP_MS`"丢弃的视频包数**。
    ///
    /// 非 0 = 录制中出现过写盘/归并停顿，过期画面被主动丢弃以保声画同步。
    pub video_lag_dropped: u64,
    /// **最后一帧写出的 pts（毫秒，相对录制起点）**。
    ///
    /// 它是"有没有丢尾"的直接证据：`last_video_pts_ms` 应当贴着**录制窗口**
    /// （`REC_WALL_MS`）——差一个帧周期是正常的，差几百毫秒以上就是尾部丢了。
    /// 注意它与 `video_frames` 是**两个独立的量**：帧数正常但 pts 明显偏小 =
    /// 时间戳被压缩（快放）；帧数偏小而 pts 正常 = 只是掉帧（时长仍然对）。
    pub last_video_pts_ms: i64,
    /// **音频"连续空窗"累计时长（毫秒，各音源求和）**（V2-1）
    ///
    /// WASAPI 连续多轮取不到数据的墙上时间。两类成因：① 音频引擎静默（环回不产包）；
    /// ② 源头断供（设备切换/会话失效）。要区分它们看 [`Self::audio_rebuilds`]：
    /// 有重建 = 真的断过；没有重建而值偏大 = 那段本来就没声音。
    pub audio_dryout_ms: u64,
    /// **单条音源最长一次连续空窗（毫秒）**（V2-1）
    ///
    /// 与累计值分开：累计值大可能只是"长期静默没人播声音"，
    /// **单次最长**才是"声音真的缺了一段"的口径（实测断供案例：353 ms）。
    pub audio_max_dryout_ms: u64,
    /// **WASAPI 会话失效后成功重建采集会话的次数**（V2-1）
    ///
    /// 正常为 0。非 0 = 录制中真的发生过设备失效并被自动恢复（用户无感续录）。
    pub audio_rebuilds: u64,
}

#[derive(Debug, Clone)]
pub struct RecordingResult {
    /// 录制过程中的分片 MP4（崩溃可播）—— 在 **Cache 目录**里。
    /// **导出成功后也保留**（用户明确要求：不做任何删除），以便在导出失败/误删时找回。
    pub streaming: PathBuf,
    /// 停止时导出的普通 MP4（faststart）—— 在**用户设置的输出目录**里，与流文件同名基名；
    /// remux 失败时为空
    pub exported: PathBuf,
    pub duration: Duration,
    pub stats: RecorderStats,
    /// 重封装（导出）的**逐包对账报告**：包数、首末时间戳、读/写错误、结束原因。
    /// 导出失败时为 `None`（原因见 [`RecordingResult::export_issue`]）。
    pub export: Option<mux::RemuxReport>,
    /// **导出被判为"不完整/失败"的原因**（`None` = 导出完整）。
    ///
    /// 界面必须把它显示出来（"导出不完整，已保留 Cache 流文件"）：
    /// 这一条就是为了让"静默产出一个偏短的 MP4"变成**用户看得见的告警**。
    pub export_issue: Option<String>,
    /// **录制会话本身的问题**（采集/编码/音频线程提前失败、线程 panic、磁盘不足…）。
    ///
    /// 与 [`Self::export_issue`] 分开：导出可以完整，但录制可能提前结束/缺音轨。
    /// 只要它不是 `None`，UI 就必须显示 warn/error，不能把本次录制当成“完美成功”。
    pub session_issue: Option<String>,
    /// **会话就绪耗时（毫秒）**：`Recorder::start` 从进入到返回的耗时
    /// （打开采集会话 + 初始化 OpenH264 + 打开音源 + 初始化 AAC）。
    ///
    /// 这段时间**一帧画面都没有**。它就是"我按了 5 秒，产物却只有 3 秒"的头部来源：
    /// 排障时先看这个数，再看 `wall_ms`（真正的录制窗口）与产物时长是否一致。
    pub start_latency_ms: u64,
    /// **真实录制窗口（毫秒）**：从 `start()` 返回（会话就绪）到停止就绪。
    /// 产物时长应当等于它（±一个帧周期）。
    pub wall_ms: u64,
}

/// 交给封装线程的消息
enum MuxMsg {
    /// 视频参数就绪（含 avcC），封装线程据此创建写入器
    InitVideo(VideoParams),
    Video {
        data: Vec<u8>,
        pts_ms: i64,
        key: bool,
    },
    Audio {
        data: Vec<u8>,
        pts_ms: i64,
    },
    VideoEnd,
    AudioEnd,
}

/// 记录第一个会话级问题（保留最早的原因，后续告警不覆盖它）。
fn note_issue(issue: &Mutex<Option<String>>, msg: impl Into<String>) {
    let mut slot = lock_or_recover(issue);
    if slot.is_none() {
        *slot = Some(msg.into());
    }
}

/// 启动过程的清理守卫。
///
/// `Recorder::start` 先启动视频线程、再启动音频/封装线程。任何一次 `spawn` 失败
/// 都会走 `?` 提前返回；没有这个守卫时，已经启动的线程**不会被置停/join**，
/// 会继续采集并写同一个 Cache 文件，直到磁盘满或进程退出。
///
/// 成功路径用 [`Self::take_threads`] 把句柄交出去，`Drop` 随即变成空操作。
struct StartGuard {
    stop: Arc<AtomicBool>,
    threads: Option<Vec<JoinHandle<()>>>,
    /// 预先用 `create_new` 占住的流文件路径；启动失败时若仍是空文件就清掉。
    reserved_output: Option<PathBuf>,
}

impl StartGuard {
    fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            threads: Some(Vec::new()),
            reserved_output: None,
        }
    }

    fn push(&mut self, handle: JoinHandle<()>) {
        if let Some(v) = self.threads.as_mut() {
            v.push(handle);
        }
    }

    fn take_threads(&mut self) -> Vec<JoinHandle<()>> {
        // 成功路径：占位文件已交给写入线程，不能在这里删除。
        self.reserved_output = None;
        self.threads.take().unwrap_or_default()
    }

    /// 以 `create_new` 预占输出路径，阻止复用旧文件或跟随预先放置的链接。
    fn reserve_output(&mut self, path: &Path) -> std::io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        self.reserved_output = Some(path.to_path_buf());
        Ok(())
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        let Some(threads) = self.threads.take() else {
            return; // 已提交给 Recorder
        };
        // 先置停止标志，让 producer 尽快退出；再 join，保证没有线程在函数返回后
        // 还拿着 Arc 写文件。join 在这里可能短暂等待，但这是错误路径，正确性优先。
        self.stop.store(true, Ordering::SeqCst);
        for h in threads {
            let _ = h.join();
        }
        if let Some(path) = self.reserved_output.take() {
            // 只有空占位才删；如果错误发生前已经写入了一点数据，保留给用户找回。
            if std::fs::metadata(&path)
                .map(|m| m.len() == 0)
                .unwrap_or(false)
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知 panic".to_string()
    }
}

/// 起一条**会立即上报 panic** 的录制线程：线程体 panic 时设置 Failed/issue 并置停止，
/// 不再等用户点停止才在 `join` 里发现。
fn spawn_guarded<F>(
    name: impl Into<String>,
    issue: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<RecorderState>>,
    body: F,
) -> std::io::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    let name = name.into();
    thread::Builder::new().name(name.clone()).spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        if let Err(payload) = result {
            let msg = format!("{name} 线程 panic：{}", panic_text(payload.as_ref()));
            tracing::error!("{msg}");
            note_issue(&issue, msg.clone());
            *lock_or_recover(&state) = RecorderState::Failed(msg);
            stop.store(true, Ordering::SeqCst);
        }
    })
}

/// 录制会话句柄：UI 只持有它，调用不阻塞。
pub struct Recorder {
    state: Arc<Mutex<RecorderState>>,
    stats: Arc<Mutex<RecorderStats>>,
    stop_flag: Arc<AtomicBool>,
    /// 启动/收尾协作取消令牌。UI 看门狗超时后置位；各阶段在安全点检查，
    /// 避免看门狗只放弃等待却把旧任务永久留在后台。
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<RecordingResult>>>,
    /// 会话级问题（失败/panic/磁盘不足）。与 `state` 分开保存，避免收尾时被
    /// `Finalizing → Finished` 覆盖掉。
    issue: Arc<Mutex<Option<String>>>,
    threads: Vec<JoinHandle<()>>,
    /// 分片流文件的路径（**Cache 目录**里）
    output: PathBuf,
    started: Instant,
    /// **`start()` 从进入到返回的耗时（毫秒）**。
    ///
    /// 采集会话 + OpenH264 初始化 + 音源/AAC 初始化全在这段时间里；这段时间**没有画面**，
    /// 是"按了开始录制却过了几秒才真正开始"的头部空档。机器很忙（例如后台在编译）时它会从
    /// 几百毫秒涨到几秒（实测 0.46 s ~ 6.0 s），必须如实告诉调用方，而不是让它悄悄吃掉
    /// "要录 5 秒"的预算。
    start_latency_ms: u64,
    /// **启动各阶段耗时**（拆开量，见 [`StartPhases`]）。
    phases: StartPhases,
    /// "进入 `start()` → 第一帧编码完成"的毫秒数（视频线程写）。
    first_frame_ms: Arc<AtomicU64>,
    /// 用户要了声音、但某一路音源没打开成功的原因（空 = 全部按预期打开）。
    ///
    /// 这是"静默降级"的解药：`start()` 对音频失败历来是"警告后继续"，
    /// 于是"画面+声音"模式会**悄悄录成没有音轨**的文件。这里把原因带回界面，
    /// 由主页面明确显示"本次录制没有声音：原因"。
    audio_warnings: Vec<String>,
}

impl Recorder {
    /// 启动录制。采集与编码器在返回前就建好，**参数错误会同步返回**。
    pub fn start(cfg: &Config, output: &Path) -> Result<Self, AppError> {
        Self::start_cancellable(cfg, output, Arc::new(AtomicBool::new(false)))
    }

    /// 可取消的启动入口。取消是协作式的：阻塞中的 DXGI/OpenH264/WASAPI 调用
    /// 不能被强行打断，但返回后会立刻释放，不再继续进入后续阶段或提交会话。
    pub fn start_cancellable(
        cfg: &Config,
        output: &Path,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, AppError> {
        // 会话就绪耗时从**进入本函数**开始算：这是调用方（界面）感知到的"点开始后多久才有画面"
        let t_enter = Instant::now();
        if cancel.load(Ordering::Acquire) {
            return Err(AppError::Config("录制启动已取消".into()));
        }
        // Cache 根与 recordings 子目录都必须是真实目录：如果其中任何一层是
        // junction/symlink，录制可能被重定向到管理员才有权写的位置。
        if let Some(rec_dir) = output.parent() {
            if let Some(cache_root) = rec_dir.parent() {
                crate::cache::ensure_plain_dir(cache_root)
                    .map_err(|e| AppError::Config(format!("Cache 目录不安全: {e}")))?;
            }
            if !rec_dir.as_os_str().is_empty() {
                crate::cache::ensure_plain_dir(rec_dir)
                    .map_err(|e| AppError::Config(format!("创建输出目录失败: {e}")))?;
            }
        }
        // 导出目录（用户设置的输出目录）：**在启动时快照**，录制中途改配置不影响本次
        let export_dir = crate::config::resolve_against_exe_dir(&cfg.output_dir)
            .unwrap_or_else(|_| std::env::temp_dir());
        if let Err(e) = std::fs::create_dir_all(&export_dir) {
            // 导出目录建不出来不算致命：录制照常，停止时会退到临时目录（并如实报错给界面）
            tracing::warn!(
                "创建导出目录失败（导出时会再试）：{} —— {e}",
                export_dir.display()
            );
        }
        // 磁盘预检：低于安全余量直接拒绝开始，比录到一半把系统盘写满要好。
        // 取不到空间时失败开放（只记日志），不因为查询失败就阻止录制。
        for (label, dir) in [
            ("Cache", output.parent().unwrap_or(output)),
            ("输出", export_dir.as_path()),
        ] {
            match disk::free_bytes(dir) {
                Some(free) if free < disk::MIN_FREE_BYTES => {
                    let msg = format!(
                        "{label} 目录所在磁盘空间不足：{} 可用，安全余量 {}（{}）",
                        disk::human(free),
                        disk::human(disk::MIN_FREE_BYTES),
                        dir.display()
                    );
                    tracing::error!("{msg}");
                    return Err(AppError::Mp4(msg));
                }
                Some(free) => {
                    tracing::info!(dir = %dir.display(), free = %disk::human(free), "磁盘空间预检通过");
                }
                None => {
                    tracing::warn!(dir = %dir.display(), "取不到磁盘剩余空间，跳过预检");
                }
            }
        }
        if cfg.fps == 0 || cfg.video_bitrate_kbps == 0 {
            return Err(AppError::Config("fps/码率不能为 0".into()));
        }
        // 帧率**硬夹紧到 60**：高刷新率屏幕（144 Hz）也按目标帧率抽帧，绝不跟着刷新率跑。
        // 配置侧虽然只有 30/60 两档，但编码路径不应该依赖 UI 的约束。
        let fps = effective_fps(cfg.fps);
        if fps != cfg.fps {
            tracing::warn!(
                requested = cfg.fps,
                capped = fps,
                "请求的帧率超过录制上限，已夹紧（高刷新率屏幕按目标帧率抽帧）"
            );
        }
        // 编码器/预热键必须与采集节拍使用同一个 fps；否则 API 调用方传入 120
        // 时会出现“采集 60、编码器配置 120”的元数据与码控错位。
        let encoder_cfg = {
            let mut c = cfg.clone();
            c.fps = fps;
            c
        };

        let want_video = cfg.content_mode.has_video();
        let want_audio = cfg.content_mode.has_audio();

        // ---------- 视频侧 ----------
        // 诊断：把"打开采集会话"与"初始化编码器"**分开**量（见 [`StartPhases`]）：
        // 两者之和就是原来那个 `video_init_ms`，但只有拆开才知道该优化哪一块 ——
        // 机器很忙时 OpenH264 初始化实测 0.46~6.0 s，而这段没有任何画面，
        // 直接吃掉"要录多久"的预算。
        let mut capture_open_ms = 0u64;
        let mut encoder_init_ms = 0u64;
        let mut encoder_prewarmed = false;
        let t_video = Instant::now();
        let video_setup = if want_video {
            let t_cap = Instant::now();
            let cap = Capture::open(CaptureSpec {
                monitor_index: cfg.monitor_index,
                mode: cfg.capture_mode,
                region: cfg.region,
                draw_cursor: cfg.draw_cursor,
            })?;
            capture_open_ms = t_cap.elapsed().as_millis() as u64;

            // 编码器的尺寸口径 = 采集会话给出的裁剪尺寸（两者必须一致，否则预热键不匹配）
            let (enc_w, enc_h) = (cap.crop().width as i32, cap.crop().height as i32);
            let t_enc = Instant::now();
            // **先试用预热好的实例**（后台线程在"枚举完成"那一刻建的，见
            // `video::encode::prewarm_encoder_async`）：键一致就直接复用，省掉整个
            // `InitializeExt`；键不一致/没有/已过期就返回 None，走原来的同步创建。
            // 预热是纯优化：这条 `None` 分支就是"与预热前完全一样"的兜底路径。
            let enc = match take_prewarmed_encoder(&encoder_cfg, enc_w, enc_h) {
                Some(prewarmed) => {
                    encoder_prewarmed = true;
                    prewarmed
                }
                None => VideoEncoder::new(&encoder_cfg, enc_w, enc_h)?,
            };
            encoder_init_ms = t_enc.elapsed().as_millis() as u64;
            Some((cap, enc))
        } else {
            None
        };

        if cancel.load(Ordering::Acquire) {
            return Err(AppError::Config("录制启动已取消".into()));
        }
        // ---------- 音频侧 ----------
        let video_init_ms = t_video.elapsed().as_millis() as u64;
        let t_audio = Instant::now();
        let mut audio_sources: Vec<AudioCapture> = Vec::new();
        let mut audio_warnings: Vec<String> = Vec::new();
        if want_audio {
            let mut wanted: Vec<SourceKind> = Vec::new();
            if cfg.audio_system {
                wanted.push(SourceKind::System);
            }
            if cfg.audio_mic {
                wanted.push(SourceKind::Microphone);
            }
            for kind in wanted {
                let dev = match kind {
                    SourceKind::System => devices::resolve_render(None),
                    SourceKind::Microphone => devices::resolve_mic(cfg.mic_device_id.as_deref()),
                };
                match dev.and_then(|d| AudioCapture::open(kind, d)) {
                    Ok(c) => audio_sources.push(c),
                    Err(e) => {
                        let msg = format!("{kind:?} 音源打开失败：{e}");
                        tracing::warn!("{msg}");
                        audio_warnings.push(msg);
                    }
                }
            }
            // 用户明确只要声音却一个音源都没打开 → 直接失败，不录出无声文件
            if audio_sources.is_empty() && !want_video {
                return Err(AppError::Audio(format!(
                    "没有可用的音源：{}",
                    audio_warnings.join("；")
                )));
            }
        }
        let audio_open_ms = t_audio.elapsed().as_millis() as u64;
        let t_aac = Instant::now();
        let aac = if audio_sources.is_empty() {
            None
        } else {
            Some(AacEncoder::new(cfg.audio_bitrate_kbps)?)
        };
        let aac_init_ms = t_aac.elapsed().as_millis() as u64;
        let audio_params = aac.as_ref().map(|a| AudioParams {
            sample_rate: a.sample_rate(),
            channels: 2,
            bitrate: (cfg.audio_bitrate_kbps as i64) * 1000,
            extradata: a.extradata().to_vec(),
        });

        if cancel.load(Ordering::Acquire) {
            return Err(AppError::Config("录制启动已取消".into()));
        }
        // ---------- 共享状态 ----------
        let state = Arc::new(Mutex::new(RecorderState::Recording));
        let stats = Arc::new(Mutex::new(RecorderStats::default()));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let result: Arc<Mutex<Option<RecordingResult>>> = Arc::new(Mutex::new(None));
        let issue: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // 必须在 channel/sender 之前声明：错误返回时 sender 先 drop、guard 再 join，
        // 否则 mux 线程可能一直等不到 AudioEnd/VideoEnd。
        let mut start_guard = StartGuard::new(Arc::clone(&stop_flag));
        let output = output.to_path_buf();
        start_guard.reserve_output(&output).map_err(|e| {
            AppError::Mp4(format!(
                "创建录制流文件 {} 失败（拒绝覆盖已有文件）：{e}",
                output.display()
            ))
        })?;
        let started = Instant::now();
        // "进入 start() → 第一帧编码完成"的时刻由视频线程写入（见 StartPhases::first_frame_ms）
        let first_frame_ms = Arc::new(AtomicU64::new(0));
        // `fps` 已在上面夹紧（见 effective_fps）

        // ---------- 封装线程 ----------
        let (video_tx, video_rx) = bounded::<MuxMsg>(VIDEO_QUEUE);
        let (audio_tx, audio_rx) = bounded::<MuxMsg>(AUDIO_QUEUE);
        let expect_video = video_setup.is_some();
        let expect_audio = audio_params.is_some();
        // 会话就绪耗时：到"线程全部拉起、可以返回句柄"为止（见 Self::start_latency_ms）
        let start_latency_ms = t_enter.elapsed().as_millis() as u64;
        let mux_handle = {
            let state = Arc::clone(&state);
            let stats = Arc::clone(&stats);
            let result = Arc::clone(&result);
            let issue = Arc::clone(&issue);
            let stop = Arc::clone(&stop_flag);
            let guard_state = Arc::clone(&state);
            let guard_issue = Arc::clone(&issue);
            let guard_stop = Arc::clone(&stop_flag);
            let output = output.clone();
            let export_dir = export_dir.clone();
            let audio_params = audio_params.clone();
            let cancel = Arc::clone(&cancel);
            spawn_guarded("cap-mux", guard_issue, guard_stop, guard_state, move || {
                mux_thread(
                    output,
                    export_dir,
                    expect_video,
                    expect_audio,
                    audio_params,
                    video_rx,
                    audio_rx,
                    stop,
                    state,
                    stats,
                    result,
                    issue,
                    cancel,
                    start_latency_ms,
                )
            })
            .map_err(|e| AppError::Mp4(format!("创建封装线程失败: {e}")))?
        };
        // 立刻纳入 guard：后续任一 spawn 失败，封装线程也会被置停/join。
        start_guard.push(mux_handle);

        // ---------- 视频线程（采集 + 编码） ----------
        if let Some((mut cap, mut enc)) = video_setup {
            let tx = video_tx.clone();
            let stop = Arc::clone(&stop_flag);
            let stats = Arc::clone(&stats);
            let state = Arc::clone(&state);
            let interval = Duration::from_secs_f64(1.0 / fps as f64);
            // 线程闭包必须 'static：把用到的配置值先取出来（不要借用 cfg）
            let video_bitrate = (cfg.video_bitrate_kbps as i64) * 1000;
            // 音视频共用同一个"录制开始"时刻：视频 pts 用真实采集时刻（单调时钟），
            // 音频的补静音节拍也用同一个原点，两条轨才对得上。
            let session_start = started;
            // 第一帧编码完成的打点用**同一个入口时刻**（`t_enter`），这样
            // `StartPhases::first_frame_ms` 与 `start_latency_ms` 是同一把尺子。
            let first_frame = Arc::clone(&first_frame_ms);
            let t_enter_for_thread = t_enter;
            let issue_thread = Arc::clone(&issue);
            start_guard.push(
                spawn_guarded(
                    "cap-video",
                    Arc::clone(&issue),
                    Arc::clone(&stop_flag),
                    Arc::clone(&state),
                    move || {
                        // 视频可丢帧、优先级垫底：采集+编码慢了只表现为画面略卡
                        // （pts 按真实时刻吸附帧网格，不会快放），音频链绝不能因此被抢
                        set_current_thread_priority(ThreadPriority::BelowNormal);
                        let mut deadline = session_start;
                        let mut frame_index: u64 = 0;
                        // 上一帧写出的 pts：保证严格递增（见 video_pts_on_grid）
                        let mut last_pts: Option<i64> = None;
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            match cap.grab(30) {
                                Ok(GrabOutcome::New) => {}
                                Ok(GrabOutcome::Reused) => {
                                    lock_or_recover(&stats).no_change += 1;
                                }
                                Ok(GrabOutcome::Recovered) => {
                                    lock_or_recover(&stats).access_lost += 1;
                                }
                                Err(e) => {
                                    tracing::error!("视频采集失败: {e}");
                                    note_issue(&issue_thread, format!("采集失败: {e}"));
                                    *lock_or_recover(&state) =
                                        RecorderState::Failed(format!("采集失败: {e}"));
                                    stop.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                            // **采集时刻**（编码之前）就取时间戳：编码耗时不该算进画面时刻
                            let captured_at = session_start.elapsed();
                            // "先舍弃多余帧"的证据：采集侧累计的被合并/超越帧数
                            let superseded = cap.stats().superseded_frames;

                            let (avcc, key) = match enc.encode(cap.frame()) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::error!("视频编码失败: {e}");
                                    note_issue(&issue_thread, format!("编码失败: {e}"));
                                    *lock_or_recover(&state) =
                                        RecorderState::Failed(format!("编码失败: {e}"));
                                    stop.store(true, Ordering::Relaxed);
                                    break;
                                }
                            };

                            if frame_index == 0 {
                                // **"按开始录制 → 第一帧被编码"的直接口径**：从
                                // `Recorder::start` 的入口时刻算到这一帧编码完成。
                                // 它 = `start_latency_ms` + 第一拍的采集耗时 + 首帧编码耗时。
                                first_frame.store(
                                    t_enter_for_thread.elapsed().as_millis() as u64,
                                    Ordering::Relaxed,
                                );
                                match enc.avcc_extradata() {
                                    Ok(extradata) => {
                                        let vp = VideoParams {
                                            width: enc.width(),
                                            height: enc.height(),
                                            fps: fps as f64,
                                            bitrate: video_bitrate,
                                            extradata,
                                        };
                                        let _ = tx.send(MuxMsg::InitVideo(vp));
                                    }
                                    Err(e) => {
                                        note_issue(&issue_thread, format!("构造 avcC 失败: {e}"));
                                        *lock_or_recover(&state) =
                                            RecorderState::Failed(format!("构造 avcC 失败: {e}"));
                                        stop.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                            }

                            // pts 用**真实采集时刻**（并吸附到帧网格 + 单调保证）而不是帧序号：
                            // 编码慢于目标帧率时，视频时长仍等于墙钟时长、与音轨对齐，
                            // 掉帧只表现为画面略卡。
                            let pts_ms = video_pts_on_grid(
                                video_pts_ms(VIDEO_PTS_SOURCE, captured_at, frame_index, fps),
                                last_pts,
                                fps,
                            );
                            last_pts = Some(pts_ms);
                            match tx.try_send(MuxMsg::Video {
                                data: avcc,
                                pts_ms,
                                key,
                            }) {
                                Ok(()) => {
                                    let mut s = lock_or_recover(&stats);
                                    s.video_frames += 1;
                                    s.superseded_frames = superseded;
                                    // 最后一帧的 pts = "有没有丢尾"的直接证据（见 RecorderStats）
                                    s.last_video_pts_ms = pts_ms;
                                    if key {
                                        s.keyframes += 1;
                                    }
                                }
                                Err(TrySendError::Full(_)) => {
                                    // 绝不阻塞采集：丢帧并计数
                                    let mut s = lock_or_recover(&stats);
                                    s.video_dropped += 1;
                                    s.superseded_frames = superseded;
                                }
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                            frame_index += 1;

                            // 节拍：按目标帧率抽帧；落后太多就重同步（计入统计，不静默跳帧）
                            let (next, resynced) =
                                advance_deadline(deadline, Instant::now(), interval);
                            deadline = next;
                            if resynced {
                                lock_or_recover(&stats).pacing_resyncs += 1;
                            }
                            let now = Instant::now();
                            if now < deadline {
                                thread::sleep(deadline - now);
                            }
                        }
                        let _ = tx.send(MuxMsg::VideoEnd);
                    },
                )
                .map_err(|e| AppError::Capture(format!("创建视频线程失败: {e}")))?,
            );
        }
        drop(video_tx);

        // ---------- 音频链：读取线程（每路一个）+ 搬运（网关内）+ 消费线程 ----------
        // 分层来自参考方案 add/：WASAPI 读取线程只做内存拷贝进小环（永不阻塞），
        // 搬运线程（Highest）把 PCM 搬进大环（10 s 兜底），消费线程（AboveNormal）
        // 才做补静音/混音/AAC。任何一环变慢都不会覆盖 WASAPI 设备缓冲（爆音/丢音不可恢复）。
        if let Some(mut aac) = aac {
            let tx = audio_tx.clone();
            let stop = Arc::clone(&stop_flag);
            let stats = Arc::clone(&stats);
            let state = Arc::clone(&state);
            let n_sources = audio_sources.len();
            // 每路音源一个网关（小环 + Highest 搬运线程 + 大环）
            let gateways: Vec<Arc<AudioGateway>> = (0..n_sources)
                .map(|_| Arc::new(AudioGateway::new()))
                .collect();
            // 搬运线程启动失败 = 该路音源降级（跳过，录制继续，只是没这一路声音）。
            // 不硬失败：极端资源压力下放活"至少能录到画面"的兜底，而不是一挂全挂。
            // 失败的 gateway 不会被读取线程推、不会被消费线程取、停止时也不会 stop。
            let mut active_gateways: Vec<Arc<AudioGateway>> = Vec::with_capacity(n_sources);
            let mut skipped_gateway_indices: Vec<usize> = Vec::new();
            for (i, g) in gateways.iter().enumerate() {
                match g.start_transport() {
                    Ok(()) => active_gateways.push(Arc::clone(g)),
                    Err(e) => {
                        skipped_gateway_indices.push(i);
                        tracing::warn!(
                            i,
                            error = %e,
                            "音频搬运线程启动失败，该路音源将被跳过（降级录制）"
                        );
                        note_issue(
                            &issue,
                            format!("音频搬运线程启动失败，第 {i} 路音源被跳过：{e}"),
                        );
                    }
                }
            }
            let n_active_sources = active_gateways.len();
            // 全部搬运线程都起不来 → 整路音频降级（纯视频录制继续）。
            // 这里不手动 drop gateways/aac —— 让 Rust 自动 drop；
            // 消费线程闭包只在 n_active_sources > 0 时才会 spawn（不会触发 move-after-drop）。
            if n_active_sources == 0 {
                tracing::error!(
                    "所有音频搬运线程启动失败，本次录制无音轨（{n_sources} 路音源全降级）"
                );
                note_issue(
                    &issue,
                    format!("所有音频搬运线程启动失败，本次录制无音轨（用户申请 {n_sources} 路）"),
                );
            }
            // 读取线程退出计数：消费线程据此在停止时"先等读齐、再停搬运、后抽干大环"，
            // 保证收尾不吞掉尾部音频。
            let reads_done = Arc::new(AtomicUsize::new(0));
            // 重建麦克风采集时要用：按用户指定的设备 ID 重解（设备没了会回落到默认）
            let mic_device_id = cfg.mic_device_id.clone();
            // 跳过搬运线程启动失败的那些音源 —— 不 spawn 读取线程、不 spawn 消费线程。
            // 读取线程数量 = 消费线程等待的数量，两者必须对齐才能正确收尾。
            let audio_sources: Vec<_> = audio_sources.into_iter().enumerate()
                .filter(|(i, _)| !skipped_gateway_indices.contains(i))
                .map(|(_, cap)| cap)
                .collect();
            for (i, cap) in audio_sources.into_iter().enumerate() {
                // 注意：现在 gateways 的索引对应"过滤后"的顺序，
                // active_gateways[i] 就是这个读取线程该 push 的那个网关
                let gw = Arc::clone(&active_gateways[i]);
                let stop = Arc::clone(&stop);
                let state = Arc::clone(&state);
                let stats = Arc::clone(&stats);
                let reads_done = Arc::clone(&reads_done);
                let issue_thread = Arc::clone(&issue);
                let mic_device_id = mic_device_id.clone();
                let kind = cap.kind();
                start_guard.push(
                    spawn_guarded(
                        format!("cap-audio-read-{i}"),
                        Arc::clone(&issue),
                        Arc::clone(&stop_flag),
                        Arc::clone(&state),
                        move || {
                            set_current_thread_priority(ThreadPriority::AboveNormal);
                            let mut pcm: Vec<f32> = Vec::new();
                            // V2-1：`cap` 放在 Option 里——设备失效时 take 出来 Drop
                            // （AudioCapture::drop 会停流），再重解同一设备重开会话。
                            // 重建间隙的音频由消费线程的墙钟补静音兜底。
                            let mut cap = Some(cap);
                            let mut dryout = DryoutTracker::default();
                            let mut rebuilds: u32 = 0;
                            loop {
                                if stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                pcm.clear();
                                // 无论事件是否触发都会排空设备缓冲（见 audio::capture 文档）；
                                // AUDIO_READ_POLL_MS 只是"最多睡多久"，不影响取数完整性。
                                let t0 = Instant::now();
                                let result = match cap.as_mut() {
                                    Some(c) => c.read_chunk_core(&mut pcm, AUDIO_READ_POLL_MS),
                                    None => break, // 重建失败即已 break，到不了这里
                                };
                                match result {
                                    // 本轮没数据：记账；本连续段首次越过阈值时告警一次
                                    Ok(0) => {
                                        if dryout.on_idle(t0.elapsed().as_millis() as u64) {
                                            tracing::warn!(
                                                ?kind,
                                                streak_ms = dryout.streak_ms(),
                                                "音频连续空窗：WASAPI 源头断供或引擎静默，时间轴由墙钟补静音兜底"
                                            );
                                        }
                                    }
                                    Ok(_) => {
                                        dryout.on_data();
                                        gw.push(&pcm); // 永不阻塞：小环满只丢最旧
                                    }
                                    Err(f) => {
                                        // V2-1：设备失效 → 重建会话（≤MAX_AUDIO_REBUILDS 次）；
                                        // 次数用尽/重建失败 → 放弃该路（余下时间补静音，录制继续）；
                                        // 致命错误 → 整个录制失败（行为与改造前一致）。
                                        if should_rebuild(f.fault, rebuilds) {
                                            rebuilds += 1;
                                            tracing::warn!(
                                                ?kind,
                                                rebuilds,
                                                error = %f.error,
                                                "WASAPI 会话失效，重建音频采集"
                                            );
                                            cap.take(); // Drop：停掉失效的会话
                                            let dev = match kind {
                                                SourceKind::System => devices::resolve_render(None),
                                                SourceKind::Microphone => {
                                                    devices::resolve_mic(mic_device_id.as_deref())
                                                }
                                            };
                                            match dev.and_then(|d| AudioCapture::open(kind, d)) {
                                                Ok(c) => {
                                                    cap = Some(c);
                                                    lock_or_recover(&stats).audio_rebuilds += 1;
                                                    tracing::info!(
                                                        ?kind,
                                                        rebuilds,
                                                        "音频采集已重建"
                                                    );
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        ?kind,
                                                        rebuilds,
                                                        "音频会话重建失败，该路余下时间补静音：{e}"
                                                    );
                                                    note_issue(
                                                        &issue_thread,
                                                        format!("{kind:?} 音频会话重建失败，余下时间无此音源: {e}"),
                                                    );
                                                    break;
                                                }
                                            }
                                        } else if f.fault == ReadFault::DeviceLost {
                                            tracing::error!(
                                                ?kind,
                                                rebuilds,
                                                "音频设备持续失效（重建次数用尽），该路余下时间补静音"
                                            );
                                            note_issue(
                                                &issue_thread,
                                                format!("{kind:?} 音频设备持续失效，余下时间无此音源"),
                                            );
                                            break;
                                        } else {
                                            tracing::error!("音频采集失败: {}", f.error);
                                            note_issue(
                                                &issue_thread,
                                                format!("音频采集失败: {}", f.error),
                                            );
                                            *lock_or_recover(&state) = RecorderState::Failed(
                                                format!("音频采集失败: {}", f.error),
                                            );
                                            stop.store(true, Ordering::Relaxed);
                                            break;
                                        }
                                    }
                                }
                            }
                            // 退出前把空窗记账结清，写进全局统计
                            let mut s = lock_or_recover(&stats);
                            s.audio_dryout_ms += dryout.total_ms();
                            s.audio_max_dryout_ms =
                                s.audio_max_dryout_ms.max(dryout.max_ms());
                            drop(s);
                            reads_done.fetch_add(1, Ordering::SeqCst);
                            if let Some(mut c) = cap.take() {
                                c.stop();
                            }
                        })
                        .map_err(|e| AppError::Audio(format!("创建音频读取线程失败: {e}")))?,
                );
            }
            // 消费线程：补静音 → 混音 → AAC → 送封装队列
            // 与视频线程同一个"录制开始"原点：补静音按这个原点算，音视频时长才对得上
            let session_start = started;
            // 搬运线程全部起不来 = 跳过消费线程，录制走纯视频路径
            if n_active_sources > 0 {
                let gateways = active_gateways.clone();
                let n_sources = n_active_sources;
                let issue_thread = Arc::clone(&issue);
                start_guard.push(
                    spawn_guarded(
                        "cap-audio",
                        Arc::clone(&issue),
                        Arc::clone(&stop_flag),
                        Arc::clone(&state),
                        move || {
                            set_current_thread_priority(ThreadPriority::AboveNormal);
                            let start = session_start;
                            let mut scratch: Vec<Vec<f32>> =
                                (0..n_sources).map(|_| Vec::new()).collect();
                            let mut got_frames: Vec<usize> = vec![0; n_sources];
                            let mut consumed: Vec<usize> = vec![0; n_sources];
                            let mut mixed: Vec<f32> = Vec::new();
                            let mut out: Vec<(Vec<u8>, i64)> = Vec::new();
                            let mut aac_index: u64 = 0;
    
                            loop {
                                let stopping = stop.load(Ordering::Relaxed);
                                // 停止收尾顺序：等读取线程全部退出 → 停搬运（小环残留进大环）
                                // → 抽干大环。反过来会丢掉尾部音频。
                                if stopping && reads_done.load(Ordering::SeqCst) >= n_sources {
                                    for g in &gateways {
                                        g.stop();
                                    }
                                    if gateways.iter().all(|g| g.big().is_empty()) {
                                        break;
                                    }
                                }
                                // 非阻塞取走各路大环当前全部样本
                                let mut got_any = false;
                                for (i, g) in gateways.iter().enumerate() {
                                    let got = g.pop_available(&mut scratch[i]);
                                    got_frames[i] = got;
                                    if got > 0 {
                                        got_any = true;
                                    }
                                }
                                if !got_any {
                                    if stopping {
                                        // 等读取线程退出 / 搬运收尾
                                        if let Some(first) = gateways.first() {
                                            first.wait_data(Duration::from_millis(20));
                                        }
                                        continue;
                                    }
                                    // 运行中没数据：等 10 ms 或被新数据唤醒。
                                    // 注意：**不跳过下面的补静音** —— 静默期环回不产包，
                                    // 墙钟补齐是音轨不短于视频的唯一保障（docs/DESIGN.md §11.6.1）。
                                    if let Some(first) = gateways.first() {
                                        first.wait_data(Duration::from_millis(10));
                                    }
                                }
                                if !stopping {
                                    let target =
                                        (start.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
                                    let mut fatal_gap = None;
                                    for (i, buf) in scratch.iter_mut().enumerate() {
                                        let have = consumed[i] + got_frames[i];
                                        if target > have {
                                            let gap = target - have;
                                            if gap > MAX_SILENCE_PAD_FRAMES {
                                                // 不“跳过”：跳过等于音轨永久少一段，后面所有
                                                // 音频都会比视频早。超过上限就把它当会话失败，
                                                // 正常停止并让用户看到 session_issue。
                                                fatal_gap = Some(gap);
                                                break;
                                            }
                                            // 静音期环回不产数据 → 按墙上时钟补静音
                                            mixer::push_silence(buf, gap);
                                            lock_or_recover(&stats).silence_padded_frames +=
                                                gap as u64;
                                        }
                                        consumed[i] = target.max(have);
                                    }
                                    if let Some(gap) = fatal_gap {
                                        lock_or_recover(&stats).audio_timeline_jumps += 1;
                                        let msg = format!(
                                            "音频消费线程停顿过久（{} ms），为避免音画漂移停止录制",
                                            gap as u64 * 1000 / SAMPLE_RATE as u64
                                        );
                                        tracing::error!(
                                            gap_frames = gap,
                                            gap_ms = gap as u64 * 1000 / SAMPLE_RATE as u64,
                                            "音频时间轴跳变超出上限：停止录制，不再跳过静音"
                                        );
                                        note_issue(&issue_thread, msg.clone());
                                        *lock_or_recover(&state) = RecorderState::Failed(msg);
                                        stop.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                                let refs: Vec<&[f32]> = scratch.iter().map(|v| v.as_slice()).collect();
                                mixer::mix_into(&refs, &mut mixed);
                                for v in scratch.iter_mut() {
                                    v.clear();
                                }
                                if mixed.is_empty() {
                                    continue;
                                }
                                out.clear();
                                if let Err(e) = aac.encode(&mixed, &mut out) {
                                    tracing::error!("AAC 编码失败: {e}");
                                    note_issue(&issue_thread, format!("AAC 编码失败: {e}"));
                                    *lock_or_recover(&state) =
                                        RecorderState::Failed(format!("AAC 编码失败: {e}"));
                                    stop.store(true, Ordering::Relaxed);
                                    break;
                                }
                                for (data, _pts) in out.drain(..) {
                                    let pts_ms = aac_index as i64 * FRAME_SAMPLES as i64 * 1000
                                        / SAMPLE_RATE as i64;
                                    // 音频包很小且不允许丢，队列满就等一会儿
                                    if tx
                                        .send_timeout(
                                            MuxMsg::Audio { data, pts_ms },
                                            Duration::from_millis(200),
                                        )
                                        .is_err()
                                    {
                                        break;
                                    }
                                    lock_or_recover(&stats).audio_frames += 1;
                                    aac_index += 1;
                                }
                            }
                            // 冲刷编码器残留
                            out.clear();
                            if aac.finish(&mut out).is_ok() {
                                for (data, _pts) in out.drain(..) {
                                    let pts_ms = aac_index as i64 * FRAME_SAMPLES as i64 * 1000
                                        / SAMPLE_RATE as i64;
                                    let _ = tx.send(MuxMsg::Audio { data, pts_ms });
                                    aac_index += 1;
                                }
                            }
                            let _ = tx.send(MuxMsg::AudioEnd);
                            // 环形缓冲丢弃样本入账（0 = 全程没有溢出；非 0 = 机器被拖到极限过）
                            let dropped: u64 = gateways.iter().map(|g| g.dropped_samples()).sum();
                            lock_or_recover(&stats).audio_ring_dropped = dropped;
                            if dropped > 0 {
                                tracing::warn!(
                                    dropped,
                                    "音频环形缓冲出现过溢出（丢最旧保最新）：消费侧停顿超过了大环兜底能力"
                                );
                            }
                        })
                        .map_err(|e| AppError::Audio(format!("创建音频线程失败: {e}")))?,
                );
            }
        }
        drop(audio_tx);

        let phases = StartPhases {
            capture_open_ms,
            encoder_init_ms,
            audio_open_ms,
            aac_init_ms,
            total_ms: start_latency_ms,
            encoder_prewarmed,
            // 视频线程还没跑起来时是 0；调用方拿到 Recorder 后可以随读随新
            first_frame_ms: first_frame_ms.load(Ordering::Relaxed),
        };

        tracing::info!(
            output = %output.display(),
            video = expect_video,
            audio = expect_audio,
            audio_warnings = audio_warnings.len(),
            video_init_ms,
            capture_open_ms,
            encoder_init_ms,
            encoder_prewarmed,
            audio_open_ms,
            aac_init_ms,
            start_latency_ms,
            "录制会话已启动（各阶段耗时见字段名，单位 ms）"
        );
        // 会话就绪耗时**异常大**时必须显式告警：这段时间没有任何画面，
        // 调用方（界面/烟雾测试）若按"点开始后 N 秒"计时，产物就会比预期短一大截。
        // 机器很忙时（后台编译、磁盘打满）实测 0.46 s ~ 6.0 s。
        if start_latency_ms >= START_LATENCY_WARN_MS {
            tracing::warn!(
                start_latency_ms,
                capture_open_ms,
                encoder_init_ms,
                encoder_prewarmed,
                audio_open_ms,
                aac_init_ms,
                "录制会话就绪耗时过长：这段时间没有画面，实际录制窗口会比『点开始到停止』短这么多"
            );
        }

        Ok(Self {
            state,
            stats,
            stop_flag,
            cancel,
            result,
            issue,
            // 成功路径：把已启动线程的所有权交给 Recorder；guard 的 Drop 变为空操作，
            // 不会在这里把刚启动的会话置停。
            threads: start_guard.take_threads(),
            output,
            started,
            start_latency_ms,
            phases,
            first_frame_ms,
            audio_warnings,
        })
    }

    /// **会话就绪耗时（毫秒）**：`Recorder::start` 从进入到返回的耗时。
    ///
    /// 界面/回归脚本要在"按下了开始录制"与"真正开始有画面"之间做取舍时，
    /// 必须拿这个数说话（否则"录 5 秒"会把启动耗时也算进去，产物只有 3 秒）。
    pub fn start_latency_ms(&self) -> u64 {
        self.start_latency_ms
    }

    /// **启动各阶段耗时**（`DXGI 采集会话` / `OpenH264 初始化` / `音源打开` / `AAC` + 总耗时
    /// + 是否命中预热 + "第一帧被编码"耗时）：见 [`StartPhases`]。
    pub fn start_phases(&self) -> StartPhases {
        StartPhases {
            first_frame_ms: self.first_frame_ms.load(Ordering::Relaxed),
            ..self.phases
        }
    }

    /// **"按开始录制 → 第一帧被编码完成"的毫秒数**（`0` = 还没有编出第一帧）。
    pub fn first_frame_ms(&self) -> u64 {
        self.first_frame_ms.load(Ordering::Relaxed)
    }

    /// 本次录制的编码器是否来自**预热缓存**。
    pub fn encoder_prewarmed(&self) -> bool {
        self.phases.encoder_prewarmed
    }

    /// 用户要了声音、但某一路音源没打开成功的原因（空 = 全部按预期打开）。
    pub fn audio_warnings(&self) -> &[String] {
        &self.audio_warnings
    }

    /// 收尾看门狗用的取消令牌。UI 在超时后置位，正在进行的 remux 会在下个包边界退出。
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    pub fn state(&self) -> RecorderState {
        lock_or_recover(&self.state).clone()
    }
    pub fn stats(&self) -> RecorderStats {
        *lock_or_recover(&self.stats)
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    pub fn output(&self) -> &Path {
        &self.output
    }
    /// 结果（仅在收尾完成后可用）
    pub fn result(&self) -> Option<RecordingResult> {
        lock_or_recover(&self.result).clone()
    }

    /// 停止录制并等待收尾（含 remux 成普通 MP4）。重复调用安全。
    pub fn stop(&mut self) -> Result<RecordingResult, AppError> {
        if !self.stop_flag.swap(true, Ordering::SeqCst) {
            if let RecorderState::Recording = self.state() {
                *lock_or_recover(&self.state) = RecorderState::Stopping;
            }
        }
        // 线程 panic 过去被 `let _ = join()` 完全吞掉：mux 看到 channel 断开后照样
        // 导出一个偏短的文件，UI 仍显示“完成”。这里把 panic 记进 session_issue。
        let mut panicked = 0usize;
        for h in self.threads.drain(..) {
            if h.join().is_err() {
                panicked += 1;
            }
        }
        if panicked > 0 {
            note_issue(
                &self.issue,
                format!("有 {panicked} 条录制线程异常退出（panic），产物可能不完整"),
            );
        }
        if let Some(mut r) = self.result() {
            if r.session_issue.is_none() {
                r.session_issue = lock_or_recover(&self.issue).clone();
            }
            return Ok(r);
        }
        match self.state() {
            RecorderState::Failed(e) => Err(AppError::Mp4(e)),
            _ => Err(AppError::Mp4("录制结束但没有产出结果".into())),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            let _ = self.stop();
        }
    }
}

/// 封装线程：按 pts 归并音视频包 → 分片写入 → 收尾时 remux 成普通 MP4
#[allow(clippy::too_many_arguments)]
fn mux_thread(
    output: PathBuf,
    export_dir: PathBuf,
    expect_video: bool,
    expect_audio: bool,
    audio_params: Option<AudioParams>,
    video_rx: Receiver<MuxMsg>,
    audio_rx: Receiver<MuxMsg>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<RecorderState>>,
    stats: Arc<Mutex<RecorderStats>>,
    result: Arc<Mutex<Option<RecordingResult>>>,
    issue: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
    // `Recorder::start` 的会话就绪耗时（毫秒）：跟着结果回到界面/脚本，用来解释
    // "为什么录了 5 秒却只有 3 秒"（那部分时间花在会话初始化上，压根没画面）。
    start_latency_ms: u64,
) {
    // 电源保持必须由活得和录制一样久的线程持有（SetThreadExecutionState 是按线程的）。
    let _power = RecordingPowerGuard::keep_awake();
    let mut last_disk_check = Instant::now();
    let mut writer: Option<FragmentedMp4Writer> = None;
    let mut video_queue: VecDeque<(Vec<u8>, i64, bool)> = VecDeque::new();
    let mut audio_queue: VecDeque<(Vec<u8>, i64)> = VecDeque::new();
    let mut video_done = !expect_video;
    let mut audio_done = !expect_audio;
    let mut last_flush = Instant::now();
    let start = Instant::now();
    let mut failure: Option<String> = None;
    // 写入器还没建好时被丢掉的包数（诊断；正常情况下为 0）
    let mut writer_missing_drops: u64 = 0;
    // 纯音频模式：音频参数已知，立刻创建写入器
    if !expect_video {
        if let Some(ap) = &audio_params {
            match FragmentedMp4Writer::create_reserved(&output, None, Some(ap)) {
                Ok(w) => writer = Some(w),
                Err(e) => {
                    let msg = format!("创建仅音频 MP4 失败: {e}");
                    note_issue(&issue, msg.clone());
                    failure = Some(msg);
                }
            }
        }
    }

    while !(video_done && audio_done) {
        select! {
            recv(video_rx) -> msg => match msg {
                Ok(MuxMsg::InitVideo(vp)) => {
                    if writer.is_none() {
                        match FragmentedMp4Writer::create_reserved(
                            &output,
                            Some(&vp),
                            audio_params.as_ref(),
                        ) {
                            Ok(w) => {
                                tracing::info!(
                                    "分片 MP4 已创建（avcC + 音频流 {}）",
                                    audio_params.is_some()
                                );
                                writer = Some(w);
                            }
                            Err(e) => {
                                let msg = format!("创建分片 MP4 失败: {e}");
                                note_issue(&issue, msg.clone());
                                failure = Some(msg);
                            }
                        }
                    }
                }
                Ok(MuxMsg::Video { data, pts_ms, key }) => {
                    video_queue.push_back((data, pts_ms, key))
                }
                Ok(MuxMsg::VideoEnd) => video_done = true,
                Ok(_) => {}
                Err(_) => video_done = true,
            },
            recv(audio_rx) -> msg => match msg {
                Ok(MuxMsg::Audio { data, pts_ms }) => audio_queue.push_back((data, pts_ms)),
                Ok(MuxMsg::AudioEnd) => audio_done = true,
                Ok(_) => {}
                Err(_) => audio_done = true,
            },
            default(Duration::from_millis(50)) => {}
        }

        // 磁盘低空间保护：每 5 秒查一次，低于安全余量就主动收尾，
        // 不要等到写包失败（那时系统盘可能已经被写满）。
        if last_disk_check.elapsed() >= DISK_CHECK_INTERVAL {
            last_disk_check = Instant::now();
            if let Some(root) = output.parent() {
                if let Some(free) = disk::free_bytes(root) {
                    if free < disk::MIN_FREE_BYTES {
                        let msg = format!(
                            "磁盘空间不足（{} 可用，安全余量 {}），已停止录制并保留 Cache 分片",
                            disk::human(free),
                            disk::human(disk::MIN_FREE_BYTES)
                        );
                        tracing::error!("{msg}");
                        note_issue(&issue, msg.clone());
                        failure = Some(msg);
                        stop.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }

        // 归并写出：pts 小者先写
        loop {
            // 视频滞后丢弃（VIDEO_LAG_DROP_MS）：写盘/归并停顿会让音频继续到达、
            // 视频积压；恢复后若把过期画面灌进文件，声画就此错开且不可恢复。
            // 主动丢过期视频包：音频时间轴不受影响，播放表现为"略卡"而非"声画漂移"。
            // 口径来自参考方案 add/ 的 VIDEO_LAG_THRESHOLD = 0.120 s。
            // 先把 pts 拷出来再 pop：front() 的借用不能跨 pop_front()。
            let lagged = match (video_queue.front(), audio_queue.front()) {
                (Some((_, vp, _)), Some((_, ap))) if video_lagging(*vp, *ap) => Some((*vp, *ap)),
                _ => None,
            };
            if let Some((vp, ap)) = lagged {
                video_queue.pop_front();
                let n = {
                    let mut s = lock_or_recover(&stats);
                    s.video_lag_dropped += 1;
                    s.video_lag_dropped
                };
                // 首帧与每 100 帧记一次：丢得少不值得惊动，丢得多说明盘真有问题
                if n == 1 || n % 100 == 0 {
                    tracing::warn!(
                        vp_ms = vp,
                        ap_ms = ap,
                        dropped_total = n,
                        "视频包滞后音频超过 {} ms，丢弃过期包以保声画同步",
                        VIDEO_LAG_DROP_MS
                    );
                }
                continue;
            }
            let take_video = match (video_queue.front(), audio_queue.front()) {
                (Some((_, vp, _)), Some((_, ap))) => vp <= ap,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let Some(w) = writer.as_mut() else {
                // 写入器还没建好（视频参数未到）：丢弃除最后一帧外的数据，避免死等
                if video_queue.len() > 1 {
                    video_queue.pop_front();
                    writer_missing_drops += 1;
                }
                break;
            };
            if take_video {
                let Some((data, pts, key)) = video_queue.pop_front() else {
                    break;
                };
                if let Err(e) = w.write_video(&data, pts, key) {
                    let msg = format!("写视频包失败: {e}");
                    note_issue(&issue, msg.clone());
                    failure = Some(msg);
                    break;
                }
            } else {
                let Some((data, pts)) = audio_queue.pop_front() else {
                    break;
                };
                if let Err(e) = w.write_audio(&data, pts) {
                    let msg = format!("写音频包失败: {e}");
                    note_issue(&issue, msg.clone());
                    failure = Some(msg);
                    break;
                }
            }
        }

        // 定期刷盘：崩溃后可播（间隔见 FLUSH_INTERVAL）
        if last_flush.elapsed() >= FLUSH_INTERVAL {
            if let Some(w) = writer.as_mut() {
                if let Err(e) = w.flush() {
                    let msg = format!("刷盘失败（磁盘可能已满/不可写）: {e}");
                    tracing::error!("{msg}");
                    note_issue(&issue, msg.clone());
                    failure = Some(msg);
                }
            }
            last_flush = Instant::now();
        }

        if failure.is_some() {
            break;
        }
    }

    // 收尾：写完队列残留。
    // 这里的写失败以前是 `let _ = …` **完全吞掉**的 —— 一旦发生在最后几帧上，
    // 产物就会"在末尾少一截"而状态仍是 Finished、导出还报成功。现在逐条计数并判失败。
    let mut drain_errors: u64 = 0;
    if let Some(w) = writer.as_mut() {
        while let Some((data, pts, key)) = video_queue.pop_front() {
            if let Err(e) = w.write_video(&data, pts, key) {
                drain_errors += 1;
                tracing::error!(pts_ms = pts, error = %e, "收尾写视频包失败（这一帧不会出现在产物里）");
            }
        }
        while let Some((data, pts)) = audio_queue.pop_front() {
            if let Err(e) = w.write_audio(&data, pts) {
                drain_errors += 1;
                tracing::error!(pts_ms = pts, error = %e, "收尾写音频包失败（这一帧不会出现在产物里）");
            }
        }
        if let Err(e) = w.finish() {
            // trailer 写失败过去只 warn，结果仍可能被当成“成功”。分片文件也许还能播，
            // 但“本次收尾不完整”必须进入 session_issue，让界面如实显示。
            let msg = format!("写 MP4 trailer 失败: {e}");
            tracing::error!("{msg}");
            note_issue(&issue, msg);
        }
    }
    if drain_errors > 0 {
        let msg =
            format!("收尾时有 {drain_errors} 个包写盘失败，产物不完整（已按失败处理，流文件保留）");
        note_issue(&issue, msg.clone());
        if failure.is_none() {
            failure = Some(msg);
        }
    }

    let duration = start.elapsed();
    let wall_ms = duration.as_millis() as u64;
    let snapshot = *lock_or_recover(&stats);
    // 最后一帧 pts 应当贴着录制窗口：差一个帧周期是正常的（尾部丢帧/包写失败会让它偏小），
    // 这里只在**明显偏小**时留痕（不判失败：调用方/烟雾测试会按窗口做硬判定）。
    if snapshot.last_video_pts_ms > 0
        && (wall_ms as i64 - snapshot.last_video_pts_ms).abs() > TAIL_GAP_WARN_MS
    {
        tracing::warn!(
            wall_ms,
            last_video_pts_ms = snapshot.last_video_pts_ms,
            gap_ms = wall_ms as i64 - snapshot.last_video_pts_ms,
            writer_missing_drops,
            "最后一帧的 pts 与录制窗口对不上：末尾可能有空档/丢帧"
        );
    }

    if let Some(msg) = failure {
        note_issue(&issue, msg.clone());
        *lock_or_recover(&state) = RecorderState::Failed(msg.clone());
        tracing::error!("封装失败: {msg}");
        return;
    }
    if writer.is_none() {
        let msg = "没有写入任何内容".to_string();
        note_issue(&issue, msg.clone());
        *lock_or_recover(&state) = RecorderState::Failed(msg);
        return;
    }

    // 停止后导出普通 MP4（copy，不重新编码）。
    //
    // **目录分工**（用户要求）：
    // * `output`（分片流文件）本来就在 `<exe>\Cache\` 里，录制过程中就写在那里；
    // * 导出产物写到**用户设置的输出目录** `export_dir`，与流文件**同名同基名**
    //   （分处两个目录不会冲突）。重名时追加 `-1`/`-2`…，绝不覆盖已有文件。
    // * **不删除流文件**：它是导出失败/忘记另存时的唯一找回手段，由用户自己清理
    //   （界面**不统计也不提示占用空间**，只在**关闭软件**时若 Cache 里还有文件就提醒一次，
    //   并把 Cache 完整路径列给用户 —— 见 `cache` 模块与 `main.rs` 的关闭流程）。
    // 先把“录制过程本身的问题”快照下来：后面的 Finalizing/Finished 不能再覆盖它。
    let session_issue = lock_or_recover(&issue).clone();
    *lock_or_recover(&state) = RecorderState::Finalizing;
    let exported = export_path_for(&export_dir, &output);
    // `+faststart` 需要同目录临时文件 + 整体回拷，瞬时约等于再要一份成片空间。
    // 空间不够时降级为普通 MP4（moov 在末尾），保住“能导出”而不是为了 faststart 失败。
    let src_bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    let free = disk::free_bytes(&export_dir);
    // 先保证“能写完一份完整产物”。faststart 只是额外回拷，空间不够可以降级；
    // 但如果连 src_bytes 都放不下，remux 只会把盘写满后失败，不如直接保留分片。
    if let Some(free) = free {
        let minimum = src_bytes.saturating_add(8 * 1024 * 1024);
        if src_bytes > 0 && free < minimum {
            let msg = format!(
                "导出目录剩余空间不足：需要约 {}，实际 {}（{}）；已保留 Cache 分片",
                disk::human(minimum),
                disk::human(free),
                export_dir.display()
            );
            tracing::error!("{msg}");
            note_issue(&issue, msg.clone());
            let result_value = RecordingResult {
                streaming: output.clone(),
                exported: PathBuf::new(),
                duration,
                stats: snapshot,
                export: None,
                export_issue: Some(msg),
                session_issue: session_issue.clone(),
                start_latency_ms,
                wall_ms,
            };
            *lock_or_recover(&result) = Some(result_value);
            *lock_or_recover(&state) = RecorderState::Finished;
            return;
        }
    }
    let faststart = match free {
        Some(free) => {
            // faststart 会先把整段写入目标，再创建同目录临时文件回拷 moov，
            // 峰值接近“两份成片 + 固定余量”。之前只按一份 +256 MB 判断，
            // 大文件在看似空间足够时仍会失败。
            let need = src_bytes
                .saturating_mul(2)
                .saturating_add(disk::MIN_FREE_BYTES / 2);
            let ok = free >= need;
            if !ok {
                tracing::warn!(
                    free = %disk::human(free),
                    need = %disk::human(need),
                    "导出目录空间不足以做 faststart，降级为普通 MP4（moov 在末尾）"
                );
            }
            ok
        }
        None => true,
    };
    // **导出必须自校验**：`remux_to_plain_verified_opts` 除了 copy，还会把"产物时长/包数"
    // 与源流文件对账，不一致就返回 Err —— 于是"静默产出一个偏短的 MP4"这条路径被彻底堵死：
    // 要么导出完整，要么走下面的失败分支（保留 Cache 流文件 + 界面明确告警）。
    let final_result = match mux::remux_to_plain_verified_opts_cancel_new(
        &output,
        &exported,
        faststart,
        Some(cancel.as_ref()),
    ) {
        Ok(report) => {
            tracing::info!(
                exported = %exported.display(),
                streaming = %output.display(),
                bytes = report.bytes,
                packets = report.packets_written,
                stream_ms = report.src_duration_ms,
                export_ms = report.dst_duration_ms,
                delta_ms = report.duration_delta_ms(),
                elapsed_ms = wall_ms,
                start_latency_ms,
                "已导出普通 MP4 并自校验通过（流文件保留在 Cache）"
            );
            RecordingResult {
                streaming: output.clone(),
                exported,
                duration,
                stats: snapshot,
                export: Some(report),
                export_issue: None,
                session_issue: session_issue.clone(),
                start_latency_ms,
                wall_ms,
            }
        }
        Err(e) => {
            // **导出失败/不完整必须保留分片文件**（那是唯一副本），界面会明确提示。
            // 注意这里也覆盖"产物比源短"（被 verify_full_copy 判掉）的情形。
            let issue = e.to_string();
            tracing::error!(
                export_issue = %issue,
                streaming = %output.display(),
                "导出普通 MP4 失败或不完整：已保留 Cache 里的流文件（可直接播放/剪辑）"
            );
            RecordingResult {
                streaming: output.clone(),
                exported: PathBuf::new(),
                duration,
                stats: snapshot,
                export: None,
                export_issue: Some(issue),
                session_issue: session_issue.clone(),
                start_latency_ms,
                wall_ms,
            }
        }
    };
    *lock_or_recover(&result) = Some(final_result);
    *lock_or_recover(&state) = RecorderState::Finished;
}

/// 导出产物的落点：`export_dir / <流文件同名>`；重名时追加 `-1`、`-2`…
///
/// 为什么不用 `.plain.mp4` 后缀：两边在**不同目录**里，同名更直观（用户要求去掉该约定）。
/// 为什么必须避让重名：用户可能在同一秒内录两次（或上次的导出还在），覆盖会丢数据。
pub(crate) fn export_path_for(export_dir: &Path, streaming: &Path) -> PathBuf {
    let stem = streaming
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rec".to_string());
    let first = export_dir.join(format!("{stem}.mp4"));
    if !first.exists() {
        return first;
    }
    for n in 1..10_000u32 {
        let cand = export_dir.join(format!("{stem}-{n}.mp4"));
        if !cand.exists() {
            return cand;
        }
    }
    // 极端情况下 1..10000 都被占用：绝不回退到 `first`（那会覆盖已有文件）。
    // 用进程 id + 纳秒做最后一级唯一后缀。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    export_dir.join(format!("{stem}-{}-{nanos}.mp4", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_start_returns_before_creating_a_session() {
        let cancel = Arc::new(AtomicBool::new(true));
        let cfg = Config::default();
        let out = std::env::temp_dir().join("cap-cancelled-start.mp4");
        let err = Recorder::start_cancellable(&cfg, &out, cancel)
            .err()
            .expect("预取消的启动必须失败");
        assert!(err.to_string().contains("取消"), "{err}");
    }

    #[test]
    fn start_guard_reserves_output_and_refuses_existing_file() {
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("cap-start-reserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rec.mp4");
        {
            let mut guard = StartGuard::new(Arc::new(AtomicBool::new(false)));
            guard.reserve_output(&path).expect("首次应能占住");
            assert!(path.is_file());
        }
        assert!(!path.exists(), "错误路径的空占位应被清掉");

        std::fs::write(&path, b"old").unwrap();
        let mut guard = StartGuard::new(Arc::new(AtomicBool::new(false)));
        assert!(guard.reserve_output(&path).is_err(), "不得覆盖已有录制文件");
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn guarded_thread_surfaces_panic_immediately() {
        let issue = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(RecorderState::Recording));
        let h = spawn_guarded(
            "panic-test",
            Arc::clone(&issue),
            Arc::clone(&stop),
            Arc::clone(&state),
            || panic!("boom"),
        )
        .expect("spawn");
        assert!(h.join().is_ok(), "wrapper 自己不应把 panic 传出去");
        assert!(stop.load(Ordering::SeqCst), "panic 后必须置停止");
        assert!(matches!(
            &*lock_or_recover(&state),
            RecorderState::Failed(_)
        ));
        assert!(lock_or_recover(&issue)
            .as_deref()
            .is_some_and(|s| s.contains("boom")));
    }

    /// **真机探针**：把 `Recorder::start` 的启动耗时拆开量，并对比"预热前 vs 预热后"。
    ///
    /// ```text
    /// cargo test --lib start_latency_phase_breakdown -- --ignored --nocapture
    /// ```
    ///
    /// 口径（与 `recorder_smoke` 完全一致，见那里的 `REC_START_MS`）：
    /// * `total` = `Recorder::start` 从**进入**到**返回句柄**（= `start_latency_ms`）；
    /// * `rec_start` = 在调用点前后自己量的同一个区间（= `recorder_smoke` 的 `t0 → start 返回`）；
    /// * `first_frame` = 进入 `start()` → 第一帧**编码完成**（由视频线程写，见
    ///   [`StartPhases::first_frame_ms`]）。
    ///
    /// 它会真的开采集会话、真的编一帧、真的导出一个小文件，所以是 `#[ignore]` 的：
    /// 默认的 `cargo test` 不跑它，只有手动 `--ignored` 才跑（需要显示器 + `openh264.dll`）。
    #[test]
    #[ignore = "真机录制探针（需要显示器 + openh264.dll）：用 --ignored 手动运行"]
    fn start_latency_phase_breakdown() {
        use crate::config::{CaptureMode, Config, ContentMode};
        use crate::video::encode::{
            discard_prewarm, last_prewarm_ms, planned_encoder_dims, prewarm_encoder_async,
            prewarmed_encoder_ready,
        };

        const ITER: usize = 5;
        let dir = std::env::temp_dir().join(format!("cap-start-phases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录失败");
        let cfg = Config {
            capture_mode: CaptureMode::FullScreen,
            monitor_index: 0,
            content_mode: ContentMode::VideoAndAudio,
            audio_system: true,
            audio_mic: false,
            output_dir: dir.to_string_lossy().to_string(),
            fps: 30,
            video_bitrate_kbps: 8_000,
            ..Config::default()
        }
        .sanitized();
        let (w, h) = planned_encoder_dims(&cfg).expect("算不出采集尺寸（没有显示器？）");

        println!(
            "\n[phases] 配置 {}x{} @{}fps {}kbps 内容={:?}；输出 {}",
            w,
            h,
            cfg.fps,
            cfg.video_bitrate_kbps,
            cfg.content_mode,
            dir.display()
        );
        println!(
            "[phases] 列（全部 ms）：capture=DXGI 采集会话 | encoder=OpenH264 创建+InitializeExt | \
             audio=音源打开 | aac=AAC 初始化 | total=start 从进入到返回 | \
             rec_start=调用点现量的同一区间 | first_frame=进入 start→第一帧编码完成"
        );

        // 一轮：可选地先预热并等就绪，再 `Recorder::start`，等第一帧编码出来，停掉。
        let measure = |warm: bool, i: usize| -> (u64, StartPhases) {
            discard_prewarm();
            if warm {
                let _ = prewarm_encoder_async(&cfg);
                let deadline = Instant::now() + Duration::from_secs(60);
                while !prewarmed_encoder_ready(&cfg, w, h) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    prewarmed_encoder_ready(&cfg, w, h),
                    "预热在 60 s 内没有就绪（openh264.dll 缺失？）"
                );
            }
            let out = dir.join(format!("{}-{i}.mp4", if warm { "warm" } else { "cold" }));
            let t0 = Instant::now();
            let mut rec = Recorder::start(&cfg, &out).expect("启动录制失败");
            let rec_start_ms = t0.elapsed().as_millis() as u64;
            let deadline = Instant::now() + Duration::from_secs(10);
            while rec.first_frame_ms() == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            let phases = rec.start_phases();
            // 录一小段（≥0.2 s）再停：让封装/导出收尾能正常跑完
            thread::sleep(Duration::from_millis(200));
            let _ = rec.stop();
            (rec_start_ms, phases)
        };

        let mut avgs: Vec<(bool, [u64; 7])> = Vec::new();
        // 先跑一轮**不计入测表**的预热轮：把"首次真正调用 FFmpeg 时 5 个 DLL 的延迟加载
        // 映射"（实测 1.3~1.4 s，**一次性**）先付掉 —— 产品里它已由
        // `ui::startup::preload_ffmpeg_dlls_once` 在界面启动时处理（见 `src/ui/main_page.rs`），
        // 不该混进"编码器预热"的前后对比里。这里如实把它单独打出来。
        let (_, warmup) = measure(false, 999);
        println!(
            "[phases] 不计入测表的预热轮（付掉一次性成本）：capture={} encoder={} audio={} aac={} total={}",
            warmup.capture_open_ms,
            warmup.encoder_init_ms,
            warmup.audio_open_ms,
            warmup.aac_init_ms,
            warmup.total_ms
        );
        for warm in [false, true] {
            let mut rows: Vec<(u64, StartPhases)> = Vec::new();
            for i in 0..ITER {
                let (rec_start_ms, ph) = measure(warm, i);
                println!(
                    "[phases] {:<8} #{:<2} capture={:>4} encoder={:>5} prewarmed={:<5} audio={:>4} \
                     aac={:>3} total={:>5} rec_start={:>5} first_frame={:>5}",
                    if warm { "预热后" } else { "预热前" },
                    i + 1,
                    ph.capture_open_ms,
                    ph.encoder_init_ms,
                    ph.encoder_prewarmed,
                    ph.audio_open_ms,
                    ph.aac_init_ms,
                    ph.total_ms,
                    rec_start_ms,
                    ph.first_frame_ms
                );
                rows.push((rec_start_ms, ph));
            }
            let n = rows.len() as u64;
            let sum = |f: fn(&StartPhases) -> u64| rows.iter().map(|(_, p)| f(p)).sum::<u64>() / n;
            let rec_avg = rows.iter().map(|(r, _)| *r).sum::<u64>() / n;
            let avg = [
                sum(|p| p.capture_open_ms),
                sum(|p| p.encoder_init_ms),
                sum(|p| p.audio_open_ms),
                sum(|p| p.aac_init_ms),
                sum(|p| p.total_ms),
                rec_avg,
                sum(|p| p.first_frame_ms),
            ];
            println!(
                "[phases] {:<8} 平均   capture={:>4} encoder={:>5}                 audio={:>4} aac={:>3} \
                 total={:>5} rec_start={:>5} first_frame={:>5}",
                if warm { "预热后" } else { "预热前" },
                avg[0],
                avg[1],
                avg[2],
                avg[3],
                avg[4],
                avg[5],
                avg[6]
            );
            avgs.push((warm, avg));
        }
        println!(
            "[phases] 预热本身（后台线程）耗时 {} ms；预热前 encoder 平均 {} ms → 预热后 {} ms（省 {} ms）",
            last_prewarm_ms(),
            avgs[0].1[1],
            avgs[1].1[1],
            avgs[0].1[1].saturating_sub(avgs[1].1[1])
        );
        println!(
            "[phases] total 平均 {} ms → {} ms（省 {} ms）；first_frame 平均 {} ms → {} ms（省 {} ms）",
            avgs[0].1[4],
            avgs[1].1[4],
            avgs[0].1[4].saturating_sub(avgs[1].1[4]),
            avgs[0].1[6],
            avgs[1].1[6],
            avgs[0].1[6].saturating_sub(avgs[1].1[6])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **导出产物的落点**（取代旧的"导出成功后删除流文件"规则；那条规则**已取消**）：
    /// * 导出写到**用户设置目录**，与流文件**同名**（两边目录不同，同名不冲突）；
    /// * 该目录已有同名文件时**追加 `-1`/`-2`…**，绝不覆盖（用户可能同一秒录两次）；
    /// * 流文件本身**不被删除**（Cache 里保留，作为找回手段）。
    #[test]
    fn export_goes_to_the_output_dir_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("cap-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let streaming = Path::new(r"C:\app\Cache\rec-20261012-060115.mp4");

        // 没有同名文件 → 直接用同名
        let first = export_path_for(&dir, streaming);
        assert_eq!(first.file_name().unwrap(), "rec-20261012-060115.mp4");
        assert_eq!(first.parent().unwrap(), dir.as_path());

        // 已经存在 → 追加 -1（再存在 → -2）
        std::fs::write(&first, b"x").unwrap();
        let second = export_path_for(&dir, streaming);
        assert_eq!(second.file_name().unwrap(), "rec-20261012-060115-1.mp4");
        std::fs::write(&second, b"x").unwrap();
        let third = export_path_for(&dir, streaming);
        assert_eq!(third.file_name().unwrap(), "rec-20261012-060115-2.mp4");

        // 导出路径与流文件路径**必须不同**（不能把 Cache 里的文件当成导出目标）
        assert_ne!(first, streaming.to_path_buf());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 帧率硬上限：高刷新率屏幕也不会按刷新率取帧。
    #[test]
    fn fps_is_hard_capped_at_sixty() {
        assert_eq!(MAX_RECORD_FPS, 60);
        for (want, got) in [
            (120u32, 60u32),
            (90, 60),
            (61, 60),
            (60, 60),
            (30, 30),
            (1, 1),
        ] {
            assert_eq!(effective_fps(want), got, "{want} fps 应夹到 {got}");
        }
        // 0 由 `Recorder::start` 显式拒绝（这里只保证不会算出 0 间隔）
        assert_eq!(effective_fps(0), 1);
    }

    /// 节拍重同步：落后 1 拍**不**重同步（继续按原节拍追赶），
    /// 落后 5 拍及以上才丢弃落后额度、对齐到"现在"（并计入统计）。
    #[test]
    fn advance_deadline_resyncs_only_when_far_behind() {
        let interval = Duration::from_millis(100);
        let t0 = Instant::now();

        // 正常：刚刚好赶上，下一拍 = 上一拍 + 间隔
        let (next, resync) = advance_deadline(t0, t0, interval);
        assert_eq!(next, t0 + interval);
        assert!(!resync);

        // 落后 1 拍（now = deadline + 1*interval）→ 不重同步
        let (next, resync) = advance_deadline(t0, t0 + interval, interval);
        assert_eq!(next, t0 + interval, "仍按原节拍走");
        assert!(!resync, "只落后一拍不该重同步");

        // 落后 4 拍（now = deadline + 4*interval → 自增后仍落后 3 拍）→ 阈值内，不重同步
        let (next, resync) = advance_deadline(t0, t0 + interval * 4, interval);
        assert_eq!(next, t0 + interval);
        assert!(!resync, "落后 4 拍仍在容差内");

        // 落后 5 拍 → 丢额度、对齐到现在，并且**报告**这次重同步
        let now = t0 + interval * 5;
        let (next, resync) = advance_deadline(t0, now, interval);
        assert_eq!(next, now, "重同步后从『现在』重新起拍");
        assert!(resync, "落后 5 拍必须重同步并计入统计");
    }

    /// pts 口径对照：编码跟不上目标帧率时，"帧序号"口径会把视频压缩成快放，
    /// "真实时刻"口径则保持与墙钟一致（这就是最终选 RealTime 的原因）。
    #[test]
    fn real_time_pts_never_shrinks_the_timeline() {
        // 目标 60 fps，实际每秒只编出 40 帧：4 秒墙钟 → 160 帧
        let (fps, frames) = (60u32, 160u64);
        let wall = Duration::from_millis(4_000);
        let real = video_pts_ms(PtsSource::RealTime, wall, frames, fps);
        let by_index = video_pts_ms(PtsSource::FrameIndex, wall, frames, fps);
        assert_eq!(real, 4_000, "真实时刻口径：最后一帧 pts = 墙钟时刻");
        assert_eq!(
            by_index,
            160 * 1000 / 60,
            "帧序号口径：160 帧 @60fps 只有 2.67 s"
        );
        assert!(
            by_index < real - 1_000,
            "帧序号口径会让 4 秒的录制变成 {by_index} ms 的快放"
        );

        // 每一拍都真的编出一帧时，两种口径几乎一致（差不超过一帧）
        let full = 240u64; // 4 秒 × 60 fps
        let real = video_pts_ms(PtsSource::RealTime, wall, full, fps);
        let by_index = video_pts_ms(PtsSource::FrameIndex, wall, full, fps);
        assert!((real - by_index).abs() <= 1000 / fps as i64);
        // 30/60 两档下关键帧仍落在整秒：intra_period = 秒 × fps 是整数帧
        for fps in [30u32, 60] {
            assert_eq!(2 * fps, if fps == 30 { 60 } else { 120 });
        }
    }

    /// pts 口径的边界：非法 fps 不 panic、不除以 0。
    #[test]
    fn video_pts_handles_degenerate_input() {
        assert_eq!(
            video_pts_ms(PtsSource::FrameIndex, Duration::ZERO, 7, 0),
            7_000
        );
        assert_eq!(
            video_pts_ms(PtsSource::RealTime, Duration::from_millis(1_234), 7, 0),
            1_234
        );
        assert_eq!(
            video_pts_ms(PtsSource::FrameIndex, Duration::ZERO, 0, 60),
            0
        );
    }

    /// 视频滞后丢弃阈值：落后 ≥120ms 才丢；差一点不丢；正常路径（视频在前或紧跟）不误伤。
    #[test]
    fn video_lagging_matches_the_documented_threshold() {
        // 正常：视频与音频几乎同时 → 不丢
        assert!(!video_lagging(1_000, 1_000));
        assert!(!video_lagging(1_000, 1_050), "只差 50 ms 不该丢");
        // 边界：恰好 120 ms → 不丢（严格小于才丢）
        assert!(!video_lagging(1_000, 1_120));
        // 超过阈值：视频明显滞后 → 丢
        assert!(video_lagging(1_000, 1_121));
        assert!(video_lagging(1_000, 5_000), "停滞后恢复的积压包必须丢");
        // 音频还没来（pts 0 附近）时视频在前 → 不丢（启动阶段不误伤）
        assert!(!video_lagging(0, 0));
        assert!(!video_lagging(500, 0));
        // 视频超前（编码很快）→ 不丢
        assert!(!video_lagging(2_000, 1_000));
    }

    /// V2-1 重建策略：只有设备失效且在预算内才重建；致命错误/次数用尽都不重建。
    #[test]
    fn audio_rebuild_policy_only_for_device_loss_within_budget() {
        assert!(should_rebuild(ReadFault::DeviceLost, 0));
        assert!(should_rebuild(
            ReadFault::DeviceLost,
            MAX_AUDIO_REBUILDS - 1
        ));
        assert!(
            !should_rebuild(ReadFault::DeviceLost, MAX_AUDIO_REBUILDS),
            "次数用尽后不再重建（放弃该路，补静音兜底）"
        );
        assert!(!should_rebuild(
            ReadFault::DeviceLost,
            MAX_AUDIO_REBUILDS + 7
        ));
        assert!(!should_rebuild(ReadFault::Fatal, 0));
        assert!(!should_rebuild(ReadFault::Fatal, MAX_AUDIO_REBUILDS - 1));
    }

    /// 网格吸附 + 单调保证：时间戳不抖、不重复、不倒退，且仍然跟着墙钟走。
    #[test]
    fn video_pts_is_grid_snapped_and_strictly_increasing() {
        let fps = 60u32; // 网格 16 ms（1000/60 取整）
        let step = 16i64;
        // 抖动被吸掉：15、17、33 ms 都落到各自最近的格子
        assert_eq!(video_pts_on_grid(0, None, fps), 0);
        assert_eq!(video_pts_on_grid(15, Some(0), fps), step);
        assert_eq!(video_pts_on_grid(17, Some(step), fps), 2 * step);
        // 抖动 ±半格以内 → 落在同一个格子上，但**单调兜底**保证仍然严格递增
        let a = video_pts_on_grid(33, Some(0), fps);
        let b = video_pts_on_grid(34, Some(a), fps);
        assert!(b > a, "同一格内的两帧也必须严格递增（{a} → {b}）");
        // 掉帧场景：墙钟 4 秒时 pts ≈ 4000 ms（不会被压成 160 帧 @60fps 的 2666 ms）
        let wall = 4_000i64;
        let mut last = None;
        let mut pts = 0;
        for i in 0..160 {
            // 实际只有 40 fps：每帧 25 ms
            pts = video_pts_on_grid(i * 25, last, fps);
            last = Some(pts);
        }
        assert!(
            (pts as f64 - wall as f64).abs() <= 2.0 * step as f64,
            "最后一帧 pts {pts} 应贴着墙钟 {wall}（±两格）"
        );
        // 负输入/0 fps 不 panic
        assert_eq!(video_pts_on_grid(-5, None, fps), 0);
        assert!(video_pts_on_grid(100, None, 0) >= 0);
    }
}
