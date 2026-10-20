//! 剪辑核心（**无界面、可直接单测**）：读流信息、扫描关键帧、关键帧吸附、copy 导出、缩略图。
//!
//! 设计要点：
//! * **不依赖 UI**：剪辑语义全部在这里，时间轴/吸附/导出可以脱离界面被单测与回归脚本直接驱动。
//! * **导出是 copy**：只搬 packet、不解码、不重编码，所以分辨率与编码参数与源完全一致
//!   （[`ExportReport::same_resolution`] 会读回产物再断言一次）。
//! * **关键帧吸附带半帧容差**：关键帧的 dts 是帧周期整数倍，但 pts 常带 composition
//!   offset，例如 90 kHz 时基下"3 秒关键帧"实存 269999（= 2999.9889 ms），换算成
//!   毫秒会少零点几毫秒。若用严格 `>=` / `<=` 比较，起点就会跳过一整个 GOP，
//!   所以比较时允许半个帧周期的误差。

use crate::error::AppError;
use crate::mux;
use crate::mux::av_err;
use ffmpeg_next::ffi as ff;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::AtomicBool;

/// **剪辑页音频波形**（峰值金字塔 + AAC 解码 + 渐进发布）。
///
/// 单独一个子模块的理由：它的公开面（[`waveform::BasePlan`] / [`waveform::Peak`] /
/// [`waveform::segments_for_view`]）与"关键帧吸附 / copy 导出"毫无耦合，
/// 而且**全部逻辑都能脱界面单测**（含真解码，素材由随包 ffmpeg 现场生成）。
pub mod waveform;

/// **剪辑页播放**（常驻解码线程 + 有界帧队列）。
///
/// 单独一个子模块的理由：它要**长时间持有** `AVFormatContext` / `AVCodecContext`
/// （打开一次、顺序解码），与"每次调用都重新打开文件的缩略图链"是两种完全不同的
/// 资源模型；混在一起会让"到底谁该负责关文件"变得不可判定。
pub mod player;

/// **剪辑页播放的音频输出**（AAC 解码 → 重采样 → WASAPI 渲染）。
///
/// 单独一个子模块的理由：`player` 那条链从设计上只搬视频（"最小可用播放器"），
/// 音频输出是**另一套资源模型**（解码 + 渲染设备 + 自己的实时时钟），
/// 且失败语义完全不同——它**只降级为静音、绝不影响播放本身**。
pub mod audio_out;

/// **编码体检**（坏段检测：快速体检随打开一起跑，深度检查手动触发）。
///
/// 单独一个子模块的理由：它是**唯一**一个"只读文件、不产生任何编辑结果"的链路，
/// 而且它的结论被三处消费（界面标红、播放跳过、导出剔除）——
/// 混进 `mod` 会让"谁在改 clip / 谁在改导出区间"变得难以追溯。
pub mod health;

/// `FFERRTAG('E','O','F',' ')`：FFmpeg 用它表示读流结束（宏，bindgen 不导出）
const AVERROR_EOF: i32 = -541_478_725;
/// `AV_NOPTS_VALUE`（宏，bindgen 不导出）
const AV_NOPTS_VALUE: i64 = i64::MIN;
/// 微秒基准（`AV_TIME_BASE` 的展开值：`av_seek_frame` 传流索引 -1 时用它做单位）
const AV_TIME_BASE: i64 = 1_000_000;
/// 帧率未知时的兜底帧率（规格要求：未知则按 1/60 秒估帧周期）
const FALLBACK_FPS: f64 = 60.0;

/// **不可信媒体解码边界**：单帧最大像素数（约 33 MP，覆盖 8K 以下）。
///
/// 剪辑页解码的是用户随便选的文件。没有这个上限时，一个构造出来的超大 SPS 就能让
/// 解码器分配巨量帧缓冲（DoS）；FFmpeg 的 `max_pixels` 会在打开解码器时直接拒绝。
pub(crate) const MAX_DECODE_PIXELS: i64 = 40_000_000;

/// 给 `AVCodecContext` 设像素上限（在 `avcodec_open2` 之前调用）。
///
/// # Safety
/// `dec` 必须是 `avcodec_alloc_context3` 返回且尚未 free 的上下文。
pub(crate) unsafe fn limit_decoder_pixels(dec: *mut ff::AVCodecContext) {
    if !dec.is_null() {
        (*dec).max_pixels = MAX_DECODE_PIXELS;
    }
}

/// 给输入上下文设探测上限（在 `avformat_find_stream_info` 之前调用）。
///
/// 8 MB probesize / 2 s analyzeduration 足够识别正常 MP4，又能挡住“故意让 demuxer
/// 读很久/分配很大”的输入。必须在 `find_stream_info` 之前设置才生效。
///
/// # Safety
/// `ictx` 必须是 `avformat_open_input` 成功返回的上下文。
pub(crate) unsafe fn limit_input_probe(ictx: *mut ff::AVFormatContext) {
    crate::mux::limit_input_probe(ictx);
}

// ---------------------------------------------------------------------------
// 播放实时节流（P54：单一来源）
// ---------------------------------------------------------------------------
//
// 这两个数同时被**两条**解码线程使用：`player`（视频）与 `audio_out`（音频）。
// P54 之前各写一份（`player::LEAD_MS` / `audio_out::LEAD_MS`），
// 而 `audio_out` 的注释里还得特意写一句"与 `player` 的 `LEAD_MS` 同值"——
// 换句话说：**重复已经被发现过一次，但只是用注释记了下来，没有消除**。
//
// 它们必须同值，理由是音画同步：两条线程各自"最多领先墙钟多少"就是各自的缓冲深度，
// 一个 200 ms、一个 300 ms 的话，seek / stop 后两边排空的时间就不一样长，
// 表现为"画面停了声音还在响"。漂移同样是**编译器看不见**的（都是合法整数字面量）。
//
/// 解码线程最多领先墙钟多少毫秒（**音视两条线程同一拍**）。
pub const DECODE_LEAD_MS: i64 = 200;
/// 节流时单次睡眠的上限（毫秒）：睡太久会让 seek / stop 的响应变迟钝。
pub const DECODE_NAP_MAX_MS: u64 = 20;

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

// P49：`av_err` 已移到 `crate::mux`（那里是全仓唯一的 FFmpeg 错误码翻译），
// 见文件顶部的 `use crate::mux::av_err;`。原先本模块与 `waveform` 各有一份逐字相同的拷贝。

fn cpath(p: &Path) -> Result<CString, AppError> {
    CString::new(p.to_string_lossy().as_bytes())
        .map_err(|_| AppError::Mp4(format!("路径含 NUL 字节: {}", p.display())))
}

/// 有理数帧率 → f64（den 为 0 视为未知）
fn rational_f64(r: ff::AVRational) -> f64 {
    if r.den == 0 {
        0.0
    } else {
        r.num as f64 / r.den as f64
    }
}

/// 时间戳（时基 `tb`）→ 毫秒，**向最近毫秒取整**。
///
/// 为什么取整而不是截断：关键帧 dts 是帧周期整数倍、pts 常带 composition offset，
/// 例如 90 kHz 下"3 秒关键帧"的 pts 是 269999 → 截断得 2999 ms。取整得 3000 ms，
/// 既贴合用户感知，也让下游按毫秒回退 seek 时能落回同一个关键帧。
fn ts_to_ms(ts: i64, tb: ff::AVRational) -> i64 {
    if ts == AV_NOPTS_VALUE {
        return 0;
    }
    if tb.num <= 0 || tb.den <= 0 {
        return ts;
    }
    let ms = ts as f64 * tb.num as f64 * 1000.0 / tb.den as f64;
    if !ms.is_finite() {
        return 0;
    }
    ms.round() as i64
}

/// 把解码帧里报告的像素格式（`i32`）还原成 FFmpeg 的 `AVPixelFormat` 枚举。
///
/// 为什么绕一圈：`AVFrame::format` 是 `int`，而 `sws_getContext` 要枚举。
/// Rust 不允许 `int as 枚举`，`transmute` 又依赖枚举布局（bindgen 没给这几个枚举加
/// `#[repr]`），伪造判别值可能是 UB。这里用 FFmpeg 自己的格式描述符表做查找：
/// 找到的判别值一定合法，找不到就报错。
fn pixel_format(raw: i32) -> Result<ff::AVPixelFormat, AppError> {
    // SAFETY: av_pix_fmt_desc_next 从 NULL 开始遍历 FFmpeg 内部静态描述符表，
    // 返回的指针在下一次调用前有效；av_pix_fmt_desc_get_id 只读该描述符。
    unsafe {
        let mut desc = ff::av_pix_fmt_desc_next(ptr::null());
        while !desc.is_null() {
            let id = ff::av_pix_fmt_desc_get_id(desc);
            if id as i32 == raw {
                return Ok(id);
            }
            desc = ff::av_pix_fmt_desc_next(desc);
        }
    }
    Err(AppError::Mp4(format!("FFmpeg 不认识的像素格式 {raw}")))
}

fn codec_name(par: *const ff::AVCodecParameters) -> String {
    if par.is_null() {
        return "?".into();
    }
    // SAFETY: par 来自已打开的输入流；avcodec_get_name 返回静态字符串或 NULL。
    unsafe {
        let c = ff::avcodec_get_name((*par).codec_id);
        if c.is_null() {
            "?".into()
        } else {
            CStr::from_ptr(c).to_string_lossy().into_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// RAII 守卫：FFmpeg 资源必须成对释放，用守卫保证任何提前返回都不泄漏
// ---------------------------------------------------------------------------

/// 输入上下文守卫（`avformat_open_input` ↔ `avformat_close_input`）
struct Input(*mut ff::AVFormatContext);

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: 指针要么为空，要么由 avformat_open_input 成功返回；关闭后置空，不会二次释放。
        unsafe {
            if !self.0.is_null() {
                ff::avformat_close_input(&mut self.0);
                self.0 = ptr::null_mut();
            }
        }
    }
}

/// packet 守卫（`av_packet_alloc` ↔ `av_packet_free`，后者内部会 unref）
struct Packet(*mut ff::AVPacket);

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: 指针由 av_packet_alloc 分配；av_packet_free 之后置空。
        unsafe {
            if !self.0.is_null() {
                ff::av_packet_free(&mut self.0);
            }
        }
    }
}

/// 解码帧守卫（`av_frame_alloc` ↔ `av_frame_free`）
struct Frame(*mut ff::AVFrame);

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: 指针由 av_frame_alloc 分配；av_frame_free 之后置空。
        unsafe {
            if !self.0.is_null() {
                ff::av_frame_free(&mut self.0);
            }
        }
    }
}

/// 解码器上下文守卫（`avcodec_alloc_context3` ↔ `avcodec_free_context`）
struct Codec(*mut ff::AVCodecContext);

impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: 指针由 avcodec_alloc_context3 分配；avcodec_free_context 之后置空。
        unsafe {
            if !self.0.is_null() {
                ff::avcodec_free_context(&mut self.0);
            }
        }
    }
}

/// swscale 上下文守卫（`sws_getContext` ↔ `sws_freeContext`）
struct Sws(*mut ff::SwsContext);

/// 预览解码的 swscale 上下文缓存：同一个播放线程连续解码同分辨率/同像素格式时复用，
/// 避免每帧 `sws_getContext` 的初始化开销。键变化时旧上下文在替换时析构。
type SwsKey = (ff::AVPixelFormat, u32, u32, u32, u32);
thread_local! {
    static SWS_CACHE: RefCell<Option<(SwsKey, Sws)>> = const { RefCell::new(None) };
}

