//! 单路音频采集（系统声音环回 / 麦克风），统一输出 **48 kHz / 2ch / f32 交错**。
//!
//! 关键点（来自 wasapi 0.26 的官方示例）：
//! * 环回采集要**打开默认渲染设备**，但 `initialize_client` 传 `Direction::Capture`
//!   —— wasapi 会在"设备是 Render + 请求 Capture + 共享模式"时自动加上
//!   `AUDCLNT_STREAMFLAGS_LOOPBACK`。
//! * `StreamMode::EventsShared { autoconvert: true, .. }` 让 WASAPI 直接转换成我们
//!   请求的格式（32 位 float / 48 kHz / 立体声），因此**不需要手写格式转换与重采样**。
//! * 事件驱动 + `get_next_packet_size` 循环取包，避免轮询。
//!
//! V2-1（10 分钟实机取证后新增，见 `docs/PENDING.md` P31 补充 / `docs/DESIGN.md` §11.29.7）：
//! * [`AudioCapture::read_chunk_core`] 的失败**带分型**（[`ReadFault`]）：设备失效类可重建会话，
//!   其余按致命处理。分型依据是 HRESULT 里的 `AUDCLNT_E_*` 码（[`fault_of_wasapi`]）。
//! * [`DryoutTracker`] 记账"连续空窗"：WASAPI 源头断供与音频引擎静默都表现为
//!   "连续多轮取不到数据"，这是它们**可被观测的唯一形式**（实测断供 353 ms）。

use crate::audio::devices::ensure_com;
use crate::error::AppError;
use wasapi::{AudioCaptureClient, AudioClient, Direction, SampleType, StreamMode, WaveFormat};

/// **音频"连续空窗"超过它就告警（毫秒）**（V2-1）
///
/// 一轮读取取不到数据 ≈ 一个轮询周期（`AUDIO_READ_POLL_MS`，200 ms）。
/// 250 ms 意味着"至少两轮连续为空"：静默期环回偶发一轮空转是正常的，
/// 连续多轮空转大概率是源头真的断了（实测案例 353 ms）。只告警不动作——
/// 时间轴由墙钟补静音兜住，但内容缺口必须留痕。
pub const DRYOUT_WARN_MS: u64 = 250;

/// 目标采样率（Hz）（**唯一来源**：`crate::audio::SAMPLE_RATE`；这里按 `usize` 用，采集侧算缓冲字节数更顺手）
pub const TARGET_RATE: usize = crate::audio::SAMPLE_RATE as usize;
/// 目标声道数（**唯一来源**：`crate::audio::CHANNELS`）
pub const TARGET_CHANNELS: usize = crate::audio::CHANNELS;
/// 每样本字节数（f32）（**唯一来源**：`crate::audio::BYTES_PER_SAMPLE`）
pub const BYTES_PER_SAMPLE: usize = crate::audio::BYTES_PER_SAMPLE;
/// 每帧字节数
pub const BLOCK_ALIGN: usize = TARGET_CHANNELS * BYTES_PER_SAMPLE;

/// 音频源种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// 系统声音（渲染设备环回）
    System,
    /// 麦克风（采集设备）
    Microphone,
}

pub struct AudioCapture {
    kind: SourceKind,
    client: AudioClient,
    capture: AudioCaptureClient,
    event: wasapi::Handle,
    scratch: Vec<u8>,
    /// 累计采集到的帧数（1 帧 = 2ch 各一个样本）
    pub frames: u64,
    /// 事件超时次数（无数据）
    pub timeouts: u64,
}

impl AudioCapture {
    pub fn open(kind: SourceKind, device: wasapi::Device) -> Result<Self, AppError> {
        // 统一走 `ensure_com`：它把 `RPC_E_CHANGED_MODE`（主线程已是 STA）当成成功，
        // 否则"仅声音模式"在 UI 线程上启动录制会直接失败（见 devices::com_ready）。
        ensure_com()?;

        let mut client = device
            .get_iaudioclient()
            .map_err(|e| AppError::Audio(format!("取 IAudioClient 失败: {e}")))?;

        let fmt = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            TARGET_RATE,
            TARGET_CHANNELS,
            None,
        );
        let (def_time, min_time) = client
            .get_device_period()
            .map_err(|e| AppError::Audio(format!("取设备周期失败: {e}")))?;

