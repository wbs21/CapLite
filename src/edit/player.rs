//! 剪辑页**播放**：一条常驻解码线程 + 有界帧队列。
//!
//! 为什么不复用 `thumbnail`：那条链是给"缩略图 / 胶片条"用的，有两处根本冲突 ——
//! * 它**每次调用都要重新 `avformat_open_input` + `find_stream_info` + seek**，
//!   按 30 fps 调就是每秒打开文件 30 次，播放必然一顿一顿；
//! * 它是**按关键帧量化**的（`thumb_keyframe`），画面只在关键帧上跳变 ——
//!   关键帧间隔 2 秒时，一秒里画面只换一次，那不能叫播放。
//!
//! 所以这里做的是"最小可用播放器"：**打开一次、顺序解码、按实时节流**。
//! 三条纪律：
//! 1. **绝不阻塞界面**：帧走 `sync_channel(8)`，界面落后就 `try_send` 失败 → **丢帧**
//!    （播放语义本来就该丢帧；为了"一帧都不丢"而让解码线程等界面，等于把卡顿传染给整条链）；
//! 2. **绝不让界面等后台**：界面侧只有 `try_recv`，从不 `recv`；
//! 3. **绝不 busy loop**：解到片尾就 `cmd.recv()` 睡死等指令（seek / stop），
//!    不设心跳、不空转。
//!
//! 解码器状态（`AVFormatContext` / `AVCodecContext` / `AVFrame` / `AVPacket`）全部用
//! `edit` 模块里既有的 RAII 守卫持有，任何提前返回都不会泄漏。

use super::{
    av_err, cpath, convert_to_rgba, limit_decoder_pixels, limit_input_probe, ts_to_ms, Codec,
    Frame, Input, Packet, AVERROR_EOF, AV_NOPTS_VALUE, AV_TIME_BASE,
};
use crate::error::AppError;
use ffmpeg_next::ffi as ff;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// 帧队列容量（帧）。
///
/// 8 帧 ≈ 1080p 下 8 × 8 MB ≈ 64 MB 的峰值 —— 够解码线程提前跑一点、又不至于把内存吃掉。
/// 满了就丢帧（见模块文档第 1 条）。
const QUEUE_CAP: usize = 8;

/// 帧队列的**字节预算**：4K 窗口下 8 帧 RGBA ≈ 264 MB，远超 1080p 的估算。
///
/// 按 `w*h*4` 算可排队帧数：1080p 仍是 8 帧（约 64 MB），4K 降到 2 帧；
/// 播放语义本来就允许丢帧，少缓冲几帧比把内存顶到几百 MB 好。
const QUEUE_BYTES_BUDGET: usize = 64 * 1024 * 1024;

fn queue_cap_for(max_w: u32, max_h: u32) -> usize {
    let bytes = (max_w.max(1) as usize)
        .saturating_mul(max_h.max(1) as usize)
        .saturating_mul(4);
    if bytes == 0 {
        return QUEUE_CAP;
    }
    (QUEUE_BYTES_BUDGET / bytes).clamp(2, QUEUE_CAP)
}
/// 解码线程最多领先墙钟多少毫秒。
///
/// 它同时是"启动时能多快出第一帧"的上限：200 ms 足够让界面在第一帧解码完之前不空转，
/// 又不至于让"按停止"之后还堆着好几帧没放完。
///
/// P54：值改为取自 [`crate::edit::DECODE_LEAD_MS`]（唯一来源，与音频解码线程共用）。
const LEAD_MS: i64 = crate::edit::DECODE_LEAD_MS;
/// 节流时单次睡眠的上限（毫秒）：睡太久会让 seek / stop 的响应变迟钝。
///
/// P54：值改为取自 [`crate::edit::DECODE_NAP_MAX_MS`]（唯一来源）。
const NAP_MAX_MS: u64 = crate::edit::DECODE_NAP_MAX_MS;

