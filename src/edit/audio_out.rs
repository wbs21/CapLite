//! 剪辑页**播放的音频输出**：AAC 解码 → 重采样 → WASAPI 渲染（与 `player.rs` 对称）。
//!
//! # 为什么单独一个模块
//!
//! `player` 那条链从设计上就只搬视频（它自己文档里写明"最小可用播放器"），
//! 于是"播放没声音"不是 bug 回归，而是**功能缺项**。本模块补上音频这一半。
//!
//! # 双线程：解码（生产）与写设备（消费）必须分开（P32 修订，2026-10-18 下午）
//!
//! 第一版把"解一帧 AAC → 写一批设备"放在同一条线程里，实测 **22% 的写入批次带静音填充**
//! （play_smoke 诊断 `silent_batches/write_batches`），用户听感是严重的周期性咔哒卡顿。
//! 根因是**粒度错配**：AAC 一帧 1024 样本 = 21.3 ms 数据，而 WASAPI 设备一个处理周期
//! 只要约 10 ms；单线程在"写批"与"解下一帧"之间来回横跳，队列水位在 0 附近抖动，
//! 设备来要数据时手里经常不够一批 → 填静音 → 可闻空洞。
//!
//! 正解是播放器的标准架构：**解码线程提前囤货，渲染线程只管放水**
//! * `clip-audio-decode`（生产）：读包 → AAC 解码 → 重采样 → 按**媒体时刻节流**
//!   （领先 [`LEAD_MS`](crate::edit::audio_out::LEAD_MS) 就睡，与 `player` 同一拍），经有界通道交给渲染线程；
//! * `clip-audio-render`（消费）：唯一碰设备的线程，按设备节奏
//!   （`get_available_space_in_frames` + 事件等待）从队列取数写设备。
//!   队列恒有 [`LEAD_MS`](crate::edit::audio_out::LEAD_MS) 的存货，写批不再欠载（静音填充归零）。
//!
//! # 三条纪律
//!
//! 1. **界面零等待**：起线程、打开文件、解码、写设备全在两条后台线程；
//! 2. **失败只降级不出错**：没有音轨 / 输出设备打不开 / 重采样失败，
//!    一律记日志后**静音续播**（`PlaybackStats::output_live` 记录真实状态），
//!    绝不让"没声"升级成"播不了"；
//! 3. **seek 与视频同拍**：界面在同一处把同一个 `t` 发给视频与音频两条链
//!    （见 `ui::editor::reanchor_play`），seek 提前 `AAC_PRIMING_MS` 且那段提前量
//!    **故意播**（盖住 AAC 解码器开头的 priming 静默）。
//!
//! # 时钟
//!
//! 音频侧**不做音视频同步**，只按自己的 pts 顺序实时播（两端从同一 `from_ms` 起跳、
//! 都按墙钟实时，差异只在启动相位）。真正的音画同步引擎是独立待办。

use super::waveform::AAC_PRIMING_MS;
use super::{
    av_err, cpath, limit_input_probe, ts_to_ms, Codec, Frame, Input, Packet, AVERROR_EOF,
    AV_NOPTS_VALUE, AV_TIME_BASE,
};
use crate::audio::devices::resolve_render;
use crate::error::AppError;
use ffmpeg_next::ffi as ff;
use std::collections::VecDeque;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self};
use std::time::{Duration, Instant};
use wasapi::{AudioClient, AudioRenderClient, Direction, SampleType, StreamMode, WaveFormat};

/// 输出采样率（Hz）：与采集侧统一，重采样一次性做到位，渲染端 autoconvert 就不再返工。
///
/// P54：值改为取自 [`crate::audio::SAMPLE_RATE`]（唯一来源）。
pub const OUT_RATE: usize = crate::audio::SAMPLE_RATE as usize;
/// 输出声道数（**唯一来源**：[`crate::audio::CHANNELS`]）。
pub(crate) const OUT_CHANNELS: usize = crate::audio::CHANNELS;
/// 每样本字节数（f32）（**唯一来源**：[`crate::audio::BYTES_PER_SAMPLE`]）。
pub const BYTES_PER_SAMPLE: usize = crate::audio::BYTES_PER_SAMPLE;
/// 每帧字节数（2ch × f32）。
pub const BLOCK_ALIGN: usize = OUT_CHANNELS * BYTES_PER_SAMPLE;
/// 解码线程最多领先墙钟多少毫秒（**队列目标水位**）。
///
/// 200 ms 足够让设备任何时候来要数据都有存货（实测把静音填充率从 22% 打到 0），
/// 又不至于让"按停止"后还堆着没放完的数据。
///
/// P54：值改为取自 [`crate::edit::DECODE_LEAD_MS`]（唯一来源，与视频解码线程共用）。
/// 原先这里是另一份 `200`，靠注释"与 `player` 的 `LEAD_MS` 同值"来维系约束——
/// 注释不会在改了一边之后报错，常量会。
pub const LEAD_MS: i64 = crate::edit::DECODE_LEAD_MS;
/// 节流时单次睡眠的上限（毫秒）：睡太久会让 seek / stop 的响应变迟钝。
///
/// P54：值改为取自 [`crate::edit::DECODE_NAP_MAX_MS`]（唯一来源）。
pub const NAP_MAX_MS: u64 = crate::edit::DECODE_NAP_MAX_MS;
/// 每轮 `pump` 最多等设备多少毫秒（等它消费出空间）。
pub(crate) const RENDER_WAIT_MS: u32 = 20;
/// 解码→渲染有界通道容量（批，每批 ≈ 一个 AAC 帧 21.3 ms）。
///
/// 32 批 ≈ 680 ms 音频。它必须显著大于 [`LEAD_MS`]，否则"囤到水位"与"通道可发"
/// 互相打架，producer 会被无谓地堵住。
pub(crate) const CHUNK_CAP: usize = 32;
/// `avcodec_send_packet` 的"现在忙着收帧，稍后再发"返回码（libavutil `EAGAIN`）。
const AVERROR_EAGAIN: i32 = -11;