        // autoconvert：让 WASAPI 负责采样率/声道/格式转换
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: def_time.max(min_time),
        };
        client
            .initialize_client(&fmt, &Direction::Capture, &mode)
            .map_err(|e| {
                AppError::Audio(format!(
                    "初始化{}采集失败: {e}（该设备可能不支持自动格式转换）",
                    if kind == SourceKind::System { "系统声音" } else { "麦克风" }
                ))
            })?;

        let event = client
            .set_get_eventhandle()
            .map_err(|e| AppError::Audio(format!("取事件句柄失败: {e}")))?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|e| AppError::Audio(format!("取 IAudioCaptureClient 失败: {e}")))?;
        client
            .start_stream()
            .map_err(|e| AppError::Audio(format!("启动采集失败: {e}")))?;

        let buffer_frames = client.get_buffer_size().unwrap_or(0);
        tracing::info!(
            ?kind,
            rate = TARGET_RATE,
            channels = TARGET_CHANNELS,
            buffer_frames,
            "音频采集已启动"
        );

        Ok(Self {
            kind,
            client,
            capture,
            event,
            scratch: vec![0u8; BLOCK_ALIGN * buffer_frames.max(1024) as usize],
            frames: 0,
            timeouts: 0,
        })
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    /// 采集缓冲帧数（仅用于日志/诊断）
    pub fn buffer_frames(&self) -> u32 {
        self.client.get_buffer_size().unwrap_or(0)
    }

    /// 把当前可用的音频追加到 `out`（交错 f32），返回本次追加的**帧数**。
    ///
    /// `timeout_ms > 0` 时先等事件（避免忙等），但**无论事件是否触发都要尝试排空缓冲**——
    /// 事件只是"有新数据的提示"，缓冲区才是真相。早期版本在事件未触发时直接返回，
    /// 导致 WASAPI 环形缓冲被覆盖，实测丢了约 1/3 的音频（音频时长 3.18s vs 视频 4.80s）。
    ///
    /// 失败分类 [`ReadFault`]，由调用方决定"重建会话"还是"整个录制失败"。
    pub fn read_chunk_core(
        &mut self,
        out: &mut Vec<f32>,
        timeout_ms: u32,
    ) -> Result<usize, ReadFailure> {
        if timeout_ms > 0 && self.event.wait_for_event(timeout_ms).is_err() {
            self.timeouts += 1;
        }

        let before = out.len();
        loop {
            let frames = match self.capture.get_next_packet_size() {
                Ok(Some(n)) if n > 0 => n as usize,
                Ok(_) => break,
                Err(e) => {
                    return Err(ReadFailure {
                        fault: fault_of_wasapi(&e),
                        error: AppError::Audio(format!("取包大小失败: {e}")),
                    });
                }
            };
            let need = frames * BLOCK_ALIGN;
            if self.scratch.len() < need {
                self.scratch.resize(need, 0);
            }
            let (read, _info) = match self.capture.read_from_device(&mut self.scratch[..need]) {
                Ok(v) => v,
                Err(e) => {
                    return Err(ReadFailure {
                        fault: fault_of_wasapi(&e),
                        error: AppError::Audio(format!("读取音频数据失败: {e}")),
                    });
                }
            };
            if read == 0 {
                break;
            }
            let bytes = read as usize * BLOCK_ALIGN;
            // 安全地按小端 f32 解包（48 kHz 立体声下这点开销可以忽略）
            out.extend(
                self.scratch[..bytes]
                    .chunks_exact(BYTES_PER_SAMPLE)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            );
            self.frames += read as u64;
        }
        Ok((out.len() - before) / TARGET_CHANNELS)
    }

    /// [`Self::read_chunk_core`] 的便捷包装：失败时丢掉分型，只返回错误。
    pub fn read_chunk(&mut self, out: &mut Vec<f32>, timeout_ms: u32) -> Result<usize, AppError> {
        self.read_chunk_core(out, timeout_ms).map_err(|f| f.error)
    }

    pub fn stop(&mut self) {
        let _ = self.client.stop_stream();
    }

}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

// SAFETY: 一个 `AudioCapture` **只被一个线程拥有**：在启动时创建，然后移交给音频线程独占使用
// （录制线程结构保证不存在并发访问）。WASAPI 的接口都在 MTA 里创建，MTA 对象可自由跨线程使用；
// 音频线程进入时会再次调用 `ensure_com()`。
unsafe impl Send for AudioCapture {}

