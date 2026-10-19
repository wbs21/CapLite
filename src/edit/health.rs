//! **编码体检（P35）**：把"编码错误"从"静默降级"变成"看得见、可跳过"。
//!
//! # 为什么需要这一层
//!
//! 改造前，四条链路遇到坏数据时**都是静默降级**，而且降级方式一模一样 ——
//! **把"读错"当成"读完"**：
//!
//! | 链路 | 改造前的写法 | 后果 |
//! |---|---|---|
//! | 播放（`player.rs`） | `av_read_frame < 0` → 当片尾 | 播到坏点就停，像"卡住" |
//! | 波形（`waveform.rs`） | `ret < 0` → `EndReason::Eof` | 坏段之后画成静音，看不出是坏的 |
//! | 关键帧扫描（`mod.rs`） | `ret < 0` → `break`，保留半截表 | **后半段吸附失效 → 导出花屏** |
//! | 缩略图（`mod.rs`） | `ret < 0` → `break` | 坏点之后的缩略图永远取不到 |
//!
//! 于是用户看到的现象是"播着播着停住 / 后半段剪不动 / 导出的片子开头花屏"，
//! 而**没有任何一处告诉他"这个文件在第 12 分 30 秒处坏了"**。
//!
//! 本模块负责把这件事查出来并说清楚；"跳过"与"剔除"分别由
//! `player`（播放）与 `mod::export_ranges_inner`（导出）消费本模块的结论。
//!
//! # 两档检查
//!
//! * **`scan_pass`（快速体检，默认自动）**：**全片解封装遍历**，不解码。
//!   它**不是额外开销** —— 关键帧扫描本来就要逐包读全片，本模块只是在那次遍历里
//!   顺手多记几个判据（读包错误 / 数据比容器短 / 时间戳跳变 / 零关键帧）。
//!   成本与 P34 的关键帧扫描**完全同一档**（毫秒级~秒级）。
//! * **[`deep_scan`](crate::edit::health::deep_scan)（深度检查，手动触发）**：**全片解码**，读
//!   `AVFrame::decode_error_flags` 与 `avcodec_send_packet` 的返回值，
//!   能抓"包结构完好但内容损坏"的花屏。成本 = 一次全片软解
//!   （1080p 约为片长的 1/3 ~ 1/1，后台 + 可取消 + 带进度）。
//!
//! # "硬错误"与"软告警"
//!
//! [`BadKind::is_hard`](crate::edit::health::BadKind::is_hard) 是**行为分界线**：
//! * **硬**（`BadKind::ReadError` / `BadKind::ShortData` / `BadKind::DecodeError`）——
//!   这段数据确实不能用：播放会**自动跳过**、导出会**自动剔除**；
//! * **软**（`BadKind::TimestampJump` / `BadKind::NoKeyframe`）——
//!   只是"可能有麻烦"：只标红提示，**绝不改变播放与导出行为**。
//!   理由：时间戳跳变常常只是元数据问题、画面完全正常；若也自动跳过，
//!   用户会遇到"莫名其妙跳了一段"，比花屏更难接受（用户 2026-10-18 定的口径：
//!   **仅硬错误自动跳**）。

use super::{
    av_err, cpath, limit_decoder_pixels, limit_input_probe, ts_to_ms, Codec, Frame, Input, Packet,
    AV_NOPTS_VALUE, AVERROR_EOF,
};
use crate::error::AppError;
use ffmpeg_next::ffi as ff;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

/// `FFERRTAG('E','A','G','A')` 的**近义值**：`AVERROR(EAGAIN)` = `-EAGAIN` = -11（宏，bindgen 不导出）
const AVERROR_EAGAIN: i32 = -11;

