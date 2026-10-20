//! 录制闭环烟雾测试（无 UI）：DXGI 采集 → 指针合成 → libyuv → OpenH264 →
//! （可选）WASAPI 采集 → 混音 → AAC-LC → FFmpeg 分片 MP4 → 重封装 → 校验。
//!
//! 用法（先 `. _rust\env.ps1`）：
//! ```text
//! record_smoke [秒数] [输出.mp4] [--no-cursor] [--audio off|system|mic|both] [--audio-only]
//! ```

use caplite::audio::aac::{AacEncoder, FRAME_SAMPLES, SAMPLE_RATE};
use caplite::audio::capture::{AudioCapture, SourceKind};
use caplite::audio::{devices, mixer};
use caplite::config::{CaptureMode, Config, Region};
use caplite::mux::{self, AudioParams, VideoParams};
use caplite::video::capture::{Capture, CaptureSpec, GrabOutcome};
use caplite::video::encode::VideoEncoder;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn write_wav_i16(path: &std::path::Path, samples: &[f32], rate: u32, channels: u16) {
    use std::io::Write;
    let data_len = samples.len() * 2;
    let Ok(f) = std::fs::File::create(path) else { return };
    let mut f = std::io::BufWriter::new(f);
    let _ = f.write_all(b"RIFF");
    let _ = f.write_all(&((36 + data_len) as u32).to_le_bytes());
    let _ = f.write_all(b"WAVEfmt ");
    let _ = f.write_all(&16u32.to_le_bytes());
    let _ = f.write_all(&1u16.to_le_bytes());
    let _ = f.write_all(&channels.to_le_bytes());
    let _ = f.write_all(&rate.to_le_bytes());
    let _ = f.write_all(&(rate * channels as u32 * 2).to_le_bytes());
    let _ = f.write_all(&(channels * 2).to_le_bytes());
    let _ = f.write_all(&16u16.to_le_bytes());
    let _ = f.write_all(b"data");
    let _ = f.write_all(&(data_len as u32).to_le_bytes());
    for s in samples {
        let _ = f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
}

/// 打开请求的音源
fn open_audio(which: &str) -> Vec<(SourceKind, AudioCapture)> {
    let mut out = Vec::new();
    let want: &[SourceKind] = match which {
        "system" => &[SourceKind::System],
        "mic" => &[SourceKind::Microphone],
        "both" => &[SourceKind::System, SourceKind::Microphone],
        _ => &[],
    };
    for kind in want {
        let dev = match kind {
            SourceKind::System => devices::resolve_render(None),
            SourceKind::Microphone => devices::resolve_mic(None),
        };
        match dev.and_then(|d| AudioCapture::open(*kind, d)) {
            Ok(c) => {
                println!(
                    "[smoke] 音源已打开：{}",
                    match kind {
                        SourceKind::System => "系统声音",
                        SourceKind::Microphone => "麦克风",
                    }
                );
                out.push((*kind, c));
            }
            Err(e) => println!("[smoke] 音源打开失败（{kind:?}）：{e}"),
        }
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 允许写成 `3` 或 `3s`：**别让写错的单位静默变成默认 5 秒**（踩过一次：
    // 传 `3s` 时 parse 失败、回落到 5，跑出来的时长和预期对不上，很容易误判成回归）。
    let secs: u64 = args
        .iter()
        .find_map(|a| a.trim_end_matches(['s', 'S']).parse::<u64>().ok())
        .unwrap_or(5);
    let draw_cursor = !args.iter().any(|a| a == "--no-cursor");
    let audio_only = args.iter().any(|a| a == "--audio-only");
    let audio_which = args
        .iter()
        .position(|a| a == "--audio")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| if audio_only { "system".into() } else { "off".into() });
    let out: PathBuf = args
        .iter()
        .find(|a| a.ends_with(".mp4"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("_rust/smoke/rec.mp4"));
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 连续手动跑 smoke 时允许覆盖上一次的测试产物；产品录制路径仍由
    // StartGuard/create_new 保证不覆盖用户文件。
    let _ = std::fs::remove_file(&out);

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();
    ffmpeg_next::init()?;

    let cfg = Config {
        fps: 30,
        video_bitrate_kbps: 8000,
        keyframe_interval_sec: 2,
        audio_bitrate_kbps: 192,
        ..Config::default()
    };

    // ---------- 音频准备 ----------
    let mut audio_sources = open_audio(&audio_which);
    let mut aac = if audio_sources.is_empty() {
        None
    } else {
        Some(AacEncoder::new(cfg.audio_bitrate_kbps)?)
    };
    let mut audio_scratch: Vec<Vec<f32>> = audio_sources.iter().map(|_| Vec::new()).collect();
    // 每路音源已消费的帧数（含补的静音），用于按墙上时钟对齐时间轴
    let mut audio_consumed: Vec<usize> = vec![0; audio_sources.len()];
    let mut mixed: Vec<f32> = Vec::new();
    let mut all_audio: Vec<f32> = Vec::new();
    let mut aac_out: Vec<(Vec<u8>, i64)> = Vec::new();
    let mut audio_frames_written: u64 = 0;

    // ---------- 视频准备（仅声音模式下跳过） ----------
    let mut capture = if audio_only {
        None
    } else {
        Some(Capture::open(CaptureSpec {
            monitor_index: 0,
            mode: CaptureMode::FullScreen,
            region: Region::default(),
            draw_cursor,
        })?)
    };
    let mut encoder = match &capture {
        Some(c) => Some(VideoEncoder::new(
            &cfg,
            c.crop().width as i32,
            c.crop().height as i32,
        )?),
        None => None,
    };

    let fps = cfg.fps as u64;
    let interval = Duration::from_secs_f64(1.0 / cfg.fps as f64);
    let start = Instant::now();
    let mut deadline = start;
    let mut frames: u64 = 0;
    let mut reused: u64 = 0;
    let mut keyframes: u64 = 0;
    let mut writer: Option<mux::FragmentedMp4Writer> = None;

    println!(
        "[smoke] 开始录制 {} 秒（音频={}，{}）→ {}",
        secs,
        audio_which,
        if audio_only { "仅声音" } else { "画面" },
        out.display()
    );

    while start.elapsed() < Duration::from_secs(secs) {
        // ---- 视频 ----
        if let (Some(cap), Some(enc)) = (capture.as_mut(), encoder.as_mut()) {
            match cap.grab(30)? {
                GrabOutcome::New => {}
                GrabOutcome::Reused | GrabOutcome::Recovered => reused += 1,
            }
            let (avcc, is_key) = enc.encode(cap.frame())?;
            if is_key {
                keyframes += 1;
            }
            if writer.is_none() {
                let vp = VideoParams {
                    width: enc.width(),
                    height: enc.height(),
                    fps: cfg.fps as f64,
                    bitrate: (cfg.video_bitrate_kbps as i64) * 1000,
                    extradata: enc.avcc_extradata()?,
                };
                let ap = aac.as_ref().map(|a| AudioParams {
                    sample_rate: a.sample_rate(),
                    channels: 2,
                    bitrate: (cfg.audio_bitrate_kbps as i64) * 1000,
                    extradata: a.extradata().to_vec(),
                });
                writer = Some(mux::FragmentedMp4Writer::create(&out, Some(&vp), ap.as_ref())?);
                println!("[smoke] 分片 MP4 已开始写入（视频+SPS/PPS）；音频流 {}", if ap.is_some() { "有" } else { "无" });
            }
            let wr = writer.as_mut().unwrap();
            wr.write_video(&avcc, (frames as i64) * 1000 / fps as i64, is_key)?;
            frames += 1;
        }

        // ---- 音频（非阻塞轮询） ----
        if let Some(enc_audio) = aac.as_mut() {
            let target_frames = (start.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as usize;
            for (i, (_, cap)) in audio_sources.iter_mut().enumerate() {
                let got = cap.read_chunk(&mut audio_scratch[i], 0)?;
                // 关键：WASAPI 环回在系统静音时**不产生数据包**（实测 4s 只拿到 2.67s），
                // 因此音频时间轴必须按墙上时钟推进——缺多少补多少静音，
                // 否则音轨会比视频短（AV 时长不一致、播放器行为怪异）。
                let have = audio_consumed[i] + got;
                if target_frames > have {
                    mixer::push_silence(&mut audio_scratch[i], target_frames - have);
                }
                audio_consumed[i] = target_frames.max(have);
            }
            let refs: Vec<&[f32]> = audio_scratch.iter().map(|v| v.as_slice()).collect();
            mixer::mix_into(&refs, &mut mixed);
            for v in audio_scratch.iter_mut() {
                v.clear();
            }
            if !mixed.is_empty() {
                all_audio.extend_from_slice(&mixed);
                aac_out.clear();
                enc_audio.encode(&mixed, &mut aac_out)?;
                for (data, _pts) in aac_out.drain(..) {
                    // 用自己维护的帧计数当时间戳：避免 AAC priming 造成的负 pts
                    let pts_ms = audio_frames_written as i64 * FRAME_SAMPLES as i64 * 1000
                        / SAMPLE_RATE as i64;
                    if writer.is_none() {
                        let ap = AudioParams {
                            sample_rate: enc_audio.sample_rate(),
                            channels: 2,
                            bitrate: (cfg.audio_bitrate_kbps as i64) * 1000,
                            extradata: enc_audio.extradata().to_vec(),
                        };
                        writer = Some(mux::FragmentedMp4Writer::create(&out, None, Some(&ap))?);
                        println!("[smoke] 分片 MP4 已开始写入（仅音频）");
                    }
                    if let Some(wr) = writer.as_mut() {
                        wr.write_audio(&data, pts_ms)?;
                    }
                    audio_frames_written += 1;
                }
            }
        }

        // 每秒刷盘，保证录制中可播
        if let Some(wr) = writer.as_mut() {
            if frames.is_multiple_of(fps) || (frames == 0 && audio_frames_written.is_multiple_of(40)) {
                wr.flush()?;
            }
        }

        // ---- 节拍 ----
        deadline += interval;
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        } else if now > deadline + interval * 3 {
            deadline = now;
        }
    }

    // 冲刷音频编码器的残留
    if let Some(enc_audio) = aac.as_mut() {
        aac_out.clear();
        enc_audio.finish(&mut aac_out)?;
        for (data, _pts) in aac_out.drain(..) {
            let pts_ms =
                audio_frames_written as i64 * FRAME_SAMPLES as i64 * 1000 / SAMPLE_RATE as i64;
            if let Some(wr) = writer.as_mut() {
                wr.write_audio(&data, pts_ms)?;
            }
            audio_frames_written += 1;
        }
    }
    if let Some(mut wr) = writer.take() {
        wr.finish()?;
    }
    for (_, cap) in audio_sources.iter_mut() {
        cap.stop();
    }

    let stream_size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!(
        "[smoke] 录制结束：视频 {} 帧（复用 {}，关键帧 {}），音频 {} 个 AAC 帧，流文件 {} KB",
        frames,
        reused,
        keyframes,
        audio_frames_written,
        stream_size / 1024
    );
    if let Some(enc) = &encoder {
        println!("[smoke] 视频码率 ≈ {} kbps", enc.measured_kbps());
    }
    if let Some(cap) = &capture {
        let s = cap.stats();
        println!(
            "[smoke] 采集统计：新帧 {}，无变化 {}，会话重建 {}",
            s.frames, s.no_change, s.access_lost
        );
    }
    if !all_audio.is_empty() {
        let wav = out.with_file_name(format!(
            "{}-audio.wav",
            out.file_stem().unwrap_or_default().to_string_lossy()
        ));
        write_wav_i16(&wav, &all_audio, SAMPLE_RATE as u32, 2);
        println!(
            "[smoke] 音频 RMS={:.5}，已写 {}",
            caplite::audio::rms(&all_audio),
            wav.display()
        );
    }

    // ---------- 重封装 + 校验 ----------
    let plain = out.with_file_name(format!(
        "{}.plain.mp4",
        out.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let t = Instant::now();
    mux::remux_to_plain(&out, &plain)?;
    println!(
        "[smoke] 重封装完成：{}（{} KB，耗时 {:?}）",
        plain.display(),
        std::fs::metadata(&plain).map(|m| m.len()).unwrap_or(0) / 1024,
        t.elapsed()
    );

    for (label, p) in [("分片流文件", &out), ("普通 MP4", &plain)] {
        match mux::probe(p) {
            Ok(info) => {
                println!(
                    "[smoke] {label}：format={} 时长 {} ms，{} 个流",
                    info.format,
                    info.duration_ms,
                    info.streams.len()
                );
                for s in &info.streams {
                    println!(
                        "          流#{} {} codec={} {}x{} {}Hz {}ch 时长 {} ms",
                        s.index,
                        s.kind,
                        s.codec,
                        s.width,
                        s.height,
                        s.sample_rate,
                        s.channels,
                        s.duration_ms
                    );
                }
            }
            Err(e) => println!("[smoke] {label} 解析失败: {e}"),
        }
    }
    Ok(())
}