/// 解码线程吐给界面的一帧：RGBA8（已按 `max_w × max_h` 等比缩小）。
pub struct FrameRgba {
    /// 显示时刻（毫秒，源时基换算）
    pub pts_ms: i64,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// 播放线程的指令（只有两条：跳到某处、收工）。
enum Cmd {
    Seek(i64),
    Stop,
}

/// 界面侧持有的播放句柄。
///
/// 生命周期：`spawn` → 播放中反复 `try_recv` / `seek` → `stop`（或 `Drop`）。
/// 线程**不 join**：`Stop` 一发出线程最多 20 ms 内自己退出，
/// 而 `join` 会把这份等待搬到 UI 线程上 —— 那正是最不该等的地方。
pub struct PlayerHandle {
    cmd: Sender<Cmd>,
    frames: Receiver<FrameRgba>,
    eof: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    join: Option<JoinHandle<()>>,
}

impl PlayerHandle {
    /// 起一条播放线程，从 `from_ms` 开始解码（`max_w × max_h` 是解码尺寸上限）。
    ///
    /// **打开文件也在这条线程里**：调用方（UI）立刻拿到句柄，
    /// 打开失败之类的错误经 [`PlayerHandle::take_error`] 异步取回 —— 界面一次都不等磁盘。
    pub fn spawn(
        path: &Path,
        from_ms: i64,
        max_w: u32,
        max_h: u32,
    ) -> Result<Self, AppError> {
        Self::spawn_skipping(path, from_ms, max_w, max_h, &[])
    }

    /// 同 [`PlayerHandle::spawn`]，但带**要自动跳过的坏段**（P35）。
    ///
    /// `skip` 是 `(start_ms, end_ms)` 升序列表，应当只包含
    /// [`super::health::BadKind::is_hard`] 的段（用户口径：**仅硬错误自动跳**）。
    /// 播放一旦落进某个区间，就 `seek` 到它的 `end_ms` 继续 ——
    /// 既不解那一段的帧，也不会因为坏包把"读错"当成片尾而停下来。
    ///
    /// 传空切片时行为与 [`PlayerHandle::spawn`] 完全一致。
    pub fn spawn_skipping(
        path: &Path,
        from_ms: i64,
        max_w: u32,
        max_h: u32,
        skip: &[(i64, i64)],
    ) -> Result<Self, AppError> {
        let path = path.to_path_buf();
        let skip: Vec<(i64, i64)> = skip.to_vec();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (frame_tx, frame_rx) = sync_channel::<FrameRgba>(queue_cap_for(max_w, max_h));
        let eof = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None::<String>));
        let (eof_thread, err_thread) = (eof.clone(), error.clone());
        let join = thread::Builder::new()
            .name("clip-play".into())
            .spawn(move || {
                let r = decode_loop(
                    &path, from_ms, max_w, max_h, &skip, &cmd_rx, &frame_tx, &eof_thread,
                );
                if let Err(e) = r {
                    if let Ok(mut slot) = err_thread.lock() {
                        *slot = Some(e.to_string());
                    }
                    // 出错也置 EOF：界面据此收尾，不会永远停在"解码中…"
                    eof_thread.store(true, Ordering::Relaxed);
                }
            })
            .map_err(|e| AppError::Mp4(format!("启动播放线程失败：{e}")))?;
        Ok(Self {
            cmd: cmd_tx,
            frames: frame_rx,
            eof,
            error,
            join: Some(join),
        })
    }

    /// 跳到 `t_ms`（播放中改跳转位置时用；不需要重建线程）。
    pub fn seek(&self, t_ms: i64) {
        let _ = self.cmd.send(Cmd::Seek(t_ms.max(0)));
    }

    /// 取一帧（**非阻塞**）：没有就返回 `None`。
    pub fn try_recv(&self) -> Option<FrameRgba> {
        self.frames.try_recv().ok()
    }

    /// 解码线程是否已经到片尾（或已出错）。
    pub fn is_eof(&self) -> bool {
        self.eof.load(Ordering::Relaxed)
    }

    /// 取走错误信息（取一次就没了）。
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut g| g.take())
    }

    /// 让线程收工。**不 join**（理由见类型文档）。
    pub fn stop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
        // 句柄直接放掉 = detach：线程收到 Stop 后自己退出，UI 一个字节都不等
        self.join = None;
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
    }
}