impl Drop for Sws {
    fn drop(&mut self) {
        // SAFETY: 指针由 sws_getContext 分配且未被别处释放。
        unsafe {
            if !self.0.is_null() {
                ff::sws_freeContext(self.0);
                self.0 = ptr::null_mut();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

/// 单条流的简要参数
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub index: i32,
    pub codec: String,
    /// 视频：宽（音频为 0）
    pub width: i32,
    /// 视频：高（音频为 0）
    pub height: i32,
    /// 视频：帧率（音频为 0.0；0.0 表示未知）
    pub fps: f64,
    /// 时间基 `(num, den)`
    pub time_base: (i32, i32),
    /// 音频：采样率（视频为 0）
    pub sample_rate: i32,
    /// 音频：声道数（视频为 0）
    pub channels: i32,
    pub duration_ms: i64,
}

/// 打开后的剪辑素材
#[derive(Debug, Clone)]
pub struct Clip {
    pub src: PathBuf,
    pub duration_ms: i64,
    pub video: Option<StreamInfo>,
    pub audio: Option<StreamInfo>,
    /// 视频关键帧时间轴（升序、去重、毫秒）
    pub keyframes_ms: Vec<i64>,
    /// **关键帧表是否已经是最终态**（P34 流式打开的第二段信号）。
    ///
    /// `false` = 表还在后台扫（[`open_streaming`] 已把 clip 交出去、扫描还没回），
    /// 此时表**可能不完整**：界面照常出首帧/胶片条（缩略图解码不依赖表），
    /// 但**导出必须等它变 `true`** —— 不完整的表会让 copy 导出落在非关键帧上。
    ///
    /// `true` = 表已定稿（扫完了 / 没有视频流可扫 / 同步 [`open`] 一次做完）。
    pub keyframes_scanned: bool,
    /// **编码体检报告**（P35）：坏段区间、是否截断、是否做过完整解码。
    ///
    /// 由同一次全片遍历产出（`health::scan_pass`），所以**不额外读一遍文件**。
    /// 三处消费它：
    /// * 界面：波形区下方红色坏段带 + 侧栏列表；
    /// * 播放（`player::PlayerHandle::spawn_skipping`）：**硬**坏段自动跳过；
    /// * 导出（`export_ranges_inner`）：**硬**坏段默认剔除，并在报告里写"已跳过 N 秒"。
    pub health: health::HealthReport,
}

impl Clip {
    /// **视频流的时长**（毫秒）—— **P27 起"取样时刻"的唯一权威**。
    ///
    /// 为什么不能用容器时长（`Clip::duration_ms`，来自 `format.duration`）：
    /// 容器里可以带一条**更长的音轨**（例如录到一半视频流就停了、或最后一帧没写完），
    /// 于是 `容器时长 > 视频流时长`。采样时刻一旦落进这段"只有音频"的尾巴里，
    /// 解码**必然**失败 —— 实机报错就是这一条：
    /// `MP4 处理失败：… 在 687695 ms 附近没有可解码的视频帧`。
    ///
    /// 拿不到（流里没写 duration / 为 0 / 没有视频流）时返回 `None`：
    /// 调用方回退到容器时长并**记一行日志**（见 `ui::editor::p27_sample_span_end_ms`）。
    pub fn video_duration_ms(&self) -> Option<i64> {
        self.video
            .as_ref()
            .map(|v| v.duration_ms)
            .filter(|d| *d > 0)
    }

    /// 帧周期（毫秒）。帧率未知时按 1/60 秒估计（规格要求）。
    pub fn frame_period_ms(&self) -> f64 {
        let fps = self.video.as_ref().map(|v| v.fps).unwrap_or(0.0);
        if fps.is_finite() && fps > 0.1 {
            1000.0 / fps
        } else {
            1000.0 / FALLBACK_FPS
        }
    }

    /// 关键帧比较的容差 = 半个帧周期
    pub fn snap_tolerance_ms(&self) -> f64 {
        self.frame_period_ms() / 2.0
    }

    pub fn width(&self) -> i32 {
        self.video.as_ref().map(|v| v.width).unwrap_or(0)
    }

    pub fn height(&self) -> i32 {
        self.video.as_ref().map(|v| v.height).unwrap_or(0)
    }

    /// 文件名（界面显示用）
    pub fn file_name(&self) -> String {
        self.src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.src.to_string_lossy().into_owned())
    }
}

// ---------------------------------------------------------------------------
// 打开 + 关键帧扫描（P34：拆成"探测"与"续扫"两段，首帧不必等扫描）
// ---------------------------------------------------------------------------

/// **打开 + 读出流信息**（P34 的第一段，快）：解复用探测，**不解任何音频/视频帧**。
///
/// 返回 `(Input, Clip)`：把 FFmpeg 格式上下文一起交出来，调用方可以**接着用它扫关键帧**
/// （[`scan_pass`]）而不必重新打开文件。
///
/// 交出的 clip 里 `keyframes_ms` 为空、`keyframes_scanned = false`（有视频流时）——
/// 关键帧表要等第二段。纯音频 / 没有视频流时 `keyframes_scanned = true`（没什么可扫）。
fn open_input_and_info(path: &Path) -> Result<(Input, Clip), AppError> {
    if !path.is_file() {
        return Err(AppError::Mp4(format!("文件不存在：{}", path.display())));
    }
    let c_path = cpath(path)?;
    // SAFETY: 本函数内所有 FFI 调用的指针都由本函数分配、由 RAII 守卫释放；
    // c_path 的生命周期覆盖整个块，FFmpeg 不会长期保存该指针。
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

        let ret = ff::avformat_find_stream_info(ictx.0, ptr::null_mut());
        if ret < 0 {
            return Err(AppError::Mp4(format!(
                "读取 {} 的流信息失败：{}",
                path.display(),
                av_err(ret)
            )));
        }
        crate::mux::validate_input_streams(ictx.0)?;

        let mut video: Option<StreamInfo> = None;
        let mut audio: Option<StreamInfo> = None;
        let mut max_stream_dur = 0i64;
        for i in 0..(*ictx.0).nb_streams as usize {
            let st = *(*ictx.0).streams.add(i);
            let par = (*st).codecpar;
            let tb = (*st).time_base;
            let dur_ms = ts_to_ms((*st).duration, tb);
            if dur_ms > max_stream_dur {
                max_stream_dur = dur_ms;
            }
            match (*par).codec_type {
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO if video.is_none() => {
                    let mut fps = rational_f64((*st).avg_frame_rate);
                    if !(fps.is_finite() && fps > 0.1) {
                        fps = rational_f64((*st).r_frame_rate);
                    }
                    video = Some(StreamInfo {
                        index: (*st).index,
                        codec: codec_name(par),
                        width: (*par).width,
                        height: (*par).height,
                        fps: if fps.is_finite() && fps > 0.1 {
                            fps
                        } else {
                            0.0
                        },
                        time_base: (tb.num, tb.den),
                        sample_rate: 0,
                        channels: 0,
                        duration_ms: dur_ms,
                    });
                }
                ff::AVMediaType::AVMEDIA_TYPE_AUDIO if audio.is_none() => {
                    audio = Some(StreamInfo {
                        index: (*st).index,
                        codec: codec_name(par),
                        width: 0,
                        height: 0,
                        fps: 0.0,
                        time_base: (tb.num, tb.den),
                        sample_rate: (*par).sample_rate,
                        channels: (*par).ch_layout.nb_channels,
                        duration_ms: dur_ms,
                    });
                }
                _ => {}
            }
        }

        let duration_ms = if (*ictx.0).duration > 0 {
            (*ictx.0).duration / (AV_TIME_BASE / 1000)
        } else {
            max_stream_dur
        };

        let keyframes_scanned = video.is_none();
        Ok((
            ictx,
            Clip {
                src: path.to_path_buf(),
                duration_ms,
                video,
                audio,
                keyframes_ms: Vec::new(),
                keyframes_scanned,
                // 体检报告要等全片遍历（`health::scan_pass`）才有内容；
                // 这里给空报告 = "还不知道"，`is_clean()` 为 true（不误报）
                health: health::HealthReport::default(),
            },
        ))
    }
}

/// **只探测流信息**（P34）：拿到 clip 就返回，**不扫关键帧**。
///
/// 给"只要元数据"的调用方用（界面首帧优先就靠它：毫秒级，长片也不卡）。
/// 要关键帧表的调用方请用 [`open`] 或 [`open_streaming`]。
pub fn probe(path: &Path) -> Result<Clip, AppError> {
    let (_ictx, clip) = open_input_and_info(path)?;
    Ok(clip)
}

/// **全片一次遍历**（P34 的第二段，慢）：关键帧表 + 编码体检报告。
///
/// P35 起这两件事**共用同一次读盘**：关键帧扫描本来就要逐包读全片，
/// 体检只是在那次遍历里顺手多记几个判据（读包错误 / 数据比容器短 /
/// 时间戳跳变 / 零关键帧），所以**零额外 IO**。
/// 拆成两次读全片是纯浪费 —— 1 GB 文件就是多读 1 GB。
///
/// 中途读包出错时**保留已扫到的部分**并把它记成一段 [`health::BadKind::ReadError`]
/// （与改造前 [`open`] 的口径一致：扫描失败不该让"打开文件"整个失败，
/// 但**必须留下证据** —— 否则界面无从解释"为什么后半段剪不动"）。
///
/// `clip` 只需提供视频流下标、时间基与视频流时长。
fn scan_pass(ictx: &Input, clip: &Clip, cancel: Option<&AtomicBool>) -> Option<health::ScanPass> {
    let Some(v) = clip.video.as_ref() else {
        // 纯音频素材：没有"视频编码错误"这回事，关键帧表也没什么可扫
        return Some(health::ScanPass::default());
    };
    health::scan_pass(
        ictx,
        v.index,
        ff::AVRational {
            num: v.time_base.0,
            den: v.time_base.1,
        },
        v.duration_ms,
        cancel,
    )
}

/// **打开 + 流式两段式**（P34 界面打开路径专用）：
///
/// 1. 先 `open_input_and_info` 拿到 clip，**立刻**经 `emit` 交出去
///    （界面据此起首帧预览/胶片条，**不必等关键帧扫描**）；
/// 2. 再在**同一条线程、同一个格式上下文**上 `scan_pass`，
///    扫完返回关键帧表 + 编码体检报告（关键帧表可能因读包出错而是不完整的一部分，
///    语义与改造前一致；体检报告会把那次读错如实记成一段坏段）。
///
/// 只有"探测本身就失败"（文件不存在/打不开/没有流信息）才返回 `Err`。
/// 没有视频流时不扫，返回空表 + 空报告。
pub fn open_streaming<F: FnOnce(&Clip)>(
    path: &Path,
    emit: F,
) -> Result<(Clip, health::ScanPass), AppError> {
    match open_streaming_cancel(path, emit, None)? {
        Some(v) => Ok(v),
        // 没有取消令牌就不可能出现"取消"分支；保持公开 API 的原有契约。
        None => Err(AppError::Cancelled),
    }
}

/// 同 [`open_streaming`]，但允许在探测后、扫描中协作取消。
///
/// `Ok(None)` 表示已取消：此时 `emit` 可能已经交出了 clip，但**不得**把半截关键帧表
/// 当成完整结果应用（调用方应按世代丢弃）。取消检查在包循环内，最多再读一个包。
pub(crate) fn open_streaming_cancel<F: FnOnce(&Clip)>(
    path: &Path,
    emit: F,
    cancel: Option<&AtomicBool>,
) -> Result<Option<(Clip, health::ScanPass)>, AppError> {
    let (ictx, clip) = open_input_and_info(path)?;
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
        return Ok(None);
    }
    emit(&clip);
    if clip.video.is_none() {
        return Ok(Some((clip, health::ScanPass::default())));
    }
    Ok(scan_pass(&ictx, &clip, cancel).map(|pass| (clip, pass)))
}

/// 打开视频（分片 mp4 与普通 mp4 都可以），读取流信息、**扫描全部关键帧**并**做一次编码体检**。
///
/// 同步一把做完（[`open_streaming`] 的便捷包装）：给烟雾工具、单测和
/// "就是要一次拿全"的调用方用。**界面打开路径不要用它** —— 它会把 UI 线程
/// 按"全集文件大小"阻塞住（P34 改造的起因），请用 [`open_streaming`]。
pub fn open(path: &Path) -> Result<Clip, AppError> {
    let (mut clip, pass) = open_streaming(path, |_| {})?;
    clip.keyframes_ms = pass.keyframes_ms;
    clip.keyframes_scanned = true;
    clip.health = pass.health;
    Ok(clip)
}

// ---------------------------------------------------------------------------
// 关键帧吸附（规格核心）
// ---------------------------------------------------------------------------

/// 把时间夹紧到 `[0, duration]`
fn clamp_ms(clip: &Clip, t_ms: i64) -> i64 {
    t_ms.clamp(0, clip.duration_ms.max(0))
}

/// **起点吸附**：返回 **≥ t 的最近关键帧**（"后一个关键帧"）。
///
/// * 允许 `keyframe >= t - 半个帧周期` 的关键帧入选（见模块文档的半帧容差说明）；
/// * `t` 超出范围时先夹紧到 `[0, duration]`；
/// * 若 `t` 已在最后一个关键帧之后，返回最后一个关键帧（规格要求）。
pub fn snap_start(clip: &Clip, t_ms: i64) -> i64 {
    let t = clamp_ms(clip, t_ms);
    if clip.keyframes_ms.is_empty() {
        // 没有关键帧信息（例如纯音频）→ 退化为不吸附
        return t;
    }
    let tol = clip.snap_tolerance_ms();
    for &k in &clip.keyframes_ms {
        if (k as f64) >= (t as f64) - tol {
            return k.max(0);
        }
    }
    clip.keyframes_ms.last().copied().unwrap_or(0).max(0)
}

