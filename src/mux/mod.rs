//! FFmpeg 封装层（薄封装，直接走版本匹配的 sys 绑定）。
//!
//! 为什么不用高层 `ffmpeg-next` 的 mux API：我们的视频由**原生 openh264 模块**编码，
//! 需要自己填 `AVCodecParameters`（含 avcC extradata）并写 packet，高层 API 覆盖不到；
//! 而这套绑定是 bindgen 用**本机 FFmpeg 头文件**生成的，布局天然与随包 DLL 一致。
//!
//! 提供三条能力：
//! 1. [`FragmentedMp4Writer`] —— 录制中写**分片 MP4**（`frag_keyframe+empty_moov`）：
//!    边录边可播，崩溃后文件仍能用。
//! 2. [`remux_to_plain`] / [`remux_range`] —— 停止后/剪辑时把（分片或普通）MP4 **重封装**
//!    成普通 MP4（`+faststart`），**不重新编码**；`remux_range` 支持单区间裁剪。
//! 3. [`remux_ranges`] —— **多段拼接**：把若干 `[start, end]` 区间依次拷进同一个输出，
//!    段间只做 seek + 时间戳平移（首尾相接、dts 严格单调），同样不重新编码。

use crate::error::AppError;
use ffmpeg_next::ffi as ff;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

/// `FFERRTAG('E','O','F',' ')`：FFmpeg 用它表示读流结束（宏，bindgen 不导出）
const AVERROR_EOF: i32 = -541_478_725;

/// 终点判定给音频留的余量（毫秒）。AAC 一帧 1024 样本，48 kHz 下约 21 ms；
/// 取 40 ms（约 25 fps 一帧）既能包住常见音频帧长，又不会明显超出用户选区。
const AUDIO_TAIL_MARGIN_MS: i64 = 40;

/// **整段导出（停止录制时的 remux）允许的时长偏差（毫秒）**。
///
/// 产物与流的时长只要差得比它多，就判为"导出不完整/被截断" → 返回错误，
/// 由调用方走"保留 Cache 流文件 + 明确告警"的路径，**绝不静默交付短文件**。
///
/// 为什么是 300 ms：同一次 remux 是逐包 copy，产物时长与源时长实测**完全相等**
/// （`recorder_smoke` 的 `REC_STREAM_MS`/`REC_EXPORT_MS` 逐轮一致）；
/// 300 ms ≈ 9 帧 @30fps，足够容纳容器时长取整，又远小于"丢一个 GOP"的量级。
pub const EXPORT_DURATION_TOLERANCE_MS: i64 = 300;

/// 读包失败时的**重试次数**（不是"总尝试次数"：总尝试 = 1 + 该值）。
/// 磁盘抖动/IO 竞争导致的读失败是暂时的，重试能救回来；一直失败就必须报错。
const REMUX_READ_RETRIES: usize = 3;

/// 每次重试前的退避（毫秒），长度可小于重试次数（不足时按最后一项）。
const REMUX_RETRY_BACKOFF_MS: [u64; 3] = [20, 100, 250];

/// 退避表用尽后的兜底值 = 上表的最后一项。
///
/// P52：原先在两处调用点写的是 `*REMUX_RETRY_BACKOFF_MS.last().unwrap()` ——
/// `last()` 返回 `Option`，`unwrap()` 是**运行期**的 panic 面，而"这个数组非空"
/// 其实由 `[u64; 3]` 的类型本身保证。改成 `const` 表达式后由**编译期**求值：
/// 既去掉两处 panic 面，也让"兜底值来自同一张表"这件事显式可见
/// （改表时不用再记得同步改兜底值）。
const REMUX_RETRY_MAX_BACKOFF_MS: u64 = REMUX_RETRY_BACKOFF_MS[REMUX_RETRY_BACKOFF_MS.len() - 1];

/// 把 FFmpeg 的负错误码翻成一句人话（`av_strerror`）。
///
/// **全仓唯一一份**（P49）：`edit` 侧原先有两份一模一样的拷贝（`edit/mod.rs` 与
/// `edit/waveform.rs`），三份内容逐字相同。`mux` 是最底层的 FFmpeg 封装模块
/// （`edit` 已经 `use crate::mux`），所以放这里、由上层引用，依赖方向不用改。
pub(crate) fn av_err(code: i32) -> String {
    // SAFETY: av_strerror 只写我们提供的栈缓冲区，长度由我们给出，指针与长度都有效。
    unsafe {
        let mut buf = [0i8; 256];
        if ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) == 0 {
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
            format!("FFmpeg 错误码 {code}")
        }
    }
}

fn cpath(p: &Path) -> Result<CString, AppError> {
    CString::new(p.to_string_lossy().as_bytes())
        .map_err(|_| AppError::Mp4(format!("路径含 NUL 字节: {}", p.display())))
}

/// 视频流参数
#[derive(Debug, Clone)]
pub struct VideoParams {
    pub width: i32,
    pub height: i32,
    pub fps: f64,
    pub bitrate: i64,
    /// avcC（AVCDecoderConfigurationRecord）
    pub extradata: Vec<u8>,
}

/// 音频流参数（AAC-LC）
#[derive(Debug, Clone)]
pub struct AudioParams {
    pub sample_rate: i32,
    pub channels: i32,
    pub bitrate: i64,
    /// AudioSpecificConfig（`esds` 里要用）
    pub extradata: Vec<u8>,
}

/// 录制中的分片 MP4 写入器
pub struct FragmentedMp4Writer {
    octx: *mut ff::AVFormatContext,
    /// 视频流（"仅声音"模式下为 `None`）
    vstream: Option<*mut ff::AVStream>,
    astream: Option<*mut ff::AVStream>,
    path: PathBuf,
    finished: bool,
    bytes_hint: u64,
}

// SAFETY: AVFormatContext 只在写入线程内使用；这里不做跨线程共享，仅让类型能放进结构体
unsafe impl Send for FragmentedMp4Writer {}

