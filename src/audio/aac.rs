//! AAC-LC 编码（走 FFmpeg `avcodec`，动态链接随包 LGPLv3 库）。
//!
//! 输入是 48 kHz / 2ch 的**交错 f32**（WASAPI autoconvert 保证），
//! 内部去交织成 FFmpeg 需要的 **FLTP**（planar float），每帧 1024 样本。
//! 输出是**裸 AAC 帧**（无 ADTS 头）——MP4 容器要的就是这个。
//! `avcodec_open2` 之后 `ctx->extradata` 就是 **AudioSpecificConfig**，用于 MP4 的 `esds`。

use crate::error::AppError;
use ffmpeg_next::ffi as ff;
use std::ptr;

/// 读流结束的错误码（宏，bindgen 不导出）
const AVERROR_EOF: i32 = -541_478_725;
/// AAC-LC 每帧样本数
pub const FRAME_SAMPLES: usize = 1024;
/// 采样率（**唯一来源**：`crate::audio::SAMPLE_RATE`，见那里的说明）
pub const SAMPLE_RATE: i32 = crate::audio::SAMPLE_RATE;
/// 声道数（同上）
pub const CHANNELS: usize = crate::audio::CHANNELS;

fn av_err(code: i32) -> String {
    unsafe {
        let mut buf = [0i8; 256];
        if ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) == 0 {
            std::ffi::CStr::from_ptr(buf.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            format!("FFmpeg 错误码 {code}")
        }
    }
}

pub struct AacEncoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    /// 不足一帧的交错残留
    pending: Vec<f32>,
    /// 下一个输出帧的 pts（单位：样本数）
    next_pts: i64,
    extradata: Vec<u8>,
    pub frames_out: u64,
    pub bytes_out: u64,
}

