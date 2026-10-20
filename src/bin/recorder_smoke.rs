//! 录制编排烟雾测试：走 `Recorder` 正式四线程路径。
//!
//! ```text
//! recorder_smoke [秒数] [输出.mp4] [--audio-only|--video-only] [--crash] [--gop N] [--fps N]
//! ```
//! * 默认录"画面+声音"，到时间后 `stop()`（会导出普通 MP4 并打印统计）。
//! * `--crash`：录到一半直接 `std::process::abort()`，用于验证"进程被杀后分片 MP4 仍可播"
//!   （分片写入每秒 flush，正是为此设计）。
//! * `--fps N`：目标帧率（30/60；>60 会被录制侧夹到 60）。给了这个参数就**严格核对**
//!   实际编码帧数是否 ≈ 帧率 × 时长（±5%），不符时退出码 3 —— 这是"按目标帧率抽帧"的
//!   可验证证据（高刷新率屏幕上多出来的帧被舍弃，见 `superseded_frames`）。

use caplite::config::{CaptureMode, Config, ContentMode, Region};
use caplite::recorder::{Recorder, RecorderState, MAX_RECORD_FPS};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secs: f64 = args
        .iter()
        .find_map(|a| a.parse::<f64>().ok())
        .unwrap_or(5.0);
    let crash = args.iter().any(|a| a == "--crash");
    let content = if args.iter().any(|a| a == "--audio-only") {
        ContentMode::AudioOnly
    } else if args.iter().any(|a| a == "--video-only") {
        ContentMode::VideoOnly
    } else {
        ContentMode::VideoAndAudio
    };
    // 关键帧间隔（秒）：分片 MP4 只在关键帧处落盘，所以它同时决定"崩溃最多丢多久"
    let gop: u32 = args
        .iter()
        .position(|a| a == "--gop")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    // 音源：默认**只录系统声音**（保持既有回归步骤的行为不变）。
    // `--audio mic` 只录麦克风、`--audio both` 两个都录（用于覆盖麦克风链路）。
    let audio_arg = args
        .iter()
        .position(|a| a == "--audio")
        .and_then(|i| args.get(i + 1))
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_else(|| "system".to_string());
    let (audio_system, audio_mic) = match audio_arg.as_str() {
        "mic" => (false, true),
        "both" => (true, true),
        "none" => (false, false),
        _ => (true, false),
    };
    // 指定一个（可能不存在的）麦克风设备：用来验证"设备打不开时给出明确错误"
    let mic_device_id: Option<String> = args
        .iter()
        .position(|a| a == "--mic-device")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // 目标帧率：显式给了 --fps 就进入"严格核对帧数"模式（见文件头）
    let fps_arg: Option<u32> = args
        .iter()
        .position(|a| a == "--fps")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    // 参数里的 `*.mp4` 是**导出产物**的落点；流文件按产品的目录分工放到同级的 `Cache\` 下
    //（同名同基名）。这样回归脚本可以同时断言"导出在输出目录"与"流文件保留在 Cache"。
    let export_path: PathBuf = args
        .iter()
        .find(|a| a.ends_with(".mp4"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("_rust/smoke/rec.mp4"));
    let out_dir = export_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let cache_dir = out_dir.join("Cache");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("建 Cache 目录失败 {}：{e}", cache_dir.display()))?;
    let streaming: PathBuf = cache_dir.join(
        export_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "rec.mp4".into()),
    );
    // smoke 工具是手工回归入口：重复运行前清理旧的固定流文件，避免
    // Recorder::start 的 create_new 预占被上一次测试产物挡住。
    let _ = std::fs::remove_file(&streaming);

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();
    ffmpeg_next::init()?;

    let mut cfg = Config {
        capture_mode: CaptureMode::FullScreen,
        monitor_index: 0,
        region: Region::default(),
        content_mode: content,
        draw_cursor: true,
        audio_system,
        audio_mic,
        mic_device_id,
        // 导出产物落到参数指定的目录（与产品一致：导出进输出目录、流文件进同级 Cache）
        output_dir: out_dir.to_string_lossy().to_string(),
        fps: fps_arg.unwrap_or(30),
        video_bitrate_kbps: 8000,
        audio_bitrate_kbps: 192,
        ..Config::default()
    }
    .sanitized();
    // 关键帧间隔**只作为测试覆盖**：产品 UI 已经没有这一项，`sanitized()` 会把字段归一到 0
    // （自动），所以这里必须在 sanitized() **之后**再写，才能构造出"每 N 秒一个关键帧"
    // 这种可预期的素材（回归脚本第 3 步要用）。`--gop 0` = 走产品的自动策略。
    cfg.keyframe_interval_sec = gop;
    println!(
        "[rec] 目标帧率 {} fps（录制侧上限 {} fps）；关键帧间隔 = {}",
        cfg.fps,
        MAX_RECORD_FPS,
        if cfg.keyframe_interval_sec == 0 {
            "自动（0）".to_string()
        } else {
            format!("{} 秒（测试覆盖）", cfg.keyframe_interval_sec)
        }
    );

    let t0 = Instant::now();
    let mut rec = Recorder::start(&cfg, &streaming)?;
    // **会话就绪耗时**：从进程里"准备开始录制"到会话真的就绪（采集会话 + OpenH264 +
    // 音源 + AAC 全建好）。这段时间**一帧画面都没有** —— 它就是"我说录 5 秒、产物却只有
    // 759 / 2013 / 2772 / 3564 ms"的头部来源：下面的 `secs` 秒预算如果按 `t0`（进程里开始
    // 准备的那一刻）计时，机器很忙时（实测 0.46~6.0 s）就会被它吃掉一大截。
    let start_ms = t0.elapsed().as_millis() as u64;
    println!(
        "[rec] 已启动：{} 秒后{}（输出 {}）",
        secs,
        if crash { "强杀进程" } else { "正常停止" },
        streaming.display()
    );
    println!(
        "[rec] 会话就绪耗时 {} ms（设备/编码器/音源初始化；这段时间没有画面，**不计入**下面要录的 {} 秒）",
        start_ms, secs
    );

    // 录制中定期打印进度，同时验证句柄是非阻塞的。
    // `--crash` 的 abort 检查**每轮都做**（不能放在"每 1 秒打印一次"的闸门里）：
    // 否则当循环恰好在 abort 窗口外退出时，代码会走到 `stop()` 正常收尾并**导出**
    // 一个普通 MP4 —— 回归脚本第 2 步"导出目录不应出现同名文件"就会偶发失败。
    //
    // `t_loop`：**录制循环真正开始**的时刻，也就是"要录的这几秒"的计时起点。
    // **计时起点必须是它、不能是 `t0`**：`t0` 那一刻采集/编码器还没就绪，把它算进"录 5 秒"
    // 会让产物时长 = 5 s − 会话就绪耗时。机器空闲时这个差只有 ~50 ms（所以历史上看着"健康"），
    // 但机器一忙（后台编译 FFmpeg、磁盘打满）它会涨到 1.4~4.3 s，于是同一句
    // `recorder_smoke 5` 录出来的媒体就只有 759~3564 ms —— **那不是丢尾，是"说好的 5 秒
    // 被启动耗时吃掉了"**。见下方 `REC_START_MS` / `REC_WALL_MS` / `REC_TAIL_GAP_MS`。
    let t_loop = Instant::now();
    let budget = Duration::from_secs_f64(secs);
    let mut last = Instant::now();
    while t_loop.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(100));
        if crash && t_loop.elapsed() >= budget.saturating_sub(Duration::from_millis(200)) {
            // 模拟崩溃：不做任何收尾，直接杀
            println!("[rec] 立刻 abort（模拟崩溃），分片文件应仍可播放");
            std::process::abort();
        }
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            let s = rec.stats();
            // 分片文件的**已落盘**大小：分片只在关键帧处完整落盘，这个数直接反映崩溃能保住多少
            let on_disk = std::fs::metadata(rec.output())
                .map(|m| m.len())
                .unwrap_or(0);
            println!(
                "[rec] t={:.1}s（录制窗口；进程 t0 起 {:.1}s）状态={:?} 视频帧={} 丢帧={} 关键帧={} 音频帧={} 静音补齐={} 被舍弃(合并)={} 节拍重同步={} 末帧pts={}ms 已落盘={} KB",
                t_loop.elapsed().as_secs_f64(),
                t0.elapsed().as_secs_f64(),
                rec.state(),
                s.video_frames,
                s.video_dropped,
                s.keyframes,
                s.audio_frames,
                s.silence_padded_frames,
                s.superseded_frames,
                s.pacing_resyncs,
                s.last_video_pts_ms,
                on_disk / 1024
            );
            if let RecorderState::Failed(e) = rec.state() {
                println!("[rec] 录制中失败：{e}");
            }
        }
    }

    let t_stop = Instant::now();
    let result = rec.stop()?;
    // **真实录制窗口**：录制循环开始 → 停止就绪（stop 把停止标志交给采集/封装线程为止）。
    // 这就是"脚本墙钟"里该被拿掉的那部分（exe 冷启动 + 设备初始化 + 导出收尾都不算进来），
    // 也是产物音频时长应当对齐的量。
    let rec_wall_ms = t_stop.duration_since(t_loop).as_millis() as u64;
    println!(
        "[rec] 停止耗时 {:?}，录制时长 {:?}（录制窗口 {} ms；会话就绪 {start_ms} ms）",
        t_stop.elapsed(),
        result.duration,
        rec_wall_ms
    );
    println!(
        "[rec] 临时流文件（Cache）：{}（导出后**仍然保留**，可找回）",
        result.streaming.display()
    );
    println!(
        "[rec] 导出文件（输出目录）：{}",
        if result.exported.as_os_str().is_empty() {
            "（导出失败，Cache 里的流文件可直接播放）".to_string()
        } else {
            result.exported.display().to_string()
        }
    );
    let s = result.stats;
    // 音源降级警告要**打印出来**（录不到声音时这是唯一线索）
    let g_audio_warnings = rec.audio_warnings().join("；");
    if !rec.audio_warnings().is_empty() {
        println!("[rec] 警告：音源降级 → {g_audio_warnings}");
    }
    println!(
        "[rec] 统计：视频 {} 帧（丢 {}、关键帧 {}、被舍弃/合并 {}、节拍重同步 {}），音频 {} 帧，无变化 {}，会话重建 {}，静音补齐 {} 帧（{:.1}s），音频断供重建 {} 次、空窗累计 {} ms（最长 {} ms）",
        s.video_frames,
        s.video_dropped,
        s.keyframes,
        s.superseded_frames,
        s.pacing_resyncs,
        s.audio_frames,
        s.no_change,
        s.access_lost,
        s.silence_padded_frames,
        s.silence_padded_frames as f64 / 48000.0,
        s.audio_rebuilds,
        s.audio_dryout_ms,
        s.audio_max_dryout_ms
    );
    let wall = result.duration.as_secs_f64();
    let measured = s.video_frames as f64 / wall.max(1e-9);
    println!(
        "[rec] 实测：墙钟 {:.3}s，编码 {} 帧 → 实测 {:.2} fps（目标 {} fps）",
        wall, s.video_frames, measured, cfg.fps
    );

    // 先把两个产物各探测一次（下面的人读明细与结构化对账都用这两个数）
    for (label, p) in [
        ("Cache 里的临时流文件", &result.streaming),
        ("输出目录里的导出文件", &result.exported),
    ] {
        if p.as_os_str().is_empty() {
            continue;
        }
        if !p.is_file() {
            println!("[rec] {label} {}：不存在（导出可能失败）", p.display());
            continue;
        }
        match caplite::mux::probe(p) {
            Ok(info) => {
                println!(
                    "[rec] {label} {}：时长 {} ms，{} 个流",
                    p.display(),
                    info.duration_ms,
                    info.streams.len()
                );
                for st in &info.streams {
                    println!(
                        "        流#{} {} codec={} {}x{} {}Hz {}ch {} ms",
                        st.index,
                        st.kind,
                        st.codec,
                        st.width,
                        st.height,
                        st.sample_rate,
                        st.channels,
                        st.duration_ms
                    );
                }
            }
            Err(e) => println!("[rec] {label} 解析失败: {e}"),
        }
    }

    // -----------------------------------------------------------------------
    // 导出完整性：**客观证据 + 硬判定**
    //
    // 这一节要回答的问题只有一个：**产物到底是"流文件本身就短"，还是"重封装把它截断了"？**
    // 判别方法就是把两个时长并列：`stream_ms`（Cache 流文件）与 `export_ms`（导出产物）。
    //   * 两者一致、且都贴着 `REC_WALL_MS` → 录制/导出都是忠实的（短只可能来自"窗口本身短"）；
    //   * 流文件长、产物短 → **重封装静默截断**（本轮已改成返回错误，走"保留流文件 + 告警"）；
    //   * 流文件本身就比 `REC_WALL_MS` 短很多 → **捕获侧丢尾**（`REC_LAST_PTS_MS` 会一起偏小）。
    // 任一条不通过都以退出码 4 结束：**不允许"成功但明显短"**。
    // -----------------------------------------------------------------------
    let probe_ms = |p: &std::path::Path| -> i64 {
        if p.as_os_str().is_empty() || !p.is_file() {
            return -1;
        }
        caplite::mux::probe(p).map(|i| i.duration_ms).unwrap_or(-1)
    };
    let stream_ms = probe_ms(&result.streaming);
    let export_ms = probe_ms(&result.exported);
    let export_delta_ms = if stream_ms >= 0 && export_ms >= 0 {
        (export_ms - stream_ms).abs()
    } else {
        -1
    };
    // 尾部空档：**流文件时长 − 录制窗口**。负数且绝对值大 = 尾部丢了（正面证据）。
    let tail_gap_ms = if stream_ms >= 0 {
        stream_ms - rec_wall_ms as i64
    } else {
        0
    };
    // 视频录制用 300 ms（≈9 帧 @30fps）判定"媒体明显短于录制窗口"；
    // 纯音频时容器时长按音频帧（1024 样本 = 21.3 ms）量化，放宽到 500 ms
    //（与回归脚本第 5 步对音频时长的容差同口径）。
    let window_tol_ms: i64 = if cfg.content_mode.has_video() { 300 } else { 500 };
    let export_present = !result.exported.as_os_str().is_empty() && result.exported.is_file();
    let mut issue = String::new();
    if !export_present {
        issue = "EXPORT_MISSING".into();
    } else if export_delta_ms < 0 {
        issue = "EXPORT_UNPROBED".into();
    } else if export_delta_ms > caplite::mux::EXPORT_DURATION_TOLERANCE_MS {
        issue = "EXPORT_TRUNCATED".into();
    } else if stream_ms >= 0 && (rec_wall_ms as i64 - stream_ms) > window_tol_ms {
        // 导出与流文件一致，但两者都比录制窗口短得多 → 捕获侧（或录制窗口）问题
        issue = "MEDIA_SHORTER_THAN_WINDOW".into();
    }

    // ---- 给回归脚本的**结构化**输出（KEY=VALUE，纯 ASCII）----
    //
    // 为什么不复用上面的自然语言行：`[rec] 统计：…音频 N 帧…` 是 UTF-8 中文，
    // 而 Windows PowerShell 5.1 用 `Process.StandardOutput.ReadToEnd()` 读子进程管道时
    // 按 **[Console]::OutputEncoding** 解码。该编码是**从父进程继承**的：
    // 父进程是「chcp 65001 + [Console]::OutputEncoding=gb2312」这种组合时（DSH 里很常见），
    // 子进程输出的 UTF-8 字节会被按 GBK 解成乱码（「音频」→「闊抽」），
    // 脚本里 `'音频\s+(\d+)\s+帧'` 这类正则就永远匹配不上。
    // （实测：子进程 Console.OutputEncoding=gb2312 时 regex match=False；
    //   =utf-8 时 match=True。见回归第 8 步的复现记录。）
    // 因此这里额外打印一份**纯 ASCII** 的 KEY=VALUE，任何代码页下都能稳定解析。
    println!("REC_CONTENT_MODE={:?}", cfg.content_mode);
    println!("REC_AUDIO_REQUESTED={}", cfg.content_mode.has_audio() && cfg.audio_system);
    // **录制循环真正开始→停止就绪**的墙钟毫秒（不含 exe 冷启动 / 设备初始化 / 导出收尾）。
    // 回归第 5 步用它替代"脚本自己测的墙钟"来断言产物音频时长。
    println!("REC_WALL_MS={rec_wall_ms}");
    // 会话就绪耗时（采集+编码器+音源初始化）：**这段时间没有画面**，
    // 是"录 5 秒只得 759 ms"这类现象的头部来源（判据：媒体时长 ≈ REC_WALL_MS，与它无关）。
    println!("REC_START_MS={start_ms}");
    println!("REC_LAST_PTS_MS={}", s.last_video_pts_ms);
    println!("REC_AUDIO_FRAMES={}", s.audio_frames);