impl FragmentedMp4Writer {
    /// 创建分片 MP4 写入器。`video` / `audio` 至少要给一个：
    /// * 画面+声音 → 两个都传
    /// * 仅画面   → 只传 `video`
    /// * 仅声音   → 只传 `audio`
    pub fn create(
        path: &Path,
        video: Option<&VideoParams>,
        audio: Option<&AudioParams>,
    ) -> Result<Self, AppError> {
        if video.is_none() && audio.is_none() {
            return Err(AppError::Mp4("至少要有一个视频或音频流".into()));
        }
        unsafe {
            let c_path = cpath(path)?;
            let fmt = c"mp4";
            let mut octx: *mut ff::AVFormatContext = ptr::null_mut();
            let ret = ff::avformat_alloc_output_context2(
                &mut octx,
                ptr::null_mut(),
                fmt.as_ptr(),
                c_path.as_ptr(),
            );
            if ret < 0 || octx.is_null() {
                return Err(AppError::Mp4(format!("创建 mp4 输出上下文失败: {}", av_err(ret))));
            }

            // 流模式：关键帧处分片 + 先写空 moov → 录制中即可播放、崩溃可播
            let key = c"movflags";
            let val = c"frag_keyframe+empty_moov+default_base_moof";
            let ret = ff::av_opt_set((*octx).priv_data, key.as_ptr(), val.as_ptr(), 0);
            if ret < 0 {
                ff::avformat_free_context(octx);
                return Err(AppError::Mp4(format!("设置 movflags 失败: {}", av_err(ret))));
            }

            // 视频流（可选）
            let mut vstream = None;
            if let Some(video) = video {
                let s = ff::avformat_new_stream(octx, ptr::null());
                if s.is_null() {
                    ff::avformat_free_context(octx);
                    return Err(AppError::Mp4("创建视频流失败".into()));
                }
                let par = (*s).codecpar;
                (*par).codec_type = ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
                (*par).codec_id = ff::AVCodecID::AV_CODEC_ID_H264;
                (*par).codec_tag = 0;
                (*par).width = video.width;
                (*par).height = video.height;
                (*par).format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
                (*par).bit_rate = video.bitrate;
                set_extradata(par, &video.extradata)?;
                (*s).time_base = ff::AVRational { num: 1, den: 1000 };
                (*s).avg_frame_rate = ff::AVRational {
                    num: (video.fps * 1000.0) as i32,
                    den: 1000,
                };
                vstream = Some(s);
            }
            let mut astream = None;
            if let Some(a) = audio {
                let s = ff::avformat_new_stream(octx, ptr::null());
                if s.is_null() {
                    ff::avformat_free_context(octx);
                    return Err(AppError::Mp4("创建音频流失败".into()));
                }
                let apar = (*s).codecpar;
                (*apar).codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
                (*apar).codec_id = ff::AVCodecID::AV_CODEC_ID_AAC;
                (*apar).codec_tag = 0;
                (*apar).sample_rate = a.sample_rate;
                // FFmpeg 8/9：声道数在 ch_layout 里
                ff::av_channel_layout_default(&mut (*apar).ch_layout, a.channels);
                (*apar).format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
                (*apar).bit_rate = a.bitrate;
                set_extradata(apar, &a.extradata)?;
                (*s).time_base = ff::AVRational { num: 1, den: 1000 };
                astream = Some(s);
            }

            // 打开输出并写头部（分片模式下 empty_moov 会立刻落盘）
            let mut pb: *mut ff::AVIOContext = ptr::null_mut();
            let ret = ff::avio_open(&mut pb, c_path.as_ptr(), ff::AVIO_FLAG_WRITE);
            if ret < 0 {
                ff::avformat_free_context(octx);
                return Err(AppError::Mp4(format!("打开输出文件失败: {}", av_err(ret))));
            }
            (*octx).pb = pb;

            let ret = ff::avformat_write_header(octx, ptr::null_mut());
            if ret < 0 {
                ff::avio_closep(&mut (*octx).pb);
                ff::avformat_free_context(octx);
                return Err(AppError::Mp4(format!("写 MP4 头部失败: {}", av_err(ret))));
            }
            ff::avio_flush(pb);

            Ok(Self {
                octx,
                vstream,
                astream,
                path: path.to_path_buf(),
                finished: false,
                bytes_hint: 0,
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 写一个视频访问单元（**AVCC**，即 4 字节大端长度前缀的 NAL）
    pub fn write_video(&mut self, data: &[u8], pts_ms: i64, keyframe: bool) -> Result<(), AppError> {
        let s = self
            .vstream
            .ok_or_else(|| AppError::Mp4("未配置视频流（仅声音模式）".into()))?;
        self.write_packet(s, data, pts_ms, keyframe)
    }

    /// 写一个 AAC 裸帧（不带 ADTS 头）
    pub fn write_audio(&mut self, data: &[u8], pts_ms: i64) -> Result<(), AppError> {
        let s = self
            .astream
            .ok_or_else(|| AppError::Mp4("未配置音频流".into()))?;
        self.write_packet(s, data, pts_ms, true)
    }

    fn write_packet(
        &mut self,
        stream: *mut ff::AVStream,
        data: &[u8],
        pts_ms: i64,
        keyframe: bool,
    ) -> Result<(), AppError> {
        unsafe {
            let mut pkt: ff::AVPacket = std::mem::zeroed();
            let ret = ff::av_new_packet(&mut pkt, data.len() as i32);
            if ret < 0 {
                return Err(AppError::Mp4(format!("分配 packet 失败: {}", av_err(ret))));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), pkt.data, data.len());
            pkt.stream_index = (*stream).index;

            // 关键：mov/mp4 复用器会在 avformat_write_header 里**改写流的 time_base**，
            // 所以必须按"写头之后的实际 time_base"把毫秒时间戳换算过去，
            // 否则时间轴会被压缩（实测漏掉这步时 6 秒的录像只报 406 ms）。
            let src_tb = ff::AVRational { num: 1, den: 1000 };
            let dst_tb = (*stream).time_base;
            let ts = if dst_tb.num > 0 && dst_tb.den > 0 {
                ff::av_rescale_q(pts_ms, src_tb, dst_tb)
            } else {
                pts_ms
            };
            pkt.pts = ts;
            pkt.dts = ts;
            pkt.pos = -1;
            if keyframe {
                pkt.flags |= ff::AV_PKT_FLAG_KEY;
            }
            self.bytes_hint += data.len() as u64;
            // av_interleaved_write_frame 接管 packet 所有权（内部 unref）
            let ret = ff::av_interleaved_write_frame(self.octx, &mut pkt);
            if ret < 0 {
                return Err(AppError::Mp4(format!("写入 packet 失败: {}", av_err(ret))));
            }
            Ok(())
        }
    }

    /// 把已缓冲的数据刷到磁盘。录制中周期性调用可让文件"随时可播"。
    pub fn flush(&mut self) -> Result<(), AppError> {
        unsafe {
            if !(*self.octx).pb.is_null() {
                ff::avio_flush((*self.octx).pb);
                // avio_flush 返回 void：底层写失败会被记录在 AVIOContext.error 上。
                // 不查它，磁盘满/网络盘断开就会一直“写成功”，直到 trailer 才发现。
                let err = (*(*self.octx).pb).error;
                if err < 0 {
                    return Err(AppError::Mp4(format!("刷盘失败: {}", av_err(err))));
                }
            }
        }
        Ok(())
    }

    /// 写 trailer 并释放。重复调用无害。
    pub fn finish(&mut self) -> Result<(), AppError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        unsafe {
            if self.octx.is_null() {
                return Ok(());
            }
            let ret = ff::av_write_trailer(self.octx);
            if !(*self.octx).pb.is_null() {
                ff::avio_closep(&mut (*self.octx).pb);
            }
            ff::avformat_free_context(self.octx);
            self.octx = ptr::null_mut();
            if ret < 0 {
                return Err(AppError::Mp4(format!("写 MP4 尾部失败: {}", av_err(ret))));
            }
        }
        Ok(())
    }
}

impl Drop for FragmentedMp4Writer {
    fn drop(&mut self) {
        // 尽力收尾：即使调用方忘记 finish，也不要泄漏 AVFormatContext
        let _ = self.finish();
    }
}

unsafe fn set_extradata(par: *mut ff::AVCodecParameters, data: &[u8]) -> Result<(), AppError> {
    if data.is_empty() {
        return Ok(());
    }
    // extradata 必须由 av_malloc 分配（FFmpeg 会用 av_freep 释放），并留 AV_INPUT_BUFFER_PADDING_SIZE 余量
    const PADDING: usize = 64;
    let buf = ff::av_malloc(data.len() + PADDING) as *mut u8;
    if buf.is_null() {
        return Err(AppError::Mp4("av_malloc extradata 失败".into()));
    }
    ptr::write_bytes(buf, 0, data.len() + PADDING);
    ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
    (*par).extradata = buf;
    (*par).extradata_size = data.len() as i32;
    Ok(())
}

/// 媒体流摘要（给剪辑窗口/校验用）
#[derive(Debug, Clone)]
pub struct StreamBrief {
    pub index: i32,
    pub kind: &'static str,
    pub codec: String,
    pub width: i32,
    pub height: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub time_base: (i32, i32),
    pub duration_ms: i64,
}

#[derive(Debug, Clone)]
pub struct MediaBrief {
    pub format: String,
    pub duration_ms: i64,
    pub streams: Vec<StreamBrief>,
}

/// 打开文件读取流信息（同时用于验证产物）
pub fn probe(path: &Path) -> Result<MediaBrief, AppError> {
    unsafe {
        let c_path = cpath(path)?;
        let mut ictx: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_open_input(&mut ictx, c_path.as_ptr(), ptr::null(), ptr::null_mut());
        if ret < 0 || ictx.is_null() {
            return Err(AppError::Mp4(format!("打开 {} 失败: {}", path.display(), av_err(ret))));
        }
        let ret = ff::avformat_find_stream_info(ictx, ptr::null_mut());
        if ret < 0 {
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("读取流信息失败: {}", av_err(ret))));
        }

        let mut streams = Vec::new();
        for i in 0..(*ictx).nb_streams as usize {
            let st = *(*ictx).streams.add(i);
            let par = (*st).codecpar;
            let kind = match (*par).codec_type {
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO => "video",
                ff::AVMediaType::AVMEDIA_TYPE_AUDIO => "audio",
                _ => "other",
            };
            let codec = {
                let c = ff::avcodec_get_name((*par).codec_id);
                if c.is_null() {
                    "?".into()
                } else {
                    CStr::from_ptr(c).to_string_lossy().into_owned()
                }
            };
            let tb = (*st).time_base;
            let dur = (*st).duration;
            let duration_ms = if dur > 0 && tb.den > 0 {
                dur * tb.num as i64 * 1000 / tb.den as i64
            } else {
                0
            };
            streams.push(StreamBrief {
                index: (*st).index,
                kind,
                codec,
                width: (*par).width,
                height: (*par).height,
                sample_rate: (*par).sample_rate,
                channels: (*par).ch_layout.nb_channels,
                time_base: (tb.num, tb.den),
                duration_ms,
            });
        }

        let fmt_name = {
            let f = (*ictx).iformat;
            if f.is_null() {
                "?".into()
            } else {
                CStr::from_ptr((*f).name).to_string_lossy().into_owned()
            }
        };
        let dur = (*ictx).duration;
        let duration_ms = if dur > 0 {
            dur / (ff::AV_TIME_BASE as i64 / 1000)
        } else {
            streams.iter().map(|s| s.duration_ms).max().unwrap_or(0)
        };
        ff::avformat_close_input(&mut ictx);

        Ok(MediaBrief {
            format: fmt_name,
            duration_ms,
            streams,
        })
    }
}