/// 定位到 `t_ms` 之前的关键帧，并清掉解码器里残留的帧。
///
/// # Safety
/// `ictx` 必须是已打开并读过流信息的输入上下文，`dec` 必须是已打开的解码器。
unsafe fn seek_to(ictx: *mut ff::AVFormatContext, dec: *mut ff::AVCodecContext, t_ms: i64) {
    let ts_us = t_ms.max(0).saturating_mul(AV_TIME_BASE / 1000);
    let ret = ff::av_seek_frame(ictx, -1, ts_us, ff::AVSEEK_FLAG_BACKWARD);
    if ret < 0 {
        // 不致命：继续从当前位置解，最多多解几帧
        tracing::warn!(error = %av_err(ret), "播放 seek 失败，从当前位置继续");
    }
    ff::avcodec_flush_buffers(dec);
}

/// 解码主循环（在 `clip-play` 线程里跑）。
///
/// `skip` 是**要自动跳过的硬坏段**（升序 `(start_ms, end_ms)`，见
/// [`PlayerHandle::spawn_skipping`]）；空切片 = 不跳。
// 8 个参数都是线程的独立输入（路径/起点/解码上限/跳过表/命令口/出口/EOF 旗标）。
// 打包成 struct 只会多一层解构、不减少任何耦合 —— 这函数只被 spawn 调用一次。
#[allow(clippy::too_many_arguments)]
fn decode_loop(
    path: &Path,
    from_ms: i64,
    max_w: u32,
    max_h: u32,
    skip: &[(i64, i64)],
    cmd: &Receiver<Cmd>,
    out: &SyncSender<FrameRgba>,
    eof: &AtomicBool,
) -> Result<(), AppError> {
    if !path.is_file() {
        return Err(AppError::Mp4(format!(
            "播放只支持本地文件：{}",
            path.display()
        )));
    }
    let c_path = cpath(path)?;
    // SAFETY: 本节创建的每个 FFmpeg 对象都由对应的 RAII 守卫持有，
    // 作用域结束（含任何提前 return）时按声明逆序释放；指针在守卫存活期间始终有效。
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
        crate::mux::validate_input_streams(ictx.0)?;

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
            return Err(AppError::Mp4("文件里没有视频流，无法播放".into()));
        }
        let vstream = *(*ictx.0).streams.add(vindex as usize);
        let vtb = (*vstream).time_base;

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
        // 预览只需要屏幕上的尺寸：4K 源在 1080p 以下预览时，用 H.264 的 lowres
        // 直接解 1/2 或 1/4，省 CPU/内存；不支持 lowres 的编码器会忽略该字段。
        if max_w > 0 && max_h > 0 {
            let src_w = (*(*vstream).codecpar).width.max(0) as u32;
            let src_h = (*(*vstream).codecpar).height.max(0) as u32;
            let lowres = if src_w >= max_w.saturating_mul(4)
                && src_h >= max_h.saturating_mul(4)
            {
                2
            } else if src_w >= max_w.saturating_mul(2) && src_h >= max_h.saturating_mul(2) {
                1
            } else {
                0
            };
            if lowres > 0
                && (*(*vstream).codecpar).codec_id == ff::AVCodecID::AV_CODEC_ID_H264
            {
                // 只对 H.264 明确支持 lowres；其他编码器保持全分辨率。
                (*dec.0).lowres = lowres;
            }
        }
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

        // 播放起点：seek 落在它之前的关键帧上，之后的帧按"是否已到起点"过滤
        let mut start_ms = from_ms.max(0);
        seek_to(ictx.0, dec.0, start_ms);
        let mut clock = Instant::now();
        // true = 大盘已经读完，正在冲解码器（不再读包，只收帧）
        let mut draining = false;
        eof.store(false, Ordering::Relaxed);

        'playback: loop {
            // ---- ① 指令：跳转 / 收工（每帧都看一眼，响应时间 ≤ 一帧解码时间）----
            loop {
                match cmd.try_recv() {
                    Ok(Cmd::Stop) => return Ok(()),
                    Ok(Cmd::Seek(t)) => {
                        start_ms = t.max(0);
                        seek_to(ictx.0, dec.0, start_ms);
                        clock = Instant::now();
                        draining = false;
                        eof.store(false, Ordering::Relaxed);
                    }
                    Err(TryRecvError::Empty) => break,
                    // 界面那边已经不要了（句柄被 drop）
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }

            // ---- ② 读一个包（只在"还没到片尾"时读）----
            if !draining {
                let ret = ff::av_read_frame(ictx.0, pkt.0);
                if ret < 0 {
                    // **P35：区分"正常片尾"与"读到坏数据"。**
                    //
                    // 改造前这里不区分：`ret < 0` 一律当片尾。于是文件中途一个坏包
                    // 就会让播放"提前播完"，用户看到的是"播着播着停住了"。
                    //
                    // 现在：坏数据**不再冒充片尾**（日志说清楚是哪一段），
                    // 而"要不要跳过去"由调用方给的 `skip` 决定 —— 播放器不猜。
                    // 不把这条升级成 `Err`：断在片尾附近是常态（录制被中断的产物），
                    // 弹一个"播放失败"对话框比"安静地停在这里 + 界面上红色坏段带"更糟。
                    if ret != AVERROR_EOF {
                        tracing::warn!(
                            error = %av_err(ret),
                            at_ms = start_ms,
                            "播放读到损坏的数据（非正常片尾），按片尾收尾；坏段信息见 clip.health"
                        );
                    }
                    // 读完了：送空包把解码器里缓存的帧冲出来
                    ff::avcodec_send_packet(dec.0, ptr::null());
                    draining = true;
                } else {
                    if (*pkt.0).stream_index == vindex {
                        let send = ff::avcodec_send_packet(dec.0, pkt.0);
                        if send < 0 && send != AVERROR_EOF {
                            // 非致命（EAGAIN 之类）：帧会从下面的 receive 里出来
                            tracing::trace!(error = %av_err(send), "播放 send_packet 非致命错误");
                        }
                    }
                    ff::av_packet_unref(pkt.0);
                }
            }

            // ---- ③ 收帧：一个包可能吐出多帧（B 帧重排）----
            let mut got_any = false;
            // 本包解出来的帧落进了硬坏段 → 记下要跳到哪，出循环后统一 seek
            let mut skip_to: Option<i64> = None;
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
                let pts_ms = ts_to_ms(pts, vtb);
                // seek 落在关键帧上：起点之前的帧只用来"热解码器"，**不显示**
                if pts_ms + 1 < start_ms {
                    ff::av_frame_unref(frame.0);
                    continue;
                }
                // ---- P35：硬坏段自动跳过（用户口径：**仅硬错误自动跳**）----
                //
                // 三个条件缺一不可：
                // * `pts_ms >= a`：落进坏段；
                // * `b > pts_ms`：段尾还在前面 —— 防死循环的关键（见下面的说明）；
                // * `pts_ms < b`：没越过段尾。
                //
                // 为什么不会死循环：跳完之后 `start_ms = b`，若 `seek_to` 退回到坏段内部
                // （`AVSEEK_FLAG_BACKWARD` 会落在前一个关键帧上，那个关键帧本身就可能
                // 在坏段里），这些帧的 `pts_ms < start_ms` 会被上面那条热解码判断丢掉，
                // 永远走不到这里。
                if let Some(&(_, end)) = skip
                    .iter()
                    .find(|&&(a, b)| pts_ms >= a && pts_ms < b && b > pts_ms)
                {
                    ff::av_frame_unref(frame.0);
                    skip_to = Some(end);
                    break;
                }
                let converted = convert_to_rgba(frame.0, max_w, max_h);
                ff::av_frame_unref(frame.0);
                let (w, h, rgba) = converted?;
                // 实时节流：最多领先墙钟 `LEAD_MS`（否则会把整片一次解完，白烧 CPU）
                let ahead = (pts_ms - start_ms) - clock.elapsed().as_millis() as i64;
                if ahead > LEAD_MS {
                    let nap = ((ahead - LEAD_MS) as u64).clamp(1, NAP_MAX_MS);
                    thread::sleep(Duration::from_millis(nap));
                }
                // 界面落后 → **丢这一帧**（try_send 满即失败），绝不让解码线程等界面
                let _ = out.try_send(FrameRgba { pts_ms, w, h, rgba });
            }

            // ---- ③.5 跳段：落进硬坏段 → 定位到段尾继续（P35）----
            if let Some(t) = skip_to.take() {
                tracing::debug!(from_ms = start_ms, to_ms = t, "播放跳过编码错误区块");
                start_ms = t.max(0);
                seek_to(ictx.0, dec.0, start_ms);
                clock = Instant::now();
                draining = false;
                eof.store(false, Ordering::Relaxed);
                continue 'playback;
            }

            // ---- ④ 片尾：告诉界面，然后睡死等指令（不 busy loop）----
            if draining && !got_any {
                eof.store(true, Ordering::Relaxed);
                match cmd.recv() {
                    Ok(Cmd::Seek(t)) => {
                        start_ms = t.max(0);
                        seek_to(ictx.0, dec.0, start_ms);
                        clock = Instant::now();
                        draining = false;
                        eof.store(false, Ordering::Relaxed);
                        continue 'playback;
                    }
                    Ok(Cmd::Stop) | Err(_) => return Ok(()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 单测：用随包 ffmpeg **现场生成**一个小素材，真的走一遍解码 / seek
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ffmpeg_exe;
    use std::path::PathBuf;
    use std::process::Command;

    /// 生成 `160×120 @ 10 fps`、2 秒、**每秒一个关键帧**（`-g 10`）的测试素材。
    ///
    /// `-g 10` 是这组测试的关键：seek 会落到关键帧上，
    /// "关键帧间隔 1 秒"让"跳到 1500 ms 之后第一帧应 ≥ 1400 ms"这条断言有意义。
    ///
    /// `name` 必须**每个用例各不相同**：cargo 是并行跑测试的，
    /// 两个用例同时 `remove_file` + 写同一个路径会互相把对方的素材删掉
    /// （症状是"生成素材失败：Error opening output file"，与功能无关的假失败）。
    fn gen_clip(name: &str) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let dir = std::env::temp_dir().join("cap-player-tests");
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join(name);
        let _ = std::fs::remove_file(&out);
        let o = Command::new(&ff)
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10",
                "-t", "2", "-c:v", "libopenh264", "-b:v", "200k",
                "-g", "10", "-pix_fmt", "yuv420p",
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            Some(out)
        } else {
            // 只有"编码器不可用"才跳过；其它错误必须让测试失败，不能伪装成通过
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            let lower = err.to_ascii_lowercase();
            assert!(
                lower.contains("unknown encoder") || lower.contains("openh264"),
                "生成播放测试素材失败：{err}"
            );
            None
        }
    }

    /// 收帧直到满足条件或超时（播放线程是异步的，测试只能这样等）
    fn drain_until(
        h: &PlayerHandle,
        limit: std::time::Duration,
        mut stop: impl FnMut(&[FrameRgba]) -> bool,
    ) -> Vec<FrameRgba> {
        let deadline = Instant::now() + limit;
        let mut got = Vec::new();
        while Instant::now() < deadline {
            while let Some(f) = h.try_recv() {
                got.push(f);
            }
            if stop(&got) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        got
    }

    /// **真解码**：顺序出帧（pts 递增、尺寸/RGBA 长度正确）+ **seek 真的跳**。
    ///
    /// 两件事合在一个用例里是**故意的**：生成素材要起一次 ffmpeg 子进程，
    /// 而 `waveform` 那一组里有几个"时序敏感"的用例（它们断言 worker 的调度顺序），
    /// 并行跑太多子进程会把它们挤到假失败。一个用例 = 一次子进程，把干扰压到最小。
    #[test]
    fn player_decodes_in_order_and_seeks() {
        let Some(path) = gen_clip("decode-2s.mp4") else {
            return; // 没有随包 ffmpeg：跳过（与波形那组测试同一口径）
        };
        let mut h = PlayerHandle::spawn(&path, 0, 160, 120).expect("应能起播放线程");

        // ---- ① 从片头顺序解码 ----
        let got = drain_until(&h, Duration::from_secs(5), |v| v.len() >= 6);
        assert!(
            got.len() >= 6,
            "应至少解出 6 帧（实际 {}），错误={:?}",
            got.len(),
            h.take_error()
        );
        for w in got.windows(2) {
            assert!(
                w[1].pts_ms >= w[0].pts_ms,
                "帧必须按 pts 递增：{} → {}",
                w[0].pts_ms,
                w[1].pts_ms
            );
        }
        let f = &got[0];
        assert_eq!((f.w, f.h), (160, 120), "解码尺寸应等于源尺寸（不放大）");
        assert_eq!(f.rgba.len(), 160 * 120 * 4, "RGBA8 长度应为 w×h×4");
        assert!(f.rgba.iter().any(|&b| b != 0), "画面不应是全黑");
        assert!(h.take_error().is_none(), "不应有解码错误");

        // ---- ② 播到片尾，把通道取干净 ----
        let _ = drain_until(&h, Duration::from_secs(5), |_| h.is_eof());
        assert!(h.is_eof(), "2 秒素材应在 5 秒内播完");
        // 取干净：否则下面的断言会拿到 seek 之前的残留帧（"碰巧通过"比失败更糟）
        while h.try_recv().is_some() {}

        // ---- ③ seek 回 1500 ms ----
        // 关键帧在 1 s，所以起点之前那些帧会被丢掉，真正吐出来的第一帧应就在 1500 ms 附近
        h.seek(1_500);
        let after = drain_until(&h, Duration::from_secs(5), |v| v.len() >= 3);
        assert!(
            after.len() >= 3,
            "seek 之后应继续出帧（实际 {} 帧，错误={:?}）",
            after.len(),
            h.take_error()
        );
        let first = &after[0];
        assert!(
            (1_400..=1_600).contains(&first.pts_ms),
            "seek 到 1500 ms 后第一帧应落在 1500 ms 附近（实际 {} ms）—— \
             早于 1400 说明没有真的跳，晚于 1600 说明跳过头",
            first.pts_ms
        );
        assert!(
            after[1].pts_ms >= first.pts_ms,
            "seek 之后仍应顺序推进（{} → {}）",
            first.pts_ms,
            after[1].pts_ms
        );
        h.stop();
    }

    /// 帧队列按字节预算：1080p 允许满 8 帧，4K 只允许 2 帧，避免 264 MB 峰值。
    #[test]
    fn queue_cap_is_byte_bounded() {
        assert_eq!(queue_cap_for(1920, 1080), 8);
        assert_eq!(queue_cap_for(3840, 2160), 2);
        assert_eq!(queue_cap_for(0, 0), 8);
    }

    /// 文件打不开时**如实报错**（不 panic、不无限重试）。
    #[test]
    fn player_reports_open_errors() {
        let bogus = std::env::temp_dir().join("cap-player-tests").join("nope.mp4");
        let h = PlayerHandle::spawn(&bogus, 0, 160, 120).expect("线程本身应能起来");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !h.is_eof() {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(h.is_eof(), "打开失败必须置 EOF，界面才不会永远停在'解码中…'");
        assert!(
            h.take_error().is_some(),
            "打开失败必须带回错误信息（界面据此提示用户）"
        );
    }
}