/// WASAPI 读取失败的分型（V2-1）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    /// **设备/会话失效**：用户切换默认输出设备、拔插声卡、音频服务重启、
    /// exclusive 模式被抢占等。**重建采集会话即可恢复**，不该让整个录制失败。
    DeviceLost,
    /// 其他失败：重建大概率无用，按致命处理。
    Fatal,
}

/// 带分型的读取失败（V2-1）
#[derive(Debug)]
pub struct ReadFailure {
    pub fault: ReadFault,
    pub error: AppError,
}

/// 把 wasapi 错误分型成"可重建"或"致命"（**纯函数**，单测覆盖）。
///
/// 依据是 `windows` 0.62.2 的 `AUDCLNT_E_*` 码（音频客户端错误都从 `0x88890000` 起）：
/// * `AUDCLNT_E_DEVICE_INVALIDATED`（`0x88890004`）：设备被拔掉/默认设备被切换，
///   这是最典型的"重建即可恢复"。
/// * `AUDCLNT_E_SERVICE_NOT_RUNNING`（`0x88890010`）：Windows Audio 服务重启中，
///   服务回来后同一个设备仍可重开。
/// * `AUDCLNT_E_UNSUPPORTED_FORMAT`（`0x88890008`）：换设备后 mix format 可能变化，
///   重开时可能短暂报"格式不受支持"，值得再试一次。
/// * `ERROR_NOT_FOUND`（`0x80070490`）：设备枚举里已经没有它（切换默认设备的竞态）。
///
/// 其余（含 `WasapiError::DeviceNotFound` 之外的所有 wasapi 层错误）一律按致命：
/// 它们是配置/用法错误，重建只会得到同样的结果。
pub fn fault_of_wasapi(e: &wasapi::WasapiError) -> ReadFault {
    match e {
        wasapi::WasapiError::Windows(err) => {
            // HRESULT(pub i32)：负数按补码存，转回 u32 才是文档里的那个码
            let code = err.code().0 as u32;
            match code {
                0x8889_0004 | 0x8889_0010 | 0x8889_0008 | 0x8007_0490 => ReadFault::DeviceLost,
                _ => ReadFault::Fatal,
            }
        }
        // 按名字解析不到设备 = 默认设备被换掉了
        wasapi::WasapiError::DeviceNotFound(_) => ReadFault::DeviceLost,
        _ => ReadFault::Fatal,
    }
}

/// **音频"连续空窗"记账器（V2-1）**
///
/// 一次读取取不到数据记为"空转一轮"（墙上耗时由调用方传入）。两个成因：
/// ① 音频引擎静默——环回本来就不产包；② 源头断供——设备切换/会话失效瞬间。
/// 两者对时间轴的影响都由消费线程的墙钟补静音兜住，但**内容真的缺了一段**，
/// 所以必须留痕：越过 [`DRYOUT_WARN_MS`] 时由调用方告警一次（同一连续段只报一次）。
#[derive(Debug, Clone)]
pub struct DryoutTracker {
    /// 当前连续空窗时长（ms）
    streak_ms: u64,
    /// 累计空窗时长（ms，含引擎静默）
    total_ms: u64,
    /// 最长一次连续空窗（ms）
    max_ms: u64,
    /// 告警阈值（ms）
    threshold_ms: u64,
    /// 当前连续段是否已告警过
    warned: bool,
}

impl Default for DryoutTracker {
    fn default() -> Self {
        Self::new(DRYOUT_WARN_MS)
    }
}

impl DryoutTracker {
    pub fn new(threshold_ms: u64) -> Self {
        Self {
            streak_ms: 0,
            total_ms: 0,
            max_ms: 0,
            threshold_ms,
            warned: false,
        }
    }

    /// 记一轮"没取到数据"，`elapsed_ms` 是这一轮的墙上耗时。
    /// 返回 `true` 表示本次连续空窗**首次**越过阈值（调用方应告警一次）。
    pub fn on_idle(&mut self, elapsed_ms: u64) -> bool {
        self.streak_ms += elapsed_ms;
        self.total_ms += elapsed_ms;
        self.max_ms = self.max_ms.max(self.streak_ms);
        if !self.warned && self.streak_ms >= self.threshold_ms {
            self.warned = true;
            true
        } else {
            false
        }
    }