// SAFETY: 编码器实例只在一个线程里使用；这里仅让类型可以放进需要 Send 的结构
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    pub fn new(bitrate_kbps: u32) -> Result<Self, AppError> {
        unsafe {
            let codec = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_AAC);
            if codec.is_null() {
                return Err(AppError::AudioEncode("FFmpeg 没有内置 aac 编码器".into()));
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(AppError::AudioEncode("avcodec_alloc_context3 失败".into()));
            }

            (*ctx).codec_type = ff::AVMediaType::AVMEDIA_TYPE_AUDIO;
            (*ctx).bit_rate = (bitrate_kbps as i64) * 1000;
            (*ctx).sample_rate = SAMPLE_RATE;
            (*ctx).sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
            (*ctx).time_base = ff::AVRational {
                num: 1,
                den: SAMPLE_RATE,
            };
            (*ctx).profile = ff::AV_PROFILE_AAC_LOW;
            ff::av_channel_layout_default(&mut (*ctx).ch_layout, CHANNELS as i32);

            let ret = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ff::avcodec_free_context(&mut { ctx });
                return Err(AppError::AudioEncode(format!(
                    "打开 aac 编码器失败: {}",
                    av_err(ret)
                )));
            }

            let extradata = if (*ctx).extradata.is_null() || (*ctx).extradata_size <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(
                    (*ctx).extradata,
                    (*ctx).extradata_size as usize,
                )
                .to_vec()
            };

            let frame = ff::av_frame_alloc();
            if frame.is_null() {
                ff::avcodec_free_context(&mut { ctx });
                return Err(AppError::AudioEncode("av_frame_alloc 失败".into()));
            }
            (*frame).format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
            (*frame).nb_samples = FRAME_SAMPLES as i32;
            (*frame).sample_rate = SAMPLE_RATE;
            ff::av_channel_layout_default(&mut (*frame).ch_layout, CHANNELS as i32);
            let ret = ff::av_frame_get_buffer(frame, 0);
            if ret < 0 {
                let mut f = frame;
                let mut c = ctx;
                ff::av_frame_free(&mut f);
                ff::avcodec_free_context(&mut c);
                return Err(AppError::AudioEncode(format!(
                    "av_frame_get_buffer 失败: {}",
                    av_err(ret)
                )));
            }

            tracing::info!(
                bitrate_kbps,
                rate = SAMPLE_RATE,
                channels = CHANNELS,
                frame_samples = FRAME_SAMPLES,
                extradata = extradata.len(),
                frame_size = (*ctx).frame_size,
                "AAC-LC 编码器就绪"
            );

            Ok(Self {
                ctx,
                frame,
                pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 4),
                next_pts: 0,
                extradata,
                frames_out: 0,
                bytes_out: 0,
            })
        }
    }

    /// AudioSpecificConfig（用于 MP4 的 esds）
    pub fn extradata(&self) -> &[u8] {
        &self.extradata
    }

    pub fn sample_rate(&self) -> i32 {
        SAMPLE_RATE
    }

    /// 送入任意长度的交错 f32，产出整数个 AAC 帧：`(裸帧字节, pts 样本数)`
    pub fn encode(
        &mut self,
        input: &[f32],
        out: &mut Vec<(Vec<u8>, i64)>,
    ) -> Result<(), AppError> {
        self.pending.extend_from_slice(input);
        let per_block = FRAME_SAMPLES * CHANNELS;
        while self.pending.len() >= per_block {
            let block: Vec<f32> = self.pending.drain(..per_block).collect();
            self.encode_block(&block, out)?;
        }
        Ok(())
    }

    fn encode_block(
        &mut self,
        block: &[f32],
        out: &mut Vec<(Vec<u8>, i64)>,
    ) -> Result<(), AppError> {
        unsafe {
            // 帧缓冲可能被上一次 send 引用，写入前确保可写
            let ret = ff::av_frame_make_writable(self.frame);
            if ret < 0 {
                return Err(AppError::AudioEncode(format!(
                    "av_frame_make_writable 失败: {}",
                    av_err(ret)
                )));
            }
            (*self.frame).nb_samples = FRAME_SAMPLES as i32;
            (*self.frame).pts = self.next_pts;
            let left = (*self.frame).data[0] as *mut f32;
            let right = (*self.frame).data[1] as *mut f32;
            debug_assert!(!left.is_null() && !right.is_null(), "FLTP 平面为空");
            for i in 0..FRAME_SAMPLES {
                *left.add(i) = block[i * CHANNELS];
                *right.add(i) = block[i * CHANNELS + 1];
            }
            self.next_pts += FRAME_SAMPLES as i64;

            let ret = ff::avcodec_send_frame(self.ctx, self.frame);
            if ret < 0 && ret != AVERROR_EOF {
                return Err(AppError::AudioEncode(format!(
                    "送 AAC 帧失败: {}",
                    av_err(ret)
                )));
            }
            self.drain(out)
        }
    }

    /// 冲刷编码器：把内部残留样本补齐并取出剩余帧
    pub fn finish(&mut self, out: &mut Vec<(Vec<u8>, i64)>) -> Result<(), AppError> {
        // 用静音补齐最后一帧
        if !self.pending.is_empty() {
            let per_block = FRAME_SAMPLES * CHANNELS;
            self.pending.resize(per_block, 0.0);
            let block: Vec<f32> = self.pending.drain(..per_block).collect();
            self.encode_block(&block, out)?;
        }
        unsafe {
            let ret = ff::avcodec_send_frame(self.ctx, ptr::null());
            if ret < 0 && ret != AVERROR_EOF {
                tracing::warn!(error = %av_err(ret), "冲刷 AAC 编码器时 send_frame 返回错误");
            }
            self.drain(out)?;
        }
        Ok(())
    }

    unsafe fn drain(&mut self, out: &mut Vec<(Vec<u8>, i64)>) -> Result<(), AppError> {
        let mut pkt = ff::av_packet_alloc();
        if pkt.is_null() {
            return Err(AppError::AudioEncode("av_packet_alloc 失败".into()));
        }
        loop {
            let ret = ff::avcodec_receive_packet(self.ctx, pkt);
            if ret == AVERROR_EOF {
                break;
            }
            if ret < 0 {
                // EAGAIN：需要更多输入
                break;
            }
            let data =
                std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize).to_vec();
            let pts = (*pkt).pts;
            self.bytes_out += data.len() as u64;
            self.frames_out += 1;
            out.push((data, pts));
            ff::av_packet_unref(pkt);
        }
        ff::av_packet_free(&mut pkt);
        Ok(())
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.frame.is_null() {
                ff::av_frame_free(&mut self.frame);
            }
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_sine_to_aac_frames() {
        // 1 秒 1 kHz 正弦（48k/2ch），应产出约 48000/1024 ≈ 46~48 个 AAC 帧
        let mut enc = match AacEncoder::new(192) {
            Ok(e) => e,
            Err(e) => {
                println!("跳过：无法初始化 AAC 编码器（{e}）");
                return;
            }
        };
        assert!(!enc.extradata().is_empty(), "应能从 FFmpeg 取到 AudioSpecificConfig");

        let n = SAMPLE_RATE as usize;
        let mut input = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = 0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / SAMPLE_RATE as f32).sin();
            input.push(v);
            input.push(v);
        }
        let mut out = Vec::new();
        enc.encode(&input, &mut out).unwrap();
        enc.finish(&mut out).unwrap();

        println!(
            "AAC 帧数 {}，总字节 {}，extradata {} 字节: {:02X?}",
            out.len(),
            enc.bytes_out,
            enc.extradata().len(),
            &enc.extradata()[..enc.extradata().len().min(8)]
        );
        assert!(
            (45..=50).contains(&out.len()),
            "1 秒 48 kHz 应约 47 个 1024 样本帧，实际 {}",
            out.len()
        );
        assert!(out.iter().all(|(d, _)| !d.is_empty()), "不应有空的 AAC 帧");
        // pts 是 1024 的整数倍且严格递增。注意：AAC 有 **priming 延迟**，
        // 首帧 pts 往往是负数（-1024），所以这里不要求 pts[0] == 0；
        // 录制时我们用自己维护的帧计数当时间戳，避免负时间戳进 MP4。
        let pts: Vec<i64> = out.iter().map(|(_, p)| *p).collect();
        println!("AAC pts 前 3 个: {:?}", &pts[..3.min(pts.len())]);
        assert!(
            pts.windows(2).all(|w| w[1] > w[0]),
            "pts 必须严格递增: {pts:?}"
        );
        assert!(
            pts.iter().all(|p| p % FRAME_SAMPLES as i64 == 0),
            "pts 应为 1024 样本的整数倍: {pts:?}"
        );
        // AAC-LC 192 kbps 一秒约 24 KB，量级检查
        assert!(
            (10_000..60_000).contains(&enc.bytes_out),
            "192 kbps 一秒应在 10~60KB 量级，实际 {}",
            enc.bytes_out
        );
    }
}