/// 时间戳**向前跳跃**超过它就算一段异常（毫秒）。
///
/// 2 s 的取法：正常 30 fps 相邻包间隔 33 ms，即使丢了一整个 GOP（2 s）也只是"正常丢帧"；
/// 超过 2 s 的真空期在录制产物里基本只有一种来源 —— 时间戳被写坏了。
pub(crate) const JUMP_FWD_MS: i64 = 2_000;
/// 时间戳**倒退**超过它就算一段异常（毫秒）。倒退比跳跃更不可能"正常"，
/// 但仍留 200 ms 余量：B 帧重排会让 DTS 与 PTS 交替领先，偶尔的几十毫秒回退是正常的。
pub(crate) const JUMP_BACK_MS: i64 = 200;
/// "数据比容器声称的短"的容忍量（毫秒）：最后一帧的显示时长 + 容器取整。
const SHORT_TOL_MS: i64 = 500;
/// 相邻同类坏段的间隔小于它就合并（避免一个坏区域报出上百条）
const MERGE_GAP_MS: i64 = 1_000;
/// 深度检查：连续多少帧带 `decode_error_flags` 才认定为"一段坏"
/// （H.264 解码器会对**单帧**做错误隐藏，偶发一帧不算问题）
const DEEP_MIN_BAD_FRAMES: u32 = 3;
/// 深度检查：每读多少个包报一次进度 / 查一次取消
const DEEP_PROGRESS_EVERY: u64 = 256;

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

/// 坏段的**种类**。`key()` 给界面做 i18n 键，不在这里拼任何面向用户的文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BadKind {
    /// **读到坏数据**：`av_read_frame` 返回非 `AVERROR_EOF` 的负值 ——
    /// 文件从这里开始读不下去了（典型：录制被强行中断、磁盘写满、文件被截断）。
    ReadError,
    /// **数据比容器声称的短**：容器说有这么长，实际包只到某个时刻就没了。
    ///
    /// 与 [`BadKind::ReadError`] 的区别：这条是"读得很干净，但提前读完了"，
    /// 分片 mp4 被中断时最常见的形态。
    ShortData,
    /// **时间戳跳变**（倒退或跳跃）。**软告警**：画面往往完全正常，只是元数据坏了。
    TimestampJump,
    /// **有视频流却一个关键帧都没有**。**软告警**：无法精确剪辑，
    /// 但不拦着用户导出（拦下来等于"这个文件完全不能用"，比花屏更糟）。
    NoKeyframe,
    /// **解码器报错 / 坏帧**：只有 [`deep_scan`] 能发现。
    DecodeError,
}

impl BadKind {
    /// **硬错误** = 这段数据确实不能用 → 播放跳过、导出剔除。
    ///
    /// 这条判据是**行为分界线**，改它等于改用户能看到的播放/导出结果，
    /// 动之前先读模块文档里"硬错误与软告警"那一节。
    pub fn is_hard(self) -> bool {
        matches!(
            self,
            BadKind::ReadError | BadKind::ShortData | BadKind::DecodeError
        )
    }

    /// i18n 键（界面按 `bad_<key>` 取文案）
    pub fn key(self) -> &'static str {
        match self {
            BadKind::ReadError => "read_error",
            BadKind::ShortData => "short_data",
            BadKind::TimestampJump => "timestamp_jump",
            BadKind::NoKeyframe => "no_keyframe",
            BadKind::DecodeError => "decode_error",
        }
    }

    /// 全部种类（界面遍历用，顺序 = 严重度降序）
    pub const ALL: [BadKind; 5] = [
        BadKind::ReadError,
        BadKind::ShortData,
        BadKind::DecodeError,
        BadKind::TimestampJump,
        BadKind::NoKeyframe,
    ];
}

/// **一段编码错误的区间**（毫秒，闭开区间 `[start_ms, end_ms)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRange {
    pub start_ms: i64,
    pub end_ms: i64,
    pub kind: BadKind,
    /// 给人看的补充说明（例如"读到 Invalid data found when processing input"）。
    /// **不参与任何判定**，只用来在侧栏里把话说清楚。
    pub detail: String,
}

impl BadRange {
    pub fn new(start_ms: i64, end_ms: i64, kind: BadKind, detail: impl Into<String>) -> Self {
        Self {
            start_ms: start_ms.max(0),
            end_ms: end_ms.max(start_ms.max(0)),
            kind,
            detail: detail.into(),
        }
    }

    /// 区间长度（毫秒）
    pub fn len_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }

    /// 与 `[s, e)` 是否有交集
    pub fn overlaps(&self, s: i64, e: i64) -> bool {
        self.start_ms < e && self.end_ms > s
    }
}