/// 解码线程交给渲染线程的一批 PCM（交错 f32，48 kHz / 2ch）。
pub(crate) struct PcmChunk {
    // P54：原先还有个 `pts_ms`（本批第一个样本的媒体时刻），标注"仅诊断/日志用"，
    // 但实际上从头到尾没人读 —— 每批都要算一次 `ts_to_ms` 再写进去，纯浪费。删掉。
    pub samples: Vec<f32>,
    /// seek 世代。渲染线程只接受当前世代的 PCM，避免 seek 命令到达后
    /// 又播放 producer 在旧位置已经排进通道的音频。
    generation: u64,
}

/// 播放音频的**运行统计**（既是测试钩子，也是诊断出口：`output_live == false` 时
/// 用户听到的"没声音"就有据可查——是没音轨、没设备，还是解码失败）。
#[derive(Debug, Clone, Default)]
pub struct PlaybackStats {
    /// 已解码并交付的音频帧数（1 帧 = 2ch 各一个样本 @48 kHz）
    pub decoded_frames: u64,
    /// 已写进渲染设备的帧数（供对账：`rendered ≈ decoded` 表示没在设备侧堆积）
    pub rendered_frames: u64,
    /// 最近一个交付帧的媒体时刻（毫秒）
    pub last_pts_ms: i64,
    /// 交付内容的最大绝对值（正弦素材 ≈ 0.09@96k；全零 = 真的没内容）
    pub peak: f32,
    /// 渲染设备是否真的在出声（false = 降级为静音播放）
    pub output_live: bool,
    /// **相邻两次写设备的墙钟间隔最大值（毫秒）**——供给节奏的直接体检。
    /// 稳态应 ≤ 设备周期（≈10 ms）的 2~3 倍；持续 >30 ms 说明供给被拖住。
    pub max_write_gap_ms: u64,
    /// **写进设备时有静音填充的批次数**（设备要 10 ms、手上不够 10 ms）。
    /// P32 修订前的病灶就是它（22%）；修订后稳态应为 **0**。
    pub silent_batches: u64,
    /// 写设备的总批数（`silent_batches / write_batches` = 欠载比例）
    pub write_batches: u64,
}

/// 两条后台线程共用的指令（同一个 `t` 发给两边：解码侧重定位、渲染侧重清队）。
#[derive(Debug, Clone, Copy)]
enum Cmd {
    Seek { ms: i64, generation: u64 },
    Stop,
}

/// 界面侧持有的音频播放句柄。
///
/// 生命周期与 `player::PlayerHandle` 相同：`spawn` → 播放中 `seek` → `stop`（或 `Drop`）。
/// 错误**不进界面**：音频链路任何失败都只降级为静音播放（见模块文档纪律 2），
/// 所以这里连 `take_error` 都没有——界面不需要为"没声音"弹错误框。
pub struct AudioTrackHandle {
    /// 解码线程指令通道
    cmd_decode: Sender<Cmd>,
    /// 渲染线程指令通道
    cmd_render: Sender<Cmd>,
    eof: Arc<AtomicBool>,
    stats: Arc<Mutex<PlaybackStats>>,
    /// 队列中尚未被渲染线程取走的样本帧数（节流水位判据）
    pending: Arc<AtomicUsize>,
    /// 单调递增的 seek 世代（只在 UI 线程修改）
    next_generation: AtomicU64,
}

impl AudioTrackHandle {
    /// 起播放音频链（两条线程：`clip-audio-decode` + `clip-audio-render`）。
    ///
    /// `connect_output = false` 只解码、不连渲染设备、不节流（单测用）；
    /// 界面恒传 `true`。
    pub fn spawn(path: &Path, from_ms: i64, connect_output: bool) -> Result<Self, AppError> {
        let path = path.to_path_buf();
        let (cmd_d_tx, cmd_d_rx) = channel::<Cmd>();
        let (cmd_r_tx, cmd_r_rx) = channel::<Cmd>();
        // 有界通道：producer 囤过 [`CHUNK_CAP`] 批时**自己会睡**（节流就在那里发生），
        // 不需要再在别处限流。
        let (pcm_tx, pcm_rx) = sync_channel::<PcmChunk>(CHUNK_CAP);
        let eof = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(PlaybackStats::default()));
        let pending = Arc::new(AtomicUsize::new(0));