/// 把（分片或普通）MP4 重封装成普通 MP4（`+faststart`），**不重新编码**。
/// 返回写出的字节数。等价于 [`remux_range`] 传 `None`：整段拷到 EOF。
///
/// 只负责"忠实拷贝"：读包失败会重试，重试仍失败**返回错误**，
/// 写入失败也**返回错误**（历史行为是只 warn 然后继续 —— 那正是"静默截断"的来源）。
/// 需要"产物是否与源等长"的自校验（停止录制时的导出路径）用
/// [`remux_to_plain_verified`]。
pub fn remux_to_plain(src: &Path, dst: &Path) -> Result<u64, AppError> {
    remux_core(src, dst, None).map(|(bytes, _)| bytes)
}

/// 重封装拷贝的**客观摘要**：导出自校验与诊断（`recorder_smoke` 的 `REC_EXPORT_*`）都用它。
///
/// 每个字段都是"数出来的"而不是推断的：包数来自拷贝循环本身，
/// 时长来自对源/产物各做一次 [`probe`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemuxReport {
    /// 从源文件成功读出的 packet 数
    pub packets_read: u64,
    /// 真正写进产物的 packet 数
    pub packets_written: u64,
    pub video_read: u64,
    pub video_written: u64,
    pub audio_read: u64,
    pub audio_written: u64,
    /// 写包失败次数（`> 0` 表示有包没进产物）
    pub write_errors: u64,
    /// 读包失败次数（含重试后成功的那几次）
    pub read_errors: u64,
    /// 实际发生的重试次数
    pub read_retries: u64,
    /// 拷贝循环是否**非正常结束**（既不是 EOF，也不是按区间终点停下）
    pub exited_early: bool,
    /// 循环结束原因：`eof` / `range-end` / `read-error`
    pub exit_reason: &'static str,
    /// 写出的第一个/最后一个 packet 的时间戳（毫秒；一个都没写出时为 -1）
    pub first_pts_ms: i64,
    pub last_pts_ms: i64,
    /// 源 / 产物的容器时长（毫秒）；取不到时为 0
    pub src_duration_ms: i64,
    pub dst_duration_ms: i64,
    pub bytes: u64,
}

impl RemuxReport {
    /// 产物与源的时长差（绝对值，毫秒）
    pub fn duration_delta_ms(&self) -> i64 {
        (self.dst_duration_ms - self.src_duration_ms).abs()
    }
}

/// **整段拷贝的完整性判定**（纯函数，便于把各种"截断形态"直接写成单测）。
///
/// 判为失败的情形（任何一种都意味着"产物与源不一致"，必须报错而不是交付）：
/// * 拷贝循环**非正常结束**（`exited_early`，例如读错误重试后仍失败）；
/// * 出现过读错误或写错误；
/// * 视频包写少了（`video_written < video_read`）；
/// * 源时长可得，且产物时长与它相差超过 [`EXPORT_DURATION_TOLERANCE_MS`]；
/// * 产物时长取不到（`dst_duration_ms <= 0`，说明产物根本解析不出时间轴）。
///
/// 源时长取不到（`src_duration_ms <= 0`）**不算通过**：此时只能靠包数判定，
/// 所以额外要求"读了多少就写了多少"（`packets_written == packets_read`）。
pub fn verify_full_copy(report: &RemuxReport) -> Result<(), AppError> {
    if report.exited_early {
        return Err(AppError::Mp4(format!(
            "重封装提前结束（原因 {}）：读到 {} 个包、写出 {} 个包，产物不完整",
            report.exit_reason, report.packets_read, report.packets_written
        )));
    }
    if report.read_errors > 0 {
        return Err(AppError::Mp4(format!(
            "重封装过程中有 {} 次读包失败（重试 {} 次），产物可能不完整",
            report.read_errors, report.read_retries
        )));
    }
    if report.write_errors > 0 {
        return Err(AppError::Mp4(format!(
            "重封装过程中有 {} 个包写入失败，产物不完整",
            report.write_errors
        )));
    }
    if report.video_written < report.video_read {
        return Err(AppError::Mp4(format!(
            "视频包写少了：源 {} 个、产物 {} 个（差 {} 个），产物不完整",
            report.video_read,
            report.video_written,
            report.video_read - report.video_written
        )));
    }
    if report.dst_duration_ms <= 0 {
        return Err(AppError::Mp4(
            "导出产物解析不出时长（可能是空文件或损坏），判为导出失败".into(),
        ));
    }
    if report.src_duration_ms > 0 {
        let delta = report.duration_delta_ms();
        if delta > EXPORT_DURATION_TOLERANCE_MS {
            return Err(AppError::Mp4(format!(
                "导出时长异常：源 {} ms，产物 {} ms，差 {} ms（阈值 {} ms）—— 判为导出不完整",
                report.src_duration_ms, report.dst_duration_ms, delta, EXPORT_DURATION_TOLERANCE_MS
            )));
        }
    } else if report.packets_written != report.packets_read {
        return Err(AppError::Mp4(format!(
            "取不到源时长，且包数对不上：读出 {} 个、写出 {} 个",
            report.packets_read, report.packets_written
        )));
    }
    Ok(())
}

/// **停止录制时的导出路径**：整段重封装 **+ 自校验**。
///
/// 与 [`remux_to_plain`] 的唯一区别是最后多一步"对账"：
/// * 源时长（[`probe`] 源文件）× 产物时长（[`probe`] 产物）× 包数 三样一起比，
///   判定规则见 [`verify_full_copy`]；
/// * 不通过就**返回错误**，由调用方（`recorder::mux_thread`）走既有的
///   "保留 Cache 流文件 + 明确告警"路径 —— 用户会看到"导出失败/导出不完整"，
///   而不是一个看不出问题的短文件。
///
/// `+faststart`、copy 语义、分辨率一律不变（与 [`remux_to_plain`] 同一条拷贝循环）。
pub fn remux_to_plain_verified(src: &Path, dst: &Path) -> Result<RemuxReport, AppError> {
    remux_to_plain_verified_opts(src, dst, true)
}

/// 同 [`remux_to_plain_verified`]，但允许调用方在空间不足时关掉 `+faststart`。
///
/// `faststart == false` 时 moov 留在文件末尾：仍然可播、可剪辑，只是不适合边下边播。
/// 这是“停止时空间不够”的降级路径，判定/对账逻辑完全一致。
pub fn remux_to_plain_verified_opts(
    src: &Path,
    dst: &Path,
    faststart: bool,
) -> Result<RemuxReport, AppError> {
    // 源时长先量（拷贝前量，量的就是"要拷多少"）
    let src_duration_ms = match probe(src) {
        Ok(b) => b.duration_ms,
        Err(e) => {
            // 量不到不算致命（包数仍能对账），但**必须留痕**：这是"没能自校验"的事实
            tracing::warn!(src = %src.display(), error = %e, "取不到源文件时长，导出自校验退化为包数对账");
            0
        }
    };
    let (bytes, mut report) = remux_core_opts(src, dst, None, faststart)?;
    report.bytes = bytes;
    report.src_duration_ms = src_duration_ms;
    let dst_brief = probe(dst).map_err(|e| {
        AppError::Mp4(format!(
            "导出产物无法解析（判为导出失败）：{e}（流文件已保留）"
        ))
    })?;
    report.dst_duration_ms = dst_brief.duration_ms;
    verify_full_copy(&report)?;
    tracing::info!(
        src = %src.display(),
        dst = %dst.display(),
        bytes = report.bytes,
        packets_read = report.packets_read,
        packets_written = report.packets_written,
        packets_video = report.video_written,
        packets_audio = report.audio_written,
        src_ms = report.src_duration_ms,
        dst_ms = report.dst_duration_ms,
        delta_ms = report.duration_delta_ms(),
        exit_reason = report.exit_reason,
        read_retries = report.read_retries,
        "整段重封装完成并已自校验（时长/包数一致）"
    );
    Ok(report)
}

