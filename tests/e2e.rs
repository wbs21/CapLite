//! 端到端集成测试（**无界面、可重复跑**）。
//!
//! 覆盖：
//! 1. 关键帧吸附语义（含"3 秒关键帧实存 269999"这类半帧边界与超范围夹紧）；
//! 2. copy 导出后**分辨率不变**、**时长正确**（既校验导出报告，也用随包 ffprobe 独立复核）；
//! 3. 分片 mp4 也能被 `edit::open` 读取、能扫到关键帧；
//! 4. 缩略图返回合法 RGBA8；
//! 5. 非法选区必须报错而不是产出垃圾。
//!
//! 素材优先用**随包 ffmpeg** 现场生成（不依赖网络）；如果随包 ffmpeg 或 libopenh264
//! 不可用，就退回到工作区里已有的 `_rust/smoke/*.plain.mp4`；两者都没有时**跳过而不是失败**。

use caplite::edit::{self, Clip, StreamInfo};
use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// 素材准备
// ---------------------------------------------------------------------------

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// 随包 ffmpeg/ffprobe（**不用**系统 PATH 里的其它 ffmpeg）
fn bundled_bin(name: &str) -> Option<PathBuf> {
    let p = manifest_dir()
        .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
        .join("bin")
        .join(name);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

fn asset_dir() -> PathBuf {
    let d = std::env::temp_dir().join("cap-e2e");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// 用随包 ffmpeg 生成 4 秒 320x240@30fps、每 1 秒一个关键帧的素材。
/// `Ok(None)` 表示环境不具备（跳过），`Err` 表示真的生成失败（测试应当失败）。
fn generate(tag: &str, frag: bool) -> Result<Option<PathBuf>, String> {
    let Some(ff) = bundled_bin("ffmpeg.exe") else {
        return Ok(None);
    };
    let out = asset_dir().join(format!("e2e-{tag}.mp4"));
    let _ = std::fs::remove_file(&out);
    let mut cmd = Command::new(&ff);
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
    let output = cmd
        .output()
        .map_err(|e| format!("无法运行随包 ffmpeg（{}）：{e}", ff.display()))?;
    if output.status.success() && out.is_file() {
        return Ok(Some(out));
    }
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    // openh264 动态库缺失时 ffmpeg 会报找不到编码器——属于"环境不具备"，跳过
    if err.contains("Unknown encoder") || err.to_ascii_lowercase().contains("openh264") {
        println!("[e2e] 随包 ffmpeg 无 libopenh264 编码能力，改用已有素材：{err}");
        return Ok(None);
    }
    Err(format!("ffmpeg 生成素材失败：{err}"))
}

/// 工作区里已有的录制产物（`_rust/smoke/*.plain.mp4`），作为素材兜底
fn fallback_asset() -> Option<PathBuf> {
    let dir = manifest_dir().join("..").join("_rust").join("smoke");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().ends_with(".plain.mp4"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// 拿到一个可用于导出的素材：优先现场生成，其次已有产物，最后跳过
fn sample_video(tag: &str) -> Option<PathBuf> {
    match generate(tag, false) {
        Ok(Some(p)) => Some(p),
        Ok(None) => {
            let fb = fallback_asset();
            if fb.is_none() {
                println!("[e2e] 跳过：既没有随包 ffmpeg 编码能力，也没有 _rust/smoke/*.plain.mp4 素材");
            }
            fb
        }
        Err(e) => panic!("{e}"),
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑：吸附语义（不依赖任何素材，任何环境都会真跑）
// ---------------------------------------------------------------------------

fn synthetic(duration_ms: i64, keyframes_ms: Vec<i64>, fps: f64) -> Clip {
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
        // 合成素材的表就是最终态（P34 的"还在扫"中间态不适用于这里）
        keyframes_scanned: true,
        // 干净素材：没有坏段 → 导出行为与改造前一字不差
        health: edit::health::HealthReport::default(),
    }
}

#[test]
fn snap_semantics_start_next_keyframe_end_previous() {
    let clip = synthetic(10_000, vec![0, 1000, 2000, 3000], 30.0);
    // 起点：≥ t 的最近关键帧（后一个关键帧）；半帧容差内允许"早不到一帧"的关键帧入选
    assert_eq!(edit::snap_start(&clip, 0), 0);
    assert_eq!(edit::snap_start(&clip, 1), 0, "1 ms 在 0 的半帧容差内");
    assert_eq!(edit::snap_start(&clip, 20), 1000, "超过半帧要往后找");
    assert_eq!(edit::snap_start(&clip, 1000), 1000);
    assert_eq!(edit::snap_start(&clip, 1500), 2000);
    // 终点：≤ t 的最近关键帧（前一个关键帧）
    assert_eq!(edit::snap_end(&clip, 2500), 2000);
    assert_eq!(edit::snap_end(&clip, 2001), 2000);
    assert_eq!(edit::snap_end(&clip, 1500), 1000);
    // 超范围夹紧
    assert_eq!(edit::snap_start(&clip, -9999), 0);
    assert_eq!(edit::snap_end(&clip, -9999), 0);
    assert_eq!(edit::snap_start(&clip, 99_999), 3000);
    assert_eq!(edit::snap_end(&clip, 99_999), 3000);
    // start >= end 必须报错
    assert!(edit::snap_range(&clip, 1200, 2400).is_err());
    assert_eq!(edit::snap_range(&clip, 100, 4500).unwrap(), (1000, 3000));
}

#[test]
fn snap_half_frame_tolerance_covers_269999() {
    // 90 kHz 时基下"3 秒关键帧"的 pts 是 269999（= 2999.99 ms），毫秒化会得到 2999
    let clip = synthetic(6_000, vec![0, 2999, 5999], 30.0);
    assert_eq!(
        edit::snap_start(&clip, 3000),
        2999,
        "半帧容差内不应跳过这个 GOP"
    );
    assert_eq!(edit::snap_end(&clip, 3000), 2999);
    // 超过半帧（30 fps 下 16.67 ms）就不再容忍
    assert_eq!(edit::snap_start(&clip, 3030), 5999);
    assert_eq!(edit::snap_end(&clip, 2980), 0);
    // 帧率未知时按 1/60 秒估帧周期（容差 ≈ 8.33 ms）
    let unknown = synthetic(6_000, vec![0], 0.0);
    assert!((unknown.frame_period_ms() - 1000.0 / 60.0).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 端到端：copy 导出
// ---------------------------------------------------------------------------

/// 挑一段"两端都正好落在关键帧、且终点不在文件末尾"的区间，
/// 避免触发 `effective_range` 的"终点拖到末尾就取媒体末尾"分支，让断言可以写死。
fn kf_span(clip: &Clip) -> Option<(i64, i64)> {
    let kfs = &clip.keyframes_ms;
    if kfs.len() < 2 {
        return None;
    }
    let tol = clip.snap_tolerance_ms();
    let mut b = kfs.len() - 1;
    while b > 0 && (kfs[b] as f64) + tol >= clip.duration_ms as f64 {
        b -= 1;
    }
    if b == 0 {
        return None;
    }
    let a = b / 2;
    if kfs[a] >= kfs[b] {
        return None;
    }
    Some((kfs[a], kfs[b]))
}

#[test]
fn export_copy_keeps_resolution_and_correct_duration() {
    let Some(src) = sample_video("plain") else {
        return;
    };
    let clip = edit::open(&src).expect("open 应成功");
    assert!(clip.video.is_some(), "素材应有视频流");
    let Some((start, end)) = kf_span(&clip) else {
        println!("[e2e] 跳过：素材关键帧太少，无法取合法区间");
        return;
    };

    let dst = asset_dir().join(format!(
        "e2e-out-{}.mp4",
        src.file_stem().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&dst);
    let rep = edit::export_copy(&clip, start, end, &dst).expect("copy 导出应成功");

    // ---- 分辨率不变（copy 的核心保证）----
    assert!(
        rep.same_resolution,
        "分辨率必须与源一致：源 {}x{} / 产物 {}x{}",
        rep.src_width,
        rep.src_height,
        rep.dst_width,
        rep.dst_height
    );
    assert_eq!(
        (rep.dst_width, rep.dst_height),
        (rep.src_width, rep.src_height)
    );
    assert!(rep.is_bitstream_copy(), "必须是位流拷贝（未重新编码）");

    // ---- 吸附结果 ----
    assert_eq!(rep.start_ms, start, "起点应吸附到请求的关键帧");
    assert_eq!(rep.end_ms, end, "终点应吸附到请求的关键帧");
    assert!(rep.bytes > 0 && rep.packets > 0);
    assert!(dst.is_file(), "产物必须真的落盘");

    // ---- 时长正确：先用报告，再用随包 ffprobe 独立复核 ----
    let want = rep.duration_ms();
    assert!(
        (rep.dst_duration_ms - want).abs() <= 250,
        "产物时长应约 {want} ms，实际 {}",
        rep.dst_duration_ms
    );
    if let Some(probe) = bundled_bin("ffprobe.exe") {
        let v = Command::new(&probe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
            ])
            .arg(&dst)
            .output()
            .expect("ffprobe 应能运行");
        assert!(
            v.status.success(),
            "ffprobe 解析产物失败：{}",
            String::from_utf8_lossy(&v.stderr)
        );
        let res = String::from_utf8_lossy(&v.stdout);
        let first = res.lines().next().unwrap_or("").trim().to_string();
        assert_eq!(
            first,
            format!("{},{}", rep.src_width, rep.src_height),
            "ffprobe 读出的产物分辨率应与源一致"
        );

        let d = Command::new(&probe)
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(&dst)
            .output()
            .expect("ffprobe 应能运行");
        let secs: f64 = String::from_utf8_lossy(&d.stdout)
            .trim()
            .parse()
            .expect("ffprobe 应给出可解析的 duration");
        let ms = (secs * 1000.0).round() as i64;
        assert!(
            (ms - want).abs() <= 250,
            "ffprobe 读出的产物时长应约 {want} ms，实际 {ms} ms"
        );
    } else {
        println!("[e2e] 没有随包 ffprobe，跳过外部复核（已用 caplite::mux::probe 自检）");
    }
}

#[test]
fn export_copy_rejects_invalid_selection() {
    let Some(src) = sample_video("empty") else {
        return;
    };
    let clip = edit::open(&src).expect("open 应成功");
    let dst = asset_dir().join("e2e-empty-out.mp4");
    let _ = std::fs::remove_file(&dst);
    // 起点取到文件尾、终点取 0 → 吸附后必然 start >= end
    assert!(edit::export_copy(&clip, clip.duration_ms, 0, &dst).is_err());
    assert!(!dst.exists(), "报错时不应留下产物");
}

#[test]
fn fragmented_mp4_can_be_opened() {
    // 先试现场生成分片 mp4；生成不了就用已有素材（已有素材本身就是分片录制产物）
    let src = match generate("frag", true) {
        Ok(Some(p)) => Some(p),
        Ok(None) => fallback_asset(),
        Err(e) => {
            println!("[e2e] 生成分片素材失败，改用已有素材：{e}");
            fallback_asset()
        }
    };
    let Some(src) = src else {
        println!("[e2e] 跳过：没有可用的分片 mp4 素材");
        return;
    };
    let clip = edit::open(&src).expect("分片 mp4 必须能被 open");
    assert!(clip.video.is_some(), "分片文件也应识别出视频流");
    assert!(clip.duration_ms > 0, "分片文件时长应 > 0");
    assert!(!clip.keyframes_ms.is_empty(), "分片文件也应扫到关键帧");
}

#[test]
fn thumbnail_returns_rgba_within_bounds() {
    let Some(src) = sample_video("thumb") else {
        return;
    };
    let clip = edit::open(&src).expect("open 应成功");
    let at = (clip.duration_ms / 3).max(0);
    let (w, h, rgba) = edit::thumbnail(&src, at, 96, 96).expect("缩略图应成功");
    assert!(w <= 96 && h <= 96, "不得超过上限：实际 {w}x{h}");
    assert_eq!(rgba.len(), (w * h * 4) as usize, "必须是 RGBA8");
    // 缩略图不能全是同一个像素（纯色说明解码/缩放坏了）
    let first = &rgba[..4];
    assert!(
        rgba.as_chunks::<4>().0.iter().any(|p| p != first),
        "缩略图不应是纯色"
    );
}

#[test]
fn existing_recording_artifacts_are_readable() {
    // 直接拿工作区里已有的录制产物过一遍 open + 关键帧扫描（没有就跳过）
    let Some(src) = fallback_asset() else {
        println!("[e2e] 跳过：_rust/smoke 下没有 *.plain.mp4");
        return;
    };
    let clip = edit::open(&src).expect("已有录制产物应能 open");
    assert!(clip.duration_ms > 0);
    if let Some(v) = &clip.video {
        assert!(v.width > 0 && v.height > 0, "应读出分辨率");
        assert!(!clip.keyframes_ms.is_empty(), "h264 录制产物应有 IDR 关键帧");
    }
}