println!("REC_AUDIO_DRYOUT_MS={}", s.audio_dryout_ms);
println!("REC_AUDIO_MAX_DRYOUT_MS={}", s.audio_max_dryout_ms);
println!("REC_AUDIO_REBUILDS={}", s.audio_rebuilds);
    println!("REC_VIDEO_FRAMES={}", s.video_frames);
    println!("REC_VIDEO_DROPPED={}", s.video_dropped);
    println!("REC_PACING_RESYNCS={}", s.pacing_resyncs);
    println!("REC_KEYFRAMES={}", s.keyframes);
    println!("REC_SILENCE_PADDED_FRAMES={}", s.silence_padded_frames);
    println!("REC_AUDIO_WARNINGS={}", g_audio_warnings);
    // ---- 导出/流文件对账（产物 vs 流文件 vs 录制窗口）----
    println!("REC_STREAM_MS={stream_ms}");
    println!("REC_EXPORT_MS={export_ms}");
    println!("REC_EXPORT_DELTA_MS={export_delta_ms}");
    println!("REC_TAIL_GAP_MS={tail_gap_ms}");
    println!("REC_EXPORT_OK={}", issue.is_empty());
    println!("REC_EXPORT_ISSUE={issue}");
    match &result.export {
        Some(r) => {
            println!("REC_EXPORT_PACKETS_READ={}", r.packets_read);
            println!("REC_EXPORT_PACKETS_WRITTEN={}", r.packets_written);
            println!("REC_EXPORT_VIDEO_READ={}", r.video_read);
            println!("REC_EXPORT_VIDEO_WRITTEN={}", r.video_written);
            println!("REC_EXPORT_AUDIO_READ={}", r.audio_read);
            println!("REC_EXPORT_AUDIO_WRITTEN={}", r.audio_written);
            println!("REC_EXPORT_READ_ERRORS={}", r.read_errors);
            println!("REC_EXPORT_READ_RETRIES={}", r.read_retries);
            println!("REC_EXPORT_WRITE_ERRORS={}", r.write_errors);
            println!("REC_EXPORT_EXITED_EARLY={}", r.exited_early);
            println!("REC_EXPORT_EXIT_REASON={}", r.exit_reason);
            println!("REC_EXPORT_FIRST_PTS_MS={}", r.first_pts_ms);
            println!("REC_EXPORT_LAST_PTS_MS={}", r.last_pts_ms);
        }
        None => {
            println!("REC_EXPORT_PACKETS_READ=-1");
            println!("REC_EXPORT_PACKETS_WRITTEN=-1");
            println!("REC_EXPORT_VIDEO_READ=-1");
            println!("REC_EXPORT_VIDEO_WRITTEN=-1");
            println!("REC_EXPORT_AUDIO_READ=-1");
            println!("REC_EXPORT_AUDIO_WRITTEN=-1");
            println!("REC_EXPORT_READ_ERRORS=-1");
            println!("REC_EXPORT_READ_RETRIES=-1");
            println!("REC_EXPORT_WRITE_ERRORS=-1");
            println!("REC_EXPORT_EXITED_EARLY=-1");
            println!("REC_EXPORT_EXIT_REASON=none");
            println!("REC_EXPORT_FIRST_PTS_MS=-1");
            println!("REC_EXPORT_LAST_PTS_MS=-1");
        }
    }
    if let Some(msg) = &result.export_issue {
        println!("[rec] 导出不完整：{msg}");
    }
    println!(
        "[rec] 对账：流文件 {stream_ms} ms / 产物 {export_ms} ms（差 {export_delta_ms} ms，阈值 {} ms）；\
         录制窗口 {rec_wall_ms} ms，末帧 pts {} ms，尾部空档 {tail_gap_ms} ms；会话就绪 {start_ms} ms",
        caplite::mux::EXPORT_DURATION_TOLERANCE_MS,
        s.last_video_pts_ms
    );

    // 显式给了 --fps 才严格核对（回归脚本不带这个参数，避免机器忙时误报）
    let mut strict_fail = false;
    // **"录出无声文件"必须报错**：内容模式要求声音（仅声音 / 画面+声音）却一帧音频都没采到，
    // 说明音源没打开（设备被禁用/不存在）；这时**明确报错并以非 0 退出**，
    // 而不是交给用户一个"没声音的成功文件"。
    if cfg.content_mode.has_audio() && s.audio_frames == 0 {
        println!(
            "[rec] 错误：内容模式要求声音（{:?}），但本次录制采集到 0 帧音频 —— \
             音源可能被禁用或设备不存在（audio_warnings={}）",
            cfg.content_mode,
            g_audio_warnings
        );
        strict_fail = true;
    }
    if fps_arg.is_some() {
        let expected = cfg.fps as f64 * wall;
        let diff = (s.video_frames as f64 - expected) / expected.max(1.0);
        let ok = diff.abs() <= 0.05;
        println!(
            "[rec] 帧率核对：期望 {:.0} 帧（{} fps × {:.3}s），实际 {} 帧，偏差 {:+.2}% → {}",
            expected,
            cfg.fps,
            wall,
            s.video_frames,
            diff * 100.0,
            if ok { "PASS" } else { "FAIL" }
        );
        println!(
            "[rec] 抽帧证据：被舍弃/合并 {} 帧（DXGI AccumulatedFrames 之和），队列溢出丢帧 {}，节拍重同步 {}",
            s.superseded_frames, s.video_dropped, s.pacing_resyncs
        );
        strict_fail = !ok;
    }

    if strict_fail {
        // 只在显式给了 --fps 时把"帧数不符"当成失败（退出码 3）
        std::process::exit(3);
    }
    // **"成功但明显短"不允许存在**：导出缺失/比流文件短超过阈值，或媒体明显短于录制窗口，
    // 一律以退出码 4 结束（回归脚本会因此 FAIL —— 那正是我们要的"失败可见"）。
    if !issue.is_empty() {
        println!(
            "[rec] 错误：导出完整性校验不通过（{issue}）—— \
             流文件 {stream_ms} ms / 产物 {export_ms} ms / 录制窗口 {rec_wall_ms} ms / 会话就绪 {start_ms} ms；\
             Cache 里的流文件已保留，可直接播放或剪辑"
        );
        std::process::exit(4);
    }
    Ok(())
}
