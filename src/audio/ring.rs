//! 音频两层环形缓冲：**采集线程永不阻塞**（参考 `add/` 方案的 TwoStageAudioRing）。
//!
//! ## 为什么需要它
//!
//! 改造前，音频线程一个人干三件事：等 WASAPI 事件 → 读 PCM → 混音 → AAC → 送封装队列
//! （队列满时还会阻塞最多 200 ms）。只要这条链上任何一环变慢（CPU 被抢占、封装满），
//! **WASAPI 客户端内部的环形缓冲就会积压并最终被覆盖** —— 表现就是录屏最常见的
//! "爆音 / 周期性丢音"，而且是不可恢复的（设备缓冲里的数据没有被任何人读过）。
//!
//! 参考方案的做法是**把硬件读取与后续全部业务逻辑解耦**，中间垫两层环形缓冲：
//!
//! ```text
//! WASAPI 读取线程 ──push──▶ 小环(0.2s) ──▶ 搬运线程(Highest) ──▶ 大环(10s) ──▶ 消费线程(AboveNormal)
//!   只做内存拷贝           防抖               随时可跑          吸收消费侧停顿      混音+AAC+入队
//! ```
//!
//! * **小环（0.2 s）**：兑现"读取线程不等待"。满了就丢**最旧**样本、保最新
//!   （与参考方案视频 BGRA 队列"满则丢旧保新"同一策略），并计入 `dropped`。
//!   正常情况下搬运线程在微秒级就把小环掏空，命中它说明机器已经被拖到极限。
//! * **大环（10 s）**：消费侧停顿（AAC 抖动、封装队列背压 200 ms）全部由它吸收，
//!   超过 10 s 才真正开始丢数据 —— 那已经不是调优能救的场景了。
//! * **搬运线程跑 `Highest`**（`crate::sys`）：它唯一的职责就是把数据搬进大缓冲，
//!   必须随时可跑；这也是参考方案里唯一用最高优先级的线程。
//!
//! 实现说明：SPSC（单生产/单消费）语义下用 `Mutex<VecDeque<f32>>` + `Condvar`
//! 就够了 —— 临界区只是一次 `extend`/`drain`，锁无竞争，比引新依赖划算；
//! 丢失语义（满则丢最旧）与可等待都是现成的。

use crate::sync::lock_or_recover;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::audio::capture::{TARGET_CHANNELS, TARGET_RATE};
use crate::sys::{set_current_thread_priority, ThreadPriority};

/// 小环容量（秒）：只用于吸收"搬运线程还没来"的调度抖动。
pub(crate) const SMALL_RING_SECONDS: f64 = 0.200;
/// 大环容量（秒）：消费侧停顿兜底（AAC/封装背压都被它吸收）。
pub(crate) const BIG_RING_SECONDS: f64 = 10.0;
/// 搬运线程空转时的等待粒度（等待期间会被 push 立即唤醒）。
/// 2 ms 在完全静音时会产生约 500 次/秒的空唤醒；10 ms 足够跟手，又能把唤醒降一个量级。
const TRANSPORT_IDLE_WAIT: Duration = Duration::from_millis(10);

/// 秒数 → 样本数容量（48 kHz 立体声 f32 交错）。
fn capacity_samples(seconds: f64) -> usize {
    ((seconds * TARGET_RATE as f64).round() as usize).max(1) * TARGET_CHANNELS
}

/// 单生产单消费音频环形缓冲（f32 样本）。
///
/// 生产端只能有一个线程调 [`AudioRing::push_slice`]，消费端只能有一个线程调
/// [`AudioRing::pop_available_into`] / [`AudioRing::wait_data`]（SPSC 约定）。
pub struct AudioRing {
    buf: Mutex<VecDeque<f32>>,
    capacity: usize,
    data_ready: Condvar,
    dropped: AtomicU64,
}

impl AudioRing {
    pub fn with_seconds(seconds: f64) -> Self {
        Self {
            buf: Mutex::new(VecDeque::new()),
            capacity: capacity_samples(seconds),
            data_ready: Condvar::new(),
            dropped: AtomicU64::new(0),
        }
    }