/// **体检报告**：`Clip::health` 就装它（快速体检的结论随打开一起回来）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthReport {
    /// 坏段列表（按 `start_ms` 升序、同类相邻已合并）
    pub bad: Vec<BadRange>,
    /// 文件在此时刻读不下去了（`None` = 读到了正常片尾）。
    ///
    /// 与 `bad` 里那条 [`BadKind::ReadError`] 是同一件事的两种表达：
    /// 列表用于绘制/剔除，这个标量用于"一句话说清楚"（侧栏标题）。
    pub truncated_at_ms: Option<i64>,
    /// 是否做过完整解码（`false` = 只做了快速体检）
    pub full_decode: bool,
    /// 检查覆盖到的时长（毫秒）
    pub scanned_ms: i64,
    /// 扫过的包数（诊断用）
    pub packets: u64,
}

impl HealthReport {
    /// 一条坏段都没有（软硬都不算）
    pub fn is_clean(&self) -> bool {
        self.bad.is_empty() && self.truncated_at_ms.is_none()
    }

    /// 有没有**硬**错误（决定"要不要跳/要不要剔"）
    pub fn has_hard(&self) -> bool {
        self.bad.iter().any(|b| b.kind.is_hard())
    }

    /// 硬坏段列表（`(start, end)`，升序）—— 播放跳过、导出剔除都用它
    pub fn hard_ranges(&self) -> Vec<(i64, i64)> {
        let mut v: Vec<(i64, i64)> = self
            .bad
            .iter()
            .filter(|b| b.kind.is_hard())
            .map(|b| (b.start_ms, b.end_ms))
            .collect();
        v.sort_unstable();
        v
    }

    /// **按时间排序 + 合并同类相邻段**。
    ///
    /// 不合并的话，一个坏区域会按包报出成百上千条（`av_read_frame` 每读一次都可能报一次），
    /// 侧栏会被淹没、绘制会退化成一大片重复矩形。
    pub fn merge(&mut self) {
        if self.bad.is_empty() {
            return;
        }
        self.bad.sort_by_key(|b| (b.start_ms, b.end_ms));
        let mut out: Vec<BadRange> = Vec::with_capacity(self.bad.len());
        for b in self.bad.drain(..) {
            if let Some(last) = out.last_mut() {
                if last.kind == b.kind && b.start_ms <= last.end_ms.saturating_add(MERGE_GAP_MS) {
                    last.end_ms = last.end_ms.max(b.end_ms);
                    if last.detail.is_empty() {
                        last.detail = b.detail;
                    }
                    continue;
                }
            }
            out.push(b);
        }
        self.bad = out;
    }

    /// 把 `[s, e)` 里的**硬坏段**切掉，返回剩下的若干子区间（都已裁到 `[s, e)` 内、升序）。
    ///
    /// * 一个硬坏段都没有时**原样返回** `vec![(s, e)]`（导出行为与改造前一字不差）；
    /// * 整段都被切掉时返回空 `Vec`（调用方据此报"没有可导出的内容"）。
    ///
    /// 注意：返回的子区间起点**不保证是关键帧** —— 那是调用方的事
    /// （`mod::resolve_segments` 会再吸附一次，见那里的注释）。
    pub fn excise(&self, s: i64, e: i64) -> Vec<(i64, i64)> {
        let (s, e) = (s.max(0), e.max(0));
        if e <= s {
            return Vec::new();
        }
        let mut cuts: Vec<(i64, i64)> = self
            .bad
            .iter()
            .filter(|b| b.kind.is_hard())
            .map(|b| (b.start_ms, b.end_ms))
            .collect();
        if cuts.is_empty() {
            return vec![(s, e)];
        }
        cuts.sort_unstable();
        let mut out: Vec<(i64, i64)> = Vec::new();
        let mut cur = s;
        for (bs, be) in cuts {
            if be <= cur {
                continue;
            }
            if bs >= e {
                break;
            }
            if bs > cur {
                out.push((cur, bs.min(e)));
            }
            cur = cur.max(be);
            if cur >= e {
                break;
            }
        }
        if cur < e {
            out.push((cur, e));
        }
        out
    }

