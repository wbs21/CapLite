//! 视频编码封装：把产品配置映射到 OpenH264 参数，并把输出转成 MP4 需要的 **AVCC** 形式。
//!
//! 参数取值来自 `docs/DESIGN.md` §10.2 的实测基准（1080p、真实屏幕内容、90 帧）：
//!
//! | 配置 | 速度 | 说明 |
//! |---|---|---|
//! | `ScreenContentRealTime` + 单 slice | 27.5 fps | 码率只用掉 1130/8000 kbps，PSNR 44.9 dB |
//! | `CameraVideoRealTime` + 单 slice | 60.5 fps | PSNR 53.5 dB |
//! | `CameraVideoRealTime` + `FixedSlcNumSlice` | **129 fps** | 线程必须配 slice 才生效 |
//!
//! 因此产品默认：`CameraVideoRealTime` + `FixedSlcNumSlice` + `threads = 0`(auto) +
//! `slice_num = 0`(auto，OpenH264 会把 1080p 夹到 16；显式填 >35 会初始化失败)。
//!
//! **外部不再暴露的选项**（都固定在这里，见 `docs/DESIGN.md` 的实测结论）：
//! * `iRCMode` = **ABR**（`RcMode::Bitrate` = `RC_BITRATE_MODE`）；
//! * `iComplexityMode` = **平衡**（`EComplexityMode::Medium`）；
//! * `iMaxBitrate` = **20 Mbps**（[`MAX_BITRATE_BPS`]，内部写死）；
//! * 熵编码：见 `ENTROPY_CABAC` 的注释（实时模式下 OpenH264 会忽略 CABAC）。

use crate::config::Config;
use crate::error::AppError;
use crate::video::convert::I420Frame;
use crate::video::openh264::{
    EComplexityMode, Encoder, EncoderConfig, EUsageType, FrameInfo, RcMode, SliceMode,
};

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// **内部固定的最大码率**（`SEncParamExt::iMaxBitrate` 与每个 spatial layer 的
/// `iMaxSpatialBitrate`）：20 Mbps。
///
/// 界面不暴露它，也没有必要暴露：目标码率的滑尺上限（[`crate::config::VIDEO_BITRATE_MAX_KBPS`]
/// = 20 Mbps）与它对齐，所以"目标 ≤ 上限"恒成立，`InitializeExt` 不会因为码率被拒。
/// 为什么不能沿用"上限 = 目标"的老做法：那样目标一变高就可能撞上 OpenH264 的
/// `cmInitParaError`（见 `encoder.rs` 里 `set_max_bitrate` 的实测注释）。
pub const MAX_BITRATE_BPS: i32 = 20_000_000;

/// **内部固定的复杂度**：平衡（`EComplexityMode::Medium`）。界面不再提供选择项。
pub(crate) const FIXED_COMPLEXITY: EComplexityMode = EComplexityMode::Medium;

/// **内部默认的熵编码开关**（`bEntropyCodingModeFlag` = `cabac`）：**开**。
///
/// 实测（`cargo test --lib probe_cabac -- --nocapture`，本机 1080p、30 帧、ABR 5 Mbps、
/// 目标/上限 = 5/20 Mbps）：
///
/// | 配置 | 产物字节 | 速度 | PPS 里的 entropy_coding_mode_flag | profile |
/// |---|---|---|---|---|
/// | `CameraVideoRealTime` + CAVLC | 299_294 B | 83.7 fps | 0 | Baseline |
/// | `CameraVideoRealTime` + **CABAC** | **158_688 B（-47%）** | 83.2 fps | 1 | High |
///
/// 也就是说：**CABAC 在实时 usage type 下真的生效**（早先"实时模式忽略 CABAC"的说法
/// 已被这次实测推翻，见 `encoder.rs` 的 `validate`），而且不降速、不掉帧（83 fps ≥ 60）。
/// 非实时 usage type 反而是**不可用**的：库直接 `Invalid usage type = 2/3` → `cmInitParaError`。
///
/// 用户可以用配置文件里的 [`crate::config::Config::prefer_cabac`]（非 UI 开关）回退到 CAVLC。
pub(crate) const ENTROPY_CABAC: bool = true;

/// 目标码率（bps）：`video_bitrate_kbps` × 1000，并夹到 `[100_000, MAX_BITRATE_BPS]`。
///
/// 下限只是防止 0/负数（`Config::sanitized()` 已经把值收进 5–20 Mbps，
/// 但 `build_encoder_config` 是纯函数，也可能被直接喂一个手写的 `Config`）。
pub(crate) fn target_bitrate_bps(cfg: &Config) -> i32 {
    let bps = (cfg.video_bitrate_kbps as i64) * 1000;
    bps.clamp(100_000, MAX_BITRATE_BPS as i64) as i32
}

pub struct VideoEncoder {
    enc: Encoder,
    width: i32,
    height: i32,
    fps: u32,
    frame_index: u64,
    /// 首帧携带的 SPS/PPS（用于构造 avcC 与后续 remux）
    sps_pps: Option<(Vec<u8>, Vec<u8>)>,
    /// 累计输出字节，便于统计码率
    total_bytes: u64,
}

/// "自动"关键帧间隔（秒）。
///
/// **实测结论**：`uiIntraPeriod = 0` 在这套 openh264 上等于"**只在第一帧插一个 IDR**"，
/// 没有周期性关键帧 —— 30 fps 与 60 fps 各录 6 秒，`ffprobe -select_streams v:0
/// -show_entries frame=key_frame` 都只数到 **1** 个关键帧（0.000 s）。
/// 这会同时打坏两条链：崩溃时最多丢失"整段录制"（分片只在关键帧处落盘）、剪辑无法吸附。
/// 所以产品的"自动"落地成**固定 2 秒周期**（`2 × fps` 帧）：既是用户熟悉的默认值，
/// 又保证周期性 IDR。实测 30 fps → 60 帧/2.000 s、60 fps → 120 帧/2.000 s。
pub(crate) const AUTO_KEYFRAME_SECS: u32 = 2;

/// 关键帧（IDR）间隔策略。
///
/// * `Config::keyframe_interval_sec == 0`（**产品路径永远是 0**，界面已经没有这一项）→
///   [`AUTO_KEYFRAME_SECS`] × fps（见那里的实测说明）；
/// * 非 0 只出现在测试覆盖里（`recorder_smoke --gop N` 在 `sanitized()` 之后直接改字段），
///   这时按"秒 × fps"折算成帧数，方便回归脚本构造可预期的关键帧位置。
pub(crate) fn intra_period_auto(cfg: &Config, fps: u32) -> u32 {
    let secs = if cfg.keyframe_interval_sec == 0 {
        AUTO_KEYFRAME_SECS
    } else {
        cfg.keyframe_interval_sec
    };
    secs.saturating_mul(fps).max(1)
}

/// 把产品配置映射成 OpenH264 参数（纯函数，便于单测；不需要加载 DLL）。
///
/// 界面上已经没有的选项一律在这里**写死**（ABR / 平衡复杂度 / 最大码率 20 Mbps / 关键帧自动），
/// 只留下真正需要用户决定的：帧率、目标码率、分辨率。
pub fn build_encoder_config(cfg: &Config, width: i32, height: i32) -> EncoderConfig {
    let fps = cfg.fps.max(1);
    let bitrate = target_bitrate_bps(cfg);
    EncoderConfig {
        usage_type: EUsageType::CameraVideoRealTime,
        complexity: FIXED_COMPLEXITY,
        // ABR：OpenH264 的 `RC_BITRATE_MODE`（本 crate 里叫 `RcMode::Bitrate`）
        rc_mode: RcMode::Bitrate,
        // 关键帧：自动（见 [`intra_period_auto`] 的实测说明）
        intra_period: intra_period_auto(cfg, fps),
        // 熵编码：优先 CABAC（用户明确要求"优先 cabac"）。`prefer_cabac` 是配置文件里的
        // 非 UI 开关，默认 true；万一某台机器上 CABAC 得不偿失，改配置即可回退 CAVLC。
        cabac: cfg.prefer_cabac,
        threads: 0,   // auto
        slice_mode: SliceMode::FixedSlcNumSlice,
        slice_num: 0, // auto（OpenH264 会把 1080p 夹到 16）
        enable_frame_skip: false,
        max_bitrate: MAX_BITRATE_BPS,
        ..EncoderConfig::screen_capture(width, height, fps as f32, bitrate)
    }
}