    /// 取到数据：结清当前连续段（清零"已告警"，下次空窗重新计）
    pub fn on_data(&mut self) {
        self.streak_ms = 0;
        self.warned = false;
    }

    pub fn streak_ms(&self) -> u64 {
        self.streak_ms
    }

    pub fn total_ms(&self) -> u64 {
        self.total_ms
    }

    pub fn max_ms(&self) -> u64 {
        self.max_ms
    }
}

/// 计算一段交错 f32 音频的均方根（用于"真的有声音"的客观断言）
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// 估算一路音频的真实采样率（按累计帧数与墙上时间），用于校验 autoconvert 是否按 48 kHz 出数
pub fn estimate_rate(frames: u64, elapsed: std::time::Duration) -> f64 {
    if elapsed.as_secs_f64() <= 0.0 {
        return 0.0;
    }
    frames as f64 / elapsed.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_known_signals() {
        assert_eq!(rms(&[]), 0.0);
        assert!((rms(&[1.0, -1.0, 1.0, -1.0]) - 1.0).abs() < 1e-6);
        assert!((rms(&[0.0, 0.0]) - 0.0).abs() < 1e-6);
        // 0.5 幅值正弦的 RMS ≈ 0.3536
        let s: Vec<f32> = (0..4800)
            .map(|i| 0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin())
            .collect();
        assert!((rms(&s) - 0.3536).abs() < 0.01, "rms={}", rms(&s));
    }

    #[test]
    fn block_align_matches_format() {
        assert_eq!(BLOCK_ALIGN, 8);
        assert_eq!(TARGET_CHANNELS, 2);
    }

    /// 构造一个携带指定 HRESULT 的 wasapi 错误（`windows::core` 即 `windows_core`，
    /// 与 wasapi 的错误载体是同一类型）
    fn windows_fault(code: u32) -> wasapi::WasapiError {
        wasapi::WasapiError::Windows(windows::core::Error::from(windows::core::HRESULT(
            code as i32,
        )))
    }

    #[test]
    fn fault_of_wasapi_classifies_rebuildable_errors() {
        // 设备失效/服务未运行/格式变化/设备没了 → 可重建
        for code in [0x8889_0004_u32, 0x8889_0010, 0x8889_0008, 0x8007_0490] {
            assert_eq!(
                fault_of_wasapi(&windows_fault(code)),
                ReadFault::DeviceLost,
                "HRESULT 0x{code:08X} 应判为可重建"
            );
        }
        // 按名字找不到设备（切换默认设备）→ 可重建
        assert_eq!(
            fault_of_wasapi(&wasapi::WasapiError::DeviceNotFound("gone".into())),
            ReadFault::DeviceLost
        );
        // 其余一律致命：E_FAIL、wasapi 层自己的配置错误
        assert_eq!(fault_of_wasapi(&windows_fault(0x8000_4005)), ReadFault::Fatal);
        assert_eq!(fault_of_wasapi(&wasapi::WasapiError::ClientNotInit), ReadFault::Fatal);
        assert_eq!(
            fault_of_wasapi(&wasapi::WasapiError::UnsupportedFormat),
            ReadFault::Fatal
        );
    }

    #[test]
    fn dryout_tracker_warns_once_per_streak() {
        let mut d = DryoutTracker::new(250);
        assert!(!d.on_idle(100));
        assert!(!d.on_idle(100));
        assert!(d.on_idle(100), "累计 300ms 空窗应触发一次告警");
        assert!(!d.on_idle(100), "同一连续段只告警一次");
        assert_eq!(d.streak_ms(), 400);
        // 取到数据：连续段结清
        d.on_data();
        assert_eq!(d.streak_ms(), 0);
        assert_eq!(d.total_ms(), 400, "total 只累计，不随 on_data 清零");
        // 新的连续段重新达到阈值 → 再告警一次
        assert!(d.on_idle(300));
        assert_eq!(d.max_ms(), 400, "max 取历史最大连续段");
        assert_eq!(d.total_ms(), 700);
    }

    #[test]
    fn dryout_tracker_default_threshold() {
        let mut d = DryoutTracker::default();
        assert!(!d.on_idle(DRYOUT_WARN_MS - 1));
        assert!(d.on_idle(1), "默认阈值 250ms：累计达到即告警");
    }
}