        let p2 = path.clone();
        let (stats_d, pending_d) = (stats.clone(), pending.clone());
        let dec = thread::Builder::new()
            .name("clip-audio-decode".into())
            .spawn(move || {
                if let Err(e) = decode_produce(
                    &p2,
                    from_ms.max(0),
                    connect_output,
                    &cmd_d_rx,
                    &pcm_tx,
                    &pending_d,
                    &stats_d,
                ) {
                    tracing::warn!(error = %e, "播放：音频解码结束，本端只有画面");
                }
                // ⚠️ **不在这里置 eof**：producer 退出只意味着"生产完了"，
                // 队列里可能还有 (~LEAD_MS) 的货没放完。eof 由渲染线程
                // "放空且见 Disconnected"时置（见 render_consume）——
                // 提前置位会让 UI 以为"播完了"，而耳机里还有话没说完。
            })
            .map_err(|e| AppError::Mp4(format!("启动音频解码线程失败：{e}")))?;

        let (eof_r, stats_r, pending_r) = (eof.clone(), stats.clone(), pending.clone());
        let ren = thread::Builder::new()
            .name("clip-audio-render".into())
            .spawn(move || {
                render_consume(
                    pcm_rx,
                    &cmd_r_rx,
                    &eof_r,
                    &stats_r,
                    &pending_r,
                    connect_output,
                );
                eof_r.store(true, Ordering::Relaxed);
            })
            .map_err(|e| AppError::Mp4(format!("启动音频渲染线程失败：{e}")))?;

        // 两条线程的 JoinHandle 直接放掉 = detach：UI 一个字节都不等，
        // 线程收 Stop 后各自退出（render 退出 = 设备句柄释放 = 链路收工）。
        drop(dec);
        drop(ren);
        Ok(Self {
            cmd_decode: cmd_d_tx,
            cmd_render: cmd_r_tx,
            eof,
            stats,
            pending,
            next_generation: AtomicU64::new(0),
        })
    }

    /// 跳到 `t_ms`：**同一个 `t` 同时发给解码与渲染两条链**（与视频 seek 同一次调用点）。
    pub fn seek(&self, t_ms: i64) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let cmd = Cmd::Seek {
            ms: t_ms.max(0),
            generation,
        };
        let _ = self.cmd_decode.send(cmd);
        let _ = self.cmd_render.send(cmd);
    }

    /// 音频链是否已经结束（播完 / 没有音轨 / 链路失败）。
    pub fn is_eof(&self) -> bool {
        self.eof.load(Ordering::Relaxed)
    }

    /// 读一份统计快照（克隆，测试断言用）。
    pub fn stats(&self) -> PlaybackStats {
        self.stats.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 队列中还没被取走的音频（毫秒）——节流水位的现成读数。
    pub fn pending_ms(&self) -> u64 {
        let frames = self.pending.load(Ordering::Relaxed) as u64;
        frames * 1000 / OUT_RATE as u64
    }

    /// 让链路收工（两条线程各发一次 Stop；都不 join）。
    pub fn stop(&mut self) {
        let _ = self.cmd_decode.send(Cmd::Stop);
        let _ = self.cmd_render.send(Cmd::Stop);
    }
}

impl Drop for AudioTrackHandle {
    fn drop(&mut self) {
        let _ = self.cmd_decode.send(Cmd::Stop);
        let _ = self.cmd_render.send(Cmd::Stop);
    }
}

// ---------------------------------------------------------------------------
// 渲染（消费）端：唯一碰 WASAPI 设备的线程
// ---------------------------------------------------------------------------

/// 重采样上下文守卫（`swr_alloc_set_opts2` ↔ `swr_free`）。
struct Swr(*mut ff::SwrContext);

impl Drop for Swr {
    fn drop(&mut self) {
        // SAFETY: 指针由 swr_alloc_set_opts2 成功分配；swr_free 之后置空，不会二次释放。
        unsafe {
            if !self.0.is_null() {
                ff::swr_free(&mut self.0);
                self.0 = ptr::null_mut();
            }
        }
    }
}

/// 渲染设备句柄（不含待写队列——队列归 `render_consume` 持有，
/// 这样"没有设备"的降级路径就不必假装有个队列）。
struct RenderSink {
    client: AudioClient,
    render: AudioRenderClient,
    event: wasapi::Handle,
    /// 已写进设备的帧数（统计对账用）
    frames_written: u64,
    /// 相邻两次写设备的墙钟间隔最大值（毫秒）
    max_write_gap_ms: u64,
    /// 有静音填充的批次数（欠载次数）
    silent_batches: u64,
    /// 写设备总批数
    write_batches: u64,
    /// 上一次写给设备的时刻（供给间隙诊断）
    last_write: Option<Instant>,
}