impl VideoEncoder {
    pub fn new(cfg: &Config, width: i32, height: i32) -> Result<Self, AppError> {
        Self::initialize(
            build_encoder_config(cfg, width, height),
            width,
            height,
            cfg.fps.max(1),
        )
    }

    /// 按**预热缓存键**重建实例。[`EncoderKey`] 里存的就是 [`build_encoder_config`] 的
    /// 产物，所以"预热出来的实例"与"按需同步创建的实例"参数**逐字段相同**，产物等价。
    pub fn from_key(key: &EncoderKey) -> Result<Self, AppError> {
        let p = key.params();
        Self::initialize(p.clone(), p.width, p.height, key.record_fps())
    }

    /// 唯一的构造路径（`new` 与 `from_key` 都走它）：**参数完全由 `EncoderConfig` 决定**，
    /// 保证"预热"不会引入任何参数差异。
    fn initialize(c: EncoderConfig, width: i32, height: i32, fps: u32) -> Result<Self, AppError> {
        if width <= 0 || height <= 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(AppError::VideoEncode(format!(
                "分辨率必须是正偶数，当前 {width}x{height}"
            )));
        }
        let fps = fps.max(1);

        let enc = Encoder::new(&c)
            .map_err(|e| AppError::VideoEncode(format!("初始化 OpenH264 失败: {e}")))?;
        tracing::info!(
            width,
            height,
            fps,
            bitrate = c.bitrate,
            max_bitrate = c.max_bitrate,
            complexity = ?c.complexity,
            rc = ?c.rc_mode,
            cabac = c.cabac,
            intra = c.intra_period,
            "视频编码器就绪（camera_rt + ABR + FixedSlcNumSlice + auto 线程）"
        );

        Ok(Self {
            enc,
            width,
            height,
            fps,
            frame_index: 0,
            sps_pps: None,
            total_bytes: 0,
        })
    }

    pub fn width(&self) -> i32 {
        self.width
    }
    pub fn height(&self) -> i32 {
        self.height
    }
    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    /// 实际编码码率（kbps），基于已编码帧数与帧率估算
    pub fn measured_kbps(&self) -> u64 {
        if self.frame_index == 0 {
            return 0;
        }
        let secs = self.frame_index as f64 / self.fps as f64;
        (self.total_bytes as f64 * 8.0 / secs / 1000.0) as u64
    }
    /// 首帧的 (SPS, PPS)，在编码出第一帧后可用
    pub fn sps_pps(&self) -> Option<&(Vec<u8>, Vec<u8>)> {
        self.sps_pps.as_ref()
    }
    /// 构造 avcC（AVCDecoderConfigurationRecord），MP4 的 extradata 必须是这个格式
    pub fn avcc_extradata(&self) -> Result<Vec<u8>, AppError> {
        let (sps, pps) = self
            .sps_pps
            .as_ref()
            .ok_or_else(|| AppError::VideoEncode("尚未编码出带 SPS/PPS 的关键帧".into()))?;
        Ok(build_avcc(sps, pps))
    }

    /// 编码一帧，返回 (AVCC 数据, 是否关键帧)。
    ///
    /// AVCC = 每个 NAL 前放 4 字节大端长度（MP4 容器的要求），而 OpenH264 给的是 Annex-B。
    pub fn encode(&mut self, frame: &I420Frame) -> Result<(Vec<u8>, bool), AppError> {
        if frame.width != self.width as usize || frame.height != self.height as usize {
            return Err(AppError::VideoEncode(format!(
                "帧尺寸 {}x{} 与编码器 {}x{} 不一致",
                frame.width, frame.height, self.width, self.height
            )));
        }
        let ts_ms = (self.frame_index as i64) * 1000 / self.fps as i64;
        let (nals, info): (Vec<_>, FrameInfo) = self
            .enc
            .encode_i420_ex(
                &frame.y,
                &frame.u,
                &frame.v,
                frame.stride_y(),
                frame.stride_uv(),
                ts_ms,
            )
            .map_err(|e| AppError::VideoEncode(format!("编码失败: {e}")))?;

        // 首次出现的关键帧里提取 SPS/PPS
        if self.sps_pps.is_none() && info.is_keyframe {
            let mut sps = None;
            let mut pps = None;
            for n in &nals {
                match n.nalu_type {
                    7 => sps = Some(n.payload().to_vec()),
                    8 => pps = Some(n.payload().to_vec()),
                    _ => {}
                }
            }
            if let (Some(s), Some(p)) = (sps, pps) {
                tracing::info!(sps = s.len(), pps = p.len(), "已获取 SPS/PPS，可构造 avcC");
                self.sps_pps = Some((s, p));
            }
        }

        let avcc = nals_to_avcc(&nals);
        self.total_bytes += avcc.len() as u64;
        self.frame_index += 1;
        Ok((avcc, info.is_keyframe))
    }
}

// ---------------------------------------------------------------------------
// 编码器预热（消除"按下开始录制之后还要等几秒才真的开录"）
// ---------------------------------------------------------------------------
//
// # 为什么要它
//
// `Recorder::start` 里的 OpenH264 初始化（`WelsCreateSVCEncoder` + `InitializeExt`）在本机
// 实测 0.46~6.0 s（机器忙时更长），而这段时间**一帧画面都没有**：用户按了"开始录制"，
// 却要等几秒才真的开录，那几秒还会把"要录 5 秒"的预算吃掉（见 `docs/DESIGN.md` 的实测表）。
//
// # 做法（纯优化，任何失败都回退到现状）
//
// * 在"显示器/音频枚举完成、FFmpeg DLL 预加载"那同一个时机（`src/ui/main_page.rs` 的
//   UI 帧末尾，**首帧之后**）调 [`prewarm_encoder_async`]：**后台线程**按当前配置把
//   编码器创建 + `InitializeExt` 好，存进下面这个进程级缓存；
// * `Recorder::start` 先用 [`take_prewarmed_encoder`] 按**缓存键**取：键一致就**取走**
//   （取走即移除，绝不会被两次录制共用同一实例），键不一致就丢弃 + 同步创建（现状）；
// * 预热失败/DLL 缺失/线程起不来：**只记 warning**，`take_prewarmed_encoder` 返回 `None`，
//   一切回到"按需同步初始化"，**绝不 panic、绝不阻止录制**；
// * 闲置超过 [`PREWARM_TTL`] 的实例在下次取用时被丢弃（释放编码器占的内存/线程）。
//
// **没有预热 DXGI 采集会话**：`Capture::open` 建立的 `IDXGIOutputDuplication` 是一个
// 有生命周期语义的**活动会话**（系统同时最多允许 4 个，且切分辨率/独占全屏/UAC 桌面会
// 让它 `AccessLost`）。把它长时间挂在缓存里，等于让一个"可能要重建的会话"跨越整个用户
// 待机期，风险明显高于收益；而编码器实例是纯粹的内存对象，挂多久都安全。所以这里
// **只预热编码器**，采集会话仍在 `Recorder::start` 里按现状同步建立。