    /// `[s, e)` 里被硬坏段占掉的时长（毫秒）—— 导出报告里"已跳过 N 秒"就是它
    pub fn hard_ms_in(&self, s: i64, e: i64) -> i64 {
        self.bad
            .iter()
            .filter(|b| b.kind.is_hard())
            .map(|b| (b.end_ms.min(e) - b.start_ms.max(s)).max(0))
            .sum()
    }
}

/// **一次全片解封装遍历的产物**：关键帧表 + 体检报告。
///
/// 两者之所以一起返回：它们**共用同一次遍历**（见模块文档"快速体检"）。
/// 拆成两次读全片是纯浪费 —— 1 GB 文件就是多读 1 GB。
#[derive(Debug, Clone, Default)]
pub struct ScanPass {
    pub keyframes_ms: Vec<i64>,
    pub health: HealthReport,
}

// ---------------------------------------------------------------------------
// 快速体检（解封装级，与关键帧扫描共用一次遍历）
// ---------------------------------------------------------------------------

/// **扫一遍全片**：收集关键帧 + 体检判据（**不解码**）。
///
/// `vindex` 传 -1（没有视频流）时只做"没什么可扫"的空返回 —— 纯音频素材不该报编码错误。
///
/// 判据（全部零额外 IO）：
/// 1. `av_read_frame` 返回负且 **不是** `AVERROR_EOF` → [`BadKind::ReadError`]
///    （从此刻到片尾都读不出来了，所以区间取 `[当前位置, 视频时长]`）；
/// 2. 视频包的 PTS 相对上一包**倒退 > [`JUMP_BACK_MS`]** 或**前进 > [`JUMP_FWD_MS`]**
///    → [`BadKind::TimestampJump`]；
/// 3. 有视频流但**一个关键帧都没有** → [`BadKind::NoKeyframe`]（整片）；
/// 4. 实际读到的最大 PTS 比**视频流声称的时长**短 > [`SHORT_TOL_MS`]
///    → [`BadKind::ShortData`]（`[最大 PTS, 视频时长]`）。
///
/// 第 4 条是"分片 mp4 被中断"的主判据：这种文件的 `av_read_frame` 往往**读得很干净**
/// （返回 EOF），只是数据本身就短了 —— 只看返回值是查不出来的。
///
/// # Safety
/// `ictx` 必须是已打开并读过流信息的输入上下文；本函数会在它上面 `av_seek_frame` 回绕到文件头。
///
/// 可见性 `pub(super)`：参数 [`Input`] 是 `edit` 的私有守卫，
/// 暴露成 `pub` 会触发 "private type in public interface"。
pub(super) fn scan_pass(
    ictx: &Input,
    vindex: i32,
    tb: ff::AVRational,
    video_dur_ms: i64,
) -> ScanPass {
    let mut out = ScanPass::default();
    if vindex < 0 {
        return out;
    }
    let video_dur_ms = video_dur_ms.max(0);
    let mut max_pts_ms: i64 = 0;
    let mut last_pts_ms: Option<i64> = None;
    // SAFETY: 指针全部来自本模块的 RAII 守卫；`ictx` 由调用方保证有效且独占。
    unsafe {
        let pkt = Packet(ff::av_packet_alloc());
        if pkt.0.is_null() {
            tracing::warn!("编码体检：av_packet_alloc 失败（本次体检跳过）");
            return out;
        }
        // 回绕到文件头：`find_stream_info` 之后读位置停在半路，不先 seek 就会漏掉开头。
        // seek 失败不致命（最多漏判开头那一小段），但要留一行日志。
        let seek = ff::av_seek_frame(ictx.0, -1, 0, ff::AVSEEK_FLAG_BACKWARD);
        if seek < 0 {
            tracing::warn!(error = %av_err(seek), "编码体检：回绕文件头失败，从当前位置开始扫");
        }
        loop {
            let ret = ff::av_read_frame(ictx.0, pkt.0);
            if ret < 0 {
                if ret != AVERROR_EOF {
                    // **硬错误**：不是正常片尾，是读到坏数据了
                    let at = last_pts_ms.unwrap_or(0).max(0);
                    out.health.truncated_at_ms = Some(at);
                    out.health.bad.push(BadRange::new(
                        at,
                        video_dur_ms.max(at),
                        BadKind::ReadError,
                        av_err(ret),
                    ));
                }
                break;
            }
            out.health.packets += 1;
            if (*pkt.0).stream_index == vindex {
                let ts = if (*pkt.0).pts != AV_NOPTS_VALUE {
                    (*pkt.0).pts
                } else {
                    (*pkt.0).dts
                };
                if ts != AV_NOPTS_VALUE {
                    let ms = ts_to_ms(ts, tb);
                    if ms > max_pts_ms {
                        max_pts_ms = ms;
                    }
                    if let Some(prev) = last_pts_ms {
                        if ms + JUMP_BACK_MS < prev || ms > prev + JUMP_FWD_MS {
                            out.health.bad.push(BadRange::new(
                                prev.min(ms),
                                prev.max(ms),
                                BadKind::TimestampJump,
                                format!("{prev} ms → {ms} ms"),
                            ));
                        }
                    }
                    last_pts_ms = Some(ms);
                    if ((*pkt.0).flags & ff::AV_PKT_FLAG_KEY) != 0 {
                        out.keyframes_ms.push(ms);
                    }
                }
            }
            ff::av_packet_unref(pkt.0);
        }
    }
    out.keyframes_ms.sort_unstable();
    out.keyframes_ms.dedup();

    // ---- 判据 3：有视频流却没有关键帧 ----
    if out.keyframes_ms.is_empty() && video_dur_ms > 0 {
        out.health.bad.push(BadRange::new(
            0,
            video_dur_ms,
            BadKind::NoKeyframe,
            "整片没有一个关键帧，无法精确剪辑".to_string(),
        ));
    }
    // ---- 判据 4：数据比容器短 ----
    if video_dur_ms > 0 && max_pts_ms + SHORT_TOL_MS < video_dur_ms {
        out.health.bad.push(BadRange::new(
            max_pts_ms.max(0),
            video_dur_ms,
            BadKind::ShortData,
            format!(
                "视频流声称 {video_dur_ms} ms，实际只读到 {max_pts_ms} ms（短 {} ms）",
                video_dur_ms - max_pts_ms
            ),
        ));
    }
    out.health.scanned_ms = video_dur_ms.max(max_pts_ms).max(0);
    out.health.merge();
    out
}