impl RenderSink {
    /// 打开**默认渲染设备**。失败返回 `None`（调用方降级为静音播放，不视为错误）。
    fn open() -> Option<Self> {
        let device = match resolve_render(None) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "播放：取默认输出设备失败，本端不出声");
                return None;
            }
        };
        let mut client = match device.get_iaudioclient() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "播放：取 IAudioClient 失败，本端不出声");
                return None;
            }
        };
        let fmt = WaveFormat::new(32, 32, &SampleType::Float, OUT_RATE, OUT_CHANNELS, None);
        let (def_time, min_time) = match client.get_device_period() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "播放：取设备周期失败，本端不出声");
                return None;
            }
        };
        // autoconvert：与采集侧同一约定——WASAPI 负责把我们的 48k/2ch/f32 适配到设备 mix
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: def_time.max(min_time),
        };
        if let Err(e) = client.initialize_client(&fmt, &Direction::Render, &mode) {
            tracing::warn!(error = %e, "播放：初始化输出设备失败，本端不出声");
            return None;
        }
        let event = match client.set_get_eventhandle() {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "播放：取事件句柄失败，本端不出声");
                return None;
            }
        };
        let render = match client.get_audiorenderclient() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "播放：取 IAudioRenderClient 失败，本端不出声");
                return None;
            }
        };
        if let Err(e) = client.start_stream() {
            tracing::warn!(error = %e, "播放：启动输出流失败，本端不出声");
            return None;
        }
        tracing::info!("播放：音频输出已打开（默认渲染设备）");
        Some(Self {
            client,
            render,
            event,
            frames_written: 0,
            max_write_gap_ms: 0,
            silent_batches: 0,
            write_batches: 0,
            last_write: None,
        })
    }

    /// 把待写队列尽量写进设备；设备满就等事件（最多 `wait_ms`），队列不够写静音。
    ///
    /// **正常路径下不会写静音**：producer 提前囤了 [`LEAD_MS`] 的货。
    /// 静音只在"producer 也断了"（解码结束/seek 刚 flush）时兜底——
    /// 宁可送静音也不能让设备欠载停摆（那会"咔哒"一声后再也没声）。
    fn pump(&mut self, pcm: &mut VecDeque<f32>, wait_ms: u32) {
        let deadline = Instant::now() + Duration::from_millis(wait_ms as u64);
        let mut scratch: Vec<f32> = Vec::new();
        loop {
            let avail = match self.client.get_available_space_in_frames() {
                Ok(n) => n as usize,
                Err(_) => return,
            };
            if avail == 0 {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return;
                }
                let ms = left.as_millis().min(wait_ms as u128) as u32;
                let _ = self.event.wait_for_event(ms.max(1));
                continue;
            }
            let have_frames = pcm.len() / OUT_CHANNELS;
            let want = avail.min(have_frames);
            scratch.clear();
            scratch.reserve(avail * OUT_CHANNELS);
            for _ in 0..want * OUT_CHANNELS {
                // 前面的 min 已保证不会 None
                scratch.push(pcm.pop_front().unwrap_or(0.0));
            }
            // 队列不够的位置补零 = 静音（见方法文档：只在异常路径发生）
            scratch.resize(avail * OUT_CHANNELS, 0.0);
            let any_silent = want < avail;
            // SAFETY: f32 切片按位转字节；x86 上 f32 内存布局即小端 IEEE-754，
            // 与请求的设备格式一致；长度严格等于 avail 帧 × 每帧字节数。
            let bytes = unsafe {
                std::slice::from_raw_parts(scratch.as_ptr() as *const u8, avail * BLOCK_ALIGN)
            };
            if self.render.write_to_device(avail, bytes, None).is_err() {
                return;
            }
            // 诊断记账（不影响播放路径）
            let now = Instant::now();
            if let Some(prev) = self.last_write.replace(now) {
                let gap = now.saturating_duration_since(prev).as_millis() as u64;
                if gap > self.max_write_gap_ms {
                    self.max_write_gap_ms = gap;
                }
            }
            self.write_batches += 1;
            if any_silent {
                self.silent_batches += 1;
            }
            self.frames_written += avail as u64;
            if Instant::now() >= deadline {
                return;
            }
        }
    }

    fn stop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