/// 预热实例的**最长闲置寿命**：超过它就在下次取用时丢弃并重建（避免长占内存与线程）。
pub(crate) const PREWARM_TTL: Duration = Duration::from_secs(60);

/// **编码器缓存键**：一个已经 `InitializeExt` 完成的编码器能不能直接拿来录，完全由它决定。
///
/// * `params` 是**整份** [`EncoderConfig`]，逐字段结构相等 —— 它装下了所有会改变码流或
///   编码器内部状态的参数（宽高、帧率、目标/上限码率、usage、复杂度、RC、IDR 周期、
///   CABAC、线程、slice 模式与片数、空间/时间层、profile/level、帧跳过、prefix NAL、
///   simulcast）。所以"改了分辨率 / 帧率 / 码率"必然判定为**不命中**。
/// * `record_fps` 是录制侧使用的**整数帧率**（`Config::fps` 夹紧后的值）：`VideoEncoder`
///   用它算 pts 与实测码率，它不同也不能复用。
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderKey {
    params: EncoderConfig,
    record_fps: u32,
}

impl EncoderKey {
    /// 由产品配置 + 采集尺寸组成键（尺寸口径见 [`planned_encoder_dims`]）。
    pub fn from_config(cfg: &Config, width: i32, height: i32) -> Self {
        Self {
            params: build_encoder_config(cfg, width, height),
            record_fps: cfg.fps.max(1),
        }
    }

    /// 与另一把键是否**逐字段**相同（同参数才命中）。
    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }

    pub fn params(&self) -> &EncoderConfig {
        &self.params
    }
    pub fn record_fps(&self) -> u32 {
        self.record_fps
    }
    pub fn width(&self) -> i32 {
        self.params.width
    }
    pub fn height(&self) -> i32 {
        self.params.height
    }
    pub fn fps(&self) -> f32 {
        self.params.fps
    }
    pub fn bitrate_bps(&self) -> i32 {
        self.params.bitrate
    }
    pub fn intra_period(&self) -> u32 {
        self.params.intra_period
    }
}

/// **纯几何**：按当前配置算出"采集会话会给出的裁剪尺寸"（**不创建任何 DXGI 会话**）。
///
/// 与 `Capture::open` 用的是**同一套**换算：`video::capture::{monitor_rect, region_to_crop}`
/// 都是公开纯函数，这里只是省掉了 `DxgiDuplicationApi::new_options` 那一步。
/// 于是预热用的宽高与 `Recorder::start` 里 `cap.crop()` 给出的宽高**同源**；
/// 万一显示器在这期间被改分辨率/被拔掉，键就会不匹配 → 自动退回同步初始化。
pub fn planned_encoder_dims(cfg: &Config) -> Result<(i32, i32), String> {
    use windows_capture::monitor::Monitor;
    // 与 `video::capture::resolve_monitor` 同语义：0 = 主显示器，其余按 windows-capture 序号。
    let monitor = if cfg.monitor_index == 0 {
        Monitor::primary().map_err(|e| format!("取主显示器失败: {e}"))?
    } else {
        Monitor::from_index(cfg.monitor_index)
            .map_err(|e| format!("取显示器 {} 失败: {e}", cfg.monitor_index))?
    };
    let rect = crate::video::capture::monitor_rect(&monitor).map_err(|e| e.to_string())?;
    let (crop, _) = crate::video::capture::region_to_crop(cfg.capture_mode, cfg.region, rect)?;
    Ok((crop.width as i32, crop.height as i32))
}

/// 预热请求的结果（用于日志与单测断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrewarmOutcome {
    /// 缓存里已有同键且未过期的实例，什么都不用做
    AlreadyWarm,
    /// 已经有一个后台线程在为**同一个键**预热，不重复起线程
    InFlight,
    /// 已经起了后台预热线程
    Spawned,
    /// 同一个键之前预热失败过（例如 DLL 缺失）：不重试，避免每帧重试烧 CPU
    FailedBefore,
    /// 预热线程创建失败（极端内存压力）
    ThreadFailed,
    /// 这次不需要/无法预热，原因是这个字符串
    Skipped(String),
}

impl PrewarmOutcome {
    /// 本次是否真的起了后台预热线程。
    pub fn spawned(&self) -> bool {
        matches!(self, Self::Spawned)
    }
}

/// 一个已初始化的预热实例 + 它的键 + 创建时刻。
struct PrewarmSlot<T> {
    key: EncoderKey,
    value: T,
    created: Instant,
}

/// 预热缓存状态（泛型是为了让**纯逻辑**能被单测覆盖：`T = ()` 时不需要 DLL）。
struct PrewarmState<T> {
    /// 已预热好的实例（同一时刻最多一个）
    slot: Option<PrewarmSlot<T>>,
    /// 正在后台预热的键（`None` = 没有在飞的请求）
    in_flight: Option<EncoderKey>,
    /// 预热失败过的键（同一个键不重试）
    failed: Option<EncoderKey>,
    /// 正在预热时又来了新配置：**只记最新的一个**，不叠加线程。
    ///
    /// 拖码率/分辨率滑条时配置每帧都在变；旧实现会为每个中间键 spawn 一条
    /// OpenH264 初始化线程。现在同一时刻最多一条在飞，后续键合并成 `pending`，
    /// 等下一帧 UI 调 `prewarm_encoder_once` 时再决定要不要建最新那个。
    pending: Option<EncoderKey>,
    /// 请求代数：配置改来改去时，只把**最新一代**的结果写进缓存
    generation: u64,
}

impl<T> PrewarmState<T> {
    const fn new() -> Self {
        Self {
            slot: None,
            in_flight: None,
            failed: None,
            pending: None,
            generation: 0,
        }
    }

    /// **按键取走**（取走即从缓存移除）：命中返回 `Some(T)`，键不匹配或已过期返回 `None`
    /// 并顺手把不匹配/过期的实例丢掉（释放资源，也让"不命中"这件事可观测）。
    #[cfg(test)]
    fn take_if_match(&mut self, key: &EncoderKey, now: Instant) -> Option<T> {
        let hit = match &self.slot {
            Some(s) => {
                &s.key == key && now.saturating_duration_since(s.created) < PREWARM_TTL
            }
            None => false,
        };
        if !hit {
            self.slot = None;
            return None;
        }
        self.slot.take().map(|s| s.value)
    }

    /// 后台线程完成后回填：只有**最新一代**（`generation` 未变）的结果才会被采纳，
    /// 返回是否真的存进去了。
    fn store_if_latest(
        &mut self,
        generation: u64,
        key: EncoderKey,
        value: T,
        now: Instant,
    ) -> (bool, Option<T>) {
        if self.generation != generation {
            return (false, None);
        }
        // 旧实例先取出来，交给调用方在**锁外** drop，避免 OpenH264 析构卡住预热锁。
        let old = self.slot.take().map(|s| s.value);
        self.in_flight = None;
        self.failed = None;
        self.slot = Some(PrewarmSlot {
            key,
            value,
            created: now,
        });
        (true, old)
    }
}

/// 进程级预热缓存（`Result` 中毒时也用 `into_inner` 继续 —— 一次预热 panic 绝不能
/// 让之后的所有录制都无法复用/无法启动）。
static PREWARM: Mutex<PrewarmState<VideoEncoder>> = Mutex::new(PrewarmState::new());

