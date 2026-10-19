//! 音频采集探针：枚举设备 → 抓系统声音 / 麦克风 → 报告帧数、实测采样率、RMS → 落 WAV。
//!
//! 用法： `audio_probe [秒数] [system|mic|both] [--sta] [--mta]`
//!
//! 客观验证"真的有声音"的方式：播放一段 1 kHz 测试音，同时运行本程序抓系统声音，
//! 播放期间 RMS 应显著高于静音段。
//!
//! `--sta` 会**先把本线程初始化成 STA** 再走正常打开流程——这正是 eframe/winit 主线程的
//! 处境，用来复现/验证"仅声音模式启动录制失败：CoInitializeEx(MTA) 失败 (0x80010106)"这个 bug。
//! 默认（`--mta`）是先 MTA，也就是旧版能跑通的那条路径。

use caplite::audio::capture::{estimate_rate, rms, AudioCapture, SourceKind, TARGET_CHANNELS, TARGET_RATE};
use caplite::audio::devices;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 很小的 16-bit PCM WAV 写入器（仅用于人工试听核对）
fn write_wav_i16(path: &Path, samples: &[f32], rate: u32, channels: u16) -> std::io::Result<()> {
    use std::io::Write;
    let data_len = samples.len() * 2;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&((36 + data_len) as u32).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * channels as u32 * 2).to_le_bytes())?; // byte rate
    f.write_all(&(channels * 2).to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits
    f.write_all(b"data")?;
    f.write_all(&(data_len as u32).to_le_bytes())?;
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        f.write_all(&v.to_le_bytes())?;
    }
    f.flush()
}

fn grab(kind: SourceKind, seconds: f64, out_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let device = match kind {
        SourceKind::System => devices::resolve_render(None)?,
        SourceKind::Microphone => devices::resolve_mic(None)?,
    };
    let name = device.get_friendlyname().unwrap_or_else(|_| "?".into());
    println!(
        "[audio] 打开{}：{name}",
        if kind == SourceKind::System { "系统声音" } else { "麦克风" }
    );

    let mut cap = AudioCapture::open(kind, device)?;
    let mut frames_buf: Vec<f32> = Vec::new();
    let t0 = Instant::now();
    let want = (seconds * TARGET_RATE as f64) as u64;
    let mut last_report = Instant::now();

    while (cap.frames) < want {
        cap.read_chunk(&mut frames_buf, 200)?;
        if last_report.elapsed().as_millis() >= 500 {
            last_report = Instant::now();
            let seg = &frames_buf[frames_buf.len().saturating_sub(TARGET_RATE * TARGET_CHANNELS)..];
            println!(
                "[audio]   {:.1}s 累计 {} 帧，最近 0.5s RMS={:.5}",
                t0.elapsed().as_secs_f64(),
                cap.frames,
                rms(seg)
            );
        }
        if t0.elapsed().as_secs_f64() > seconds + 2.0 {
            println!("[audio]  超时退出（可能该音源没有数据）");
            break;
        }
    }
    let elapsed = t0.elapsed();
    let rate = estimate_rate(cap.frames, elapsed);
    let tag = if kind == SourceKind::System { "system" } else { "mic" };
    let wav = out_dir.join(format!("audio-{tag}.wav"));
    write_wav_i16(&wav, &frames_buf, TARGET_RATE as u32, TARGET_CHANNELS as u16)?;

    println!(
        "[audio] {tag} 结束：{} 帧（{:.2}s 数据 / {:.2}s 墙钟，实测 {:.0} Hz），整体 RMS={:.5}，超时 {} 次",
        cap.frames,
        cap.frames as f64 / TARGET_RATE as f64,
        elapsed.as_secs_f64(),
        rate,
        rms(&frames_buf),
        cap.timeouts
    );
    println!("[audio]   已写 {}", wav.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds: f64 = args
        .iter()
        .find_map(|a| a.parse::<f64>().ok())
        .unwrap_or(3.0);
    let which = args
        .iter()
        .find(|a| ["system", "mic", "both"].contains(&a.as_str()))
        .cloned()
        .unwrap_or_else(|| "both".into());
    let out_dir = PathBuf::from("_rust/audio");
    std::fs::create_dir_all(&out_dir)?;

    // 复现用户环境的开关：`--sta` = "主线程已经被 winit/eframe 初始化成 STA"（见文件头注释）。
    // 没有 `--sta` 时线程还是干净的，`ensure_com()` 会自己按 MTA 初始化。
    if args.iter().any(|a| a == "--sta") {
        let hr = devices::initialize_sta_for_probe();
        println!("[com] 本线程已按 STA 初始化：{hr:?}");
    }
    // `ensure_com()` 正是 `AudioCapture::open()` 内部调用的那一个函数：
    // 在 STA 线程上它此前会返回 `CoInitializeEx(MTA) 失败: (0x80010106)` 并让整个打开流程失败。
    match devices::ensure_com() {
        Ok(()) => println!("[com] ensure_com 通过（S_OK / S_FALSE / RPC_E_CHANGED_MODE 都算就绪）"),
        Err(e) => {
            println!("[com] ensure_com 失败：{e}");
            return Ok(());
        }
    }

    println!("=== 音频设备 ===");
    match devices::list_render_devices() {
        Ok(v) => {
            println!("渲染设备 {} 个：", v.len());
            for d in &v {
                println!("  - {}", d.label());
            }
        }
        Err(e) => println!("渲染设备枚举失败: {e}"),
    }
    match devices::list_capture_devices() {
        Ok(v) => {
            println!("采集设备 {} 个：", v.len());
            for d in &v {
                println!("  - {}", d.label());
            }
        }
        Err(e) => println!("采集设备枚举失败: {e}"),
    }

    println!("\n=== 采集 {seconds}s（{which}）===");
    if which == "system" || which == "both" {
        if let Err(e) = grab(SourceKind::System, seconds, &out_dir) {
            println!("[audio] 系统声音采集失败：{e}");
        }
    }
    if which == "mic" || which == "both" {
        if let Err(e) = grab(SourceKind::Microphone, seconds, &out_dir) {
            println!("[audio] 麦克风采集失败：{e}");
        }
    }
    Ok(())
}
