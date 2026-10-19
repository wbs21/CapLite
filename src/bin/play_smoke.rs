//! 播放音频链路烟雾测试：**一边播、一边环回采集**，用"扬声器出声被系统录到"
//! 客观证明"播放有声音"（而不是靠人站在机器旁听）。
//!
//! ```text
//! play_smoke [秒数] [素材.mp4]
//! ```
//! * 默认现生成一段 440 Hz 正弦 AAC 素材（随包 ffmpeg），播 3 秒；
//! * 同时开环回采集（`--audio` 固定 system，因为播放走的正是默认渲染设备的环回）；
//! * 对账四件事：输出设备真的开了（`output_live`）、真的写了设备
//!   （`rendered_frames ≈ 秒数 × 48 kHz`）、**环回采到了声音**（RMS > 阈值）、
//!   解码内容有峰值。任一不符 → 退出码 2 并指明是哪一项。
//!
//! 这是 `edit::audio_out` 的**实机验收工具**：沙箱里"设备不存在"只走降级路径，
//! 真出声必须在这里验。UI 侧的最终验收是：剪辑页点播放，耳朵听到声音。

use caplite::audio::capture::{rms, AudioCapture, SourceKind};
use caplite::audio::devices::resolve_render;
use caplite::edit::audio_out::AudioTrackHandle;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 环回 RMS 低于它视为"没出声"（扬声器音量再小也远高于此）
const MIN_LOOPBACK_RMS: f32 = 0.01;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: f64 = args
        .iter()
        .find_map(|a| a.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(3.0);
    let given = args.iter().find(|a| !a.parse::<f64>().is_ok()).cloned();

    // ---- 素材：给了路径就用，否则现生成 440 Hz 正弦 ----
    let path = if let Some(p) = given {
        PathBuf::from(p)
    } else {
        gen_sine(secs + 0.5)?
    };
    println!("PLAY_FILE={}", path.display());
    println!("PLAY_SECS={secs}");

    // ---- 环回采集：先起，覆盖播放的启动相位 ----
    let device = resolve_render(None)?;
    let mut cap = AudioCapture::open(SourceKind::System, device)?;
    let cap_secs = secs + 1.5;
    let cap_thread = std::thread::spawn(move || {
        let mut buf: Vec<f32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs_f64(cap_secs);
        while Instant::now() < deadline {
            let _ = cap.read_chunk(&mut buf, 200);
        }
        buf
    });
    // 让采集流稳定 300 ms 再开播（否则声道建立期的空白会被算进来）
    std::thread::sleep(Duration::from_millis(300));

    // ---- 播放 ----
    let mut h = AudioTrackHandle::spawn(&path, 0, true).expect("播放线程应能启动");
    let deadline = Instant::now() + Duration::from_secs_f64(secs + 1.0);
    while Instant::now() < deadline && !h.is_eof() {
        std::thread::sleep(Duration::from_millis(50));
    }
    h.stop();
    let s = h.stats();
    let loopback = cap_thread.join().expect("采集线程应能 join");

    // ---- 对账 ----
    let lb_rms = rms(&loopback);
    let expected_frames = (secs * caplite::edit::audio_out::OUT_RATE as f64) as u64;
    println!("PLAY_OUTPUT_LIVE={}", s.output_live);
    println!("PLAY_RENDERED_FRAMES={}", s.rendered_frames);
    println!("PLAY_RENDERED_SECS={:.3}", s.rendered_frames as f64 / 48000.0);
    println!("PLAY_DECODED_FRAMES={}", s.decoded_frames);
    println!("PLAY_DECODED_PEAK={:.4}", s.peak);
    println!("PLAY_LAST_PTS_MS={}", s.last_pts_ms);
    println!("PLAY_LOOPBACK_SAMPLES={}", loopback.len() / 2);
    println!("PLAY_LOOPBACK_RMS={:.4}", lb_rms);
    println!("PLAY_MAX_WRITE_GAP_MS={}", s.max_write_gap_ms);
    println!("PLAY_SILENT_BATCHES={}", s.silent_batches);
    println!("PLAY_WRITE_BATCHES={}", s.write_batches);
    if s.write_batches > 0 {
        println!(
            "PLAY_UNDERRUN_PCT={:.1}",
            s.silent_batches as f64 / s.write_batches as f64 * 100.0
        );
    }

    let mut failed: Vec<&str> = Vec::new();
    if !s.output_live {
        failed.push("输出设备没打开（output_live=false，查日志里的降级原因）");
    }
    // 渲染量对账：音频可能比 secs 略短（素材尾 / 提前 stop），下限放宽到 60%
    if s.rendered_frames < expected_frames * 6 / 10 {
        failed.push("写进设备的帧数远小于预期（rendered_frames 太小）");
    }
    if s.peak <= 0.05 {
        failed.push("解码内容没有峰值（peak 太小，素材有问题？）");
    }
    if lb_rms < MIN_LOOPBACK_RMS {
        failed.push("环回没采到声音（RMS 过低：设备静音/播放没出声）");
    }
    if !failed.is_empty() {
        for f in &failed {
            eprintln!("[play] 失败：{f}");
        }
        std::process::exit(2);
    }
    println!("PLAY_OK=1（环回 RMS {lb_rms:.4} > {MIN_LOOPBACK_RMS}：扬声器确实在响）");
    Ok(())
}

/// 用随包 ffmpeg 生成一段 440 Hz 正弦 AAC 素材（`secs` 秒，mp4 容器）。
fn gen_sine(secs: f64) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let ff = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
        .join("bin")
        .join("ffmpeg.exe");
    if !ff.is_file() {
        return Err(format!("找不到随包 ffmpeg：{}", ff.display()).into());
    }
    let out = std::env::temp_dir().join("cap-play-smoke-sine.mp4");
    let status = std::process::Command::new(&ff)
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=48000:duration={secs}"),
            "-c:a",
            "aac",
            "-b:a",
            "128k",
        ])
        .arg(&out)
        .status()?;
    if !status.success() {
        return Err("ffmpeg 生成正弦素材失败".into());
    }
    Ok(out)
}