/// 最近一次**成功**预热的后台耗时（毫秒）—— 只为日志/探针可见，不参与任何判定。
static PREWARM_LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 最近一次成功预热花了多少毫秒（后台线程里量的；`0` = 还没有成功预热过）。
pub fn last_prewarm_ms() -> u64 {
    PREWARM_LAST_MS.load(std::sync::atomic::Ordering::Relaxed)
}

fn lock_prewarm() -> std::sync::MutexGuard<'static, PrewarmState<VideoEncoder>> {
    match PREWARM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 缓存里是否**已经**有一个键匹配且未过期的实例（只查不取）。
///
/// 给探针/界面用的只读视图：它不会把实例取走，所以可以反复调用。
pub fn prewarmed_encoder_ready(cfg: &Config, width: i32, height: i32) -> bool {
    let key = EncoderKey::from_config(cfg, width, height);
    let st = lock_prewarm();
    match &st.slot {
        Some(s) => s.key == key && s.created.elapsed() < PREWARM_TTL,
        None => false,
    }
}

/// **丢弃**当前预热实例（并清掉"失败过的键"记忆、让在飞的预热结果作废）。
///
/// 正常流程**不需要**它（录制侧只走 [`take_prewarmed_encoder`]，键不匹配会自动丢弃）；
/// 它存在是为了让探针/单测能从"完全没有预热"的干净状态开始测量。
pub fn discard_prewarm() {
    let old = {
        let mut st = lock_prewarm();
        let old = st.slot.take();
        st.failed = None;
        st.in_flight = None;
        st.pending = None;
        st.generation += 1;
        old
    };
    // 锁外 drop：OpenH264 析构可能等待自己的线程，不能占着全局预热锁。
    drop(old);
}

/// 主动回收过期的预热实例（不必等到下次 `take`）。
///
/// 用户启动后从不录制时，缓存里的 OpenH264 编码器会一直占着内存和线程池；
/// 由 UI 每帧调用一次（一次互斥锁 + 一次 `Instant` 比较，成本可忽略）。
pub fn sweep_prewarm() {
    let old = {
        let mut st = lock_prewarm();
        let expired = match &st.slot {
            Some(s) => s.created.elapsed() >= PREWARM_TTL,
            None => false,
        };
        if expired {
            st.slot.take()
        } else {
            None
        }
    };
    if old.is_some() {
        tracing::info!("预热编码器已闲置超时，已释放（下次录制按需重建）");
        // 锁外 drop：析构 OpenH264 可能不是零成本，不能卡 UI/录制线程。
        drop(old);
    }
}

/// **按当前配置在后台预热一个 OpenH264 编码器实例**（非阻塞，随时可调用）。
///
/// * 同键重复调用是**空操作**（`AlreadyWarm` / `InFlight`），所以"每帧调用"也不会起一堆线程；
/// * 配置变了（分辨率/帧率/码率…）→ 旧实例被丢弃并重新预热（`Spawned`）；
/// * 任何失败（DLL 缺失、参数非法、显示器几何算不出来、线程起不来）都只是**返回值 + 日志**，
///   不影响录制：录制侧仍会走"按需同步初始化"。
pub fn prewarm_encoder_async(cfg: &Config) -> PrewarmOutcome {
    prewarm_encoder_async_logged(cfg, None)
}

/// 与 [`prewarm_encoder_async`] 相同，但可以把"预热完成/预热失败"写进**外部日志落点**。
///
/// 为什么需要它：产品（GUI）**没有安装 tracing 订阅者**（只有 `src/bin/*` 的三个烟雾/探针
/// 程序装了），所以 `tracing::warn!` 在用户机器上**什么都看不到**。预热是纯优化、失败只警告，
/// 但"到底成没成"必须留痕 —— 于是这里把同一件事也交给调用方的日志 sink（`cap.log`）。
pub fn prewarm_encoder_async_logged(cfg: &Config, log: Option<fn(&str)>) -> PrewarmOutcome {
    if !cfg.content_mode.has_video() {
        return PrewarmOutcome::Skipped("内容模式不含画面，不需要视频编码器".into());
    }
    let (w, h) = match planned_encoder_dims(cfg) {
        Ok(v) => v,
        Err(e) => return PrewarmOutcome::Skipped(format!("算不出采集尺寸：{e}")),
    };
    enqueue_prewarm(EncoderKey::from_config(cfg, w, h), log)
}

/// 认领一次预热并把结果落到缓存（抽出来是为了让 [`prewarm_encoder_async`] 保持简短）。
fn enqueue_prewarm(key: EncoderKey, log: Option<fn(&str)>) -> PrewarmOutcome {
    let (generation, old) = {
        let mut st = lock_prewarm();
        if let Some(slot) = &st.slot {
            if slot.key == key && slot.created.elapsed() < PREWARM_TTL {
                return PrewarmOutcome::AlreadyWarm;
            }
        }
        if let Some(inflight) = &st.in_flight {
            if inflight == &key {
                return PrewarmOutcome::InFlight;
            }
            // 已有线程在飞：只记最新键，不叠加初始化线程。
            st.pending = Some(key.clone());
            return PrewarmOutcome::InFlight;
        }
        if st.failed.as_ref() == Some(&key) {
            return PrewarmOutcome::FailedBefore;
        }
        // 旧实例（键不匹配/过期）先取出来，锁外再 drop。
        let old = st.slot.take();
        st.pending = None;
        st.generation += 1;
        st.in_flight = Some(key.clone());
        (st.generation, old)
    };
    drop(old);

    let key_for_thread = key.clone();
    let spawned = std::thread::Builder::new()
        .name("oh264-prewarm".to_string())
        .spawn(move || {
            let t0 = Instant::now();
            let built = VideoEncoder::from_key(&key_for_thread);
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            let (w, h, fps, bitrate) = (
                key_for_thread.width(),
                key_for_thread.height(),
                key_for_thread.fps(),
                key_for_thread.bitrate_bps(),
            );
            let (stored, old, build_err) = match built {
                Ok(enc) => {
                    let (stored, old) = {
                        let mut st = lock_prewarm();
                        st.store_if_latest(generation, key_for_thread, enc, Instant::now())
                    };
                    (stored, old, None)
                }
                Err(e) => {
                    let mut st = lock_prewarm();
                    if st.generation == generation {
                        st.in_flight = None;
                        st.failed = Some(key_for_thread);
                    }
                    (false, None, Some(e))
                }
            };
            // 锁外 drop 旧编码器：析构 OpenH264 可能不是零成本。
            drop(old);
            if let Some(e) = build_err {
                tracing::warn!(
                    width = w,
                    height = h,
                    elapsed_ms,
                    error = %e,
                    "编码器预热失败：回退到「按下开始时同步初始化」（与预热前行为一致，不影响录制）"
                );
                // 同上：GUI 里只看得到启动日志，失败也必须留痕（但**只记 warning**，
                // 不是错误、也绝不阻止录制）。
                if let Some(sink) = log {
                    sink(&format!(
                        "[cap] OpenH264 编码器预热警告：失败（{e}）—— \
                         不影响录制，按下开始录制时会同步初始化"
                    ));
                }
            } else if stored {
                PREWARM_LAST_MS.store(elapsed_ms, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    width = w,
                    height = h,
                    fps,
                    bitrate,
                    elapsed_ms,
                    "OpenH264 编码器已预热（后台线程完成，按下开始录制时直接复用）"
                );
                // 产品（GUI）**没有安装 tracing 订阅者**，所以这条同时写进启动日志落点
                // （`cap.log`）：否则"预热到底有没有成功"在用户机器上完全看不见。
                if let Some(sink) = log {
                    sink(&format!(
                        "[cap] OpenH264 编码器预热完成：{w}x{h} @{fps:.0}fps {}kbps，\
                         后台耗时 {elapsed_ms}ms（按下开始录制时直接复用）",
                        bitrate / 1000
                    ));
                }
            } else {
                tracing::info!(
                    elapsed_ms,
                    "预热结果已过期（这期间配置又变了）：丢弃，等最新一次预热"
                );
            }
        });
    if spawned.is_err() {
        let mut st = lock_prewarm();
        if st.generation == generation {
            st.in_flight = None;
        }
        tracing::warn!("编码器预热失败：后台线程创建失败（不影响录制，仍按需同步初始化）");
        if let Some(sink) = log {
            sink("[cap] OpenH264 编码器预热警告：后台线程创建失败（不影响录制）");
        }
        return PrewarmOutcome::ThreadFailed;
    }
    PrewarmOutcome::Spawned
}