impl Drop for RenderSink {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// 解码（生产）线程：demux → AAC 解码 → 重采样 → 按媒体时刻节流 → 交渲染线程
// ---------------------------------------------------------------------------

/// 定位到 `start_ms - `[`AAC_PRIMING_MS`] 之前的关键包，并清掉解码器 / 重采样器的残留。
///
/// # Safety
/// 三个指针必须都是本模块刚建好、守卫仍存活的有效指针。
unsafe fn seek_audio(
    ictx: *mut ff::AVFormatContext,
    dec: *mut ff::AVCodecContext,
    swr: *mut ff::SwrContext,
    start_ms: i64,
) {
    let target = (start_ms - AAC_PRIMING_MS).max(0);
    let ts_us = target.saturating_mul(AV_TIME_BASE / 1000);
    let ret = ff::av_seek_frame(ictx, -1, ts_us, ff::AVSEEK_FLAG_BACKWARD);
    if ret < 0 {
        // 不致命：继续从当前位置解，最多多解几帧
        tracing::warn!(error = %av_err(ret), "播放 seek 失败，从当前位置继续");
    }
    ff::avcodec_flush_buffers(dec);
    ff::swr_close(swr);
    if ff::swr_init(swr) < 0 {
        tracing::warn!("播放：seek 后重置重采样器失败");
    }
}

/// 起点之前的帧要不要丢（**纯函数**，单测覆盖）。
///
/// seek 落在 `start_ms - AAC_PRIMING_MS` 上，那段提前量是**故意要播的**
/// （盖住 AAC 开头的 priming 静默），所以判据不是"pts < start"，
/// 而是"pts 连提前量都盖不住"——那种帧只可能是 seek 落点更早的残余。
pub(crate) fn skip_before_start(pts_ms: i64, start_ms: i64) -> bool {
    pts_ms + AAC_PRIMING_MS < start_ms
}

/// 节流睡眠毫秒（**纯函数**，单测覆盖）：队列超过目标水位 [`LEAD_MS`] 多少就睡多少
/// （封顶 [`NAP_MAX_MS`]，seek/stop 的响应因此永远不迟于 20 ms）。
pub(crate) fn nap_ms(pending_frames: i64) -> u64 {
    let lead_frames = LEAD_MS * OUT_RATE as i64 / 1000;
    if pending_frames <= lead_frames {
        return 0;
    }
    let ahead_ms = (pending_frames - lead_frames) * 1000 / OUT_RATE as i64;
    ahead_ms.clamp(1, NAP_MAX_MS as i64) as u64
}

/// 生产线程主体：打开文件 → 解音频 → 重采样 → 节流 → 有界通道。
///
/// `throttle`（界面恒 true）为 false 时全速解码不睡——单测用它瞬间跑完。
/// 线程退出 = `pcm_tx` 被 drop = 渲染线程见 `Disconnected`，据此收尾。
fn decode_produce(
    path: &Path,
    from_ms: i64,
    throttle: bool,
    cmd: &Receiver<Cmd>,
    pcm_tx: &SyncSender<PcmChunk>,
    pending: &AtomicUsize,
    stats: &Mutex<PlaybackStats>,
) -> Result<(), AppError> {
    if !path.is_file() {
        return Err(AppError::Mp4(format!(
            "音频播放只支持本地文件：{}",
            path.display()
        )));
    }
    let c_path = cpath(path)?;
    // SAFETY: 本节每个 FFmpeg 对象都由 RAII 守卫持有，提前返回也按逆序释放。
    unsafe {
        let mut raw: *mut ff::AVFormatContext = ptr::null_mut();
        let ret = ff::avformat_open_input(&mut raw, c_path.as_ptr(), ptr::null(), ptr::null_mut());
        if ret < 0 || raw.is_null() {
            return Err(AppError::Mp4(format!(
                "播放：打开 {} 失败：{}",
                path.display(),
                av_err(ret)
            )));
        }
        let ictx = Input(raw);
        limit_input_probe(ictx.0);
        if ff::avformat_find_stream_info(ictx.0, ptr::null_mut()) < 0 {
            return Err(AppError::Mp4("播放：读取流信息失败".into()));
        }
        crate::mux::validate_input_streams(ictx.0)?;

        // 第一条音频流
        let mut aindex = -1i32;
        for i in 0..(*ictx.0).nb_streams as usize {
            let st = *(*ictx.0).streams.add(i);
            if (*(*st).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_AUDIO {
                aindex = (*st).index;
                break;
            }
        }
        if aindex < 0 {
            // 没有音轨：**如实降级，不是错误**（不少素材/片段就是没音轨）
            tracing::info!(path = %path.display(), "播放：素材没有音频流，本端不出声");
            return Ok(());
        }
        let astream = *(*ictx.0).streams.add(aindex as usize);
        let atb = (*astream).time_base;

        let codec = ff::avcodec_find_decoder((*(*astream).codecpar).codec_id);
        if codec.is_null() {
            tracing::warn!("播放：本程序没有该音轨编码的解码器，本端不出声");
            return Ok(());
        }
        let dec = Codec(ff::avcodec_alloc_context3(codec));
        if dec.0.is_null() {
            return Err(AppError::Mp4("播放：avcodec_alloc_context3 失败".into()));
        }
        if ff::avcodec_parameters_to_context(dec.0, (*astream).codecpar) < 0 {
            return Err(AppError::Mp4("播放：复制解码参数失败".into()));
        }
        (*dec.0).pkt_timebase = atb;
        if ff::avcodec_open2(dec.0, codec, ptr::null_mut()) < 0 {
            return Err(AppError::Mp4("播放：打开音频解码器失败".into()));
        }

        // ---- 重采样：源格式 → 48 kHz / 2ch / f32（与渲染端请求的格式严格一致）----
        let mut stereo = ff::AVChannelLayout {
            order: ff::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC,
            nb_channels: 0,
            u: std::mem::zeroed(),
            opaque: ptr::null_mut(),
        };
        if ff::av_channel_layout_from_mask(&mut stereo, ff::AV_CH_LAYOUT_STEREO) < 0 {
            return Err(AppError::Audio("播放：构造立体声布局失败".into()));
        }
        let in_layout = (*dec.0).ch_layout;
        let in_fmt = (*dec.0).sample_fmt;
        let in_rate = if (*dec.0).sample_rate > 0 {
            (*dec.0).sample_rate
        } else {
            (*(*astream).codecpar).sample_rate.max(1)
        };
        let mut swr_raw: *mut ff::SwrContext = ptr::null_mut();
        let ret = ff::swr_alloc_set_opts2(
            &mut swr_raw,
            &stereo,
            ff::AVSampleFormat::AV_SAMPLE_FMT_FLT,
            OUT_RATE as i32,
            &in_layout,
            in_fmt,
            in_rate,
            0,
            ptr::null_mut(),
        );
        if ret < 0 || swr_raw.is_null() {
            return Err(AppError::Audio(format!(
                "播放：创建重采样器失败：{}",
                av_err(ret)
            )));
        }
        let swr = Swr(swr_raw);
        if ff::swr_init(swr.0) < 0 {
            return Err(AppError::Audio("播放：初始化重采样器失败".into()));
        }

        let pkt = Packet(ff::av_packet_alloc());
        if pkt.0.is_null() {
            return Err(AppError::Mp4("播放：av_packet_alloc 失败".into()));
        }
        let frame = Frame(ff::av_frame_alloc());
        if frame.0.is_null() {
            return Err(AppError::Mp4("播放：av_frame_alloc 失败".into()));
        }
        // 每帧最多产出多少输出样本：按源采样率留一倍余量（与 waveform 同款估算）
        let max_out = in_rate.max(1024) as usize * 2 + 4096;
        let mut out_buf = vec![0.0f32; max_out * OUT_CHANNELS];

        let mut start_ms = from_ms.max(0);
        let mut generation = 0u64;
        seek_audio(ictx.0, dec.0, swr.0, start_ms);
        // true = 大盘已经读完，正在冲解码器（不再读包，只收帧）
        let mut draining = false;

        loop {
            // ---- ① 指令：跳转 / 收工 ----
            loop {
                match cmd.try_recv() {
                    Ok(Cmd::Stop) => return Ok(()),
                    Ok(Cmd::Seek {
                        ms,
                        generation: new_generation,
                    }) => {
                        start_ms = ms.max(0);
                        generation = new_generation;
                        seek_audio(ictx.0, dec.0, swr.0, start_ms);
                        draining = false;
                    }
                    Err(TryRecvError::Empty) => break,
                    // 渲染线程没了（界面不要了）
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }

            // ---- ② 读一个包（只在"还没到片尾"时读）----
            if !draining {
                let ret = ff::av_read_frame(ictx.0, pkt.0);
                if ret < 0 {
                    ff::avcodec_send_packet(dec.0, ptr::null());
                    draining = true;
                } else {
                    if (*pkt.0).stream_index == aindex {
                        let send = ff::avcodec_send_packet(dec.0, pkt.0);
                        if send < 0 && send != AVERROR_EOF && send != AVERROR_EAGAIN {
                            tracing::trace!(error = %av_err(send), "播放音频 send_packet 非致命错误");
                        }
                    }
                    ff::av_packet_unref(pkt.0);
                }
            }

            // ---- ③ 收帧 → 重采样 → 节流 → 交接 ----
            let mut got_any = false;
            loop {
                let recv = ff::avcodec_receive_frame(dec.0, frame.0);
                if recv < 0 {
                    break;
                }
                got_any = true;
                let ts = if (*frame.0).best_effort_timestamp != AV_NOPTS_VALUE {
                    (*frame.0).best_effort_timestamp
                } else {
                    (*frame.0).pts
                };
                let pts_ms = ts_to_ms(ts, atb);
                if skip_before_start(pts_ms, start_ms) {
                    ff::av_frame_unref(frame.0);
                    continue;
                }
                let out_ptrs: [*mut u8; 1] = [out_buf.as_mut_ptr().cast::<u8>()];
                let got = ff::swr_convert(
                    swr.0,
                    out_ptrs.as_ptr(),
                    max_out as i32,
                    (*frame.0).data.as_ptr() as *const *const u8,
                    (*frame.0).nb_samples,
                );
                ff::av_frame_unref(frame.0);
                if got < 0 {
                    tracing::warn!(error = %av_err(got), "播放：重采样失败，本端停止出声");
                    return Ok(());
                }
                let n = got as usize;
                if n == 0 {
                    continue;
                }
                let samples = out_buf[..n * OUT_CHANNELS].to_vec();
                if let Ok(mut s) = stats.lock() {
                    s.decoded_frames += n as u64;
                    s.last_pts_ms = pts_ms;
                    let peak = samples.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                    if peak > s.peak {
                        s.peak = peak;
                    }
                }
                // 节流：领先墙钟超过 LEAD_MS 就睡（与 player 同一拍）。
                // 睡在 send 之前，所以"囤货水位"天然落在 LEAD_MS 附近。
                if throttle {
                    let pend = pending.load(Ordering::Relaxed) as i64;
                    let nap = nap_ms(pend);
                    if nap > 0 {
                        thread::sleep(Duration::from_millis(nap));
                    }
                }
                pending.fetch_add(n, Ordering::Relaxed);
                // 交接（通道满则在这里睡——那正是节流的另一半）。
                // send 失败 = 渲染线程已走（stop）：安静收工。
                if pcm_tx
                    .send(PcmChunk {
                        samples,
                        generation,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }

            // ---- ④ 片尾：producer 的使命到此为止 ----
            //
            // 不设 eof、不清通道：队列里可能还有 (~LEAD_MS) 的货没放完，
            // eof 由渲染线程"放空且见 Disconnected"时置（见 render_consume）。
            // 这里 return = pcm_tx drop = 渲染侧收 Disconnected。
            if draining && !got_any {
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 渲染（消费）线程：唯一碰设备的线程——收数据、按设备节奏放水
// ---------------------------------------------------------------------------

/// seek 世代判据（纯函数，单测覆盖）。
fn pcm_generation_matches(chunk_generation: u64, current_generation: u64) -> bool {
    chunk_generation == current_generation
}

/// 消费线程主体。四件事按优先级：指令（seek/stop）→ 收数据 → 放水 → eof 收尾。
///
/// **延迟打开设备**：等第一批数据到手（或 producer 结束）才开渲染设备——
/// 无音轨的文件根本不会碰扬声器；正常播放的开场也直接就是"水位够了再放"，
/// 不会出现"设备先跑起来、手里却没货"的启动爆音。
fn render_consume(
    pcm_rx: Receiver<PcmChunk>,
    cmd: &Receiver<Cmd>,
    eof: &AtomicBool,
    stats: &Mutex<PlaybackStats>,
    pending: &AtomicUsize,
    connect_output: bool,
) {
    let mut sink: Option<RenderSink> = None;
    // 设备只尝试开一次（失败即降级，不重试——播放器没有"重开扬声器"的语义）
    let mut sink_tried = false;
    // 待写队列（交错 f32）。它只属于本线程。
    let mut queue: VecDeque<f32> = VecDeque::new();
    let mut disconnected = false;
    let mut generation = 0u64;
    let lead_frames = LEAD_MS * OUT_RATE as i64 / 1000;

    while !disconnected || !queue.is_empty() {
        // ---- ① 指令：stop / seek 清队（解码侧重定位是它自己的事）----
        loop {
            match cmd.try_recv() {
                Ok(Cmd::Stop) => return,
                Ok(Cmd::Seek {
                    generation: new_generation,
                    ..
                }) => {
                    // ⚠️ 只清 queue，**不再扣 pending**：这些帧在 try_recv 收到时
                    // 就已经扣过了；再扣会把 usize 扣到下溢 panic。
                    queue.clear();
                    generation = new_generation;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // ---- ② 收数据（非阻塞，一来就全收进来）----
        loop {
            match pcm_rx.try_recv() {
                Ok(chunk) => {
                    let n = chunk.samples.len() / OUT_CHANNELS;
                    pending.fetch_sub(n, Ordering::Relaxed);
                    // seek 之前 producer 已经排进通道的批必须丢弃：
                    // 命令通道与 PCM 通道彼此独立，清掉本地 queue 并不能
                    // 清掉 PCM 通道里已经存在的旧数据。
                    //
                    // 如果新世代的 PCM 比 Seek 命令先到，则让 chunk 反向推进
                    // 本地世代并清空旧队列，避免丢掉 seek 后的第一帧音频。
                    if chunk.generation > generation {
                        generation = chunk.generation;
                        queue.clear();
                    }
                    if pcm_generation_matches(chunk.generation, generation) {
                        queue.extend(chunk.samples);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // ---- ③ 时机到了才开设备 ----
        if !sink_tried && (disconnected || queue.len() / OUT_CHANNELS >= lead_frames as usize) {
            sink_tried = true;
            if connect_output {
                sink = RenderSink::open();
            }
            if let Ok(mut s) = stats.lock() {
                s.output_live = sink.is_some();
            }
        }

        // ---- ④ 放水 ----
        if let Some(s) = sink.as_mut() {
            s.pump(&mut queue, RENDER_WAIT_MS);
            let snap = (
                s.frames_written,
                s.max_write_gap_ms,
                s.silent_batches,
                s.write_batches,
            );
            if let Ok(mut st) = stats.lock() {
                st.rendered_frames = snap.0;
                st.max_write_gap_ms = snap.1;
                st.silent_batches = snap.2;
                st.write_batches = snap.3;
            }
        } else {
            // 没设备（测试/降级路径）：只需把队列 drain 掉，别空转。
            // 同样**不扣 pending**（收到时已扣）；无条件清是必须的——
            // 否则 disconnected 后 queue 永远非空，外层循环死转。
            queue.clear();
            if !disconnected {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    // ---- ⑤ 放空且 producer 已走：置 eof，然后睡等指令 ----
    eof.store(true, Ordering::Relaxed);
    loop {
        match cmd.recv() {
            // producer 已退出，seek 无从重启播放：忽略（界面会重新 spawn）
            Ok(Cmd::Seek { .. }) => continue,
            Ok(Cmd::Stop) | Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// 单测：素材由随包 ffmpeg 现场生成；默认不连渲染设备（单测不出声）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ffmpeg_exe;
    use std::path::PathBuf;
    use std::process::Command;

    fn asset_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cap-audio-out-tests");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 生成 2 秒素材：视频 testsrc2 + **AAC 正弦 440 Hz** 音轨。
    fn gen_clip_with_audio(name: &str) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(name);
        let _ = std::fs::remove_file(&out);
        let o = Command::new(&ff)
            .args([
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
                "2",
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
                "-shortest",
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            Some(out)
        } else {
            let err = String::from_utf8_lossy(&o.stderr).to_string();
            assert!(
                err.to_ascii_lowercase().contains("unknown encoder")
                    || err.to_ascii_lowercase().contains("openh264"),
                "生成音频播放素材失败：{err}"
            );
            None
        }
    }

    /// 生成 2 秒**纯视频**素材（没有音轨，用来验证如实降级）。
    fn gen_clip_video_only(name: &str) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(name);
        let _ = std::fs::remove_file(&out);
        let o = Command::new(&ff)
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
                "2",
                "-c:v",
                "libopenh264",
                "-b:v",
                "500k",
                "-g",
                "30",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            Some(out)
        } else {
            None
        }
    }

    fn wait_eof(h: &AudioTrackHandle, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !h.is_eof() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn skip_before_start_keeps_priming_frames() {
        // 提前量 AAC_PRIMING_MS(=44ms) 内的帧**故意保留**（盖 AAC priming 静默）
        assert!(!skip_before_start(1_000, 1_000), "恰在起点：不丢");
        assert!(
            !skip_before_start(1_000 - AAC_PRIMING_MS, 1_000),
            "提前量边界：不丢"
        );
        assert!(
            skip_before_start(900, 1_000),
            "比提前量还早：丢（seek 残余）"
        );
        assert!(skip_before_start(0, 1_000), "远早于起点：丢");
        assert!(!skip_before_start(0, 0), "从片头播：什么都不丢");
    }

    #[test]
    fn nap_ms_throttles_only_above_lead_watermark() {
        let lead_frames = LEAD_MS * OUT_RATE as i64 / 1000;
        assert_eq!(nap_ms(0), 0, "空队列不睡");
        assert_eq!(nap_ms(lead_frames), 0, "恰好水位不睡");
        assert_eq!(nap_ms(lead_frames + 1), 1, "超一点也睡（至少 1ms）");
        // 超 10 ms 水位 → 睡 10 ms（未封顶的常规路径）
        assert_eq!(nap_ms(lead_frames + 10 * 48), 10);
        // 超 40 ms 水位 → 封顶到 NAP_MAX_MS（seek/stop 响应不迟于 20 ms）
        assert_eq!(nap_ms(lead_frames + 40 * 48), NAP_MAX_MS);
        // 超 10 s 水位 → 同样封顶（nap 永远 ≤ NAP_MAX_MS）
        assert_eq!(nap_ms(lead_frames + 480_000), NAP_MAX_MS);
    }

    /// **真解码**：AAC 音轨 → 重采样 → 48k/2ch/f32（不连设备，不出声）。
    /// eof 由渲染线程"放空 + producer 离开"置位，顺带验证明通道清空。
    #[test]
    fn decodes_aac_track_end_to_end() {
        let Some(path) = gen_clip_with_audio("decode.mp4") else {
            return; // 没有随包 ffmpeg：跳过
        };
        let h = AudioTrackHandle::spawn(&path, 0, false).expect("应能起播放音频链");
        wait_eof(&h, Duration::from_secs(10));
        assert!(h.is_eof(), "2 秒素材应在 10 秒内播完");
        let s = h.stats();
        // 2 s @48 kHz ≈ 96 000 帧（容器/编码边界留 1 成余量）
        assert!(
            (86_000..=104_000).contains(&s.decoded_frames),
            "解出帧数应≈2 秒音频，实际 {}",
            s.decoded_frames
        );
        assert!(
            (1_800..=2_100).contains(&s.last_pts_ms),
            "最后 pts 应≈片尾 2 s，实际 {}",
            s.last_pts_ms
        );
        assert!(
            s.peak > 0.05,
            "正弦 440 Hz 的峰值线性值应 > 0.05（真的解出了内容），实际 {} \\
             （随包 ffmpeg 生成的 sine+AAC 96k 实测峰值约 -16 dB）",
            s.peak
        );
        assert!(!s.output_live, "connect_output=false 不应碰渲染设备");
        assert_eq!(s.rendered_frames, 0, "没设备就不该有写入");
        assert_eq!(h.pending_ms(), 0, "播完时通道应排空（pending 归零）");
    }

    /// seek：跳到 1500 ms 之后应从那里继续解。
    #[test]
    fn seek_jumps_to_the_requested_position() {
        let Some(path) = gen_clip_with_audio("seek.mp4") else {
            return;
        };
        let mut h = AudioTrackHandle::spawn(&path, 0, false).expect("应能起播放音频链");
        // 立即 seek：解码线程可能还没起来、也可能已经全速解完了 2 秒（无节流不睡），
        // 两个时点 seek 都必须生效。
        h.seek(1_500);
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.stats().last_pts_ms < 1_400 && !h.is_eof() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            h.stats().last_pts_ms >= 1_400,
            "seek 到 1500 ms 后应解到 1400 ms 之后，实际 {}",
            h.stats().last_pts_ms
        );
        wait_eof(&h, Duration::from_secs(10));
        let s = h.stats();
        assert!(
            s.last_pts_ms >= 1_900,
            "seek 后续播应仍能到片尾附近，实际 {}",
            s.last_pts_ms
        );
        assert!(s.peak > 0.05, "seek 之后的内容也应真的有声音");
        h.stop();
    }

    #[test]
    fn stale_pcm_after_seek_is_rejected() {
        assert!(pcm_generation_matches(7, 7), "当前世代必须接收");
        assert!(
            !pcm_generation_matches(6, 7),
            "seek 前 producer 已排进通道的旧世代必须丢弃"
        );
    }

    /// 没有音轨：**立即结束且不算错误**（如实降级的样板），设备都不碰。
    #[test]
    fn video_only_file_degrades_without_error() {
        let Some(path) = gen_clip_video_only("video-only.mp4") else {
            return;
        };
        let h = AudioTrackHandle::spawn(&path, 0, false).expect("应能起播放音频链");
        wait_eof(&h, Duration::from_secs(10));
        assert!(h.is_eof(), "没有音频流应立即结束");
        let s = h.stats();
        assert_eq!(s.decoded_frames, 0, "没有音轨就不该解出任何音频");
        assert!(!s.output_live, "没有音轨不该打开渲染设备");
    }
}