/// 按时间区间重封装（`-c copy` 语义，毫秒）。返回写出的字节数。
///
/// * **起点**：`av_seek_frame(AVSEEK_FLAG_BACKWARD)` 定位到该时间戳**之前（不晚于它）的
///   关键帧**，产物从那个关键帧开始，所以实际起点可能略早于 `start_ms`。
/// * **终点**：在**不早于 `end_ms` 的那个包之前停止**——即写入所有时间戳 `< end_ms`
///   的包，遇到第一个时间戳 `>= end_ms` 的包就停下（该包及其后的包都不写）。
///   判定用**输入流自己的 `time_base`**、在 `av_packet_rescale_ts` 改写时间戳之前完成；
///   视频/音频各自独立判定，音频额外留约一帧（`1/25 s`）的余量，避免尾音被切掉。
/// * `range` 为 `None` 时表示**从文件开头一直拷到 EOF**（[`remux_to_plain`] 就是这条路径）。
///
/// 语义细节：读包失败会**重试**（`REMUX_READ_RETRIES` 次、带退避），
/// 重试仍失败或写包失败都**返回错误**——绝不"记一条 warn 就当成功"。
pub fn remux_range(src: &Path, dst: &Path, range: Option<(i64, i64)>) -> Result<u64, AppError> {
    remux_core(src, dst, range).map(|(bytes, _)| bytes)
}

/// 拷贝循环本体：把 `src` 的 packet 原样搬到 `dst`（`+faststart`，不重新编码）。
///
/// 返回 `(产物字节数, 逐包对账报告)`。所有"非 EOF 的读结束"都会变成 `Err`：
/// 这条路径以前只 `warn!` 然后 `break`，产物静默偏短而调用方一无所知。
fn remux_core(
    src: &Path,
    dst: &Path,
    range: Option<(i64, i64)>,
) -> Result<(u64, RemuxReport), AppError> {
    remux_core_opts(src, dst, range, true)
}

/// `remux_core` 的可选 faststart 版本。
///
/// `+faststart` 会在目标目录创建临时文件并整体回拷，停止瞬间需要约等于成片大小的
/// 额外空间。空间不足时调用方可以传 `false`：moov 留在文件末尾，仍然可播，
/// 只是不适合边下边播/快速起播。
fn remux_core_opts(
    src: &Path,
    dst: &Path,
    range: Option<(i64, i64)>,
    faststart: bool,
) -> Result<(u64, RemuxReport), AppError> {
    unsafe {
        let c_src = cpath(src)?;
        let c_dst = cpath(dst)?;
        let fmt = c"mp4";

        // ---- 输入 ----
        let mut ictx: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_open_input(&mut ictx, c_src.as_ptr(), ptr::null(), ptr::null_mut());
        if ret < 0 || ictx.is_null() {
            return Err(AppError::Mp4(format!("打开源文件失败: {}", av_err(ret))));
        }
        let ret = ff::avformat_find_stream_info(ictx, ptr::null_mut());
        if ret < 0 {
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("读取源流信息失败: {}", av_err(ret))));
        }

        // ---- 输出 ----
        let mut octx: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_alloc_output_context2(
            &mut octx,
            ptr::null_mut(),
            fmt.as_ptr(),
            c_dst.as_ptr(),
        );
        if ret < 0 || octx.is_null() {
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("创建输出上下文失败: {}", av_err(ret))));
        }
        if faststart {
            let key = c"movflags";
            let val = c"+faststart";
            ff::av_opt_set((*octx).priv_data, key.as_ptr(), val.as_ptr(), 0);
        }

        let n = (*ictx).nb_streams as usize;
        for i in 0..n {
            let ist = *(*ictx).streams.add(i);
            let ost = ff::avformat_new_stream(octx, ptr::null());
            if ost.is_null() {
                ff::avformat_free_context(octx);
                ff::avformat_close_input(&mut ictx);
                return Err(AppError::Mp4("创建输出流失败".into()));
            }
            let ret = ff::avcodec_parameters_copy((*ost).codecpar, (*ist).codecpar);
            if ret < 0 {
                ff::avformat_free_context(octx);
                ff::avformat_close_input(&mut ictx);
                return Err(AppError::Mp4(format!("复制编解码参数失败: {}", av_err(ret))));
            }
            // P52：`codecpar` 是裸指针，`as_mut()` 为空时 `.unwrap()` 会 panic。
            // `avformat_new_stream` 出来的流它恒非空（FFmpeg 的不变式），但真为空时
            // "沿用源的 codec_tag"也只是小瑕疵 —— 不值得让整次导出崩掉，故用 `if let` 接住。
            if let Some(par) = (*ost).codecpar.as_mut() {
                par.codec_tag = 0;
            }
            (*ost).time_base = (*ist).time_base;
        }

        let mut pb: *mut ff::AVIOContext = ptr::null_mut();
        let ret = ff::avio_open(&mut pb, c_dst.as_ptr(), ff::AVIO_FLAG_WRITE);
        if ret < 0 {
            ff::avformat_free_context(octx);
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("打开输出文件失败: {}", av_err(ret))));
        }
        (*octx).pb = pb;
        let ret = ff::avformat_write_header(octx, ptr::null_mut());
        if ret < 0 {
            ff::avio_closep(&mut (*octx).pb);
            ff::avformat_free_context(octx);
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("写输出头部失败: {}", av_err(ret))));
        }

        // ---- 可选：先定位到起点（含）之前的关键帧 ----
        // `stop_ms`: `Some(end)` = 遇到第一个时间戳不早于 `end` 的包就停；`None` = 拷到 EOF。
        let mut stop_ms: Option<i64> = None;
        if let Some((start_ms, end_ms)) = range {
            // av_seek_frame 以 AV_TIME_BASE(微秒) 为单位；BACKWARD = 落在不晚于该点的关键帧上
            let ts = start_ms.saturating_mul(1000);
            let ret = ff::av_seek_frame(ictx, -1, ts, ff::AVSEEK_FLAG_BACKWARD);
            if ret < 0 {
                tracing::warn!(error = %av_err(ret), "av_seek_frame 失败，将从文件开头复制");
            }
            stop_ms = Some(end_ms);
        }

        // ---- 拷贝 packet ----
        let mut report = RemuxReport {
            exit_reason: "eof",
            first_pts_ms: -1,
            last_pts_ms: -1,
            ..Default::default()
        };
        // 第一个"致命"错误（重试耗尽 / 写失败）；循环结束后据此返回 Err
        let mut fatal: Option<String> = None;
        let mut retry = 0usize;
        let mut pkt: ff::AVPacket = std::mem::zeroed();
        loop {
            let ret = ff::av_read_frame(ictx, &mut pkt);
            if ret < 0 {
                if ret == AVERROR_EOF {
                    report.exit_reason = "eof";
                    break;
                }
                // **读失败不再静默 break**：先重试（带退避），仍失败就判为致命错误
                report.read_errors += 1;
                ff::av_packet_unref(&mut pkt);
                if retry < REMUX_READ_RETRIES {
                    let backoff = REMUX_RETRY_BACKOFF_MS
                        .get(retry)
                        .copied()
                        .unwrap_or(REMUX_RETRY_MAX_BACKOFF_MS);
                    retry += 1;
                    report.read_retries += 1;
                    tracing::warn!(
                        error = %av_err(ret),
                        attempt = retry,
                        backoff_ms = backoff,
                        "读取源 packet 失败，稍后重试"
                    );
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                report.exited_early = true;
                report.exit_reason = "read-error";
                fatal = Some(format!(
                    "读取源文件失败（已重试 {} 次）：{}",
                    report.read_retries,
                    av_err(ret)
                ));
                break;
            }
            // 成功读到包 → 连续失败计数清零（重试额度按"连续失败"计）
            retry = 0;
            report.packets_read += 1;
            let idx = pkt.stream_index as usize;
            if idx < n {
                let ist = *(*ictx).streams.add(idx);
                let ost = *(*octx).streams.add(idx);
                let is_audio = (*(*ist).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
                if is_audio {
                    report.audio_read += 1;
                } else {
                    report.video_read += 1;
                }

                // ---- 终点判定：必须在 av_packet_rescale_ts 改写时间戳**之前**、
                // 用**输入流自己的 time_base** 换算成毫秒 ----
                if let Some(end) = stop_ms {
                    let tb = (*ist).time_base;
                    // 音频比视频多留约一帧的余量（AAC 1024 样本 / 25 fps ≈ 40 ms），
                    // 否则恰好跨过终点的那个音频包会被丢掉、尾音被切
                    let margin_ms = if is_audio { AUDIO_TAIL_MARGIN_MS } else { 0 };
                    let ts = if pkt.dts != ff::AV_NOPTS_VALUE {
                        pkt.dts
                    } else {
                        pkt.pts
                    };
                    if ts != ff::AV_NOPTS_VALUE {
                        let ms = if tb.num > 0 && tb.den > 0 {
                            ff::av_rescale_q(ts, tb, ff::AVRational { num: 1, den: 1000 })
                        } else {
                            ts
                        };
                        if ms >= end + margin_ms {
                            ff::av_packet_unref(&mut pkt);
                            report.exit_reason = "range-end";
                            break;
                        }
                    }
                }

                // 记录产物时间轴上的首/末时间戳（相对源时间轴，毫秒）
                if report.first_pts_ms < 0 {
                    let tb = (*ist).time_base;
                    let ts = if pkt.pts != ff::AV_NOPTS_VALUE {
                        pkt.pts
                    } else {
                        pkt.dts
                    };
                    if ts != ff::AV_NOPTS_VALUE && tb.num > 0 && tb.den > 0 {
                        report.first_pts_ms =
                            ff::av_rescale_q(ts, tb, ff::AVRational { num: 1, den: 1000 });
                    }
                }
                {
                    let tb = (*ist).time_base;
                    let ts = if pkt.pts != ff::AV_NOPTS_VALUE {
                        pkt.pts
                    } else {
                        pkt.dts
                    };
                    if ts != ff::AV_NOPTS_VALUE && tb.num > 0 && tb.den > 0 {
                        let ms = ff::av_rescale_q(ts, tb, ff::AVRational { num: 1, den: 1000 });
                        if ms > report.last_pts_ms {
                            report.last_pts_ms = ms;
                        }
                    }
                }

                // 时间基换算到输出流
                ff::av_packet_rescale_ts(&mut pkt, (*ist).time_base, (*ost).time_base);
                pkt.pos = -1;
                let ret = ff::av_interleaved_write_frame(octx, &mut pkt);
                if ret < 0 {
                    // **丢一个包 = 产物不完整**：不再"warn 后继续"，直接判失败
                    report.write_errors += 1;
                    fatal = Some(format!("写输出 packet 失败（第 {} 个包）：{}", report.packets_read, av_err(ret)));
                    break;
                }
                report.packets_written += 1;
                if is_audio {
                    report.audio_written += 1;
                } else {
                    report.video_written += 1;
                }
            } else {
                ff::av_packet_unref(&mut pkt);
            }
        }

        let ret = ff::av_write_trailer(octx);
        if ret < 0 {
            let msg = av_err(ret);
            tracing::error!(error = %msg, "写输出 trailer 失败");
            fatal.get_or_insert_with(|| format!("写输出 trailer 失败：{msg}"));
        }
        ff::avio_closep(&mut (*octx).pb);
        ff::avformat_free_context(octx);
        ff::avformat_close_input(&mut ictx);

        let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        report.bytes = size;
        tracing::info!(
            packets_read = report.packets_read,
            packets_written = report.packets_written,
            video = report.video_written,
            audio = report.audio_written,
            read_errors = report.read_errors,
            read_retries = report.read_retries,
            write_errors = report.write_errors,
            exit_reason = report.exit_reason,
            first_pts_ms = report.first_pts_ms,
            last_pts_ms = report.last_pts_ms,
            bytes = size,
            dst = %dst.display(),
            "重封装拷贝完成（未重新编码）"
        );
        if let Some(e) = fatal {
            // 这里返回 Err：调用方必须走"保留流文件 + 告警"路径，而不是交付短文件
            return Err(AppError::Mp4(format!(
                "{e}（读出 {} 个包、写出 {} 个包；已停止导出，流文件保留）",
                report.packets_read, report.packets_written
            )));
        }
        if report.packets_written == 0 {
            return Err(AppError::Mp4(format!(
                "没有拷贝到任何 packet（源 {} 解出 {} 个包，结束原因 {}）",
                src.display(),
                report.packets_read,
                report.exit_reason
            )));
        }
        Ok((size, report))
    }
}