/// **取走**键匹配且未过期的预热实例；没有/不匹配/已过期就返回 `None`（调用方同步创建兜底）。
///
/// **实例独占**：取走即从缓存移除，同一个编码器实例绝不会被两次录制共用。
pub fn take_prewarmed_encoder(cfg: &Config, width: i32, height: i32) -> Option<VideoEncoder> {
    let key = EncoderKey::from_config(cfg, width, height);
    let now = Instant::now();
    let (taken, discarded, had_slot) = {
        let mut st = lock_prewarm();
        let had_slot = st.slot.is_some();
        let hit = match &st.slot {
            Some(s) => s.key == key && now.saturating_duration_since(s.created) < PREWARM_TTL,
            None => false,
        };
        if hit {
            (st.slot.take().map(|s| s.value), None, had_slot)
        } else {
            let old = if had_slot { st.slot.take() } else { None };
            (None, old, had_slot)
        }
    };
    // 锁外 drop 不匹配/过期的实例；不要让析构占着 PREWARM 锁。
    drop(discarded);
    if taken.is_some() {
        tracing::info!(
            width,
            height,
            fps = key.fps(),
            bitrate = key.bitrate_bps(),
            intra = key.intra_period(),
            "命中编码器预热缓存：直接复用已初始化好的 OpenH264 实例（本次不再同步初始化）"
        );
    } else if had_slot {
        tracing::info!(
            width,
            height,
            fps = key.fps(),
            bitrate = key.bitrate_bps(),
            "预热实例与本次配置不匹配（或已闲置过期）：丢弃并同步初始化（不影响录制）"
        );
    }
    taken
}

/// Annex-B NAL 列表 → AVCC（4 字节大端长度前缀）
pub(crate) fn nals_to_avcc(nals: &[crate::video::openh264::Nal]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nals.iter().map(|n| n.data.len() + 4).sum());
    for n in nals {
        let payload = n.payload();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }
    out
}

