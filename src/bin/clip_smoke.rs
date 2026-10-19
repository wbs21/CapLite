//! 剪辑烟雾测试（给 `scripts/regression.ps1` 调用）。
//!
//! ```text
//! clip_smoke <源.mp4> <起点> <终点> [产物.mp4]
//! ```
//! * 起点/终点支持 `1200`（毫秒）、`1.2s`（秒）、`1200ms`；
//! * 走 `caplite::edit::export_copy`：**关键帧吸附 + copy 导出（不重新编码）**，
//!   并且导出后会自己 probe 产物校验分辨率与时长；
//! * 把结果按 `KEY=VALUE` 打印出来，方便 PowerShell 正则解析；
//! * 成功 `CLIP_RESULT=PASS` 且退出码 0，失败 `CLIP_RESULT=FAIL` 且退出码 1。

use caplite::edit;
use std::path::PathBuf;
use std::process::ExitCode;

/// 解析时间参数：`1200` / `1.2s` / `1200ms`
fn parse_time(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let v = if let Some(rest) = s.strip_suffix("ms") {
        rest.parse::<f64>().map(|f| f.round() as i64)
    } else if let Some(rest) = s.strip_suffix('s') {
        rest.parse::<f64>().map(|f| (f * 1000.0).round() as i64)
    } else {
        s.parse::<f64>().map(|f| f.round() as i64)
    };
    v.map_err(|e| format!("无法解析时间“{s}”：{e}"))
}

fn usage() {
    eprintln!("用法：clip_smoke <源.mp4> <起点> <终点> [产物.mp4]");
    eprintln!("      时间支持 1200（毫秒）/ 1.2s（秒）/ 1200ms");
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        usage();
        return ExitCode::from(2);
    }
    let src = PathBuf::from(&args[0]);
    let start_ms = match parse_time(&args[1]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            usage();
            return ExitCode::from(2);
        }
    };
    let end_ms = match parse_time(&args[2]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            usage();
            return ExitCode::from(2);
        }
    };
    let dst = args.get(3).map(PathBuf::from).unwrap_or_else(|| {
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "clip".into());
        src.with_file_name(format!("{stem}-clip.mp4"))
    });

    let clip = match edit::open(&src) {
        Ok(c) => c,
        Err(e) => {
            println!("CLIP_RESULT=FAIL");
            println!("CLIP_ERROR={e}");
            eprintln!("[clip] 打开失败：{e}");
            return ExitCode::from(1);
        }
    };
    let v = clip.video.as_ref();
    println!("[clip] 源：{}", clip.src.display());
    println!(
        "[clip] 时长 {} ms，视频 {}x{} @ {:.2} fps，音频 {}，关键帧 {} 个：{:?}",
        clip.duration_ms,
        v.map(|v| v.width).unwrap_or(0),
        v.map(|v| v.height).unwrap_or(0),
        v.map(|v| v.fps).unwrap_or(0.0),
        if clip.audio.is_some() { "有" } else { "无" },
        clip.keyframes_ms.len(),
        clip.keyframes_ms
    );

    println!("CLIP_SRC={}", clip.src.display());
    println!("CLIP_SRC_DURATION_MS={}", clip.duration_ms);
    println!("CLIP_KEYFRAMES={}", clip.keyframes_ms.len());
    // ---- P35：编码体检（快速体检随 open 一起跑；深度检查不在这个工具里）----
    let hard_n = clip.health.hard_ranges().len();
    println!("CLIP_HEALTH_BAD={}", clip.health.bad.len());
    println!("CLIP_HEALTH_HARD={hard_n}");
    println!(
        "CLIP_HEALTH_TRUNCATED_AT_MS={}",
        clip.health
            .truncated_at_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("CLIP_HEALTH_FULL_DECODE={}", clip.health.full_decode);
    println!("CLIP_HEALTH_PACKETS={}", clip.health.packets);
    for b in &clip.health.bad {
        println!(
            "CLIP_BAD kind={} hard={} start={} end={} detail={}",
            b.kind.key(),
            b.kind.is_hard(),
            b.start_ms,
            b.end_ms,
            b.detail
        );
    }
    if clip.health.is_clean() {
        println!("[clip] 编码体检：未发现异常");
    } else {
        println!(
            "[clip] 编码体检：{} 处异常，其中 {} 处会跳过/剔除（硬错误）",
            clip.health.bad.len(),
            hard_n
        );
        for b in &clip.health.bad {
            println!(
                "[clip]   {} {} – {}（{}）{}",
                b.kind.key(),
                b.start_ms,
                b.end_ms,
                if b.kind.is_hard() { "跳过" } else { "仅提示" },
                b.detail
            );
        }
    }
    println!("CLIP_REQUESTED_START_MS={start_ms}");
    println!("CLIP_REQUESTED_END_MS={end_ms}");

    let rep = match edit::export_copy(&clip, start_ms, end_ms, &dst) {
        Ok(r) => r,
        Err(e) => {
            println!("CLIP_RESULT=FAIL");
            println!("CLIP_ERROR={e}");
            eprintln!("[clip] 导出失败：{e}");
            return ExitCode::from(1);
        }
    };

    println!("[clip] 产物：{}", rep.dst.display());
    println!(
        "[clip] 吸附后区间 {}+{} ms（实际 {} ms），包 {}，字节 {}",
        rep.start_ms,
        rep.duration_ms(),
        rep.dst_duration_ms,
        rep.packets,
        rep.bytes
    );
    println!(
        "[clip] 分辨率：源 {}x{} / 产物 {}x{} → {}",
        rep.src_width,
        rep.src_height,
        rep.dst_width,
        rep.dst_height,
        if rep.same_resolution { "一致" } else { "不一致" }
    );

    println!("CLIP_DST={}", rep.dst.display());
    println!("CLIP_SNAP_START_MS={}", rep.start_ms);
    println!("CLIP_SNAP_END_MS={}", rep.end_ms);
    println!("CLIP_SELECTION_MS={}", rep.duration_ms());
    println!("CLIP_DST_DURATION_MS={}", rep.dst_duration_ms);
    // P35：因编码错误被剔除掉的时长（干净文件恒为 0）
    println!("CLIP_SKIPPED_BAD_MS={}", rep.skipped_bad_ms);
    println!("CLIP_SEGMENTS={}", rep.segments);
    if rep.skipped_bad_ms > 0 {
        println!(
            "[clip] 已跳过约 {} 秒编码错误内容（产物比选区短）",
            (rep.skipped_bad_ms as f64 / 1000.0).round()
        );
    }
    println!("CLIP_PACKETS={}", rep.packets);
    println!("CLIP_BYTES={}", rep.bytes);
    println!("CLIP_SRC_WIDTH={}", rep.src_width);
    println!("CLIP_SRC_HEIGHT={}", rep.src_height);
    println!("CLIP_DST_WIDTH={}", rep.dst_width);
    println!("CLIP_DST_HEIGHT={}", rep.dst_height);
    println!(
        "CLIP_SAME_RESOLUTION={}",
        if rep.same_resolution { "true" } else { "false" }
    );
    println!("CLIP_BITSTREAM_COPY={}", if rep.is_bitstream_copy() { "true" } else { "false" });
    println!("CLIP_RESULT=PASS");
    ExitCode::SUCCESS
}