/// **多段拼接的 copy 导出**：把 `ranges` 里的每一段 `[start_ms, end_ms]` 依次搬到
/// `dst`，段间**只做 seek + 时间戳平移**，不重新编码、不改变分辨率。
///
/// 与 [`remux_range`] 的关系：单段时语义完全一致（起点 `AVSEEK_FLAG_BACKWARD`
/// 落到不晚于起点的关键帧、终点在第一个时间戳不早于终点的包前停、音频多留一帧
/// 余量），本函数是它的"多段"推广 —— **输入只开一次、输出只开一次**：
///
/// * **时间戳平移**：每一段写入前，为该流取平移基准
///   `base = max(上一段写出的末包 dts + 1, 0)`（输出时基刻度）——
///   于是输出时间轴上各段**首尾相接**、每个流的 `dts` **严格单调**，段间绝不倒挂；
/// * **起点即关键帧**：调用方（[`crate::edit::export_ranges`]）保证每段起点已吸附到
///   关键帧，所以 seek 落点就是段起点，每段第一帧必然是关键帧；为稳妥，起点之前
///   （seek 余量里）的包一律丢弃，且**第一帧不是关键帧就直接报错**——
///   绝不产出开头不是关键帧的废片；
/// * **顺序即播放顺序**（P36）：`ranges` 必须非空，每段 `start < end`；
///   **不要求升序、不要求互不重叠** —— 本函数对每段独立 `seek`，时间戳平移按
///   `out_end_us` 累加，所以乱序 / 重叠 / 同一区间出现两次都天然正确。
///   这正好对应"片段列表可以拖拽调序"的需求（见 [`crate::edit::resolve_segments_ex`]）；
/// * 失败（读包重试耗尽 / 写包失败 / 段起点未对齐关键帧）时**删除半成品**并返回错误。
pub fn remux_ranges(src: &Path, dst: &Path, ranges: &[(i64, i64)]) -> Result<u64, AppError> {
    if ranges.is_empty() {
        return Err(AppError::Mp4("没有需要导出的区间".into()));
    }
    for &(s, e) in ranges {
        if s >= e {
            return Err(AppError::Mp4(format!(
                "导出区间无效：起点 {s} ms 不早于终点 {e} ms"
            )));
        }
    }

    unsafe {
        let c_src = cpath(src)?;
        let c_dst = cpath(dst)?;
        let fmt = c"mp4";

        // ---- 输入（只开一次）----
        let mut ictx: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_open_input(&mut ictx, c_src.as_ptr(), ptr::null(), ptr::null_mut());
        if ret < 0 || ictx.is_null() {
            return Err(AppError::Mp4(format!("打开源文件失败: {}", av_err(ret))));
        }
        let ret = ff::avformat_find_stream_info(ictx, ptr::null_mut());
        if ret < 0 {
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("读取源流信息失败: {}", av_err(ret))));
        }

        // ---- 输出（只开一次）：流参数整体拷贝，`+faststart` ----
        let mut octx: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_alloc_output_context2(
            &mut octx,
            ptr::null_mut(),
            fmt.as_ptr(),
            c_dst.as_ptr(),
        );
        if ret < 0 || octx.is_null() {
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("创建输出上下文失败: {}", av_err(ret))));
        }
        let key = c"movflags";
        let val = c"+faststart";
        ff::av_opt_set((*octx).priv_data, key.as_ptr(), val.as_ptr(), 0);

        let n = (*ictx).nb_streams as usize;
        for i in 0..n {
            let ist = *(*ictx).streams.add(i);
            let ost = ff::avformat_new_stream(octx, ptr::null());
            if ost.is_null() {
                ff::avformat_free_context(octx);
                ff::avformat_close_input(&mut ictx);
                return Err(AppError::Mp4("创建输出流失败".into()));
            }
            let ret = ff::avcodec_parameters_copy((*ost).codecpar, (*ist).codecpar);
            if ret < 0 {
                ff::avformat_free_context(octx);
                ff::avformat_close_input(&mut ictx);
                return Err(AppError::Mp4(format!("复制编解码参数失败: {}", av_err(ret))));
            }
            // P52：`codecpar` 是裸指针，`as_mut()` 为空时 `.unwrap()` 会 panic。
            // `avformat_new_stream` 出来的流它恒非空（FFmpeg 的不变式），但真为空时
            // "沿用源的 codec_tag"也只是小瑕疵 —— 不值得让整次导出崩掉，故用 `if let` 接住。
            if let Some(par) = (*ost).codecpar.as_mut() {
                par.codec_tag = 0;
            }
            (*ost).time_base = (*ist).time_base;
        }

        let mut pb: *mut ff::AVIOContext = ptr::null_mut();
        let ret = ff::avio_open(&mut pb, c_dst.as_ptr(), ff::AVIO_FLAG_WRITE);
        if ret < 0 {
            ff::avformat_free_context(octx);
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("打开输出文件失败: {}", av_err(ret))));
        }
        (*octx).pb = pb;
        let ret = ff::avformat_write_header(octx, ptr::null_mut());
        if ret < 0 {
            ff::avio_closep(&mut (*octx).pb);
            ff::avformat_free_context(octx);
            ff::avformat_close_input(&mut ictx);
            return Err(AppError::Mp4(format!("写输出头部失败: {}", av_err(ret))));
        }

        // ---- 逐段：seek → 过滤 → 平移 → 写 ----
        // 拼接用的统一时间轴（微秒，跨流同口径）：
        //   out_end_us = 已写出内容的**最大结束时间**（-1 = 还没写过）；
        //   seg_anchor = 本段第一个被保留包的 pts（微秒），用来把本段"归零"；
        //   shift_us   = 本段统一平移量 = out_end_us - seg_anchor，
        //                于是本段紧接上一段、且**所有流用同一个平移量**（A/V 不失步）。
        let mut out_end_us: i64 = -1;
        // 每流"本段是否已经写过第一个视频包"（关键帧校验用，只针对视频流）
        let mut seg_video_seen = vec![false; n];
        let mut packets_written = 0u64;
        let mut fatal: Option<String> = None;
        let mut pkt: ff::AVPacket = std::mem::zeroed();
        let us_tb = ff::AVRational { num: 1, den: 1_000_000 };

        'segments: for (seg_idx, &(start_ms, end_ms)) in ranges.iter().enumerate() {
            // 新的一段：锚点在读到本段第一个包时确定，平移量随之计算
            let mut seg_anchor_us: Option<i64> = None;
            let mut shift_us: i64 = 0;
            seg_video_seen.fill(false);

            // seek：以 AV_TIME_BASE（微秒）为单位，BACKWARD = 不晚于起点的关键帧
            let ts = start_ms.saturating_mul(1000);
            let ret = ff::av_seek_frame(ictx, -1, ts, ff::AVSEEK_FLAG_BACKWARD);
            if ret < 0 {
                tracing::warn!(
                    error = %av_err(ret),
                    segment = seg_idx + 1,
                    "第 {} 段 av_seek_frame 失败，将从当前位置读取（起点关键帧校验会兜底）",
                    seg_idx + 1
                );
            }

            let mut retry = 0usize;
            loop {
                let ret = ff::av_read_frame(ictx, &mut pkt);
                if ret < 0 {
                    if ret == AVERROR_EOF {
                        // EOF：当前段之后不可能还有内容（ranges 已钳到片尾内），正常收尾
                        break;
                    }
                    // 读失败：重试（带退避），仍失败判致命错误
                    ff::av_packet_unref(&mut pkt);
                    if retry < REMUX_READ_RETRIES {
                        let backoff = REMUX_RETRY_BACKOFF_MS
                            .get(retry)
                            .copied()
                            .unwrap_or(REMUX_RETRY_MAX_BACKOFF_MS);
                        retry += 1;
                        tracing::warn!(
                            error = %av_err(ret),
                            attempt = retry,
                            backoff_ms = backoff,
                            "读取源 packet 失败，稍后重试"
                        );
                        std::thread::sleep(Duration::from_millis(backoff));
                        continue;
                    }
                    fatal = Some(format!(
                        "读取源文件失败（已重试 {} 次）：{}",
                        retry,
                        av_err(ret)
                    ));
                    break 'segments;
                }
                retry = 0;
                let idx = pkt.stream_index as usize;
                if idx >= n {
                    ff::av_packet_unref(&mut pkt);
                    continue;
                }
                let ist = *(*ictx).streams.add(idx);
                let ost = *(*octx).streams.add(idx);
                let is_audio =
                    (*(*ist).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_AUDIO;

                // ---- 起点过滤：seek 余量里（时间戳早于段起点）的包一律丢弃 ----
                // 判定用输入流自己的 time_base，与终点判定同源
                let tb = (*ist).time_base;
                let dts_or_pts = if pkt.dts != ff::AV_NOPTS_VALUE {
                    pkt.dts
                } else {
                    pkt.pts
                };
                let pkt_ms = if dts_or_pts != ff::AV_NOPTS_VALUE && tb.num > 0 && tb.den > 0 {
                    ff::av_rescale_q(dts_or_pts, tb, ff::AVRational { num: 1, den: 1000 })
                } else {
                    dts_or_pts
                };
                if pkt_ms != ff::AV_NOPTS_VALUE && pkt_ms < start_ms {
                    ff::av_packet_unref(&mut pkt);
                    continue;
                }
                // ---- 终点判定：第一个时间戳不早于终点的包停下（音频多留一帧）----
                let margin_ms = if is_audio { AUDIO_TAIL_MARGIN_MS } else { 0 };
                if pkt_ms != ff::AV_NOPTS_VALUE && pkt_ms >= end_ms + margin_ms {
                    ff::av_packet_unref(&mut pkt);
                    break;
                }

                // ---- 关键帧校验：每段写出的**第一个视频包**必须是关键帧 ----
                // 起点已吸附到关键帧，seek 落点就是段起点；若这里不是关键帧，
                // 说明 seek 没落在关键帧上（素材索引异常/起点未对齐），
                // 继续写只会产出一个"开头无法解码"的废片 —— 直接报错让用户去吸附。
                if !is_audio && !seg_video_seen[idx] {
                    seg_video_seen[idx] = true;
                    if pkt.flags & ff::AV_PKT_FLAG_KEY == 0 {
                        fatal = Some(format!(
                            "第 {} 段的起点没有对齐到关键帧（seek 落点不是关键帧）——\
                             请重新吸附选区，或改用逐帧精确导出",
                            seg_idx + 1
                        ));
                        ff::av_packet_unref(&mut pkt);
                        break 'segments;
                    }
                }

                // ---- 时间戳：统一到微秒 → 归零 + 段间平移 → 转回输出时基 ----
                // 先把 pts/dts 换成微秒（跨流同口径），再统一平移，最后转成输出流时基。
                // 每一段都从 out_end_us 处接上，且音频/视频用同一个平移量，A/V 不会失步。
                let ost_tb = (*ost).time_base;
                let to_us = |ts: i64| -> i64 {
                    if ts == ff::AV_NOPTS_VALUE || tb.num <= 0 || tb.den <= 0 {
                        ts
                    } else {
                        ff::av_rescale_q(ts, tb, us_tb)
                    }
                };
                let raw_pts_us = to_us(pkt.pts);
                let raw_dts_us = to_us(pkt.dts);
                let dur_us = if pkt.duration > 0 && tb.num > 0 && tb.den > 0 {
                    ff::av_rescale_q(pkt.duration, tb, us_tb)
                } else {
                    0
                };
                // 本段锚点：第一个被保留包的 pts（没有 pts 就用 dts）
                if seg_anchor_us.is_none() {
                    let anchor = if raw_pts_us != ff::AV_NOPTS_VALUE {
                        raw_pts_us
                    } else {
                        raw_dts_us
                    };
                    if anchor != ff::AV_NOPTS_VALUE {
                        seg_anchor_us = Some(anchor);
                        shift_us = if out_end_us < 0 { 0 } else { out_end_us } - anchor;
                    }
                }
                let shift = if seg_anchor_us.is_some() { shift_us } else { 0 };
                let from_us = |us: i64| -> i64 {
                    if us == ff::AV_NOPTS_VALUE || ost_tb.num <= 0 || ost_tb.den <= 0 {
                        us
                    } else {
                        ff::av_rescale_q(us + shift, us_tb, ost_tb)
                    }
                };
                let new_pts_us = if raw_pts_us != ff::AV_NOPTS_VALUE {
                    raw_pts_us + shift
                } else {
                    ff::AV_NOPTS_VALUE
                };
                pkt.pts = from_us(raw_pts_us);
                pkt.dts = from_us(raw_dts_us);
                if pkt.duration > 0 && tb.num > 0 && tb.den > 0 {
                    pkt.duration = ff::av_rescale_q(pkt.duration, tb, ost_tb);
                }
                pkt.pos = -1;

                let ret = ff::av_interleaved_write_frame(octx, &mut pkt);
                if ret < 0 {
                    fatal = Some(format!(
                        "写输出 packet 失败（第 {} 段）：{}",
                        seg_idx + 1,
                        av_err(ret)
                    ));
                    ff::av_packet_unref(&mut pkt);
                    break 'segments;
                }
                packets_written += 1;
                // 更新已写出内容的结束时间（微秒）：pts + duration，缺 pts 时退回 dts
                if seg_anchor_us.is_some() {
                    let end_candidate = if new_pts_us != ff::AV_NOPTS_VALUE {
                        new_pts_us + dur_us
                    } else if raw_dts_us != ff::AV_NOPTS_VALUE {
                        raw_dts_us + shift + dur_us
                    } else {
                        i64::MIN
                    };
                    if end_candidate != i64::MIN && end_candidate > out_end_us {
                        out_end_us = end_candidate;
                    }
                }
            }
        }

        let ret = ff::av_write_trailer(octx);
        if ret < 0 {
            let msg = av_err(ret);
            tracing::error!(error = %msg, "写输出 trailer 失败");
            fatal.get_or_insert_with(|| format!("写输出 trailer 失败：{msg}"));
        }
        ff::avio_closep(&mut (*octx).pb);
        ff::avformat_free_context(octx);
        ff::avformat_close_input(&mut ictx);

        if let Some(e) = fatal {
            // 半成品一律删掉：绝不交付可疑文件
            let _ = std::fs::remove_file(dst);
            return Err(AppError::Mp4(format!(
                "{e}（已写出 {packets_written} 个包；半成品文件已删除）"
            )));
        }
        if packets_written == 0 {
            let _ = std::fs::remove_file(dst);
            return Err(AppError::Mp4(format!(
                "没有拷贝到任何 packet（源 {}，{} 个区间）",
                src.display(),
                ranges.len()
            )));
        }

        let size = std::fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
        tracing::info!(
            src = %src.display(),
            dst = %dst.display(),
            segments = ranges.len(),
            packets = packets_written,
            bytes = size,
            "多段拼接 copy 导出完成（未重新编码）"
        );
        Ok(size)
    }
}