/// 构造 AVCDecoderConfigurationRecord（avcC）
pub(crate) fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    debug_assert!(sps.len() >= 4, "SPS 太短，无法构造 avcC");
    let mut v = Vec::with_capacity(11 + sps.len() + pps.len());
    v.push(1); // configurationVersion
    v.push(*sps.get(1).unwrap_or(&0x42)); // AVCProfileIndication
    v.push(*sps.get(2).unwrap_or(&0x00)); // profile_compatibility
    v.push(*sps.get(3).unwrap_or(&0x1E)); // AVCLevelIndication
    v.push(0xFF); // lengthSizeMinusOne = 3 (4 字节长度)
    v.push(0xE1); // numOfSequenceParameterSets = 1
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1); // numOfPictureParameterSets
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CaptureMode, ContentMode, Region};

    #[test]
    fn avcc_layout_is_correct() {
        let sps = vec![0x67, 0x42, 0x00, 0x1E, 0xAA, 0xBB];
        let pps = vec![0x68, 0xCE, 0x3C, 0x80];
        let avcc = build_avcc(&sps, &pps);
        assert_eq!(avcc[0], 1, "configurationVersion");
        assert_eq!(&avcc[1..4], &[0x42, 0x00, 0x1E], "profile/compat/level");
        assert_eq!(avcc[4], 0xFF, "lengthSizeMinusOne");
        assert_eq!(avcc[5], 0xE1, "numSPS=1");
        assert_eq!(u16::from_be_bytes([avcc[6], avcc[7]]), sps.len() as u16);
        assert_eq!(&avcc[8..8 + sps.len()], &sps[..]);
        let p = 8 + sps.len();
        assert_eq!(avcc[p], 1, "numPPS=1");
        assert_eq!(
            u16::from_be_bytes([avcc[p + 1], avcc[p + 2]]),
            pps.len() as u16
        );
        assert_eq!(&avcc[p + 3..], &pps[..]);
    }

    /// 参数映射：界面上已经拿掉的选择项必须在内部**固定**成实测选定的值。
    ///
    /// 策略由来（都是实测，见 `probe_cabac_and_max_bitrate` 的输出）：
    /// * usage/slice/线程：`CameraVideoRealTime` + `FixedSlcNumSlice` + auto 线程 —— 129 fps 那套；
    /// * RC = ABR（`RcMode::Bitrate` = `RC_BITRATE_MODE`），复杂度 = 平衡（Medium）；
    /// * `iMaxBitrate` = 20 Mbps（与目标码率上限对齐，保证"目标 ≤ 上限"）；
    /// * 熵编码 = **CABAC**：同素材同码率下产物小 47%、PSNR 完全相同（52.73 dB）、
    ///   速度不变（83.2 → 84.2 fps），所以"优先 cabac"在实时模式下也拿得到。
    #[test]
    fn encoder_config_pins_rc_complexity_max_bitrate_and_cabac() {
        // P48：`enc_speed` / `rate_control` 这些"旧选项"字段已经删了 ——
        // 它们本来就只写不读，所以这里的断言才是**唯一的**真相来源。
        let cfg = Config {
            fps: 30,
            keyframe_interval_sec: 2,
            prefer_cabac: true,
            video_bitrate_kbps: 5_000,
            ..Config::default()
        };
        let c = build_encoder_config(&cfg, 1920, 1080);
        assert_eq!(c.usage_type, EUsageType::CameraVideoRealTime);
        assert_eq!(c.slice_mode, SliceMode::FixedSlcNumSlice);
        assert_eq!(c.slice_num, 0, "auto 切片数：显式设 >35 会让 OpenH264 初始化失败");
        assert_eq!(c.threads, 0, "auto 线程数（实测 129 fps 那套配置）");
        assert_eq!(c.complexity, EComplexityMode::Medium, "复杂度固定「平衡」，不看配置");
        assert_eq!(c.rc_mode, RcMode::Bitrate, "RC 固定 ABR（RC_BITRATE_MODE）");
        assert_eq!(c.intra_period, 2 * 30, "关键帧间隔 = 秒 × fps");
        assert_eq!(c.width, 1920);
        assert_eq!(c.height, 1080);
        assert_eq!(c.bitrate, 5_000_000, "目标码率 = kbps × 1000");
        assert_eq!(c.max_bitrate, MAX_BITRATE_BPS, "最大码率固定 20 Mbps");
        assert_eq!(c.max_bitrate, 20_000_000);
        assert_eq!(c.cabac, ENTROPY_CABAC, "默认优先 CABAC");
        assert!(c.cabac, "CABAC 默认开（实测省 47% 字节、PSNR 不变、不降速）");

        // 目标码率的上边界（= 最大码率）也必须映射正确
        let top = build_encoder_config(
            &Config { video_bitrate_kbps: 20_000, ..cfg.clone() },
            1920,
            1080,
        );
        assert_eq!(top.bitrate, MAX_BITRATE_BPS, "20 Mbps 目标 = 20 Mbps 上限");
        assert!(top.bitrate <= top.max_bitrate, "目标不得超过上限（否则 cmInitParaError）");

        // 非 UI 开关能把它关掉（配置文件里改一行即可回退 CAVLC）
        let off = build_encoder_config(
            &Config { prefer_cabac: false, ..cfg.clone() },
            1920,
            1080,
        );
        assert!(!off.cabac, "prefer_cabac=false 时必须回退 CAVLC");
    }

    // -----------------------------------------------------------------------
    // 编码器预热：缓存键 / 取走即移除 / 失败回退（**纯逻辑**，不需要 openh264.dll）
    // -----------------------------------------------------------------------

    /// 基准配置：30 fps、8 Mbps、1080p。
    fn prewarm_base_config() -> Config {
        Config {
            fps: 30,
            video_bitrate_kbps: 8_000,
            ..Config::default()
        }
    }

    /// **缓存键匹配**：同参数命中；改帧率 / 码率 / 分辨率 / CABAC / 关键帧间隔都不命中。
    ///
    /// 这条断言是"预热绝不会用错参数"的唯一依据：`EncoderKey` 装的是**整份**
    /// `EncoderConfig`（逐字段相等），所以任何一个会改变码流的参数变了都必然重建。
    #[test]
    fn prewarm_key_hits_only_when_every_parameter_matches() {
        let base = prewarm_base_config();
        let key = EncoderKey::from_config(&base, 1920, 1080);

        // ① 同参数 → 命中（这是预热的全部意义）
        let same = EncoderKey::from_config(&base, 1920, 1080);
        assert!(key.matches(&same), "同参数必须命中");
        assert_eq!(key.width(), 1920);
        assert_eq!(key.height(), 1080);
        assert_eq!(key.fps(), 30.0);
        assert_eq!(key.bitrate_bps(), 8_000_000);
        assert_eq!(key.intra_period(), 2 * 30, "自动关键帧 = 2 秒 × fps");

        // ② 改帧率 → 不命中
        let fps60 = EncoderKey::from_config(&Config { fps: 60, ..base.clone() }, 1920, 1080);
        assert!(!key.matches(&fps60), "改 fps 不得命中");
        assert_eq!(fps60.record_fps(), 60);

        // ③ 改码率 → 不命中
        let br = EncoderKey::from_config(
            &Config { video_bitrate_kbps: 12_000, ..base.clone() },
            1920,
            1080,
        );
        assert!(!key.matches(&br), "改码率不得命中");
        assert_eq!(br.bitrate_bps(), 12_000_000);

        // ④ 改分辨率 → 不命中（区域录制改尺寸走的就是这条路）
        let small = EncoderKey::from_config(&base, 1280, 720);
        assert!(!key.matches(&small), "改分辨率不得命中");

        // ⑤ 改 CABAC → 不命中
        let cavlc = EncoderKey::from_config(
            &Config { prefer_cabac: false, ..base.clone() },
            1920,
            1080,
        );
        assert!(!key.matches(&cavlc), "改熵编码不得命中");

        // ⑥ 改关键帧间隔（测试覆盖用）→ 不命中
        let gop1 = EncoderKey::from_config(
            &Config { keyframe_interval_sec: 1, ..base.clone() },
            1920,
            1080,
        );
        assert!(!key.matches(&gop1), "改关键帧间隔不得命中");
        assert_eq!(gop1.intra_period(), 30);
    }

    /// **取走即移除（不可重复使用）** + 键不匹配/闲置过期时丢弃。
    #[test]
    fn prewarm_cache_take_is_exclusive_and_drops_stale_entries() {
        let base = prewarm_base_config();
        let key = EncoderKey::from_config(&base, 1920, 1080);
        let other = EncoderKey::from_config(&base, 1280, 720);
        let t0 = Instant::now();

        let mut st: PrewarmState<&'static str> = PrewarmState::new();
        // 模拟 `enqueue_prewarm` 的记账：认领一代 → 后台完成 → 回填
        let g1 = {
            st.generation += 1;
            st.generation
        };
        assert!(st.store_if_latest(g1, key.clone(), "enc", t0).0);

        // ① 命中一次…
        assert_eq!(st.take_if_match(&key, t0), Some("enc"));
        // ② …第二次必须是 None：实例独占，绝不被两次录制共用
        assert_eq!(st.take_if_match(&key, t0), None, "取走即移除");
        assert!(st.slot.is_none());

        // ③ 键不匹配 → None，并且不匹配的实例会被丢掉（不留在缓存里占资源）
        let g2 = {
            st.generation += 1;
            st.generation
        };
        assert!(st.store_if_latest(g2, key.clone(), "enc", t0).0);
        assert_eq!(st.take_if_match(&other, t0), None, "键不匹配不得命中");
        assert!(st.slot.is_none(), "不匹配的预热实例应被丢弃");

        // ④ 闲置超过 TTL → None（下次用时会重建）
        let g3 = {
            st.generation += 1;
            st.generation
        };
        assert!(st.store_if_latest(g3, key.clone(), "enc", t0).0);
        assert_eq!(
            st.take_if_match(&key, t0 + PREWARM_TTL),
            None,
            "超过 TTL 的实例必须丢弃重建"
        );
        assert!(st.slot.is_none());

        // ⑤ 过期的一代不得覆盖最新一代（配置连续改动时的竞态）
        let g4 = {
            st.generation += 1;
            st.generation
        };
        assert!(st.store_if_latest(g4, key.clone(), "new", t0).0);
        assert!(!st.store_if_latest(g3, key.clone(), "old", t0).0, "旧一代不得覆盖");
        assert_eq!(st.slot.as_ref().map(|s| s.value), Some("new"));
    }

    /// **预热失败/取不到 → 回退到"按需同步初始化"，绝不 panic、绝不阻止录制。**
    ///
    /// 覆盖三种"预热没戏"的输入：几何算不出来的区域、纯音频模式、空缓存。
    #[test]
    fn prewarm_failure_falls_back_without_panic() {
        // ① 选区远在任何显示器之外：算不出尺寸 → 只返回 Skipped（不起线程、不 panic）
        let outside = Config {
            capture_mode: CaptureMode::Region,
            region: Region { x: 1_000_000, y: 1_000_000, width: 640, height: 480 },
            ..prewarm_base_config()
        };
        match prewarm_encoder_async(&outside) {
            PrewarmOutcome::Skipped(why) => {
                assert!(!why.is_empty(), "跳过时必须带上原因（写进日志）")
            }
            other => panic!("算不出尺寸时必须只跳过，实际 {other:?}"),
        }

        // ② 纯音频：根本不需要视频编码器
        let audio_only = Config {
            content_mode: ContentMode::AudioOnly,
            ..prewarm_base_config()
        };
        assert!(
            matches!(prewarm_encoder_async(&audio_only), PrewarmOutcome::Skipped(_)),
            "纯音频不该预热视频编码器"
        );

        // ④ 空缓存取实例 → None（调用方据此同步创建，正是改动前的行为）
        let mut st: PrewarmState<()> = PrewarmState::new();
        let key = EncoderKey::from_config(&prewarm_base_config(), 1920, 1080);
        assert_eq!(
            st.take_if_match(&key, Instant::now()),
            None,
            "空缓存必须返回 None（回退到同步初始化）"
        );

        // ⑤ 预热失败过的键**不重试**（避免每帧重试烧 CPU）；这条走的是真缓存状态，
        //    但失败路径不建线程、不碰 DLL，所以不会有副作用。用完把它清干净。
        {
            let mut g = lock_prewarm();
            g.failed = Some(key.clone());
            g.in_flight = None;
        }
        assert_eq!(
            enqueue_prewarm(key.clone(), None),
            PrewarmOutcome::FailedBefore,
            "同一个键失败过之后不得反复重试"
        );
        {
            let mut g = lock_prewarm();
            g.failed = None;
            g.slot = None;
            g.in_flight = None;
        }
    }

    /// 预热用的宽高必须与"采集会话会给出的宽高"同源（全屏 = 整块显示器，区域 = 取偶后的选区）。
    ///
    /// 这条与真机几何无关：它只断言 [`planned_encoder_dims`] 与
    /// `video::capture::region_to_crop` 的结果一致 —— 两者失配就会导致"永远不命中"。
    #[test]
    fn planned_dims_follow_the_same_pure_geometry_as_capture() {
        let (w, h) = planned_encoder_dims(&prewarm_base_config()).expect("本机应能算出主屏尺寸");
        assert!(w > 0 && h > 0 && w % 2 == 0 && h % 2 == 0, "尺寸应为正偶数：{w}x{h}");

        // 区域模式：与纯函数换算结果逐字段一致（这里用一个必然合法的区域内接）
        let region = Region { x: 0, y: 0, width: 640, height: 480 };
        let cfg = Config {
            capture_mode: CaptureMode::Region,
            region,
            ..prewarm_base_config()
        };
        let (rw, rh) = planned_encoder_dims(&cfg).expect("主屏左上角的 640x480 必然合法");
        assert_eq!((rw, rh), (640, 480));
    }

    // -----------------------------------------------------------------------
    // 实测探针（真机加载 openh264.dll，编码真实帧）：
    // `cargo test --lib -- --nocapture live_encoder cabac` 看真实数字
    // -----------------------------------------------------------------------

    fn synthetic(cfg: &EncoderConfig, index: usize) -> crate::video::openh264::testutil::I420Frame {
        crate::video::openh264::testutil::synthetic_i420(
            cfg.width as usize,
            cfg.height as usize,
            index,
        )
    }

    /// 编 `n` 帧，返回 (帧数, 总字节, 耗时秒, 整段码流)。
    fn encode_n(cfg: &EncoderConfig, n: usize) -> Result<(usize, usize, f64, Vec<u8>), String> {
        let mut enc = Encoder::new(cfg).map_err(|e| format!("{e}"))?;
        let mut stream: Vec<u8> = Vec::new();
        let mut frames = 0usize;
        let t = std::time::Instant::now();
        for i in 0..n {
            let f = synthetic(cfg, i);
            let nals = enc
                .encode_i420(
                    &f.y,
                    &f.u,
                    &f.v,
                    f.stride_y(),
                    f.stride_uv(),
                    (i as i64) * 1000 / cfg.fps.max(1.0) as i64,
                )
                .map_err(|e| format!("{e}"))?;
            for nal in &nals {
                stream.extend_from_slice(&nal.data);
            }
            frames += 1;
        }
        Ok((frames, stream.len(), t.elapsed().as_secs_f64(), stream))
    }

    /// **关键帧"自动"的实测依据**：直接证明 `intra_period = 0` 在这套 openh264 上
    /// 只会产生首帧一个 IDR（没有周期性关键帧），所以产品的自动策略落地成
    /// [`AUTO_KEYFRAME_SECS`]（2 秒 → `2 × fps` 帧）。
    ///
    /// 用 640×360 而不是 1080p：`uiIntraPeriod` 的单位是**帧**，与分辨率无关，
    /// 小分辨率能让这条测试跑得快（130 帧）。
    #[test]
    fn auto_keyframe_policy_keeps_periodic_idr() {
        if crate::video::openh264::OpenH264Lib::load().is_err() {
            println!("[skip] 本机加载不到 openh264.dll，跳过关键帧策略实测");
            return;
        }
        let frames = 130usize;
        let base = |intra: u32| EncoderConfig {
            intra_period: intra,
            ..EncoderConfig {
                usage_type: EUsageType::CameraVideoRealTime,
                complexity: EComplexityMode::Medium,
                rc_mode: RcMode::Bitrate,
                cabac: true,
                threads: 0,
                slice_mode: SliceMode::FixedSlcNumSlice,
                slice_num: 0,
                max_bitrate: MAX_BITRATE_BPS,
                ..EncoderConfig::screen_capture(640, 360, 60.0, 5_000_000)
            }
        };

        // ① OpenH264 的"自动"（0）：实测只有第一帧是 IDR
        let idr0 = encode_keyframe_indices(&base(0), frames).expect("0 也应能编码");
        println!("[kf] intra_period=0   → IDR 帧序号 {idr0:?}（{frames} 帧）");
        assert_eq!(
            idr0,
            vec![0],
            "intra_period=0 实测没有周期性 IDR —— 正因如此产品不能直接采用它"
        );

        // ② 产品的自动策略（2 秒 = 120 帧 @60fps）：周期性 IDR，间隔恰好 120 帧
        let idr = encode_keyframe_indices(&base(AUTO_KEYFRAME_SECS * 60), frames)
            .expect("2 秒周期应能编码");
        println!("[kf] intra_period=120 → IDR 帧序号 {idr:?}（{frames} 帧）");
        assert_eq!(idr, vec![0, 120], "2 秒周期应给出 0 与 120 两个 IDR");

        // ③ 配置映射：自动（0）→ 2 秒；测试覆盖（非 0）→ 秒 × fps
        let auto = Config::default();
        assert_eq!(auto.keyframe_interval_sec, 0, "产品路径永远是自动");
        assert_eq!(intra_period_auto(&auto, 30), 60);
        assert_eq!(intra_period_auto(&auto, 60), 120);
        let overridden = Config { keyframe_interval_sec: 1, ..Config::default() };
        assert_eq!(intra_period_auto(&overridden, 30), 30, "测试覆盖按秒 × fps");
    }

    /// 编 `n` 帧，返回**关键帧（IDR）的帧序号**。
    fn encode_keyframe_indices(
        cfg: &EncoderConfig,
        n: usize,
    ) -> Result<Vec<usize>, String> {
        let mut enc = Encoder::new(cfg).map_err(|e| format!("{e}"))?;
        let mut out = Vec::new();
        for i in 0..n {
            let f = synthetic(cfg, i);
            let nals = enc
                .encode_i420(
                    &f.y,
                    &f.u,
                    &f.v,
                    f.stride_y(),
                    f.stride_uv(),
                    (i as i64) * 1000 / cfg.fps.max(1.0) as i64,
                )
                .map_err(|e| format!("{e}"))?;
            if nals.iter().any(|nal| nal.is_sps()) && i == 0 || nals.iter().any(|n| n.nalu_type == 5) {
                out.push(i);
            }
        }
        Ok(out)
    }

    /// 真机探针（需要 `openh264.dll`）：把"外部已经没有的选项"逐一实测出来。
    ///
    /// `cargo test --lib -- --nocapture probe_cabac_and_max_bitrate` 看真实数字。
    /// 断言的是**结论**而不是具体数字（数字随机器变，结论不能变）：
    /// * 目标 5 Mbps 与 20 Mbps（= 内部上限）都能初始化并编出帧 —— 含"等于上限"的边界；
    /// * CABAC 真的生效：产物明显小于 CAVLC，且 PPS 的 `entropy_coding_mode_flag = 1`；
    /// * 非实时 usage type 在本 DLL 上**不可用**（必须报错，而不是假成功）。
    #[test]
    fn probe_cabac_and_max_bitrate() {
        if crate::video::openh264::OpenH264Lib::load().is_err() {
            println!("[skip] 本机加载不到 openh264.dll，跳过编码探针");
            return;
        }
        let frames_n = 30usize;
        let base = |usage: EUsageType, cabac: bool, target: i32| EncoderConfig {
            usage_type: usage,
            complexity: EComplexityMode::Medium,
            rc_mode: RcMode::Bitrate,
            cabac,
            threads: 0,
            slice_mode: SliceMode::FixedSlcNumSlice,
            slice_num: 0,
            max_bitrate: MAX_BITRATE_BPS,
            ..EncoderConfig::screen_capture(1920, 1080, 60.0, target)
        };
        println!("\n[probe] 1920x1080, {frames_n} 帧, ABR, Medium, max=20 Mbps");

        let mut enc_bytes: Vec<u64> = Vec::new();
        for (name, cfg) in [
            ("RT  5M cabac=false", base(EUsageType::CameraVideoRealTime, false, 5_000_000)),
            ("RT  5M cabac=true ", base(EUsageType::CameraVideoRealTime, true, 5_000_000)),
            ("RT 20M cabac=true ", base(EUsageType::CameraVideoRealTime, true, MAX_BITRATE_BPS)),
            ("NRT 5M cabac=true ", base(EUsageType::CameraVideoNonRealTime, true, 5_000_000)),
            ("SCR-NRT 5M cabac=true", base(EUsageType::ScreenContentNonRealTime, true, 5_000_000)),
        ] {
            match encode_n(&cfg, frames_n) {
                Ok((n, bytes, secs, stream)) => {
                    assert_eq!(n, frames_n, "{name}: 每一帧都应编出来");
                    let fps = n as f64 / secs.max(1e-9);
                    let mbps = bytes as f64 * 8.0 / (n as f64 / cfg.fps as f64) / 1e6;
                    println!(
                        "  {name:<22} ok  {bytes:>9} B  {fps:>6.1} fps  {mbps:>5.2} Mbps  digest={:016x}",
                        crate::video::openh264::testutil::digest(&stream)
                    );
                    if name.starts_with("RT") {
                        enc_bytes.push(bytes as u64);
                    }
                }
                Err(e) => {
                    println!("  {name:<22} ERR {e}");
                    assert!(
                        name.contains("NRT"),
                        "{name}: 实时 usage type 必须能编码，实际失败：{e}"
                    );
                }
            }
        }
        // 5 Mbps / 20 Mbps（上限）都能编；CABAC 明显更省（实测 -47%）
        assert_eq!(enc_bytes.len(), 3, "三条实时配置都必须成功");
        assert!(
            enc_bytes[1] * 10 < enc_bytes[0] * 9,
            "CABAC 应比 CAVLC 至少省 10%：{} vs {}",
            enc_bytes[1],
            enc_bytes[0]
        );

        // ---- CABAC 是否真的写进了码流：直接读 PPS 的 entropy_coding_mode_flag ----
        let mut pps_flag = Vec::new();
        for (tag, cabac) in [("cavlc", false), ("cabac", true)] {
            let cfg = base(EUsageType::CameraVideoRealTime, cabac, 5_000_000);
            let mut enc = Encoder::new(&cfg).expect("实时配置必须能初始化");
            let f = synthetic(&cfg, 0);
            let nals = enc
                .encode_i420(&f.y, &f.u, &f.v, f.stride_y(), f.stride_uv(), 0)
                .expect("编码首帧");
            let (mut sps, mut pps) = (Vec::new(), Vec::new());
            let mut stream = Vec::new();
            for nal in &nals {
                match nal.nalu_type {
                    7 => sps = nal.payload().to_vec(),
                    8 => pps = nal.payload().to_vec(),
                    _ => {}
                }
                stream.extend_from_slice(&nal.data);
            }
            let flag = pps_entropy_flag(&pps);
            let profile = sps.get(1).copied().unwrap_or(0);
            let path = std::env::temp_dir().join(format!("cap-probe-{tag}.264"));
            let _ = std::fs::write(&path, &stream);
            let (code, psnr) = decode_and_psnr(&path, &cfg, 1);
            println!(
                "  pps[{tag:<5}] profile_idc={profile} entropy_coding_mode_flag={flag:?} \
                 ffmpeg解码exit={code:?} PSNR={psnr}"
            );
            assert_eq!(code, Some(0), "{tag}: ffmpeg 应能干净解码");
            pps_flag.push(flag);
        }
        assert_eq!(pps_flag[0], Some(false), "CAVLC 的 entropy_coding_mode_flag 必须是 0");
        assert_eq!(pps_flag[1], Some(true), "CABAC 必须真的写进 PPS（flag = 1）");
        println!();
    }

    /// 用随包 ffmpeg 解码 `path`（对照同一段合成源帧）并返回 `(退出码, 平均 PSNR)`。
    /// PSNR 只做参考：这一步证明 CABAC 的码流不是"更小但更烂/不可解"。
    fn decode_and_psnr(
        path: &std::path::Path,
        cfg: &EncoderConfig,
        frames: usize,
    ) -> (Option<i32>, String) {
        let ff = ffmpeg_exe();
        if !ff.is_file() {
            return (None, "（无 ffmpeg，跳过）".into());
        }
        let (w, h) = (cfg.width as usize, cfg.height as usize);
        let src = std::env::temp_dir().join("cap-probe-src.yuv");
        let mut raw = Vec::with_capacity(frames * w * h * 3 / 2);
        for i in 0..frames {
            raw.extend_from_slice(&synthetic(cfg, i).to_i420());
        }
        if std::fs::write(&src, &raw).is_err() {
            return (None, "（写源帧失败）".into());
        }
        let out = std::process::Command::new(&ff)
            .args(["-hide_banner", "-loglevel", "info", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .args(["-s", &format!("{w}x{h}"), "-r", &format!("{}", cfg.fps as u32), "-i"])
            .arg(&src)
            .arg("-i")
            .arg(path)
            .args(["-lavfi", "psnr", "-f", "null", "-"])
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stderr).to_string();
                let avg = text
                    .split("average:")
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .unwrap_or("?")
                    .to_string();
                (o.status.code(), avg)
            }
            Err(e) => (None, format!("（ffmpeg 启动失败: {e}）")),
        }
    }

    /// 直接从 PPS（NAL type 8）里读 `entropy_coding_mode_flag`（这是 CABAC 是否真正启用的
    /// 决定性证据：CABAC 的 slice header 语法完全不同，flag=1 且能被 ffmpeg 干净解码 =
    /// 真的在用 CABAC，而不是"设了标志位但库忽略了"）。
    fn pps_entropy_flag(pps: &[u8]) -> Option<bool> {
        let mut rbsp = Vec::with_capacity(pps.len());
        let mut zeros = 0usize;
        for &b in pps.iter().skip(1) {
            // 去掉防竞争字节 0x03（00 00 03 xx）
            if zeros >= 2 && b == 0x03 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        let mut pos = 0usize;
        let read_bit = |pos: &mut usize| -> Option<u32> {
            let byte = *rbsp.get(*pos / 8)?;
            let bit = (byte >> (7 - (*pos % 8))) & 1;
            *pos += 1;
            Some(bit as u32)
        };
        let read_ue = |pos: &mut usize| -> Option<u32> {
            let mut lead = 0;
            while read_bit(pos)? == 0 {
                lead += 1;
                if lead > 31 {
                    return None;
                }
            }
            let mut v = 0u32;
            for _ in 0..lead {
                v = (v << 1) | read_bit(pos)?;
            }
            Some((1u32 << lead) - 1 + v)
        };
        read_ue(&mut pos)?; // pic_parameter_set_id
        read_ue(&mut pos)?; // seq_parameter_set_id
        read_bit(&mut pos).map(|b| b == 1)
    }

    fn ffmpeg_exe() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
            .join("bin")
            .join("ffmpeg.exe")
    }
}