/// **终点吸附**：返回 **≤ t 的最近关键帧**（"前一个关键帧"）。
///
/// 同样带半帧容差；`t` 夹紧到 `[0, duration]`；若 `t` 早于第一个关键帧，
/// 返回第一个关键帧。
pub fn snap_end(clip: &Clip, t_ms: i64) -> i64 {
    let t = clamp_ms(clip, t_ms);
    if clip.keyframes_ms.is_empty() {
        return t;
    }
    let tol = clip.snap_tolerance_ms();
    for &k in clip.keyframes_ms.iter().rev() {
        if (k as f64) <= (t as f64) + tol {
            return k.max(0);
        }
    }
    clip.keyframes_ms[0].max(0)
}

/// 严格按规格吸附出的区间；`start >= end` 时报错（空选区无法 copy 导出）。
pub fn snap_range(clip: &Clip, start_ms: i64, end_ms: i64) -> Result<(i64, i64), AppError> {
    let s = snap_start(clip, start_ms);
    let e = snap_end(clip, end_ms);
    if s >= e {
        return Err(AppError::Mp4(format!(
            "剪辑区间无效：吸附后起点 {s} ms 不早于终点 {e} ms（关键帧间隔太大或选区太窄）"
        )));
    }
    Ok((s, e))
}

/// **导出/显示实际使用的有效区间**。
///
/// 与 [`snap_range`] 的唯一差别在终点：如果用户把终点拖到（或超过）媒体末尾，
/// 就取**媒体真实末尾**而不是"最后一个关键帧"——否则最后一个 GOP 会被无声丢掉
/// （例如 5 秒录像、关键帧在 0/2/4 s，用户选到末尾却只能导出到 4 s）。
/// 这个"取到末尾"的区间同时满足 `mux::remux_range` 的"起点 → EOF"语义。
pub fn effective_range(clip: &Clip, start_ms: i64, end_ms: i64) -> Result<(i64, i64), AppError> {
    let (s, e) = snap_range(clip, start_ms, end_ms)?;
    let tol = clip.snap_tolerance_ms();
    if (end_ms as f64) + tol >= clip.duration_ms as f64 && clip.duration_ms > e {
        Ok((s, clip.duration_ms))
    } else {
        Ok((s, e))
    }
}

// ---------------------------------------------------------------------------
// 关键帧查询（纯函数，给界面做"鼠标附近的关键帧缩略图"用）
// ---------------------------------------------------------------------------

/// 取距离 `t_ms` 最近的至多 `n` 个关键帧，**按时间升序返回**。
///
/// 为什么升序返回而不是按距离排序：界面要沿时间轴从左到右把这几张缩略图铺开，
/// 紧挨鼠标的那一张自然落在中间，"前后各几张"看起来最直观。
///
/// 语义细节（都有单测）：
/// * `n == 0` 或素材没有关键帧 → 空；
/// * `n >= 关键帧数` → 返回全部关键帧（而不是补重复项）；
/// * `t_ms` 先按 `clamp_ms` 夹到 `[0, duration]`；
/// * 距离相同时优先取**不晚于** `t_ms` 的那个 —— 保证同一鼠标位置永远给同一结果；
/// * 前提：`clip.keyframes_ms` 是升序去重的（[`open`] 保证）。
pub(crate) fn nearest_keyframes(clip: &Clip, t_ms: i64, n: usize) -> Vec<i64> {
    let kfs = &clip.keyframes_ms;
    if n == 0 || kfs.is_empty() {
        return Vec::new();
    }
    if n >= kfs.len() {
        return kfs.clone();
    }
    let t = clamp_ms(clip, t_ms);
    // 二分找到第一个 >= t 的关键帧：左侧全部 < t，右侧全部 >= t
    let ins = kfs.partition_point(|k| *k < t);
    let mut left = ins; // 下一个候选的左端（ins-1 是最近已取到的）
    let mut right = ins; // 下一个候选的右端
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let l = if left > 0 { Some(left - 1) } else { None };
        let r = if right < kfs.len() { Some(right) } else { None };
        match (l, r) {
            (Some(li), Some(ri)) => {
                // 距离相同取左侧（`<=`），保证结果与鼠标位置的对应关系是确定的
                if t - kfs[li] <= kfs[ri] - t {
                    out.push(kfs[li]);
                    left -= 1;
                } else {
                    out.push(kfs[ri]);
                    right += 1;
                }
            }
            (Some(li), None) => {
                out.push(kfs[li]);
                left -= 1;
            }
            (None, Some(ri)) => {
                out.push(kfs[ri]);
                right += 1;
            }
            (None, None) => break, // 理论上取满 n 个之前不会走到这里
        }
    }
    out.sort_unstable();
    out
}

/// 取距离 `t_ms` **最近的那一个**关键帧；没有关键帧（或素材为空）时为 `None`。
///
/// 这是 `nearest_keyframes` 在 `n = 1` 时的特例，单独给一个名字是因为界面现在的
/// 诉求就是"只显示离鼠标最近的那**一张**缩略图"：语义（夹紧、距离相同取不晚于 `t` 的
/// 那个、升序前提）与批量版本**完全一致**，两处永远不会给出互相矛盾的答案。
pub fn nearest_keyframe(clip: &Clip, t_ms: i64) -> Option<i64> {
    nearest_keyframes(clip, t_ms, 1).into_iter().next()
}

// ---------------------------------------------------------------------------
// 多段（分片）模型：若干区间合并导出
// ---------------------------------------------------------------------------

/// **一个待导出区间**（毫秒，未吸附的原始值）。
///
/// `end_ms` 到片尾时（>= 素材时长，在吸附容差内）按"到片尾"处理 —— 与
/// [`effective_range`] 的单段口径一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
}

impl Segment {
    pub fn new(start_ms: i64, end_ms: i64) -> Self {
        Self { start_ms, end_ms }
    }
}

/// **校验并解析分段列表**（多段合并导出的前置）。
///
/// 1. 每段起点吸附到"后一个关键帧"、终点吸附到"前一个关键帧"
///    （终点贴片尾时取**媒体真实末尾**，与 [`effective_range`] 同口径）；
/// 2. 列表**顺序即播放顺序**（P36）：允许乱序、允许重叠、允许重复。
///    这不是放宽约束，而是把列表的正确定义摆正 —— 它是一份**播放列表**，
///    不是"把影片切成互不重叠的几块"。`remux_ranges` 逐段独立 seek +
///    累加平移，天然支持任意顺序（见那里的注释）；
/// 3. 每段吸附后必须 `start < end`（空段无法导出）；
/// 4. **剔除硬编码坏段**（P35，见 [`resolve_segments_ex`]）；
/// 5. 返回 `(吸附后的区间列表, 合并总时长)`，总时长 = Σ 各段长度。
pub fn resolve_segments(clip: &Clip, raw: &[Segment]) -> Result<(Vec<Segment>, i64), AppError> {
    let (segs, total, _skipped) = resolve_segments_ex(clip, raw)?;
    Ok((segs, total))
}

/// [`resolve_segments`] 的完整版：多返回一个"因编码错误被剔除的毫秒数"。
///
/// **剔除（P35）**：每段吸附完之后，再按 [`Clip::health`] 里的**硬坏段**切开
/// （[`health::HealthReport::excise`]），并对每个切出来的子区间做两件事：
/// * 起点**重新吸附到关键帧** —— 切除后的新起点必然不是关键帧，直接 copy 导出
///   会让产物开头花屏（这正是本功能要消除的现象，绝不能自己制造）；
/// * `start >= end` 的子区间直接丢掉（切完太窄，导不出东西）。
///
/// **幂等性**：干净文件（`health` 里没有硬坏段）时 `excise` 原样返回，
/// 重新吸附也落在同一个关键帧上，所以 `skipped == 0`、区间一字不变 ——
/// 这是"改造前后行为完全一致"的保证，有单测钉着。
///
/// **顺序（P36）**：输出列表与输入列表**逐项一一对应**，顺序就是用户排的顺序。
/// 曾经这里校验"必须升序且互不重叠"，那是把列表当成"时间切分"；改成播放列表后
/// 这条校验不仅多余，还会让"把第 3 段拖到第 1 位"直接报错。逐段吸附彼此独立，
/// 拼接侧也不依赖顺序，所以删掉它对既有行为零影响（干净文件下的单测全部照旧）。
pub fn resolve_segments_ex(
    clip: &Clip,
    raw: &[Segment],
) -> Result<(Vec<Segment>, i64, i64), AppError> {
    if raw.is_empty() {
        return Err(AppError::Mp4("没有需要导出的分段".into()));
    }
    if clip.video.is_some() && clip.keyframes_ms.is_empty() {
        return Err(AppError::Mp4(
            "源视频没有可用关键帧，无法安全地执行 copy 导出".into(),
        ));
    }
    let mut snapped: Vec<Segment> = Vec::with_capacity(raw.len());
    for (i, seg) in raw.iter().enumerate() {
        let s = snap_start(clip, seg.start_ms);
        // 终点：默认吸附到"前一个关键帧"；用户把终点拖到片尾时取真实片尾
        // （与 effective_range 完全同口径，否则最后一个 GOP 会被无声丢掉）
        let tol = clip.snap_tolerance_ms();
        let e_snap = snap_end(clip, seg.end_ms);
        let e = if (seg.end_ms as f64) + tol >= clip.duration_ms as f64 && clip.duration_ms > e_snap
        {
            clip.duration_ms
        } else {
            e_snap
        };
        if s >= e {
            return Err(AppError::Mp4(format!(
                "第 {} 个分段无效：吸附后起点 {} ms 不早于终点 {} ms（关键帧间隔太大或选区太窄）",
                i + 1,
                s,
                e
            )));
        }
        snapped.push(Segment {
            start_ms: s,
            end_ms: e,
        });
    }

    // ---- P35：按硬编码坏段切开 ----
    let mut out: Vec<Segment> = Vec::with_capacity(snapped.len());
    for seg in &snapped {
        for (a, b) in clip.health.excise(seg.start_ms, seg.end_ms) {
            // 切出来的新起点不是关键帧 → 必须重新吸附，否则产物开头花屏。
            // 干净文件下这一步是幂等的（`a` 本来就在关键帧上），所以不会改变既有行为。
            let s = snap_start(clip, a);
            if s >= b {
                continue; // 切完太窄（不到一个 GOP），导不出东西
            }
            out.push(Segment {
                start_ms: s,
                end_ms: b,
            });
        }
    }
    if out.is_empty() {
        return Err(AppError::Mp4(
            "选区全部落在编码错误的区块里，没有可导出的内容".into(),
        ));
    }
    let total: i64 = out.iter().map(|s| s.end_ms - s.start_ms).sum();
    // 剔除量 = 吸附后原始总长 − 实际写出总长（含切点重新吸附的损失，见 ExportReport 的说明）
    let before: i64 = snapped.iter().map(|s| s.end_ms - s.start_ms).sum();
    Ok((out, total, (before - total).max(0)))
}

// ---------------------------------------------------------------------------
// copy 导出
// ---------------------------------------------------------------------------

/// 导出报告：给界面显示，也给调用方/回归脚本做断言
#[derive(Debug, Clone)]
pub struct ExportReport {
    /// 用户请求的区间（未吸附）
    pub requested_start_ms: i64,
    pub requested_end_ms: i64,
    /// 实际写出的起点/终点（起点一定是关键帧）
    pub start_ms: i64,
    pub end_ms: i64,
    pub dst: PathBuf,
    pub packets: u64,
    pub bytes: u64,
    pub src_width: i32,
    pub src_height: i32,
    pub dst_width: i32,
    pub dst_height: i32,
    /// 产物分辨率是否与源完全一致（copy 导出必须为 true，否则导出会直接报错）
    pub same_resolution: bool,
    /// 产物探测到的时长
    pub dst_duration_ms: i64,
    /// **分段数**：1 = 普通单段裁剪导出；>1 = 多段合并导出
    /// （[`export_ranges`]）。
    pub segments: usize,
    /// **合并总时长**（毫秒）：Σ 各段长度。单段导出时 = `end_ms - start_ms`；
    /// 多段导出时是真正的"合并后时长"（`duration_ms()` 给出的是首尾跨度，
    /// 段间空洞不计入）。
    pub total_duration_ms: i64,
    /// **因编码错误被剔除掉的时长**（毫秒，P35）。
    ///
    /// 口径：`吸附后的原始总时长 − 实际写出的总时长`。它同时包含两件事：
    /// * 被 [`health::BadKind::is_hard`] 的坏段切掉的部分（主要来源）；
    /// * 切点后**重新吸附到关键帧**丢掉的那一小截（每个切点 ≤ 一个 GOP）。
    ///
    /// 干净文件恒为 `0`（重吸附是幂等的，见 `resolve_segments_ex` 的注释）。
    /// 界面据此显示"已跳过 N 秒编码错误内容" —— **必须显示**，
    /// 否则用户会以为导出的就是完整的选区。
    pub skipped_bad_ms: i64,
}