// ---------------------------------------------------------------------------
// 深度检查（全片解码）
// ---------------------------------------------------------------------------

/// **全片解码检查**（后台线程里跑；`cancel` 一置最多再解一个包就退出）。
///
/// 判据：
/// 1. `avcodec_send_packet` 返回既不是 `EAGAIN` 也不是 `EOF` 的负值 → 该包时间点记一次坏；
/// 2. `AVFrame::decode_error_flags != 0` → 该帧是"解码器承认的坏帧"（花屏的来源）；
/// 3. 连续 `DEEP_MIN_BAD_FRAMES` 帧坏才算一段（单帧错误隐藏不算）；
/// 4. 读包错误 / 数据比容器短，与 `scan_pass` 同口径（深度检查是它的超集）。
///
/// `progress` 每读 `DEEP_PROGRESS_EVERY` 个包被调用一次，参数是"已检查到的毫秒"，
/// 供界面画进度条（**不保证单调严格递增**：坏包会让 PTS 回退，界面自己取 max）。
///
/// 取消时返回 `Err`（调用方据此丢弃结果，不当成"检查通过"）。
pub fn deep_scan(
    path: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(i64),
) -> Result<HealthReport, AppError> {
    let c_path = cpath(path)?;
    let mut out = HealthReport {
        full_decode: true,
        ..HealthReport::default()
    };
    // 解到的最大 PTS：**在 unsafe 块外面声明** —— 块外收尾时还要用它算 `scanned_ms`
    let mut max_pts_ms: i64 = 0;
    // SAFETY: 本节创建的每个 FFmpeg 对象都由对应的 RAII 守卫持有，任何提前返回都会释放。
    unsafe {
        let mut raw: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_open_input(&mut raw, c_path.as_ptr(), ptr::null(), ptr::null_mut());
        if ret < 0 || raw.is_null() {
            return Err(AppError::Mp4(format!(
                "打开 {} 失败：{}",
                path.display(),
                av_err(ret)
            )));
        }
        let ictx = Input(raw);
        limit_input_probe(ictx.0);
        if ff::avformat_find_stream_info(ictx.0, ptr::null_mut()) < 0 {
            return Err(AppError::Mp4("读取流信息失败".into()));
        }

        // 第一条视频流
        let mut vindex = -1i32;
        for i in 0..(*ictx.0).nb_streams as usize {
            let st = *(*ictx.0).streams.add(i);
            if (*(*st).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO {
                vindex = (*st).index;
                break;
            }
        }
        if vindex < 0 {
            return Err(AppError::Mp4("文件里没有视频流，无法做编码检查".into()));
        }
        let vstream = *(*ictx.0).streams.add(vindex as usize);
        let vtb = (*vstream).time_base;
        let video_dur_ms = ts_to_ms((*vstream).duration, vtb).max(0);

        let codec = ff::avcodec_find_decoder((*(*vstream).codecpar).codec_id);
        if codec.is_null() {
            return Err(AppError::Mp4("FFmpeg 找不到对应的视频解码器".into()));
        }
        let dec = Codec(ff::avcodec_alloc_context3(codec));
        if dec.0.is_null() {
            return Err(AppError::Mp4("avcodec_alloc_context3 失败".into()));
        }
        if ff::avcodec_parameters_to_context(dec.0, (*vstream).codecpar) < 0 {
            return Err(AppError::Mp4("复制解码参数失败".into()));
        }
        (*dec.0).pkt_timebase = vtb;
        limit_decoder_pixels(dec.0);
        if ff::avcodec_open2(dec.0, codec, ptr::null_mut()) < 0 {
            return Err(AppError::Mp4("打开视频解码器失败".into()));
        }

        let pkt = Packet(ff::av_packet_alloc());
        if pkt.0.is_null() {
            return Err(AppError::Mp4("av_packet_alloc 失败".into()));
        }
        let frame = Frame(ff::av_frame_alloc());
        if frame.0.is_null() {
            return Err(AppError::Mp4("av_frame_alloc 失败".into()));
        }

        let mut last_pts_ms: Option<i64> = None;
        // 正在累积的"连续坏帧"段：起点 + 已累计帧数
        let mut bad_from: Option<i64> = None;
        let mut bad_frames: u32 = 0;
        let mut draining = false;

        'outer: loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(AppError::Mp4("编码检查已取消".into()));
            }
            if !draining {
                let ret = ff::av_read_frame(ictx.0, pkt.0);
                if ret < 0 {
                    if ret != AVERROR_EOF {
                        let at = last_pts_ms.unwrap_or(0).max(0);
                        out.truncated_at_ms = Some(at);
                        out.bad.push(BadRange::new(
                            at,
                            video_dur_ms.max(at),
                            BadKind::ReadError,
                            av_err(ret),
                        ));
                    }
                    // 送空包把解码器里缓存的帧冲出来，再收一遍
                    ff::avcodec_send_packet(dec.0, ptr::null());
                    draining = true;
                } else {
                    out.packets += 1;
                    if out.packets.is_multiple_of(DEEP_PROGRESS_EVERY) {
                        progress(max_pts_ms);
                    }
                    if (*pkt.0).stream_index == vindex {
                        let ts = if (*pkt.0).pts != AV_NOPTS_VALUE {
                            (*pkt.0).pts
                        } else {
                            (*pkt.0).dts
                        };
                        if ts != AV_NOPTS_VALUE {
                            let ms = ts_to_ms(ts, vtb);
                            if ms > max_pts_ms {
                                max_pts_ms = ms;
                            }
                            if let Some(prev) = last_pts_ms {
                                if ms + JUMP_BACK_MS < prev || ms > prev + JUMP_FWD_MS {
                                    out.bad.push(BadRange::new(
                                        prev.min(ms),
                                        prev.max(ms),
                                        BadKind::TimestampJump,
                                        format!("{prev} ms → {ms} ms"),
                                    ));
                                }
                            }
                            last_pts_ms = Some(ms);
                        }
                        let send = ff::avcodec_send_packet(dec.0, pkt.0);
                        if send < 0 && send != AVERROR_EOF && send != AVERROR_EAGAIN {
                            // 解码器明确拒收这个包：这个时间点就是坏的
                            let at = last_pts_ms.unwrap_or(max_pts_ms).max(0);
                            let period = 1i64;
                            out.bad.push(BadRange::new(
                                at,
                                at + period,
                                BadKind::DecodeError,
                                av_err(send),
                            ));
                        }
                    }
                    ff::av_packet_unref(pkt.0);
                }
            }

            // ---- 收帧：只看 decode_error_flags，不做任何像素转换（省掉 sws 是这条链能快的原因）----
            let mut got_any = false;
            loop {
                let recv = ff::avcodec_receive_frame(dec.0, frame.0);
                if recv < 0 {
                    break;
                }
                got_any = true;
                let pts = if (*frame.0).best_effort_timestamp != AV_NOPTS_VALUE {
                    (*frame.0).best_effort_timestamp
                } else {
                    (*frame.0).pts
                };
                let ms = if pts != AV_NOPTS_VALUE {
                    ts_to_ms(pts, vtb).max(0)
                } else {
                    last_pts_ms.unwrap_or(0).max(0)
                };
                if (*frame.0).decode_error_flags != 0 {
                    bad_frames += 1;
                    if bad_from.is_none() {
                        bad_from = Some(ms);
                    }
                } else if let Some(from) = bad_from.take() {
                    // 坏帧段结束
                    if bad_frames >= DEEP_MIN_BAD_FRAMES {
                        out.bad.push(BadRange::new(
                            from,
                            ms.max(from + 1),
                            BadKind::DecodeError,
                            format!("{bad_frames} 帧解码出错"),
                        ));
                    }
                    bad_frames = 0;
                }
                ff::av_frame_unref(frame.0);
            }
            if draining && !got_any {
                break 'outer;
            }
        }
        // 片尾还在坏帧段里 → 收尾
        if let Some(from) = bad_from {
            if bad_frames >= DEEP_MIN_BAD_FRAMES {
                out.bad.push(BadRange::new(
                    from,
                    max_pts_ms.max(from + 1),
                    BadKind::DecodeError,
                    format!("{bad_frames} 帧解码出错"),
                ));
            }
        }
        // 数据比容器短（与快速体检同口径）
        if video_dur_ms > 0 && max_pts_ms + SHORT_TOL_MS < video_dur_ms {
            out.bad.push(BadRange::new(
                max_pts_ms.max(0),
                video_dur_ms,
                BadKind::ShortData,
                format!(
                    "视频流声称 {video_dur_ms} ms，实际只解到 {max_pts_ms} ms（短 {} ms）",
                    video_dur_ms - max_pts_ms
                ),
            ));
        }
        progress(max_pts_ms);
    }
    out.scanned_ms = out.scanned_ms.max(max_pts_ms).max(0);
    out.merge();
    Ok(out)
}