// ---------------------------------------------------------------------------
// 测试：无界面，素材用随包 ffmpeg 现场生成（不依赖网络）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ffmpeg_exe;

    fn asset_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cap-mux-tests");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 现场生成 3 秒 320x240@30fps、每 1 秒一个关键帧的素材。
    ///
    /// 随包 ffmpeg 只有 libopenh264（**没有 x264**），所以视频编码器用 `libopenh264`。
    /// 返回 `None` 表示环境不具备（缺 ffmpeg 或缺 libopenh264）→ 调用方跳过而不是失败。
    ///
    /// `tag` 用来区分文件名：cargo 默认并发跑同一个二进制里的测试，
    /// 两个测试写同一个素材路径会互相删文件（实测会偶发 `probe` 失败）。
    fn gen_mp4(tag: &str) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(format!("mux-range-src-{tag}.mp4"));
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
                "testsrc2=size=320x240:rate=30",
                "-t",
                "3",
                "-c:v",
                "libopenh264",
                "-b:v",
                "1M",
                "-g",
                "30",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            return Some(out);
        }
        // 失败时把 stderr 打出来：换编码器/参数时能直接看到原因
        let err = String::from_utf8_lossy(&o.stderr).to_string();
        println!("[skip] 随包 ffmpeg 生成 3 秒素材失败：{err}");
        None
    }

    /// 终点语义：`remux_range(src, dst, Some((1000, 2000)))` 必须真的在 2 秒处停下，
    /// 而不是像修 bug 之前那样一路拷到 EOF。
    #[test]
    fn remux_range_stops_at_end() {
        let Some(src) = gen_mp4("bounded") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过终点语义测试");
            return;
        };
        let src_brief = probe(&src).expect("源素材应能被 probe 解析");
        assert!(
            src_brief.duration_ms >= 2800,
            "源素材应约 3 秒，实际 {} ms",
            src_brief.duration_ms
        );

        let dst = asset_dir().join("mux-range-out.mp4");
        let _ = std::fs::remove_file(&dst);
        let bytes = remux_range(&src, &dst, Some((1000, 2000))).expect("区间重封装应成功");
        assert!(dst.is_file(), "产物必须真的落盘");
        assert!(bytes > 0 && std::fs::metadata(&dst).unwrap().len() > 0);

        let out = probe(&dst).expect("产物应能被 probe 解析");
        println!(
            "[mux] 源 {} ms → [1000, 2000] 产物 {} ms（{} 字节）",
            src_brief.duration_ms, out.duration_ms, bytes
        );
        assert!(
            out.duration_ms > 500,
            "[1s, 2s] 选区产物时长应 > 500 ms，实际 {} ms（终点生效时不该是整段）",
            out.duration_ms
        );
        assert!(
            out.duration_ms < 1600,
            "[1s, 2s] 选区产物时长应 < 1600 ms，实际 {} ms（终点未生效时会接近源的 {} ms）",
            out.duration_ms,
            src_brief.duration_ms
        );
        // copy 语义：分辨率与源一致
        let v = out.streams.iter().find(|s| s.kind == "video");
        assert_eq!(
            v.map(|s| (s.width, s.height)),
            Some((320, 240)),
            "copy 重封装必须保持分辨率"
        );
    }

    /// 多段拼接：`[1000, 1500] + [2000, 3000]` 应得到约 **1500 ms** 的产物
    /// （两段各 500 / 1000 ms，在输出时间轴上首尾相接），而不是源的全长。
    ///
    /// 素材：3 秒、关键帧每 1 秒一个（0/1000/2000/3000 ms）——两段的起点
    /// （1000、2000）本身就是关键帧，这正是 `edit::resolve_segments` 吸附之后
    /// 交给 `remux_ranges` 的口径。
    #[test]
    fn remux_ranges_concatenates_two_segments() {
        let Some(src) = gen_mp4("ranges2") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过多段拼接测试");
            return;
        };
        let src_brief = probe(&src).expect("源素材应能被 probe 解析");
        let dst = asset_dir().join("mux-ranges-2seg.mp4");
        let _ = std::fs::remove_file(&dst);
        let ranges = [(1000i64, 1500i64), (2000, 3000)];
        let bytes = remux_ranges(&src, &dst, &ranges).expect("多段拼接应成功");
        assert!(dst.is_file(), "产物必须真的落盘");
        assert!(bytes > 0 && std::fs::metadata(&dst).unwrap().len() > 0);

        let out = probe(&dst).expect("产物应能被 probe 解析");
        println!(
            "[mux] 源 {} ms → 两段 [1000,1500]+[2000,3000] 产物 {} ms（{} 字节）",
            src_brief.duration_ms, out.duration_ms, bytes
        );
        // 期望总时长 = 500 + 1000 = 1500 ms；容器时长按最后一帧的显示时长补足，
        // 容差取 3 帧 + 150 ms（与 edit 层一致的口径）
        let expected = 1500i64;
        let tol = 150 + 3 * 33;
        assert!(
            (out.duration_ms - expected).abs() <= tol,
            "两段拼接产物时长应约 {expected} ms，实际 {} ms（容差 {tol}）",
            out.duration_ms
        );
        // copy 语义：分辨率与源一致
        let v = out.streams.iter().find(|s| s.kind == "video");
        assert_eq!(
            v.map(|s| (s.width, s.height)),
            Some((320, 240)),
            "copy 重封装必须保持分辨率"
        );
    }

    /// **乱序拼接**（P36）：同样的两段，把列表顺序反过来，产物时长不变、
    /// 但**内容顺序真的反了**。
    ///
    /// 这是"片段列表可以拖拽调序"的底层保证。曾经文档写着"ranges 必须升序"，
    /// 那是把列表当成时间切分；实际上本函数逐段独立 seek + 累加平移，
    /// 顺序完全由调用方给的顺序决定。这条测试把它钉住：
    /// ① 乱序也成功、时长仍是 1500 ms；② 与升序产物**字节不同**（顺序生效了）。
    #[test]
    fn remux_ranges_honours_an_unsorted_order() {
        let Some(src) = gen_mp4("ranges_rev") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过乱序拼接测试");
            return;
        };
        let seq = asset_dir().join("mux-ranges-seq.mp4");
        let rev = asset_dir().join("mux-ranges-rev.mp4");
        let _ = std::fs::remove_file(&seq);
        let _ = std::fs::remove_file(&rev);

        let a = (1000i64, 1500i64); // 500 ms
        let b = (2000i64, 3000i64); // 1000 ms
        remux_ranges(&src, &seq, &[a, b]).expect("升序拼接应成功");
        remux_ranges(&src, &rev, &[b, a]).expect("乱序拼接也必须成功（P36 起允许调序）");

        let expected = 1500i64;
        let tol = 150 + 3 * 33;
        for (name, path) in [("升序", &seq), ("乱序", &rev)] {
            let out = probe(path).expect("产物应能被 probe 解析");
            println!("[mux] {name}拼接产物 {} ms", out.duration_ms);
            assert!(
                (out.duration_ms - expected).abs() <= tol,
                "{name}拼接产物时长应约 {expected} ms，实际 {} ms（容差 {tol}）",
                out.duration_ms
            );
            let v = out.streams.iter().find(|s| s.kind == "video");
            assert_eq!(
                v.map(|s| (s.width, s.height)),
                Some((320, 240)),
                "{name}拼接必须保持分辨率"
            );
        }
        // 顺序生效的硬证据：两种顺序产出的字节**必须不同**
        // （时长一样、流参数一样，唯一差别就是段的先后）
        let seq_bytes = std::fs::read(&seq).expect("读升序产物");
        let rev_bytes = std::fs::read(&rev).expect("读乱序产物");
        assert_ne!(
            seq_bytes, rev_bytes,
            "乱序产物与升序产物字节完全相同，说明段的顺序没有被尊重"
        );
    }

    /// 单段区间：`remux_ranges` 与 `remux_range` 语义一致（都是"起点关键帧 + 终点截断"）。
    #[test]
    fn remux_ranges_single_segment_matches_remux_range() {
        let Some(src) = gen_mp4("ranges1") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过单段拼接测试");
            return;
        };
        let dst = asset_dir().join("mux-ranges-1seg.mp4");
        let _ = std::fs::remove_file(&dst);
        let bytes = remux_ranges(&src, &dst, &[(1000i64, 2000i64)]).expect("单段拼接应成功");
        assert!(bytes > 0);
        let out = probe(&dst).expect("产物应能被 probe 解析");
        assert!(
            out.duration_ms > 500 && out.duration_ms < 1600,
            "[1000, 2000] 产物时长应约 1000 ms，实际 {} ms",
            out.duration_ms
        );
    }

    /// 非法输入：空列表 / 起点不小于终点 → 报错且不产生文件。
    #[test]
    fn remux_ranges_rejects_invalid_input() {
        let Some(src) = gen_mp4("rangesbad") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过非法输入测试");
            return;
        };
        let dst = asset_dir().join("mux-ranges-bad.mp4");
        // 空列表
        assert!(remux_ranges(&src, &dst, &[]).is_err(), "空区间列表必须报错");
        assert!(!dst.exists(), "失败时不该留下产物");
        // 起点 >= 终点
        let bad = [(2000i64, 2000i64), (3000, 1000)];
        assert!(
            remux_ranges(&src, &dst, &bad).is_err(),
            "起点不早于终点的区间必须报错"
        );
        assert!(!dst.exists(), "失败时不该留下产物");
    }

    /// `None` 等价于"拷到 EOF"：整段搬过去，时长与源基本一致。
    #[test]
    fn remux_to_plain_copies_whole_file() {
        let Some(src) = gen_mp4("plain") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过整段重封装测试");
            return;
        };
        let src_brief = probe(&src).expect("源素材应能被 probe 解析");
        let dst = asset_dir().join("mux-plain-out.mp4");
        let _ = std::fs::remove_file(&dst);
        remux_to_plain(&src, &dst).expect("整段重封装应成功");
        let out = probe(&dst).expect("产物应能被 probe 解析");
        assert!(
            (out.duration_ms - src_brief.duration_ms).abs() <= 150,
            "整段重封装时长应与源一致：源 {} ms / 产物 {} ms",
            src_brief.duration_ms,
            out.duration_ms
        );
    }

    // -----------------------------------------------------------------------
    // "绝不静默交付短文件"的判定逻辑（纯函数）
    // -----------------------------------------------------------------------

    /// 一个"完全一致"的整段拷贝报告：5000 ms → 5000 ms，包数一个不少。
    fn healthy_report() -> RemuxReport {
        RemuxReport {
            packets_read: 270,
            packets_written: 270,
            video_read: 150,
            video_written: 150,
            audio_read: 120,
            audio_written: 120,
            write_errors: 0,
            read_errors: 0,
            read_retries: 0,
            exited_early: false,
            exit_reason: "eof",
            first_pts_ms: 0,
            last_pts_ms: 4983,
            src_duration_ms: 5000,
            dst_duration_ms: 5000,
            bytes: 1_000_000,
        }
    }

    /// 一致的报告必须通过（否则真实录制会被误判成失败）。
    #[test]
    fn verify_full_copy_accepts_consistent_report() {
        let r = healthy_report();
        assert!(verify_full_copy(&r).is_ok(), "一致的拷贝报告不该判失败");
        assert_eq!(r.duration_delta_ms(), 0);
        // 边界：正好等于阈值仍算通过（`>` 而不是 `>=`）
        let mut edge = healthy_report();
        edge.dst_duration_ms = edge.src_duration_ms + EXPORT_DURATION_TOLERANCE_MS;
        assert!(verify_full_copy(&edge).is_ok(), "差值恰好等于阈值应放过");
        // 容差内的正常抖动（容器时长取整）不判失败
        let mut jitter = healthy_report();
        jitter.dst_duration_ms = jitter.src_duration_ms - 150;
        assert!(verify_full_copy(&jitter).is_ok(), "150 ms 抖动在容差内");
    }

    /// **每一种"短产物"形态都必须被判失败**（这就是本轮 bug 的守门测试）：
    /// 历史行为是"只 warn 然后继续"，于是 759 ms 的产物被当成成功交付。
    #[test]
    fn verify_full_copy_rejects_every_truncation_shape() {
        // ① 时长明显偏短：现象里的 759 / 2013 / 2772 / 3564 ms 全部覆盖
        for short_ms in [759i64, 2013, 2772, 3564] {
            let mut r = healthy_report();
            r.dst_duration_ms = short_ms;
            let e = verify_full_copy(&r).expect_err(&format!("{short_ms} ms 的产物必须判失败"));
            let msg = e.to_string();
            assert!(
                msg.contains(&short_ms.to_string()) && msg.contains("5000"),
                "报错要带上源/产物时长：{msg}"
            );
        }
        // ② 读包失败（重试也没救回来）
        let mut r = healthy_report();
        r.read_errors = 1;
        r.exited_early = true;
        r.exit_reason = "read-error";
        assert!(verify_full_copy(&r).is_err(), "读错误必须判失败");
        // ③ 提前退出（既不是 EOF 也不是区间终点）
        let mut r = healthy_report();
        r.exited_early = true;
        r.exit_reason = "read-error";
        assert!(verify_full_copy(&r).is_err(), "提前退出必须判失败");
        // ④ 有包没写进去
        let mut r = healthy_report();
        r.write_errors = 1;
        assert!(verify_full_copy(&r).is_err(), "写包失败必须判失败");
        // ⑤ 视频包写少了（时长可能"看起来对"，但内容是残缺的）
        let mut r = healthy_report();
        r.video_read = 150;
        r.video_written = 149;
        r.packets_written = 269;
        let e = verify_full_copy(&r).expect_err("视频包少一个必须判失败");
        assert!(e.to_string().contains("视频包写少了"), "{e}");
        // ⑥ 产物根本解析不出时长
        let mut r = healthy_report();
        r.dst_duration_ms = 0;
        assert!(verify_full_copy(&r).is_err(), "产物时长为 0 必须判失败");
        // ⑦ 取不到源时长时，包数必须严格对账
        let mut r = healthy_report();
        r.src_duration_ms = 0;
        r.packets_written = 269;
        assert!(verify_full_copy(&r).is_err(), "源时长未知时包数不齐必须判失败");
        let mut ok = healthy_report();
        ok.src_duration_ms = 0;
        assert!(verify_full_copy(&ok).is_ok(), "源时长未知但包数齐 → 通过");
    }

    /// 真素材上的自校验路径：`remux_to_plain_verified` 要给出"时长一致 + 包数齐全"的报告，
    /// 并且 `video_written == video_read`（整段拷贝不该丢包）。
    #[test]
    fn remux_to_plain_verified_reports_consistent_copy() {
        let Some(src) = gen_mp4("verified") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过整段导出+自校验测试");
            return;
        };
        let dst = asset_dir().join("mux-verified-out.mp4");
        let _ = std::fs::remove_file(&dst);
        let report = remux_to_plain_verified(&src, &dst).expect("整段导出+自校验应通过");
        assert!(dst.is_file(), "产物必须真的落盘");
        assert!(report.bytes > 0);
        assert!(report.packets_read > 0 && report.packets_written == report.packets_read);
        assert_eq!(report.video_written, report.video_read);
        assert!(!report.exited_early && report.read_errors == 0 && report.write_errors == 0);
        assert_eq!(report.exit_reason, "eof");
        assert!(
            report.duration_delta_ms() <= EXPORT_DURATION_TOLERANCE_MS,
            "源 {} ms / 产物 {} ms",
            report.src_duration_ms,
            report.dst_duration_ms
        );
        // 首/末时间戳：起点≈0，末点≈源时长（这也是"没有丢尾"的直接证据）
        assert!(report.first_pts_ms <= 50, "首包应贴着 0：{}", report.first_pts_ms);
        assert!(
            report.last_pts_ms > 0 && report.last_pts_ms <= report.src_duration_ms,
            "末包时间戳应落在 [0, 源时长] 内：{} (源 {} ms)",
            report.last_pts_ms,
            report.src_duration_ms
        );
    }
}