impl ExportReport {
    /// 实际选区长度
    pub fn duration_ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }

    /// 是否按位流拷贝（未重新编码）。成立的前提是分辨率一致——
    /// 本模块的导出路径只搬 packet，所以这个值恒为 true。
    pub fn is_bitstream_copy(&self) -> bool {
        self.same_resolution
    }
}

/// **copy 导出**：把 `[start_ms, end_ms]` 原样搬到 `dst`，不重新编码、不改变分辨率。
///
/// 单段只是 `export_ranges_inner` 的一段特例 —— 两条路径**共用同一份实现**，
/// 所以"吸附 / 剔除坏段 / 自校验"的口径永远不会分叉
/// （改造前这里是两份几乎一样的代码，P35 加剔除时差点只改了一边）。
pub fn export_copy(
    clip: &Clip,
    start_ms: i64,
    end_ms: i64,
    dst: &Path,
) -> Result<ExportReport, AppError> {
    export_ranges_inner(clip, &[Segment::new(start_ms, end_ms)], dst, None)
}

pub(crate) fn export_copy_cancel(
    clip: &Clip,
    start_ms: i64,
    end_ms: i64,
    dst: &Path,
    cancel: &AtomicBool,
) -> Result<ExportReport, AppError> {
    export_ranges_inner(clip, &[Segment::new(start_ms, end_ms)], dst, Some(cancel))
}

/// **导出的唯一实现**：单段 / 多段都走这里。
///
/// 1. [`resolve_segments_ex`]：逐段吸附关键帧 + **剔除硬编码坏段**（P35），
///    同时给出"被剔除的毫秒数"；
/// 2. 只有一段 → [`mux::remux_range`]；多段 → [`mux::remux_ranges`]
///    （`-c copy` 语义，只搬 packet、不解码、不重编码）：
///    * 起点 → 不晚于它的关键帧（`av_seek_frame(AVSEEK_FLAG_BACKWARD)`）；
///    * 终点 → 拷贝到**不早于该时间戳的包之前**为止（`src/mux/mod.rs` 的终点判定）；
///    * 终点拖到/超过媒体末尾时，吸附阶段会把它换成媒体真实末尾——
///      这时判定天然在 EOF 处结束，行为与"拷到 EOF"一致；
/// 3. **自身校验**：用 [`mux::probe`] 读回产物，断言分辨率与源一致、
///    时长 ≈ 合并总时长（容差按段数累加：每段最多多写一帧 + 容器取整）。
fn export_ranges_inner(
    clip: &Clip,
    segments: &[Segment],
    dst: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<ExportReport, AppError> {
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err(AppError::Mp4("导出已取消".into()));
    }
    let (ranges, total_ms, skipped_ms) = resolve_segments_ex(clip, segments)?;

    // 空间紧时优先关掉 `+faststart`：moov 留在文件末尾仍然可播、可继续剪辑，
    // 比"写到一半因回拷空间不足而失败"好；连单份产物都放不下才报错。
    let mut faststart = true;
    if let Some(dir) = dst.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|err| AppError::Mp4(format!("创建目录 {} 失败：{err}", dir.display())))?;
            // copy 导出按“选中时长 / 源时长”估算体积；播放列表允许重复片段，
            // 所以不能简单假设产物 ≤ 源文件。`+faststart` 还要再回拷一份完整产物。
            if let Some(free) = crate::disk::free_bytes(dir) {
                let src = std::fs::metadata(&clip.src).map(|m| m.len()).unwrap_or(0);
                let source_span = clip.duration_ms.max(1) as u128;
                let selected_span = total_ms.max(0) as u128;
                let estimated = ((src as u128)
                    .saturating_mul(selected_span)
                    .saturating_add(source_span - 1)
                    / source_span)
                    .min(u64::MAX as u128) as u64;
                // 10% 封装/音频余量 + faststart 临时回拷 + 固定安全余量。
                let estimated = estimated
                    .saturating_add(estimated / 10)
                    .max(16 * 1024 * 1024);
                if src > 0 {
                    let fixed = 64 * 1024 * 1024;
                    let single = estimated.saturating_add(fixed);
                    let dual = estimated.saturating_mul(2).saturating_add(fixed);
                    if free < single {
                        return Err(AppError::Mp4(format!(
                            "导出目录空间不足：至少需要约 {}，实际 {}（{}）",
                            crate::disk::human(single),
                            crate::disk::human(free),
                            dir.display()
                        )));
                    }
                    faststart = free >= dual;
                }
            }
        }
    }

    // 先在 `.part` 里写、读回校验，全部通过后才替换正式目标。
    // 这样"导出到一半取消 / 产物时长异常 / 探测失败"都不会破坏用户已有的同名文件。
    let out = mux::AtomicExport::new(&clip.src, dst)?;
    let range_pairs: Vec<(i64, i64)> = ranges.iter().map(|s| (s.start_ms, s.end_ms)).collect();
    let bytes = if range_pairs.len() == 1 {
        mux::remux_range_to(
            &clip.src,
            out.temp(),
            Some(range_pairs[0]),
            faststart,
            cancel,
        )?
    } else {
        mux::remux_ranges_to(&clip.src, out.temp(), &range_pairs, faststart, cancel)?
    };
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err(AppError::Mp4("导出已取消".into()));
    }

    // ---- 自身校验 ----
    let brief = mux::probe(out.temp())?;
    let dst_video = brief.streams.iter().find(|x| x.kind == "video");
    let (dst_width, dst_height) = dst_video.map(|v| (v.width, v.height)).unwrap_or((0, 0));
    let (src_width, src_height) = (clip.width(), clip.height());
    let same_resolution = src_width == dst_width && src_height == dst_height;
    if clip.video.is_some() && !same_resolution {
        return Err(AppError::Mp4(format!(
            "导出后分辨率发生变化（源 {src_width}x{src_height}，产物 {dst_width}x{dst_height}），\
             这不该发生在 copy 导出里"
        )));
    }
    let dst_duration_ms = brief.duration_ms;
    // 每段最多"多写一帧"（包边界）+ 150 ms 容器取整，容差按段数累加
    let duration_tol = ((clip.frame_period_ms() * 3.0).max(150.0) as i64)
        .checked_mul(ranges.len() as i64)
        .unwrap_or(i64::MAX)
        .max(150);
    if (dst_duration_ms - total_ms).abs() > duration_tol {
        return Err(AppError::Mp4(format!(
            "导出时长异常：期望约 {total_ms} ms（{} 段合计），实际 {dst_duration_ms} ms",
            ranges.len()
        )));
    }
    let packets = count_packets(out.temp())?;
    let bytes = if bytes == 0 {
        std::fs::metadata(out.temp()).map(|m| m.len()).unwrap_or(0)
    } else {
        bytes
    };

    // P52：下面要用 `ranges` 的首尾和 `segments` 的首尾。它们的非空性由
    // `resolve_segments_ex` 保证（输入为空、或全部落在硬编码坏块里都会先返回 Err），
    // 但那是**另一个函数**里的不变式 —— 原先这里写的是 `ranges[0]` / `ranges.last().unwrap()`，
    // 一旦那边改了约定，这里就变成 panic。就地接住，退化成一条错误提示
    // （与本项目"不该发生的状态也走 AppError、不崩进程"的约定一致）。
    let (Some(first), Some(last), Some(req_first), Some(req_last)) = (
        ranges.first().copied(),
        ranges.last().copied(),
        segments.first(),
        segments.last(),
    ) else {
        return Err(AppError::Mp4("吸附后没有可导出的分段".into()));
    };
    tracing::info!(
        src = %clip.src.display(),
        dst = %dst.display(),
        segments = ranges.len(),
        total_ms,
        skipped_bad_ms = skipped_ms,
        bytes,
        packets,
        "copy 导出完成（未重新编码，分辨率未改变）"
    );
    if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err(AppError::Mp4("导出已取消".into()));
    }
    out.commit()?;

    Ok(ExportReport {
        requested_start_ms: req_first.start_ms,
        requested_end_ms: req_last.end_ms,
        start_ms: first.start_ms,
        end_ms: last.end_ms,
        dst: dst.to_path_buf(),
        packets,
        bytes,
        src_width,
        src_height,
        dst_width,
        dst_height,
        same_resolution,
        dst_duration_ms,
        segments: ranges.len(),
        total_duration_ms: total_ms,
        skipped_bad_ms: skipped_ms,
    })
}

/// **多段合并的 copy 导出**：把 `segments` 里的每一段依次拼接到 `dst`，
/// 不重新编码、不改变分辨率（[`mux::remux_ranges`]）。
///
/// 与 [`export_copy`] 是**同一个实现**（`export_ranges_inner`）的两个入口：
/// * 分段先过 [`resolve_segments_ex`]：逐段吸附关键帧、查排序/重叠/空段，
///   再**剔除硬编码坏段**（P35）；
/// * 只有一段时走的仍是 `remux_range`，行为与 [`export_copy`] 完全一致；
/// * 每段起点吸附后即是关键帧（切坏段后的新起点会**再吸附一次**），
///   所以产物每段开头都能解码 —— 这是"导出不再花屏"的关键。
pub fn export_ranges(
    clip: &Clip,
    segments: &[Segment],
    dst: &Path,
) -> Result<ExportReport, AppError> {
    export_ranges_inner(clip, segments, dst, None)
}

pub(crate) fn export_ranges_cancel(
    clip: &Clip,
    segments: &[Segment],
    dst: &Path,
    cancel: &AtomicBool,
) -> Result<ExportReport, AppError> {
    export_ranges_inner(clip, segments, dst, Some(cancel))
}