    /// 容量（样本数，不是帧数）
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前堆积样本数
    pub fn len(&self) -> usize {
        lock_or_recover(&self.buf).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 因"满"被丢弃的样本数（**始终丢最旧、保最新**）。
    pub fn dropped_samples(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 生产端：追加样本。满了先丢最旧的，保证新数据进得去。返回写入的样本数。
    pub fn push_slice(&self, data: &[f32]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let mut buf = lock_or_recover(&self.buf);
        let need = data.len();
        if need >= self.capacity {
            // 单批就超过整环容量（正常不会发生：一批 ≈ 一个设备周期几毫秒）：
            // 旧数据全部作废，只保留本批末尾一满环。
            let keep_from = need - self.capacity;
            self.dropped
                .fetch_add((buf.len() + keep_from) as u64, Ordering::Relaxed);
            buf.clear();
            buf.extend(data[keep_from..].iter().copied());
        } else {
            let avail = self.capacity - buf.len();
            if need > avail {
                let evict = need - avail;
                self.dropped.fetch_add(evict as u64, Ordering::Relaxed);
                buf.drain(..evict);
            }
            buf.extend(data.iter().copied());
        }
        drop(buf);
        self.data_ready.notify_all();
        need
    }

    /// 消费端：把当前**全部**可取样本追加进 `out`（非阻塞）。返回取走的帧数。
    pub fn pop_available_into(&self, out: &mut Vec<f32>) -> usize {
        let mut buf = lock_or_recover(&self.buf);
        let n = buf.len();
        out.extend(buf.drain(..));
        n / TARGET_CHANNELS
    }

    /// 消费端：等到"有数据可取"或超时（返回是否确有数据；仍需 `pop_available_into` 取）。
    pub fn wait_data(&self, timeout: Duration) -> bool {
        let buf = lock_or_recover(&self.buf);
        if !buf.is_empty() {
            return true;
        }
        let (guard, _) = self
            .data_ready
            .wait_timeout(buf, timeout)
            .unwrap_or_else(|e| e.into_inner());
        !guard.is_empty()
    }

    /// 叫醒所有在等数据的消费端（停止/唤醒用）。
    pub fn notify(&self) {
        let _guard = lock_or_recover(&self.buf);
        self.data_ready.notify_all();
    }
}

/// 一层音频网关 = 小环 + 搬运线程 + 大环（见模块文档）。
///
/// 每路音源（系统声音 / 麦克风）一个实例。生命周期：
/// `new()` → `start_transport()` → 读取线程 `push()` / 消费线程 `pop_available()` → `stop()`。
///
/// # 线程安全约定
/// 搬运线程持有 `Arc<Self>`，对象生命周期由引用计数保证，不再依赖“先 stop 再析构”的
/// 人肉约定。`stop()` 仍负责置位并 join，让收尾及时；忘记调用时也会在最后一个 `Arc`
/// 释放后由 `Drop` 兜底。小环只被"读取线程写、搬运线程读"，大环只被
/// "搬运线程写、消费线程读"（SPSC），与 `AudioRing` 的约定一致。
pub struct AudioGateway {
    small: AudioRing,
    big: AudioRing,
    stop: AtomicBool,
    transport: Mutex<Option<thread::JoinHandle<()>>>,
}

impl AudioGateway {
    pub fn new() -> Self {
        Self {
            small: AudioRing::with_seconds(SMALL_RING_SECONDS),
            big: AudioRing::with_seconds(BIG_RING_SECONDS),
            stop: AtomicBool::new(false),
            transport: Mutex::new(None),
        }
    }

    /// 小环（供读取线程写入 / 诊断）
    pub fn small(&self) -> &AudioRing {
        &self.small
    }

    /// 大环（供消费线程读取 / 诊断）
    pub fn big(&self) -> &AudioRing {
        &self.big
    }

    /// 读取线程调用：写入小环（永不阻塞；满了丢最旧并计数）。
    pub fn push(&self, pcm: &[f32]) {
        self.small.push_slice(pcm);
    }

    /// 消费线程调用：非阻塞取走大环当前全部样本。返回帧数。
    pub fn pop_available(&self, out: &mut Vec<f32>) -> usize {
        self.big.pop_available_into(out)
    }

    /// 消费线程调用：等大环有数据（超时返回 false）。
    pub fn wait_data(&self, timeout: Duration) -> bool {
        self.big.wait_data(timeout)
    }

    /// 两层合计被丢弃的样本数（小环 + 大环）。
    pub fn dropped_samples(&self) -> u64 {
        self.small.dropped_samples() + self.big.dropped_samples()
    }

    /// 启动搬运线程（`Highest` 优先级）。重复调用只会启动一个。
    ///
    /// P52：返回值从 `()` 改成 `Result` —— 原来是 `.expect("创建音频搬运线程失败")`，
    /// 而这里是**录制启动路径**，spawn 失败（线程数打满 / 资源不足）会直接把整个进程带崩。
    /// 全仓其它四处线程创建都走 `map_err → AppError`，这里没有理由例外：
    /// 交给调用方转成一条用户能看到的错误，而不是 panic。
    pub fn start_transport(self: &Arc<Self>) -> Result<(), std::io::Error> {
        let mut slot = lock_or_recover(&self.transport);
        if slot.is_some() {
            return Ok(());
        }
        // 线程只持 `Weak`：外部最后一个 `Arc` 释放时 `Drop` 仍能执行并 join；
        // 若线程持有强引用，"忘记 stop" 就会让对象和线程互相续命。
        let gateway = Arc::downgrade(self);
        let handle = thread::Builder::new()
            .name("cap-audio-transport".into())
            .spawn(move || {
                transport_loop(&gateway);
            })?;
        *slot = Some(handle);
        Ok(())
    }

    /// 停止搬运线程并等它退出（录制停止收尾时调用；重复调用安全）。
    ///
    /// 顺序：**先 set 标志再 join**。搬运线程最多再跑一轮（`wait_data` 10 ms 超时，
    /// 且 `notify` 会立即叫醒它），所以 join 不会卡。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.small.notify();
        self.big.notify();
        let handle = lock_or_recover(&self.transport).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl Default for AudioGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioGateway {
    fn drop(&mut self) {
        // 兜底只叫停、不 join：最后一个 `Arc` 可能正是在搬运线程里释放的，
        // 此时 join 自己会死锁。正常路径由 `stop()` 负责 join。
        self.stop.store(true, Ordering::SeqCst);
        self.small.notify();
        self.big.notify();
    }
}

/// 搬运线程主体：小环 → 大环。跑在 `Highest` 优先级（见 [`crate::sys`]）。
fn transport_loop(weak: &std::sync::Weak<AudioGateway>) {
    set_current_thread_priority(ThreadPriority::Highest);
    let mut chunk: Vec<f32> = Vec::new();
    loop {
        // 每轮短暂升级：等待期间不长期持有强引用，否则析构永远等不到。
        let Some(gw) = weak.upgrade() else {
            break;
        };
        if gw.stop.load(Ordering::SeqCst) {
            break;
        }
        let got = gw.small.pop_available_into(&mut chunk);
        if got > 0 {
            // 大环满（消费侧停了 10 s 以上）：丢最旧，照样搬 —— 保最新
            gw.big.push_slice(&chunk);
            chunk.clear();
            // 立刻再检查一轮小环，尽量把小环掏空
            continue;
        }
        // 小环空了：等 10 ms 或被新数据唤醒
        gw.small.wait_data(TRANSPORT_IDLE_WAIT);
    }
    // 收尾：把残留搬完（停止后不会有新数据进来了）
    if let Some(gw) = weak.upgrade() {
        let got = gw.small.pop_available_into(&mut chunk);
        if got > 0 {
            gw.big.push_slice(&chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 生产/消费顺序与内容：推进去的样本按原顺序读出来。
    #[test]
    fn ring_preserves_order() {
        let r = AudioRing::with_seconds(1.0);
        let frames = 100;
        let data: Vec<f32> = (0..frames * TARGET_CHANNELS).map(|i| i as f32).collect();
        assert_eq!(r.push_slice(&data), data.len());
        let mut out = Vec::new();
        assert_eq!(r.pop_available_into(&mut out), frames);
        assert_eq!(out, data);
        // 取完了就该空
        assert_eq!(r.pop_available_into(&mut out), 0);
    }

    /// 满则丢**最旧**、保最新，并计数。
    #[test]
    fn overflow_drops_oldest_and_keeps_newest() {
        let r = AudioRing::with_seconds(1.0); // 48000 帧容量
        let chunk: Vec<f32> = (0..48_000 * TARGET_CHANNELS).map(|i| i as f32).collect();
        r.push_slice(&chunk); // 正好填满 1 秒
        assert_eq!(r.len(), chunk.len());

        // 再推 0.1 秒：应把最旧的 0.1 秒挤出去
        let more: Vec<f32> = (48_000 * TARGET_CHANNELS..(48_000 + 4_800) * TARGET_CHANNELS)
            .map(|i| i as f32)
            .collect();
        r.push_slice(&more);
        assert_eq!(r.dropped_samples(), 4_800 * TARGET_CHANNELS as u64);

        let mut out = Vec::new();
        r.pop_available_into(&mut out);
        // 剩下的 = 原 0.9 s + 新 0.1 s，首样本是"0.1 秒处"那个
        assert_eq!(out.len(), 48_000 * TARGET_CHANNELS);
        assert_eq!(out[0], 4_800.0 * TARGET_CHANNELS as f32);
        assert_eq!(*out.last().unwrap(), more.last().copied().unwrap());
    }

    /// 单批超过整环容量：只保留本批末尾一满环，旧数据全废。
    #[test]
    fn oversized_chunk_keeps_only_its_tail() {
        let r = AudioRing::with_seconds(0.2); // 9600 帧
        let big: Vec<f32> = (0..(9_600 + 100) * TARGET_CHANNELS)
            .map(|i| i as f32)
            .collect();
        r.push_slice(&big);
        assert_eq!(r.len(), 9_600 * TARGET_CHANNELS);
        let mut out = Vec::new();
        r.pop_available_into(&mut out);
        assert_eq!(out.last().copied(), Some(big[big.len() - 1]));
        assert_eq!(r.dropped_samples(), 100 * TARGET_CHANNELS as u64);
    }

    /// `wait_data`：空等超时返回 false；有数据立即醒来。
    #[test]
    fn wait_data_wakes_on_push_or_timeout() {
        let r = AudioRing::with_seconds(1.0);
        assert!(!r.wait_data(Duration::from_millis(30)), "空环应等超时");
        r.push_slice(&[1.0, 2.0]);
        assert!(r.wait_data(Duration::from_millis(10)));
        assert_eq!(r.len(), 2);
    }

    /// 两层网关的端到端：push 进小环 → 搬运线程搬进大环 → 消费端读到的就是原数据。
    #[test]
    fn gateway_moves_small_to_big() {
        let gw = Arc::new(AudioGateway::new());
        gw.start_transport().expect("搬运线程应能启动");
        let data: Vec<f32> = (0..4_800 * TARGET_CHANNELS)
            .map(|i| i as f32 * 0.001)
            .collect();
        gw.push(&data);
        // 等搬运线程干活（微秒级；给 2 s 足够覆盖负载高的机器）
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while gw.big().len() < data.len() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut out = Vec::new();
        assert_eq!(gw.pop_available(&mut out), 4_800);
        assert_eq!(out, data, "搬运不应改变样本内容与顺序");
        assert_eq!(gw.dropped_samples(), 0, "顺畅路径不应丢样本");
        gw.stop();
    }

    /// 停止后搬运线程退出、残留小环数据被搬空（stop 不吞数据）。
    #[test]
    fn stop_flushes_the_small_ring() {
        let gw = Arc::new(AudioGateway::new());
        gw.start_transport().expect("搬运线程应能启动");
        // 直接把数据塞进小环（不等搬运），然后立即 stop
        let data: Vec<f32> = (0..1_000).map(|i| i as f32).collect();
        gw.small().push_slice(&data);
        gw.stop();
        let mut out = Vec::new();
        assert_eq!(gw.pop_available(&mut out), 500, "stop 应把残留搬进大环");
        assert_eq!(out, data);
    }

    /// 压力冒烟：持续快速 push 而不消费，搬运/丢弃路径都不应 panic 或卡死。
    /// 25 秒的数据塞进 10 秒的大环 → 必然触发"丢最旧保最新"路径。
    #[test]
    fn gateway_survives_sustained_push_without_consumer() {
        let gw = Arc::new(AudioGateway::new());
        gw.start_transport().expect("搬运线程应能启动");
        let chunk: Vec<f32> = (0..48_000 * TARGET_CHANNELS) // 1 秒/批
            .map(|i| (i % 97) as f32)
            .collect();
        for _ in 0..25 {
            gw.push(&chunk);
            std::thread::sleep(Duration::from_millis(1));
        }
        // 没人消费 → 大环迟早丢最旧
        assert!(gw.dropped_samples() > 0, "没人消费就该丢最旧保最新");
        gw.stop();
        // stop 后搬运线程已 join：再 push 只会堆在小环里，不会 panic
        let before = gw.small().len();
        gw.push(&chunk);
        assert!(gw.small().len() > before);
    }

    /// 容量口径：0.2 s / 10 s 在 48 kHz 立体声下是预期的样本数。
    #[test]
    fn capacities_match_documentation() {
        let small = AudioRing::with_seconds(SMALL_RING_SECONDS);
        let big = AudioRing::with_seconds(BIG_RING_SECONDS);
        assert_eq!(small.capacity(), 9_600 * TARGET_CHANNELS);
        assert_eq!(big.capacity(), 480_000 * TARGET_CHANNELS);
    }

    /// 网关可跨线程共享（Arc）—— 录制侧的用法。
    #[test]
    fn gateway_is_shareable() {
        let gw = Arc::new(AudioGateway::new());
        let g2 = Arc::clone(&gw);
        let t = std::thread::spawn(move || {
            g2.start_transport().expect("搬运线程应能启动");
            g2.push(&[0.5, -0.5]);
        });
        t.join().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while gw.big().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!gw.big().is_empty());
        gw.stop();
    }

    /// 忘记 `stop()` 时，最后一个 `Arc` 释放必须真的回收对象与搬运线程；
    /// 搬运线程只持 `Weak`，不能反过来把对象吊住。
    #[test]
    fn dropping_last_arc_without_stop_reclaims_gateway() {
        let gw = Arc::new(AudioGateway::new());
        gw.start_transport().expect("搬运线程应能启动");
        let weak = Arc::downgrade(&gw);
        drop(gw);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while weak.upgrade().is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(weak.upgrade().is_none(), "忘记 stop 也不得泄漏网关");
    }
}