// ---------------------------------------------------------------------------
// 单测：纯逻辑（无 IO）+ 真文件（随包 ffmpeg 现场生成）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn br(s: i64, e: i64, k: BadKind) -> BadRange {
        BadRange::new(s, e, k, String::new())
    }

    /// 空报告：`excise` 必须**原样返回**，导出行为与改造前完全一致
    #[test]
    fn empty_report_excises_nothing() {
        let h = HealthReport::default();
        assert!(h.is_clean());
        assert!(!h.has_hard());
        assert_eq!(h.excise(1000, 5000), vec![(1000, 5000)]);
        assert_eq!(h.hard_ms_in(0, 10_000), 0);
    }

    /// 软告警**不参与剔除**：时间戳跳变只标红，不改导出
    #[test]
    fn soft_ranges_are_never_excised() {
        let mut h = HealthReport::default();
        h.bad.push(br(2000, 3000, BadKind::TimestampJump));
        h.bad.push(br(0, 9000, BadKind::NoKeyframe));
        assert!(!h.is_clean(), "软告警也要出现在报告里（界面要标红）");
        assert!(!h.has_hard(), "软段不算硬错误");
        assert_eq!(h.excise(0, 10_000), vec![(0, 10_000)]);
        assert_eq!(h.hard_ms_in(0, 10_000), 0);
    }

    /// 中间一段硬错误 → 切成两段
    #[test]
    fn hard_range_splits_a_selection() {
        let mut h = HealthReport::default();
        h.bad.push(br(3000, 4000, BadKind::ReadError));
        assert_eq!(h.excise(0, 10_000), vec![(0, 3000), (4000, 10_000)]);
        assert_eq!(h.hard_ms_in(0, 10_000), 1000);
    }

    /// 坏段盖住选区两端 / 整段 / 完全在外
    #[test]
    fn excision_edge_cases() {
        let mut h = HealthReport::default();
        h.bad.push(br(0, 2000, BadKind::ReadError));
        assert_eq!(h.excise(1000, 5000), vec![(2000, 5000)], "盖住起点");
        h.bad.clear();
        h.bad.push(br(4000, 9000, BadKind::ShortData));
        assert_eq!(h.excise(1000, 5000), vec![(1000, 4000)], "盖住终点");
        h.bad.clear();
        h.bad.push(br(0, 99_999, BadKind::ReadError));
        assert!(h.excise(1000, 5000).is_empty(), "整段都在坏区里");
        h.bad.clear();
        h.bad.push(br(6000, 7000, BadKind::ReadError));
        assert_eq!(h.excise(1000, 5000), vec![(1000, 5000)], "坏段在选区外");
    }

    /// 多个硬坏段按时间切，不重叠、不丢长度
    #[test]
    fn multiple_hard_ranges_cut_in_order() {
        let mut h = HealthReport::default();
        h.bad.push(br(8000, 9000, BadKind::DecodeError));
        h.bad.push(br(2000, 3000, BadKind::ReadError));
        let got = h.excise(0, 10_000);
        assert_eq!(got, vec![(0, 2000), (3000, 8000), (9000, 10_000)]);
        let kept: i64 = got.iter().map(|(a, b)| b - a).sum();
        assert_eq!(kept, 10_000 - 2000);
    }

    /// 合并：同类相邻（间隔 ≤ MERGE_GAP_MS）合成一条；不同类不合并
    #[test]
    fn merge_joins_same_kind_and_keeps_kinds_apart() {
        let mut h = HealthReport::default();
        h.bad.push(br(5000, 6000, BadKind::ReadError));
        h.bad.push(br(4000, 4500, BadKind::ReadError));
        h.bad.push(br(6100, 6500, BadKind::TimestampJump));
        h.merge();
        assert_eq!(h.bad.len(), 2, "两条 ReadError 合成一条，跳变单独留着");
        assert_eq!(h.bad[0], br(4000, 6000, BadKind::ReadError));
        assert_eq!(h.bad[1], br(6100, 6500, BadKind::TimestampJump));
    }

    /// 合并后仍按时间升序（`excise` 依赖这个前提）
    #[test]
    fn merge_sorts_by_start() {
        let mut h = HealthReport::default();
        h.bad.push(br(9000, 9500, BadKind::DecodeError));
        h.bad.push(br(1000, 1500, BadKind::DecodeError));
        h.merge();
        assert!(h.bad[0].start_ms < h.bad[1].start_ms);
    }

    /// 硬/软的归类就是行为分界线，钉死它
    #[test]
    fn hard_kinds_are_the_behavior_boundary() {
        for k in [BadKind::ReadError, BadKind::ShortData, BadKind::DecodeError] {
            assert!(k.is_hard(), "{k:?} 必须是硬错误（播放跳过 / 导出剔除）");
        }
        for k in [BadKind::TimestampJump, BadKind::NoKeyframe] {
            assert!(!k.is_hard(), "{k:?} 必须只是软告警（只标红，不改行为）");
        }
        // i18n 键不许重复（重复会让两种错因显示同一句文案）
        let mut keys: Vec<&str> = BadKind::ALL.iter().map(|k| k.key()).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "BadKind::key() 有重复");
    }

    /// `overlaps` / `len_ms` 的基本语义
    #[test]
    fn bad_range_geometry() {
        let b = br(1000, 3000, BadKind::ReadError);
        assert_eq!(b.len_ms(), 2000);
        assert!(b.overlaps(0, 1001));
        assert!(!b.overlaps(0, 1000), "左端点相接不算相交");
        assert!(b.overlaps(2999, 5000));
        assert!(!b.overlaps(3000, 5000), "右端点相接不算相交");
    }
}