/// 数一下文件里的 packet 数（导出报告用；只解封装、不解码）
pub(crate) fn count_packets(path: &Path) -> Result<u64, AppError> {
    if !path.is_file() {
        return Err(AppError::Mp4(format!(
            "只支持本地文件：{}",
            path.display()
        )));
    }
    let c_path = cpath(path)?;
    // SAFETY: 所有指针都在本函数内分配并有 RAII 守卫；c_path 生命周期覆盖整个块。
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
        crate::mux::validate_input_streams(ictx.0)?;
        let pkt = Packet(ff::av_packet_alloc());
        if pkt.0.is_null() {
            return Err(AppError::Mp4("av_packet_alloc 失败".into()));
        }
        let mut n = 0u64;
        loop {
            let ret = ff::av_read_frame(ictx.0, pkt.0);
            if ret < 0 {
                break;
            }
            n += 1;
            ff::av_packet_unref(pkt.0);
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// 缩略图
// ---------------------------------------------------------------------------

/// 解码 `at_ms` 附近的**一帧**，等比缩放成不超过 `max_w × max_h` 的 **RGBA8**。
///
/// 返回 `(宽, 高, RGBA 字节)`，可直接喂给 `egui::ColorImage::from_rgba_unmultiplied`。
pub fn thumbnail(
    path: &Path,
    at_ms: i64,
    max_w: u32,
    max_h: u32,
) -> Result<(u32, u32, Vec<u8>), AppError> {
    if !path.is_file() {
        return Err(AppError::Mp4(format!(
            "只支持本地文件：{}",
            path.display()
        )));
    }
    let c_path = cpath(path)?;
    let max_w = max_w.max(1);
    let max_h = max_h.max(1);
    let at_ms = at_ms.max(0);
    // SAFETY: 输入上下文/解码器/帧/包全部由本函数分配，分别由 Input/Codec/Frame/Packet/Sws
    // 守卫释放；c_path 生命周期覆盖整个块。
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
        let ret = ff::avformat_find_stream_info(ictx.0, ptr::null_mut());
        if ret < 0 {
            return Err(AppError::Mp4(format!("读取流信息失败：{}", av_err(ret))));
        }
        crate::mux::validate_input_streams(ictx.0)?;

        // 找第一条视频流
        let mut vindex = -1i32;
        for i in 0..(*ictx.0).nb_streams as usize {
            let st = *(*ictx.0).streams.add(i);
            if (*(*st).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO {
                vindex = (*st).index;
                break;
            }
        }
        if vindex < 0 {
            return Err(AppError::Mp4(format!(
                "{} 里没有视频流，无法取缩略图",
                path.display()
            )));
        }
        let vstream = *(*ictx.0).streams.add(vindex as usize);
        let vtb = (*vstream).time_base;

        // 打开解码器
        let codec = ff::avcodec_find_decoder((*(*vstream).codecpar).codec_id);
        if codec.is_null() {
            return Err(AppError::Mp4("FFmpeg 找不到对应的视频解码器".into()));
        }
        let codec_guard = Codec(ff::avcodec_alloc_context3(codec));
        if codec_guard.0.is_null() {
            return Err(AppError::Mp4("avcodec_alloc_context3 失败".into()));
        }
        let ret = ff::avcodec_parameters_to_context(codec_guard.0, (*vstream).codecpar);
        if ret < 0 {
            return Err(AppError::Mp4(format!("复制解码参数失败：{}", av_err(ret))));
        }
        (*codec_guard.0).pkt_timebase = vtb;
        limit_decoder_pixels(codec_guard.0);
        let ret = ff::avcodec_open2(codec_guard.0, codec, ptr::null_mut());
        if ret < 0 {
            return Err(AppError::Mp4(format!(
                "打开视频解码器失败：{}",
                av_err(ret)
            )));
        }

        // 定位到目标时刻之前的关键帧（失败就从文件开头解，最多多解几帧）
        let ts_us = at_ms * (AV_TIME_BASE / 1000);
        let ret = ff::av_seek_frame(ictx.0, -1, ts_us, ff::AVSEEK_FLAG_BACKWARD);
        if ret < 0 {
            tracing::warn!(error = %av_err(ret), "缩略图 seek 失败，将从文件开头解码");
        }

        let pkt = Packet(ff::av_packet_alloc());
        if pkt.0.is_null() {
            return Err(AppError::Mp4("av_packet_alloc 失败".into()));
        }
        let frame = Frame(ff::av_frame_alloc());
        if frame.0.is_null() {
            return Err(AppError::Mp4("av_frame_alloc 失败".into()));
        }

        // 解一帧：send_packet → 收到"显示时间 ≥ at_ms"的那一帧就转换。
        // 注意 frame 缓冲会被下一次 receive 复用，所以命中后必须立刻 sws 转换。
        let mut rgba: Option<(u32, u32, Vec<u8>)> = None;
        let mut packets_read = 0usize;
        'outer: loop {
            let ret = ff::av_read_frame(ictx.0, pkt.0);
            if ret < 0 {
                break;
            }
            if (*pkt.0).stream_index == vindex {
                let send = ff::avcodec_send_packet(codec_guard.0, pkt.0);
                ff::av_packet_unref(pkt.0);
                if send < 0 && send != AVERROR_EOF {
                    tracing::trace!(error = %av_err(send), "send_packet 返回非致命错误");
                }
                loop {
                    let recv = ff::avcodec_receive_frame(codec_guard.0, frame.0);
                    if recv < 0 {
                        break;
                    }
                    let pts = if (*frame.0).best_effort_timestamp != AV_NOPTS_VALUE {
                        (*frame.0).best_effort_timestamp
                    } else {
                        (*frame.0).pts
                    };
                    let frame_ms = ts_to_ms(pts, vtb);
                    if frame_ms >= at_ms || at_ms == 0 {
                        rgba = Some(convert_to_rgba(frame.0, max_w, max_h)?);
                        ff::av_frame_unref(frame.0);
                        break 'outer;
                    }
                    ff::av_frame_unref(frame.0);
                }
                packets_read += 1;
                if packets_read > 4000 {
                    tracing::warn!(
                        "缩略图解码超过 4000 个包仍未命中目标时刻，使用已解出的最后一帧"
                    );
                    break;
                }
            } else {
                ff::av_packet_unref(pkt.0);
            }
        }

        // 目标时刻在文件末尾之后（或一直没命中）：冲刷解码器，取最后一帧
        if rgba.is_none() {
            ff::avcodec_send_packet(codec_guard.0, ptr::null());
            loop {
                let recv = ff::avcodec_receive_frame(codec_guard.0, frame.0);
                if recv < 0 {
                    break;
                }
                rgba = Some(convert_to_rgba(frame.0, max_w, max_h)?);
                ff::av_frame_unref(frame.0);
            }
        }

        rgba.ok_or_else(|| {
            AppError::Mp4(format!(
                "{} 在 {at_ms} ms 附近没有可解码的视频帧",
                path.display()
            ))
        })
    }
}

/// 把 `frame` 转成不超过 `max_w × max_h` 的 RGBA8（等比缩放，不放大）。
///
/// # Safety
/// `frame` 必须指向有效的、已解码的 AVFrame。
unsafe fn convert_to_rgba(
    frame: *mut ff::AVFrame,
    max_w: u32,
    max_h: u32,
) -> Result<(u32, u32, Vec<u8>), AppError> {
    let fw = (*frame).width;
    let fh = (*frame).height;
    if fw <= 0 || fh <= 0 {
        return Err(AppError::Mp4("解码帧尺寸非法".into()));
    }
    let scale = (max_w as f64 / fw as f64)
        .min(max_h as f64 / fh as f64)
        .min(1.0);
    let dw = ((fw as f64 * scale).round() as u32).max(1);
    let dh = ((fh as f64 * scale).round() as u32).max(1);

    let src_fmt = pixel_format((*frame).format)?;
    let key: SwsKey = (src_fmt, fw as u32, fh as u32, dw, dh);

    let stride = dw as usize * 4;
    let mut buf = vec![0u8; stride * dh as usize];
    let dst_data = [buf.as_mut_ptr()];
    let dst_stride = [stride as i32];
    let mut src_linesizes = [0i32; 8];
    for (i, l) in src_linesizes.iter_mut().enumerate() {
        *l = (*frame).linesize[i];
    }

    // SAFETY: src 数据/跨距来自有效解码帧；dst 是刚分配、长度足够的 Vec；
    // 缓存的 sws 上下文与 key（格式/尺寸）一致。
    let written = SWS_CACHE.with(|cell| -> Result<i32, AppError> {
        let mut slot = cell.borrow_mut();
        let rebuild = slot.as_ref().map(|(k, _)| *k != key).unwrap_or(true);
        if rebuild {
            // SAFETY: 参数来自 FFmpeg 自己的格式表与当前帧尺寸；失败返回空指针。
            let ctx = unsafe {
                ff::sws_getContext(
                    fw,
                    fh,
                    src_fmt,
                    dw as i32,
                    dh as i32,
                    ff::AVPixelFormat::AV_PIX_FMT_RGBA,
                    ff::SwsFlags::SWS_BILINEAR as i32,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if ctx.is_null() {
                return Err(AppError::Mp4("sws_getContext 失败".into()));
            }
            *slot = Some((key, Sws(ctx)));
        }
        let sws = &mut slot.as_mut().expect("sws cache 刚填过").1;
        let written = unsafe {
            ff::sws_scale(
                sws.0,
                (*frame).data.as_ptr() as *const *const u8,
                src_linesizes.as_ptr(),
                0,
                fh,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            )
        };
        if written <= 0 {
            return Err(AppError::Mp4("sws_scale 转换失败".into()));
        }
        Ok(written)
    })?;
    let _ = written;
    Ok((dw, dh, buf))
}

// ---------------------------------------------------------------------------
// 测试：全部无界面，素材用随包 ffmpeg 现场生成（不依赖网络）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ffmpeg_exe;

    fn asset_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cap-edit-tests");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 现场生成 4 秒 320x240@30fps、每 1 秒一个关键帧的测试 mp4（含 aac 音轨）。
    /// 返回 `None` 表示环境不具备（缺 ffmpeg 或缺 libopenh264），调用方跳过而不是失败。
    fn gen_mp4(name: &str, frag: bool) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(name);
        let _ = std::fs::remove_file(&out);
        let mut cmd = std::process::Command::new(&ff);
        cmd.args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "4",
            "-c:v",
            "libopenh264",
            "-b:v",
            "500k",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
        ]);
        if frag {
            cmd.args(["-movflags", "frag_keyframe+empty_moov"]);
        }
        cmd.arg(&out);
        match cmd.output() {
            Ok(o) if o.status.success() && out.is_file() => Some(out),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                // 随包 ffmpeg 理论上带 libopenh264，但换机器可能没有；此时跳过而不是误报失败
                if err.contains("Unknown encoder") || err.to_ascii_lowercase().contains("openh264")
                {
                    println!("[skip] 随包 ffmpeg 不具备 libopenh264 编码能力：{err}");
                    None
                } else {
                    panic!("ffmpeg 生成测试素材失败：{err}");
                }
            }
            Err(e) => {
                println!("[skip] 无法运行随包 ffmpeg：{e}");
                None
            }
        }
    }

    /// 造一个"只有时间轴"的假 Clip，用来精确测吸附语义（不依赖真实媒体）
    fn synthetic_clip(duration_ms: i64, keyframes_ms: Vec<i64>, fps: f64) -> Clip {
        Clip {
            src: PathBuf::from("synthetic.mp4"),
            duration_ms,
            video: Some(StreamInfo {
                index: 0,
                codec: "h264".into(),
                width: 320,
                height: 240,
                fps,
                time_base: (1, 90000),
                sample_rate: 0,
                channels: 0,
                duration_ms,
            }),
            audio: None,
            keyframes_ms,
            // 测试素材的表就是最终态（没有"还在扫"的中间态）
            keyframes_scanned: true,
            // 合成素材默认"干净"：没有坏段 → 导出行为与改造前一字不差
            health: health::HealthReport::default(),
        }
    }

    #[test]
    fn snap_start_takes_next_keyframe() {
        let clip = synthetic_clip(10_000, vec![0, 1000, 2000, 3000], 30.0);
        assert_eq!(snap_start(&clip, 0), 0);
        // 半帧容差（30 fps 下 16.67 ms）允许"比请求早不到一帧"的关键帧入选：
        // 这正是防止跳过一个 GOP 的代价，肉眼不可分辨；超过半帧就必须往后找。
        assert_eq!(snap_start(&clip, 1), 0, "1 ms 落在 0 的半帧容差内");
        assert_eq!(
            snap_start(&clip, 20),
            1000,
            "超过半帧就必须吸附到后一个关键帧"
        );
        assert_eq!(snap_start(&clip, 999), 1000);
        assert_eq!(snap_start(&clip, 1000), 1000, "正好落在关键帧上时保持不动");
        assert_eq!(snap_start(&clip, 1500), 2000);
    }

    #[test]
    fn snap_end_takes_previous_keyframe() {
        let clip = synthetic_clip(10_000, vec![0, 1000, 2000, 3000], 30.0);
        assert_eq!(snap_end(&clip, 3000), 3000);
        assert_eq!(snap_end(&clip, 2999), 3000, "半帧容差内应保留该关键帧");
        assert_eq!(snap_end(&clip, 2500), 2000);
        assert_eq!(snap_end(&clip, 2001), 2000);
        assert_eq!(snap_end(&clip, 1500), 1000, "终点必须吸附到前一个关键帧");
        assert_eq!(snap_end(&clip, 0), 0);
    }

    /// 规格里的关键边界：90 kHz 时基下"3 秒关键帧"实存 269999（= 2999.99 ms），
    /// 换算成毫秒是 2999。没有半帧容差时 `snap_start(3000)` 会跳到下一个关键帧（跳过一个 GOP）。
    #[test]
    fn snap_half_frame_tolerance_around_269999() {
        // 30 fps → 帧周期 33.33 ms → 容差 16.67 ms
        let clip = synthetic_clip(6_000, vec![0, 2999, 5999], 30.0);
        assert_eq!(
            snap_start(&clip, 3000),
            2999,
            "2999 与 3000 只差 1 ms（< 半帧），必须被认为就是那个关键帧"
        );
        assert_eq!(snap_end(&clip, 3000), 2999);
        // 超过半帧就不再容忍：3030 与 2999 差 31 ms > 16.67 ms
        assert_eq!(snap_start(&clip, 3030), 5999);
        assert_eq!(snap_end(&clip, 2980), 0, "2980 与 2999 差 19 ms > 半帧");
        // ts_to_ms 本身按最近毫秒取整：269999 @ 1/90000 → 3000
        assert_eq!(
            ts_to_ms(
                269_999,
                ff::AVRational {
                    num: 1,
                    den: 90_000
                }
            ),
            3000
        );
        // 帧率未知时按 1/60 秒估帧周期（容差 ≈ 8.33 ms）
        let unknown = synthetic_clip(6_000, vec![0, 2999], 0.0);
        assert!((unknown.frame_period_ms() - 1000.0 / 60.0).abs() < 1e-6);
    }

    #[test]
    fn snap_clamps_out_of_range() {
        let clip = synthetic_clip(10_000, vec![0, 1000, 2000, 3000], 30.0);
        // 负数夹到 0
        assert_eq!(snap_start(&clip, -5_000), 0);
        assert_eq!(snap_end(&clip, -1), 0);
        // 超出末尾：起点退回最后一个关键帧，终点取最后一个关键帧
        assert_eq!(snap_start(&clip, 99_999), 3000);
        assert_eq!(snap_end(&clip, 99_999), 3000);
        let c2 = synthetic_clip(20_000, vec![0, 1000], 30.0);
        assert_eq!(snap_start(&c2, 20_000), 1000);
    }

    #[test]
    fn snap_range_rejects_start_not_before_end() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000], 30.0);
        // 起点吸附到 2000（≥1200），终点吸附到 2000（≤2400）→ 空区间
        let err = snap_range(&clip, 1200, 2400).unwrap_err();
        assert!(format!("{err}").contains("区间无效"), "实际错误：{err}");
        // 明确 start > end
        assert!(snap_range(&clip, 5000, 1000).is_err());
        // 合法区间：100→2000，4500→4000
        assert_eq!(snap_range(&clip, 100, 4500).unwrap(), (2000, 4000));
    }

    #[test]
    fn effective_range_extends_end_to_media_tail() {
        // 5 秒素材、关键帧在 0/2/4 s：终点拖到末尾时必须取真实末尾，否则丢最后一个 GOP
        let clip = synthetic_clip(5_000, vec![0, 2000, 4000], 30.0);
        assert_eq!(effective_range(&clip, 100, 5_000).unwrap(), (2000, 5_000));
        // 没拖到末尾就还是按关键帧吸附
        assert_eq!(effective_range(&clip, 100, 4_500).unwrap(), (2000, 4000));
    }

    /// `nearest_keyframes` 的边界：空素材、n=0、n 超过总数、越界鼠标位置、距离相同取左侧。
    #[test]
    fn nearest_keyframes_edges() {
        let clip = synthetic_clip(10_000, vec![0, 1000, 2000, 3000, 4000], 30.0);

        // n = 0 / 没有关键帧 → 空
        assert!(nearest_keyframes(&clip, 1500, 0).is_empty());
        let empty = synthetic_clip(10_000, vec![], 30.0);
        assert!(nearest_keyframes(&empty, 1500, 3).is_empty());

        // n 超过总数 → 全部（升序、不重复）
        assert_eq!(
            nearest_keyframes(&clip, 1500, 99),
            vec![0, 1000, 2000, 3000, 4000]
        );

        // 鼠标在 2100 ms：最近的 3 个是 2000 / 3000 / 1000（返回按时间升序）
        assert_eq!(nearest_keyframes(&clip, 2100, 3), vec![1000, 2000, 3000]);
        // 距离相同（1500 与 1000/2000 都差 500）→ 取不晚于鼠标的那个
        assert_eq!(nearest_keyframes(&clip, 1500, 1), vec![1000]);
        assert_eq!(nearest_keyframes(&clip, 1500, 2), vec![1000, 2000]);

        // 越界：夹到 [0, duration] 后再取，不会少取也不会越界
        assert_eq!(nearest_keyframes(&clip, -9999, 2), vec![0, 1000]);
        assert_eq!(nearest_keyframes(&clip, 99_999, 2), vec![3000, 4000]);

        // 单个关键帧：取几个都只有它
        let one = synthetic_clip(10_000, vec![1234], 30.0);
        assert_eq!(nearest_keyframes(&one, 0, 5), vec![1234]);
    }

    /// `nearest_keyframe`（只取一张，给"鼠标附近那张缩略图"用）的边界与语义：
    /// 必须与 `nearest_keyframes(.., 1)` 完全一致，且 t=0 / t=duration / 空列表都不 panic。
    #[test]
    fn nearest_keyframe_edges() {
        let clip = synthetic_clip(10_000, vec![0, 1000, 2000, 3000, 4000], 30.0);

        // 空关键帧列表 → None（绝不 panic、绝不返回 0 冒充一个关键帧）
        assert_eq!(
            nearest_keyframe(&synthetic_clip(10_000, vec![], 30.0), 1500),
            None
        );
        // 时长为 0 / 负数的退化素材也一样
        assert_eq!(nearest_keyframe(&synthetic_clip(0, vec![], 30.0), 0), None);

        // 与批量版本逐点一致（界面只显示一张，逻辑不许有第二套）
        for t in [-9999i64, 0, 1, 500, 1000, 1500, 4999, 9999, 10_000, 99_999] {
            assert_eq!(
                nearest_keyframe(&clip, t),
                nearest_keyframes(&clip, t, 1).into_iter().next(),
                "t = {t} ms 时单张版本与批量版本必须一致"
            );
        }

        // 边界：t = 0 → 第一个关键帧；t = duration → 最后一个关键帧
        assert_eq!(nearest_keyframe(&clip, 0), Some(0));
        assert_eq!(nearest_keyframe(&clip, 10_000), Some(4000));
        // 超出 [0, duration] 先夹紧再取
        assert_eq!(nearest_keyframe(&clip, -1), Some(0));
        assert_eq!(nearest_keyframe(&clip, 99_999), Some(4000));
        // 距离相同（1500 距 1000/2000 都是 500）→ 取不晚于 t 的那个
        assert_eq!(nearest_keyframe(&clip, 1500), Some(1000));
        assert_eq!(nearest_keyframe(&clip, 2600), Some(3000));
        // 单个关键帧：任何位置都只有它
        assert_eq!(
            nearest_keyframe(&synthetic_clip(10_000, vec![1234], 30.0), 0),
            Some(1234)
        );
    }

    #[test]
    fn open_reads_keyframes_and_duration() {
        let Some(src) = gen_mp4("open-plain.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        assert!(clip.video.is_some(), "应识别出视频流");
        assert!(clip.audio.is_some(), "应识别出音频流");
        let v = clip.video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (320, 240));
        assert!((v.fps - 30.0).abs() < 0.01, "帧率应约 30，实际 {}", v.fps);
        assert!(
            (clip.duration_ms - 4000).abs() <= 60,
            "时长应约 4000 ms，实际 {}",
            clip.duration_ms
        );
        assert!(
            clip.keyframes_ms.len() >= 4,
            "每 1 秒一个关键帧，4 秒应有 4 个，实际 {:?}",
            clip.keyframes_ms
        );
        for (i, k) in clip.keyframes_ms.iter().enumerate().take(4) {
            let want = (i as i64) * 1000;
            assert!(
                (*k - want).abs() <= 20,
                "第 {i} 个关键帧应约 {want} ms，实际 {k}"
            );
        }
    }

    #[test]
    fn open_reads_fragmented_mp4() {
        let Some(src) = gen_mp4("open-frag.mp4", true) else {
            return;
        };
        let clip = open(&src).expect("分片 mp4 也必须能 open");
        assert!(clip.video.is_some());
        assert!(!clip.keyframes_ms.is_empty(), "分片 mp4 也应扫到关键帧");
        assert!(clip.duration_ms > 3500, "时长 {} ms 偏短", clip.duration_ms);
    }

    /// **P34：探测段只读流信息** —— clip 立刻可用，关键帧表留空且标记"未扫"。
    #[test]
    fn probe_returns_clip_without_keyframes_and_marks_unscanned() {
        let Some(src) = gen_mp4("probe.mp4", false) else {
            return;
        };
        let clip = probe(&src).expect("probe 应成功");
        assert!(clip.video.is_some(), "探测就要能拿到视频流");
        assert!(clip.audio.is_some(), "探测就要能拿到音频流");
        assert!(
            clip.duration_ms > 3500,
            "探测就要能拿到时长：{}",
            clip.duration_ms
        );
        assert!(
            clip.keyframes_ms.is_empty(),
            "探测段不扫关键帧，表必须为空（实测 {:?}）",
            clip.keyframes_ms
        );
        assert!(
            !clip.keyframes_scanned,
            "有视频流时 keyframes_scanned 必须为 false（表还没扫）"
        );
    }

    /// **P34：流式打开** —— clip 在扫描**之前**就经 emit 交出，
    /// 之后同一上下文续扫出的关键帧与同步 `open()` 逐项一致。
    #[test]
    fn open_streaming_emits_clip_before_scanning_and_matches_open() {
        let Some(src) = gen_mp4("open-streaming.mp4", false) else {
            return;
        };
        let emitted = std::sync::Arc::new(std::sync::Mutex::new(None::<Clip>));
        let (clip, pass) = {
            let emitted = emitted.clone();
            open_streaming(&src, move |c| {
                *emitted.lock().unwrap_or_else(|e| e.into_inner()) = Some(c.clone());
            })
            .expect("open_streaming 应成功")
        };
        // emit 在扫描之前就被调用过，且交出的就是探测出来的 clip（表还是空的）
        let emitted = emitted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("emit 必须被调用过一次");
        assert!(
            emitted.keyframes_ms.is_empty() && !emitted.keyframes_scanned,
            "emit 出去的 clip 是「探测即交」的形态：表空、未扫"
        );
        assert_eq!(emitted.src, clip.src);
        // 扫描结果与同步 open() 完全一致（同一条物理路径，两条路必须同一份表）
        let full = open(&src).expect("open 应成功");
        assert_eq!(
            pass.keyframes_ms, full.keyframes_ms,
            "流式续扫必须与同步 open 逐项一致"
        );
        assert!(!pass.keyframes_ms.is_empty(), "扫完必须有关键帧");
        assert!(full.keyframes_scanned, "同步 open 的表是最终态");
        // P35：体检报告也随同一次遍历回来，且干净素材不该报任何坏段
        assert!(
            pass.health.is_clean(),
            "正常素材不该有坏段：{:?}",
            pass.health.bad
        );
        assert_eq!(
            pass.health, full.health,
            "流式续扫的体检结论必须与同步 open 一致"
        );
    }

    /// 打开前已取消：连 `emit` 都不该发生，更不该开始扫全片。
    #[test]
    fn open_streaming_cancel_before_emit_returns_none() {
        let Some(src) = gen_mp4("open-cancel-before.mp4", false) else {
            return;
        };
        let cancel = AtomicBool::new(true);
        let emitted = std::sync::Arc::new(AtomicBool::new(false));
        let r = {
            let emitted = emitted.clone();
            open_streaming_cancel(
                &src,
                move |_| emitted.store(true, std::sync::atomic::Ordering::Relaxed),
                Some(&cancel),
            )
            .expect("取消不是 IO 错误")
        };
        assert!(r.is_none(), "取消后不得返回扫描结果");
        assert!(!emitted.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// emit 之后、扫描过程中取消：返回 `None`，而不是把半截表当成完整结果。
    #[test]
    fn open_streaming_cancel_during_scan_discards_partial_table() {
        let Some(src) = gen_mp4("open-cancel-scan.mp4", false) else {
            return;
        };
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let emit_cancel = cancel.clone();
        let r = open_streaming_cancel(
            &src,
            move |_| emit_cancel.store(true, std::sync::atomic::Ordering::Relaxed),
            Some(&cancel),
        )
        .expect("取消不是 IO 错误");
        assert!(r.is_none(), "取消后不能把半截关键帧表交给界面");
    }

    /// **P34：没有视频流时没有"第二段"** —— 表直接是最终态（空），不等扫描。
    #[test]
    fn open_streaming_without_video_marks_table_final() {
        // 只造音轨：`-f lavfi -i sine` 不带视频流
        let Some(ff) = ffmpeg_exe() else {
            println!("[skip] 找不到随包 ffmpeg，跳过纯音频流式打开测试");
            return;
        };
        let out = asset_dir().join("audio-only.mp4");
        let _ = std::fs::remove_file(&out);
        let status = std::process::Command::new(&ff)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1",
                "-c:a",
                "aac",
                &out.to_string_lossy(),
            ])
            .output();
        match status {
            Ok(o) if o.status.success() && out.is_file() => {}
            _ => {
                println!("[skip] 本机随包 ffmpeg 不可用，跳过纯音频流式打开测试");
                return;
            }
        }
        let seen = std::sync::Arc::new(std::sync::Mutex::new(false));
        let (clip, pass) = {
            let seen = seen.clone();
            open_streaming(&out, move |c| {
                if c.video.is_none() {
                    *seen.lock().unwrap_or_else(|e| e.into_inner()) = true;
                }
            })
            .expect("纯音频也必须 open 成功")
        };
        assert!(clip.video.is_none(), "这个素材没有视频流");
        assert!(pass.keyframes_ms.is_empty(), "没有视频流就没有关键帧可扫");
        assert!(
            pass.health.is_clean(),
            "纯音频没有「视频编码错误」这回事，不该报坏段"
        );
        assert!(clip.keyframes_scanned, "没有视频流 ⇒ 表直接是最终态");
        assert!(
            *seen.lock().unwrap_or_else(|e| e.into_inner()),
            "纯音频也要走 emit（界面照样要载入）"
        );
    }

    /// **P34：探测失败要如实报错**（文件不存在 / 打不开都不该 panic）。
    #[test]
    fn open_streaming_reports_probe_failure() {
        let missing = asset_dir().join("no-such-file-p34.mp4");
        let _ = std::fs::remove_file(&missing);
        let e = open_streaming(&missing, |_| {}).expect_err("不存在的文件必须报错");
        let msg = e.to_string();
        assert!(
            msg.contains("文件不存在") || msg.contains("打开"),
            "错误要有信息：{msg}"
        );
    }

    // =======================================================================
    // P35：编码体检（真文件：正常素材必须"干净"，截断素材必须被查出来）
    // =======================================================================

    /// 用随包 ffmpeg 造一个 `3 s / 160×120 / 每秒一个关键帧`、**moov 在前**的素材。
    ///
    /// `-movflags +faststart` 是截断测试的前提：moov 在文件尾部时，砍掉尾巴连
    /// `avformat_open_input` 都过不去（那是"打不开"，不是"数据不全"，测不到本功能）。
    fn p35_clip(name: &str) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(name);
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
                "testsrc2=size=160x120:rate=30",
                "-t",
                "3",
                "-c:v",
                "libopenh264",
                "-b:v",
                "300k",
                "-g",
                "30",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            Some(out)
        } else {
            println!(
                "[skip] 造 P35 素材失败：{}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
    }

    /// **干净素材：快速体检一条坏段都不许报**。
    ///
    /// 这条是"改造前后行为完全一致"的守门人：体检一旦对正常文件误报，
    /// 播放会开始莫名跳段、导出会开始莫名截短 —— 比不查更糟。
    #[test]
    fn p35_open_reports_clean_health_for_a_normal_clip() {
        let Some(src) = p35_clip("p35-clean.mp4") else {
            return;
        };
        let clip = open(&src).expect("正常素材必须能 open");
        assert!(
            clip.health.is_clean(),
            "正常素材被误报成坏文件：{:?}",
            clip.health.bad
        );
        assert!(!clip.health.has_hard(), "干净素材不该有硬坏段");
        assert!(clip.health.packets > 0, "体检必须真的扫过包");
        assert!(
            clip.health.scanned_ms > 0,
            "体检必须报告覆盖时长（否则界面无从显示进度）"
        );
        assert!(!clip.keyframes_ms.is_empty(), "关键帧表照常产出");
        assert_eq!(clip.health.hard_ranges(), Vec::new(), "硬坏段列表必须为空");
    }

    /// **截断素材：必须被查出来，并且归成"硬错误"**。
    ///
    /// 这正是用户实机遇到的形态：录制被强行中断 / 磁盘写满 → 分片 mp4 的尾巴没了。
    /// 改造前这种文件"读得很干净"（`av_read_frame` 正常返回 EOF），
    /// 于是**没有任何一处**发现后半段根本不存在 —— 剪辑吸附失效、导出花屏。
    #[test]
    fn p35_open_flags_a_truncated_file() {
        let Some(src) = p35_clip("p35-full.mp4") else {
            return;
        };
        let bytes = std::fs::read(&src).expect("读素材");
        // 砍掉尾部 40%：moov（faststart 在前）仍然完整，mdat 少了一截
        let keep = (bytes.len() as f64 * 0.6) as usize;
        let trunc = asset_dir().join("p35-truncated.mp4");
        std::fs::write(&trunc, &bytes[..keep]).expect("写截断素材");

        let clip = match open(&trunc) {
            Ok(c) => c,
            Err(e) => {
                // 极少数情况下截断位置刚好破坏了 moov：那是"打不开"，不是本功能的范围
                println!("[skip] 截断素材连打开都失败了（{e}），本环境不适用");
                return;
            }
        };
        assert!(
            clip.health.has_hard(),
            "截断文件必须至少报一段硬坏段（实际 {:?}）",
            clip.health.bad
        );
        let kinds: Vec<_> = clip.health.bad.iter().map(|b| b.kind).collect();
        assert!(
            kinds.contains(&health::BadKind::ShortData)
                || kinds.contains(&health::BadKind::ReadError),
            "截断应归成「数据不完整」或「读取中断」，实际 {kinds:?}"
        );
        // 坏段必须落在文件的后半截（而不是整片都标红）
        let hr = clip.health.hard_ranges();
        assert!(
            hr.iter().all(|(s, _)| *s >= 0) && hr.iter().any(|(s, _)| *s > 0),
            "坏段起点应该出现在后半截，实际 {hr:?}"
        );
    }

    /// **P35：干净素材的导出区间一字不变**（`skipped_bad_ms == 0`）。
    ///
    /// 这是"加剔除不会偷偷改用户导出内容"的保证 —— 干净文件走的必须是原来的路径。
    #[test]
    fn p35_clean_health_leaves_export_ranges_untouched() {
        let clip = synthetic_clip(10_000, vec![0, 1_000, 2_000, 3_000, 4_000], 30.0);
        let (segs, total, skipped) =
            resolve_segments_ex(&clip, &[Segment::new(0, 4_000)]).expect("解析应成功");
        assert_eq!(segs, vec![Segment::new(0, 4_000)], "区间必须原样保留");
        assert_eq!(total, 4_000);
        assert_eq!(skipped, 0, "干净素材不许报「跳过了多少」");
    }

    /// 有视频但没有任何关键帧时不能继续 copy 导出：
    /// `snap_start` 的降级会让产物从非关键帧开头，播放器可能直接花屏。
    #[test]
    fn export_rejects_video_without_any_keyframe() {
        let clip = synthetic_clip(10_000, vec![], 30.0);
        let err = resolve_segments_ex(&clip, &[Segment::new(0, 2_000)])
            .expect_err("无关键帧视频必须拒绝导出");
        assert!(err.to_string().contains("关键帧"), "{err}");
    }

    /// **P35：硬坏段被切掉，且新起点重新吸附到关键帧**（否则产物开头花屏）。
    #[test]
    fn p35_hard_range_is_excised_and_resnapped() {
        let mut clip = synthetic_clip(10_000, vec![0, 1_000, 2_000, 3_000, 4_000], 30.0);
        clip.health.bad.push(health::BadRange::new(
            1_500,
            2_600,
            health::BadKind::ReadError,
            "测试",
        ));
        let (segs, total, skipped) =
            resolve_segments_ex(&clip, &[Segment::new(0, 10_000)]).expect("解析应成功");
        // 前半截停在坏段起点；后半截从坏段尾部往后**重新吸附**到 3000（关键帧）
        assert_eq!(
            segs,
            vec![Segment::new(0, 1_500), Segment::new(3_000, 10_000)],
            "坏段必须被切开，且新起点必须是关键帧"
        );
        assert_eq!(total, 8_500);
        assert!(
            skipped >= 1_100,
            "至少要报出坏段本身的 1100 ms（实际 {skipped}）"
        );
        // 每个子区间起点都必须是关键帧 —— 这是"导出不花屏"的硬条件
        for s in &segs {
            assert!(
                clip.keyframes_ms.contains(&s.start_ms),
                "起点 {} 不是关键帧",
                s.start_ms
            );
        }
    }

    /// **P35：软告警不改变导出区间**（时间戳跳变只是标红，不该切用户的内容）。
    #[test]
    fn p35_soft_ranges_do_not_touch_export() {
        let mut clip = synthetic_clip(10_000, vec![0, 1_000, 2_000, 3_000], 30.0);
        clip.health.bad.push(health::BadRange::new(
            1_500,
            2_600,
            health::BadKind::TimestampJump,
            "测试",
        ));
        let (segs, total, skipped) =
            resolve_segments_ex(&clip, &[Segment::new(0, 10_000)]).expect("解析应成功");
        assert_eq!(segs, vec![Segment::new(0, 10_000)], "软告警不许切内容");
        assert_eq!(total, 10_000);
        assert_eq!(skipped, 0);
    }

    /// **P35：整段都落在坏区里 → 如实报错**，而不是导出一个空文件。
    #[test]
    fn p35_selection_entirely_inside_bad_range_is_an_error() {
        let mut clip = synthetic_clip(10_000, vec![0, 1_000, 2_000], 30.0);
        clip.health.bad.push(health::BadRange::new(
            0,
            10_000,
            health::BadKind::ShortData,
            "整片截断",
        ));
        let e =
            resolve_segments_ex(&clip, &[Segment::new(0, 3_000)]).expect_err("全在坏区里必须报错");
        assert!(
            e.to_string().contains("编码错误"),
            "错误信息要说清楚是坏段导致的：{e}"
        );
    }

    /// **P36：分段列表的顺序被完整尊重**（乱序、重叠、重复都不再报错）。
    ///
    /// 改造前这里有"必须按 start 升序且互不重叠"的硬校验，于是"把第 3 段拖到
    /// 第 1 位"会直接导出失败。改成播放列表语义后：
    /// * 输出与输入**逐项一一对应**（顺序 = 用户排的顺序）；
    /// * 每段的吸附仍然独立生效（起点向右、终点向左）；
    /// * 总时长与顺序无关（都是各段长度之和）。
    #[test]
    fn p36_segment_order_is_preserved_not_sorted() {
        let clip = synthetic_clip(10_000, vec![0, 1_000, 2_000, 3_000, 4_000, 5_000], 30.0);
        // 故意倒着给：先是 3–4 s，再是 1–2 s
        let raw = [Segment::new(3_000, 4_000), Segment::new(1_000, 2_000)];
        let (segs, total, skipped) = resolve_segments_ex(&clip, &raw).expect("乱序列表必须能解析");
        assert_eq!(
            segs,
            vec![Segment::new(3_000, 4_000), Segment::new(1_000, 2_000)],
            "输出顺序必须与输入完全一致，不许被偷偷排序"
        );
        assert_eq!(total, 2_000, "总时长 = 各段之和，与顺序无关");
        assert_eq!(skipped, 0, "干净素材不许有剔除量");
    }

    /// **P36：同一区间出现两次 = 播两遍**（播放列表语义，不是切分语义）。
    #[test]
    fn p36_repeated_segment_is_allowed() {
        let clip = synthetic_clip(10_000, vec![0, 1_000, 2_000], 30.0);
        let raw = [Segment::new(0, 2_000), Segment::new(0, 2_000)];
        let (segs, total, _) = resolve_segments_ex(&clip, &raw).expect("重复段必须允许");
        assert_eq!(segs.len(), 2, "重复段不该被去重");
        assert_eq!(total, 4_000, "同一段放两次就是两倍时长");
    }

    /// **P35：深度检查（全片解码）在正常素材上同样不误报**，且如实标记 `full_decode`。
    #[test]
    fn p35_deep_scan_is_clean_on_a_normal_clip() {
        let Some(src) = p35_clip("p35-deep.mp4") else {
            return;
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut progress_calls = 0usize;
        let rep =
            health::deep_scan(&src, &cancel, |_| progress_calls += 1).expect("深度检查应成功");
        assert!(rep.full_decode, "深度检查必须标记 full_decode");
        assert!(
            rep.is_clean(),
            "正常素材的深度检查不该报坏段：{:?}",
            rep.bad
        );
        assert!(rep.packets > 0, "必须真的解过包");
        assert!(
            progress_calls > 0,
            "进度回调必须至少被调用一次（否则界面进度条永远不动）"
        );
    }

    /// **P35：深度检查可以被取消**（换文件时不能把它留在后台空转）。
    #[test]
    fn p35_deep_scan_honours_cancel() {
        let Some(src) = p35_clip("p35-deep-cancel.mp4") else {
            return;
        };
        // 一开始就置取消：线程必须在**第一个包之前**就退出来
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let r = health::deep_scan(&src, &cancel, |_| {});
        assert!(
            r.is_err(),
            "已取消时必须返回 Err（绝不能被当成「检查通过」）"
        );
    }

    /// **P34 量化对照（手动跑，默认忽略）**：长片上 `probe` 与 `open`（含全文件关键帧扫描）的耗时差。
    ///
    /// 跑法：`cargo test --lib p34_probe_vs_open_on_a_long_clip -- --ignored --nocapture`
    ///
    /// 为什么要留这条：P34 的动机就是"扫描耗时 ∝ 文件大小"，这条把那个比例实测出来
    /// （打印 MB/s），以后改扫描策略时有对照基线。断言只写"探测必须明显快于全扫"，
    /// 不写具体毫秒数（机器差异太大，写死就是脆弱测试）。
    #[test]
    #[ignore = "要现场生成 2 分钟 1080p 素材（几十秒），手动跑"]
    fn p34_probe_vs_open_on_a_long_clip() {
        let Some(ff) = ffmpeg_exe() else {
            println!("[skip] 找不到随包 ffmpeg，跳过 P34 量化对照");
            return;
        };
        let out = asset_dir().join("p34-long-1080p.mp4");
        if !out.is_file() {
            let status = std::process::Command::new(&ff)
                .args([
                    "-y",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=1920x1080:rate=30",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000",
                    "-t",
                    "120",
                    "-c:v",
                    "libopenh264",
                    "-b:v",
                    "8M",
                    "-g",
                    "60",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "128k",
                    &out.to_string_lossy(),
                ])
                .output();
            match status {
                Ok(o) if o.status.success() && out.is_file() => {}
                _ => {
                    println!("[skip] 随包 ffmpeg 不可用，跳过 P34 量化对照");
                    return;
                }
            }
        }
        let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let mb = bytes as f64 / (1024.0 * 1024.0);
        // 预热一次（把文件读进页缓存/把 DLL 拉进来），避免拿"冷启动"当稳态
        let _ = probe(&out);
        let t_probe = std::time::Instant::now();
        let clip = probe(&out).expect("probe 应成功");
        let probe_ms = t_probe.elapsed().as_secs_f64() * 1000.0;
        let t_open = std::time::Instant::now();
        let full = open(&out).expect("open 应成功");
        let open_ms = t_open.elapsed().as_secs_f64() * 1000.0;
        let scan_ms = (open_ms - probe_ms).max(0.0);
        let throughput = if scan_ms > 0.0 {
            mb / (scan_ms / 1000.0)
        } else {
            f64::INFINITY
        };
        println!(
            "[P34] file={mb:.1} MB probe={probe_ms:.1} ms open={open_ms:.1} ms \
             scan≈{scan_ms:.1} ms ({throughput:.0} MB/s) keyframes={}",
            full.keyframes_ms.len()
        );
        assert!(!clip.keyframes_scanned, "probe 不该带表");
        // 只钉"全扫更贵"这一条（具体毫秒数看上面打印；写死数字就是脆弱测试）
        assert!(
            open_ms > probe_ms,
            "全扫（{open_ms:.1} ms）必须比探测（{probe_ms:.1} ms）贵"
        );
    }

    #[test]
    fn thumbnail_scales_down_and_returns_rgba() {
        let Some(src) = gen_mp4("thumb.mp4", false) else {
            return;
        };
        let (w, h, rgba) = thumbnail(&src, 1500, 160, 120).expect("thumbnail 应成功");
        assert!(w <= 160 && h <= 120, "不得超过 max：实际 {w}x{h}");
        assert_eq!(
            (w, h),
            (160, 120),
            "320x240 等比缩到不超过 160x120 应为 160x120"
        );
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // testsrc2 是彩色测试图，转换后不可能全是同一个值
        let first = &rgba[..4];
        assert!(rgba.chunks_exact(4).any(|p| p != first), "缩略图不应是纯色");
        // 再取一帧（时刻不同）也应该成功
        let (w2, h2, rgba2) = thumbnail(&src, 300, 64, 64).expect("thumbnail 应成功");
        assert!(w2 <= 64 && h2 <= 64);
        assert_eq!(rgba2.len(), (w2 * h2 * 4) as usize);
    }

    #[test]
    fn export_copy_keeps_resolution_and_range() {
        let Some(src) = gen_mp4("export-src.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        let dst = asset_dir().join("export-out.mp4");
        let _ = std::fs::remove_file(&dst);
        // 关键帧在 0/1/2/3 s：1.2 s → 2 s，3.4 s → 3 s
        let rep = export_copy(&clip, 1200, 3400, &dst).expect("copy 导出应成功");
        assert!(rep.same_resolution, "分辨率必须与源一致");
        assert!(rep.is_bitstream_copy(), "必须是位流拷贝");
        assert_eq!((rep.dst_width, rep.dst_height), (320, 240));
        assert_eq!(
            (rep.start_ms, rep.end_ms),
            (2000, 3000),
            "应吸附到 2 s..3 s"
        );
        assert_eq!(rep.duration_ms(), 1000);
        assert!(rep.bytes > 0 && rep.packets > 0, "应有真实产物与 packet");
        assert!(
            (rep.dst_duration_ms - 1000).abs() <= 150,
            "产物时长应约 1000 ms，实际 {}",
            rep.dst_duration_ms
        );
        assert!(dst.is_file());
    }

    /// 取消导出必须保留用户已有的同名文件，且不留下 `.part`。
    #[test]
    fn cancelled_export_copy_preserves_existing_destination() {
        let Some(src) = gen_mp4("export-cancel-src.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        let dst = asset_dir().join("export-cancel-existing.mp4");
        let stable = b"existing export must survive cancellation";
        std::fs::write(&dst, stable).unwrap();
        let cancel = AtomicBool::new(true);

        let err =
            export_copy_cancel(&clip, 0, 2_000, &dst, &cancel).expect_err("预取消的导出必须失败");
        assert!(err.to_string().contains("取消"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), stable);
        let leftovers: Vec<_> = std::fs::read_dir(asset_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".export-cancel-existing.mp4.") && n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "取消后残留临时文件：{leftovers:?}");
    }

    /// 产物写完后校验失败也不能替换正式目标：用户已有的同名文件必须原样保留。
    #[test]
    fn failed_post_export_validation_preserves_existing_destination() {
        let Some(src) = gen_mp4("export-verify-fail-src.mp4", false) else {
            return;
        };
        let mut clip = open(&src).expect("open 应成功");
        // 人为制造"源声称 123x240、产物实际 320x240"的校验失败。
        clip.video.as_mut().expect("应有视频流").width = 123;
        let dst = asset_dir().join("export-verify-fail-existing.mp4");
        let stable = b"existing export must survive failed validation";
        std::fs::write(&dst, stable).unwrap();

        let err = export_copy(&clip, 0, 2_000, &dst).expect_err("分辨率校验必须失败");
        assert!(err.to_string().contains("分辨率"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), stable);
        let leftovers: Vec<_> = std::fs::read_dir(asset_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".export-verify-fail-existing.mp4.") && n.ends_with(".part"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "校验失败后残留临时文件：{leftovers:?}"
        );
    }

    #[test]
    fn export_copy_to_tail_uses_remux_range() {
        let Some(src) = gen_mp4("export-tail-src.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        let dst = asset_dir().join("export-tail.mp4");
        let _ = std::fs::remove_file(&dst);
        // 终点取到文件尾 → 走 mux::remux_range（"起点 → EOF"语义正好够用）
        let rep = export_copy(&clip, 1000, clip.duration_ms, &dst).expect("copy 导出应成功");
        assert_eq!(rep.start_ms, 1000);
        assert_eq!(rep.end_ms, clip.duration_ms, "终点应取到媒体真实末尾");
        assert!(rep.same_resolution);
        assert_eq!((rep.dst_width, rep.dst_height), (320, 240));
        let want = rep.duration_ms();
        assert!(
            (rep.dst_duration_ms - want).abs() <= 200,
            "时长应约 {want} ms，实际 {}",
            rep.dst_duration_ms
        );
    }

    #[test]
    fn export_copy_errors_on_empty_selection() {
        let Some(src) = gen_mp4("export-err.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        let dst = asset_dir().join("export-err-out.mp4");
        let _ = std::fs::remove_file(&dst);
        // 1.2 s→2.4 s 会吸附成 2000..2000（空区间），必须报错而不是产出空文件
        assert!(export_copy(&clip, 1200, 2400, &dst).is_err());
        assert!(export_copy(&clip, 3000, 1000, &dst).is_err());
        assert!(!dst.exists(), "报错时不应留下产物");
    }

    #[test]
    fn open_rejects_missing_file() {
        let missing = asset_dir().join("definitely-not-here.mp4");
        let _ = std::fs::remove_file(&missing);
        assert!(open(&missing).is_err());
    }

    // -----------------------------------------------------------------------
    // 多段（分片）模型
    // -----------------------------------------------------------------------
    //
    // 素材口径（`gen_mp4`）：4 秒、30 fps、**关键帧每 1 秒一个**（0/1000/2000/3000/4000），
    // 容器时长约 4000 ms。吸附口径与 [`snap_start`]/[`snap_end`] 相同：
    // 起点 → **后一个**关键帧（≥ t−容差），终点 → **前一个**关键帧（≤ t+容差），
    // 终点贴片尾（t+容差 ≥ 时长）时取**真实片尾**。

    /// 解析：逐段吸附关键帧、总时长 = Σ 吸附后段长；相邻段首尾相接合法。
    #[test]
    fn resolve_segments_snaps_and_sums() {
        let Some(src) = gen_mp4("resolve-segs.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        // 段① 500..2100 → (1000, 2000)；段② 2000..3100 → (2000, 3000)（首尾相接合法）；
        // 段③ 3000..3990 → 终点贴片尾 → (3000, 4000)
        let segs = [
            Segment::new(500, 2100),
            Segment::new(2000, 3100),
            Segment::new(3000, 3990),
        ];
        let (ranges, total) = resolve_segments(&clip, &segs).expect("三段合法区间应通过");
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].start_ms, 1000, "段①起点吸附到后一个关键帧");
        assert_eq!(ranges[0].end_ms, 2000, "段①终点吸附到前一个关键帧");
        assert_eq!(ranges[1].start_ms, 2000, "段②与段①首尾相接（合法）");
        assert_eq!(ranges[2].end_ms, 4000, "段③终点贴片尾 → 取真实片尾");
        assert_eq!(total, 3000, "总时长 = 1000 + 1000 + 1000");
    }

    /// 空列表 / 吸附后为空段 → 报错。
    ///
    /// **P36 起不再拒绝"重叠"和"未排序"**：列表是播放列表，顺序由用户拖拽决定，
    /// 同一区间出现两次就是播两遍。这两条旧断言已经反过来了 —— 见
    /// `p36_segment_order_is_preserved_not_sorted` 与 `p36_repeated_segment_is_allowed`。
    #[test]
    fn resolve_segments_rejects_bad_input() {
        let Some(src) = gen_mp4("resolve-bad.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        // 空列表
        assert!(resolve_segments(&clip, &[]).is_err(), "空分段列表必须报错");
        // 空段：600..1700 → (1000, 1000)（终点也吸附回 1000）
        let empty_seg = [Segment::new(600, 1700)];
        assert!(
            resolve_segments(&clip, &empty_seg).is_err(),
            "吸附后起点 == 终点的段必须报错"
        );
        // 重叠：以前报错，现在**必须通过**（播放列表语义）
        let overlap = [Segment::new(500, 3100), Segment::new(1500, 3900)];
        assert!(
            resolve_segments(&clip, &overlap).is_ok(),
            "重叠的分段现在是合法的（同一段可以加两次）"
        );
        // 未排序：同样必须通过，且顺序原样保留
        // （用两段都合法的区间：3 秒素材上 (0,1000) 与 (1000,2000) 各自都吸得动）
        let unsorted = [Segment::new(1000, 2000), Segment::new(0, 1000)];
        let (r, _) = resolve_segments(&clip, &unsorted).expect("未排序的分段现在合法");
        assert!(
            r[0].start_ms > r[1].start_ms,
            "输出顺序必须与输入一致，不许被偷偷排序：{:?}",
            r
        );
    }

    /// 真素材上的多段合并导出：两段拼接，产物时长 ≈ Σ 段长、分辨率不变。
    #[test]
    fn export_ranges_merges_two_segments() {
        let Some(src) = gen_mp4("export-ranges.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        let dst = asset_dir().join("export-ranges-out.mp4");
        let _ = std::fs::remove_file(&dst);
        // 段① 500..2100 → (1000, 2000)；段② 2500..3990 → (3000, 4000)。合计 2000 ms。
        let segs = [Segment::new(500, 2100), Segment::new(2500, 3990)];
        let rep = export_ranges(&clip, &segs, &dst).expect("两段合并导出应成功");
        assert!(dst.is_file(), "产物必须真的落盘");
        assert!(rep.bytes > 0);
        assert_eq!(rep.segments, 2, "报告应记录分段数");
        assert_eq!(rep.total_duration_ms, 2000, "总时长 = 两段各 1000 ms");
        assert!(rep.same_resolution, "copy 导出分辨率必须不变");
        assert_eq!((rep.dst_width, rep.dst_height), (320, 240));
        assert!(
            (rep.dst_duration_ms - rep.total_duration_ms).abs() <= 2 * 200,
            "产物时长应约 {} ms，实际 {}",
            rep.total_duration_ms,
            rep.dst_duration_ms
        );
    }

    /// 单段调用 `export_ranges`：行为与 `export_copy` 完全一致
    /// （segments == 1、总时长 == 选区长度、吸附口径相同）。
    #[test]
    fn export_ranges_single_segment_is_export_copy() {
        let Some(src) = gen_mp4("export-ranges-1.mp4", false) else {
            return;
        };
        let clip = open(&src).expect("open 应成功");
        let dst = asset_dir().join("export-ranges-1-out.mp4");
        let _ = std::fs::remove_file(&dst);
        // 1000..2500 → 吸附 (1000, 2000)
        let rep = export_ranges(&clip, &[Segment::new(1000, 2500)], &dst).expect("单段导出应成功");
        assert_eq!(rep.segments, 1);
        assert_eq!(rep.total_duration_ms, rep.duration_ms());
        let (s, e) = effective_range(&clip, 1000, 2500).expect("有效区间");
        assert_eq!(
            (rep.start_ms, rep.end_ms),
            (s, e),
            "单段口径应与 export_copy 相同"
        );
        assert_eq!((s, e), (1000, 2000), "1000..2500 吸附成 (1000, 2000)");
    }
}
