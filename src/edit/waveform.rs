//! **剪辑页音频波形**：可见区间密集切片 + **全片密集基准层**（P22）。
//!
//! 分工（与 `ui::editor` 的边界）：本模块只做"**数据**"——分桶、解码调度、缓存、
//! 阶段耗时；**一个像素都不画**。界面只负责"按同一套时间→x 映射把段画出来"。
//!
//! # 两层结构（P22：L0 稀疏概览已被**全片密集基准层**取代）
//!
//! | 层 | 桶宽 | 覆盖 | 怎么得到 | 代价 |
//! |---|---|---|---|---|
//! | **全片基准层** | `BASE_BUCKET_MS` = **100 ms** | **整片**（1 h ≈ 36,000 片 ≈ 288 KB） | `WAVE_WORKERS` = **4** 个后台线程按 `SEGMENT_MS` = **8 min** 分段**并行**解码 | **与片长成正比**（1 h 密集解码 ≈ 几秒，4 线程） |
//! | **可见区间（密集）** | 档位表里**不粗于 `可见时长 / 600`** 的最粗一档 | 恒 **600** 个左右 | `seek` 到瓦片起点、**逐包解满整片**（不跳窗口） | 与**可见时长**成正比，按 `TILE_MS` 瓦片缓存 |
//!
//! **哪一层上场由可见时长唯一决定**：`可见时长 <= ``DENSE_MAX_VISIBLE_MS`（120 s）
//! → **密集层**（基准层只做"还没细化到"的兜底）；再长 → **基准层**
//! —— 那里**不再是**"每桶只解中心 0.25 s、其余填充"的稀疏概览，而是 100 ms 一格的真数据。
//!
//! P20 及以前，长可见区间走的是 **L0 稀疏概览**（固定 1024 桶、每桶只解中心 0.25 s）。
//! 它的好处是"代价与片长无关"，代价是**放大之后必然露出稀疏填充的块状**，
//! 而且"适应全片"视图与"放大后的视图"是两套互不相干的口径。
//! P22 把两套合并成一套：**整片都按 100 ms 密集算一遍**（后台 4 线程），
//! 于是任何视口的数据都来自**同一条口径**（真解码的 min/max），
//! 只有"更细"这一件事由可见区间的密集层负责。
//!
//! # 可见区间最优先（P22 补充 1）
//!
//! 打开后：**先解当前可见区间**（密集层，毫秒级可见），同时 4 个后台线程补齐全片基准层；
//! 平移/缩放时把**新的可见区间**写进共享计划（[`BasePlan::set_visible`](crate::edit::waveform::BasePlan::set_visible)），
//! 每个 worker 在下一次取任务时按"离可见区间最近"挑段 —— 于是"新进入的区间"插队，
//! 远处的段自然后算。**不取消在算的段**（一段 8 min ≈ 百毫秒级解码），
//! 也不需要"取消过期请求"那套：段是**幂等且互不重叠**的，算完即入层。
//!
//! # 并行 = 多段独立解码（P22 补充 2）
//!
//! AAC 解码本身单线程，但可以**各开一套 demuxer + decoder** 负责一段：
//! 每段 `av_seek_frame`(音频流) → 预热 `WARMUP_PACKETS` 个包 → 丢弃段边界之前的采样 →
//! **按包 PTS 绝对定位**（而不是"从 0 数采样点"），于是段与段之间**不会出现可见断层**。
//! `AAC_PRIMING_MS` 常量给出 AAC 编解码 priming 的量级与依据，作为 seek 的提前量与预热量。
//!
//! **关键不变式（有单测钉住）**：并行（4 线程分段）与单线程（整片顺序解）的基准层
//! 逐片一致 —— 同一段素材、同一套分桶，两条路径必须给出同一份 min/max。
//!
//! # 可见区间 = 恒定 600 个切片（P18；P20 换了上场门槛）
//!
//! "用户看得见的那一段"走**另一条路**：**密集解码**（逐包解满整片瓦片，
//! 不跳窗口），分辨率由 `VISIBLE_BUCKETS` 决定 ——
//! `桶宽 = 可见时长 / 600`，再向下取到 [`tier_for_visible_ms`](crate::edit::waveform::tier_for_visible_ms) 给出的**离散档位**
//! （`DENSE_TIERS_MS`：`TILE_MS` 的因数序列 200→…→1 ms）：
//!
//! * **档位单调**：可见时长减半 ⇒ 所需档位**严格更细**（细到 `MIN_BUCKET_MS` 为止）；
//! * **档位稳定**：档位只在跨过档位边界时才换，1 px 的缩放不会触发重算；
//! * **档位只服务缓存**（P20）：把桶宽量化到离散档位，让"平移回来 / 来回缩放"命中
//!   同一份瓦片；**画的时候永远按当前可见区间的 `时间 → x` 映射落位**
//!   （[`segments_for_view`](crate::edit::waveform::segments_for_view) 的段自带 `start_ms/end_ms`，没有任何"按瓦片拉伸"的余地）；
//! * **覆盖到门槛**：最粗一档 = `可见门槛 / ``VISIBLE_BUCKETS`（120 s / 600 = 200 ms），
//!   于是"可见区间 <= 120 s"这条路上**桶数恒为 600**（不会被上限夹成更少）；
//! * **封顶**：桶宽细到 `MIN_BUCKET_MS` = 1 ms（= 48 个采样点 @ 48 kHz）就不再细化 ——
//!   再细只是把同一个采样点抄几遍，没有新信息；此时可见区间**完整覆盖**（无空洞）。
//!
//! **哪一层上场只看可见时长**：`可见时长 <= ``DENSE_MAX_VISIBLE_MS` = 120 s → 密集层；
//! 再长 → **基准层**（[`segments_for_view`](crate::edit::waveform::segments_for_view)）。P18 的门槛曾是"可见区间短于一片瓦片（5 s）"，
//! 于是从"适应全片"放大到 5 s 之前画面**恒为稀疏概览**（每桶只解中心 0.25 s、其余填充）——
//! 这正是用户说的"缩放时波形不更新"。
//!
//! # 渐进 · 取消 · 降级
//!
//! * **渐进**：基准层每算完一段就把它写进共享层（[`BasePlan`](crate::edit::waveform::BasePlan)）并唤醒界面一次，
//!   界面"算到哪画到哪"；只有**整片切片**（100 ms 对齐）才算数。
//! * **取消**：单次解码的 `emit` 回调返回 `false` 即停；worker 在每个包之间查一次取消标志，
//!   而每段的解码量本身**有上界**（8 min 音频），所以取消是毫秒到几十毫秒级。
//! * **降级**：没有音轨 / 解码器没编进来 → 返回 [`AppError::Audio`] 且消息里带上编码名，
//!   调用方**如实降级**（不画波形 + 记日志 + 状态栏提示），绝不 panic、绝不卡界面。
//!
//! # 落盘缓存（只缓存**完整的**基准层）
//!
//! 键 = 路径 + 大小 + 修改时间 + `ALGO_VERSION`（算法/常量版本），文件放既有的 `Cache\`
//! （沿用"程序目录 → 配置目录 → 临时目录"的回退链，**不写用户文档目录**）。
//! 读的时候**校验 magic / 版本 / 长度 / 取值范围**，任何一处不对就当未命中重算 —— 绝不 panic。
//! 1 小时的基准层 = 36,000 个 `Peak` + 一张 done 位图 ≈ 292 KB。

use crate::error::AppError;
use crate::mux::av_err;
use ffmpeg_next::ffi as ff;
use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// **分阶段耗时日志**：`[cap][t=..ms] phase=wave_* scope=wave dur=..ms extra="..."`。
///
/// 为什么复用 [`crate::ui::startup::mark_with`] 而不是自己开一个日志文件：
/// 落盘策略（release 写 `cap.log` / debug 打控制台）、时间轴零点、格式都是**同一套**，
/// 于是"波形的阶段耗时"与"启动阶段耗时"在同一个文件里按同一格式排在一起，
/// 一句 `Select-String 'scope=wave'` 就能捞出全部证据（见模块文档"分阶段耗时"一节）。
fn wave_phase(phase: &'static str, d: Duration, extra: impl Into<String>) {
    crate::ui::startup::mark_with(phase, "wave", Some(d), Some(extra.into()));
}

/// 内部统一的解码采样率（Hz）。
///
/// 为什么定 48 kHz：录制侧就是 48 kHz（WASAPI 环回/麦克风混音后），所以绝大多数素材
/// **不需要重采样**；它还能被 [`MIN_BUCKET_MS`] 整除（48 个采样点 = 1 ms），
/// 于是密集层的桶边界永远精确落在采样点上，不靠取整糊过去。
///
/// P54：值改为取自 [`crate::audio::SAMPLE_RATE`]（链路统一采样率的唯一来源）——
/// 这里若另写一份 48_000，录制侧改采样率时波形解码会静默地按老速率跑，
/// 症状只是"波形时长对不上"，编译器不会有任何提示。
const DECODE_RATE: i32 = crate::audio::SAMPLE_RATE;

/// **全片密集基准层的桶宽**（毫秒）：每 100 ms 一个切片（min/max）。
///
/// 为什么是 100 ms（用户 2026-10-14 确认的参数）：
/// * **1 小时 = 36,000 片**（`3600_000 / 100`），每片两个 `f32` ⇒ **288 KB** 内存 / 磁盘，
///   与"为了整片视图再退化成稀疏填充"相比，这个代价可以忽略；
/// * 它与密集档位表**天然接续**：门槛处（120 s 视口）的所需桶宽正好是 200 ms，
///   而 100 ms 是它的一半 —— 于是"长视口用基准层、短视口用密集层"的切换在视觉上
///   只是"切片从 2 片/像素变成 1 片/像素"，不会出现明显的粗细跳变；
/// * 比"总时长/1024"那种**变宽**切片好：基准层的切片宽是**常量**，
///   于是落盘格式、切片索引、优先级计算都不依赖时长（时长只决定切片个数）。
pub const BASE_BUCKET_MS: i64 = 100;

/// **基准层切片的个数上限**（护栏，不是精度常量）：24 小时 ⇒ 864,000 片 ≈ 6.9 MB。
///
/// 超过它的时长（现实中不存在）会把基准层**截断**到这一段，
/// 于是"分配一个 `Vec` 把内存吃光"这件事在结构上不可能发生。
pub(crate) const BASE_SLICES_MAX: usize = 24 * 3600 * 10;

/// **分段长度**（毫秒）：每个工作线程一次解满 8 分钟，然后领下一段。
///
/// 为什么是 8 分钟（原 5 分钟，2026-10-18 用户要求改大）：
/// * **段越短 → 段数越多 → `av_seek_frame` + 打开解码器的固定开销越多**；
///   段越长 → 单段解码时间越长 → "可见区间插队"的响应越钝（在算的那一段不能被打断）。
///   8 分钟音频 ≈ 一秒以内的解码量，固定开销仍可忽略、插队延迟仍可接受；
///   用户侧的心智模型也更顺（"一段 ≈ 一个主题/一节课"，1 小时 = 7.5 段）；
/// * 它正好是 [`TILE_MS`]（5 s）的 96 倍、[`BASE_BUCKET_MS`] 的 4800 倍 ——
///   段边界与切片边界**严格对齐**（整除），于是"往共享层写一段"就是一段连续下标，不需要补零头。
pub(crate) const SEGMENT_MS: i64 = 480_000;

/// **基准层的工作线程数**（用户 2026-10-14 确认：**4 线程**）。
///
/// 为什么不是 8：AAC 解码是纯 CPU 的批处理，4 个 worker 加上 1 个可见区间细化线程
/// 已经把常见的 4~8 核吃满；再往上只会在"与界面/预览/编码抢核"上亏回去
/// （用户明确要求"4 线程，别开 8"）。
pub const WAVE_WORKERS: usize = 4;

/// **AAC 编解码 priming 的量级**（毫秒）—— 分段解码必须补偿的那一段。
///
/// **依据**：AAC 一帧固定 **1024 个采样点**；本项目解码侧统一 48 kHz
/// （`DECODE_RATE`），于是 1 帧 = `1024 / 48000 s` = **21.33 ms**。
/// 编码器会把输入缓冲 1~2 帧（取决于实现与 `initial_padding` / edit list 的写法）
/// 再开始输出，解码侧对应的就是开头多出来的那 1~2 帧 —— 也就是 **21 ~ 44 ms**。
///
/// 本模块**不靠这个常量"平移"数据**（那是猜），而是靠两条硬措施：
/// 1. **seek 提前 [`AAC_PRIMING_MS`]**：段起点往前多要 44 ms，保证"边界前的那个包"
///    一定被解到，不会因为 seek 落在目标之后而丢掉一小段音频；
/// 2. **丢弃段边界之前的采样**：落位不是"从 0 数采样点"，而是**按包 PTS 绝对定位**
///    （见 [`RangeDecoder::read_range_pts`]）—— priming 由容器/解码器（`skip_samples`、
///    edit list）如实表达在 PTS 上，于是"补偿"这件事是**精确**的，不需要估算。
///
/// 这个常量因此同时是"seek 提前量"与"预热窗口的上界"，
/// 并作为"为什么段与段之间不会出现可见断层"这条不变式的书面依据。
pub const AAC_PRIMING_MS: i64 = 44;

/// **seek 之后先丢弃几个包**（预热）：让解码器的重叠-相加（MDCT）历史与重采样器状态收敛。
///
/// AAC 的帧间重叠只依赖**前一帧**，所以 2 个包已经足够（第 3 个包起输出与顺序解码逐位一致）
/// —— 这正是"并行结果 == 单线程结果"那条单测能成立的物理前提。
pub(crate) const WARMUP_PACKETS: usize = 2;

/// **算法 / 常量的版本号**：任何一个常量（桶数、切片宽、档位、瓦片长度、段长度…）变了
/// 都必须 +1 —— 它是落盘缓存键的一部分，也是缓存文件头里校验的第二个字段。
/// 不改它就会出现"新算法读了旧缓存、画出一条形状不对的波形"这种最难查的错。
///
/// v4（P18）：可见区间从"固定 1 ms/桶 + 稀疏窗口只算中心"改成
/// "`VISIBLE_BUCKETS` = 600 驱动档位 + 密集解码"。
///
/// v5（P20）：**"哪一层上场"的门槛从"一片瓦片（5 s）"改成 120 s**，档位表也随之
/// 从 `[5000…1]` 换成 `[200…1]`（每一档仍是 [`TILE_MS`] 的因数）。
///
/// v6（P22）：**L0 稀疏概览被全片密集基准层取代**（100 ms/片、4 线程、5 分钟分段、
/// 按 PTS 绝对定位 + AAC priming 补偿）。落盘格式**整个换了一版**
/// （`wave-base-*.bin`，带 done 位图），旧文件按"magic / 版本不符"当未命中处理。
///
/// v7（2026-10-18，用户要求）：**分段长度 5 min → 8 min**（`SEGMENT_MS`
/// 300_000 → 480_000）。数据口径（100 ms/片、按 PTS 绝对定位）没变，
/// 但按"常量一变就升版本"的纪律照样 +1：旧缓存即使内容一致也整体作废重算
/// （一次性的几秒代价，换"绝不读旧口径数据"的保证）。
pub(crate) const ALGO_VERSION: u32 = 7;

/// **可见区间恒定切成这么多片**（P18，用户口径：`VISIBLE_BUCKETS = 600`）。
///
/// 为什么是"恒定 600"而不是"按像素算"：
/// * 用户原话是「一直按可见区域内 600 个切片计算，放到最大时，可以看到完整波形」——
///   桶数恒定 ⇒ 段数恒定 ⇒ **任何缩放级别下"一个切片 ≈ 一个像素"**（600 px 宽的时间线），
///   视觉密度不随缩放跳变；
/// * 它是**单一来源**：桶宽 = `可见时长 / 600`（见 [`visible_bucket_ms`]），
///   档位选择、绘制切层、细化请求全都从它推出来，没有任何第二处硬编码分辨率。
pub const VISIBLE_BUCKETS: usize = 600;

/// **可见区间能算到的最细桶宽**（毫秒）：[`MIN_BUCKET_MS`] = 1 ms。
///
/// 为什么封顶在 1 ms：解码侧统一 48 kHz（`DECODE_RATE`），1 ms 正好是 **48 个采样点**。
/// 比 1 ms 更细的桶只会把同一个采样点抄进相邻几桶（峰值完全相同），是纯重复劳动，
/// 没有任何新信息 —— 所以 `可见时长 / 600` 小于 1 ms 时一律用 1 ms 这一档。
pub(crate) const MIN_BUCKET_MS: i64 = 1;

/// **密集档位表**（毫秒 / 桶）—— [`TILE_MS`] = 5 s 的**因数**里挑出的一条梯子（从粗到细）。
///
/// 三条硬约束（单测 `tile_and_bucket_time_mapping_is_consistent` 逐档钉住）：
/// 1. **每一档都整除 [`TILE_MS`]**：一片瓦片的桶数恒为 `TILE_MS / 档位`（整数），
///    桶边界恒落在采样点上，也**不会出现"最后半桶"**（那会让波形在片尾抽一下）；
/// 2. 相邻档位比例落在 `(1, 2.5]`：一次滚轮（1.1~1.25×）最多换一档；
/// 3. **最粗一档 = 门槛处的所需桶宽** = [`DENSE_MAX_VISIBLE_MS`] / [`VISIBLE_BUCKETS`]
///    = 120_000 / 600 = **200 ms** —— 于是"可见区间 <= 门槛"这条路上`桶数恒为 600`
///    （桶宽永不被 [`TILE_MS`] 夹住），而门槛之外交给**基准层**（见 [`segments_for_view`]）。
///
/// P20 之前这一档表是 `[5000, 2500, …, 1]`（最粗 5000）：那时上场门槛是"一片瓦片"，
/// 最粗档只需要 == `TILE_MS`。门槛改成 120 s 之后，最粗的**有用**档位变成 200 ms ——
/// 5000 ms 那一档在"可见 <= 120 s"时永远选不到（可见 120 s 只需 200 ms/桶），
/// 留着只会让"档位表看起来覆盖了很粗的区间"而掩盖真实的覆盖边界，所以删掉。
///
/// 1000/100/10/1 是旧实现里就写死过的那几档分辨率，
/// 这里把它们**取整扩张**成一条覆盖"200 ms → 1 ms"的完整梯子（5 s 的因数序列）。
pub(crate) const DENSE_TIERS_MS: [i64; 10] = [200, 125, 100, 50, 25, 20, 10, 5, 2, 1];

/// **密集层的可见时长门槛**（毫秒）：可见区间**不长于**它就走密集切片。
///
/// 为什么是 120 s（P20，用户确认的参数）：
/// * 门槛处的所需桶宽 = `120 s / 600` = **200 ms**，正好是 `DENSE_TIERS_MS` 的最粗一档 ——
///   "档位表覆盖到门槛"这条不变式由这个等式同时钉住（单测
///   `p20_dense_tiers_cover_exactly_up_to_the_visible_threshold`）；
/// * 一屏最多铺开 `120 s / ``TILE_MS`` = **24 片瓦片**（60 万采样点 ≈ 2.4 MB 解码量），
///   这个量级"放大一下就能立刻看到结果"，不会让界面等；
/// * 用户原话是「一直按可见区域内 600 个切片计算，放到最大时，可以看到完整波形」，
///   而用户真正会用的缩放区间（几秒 ~ 两三分钟）**全部落在门槛之内** ——
///   不能再让"适应全片 → 5 s"这段中间地带退回稀疏窗口（那正是 P20 要修的根因）；
/// * P22 起门槛之外**不再退化成稀疏填充**：交给 100 ms 一片的全片密集基准层
///   （[`BASE_BUCKET_MS`]，见 [`segments_for_view`]）。
pub const DENSE_MAX_VISIBLE_MS: f64 = 120_000.0;

/// **可见时长 `visible_ms` 需要的桶宽**（毫秒）＝ `可见时长 / `[`VISIBLE_BUCKETS`]，
/// 夹到 `[``MIN_BUCKET_MS``, `门槛所需桶宽`]`。
///
/// 上限取 `门槛所需桶宽`（= 200 ms）：可见区间长到超过 [`DENSE_MAX_VISIBLE_MS`] 时
/// "600 个切片"已经不是密集层的事了（那要解几百秒音频），由 [`segments_for_view`]
/// 交给基准层；把上限夹在这里，`tier_for_visible_ms` 就**永远不会**给出比门槛更粗的档
/// —— "档位表覆盖到门槛"于是不需要额外的边界判断。
pub fn visible_bucket_ms(visible_ms: f64) -> f64 {
    if !visible_ms.is_finite() || visible_ms <= 0.0 {
        return MIN_BUCKET_MS as f64;
    }
    (visible_ms / VISIBLE_BUCKETS as f64).clamp(MIN_BUCKET_MS as f64, dense_max_tier_ms() as f64)
}

/// **门槛处的所需桶宽**（= 最粗档、= [`VISIBLE_BUCKETS`] 个切片铺满门槛时长）。
///
/// 由 `门槛 / 桶数` 推出来（不是独立常量）：这样"档位表最粗一档 == 门槛处所需桶宽"
/// 这条不变式改一个数字就同时成立，不会出现"改了门槛忘了改档位表"。
pub(crate) fn dense_max_tier_ms() -> i64 {
    (DENSE_MAX_VISIBLE_MS / VISIBLE_BUCKETS as f64).round() as i64
}

/// **选出该用哪一档桶宽**：满足 `档位 ≤ 所需桶宽` 的**最粗**一档（`可见时长` 越短 → 档位越细）。
///
/// 这条规则直接来自用户的"不要每次 1 px 缩放就换档重算"：取"不粗于所需"的**最粗**一档，
/// 于是只要 `可见时长 / 600` 没跨过档位边界，桶宽就一动不动。
///
/// 单调性（`tier_index_for_visible_ms` 的单测钉着）：
/// `visible_ms` 减小 ⇒ 所需桶宽减小 ⇒ 选中的档位**不会变粗**（只可能更细或不动）。
pub fn tier_for_visible_ms(visible_ms: f64) -> i64 {
    DENSE_TIERS_MS[tier_index_for_visible_ms(visible_ms)]
}

/// [`tier_for_visible_ms`] 的档位**下标**（`0` = 最粗的那一档 `DENSE_TIERS_MS[0]`）。
///
/// 单独暴露下标是为了让单测能断言"严格更细"（比较下标，而不是比较桶宽数值）。
pub(crate) fn tier_index_for_visible_ms(visible_ms: f64) -> usize {
    let want = visible_bucket_ms(visible_ms);
    for (i, t) in DENSE_TIERS_MS.iter().enumerate() {
        // 档位从粗到细：第一个"不粗于所需"的就是答案（最粗的满足者）
        if (*t as f64) <= want {
            return i;
        }
    }
    DENSE_TIERS_MS.len() - 1
}

/// 第 `tier` 档一片瓦片里有几个桶（由 [`TILE_MS`] / 档位推出，不是独立常量）。
pub(crate) fn buckets_per_tile(tier_ms: i64) -> usize {
    if tier_ms <= 0 {
        return 0;
    }
    (TILE_MS / tier_ms).max(1) as usize
}

/// **可见区间最多摊开几片瓦片**（运行时护栏，不是精度常量）。
///
/// P20 的上场门槛是 [`DENSE_MAX_VISIBLE_MS`] = 120 s，最粗档位 200 ms ⇒
/// 一片 5 s 瓦片里有 `5000 / 200` = 25 个桶 ⇒ 一屏最多 `120_000 / TILE_MS` = **24 片**。
/// 这里取 **32**（比真实上界多留几片的余量：可见区间的两端会各压到一片不完整的瓦片）。
///
/// 它只在"用一个远粗于档位表的区间去请求密集瓦片"这种异常输入下才生效
/// （正常路径：可见区间的片数恒 <= 24），作用是**给"一次收敛要遍历多少片"一个硬上界**，
/// 其余部分由基准层兜底（见 [`segments_for_view`]）。
pub const MAX_DENSE_TILES_PER_VIEW: usize = 32;

/// **密集结果按固定 5 秒瓦片缓存**（毫秒）。
///
/// 为什么是瓦片而不是"按请求区间缓存"：请求区间随缩放/平移每帧都在变（且互相重叠），
/// 按请求缓存等于没有缓存；固定网格让"平移回来"和"换缩放级别"都能命中同一份结果。
/// （P18：瓦片**自身的长度**不变，变的是"一片里有几个桶"——见 [`buckets_per_tile`]。）
pub(crate) const TILE_MS: i64 = 5_000;

/// **最细档位**一片瓦片里有几个桶（[`MIN_BUCKET_MS`] ⇒ 5000 桶 / 5 s）。
///
/// 这是"一片瓦片最多多大"的内存上界：5000 × 8 B = 40 KB，LRU 64 片 ⇒ ≤ 2.6 MB。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) const TILE_BUCKETS: usize = (TILE_MS / MIN_BUCKET_MS) as usize;

/// **密集瓦片缓存的 LRU 上限**（片，**跨档位合计**）。
///
/// 64 片 × 5 s = 320 s 的音频，覆盖"一个屏幕最多几秒~几十秒"的任何视图都绰绰有余；
/// 内存上界 ≈ 64 × 5000 × 8 B = 2.6 MB（最细档位；粗档位一片只有几个桶，更小）。
///
/// 为什么**跨档位**合计而不是每档 64：档位有 10 个，每档各留 64 片就等于没有上限
/// （10 × 2.6 MB）。合计 64 片既守住了内存，又天然让"用户刚看过的那个档位"留在缓存里。
///
/// P20 起还有一条**配套约束**：一屏最多要 24 片（[`MAX_DENSE_TILES_PER_VIEW`] 的文档），
/// 64 > 24 保证"当前可见区间的完整覆盖"永远装得下 —— 否则会出现"一边要数据、
/// 一边把刚算好的片挤掉"的抖动（界面侧的"已请求"集合必须按这条口径修剪，见 `pump_refine`）。
pub const TILE_CACHE_MAX: usize = 64;

/// 一次"细化"请求最多同时排队多少片（护栏：极端缩小/拖动时不至于把队列灌爆）。
pub const TILE_REQ_MAX: usize = 64;

/// `f32` 采样点 → `[0,1]` 的限幅（解码后本来就在范围内，这里是**防御**：
/// 损坏文件给出 ±1e30 时不能把绘制坐标算成 `inf`）。
fn clamp01(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(-1.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// 全片密集基准层：固定 100 ms 切片 + 4 线程分段并行
// ---------------------------------------------------------------------------

/// 一个解码窗口的**区间**（毫秒，相对文件开头，`[start_ms, start_ms + samples.len())`）。
///
/// 有了它，任何一段样本都能被换算成"它在时间轴上的位置" ——
/// 这是密集瓦片与基准层共用的一条口径（两者的桶边界都由它来对齐）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkRange {
    pub start_ms: i64,
    pub len: usize,
}

/// [`ChunkRange`] 覆盖的结束时刻（毫秒）。
pub(crate) fn chunk_end_ms(c: ChunkRange) -> i64 {
    c.start_ms + (c.len as i64) * 1000 / DECODE_RATE as i64
}

/// **全片密集基准层**：整片按 [`BASE_BUCKET_MS`] 一格切开的 min/max。
///
/// 它是"任何视口都直接画"的那份数据（P22）：
/// * **切片宽是常量**（不像旧 L0 的"总时长/1024"），于是索引与时间的关系是纯乘法；
/// * **`done[i]` 如实记录哪几片已经有真数据**：worker 只写自己那一段，
///   界面只画 `done[i]` 的片 —— "算到哪画到哪"就是这一条位图；
/// * **不可变快照式共享**：worker 写完之后把整层克隆成新的 `Arc<BaseLayer>`
///   发布出去（见 [`BasePlan`]），读的人永远看到一份自洽的数据，不需要细粒度锁。
#[derive(Debug, Clone, PartialEq)]
pub struct BaseLayer {
    /// 素材总时长（毫秒）；决定切片个数，也决定切片到时间的换算
    pub duration_ms: i64,
    /// 切片宽（毫秒）—— 恒为 [`BASE_BUCKET_MS`]（落盘时也要校验，见 `decode_base`）
    pub bucket_ms: i64,
    /// 每一片的峰值；长度 = [`BaseLayer::slice_count`]
    pub peaks: Vec<Peak>,
    /// 每一片是否有真数据（false = 还没算到 / 那一段解码失败）
    pub done: Vec<bool>,
    /// 已经有真数据的片数（O(1) 读，用于进度与完成判定）
    pub done_slices: usize,
    /// 所有段都**尝试过**了（含失败的段）：渐进结束
    pub finished: bool,
    /// 打不开 / seek 失败 / 解码失败的段数（只记日志与取证，不影响绘制）
    pub failed_segments: usize,
}

impl BaseLayer {
    /// 空层（时长为 `duration_ms`，一片都没算）。
    pub fn empty(duration_ms: i64) -> Self {
        let n = base_slice_count(duration_ms);
        Self {
            duration_ms: duration_ms.max(0),
            bucket_ms: BASE_BUCKET_MS,
            peaks: vec![Peak::ZERO; n],
            done: vec![false; n],
            done_slices: 0,
            finished: false,
            failed_segments: 0,
        }
    }

    /// 整片切片的个数（= `ceil(时长 / 100 ms)`，夹到 `BASE_SLICES_MAX`）。
    pub fn slice_count(&self) -> usize {
        self.peaks.len()
    }

    /// 有没有任何可画的峰值。
    pub fn has_peaks(&self) -> bool {
        self.done_slices > 0
    }

    /// 是不是**每一片都有数据**（= 整片基准层完整）。
    pub fn is_complete(&self) -> bool {
        !self.peaks.is_empty() && self.done_slices >= self.peaks.len()
    }

    /// 已经覆盖的比例（`0.0..=1.0`；空层 = 1.0，避免界面显示 0/0）。
    pub fn coverage(&self) -> f64 {
        if self.peaks.is_empty() {
            1.0
        } else {
            self.done_slices as f64 / self.peaks.len() as f64
        }
    }

    /// 第 `i` 片覆盖的毫秒区间 `[start, end)`（纯乘法）。
    pub fn span_of(&self, i: usize) -> (i64, i64) {
        let w = self.bucket_ms.max(1);
        (i as i64 * w, (i as i64 + 1) * w)
    }

    /// 把**一段**的结果写进层里（按切片下标写入）。
    ///
    /// `start_slice` 必须与 `peaks` 的第一项对齐（由 `segment_slice_range` 给出），
    /// 于是"段边界"这件事只有一处定义，worker 与单测用的是同一套下标。
    /// 越界的部分被丢弃（绝不 panic）。
    pub fn write_slices(&mut self, start_slice: usize, peaks: &[Peak]) {
        for (k, p) in peaks.iter().enumerate() {
            let i = start_slice + k;
            if i >= self.peaks.len() {
                break;
            }
            if !self.done[i] {
                self.done[i] = true;
                self.done_slices += 1;
            }
            self.peaks[i] = Peak {
                min: clamp01(p.min),
                max: clamp01(p.max),
            };
        }
    }

    /// **连续覆盖到哪一刻**（毫秒）：从 0 开始第一段"没数据"的切片起点。
    ///
    /// 界面用它画"算到这儿"的渐进指示线 —— 与旧 L0 的 `available_ms` 同一条口径，
    /// 但这里它是**真的连续**（基准层按段填充，从可见区间往外长）。
    pub fn covered_ms(&self) -> i64 {
        for (i, d) in self.done.iter().enumerate() {
            if !*d {
                return self.span_of(i).0;
            }
        }
        self.duration_ms.max(0)
    }

    /// 有数据的切片数（= `done_slices` 的慢速重算，只在单测里用来自证一致）。
    pub fn count_done(&self) -> usize {
        self.done.iter().filter(|d| **d).count()
    }
}

/// 时长 → 基准层切片个数（`ceil(时长 / `[`BASE_BUCKET_MS`]`)`，夹到 [`BASE_SLICES_MAX`]）。
pub(crate) fn base_slice_count(duration_ms: i64) -> usize {
    if duration_ms <= 0 {
        return 0;
    }
    let w = BASE_BUCKET_MS.max(1);
    let n = (duration_ms + w - 1) / w;
    (n.max(0) as usize).min(BASE_SLICES_MAX)
}

/// 时长 → 基准层分段个数（`ceil(时长 / ``SEGMENT_MS``)`，至少 0）。
pub fn segment_count(duration_ms: i64) -> usize {
    segment_count_of(duration_ms, SEGMENT_MS)
}

/// [`segment_count`] 的**显式段长**版本。
///
/// 单测需要"用中等素材 + 多段"来覆盖段边界（见 `p22_parallel_matches_single_thread_slice_by_slice`），
/// 所以段长在这里是一个参数；生产路径一律走 [`SEGMENT_MS`]。
pub(crate) fn segment_count_of(duration_ms: i64, seg_ms: i64) -> usize {
    if duration_ms <= 0 {
        return 0;
    }
    let s = seg_ms.max(1);
    ((duration_ms + s - 1) / s).max(0) as usize
}

/// 第 `seg` 段覆盖的毫秒区间 `[start, end)`（`end` 已夹到片内）。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn segment_span_ms(seg: usize, duration_ms: i64) -> (i64, i64) {
    segment_span_ms_of(seg, duration_ms, SEGMENT_MS)
}

/// `segment_span_ms` 的**显式段长**版本。
pub fn segment_span_ms_of(seg: usize, duration_ms: i64, seg_ms: i64) -> (i64, i64) {
    let s = seg_ms.max(1);
    let start = seg as i64 * s;
    (start, (start + s).min(duration_ms.max(start)))
}

/// 第 `seg` 段对应的基准层切片下标区间 `[start_slice, end_slice)`。
///
/// [`SEGMENT_MS`] 是 [`BASE_BUCKET_MS`] 的整数倍（4800 倍），所以这里就是纯除法，
/// **段边界与切片边界严格对齐** —— 这是"写一段 = 写一段连续下标"的前提
/// （单测 `p22_segments_align_with_base_slices` 钉住）。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn segment_slice_range(seg: usize, duration_ms: i64) -> (usize, usize) {
    segment_slice_range_of(seg, duration_ms, SEGMENT_MS)
}

/// [`segment_slice_range`] 的**显式段长**版本。
pub(crate) fn segment_slice_range_of(seg: usize, duration_ms: i64, seg_ms: i64) -> (usize, usize) {
    let (a, b) = segment_span_ms_of(seg, duration_ms, seg_ms);
    let w = BASE_BUCKET_MS.max(1);
    let start = (a.max(0) / w) as usize;
    let end = ((b.max(0) + w - 1) / w) as usize;
    (start.min(base_slice_count(duration_ms)), end)
}

/// 时刻 `ms` 落在第几段里（非负；时长未知时恒为 0）。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn segment_of_ms(ms: i64, duration_ms: i64) -> usize {
    segment_of_ms_of(ms, duration_ms, SEGMENT_MS)
}

/// [`segment_of_ms`] 的**显式段长**版本。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn segment_of_ms_of(ms: i64, duration_ms: i64, seg_ms: i64) -> usize {
    let s = seg_ms.max(1);
    let n = segment_count_of(duration_ms, seg_ms).max(1);
    ((ms.max(0) / s) as usize).min(n - 1)
}

/// 时刻 `ms` 落在第几片基准层切片里（非负；空层恒为 0）。
pub(crate) fn base_slice_of_ms(ms: i64) -> usize {
    (ms.max(0) / BASE_BUCKET_MS.max(1)) as usize
}

// ---------------------------------------------------------------------------
// 可见区间（密集）：档位桶宽 + 固定 5 秒瓦片缓存
// ---------------------------------------------------------------------------

/// 时刻 `ms` 落在第几片瓦片里（非负）。
pub fn tile_of_ms(ms: i64) -> usize {
    (ms.max(0) / TILE_MS) as usize
}

/// 第 `tile` 片覆盖的毫秒区间 `[start, end)`。
pub(crate) fn tile_span_ms(tile: usize) -> (i64, i64) {
    (tile as i64 * TILE_MS, (tile as i64 + 1) * TILE_MS)
}

/// 第 `tile` 片里第 `bucket` 个桶（桶宽 = `tier_ms`）对应的毫秒区间。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn tile_bucket_span_ms(tier_ms: i64, tile: usize, bucket: usize) -> (i64, i64) {
    let (ts, te) = tile_span_ms(tile);
    let w = tier_ms.max(1);
    let start = ts + bucket as i64 * w;
    (start, (start + w).min(te))
}

/// 第 `tile` 片里第 `bucket` 个桶在**该片样本数组**里的下标范围 `[from, to)`。
///
/// 由"毫秒 × 48 个采样点 / 1000"直接乘出来（不是累加），所以永远精确落在采样点上
/// （[`DENSE_TIERS_MS`] 里每一档都能被 1000 整除，乘出来是整数）；
/// 越界时夹到数组长度（最后一桶不满是常态）。
pub(crate) fn tile_bucket_sample_range(
    tier_ms: i64,
    tile: usize,
    bucket: usize,
    samples: usize,
) -> (usize, usize) {
    let _ = tile; // 片内下标只由 bucket 决定；参数保留是为了"同源口径"读起来一致
    let per = ((tier_ms.max(1) * DECODE_RATE as i64) / 1000).max(1) as usize;
    let from = bucket.saturating_mul(per);
    (from.min(samples), from.saturating_add(per).min(samples))
}

/// **一片密集瓦片**：固定 5 秒长，桶宽 = 它自己的 `bucket_ms`（最多 `TILE_BUCKETS` 个）。
///
/// `peaks.len() < buckets_per_tile(bucket_ms)` 表示"只算到这儿"（渐进）或
/// "素材到这儿就结束了"。`dense` **恒为 true**：这一层的数据只能由
/// `scan_to_tile`（逐包解满整片）产出 —— 它是"可见区间不偷偷退化成稀疏窗口"
/// 这条口径在数据上的凭据（单测 `dense_tiles_never_come_from_the_sparse_window_path`
/// 就断言它）。
#[derive(Debug, Clone, PartialEq)]
pub struct DetailTile {
    pub tile: usize,
    /// 这一片**开始**的时刻（毫秒）；用于把桶序号换算成时间
    pub start_ms: i64,
    /// 这一片的**桶宽**（毫秒）= 生成它时的档位（`DENSE_TIERS_MS` 之一）
    pub bucket_ms: i64,
    pub peaks: Vec<Peak>,
    /// 这一片是否已经算到片尾（false = 还在算 / 被取消，界面只画已算出的部分）
    pub complete: bool,
    /// 是否来自**密集解码**路径（本项目里恒为 true；见类型文档）
    pub dense: bool,
}

impl DetailTile {
}

/// 一帧要画的**一段**：时间区间 + 峰值 + 数据来源。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaveSeg {
    pub start_ms: f64,
    pub end_ms: f64,
    pub peak: Peak,
    /// true = 本段的值来自**真解码**（可见区间密集瓦片，或作为主数据源的全片基准层）；
    /// false = **兜底**：可见区间还没细化到，先用 100 ms 基准层顶着（画得淡一档）
    pub detail: bool,
    /// 切片序号（密集层 = 片内序号；基准层 = 基准切片号）。
    /// 只用于日志/取证，绘制不依赖它。
    pub bucket: usize,
}

/// 本帧波形用的**数据来源**（口径的唯一判据，界面与单测都读它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLayer {
    /// 没有任何数据（还没算出来 / 降级）
    None,
    /// **全片基准层**（[`BASE_BUCKET_MS`] = 100 ms 一片）：可见区间长于 [`DENSE_MAX_VISIBLE_MS`]
    Base,
    /// **可见区间密集层**（`DENSE_TIERS_MS` 里的一档）：可见区间不长于门槛
    Dense,
}

/// [`segments_for_view`] 的结果：要画的段 + 这一帧的**分辨率口径**。
#[derive(Debug, Clone, PartialEq)]
pub struct WaveSegments {
    pub segs: Vec<WaveSeg>,
    /// 这一帧用的是哪一层（见 [`ViewLayer`]）
    pub layer: ViewLayer,
    /// 本帧切片的**时间宽**（毫秒）：密集层 = 档位；基准层 = [`BASE_BUCKET_MS`]；`None` 层 = 0
    pub bucket_ms: i64,
    /// 密集层里"还没算到"的可见时间区间（毫秒）——界面据此决定要不要请求细化。
    /// `None` = 没有缺口（全部命中 / 本帧根本不该请求密集瓦片）。
    pub refine_ms: Option<(i64, i64)>,
    /// 渐进指示线该画在哪一刻（毫秒）：= 连续覆盖到"的位置。
    /// `None` = 别画（没数据，或可见区间已经补满）。
    pub pending_ms: Option<i64>,
}

impl WaveSegments {
    /// 这一帧每一段的**时间区间** `(start_ms, end_ms)`（顺序 = 绘制顺序）。
    ///
    /// 存在的理由只有一个：让"绘制严格按当前可见区间的时间→x 映射落位"这件事
    /// **可以被断言**。同一档位下改变缩放时，拿两帧的这份序列一比就能看出
    /// 区别 ——
    /// * **正确**（按时对齐）：两帧里**同一时刻**的段起止完全一样，
    ///   只是窗口里**多出/少掉**若干段（段数变化、切片的**时间宽**相同）；
    /// * **错误**（整块拉伸）：段的时间宽会随缩放**变宽/变窄**，
    ///   即"同一批数据被铺到更大的时间跨度上"。
    ///
    /// 所以 P20 的单测用它钉住"不是拉伸"：见 `p20_painting_is_positioned_by_time_not_stretched`。
    pub fn bucket_spans(&self) -> Vec<(f64, f64)> {
        self.segs.iter().map(|s| (s.start_ms, s.end_ms)).collect()
    }
}

/// **可见区间内要画的段**（纯逻辑，绘制只负责按同一套时间→x 映射画出来）。
///
/// * `base`：**全片密集基准层**（P22；长可见区间的唯一数据源，也是密集层没算到那几段的兜底）；
/// * `tiles`：**已经算好**的密集瓦片（键 = 片号）—— 只读，不在这里触发计算；
/// * `t0/t1`：可见时间窗（毫秒）；
/// * `target_cols`：这一帧时间线大约有多少像素宽（**只为合并**：基准层在长视口下
///   可能有远多于像素的切片，逐片发 shape 会把绘制时间吃光）。0 或 1 = 不合并。
///
/// **分辨率与"哪一层上场"都由可见时长唯一决定**：
///
/// 1. **可见区间长于 [`DENSE_MAX_VISIBLE_MS`]（120 s）→ 走基准层**：
///    100 ms 一片、**只给已经有数据的片**（没算到的就是空白 + 一条渐进指示线）。
///    `refine_ms = None`（这里的缺口要等后台 worker 补，不该让界面去排密集瓦片）；
/// 2. **不长于门槛 → 走密集层**：`桶宽 = `[`tier_for_visible_ms`]`(可见时长)`
///    （= 可见时长/600 量化到档位），瓦片没算到的那几段用**基准层兜底**
///    （`detail = false`）+ 记进 [`WaveSegments::refine_ms`]。
///
/// 两层的**段都自带 `start_ms/end_ms`**，绘制只按 `x_of(段的时间)` 落位 ——
/// 于是"同一档位下改变缩放"必然表现为"**看到的段数变多/变少、位置按时间对齐**"，
/// 不可能出现"同一批数据被拉伸"（那种画法需要"按瓦片铺开"的代码，这里没有）。
pub fn segments_for_view(
    base: &BaseLayer,
    tiles: &std::collections::HashMap<usize, DetailTile>,
    t0: f64,
    t1: f64,
    target_cols: usize,
) -> WaveSegments {
    let mut out = WaveSegments {
        segs: Vec::new(),
        layer: ViewLayer::None,
        bucket_ms: 0,
        refine_ms: None,
        pending_ms: None,
    };
    if !t0.is_finite() || !t1.is_finite() || t1 <= t0 {
        return out;
    }
    let dur = base.duration_ms.max(0) as f64;
    // 可见窗夹到片内（片头之前 / 片尾之后没有任何音频，画出来只会凭空多一段）
    let a = t0.max(0.0).min(dur);
    let b = t1.max(0.0).min(dur);
    if b <= a {
        return out;
    }
    let span = b - a;
    if span > DENSE_MAX_VISIBLE_MS {
        // 规则 1：长可见区间 —— **基准层**（真数据，100 ms/片；不再是稀疏填充）
        out.layer = ViewLayer::Base;
        out.bucket_ms = base.bucket_ms.max(1);
        let cols = target_cols.max(1);
        let merged = push_base_segs(&mut out, base, a, b, true, cols, true);
        // 可见区间里"还没算到"的地方：画一条渐进指示线（数据到达会由 worker 唤醒重绘）
        out.pending_ms = merged.first_missing;
        return out;
    }
    // 规则 2：密集层。档位由可见时长决定（`VISIBLE_BUCKETS` = 600 的单一来源）
    let bucket_ms = tier_for_visible_ms(span);
    out.layer = ViewLayer::Dense;
    out.bucket_ms = bucket_ms;

    // 片数上界：正常路径下 `ceil(120 s / 5 s)` = 24 片，这里的护栏只防
    // "档位表被改成极粗 + 可见区间极大"那种异常输入。
    let last_tile_allowed = tile_of_ms(a as i64).saturating_add(MAX_DENSE_TILES_PER_VIEW);
    let first_tile = tile_of_ms(a as i64);
    let last_tile = tile_of_ms(b as i64).min(last_tile_allowed);

    // 逐片收集"已算好的密集段"，同时把**没覆盖到的空隙**记下来
    let mut covered_to = a;
    let mut gaps: Vec<(i64, i64)> = Vec::new();
    for tile in first_tile..=last_tile {
        let (ts, te) = tile_span_ms(tile);
        match tiles.get(&tile).filter(|t| !t.peaks.is_empty()) {
            None => gaps.push((ts, te)),
            Some(t) => {
                if (ts as f64) > covered_to + 0.5 {
                    gaps.push((covered_to.round() as i64, ts));
                }
                let w = t.bucket_ms.max(1);
                // 桶序号 → 时间：用瓦片自己的 `start_ms` + 自己的桶宽（同源，不漂）
                for b_idx in 0..t.peaks.len() {
                    let bs = t.start_ms + b_idx as i64 * w;
                    let be = bs + w;
                    if (be as f64) <= a || (bs as f64) >= b {
                        continue;
                    }
                    out.segs.push(WaveSeg {
                        start_ms: bs as f64,
                        end_ms: be as f64,
                        peak: t.peaks[b_idx],
                        detail: true,
                        bucket: b_idx,
                    });
                }
                covered_to = covered_to.max((te as f64).min(b));
            }
        }
    }
    if covered_to + 0.5 < b {
        gaps.push((covered_to.round() as i64, b.round() as i64));
    }
    // 空隙：记进 refine（界面拿去排队）+ 用**基准层**兜底（不能留空，用户看的是"有没有声音"）
    let mut pending_first: Option<i64> = None;
    for (gs, ge) in gaps {
        let gs = gs.max(a.round() as i64).min(b.round() as i64);
        let ge = ge.max(a.round() as i64).min(b.round() as i64);
        if ge <= gs {
            continue;
        }
        if pending_first.is_none() {
            pending_first = Some(gs);
        }
        push_base_segs(&mut out, base, gs as f64, ge as f64, false, 0, false);
    }
    if let Some(p) = pending_first {
        out.refine_ms = Some((p, b.round() as i64));
        out.pending_ms = Some(p);
    }
    out
}

/// [`push_base_segs`] 的附加结论：本帧基准层里**第一片还没算到**的时刻。
#[derive(Debug, Default, Clone, Copy)]
struct BasePush {
    /// 覆盖区间里第一片"没数据"的切片起点（毫秒）；`None` = 这一段全都已经有数据
    first_missing: Option<i64>,
}

/// 把**全片基准层**折成"透明地把一组段画出来"的样子（只给**已经有数据**的切片）。
///
/// * `primary`：这些段是不是本帧的主数据源（长可见区间 = true ⇒ `detail = true`；
///   短可见区间的兜底 = false ⇒ 画得淡一档，一眼看出"这段还没细算"）；
/// * `cols`：> 0 时把相邻切片**合并**到"每列一片"（min/max 的合并是无损的：
///   取 min 的 min、max 的 max），于是 1 小时整片视图画的是 ≈ 列数 个 shape，
///   而不是 36,000 个。合并**不改时间**（段的 start/end 仍是绝对时刻）。
/// * `track_missing`：是否记录"第一片没数据"（长可见区间那条路要用它画渐进线）。
fn push_base_segs(
    out: &mut WaveSegments,
    base: &BaseLayer,
    t0: f64,
    t1: f64,
    primary: bool,
    cols: usize,
    track_missing: bool,
) -> BasePush {
    let mut res = BasePush::default();
    if base.peaks.is_empty() || t1 <= t0 {
        return res;
    }
    let n = base.peaks.len();
    let first = base_slice_of_ms(t0.max(0.0).round() as i64).min(n.saturating_sub(1));
    let last = base_slice_of_ms(t1.max(0.0).round() as i64).min(n.saturating_sub(1));
    if first > last {
        return res;
    }
    // 合并宽度：让"每列最多一片"（cols 是像素宽度；不合并时 = 1）
    let group = if cols > 1 {
        (((last - first + 1) as f64) / cols as f64).ceil().max(1.0) as usize
    } else {
        1
    };
    // 只在**连续的已算切片**内部合并：合并跨过缺口就等于把"没算到的空洞"画成有数据。
    let mut i = first;
    while i <= last {
        if !base.done[i] {
            if track_missing && res.first_missing.is_none() {
                res.first_missing = Some(base.span_of(i).0);
            }
            i += 1;
            continue;
        }
        // 本段连续已算区间的末尾（不超过 last）
        let mut run_end = i;
        while run_end < last && base.done[run_end + 1] {
            run_end += 1;
        }
        let mut k = i;
        while k <= run_end {
            let g_end = (k + group - 1).min(run_end);
            let mut acc = base.peaks[k];
            for k2 in (k + 1)..=g_end {
                let p = base.peaks[k2];
                acc = Peak {
                    min: acc.min.min(p.min),
                    max: acc.max.max(p.max),
                };
            }
            let (bs, _) = base.span_of(k);
            let (_, be) = base.span_of(g_end);
            // 与密集瓦片那条路同一条口径：整段都在可见区间之外就不给（右端那片常常
            // 正好从 `t1` 开始，画出来也只会被裁掉）
            if (be as f64) <= t0 || (bs as f64) >= t1 {
                k = g_end + 1;
                continue;
            }
            out.segs.push(WaveSeg {
                start_ms: bs as f64,
                end_ms: be.max(bs + 1) as f64,
                peak: acc,
                detail: primary,
                bucket: k,
            });
            k = g_end + 1;
        }
        i = run_end + 1;
    }
    res
}

/// **密集瓦片缓存的 LRU**：键 = 片号，上限 [`TILE_CACHE_MAX`] 片（**跨档位合计**）。
///
/// 为什么用"顺序表 + 每次访问移到最后"而不是自己做链表：上限只有 64，
/// `Vec::remove` 的代价可以忽略，而实现简单到不可能有 bug。
///
/// **为什么键里没有档位**：绘制一帧只用**一个**档位（可见时长决定），
/// 而换档时 [`TileCache::retain_tier`] 会把上一档的片全丢掉 ——
/// 于是"片号"在任何时刻都不歧义，缓存里**只有一个档位**的数据。
/// 这条不变式也让"内存上界 = 64 × 最细一片 40 KB"这句话成立（跨档位不叠加）。
#[derive(Debug, Default)]
pub struct TileCache {
    map: std::collections::HashMap<usize, DetailTile>,
    order: Vec<usize>,
}

impl TileCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&mut self, tile: usize) -> Option<&DetailTile> {
        if self.map.contains_key(&tile) {
            self.touch(tile);
            self.map.get(&tile)
        } else {
            None
        }
    }

    /// 只读查询（**不算访问**）：绘制每帧都会读全部可见瓦片，
    /// 若每读一次都改 LRU 顺序，"最近使用"就退化成"绘制顺序"，缓存会失去意义。
    pub fn peek(&self, tile: usize) -> Option<&DetailTile> {
        self.map.get(&tile)
    }

    pub fn insert(&mut self, tile: DetailTile) {
        let key = tile.tile;
        self.map.insert(key, tile);
        self.touch(key);
        while self.map.len() > TILE_CACHE_MAX {
            // 淘汰最久未用的那一片（order[0]）
            let victim = self.order.remove(0);
            self.map.remove(&victim);
        }
    }

    /// **换档时清掉别的档位的片**（P18）：缓存里只留 `bucket_ms` 这一档。
    ///
    /// 为什么必须清：绘制只按"片号"取值（见类型文档），若留着上一档的同一个片号，
    /// 就会把粗档的桶当成细档画出来（横坐标按粗桶宽铺，看上去就是"块状"）。
    /// 返回清掉了几片（日志/单测用）。
    pub fn retain_tier(&mut self, bucket_ms: i64) -> usize {
        let before = self.map.len();
        self.map.retain(|_, t| t.bucket_ms == bucket_ms);
        self.order.retain(|k| self.map.contains_key(k));
        before - self.map.len()
    }

    /// 缓存里当前所有的档位（正常路径下最多 1 个；单测用它钉住这条不变式）。
    pub fn tiers(&self) -> Vec<i64> {
        let mut v: Vec<i64> = self.map.values().map(|t| t.bucket_ms).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn touch(&mut self, tile: usize) {
        if let Some(i) = self.order.iter().position(|&t| t == tile) {
            self.order.remove(i);
        }
        self.order.push(tile);
    }

    pub fn keys(&self) -> impl Iterator<Item = &usize> {
        self.map.keys()
    }

    /// 快照成 `HashMap`（绘制接口要的是只读视图；克隆 64 片 ≈ 2.6 MB 上限，
    /// 实际只有可见的那几片，几十微秒级）。
    pub fn snapshot_map(&self) -> std::collections::HashMap<usize, DetailTile> {
        self.map.clone()
    }
}

/// 从"解码出来的样本 + 它在时间轴上的起点"折出一片密集瓦片的桶。
///
/// **这就是"密集解码"的落点**：`samples` 是这一片 5 秒钟**逐包解满**的全部采样点
/// （[`compute_tile`] 里 `read_range` 解的就是整个 `[ts, te)`，没有任何"只取中心窗口"
/// 的跳跃），这里只是把它们按档位桶宽折成 min/max。它与**全片基准层**
/// （[`BaseLayer`]：`read_range_pts` + [`BASE_BUCKET_MS`] 一片）是两条**互不调用**的路径。
///
/// 桶边界用**整数下标**（`bucket × 档位采样点数`）而不是"逐段累加浮点毫秒"：
/// 累加会让第 5000 个桶漂出几个采样点，整数下标永远不会。
pub(crate) fn scan_to_tile(tier_ms: i64, tile: usize, chunk: ChunkRange, samples: &[f32]) -> DetailTile {
    let w = tier_ms.max(1);
    let (ts, te) = tile_span_ms(tile);
    let per = ((w * DECODE_RATE as i64) / 1000).max(1) as usize;
    let cap = buckets_per_tile(w);
    let mut peaks = Vec::with_capacity(cap);
    // 落在本瓦片之前的样本：整桶丢弃（它们属于上一片）
    let lead = ((ts - chunk.start_ms).max(0) * DECODE_RATE as i64 / 1000) as usize;
    let total = samples.len().saturating_sub(lead);
    let n = total.div_ceil(per);
    for b in 0..n.min(cap) {
        let (from, to) = tile_bucket_sample_range(w, tile, b, total);
        let (from, to) = (from + lead, to + lead);
        peaks.push(if from < to {
            fold_peaks(samples[from..to].iter())
        } else {
            Peak::ZERO
        });
    }
    let complete = peaks.len() >= cap || chunk_end_ms(chunk) >= te;
    DetailTile {
        tile,
        start_ms: ts,
        bucket_ms: w,
        peaks,
        complete,
        dense: true,
    }
}


// ---------------------------------------------------------------------------
// 落盘缓存（只缓存**完整的**全片基准层）
// ---------------------------------------------------------------------------

/// 缓存文件头 magic（`CapLite` 的**基准层**缓存，v6 容器）。
///
/// P22 换了容器：旧的是 `CAPL0WV1`（固定 1024 桶）。magic 不同 ⇒ 旧文件一律当未命中，
/// 不会出现"新算法读旧格式"这种事（版本号 [`ALGO_VERSION`] 是第二道保险）。
const CACHE_MAGIC: [u8; 8] = *b"CAPBASE1";

/// 缓存文件头长度（字节）：`magic[8] + algo:u32 + duration_ms:i64 + bucket_ms:i64
/// + n:u32 + done:u32 + failed:u32`。
///
/// ⚠️ 它必须与 [`encode_base`] 实际写出的字节数**严格相等**：这里多算 8 字节曾经导致
/// 每一个缓存文件都被判成"长度不对"、于是**永远命中不了**（而且表面上看不出问题，
/// 只是每次重算）。所以这个常量旁边就写着它由哪几段加起来 —— 改布局时一眼能对上。
const CACHE_HEADER_LEN: usize = 8 + 4 + 8 + 8 + 4 + 4 + 4;

/// **缓存键**：路径 + 修改时间 + 大小 + 算法版本。
///
/// 为什么要这四样：只看路径会在"录完立刻重开同一个文件名"时命中旧波形
/// （同一路径、内容已经变了）；加上修改时间与大小之后，任何"文件被重写"都能识别；
/// 算法版本则保证"改了常量就绝不会读到旧口径的数据"（见 `ALGO_VERSION`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub path: std::path::PathBuf,
    /// 修改时间（UNIX 纪元以来的纳秒；取不到时为 0）
    pub mtime_ns: u128,
    pub size: u64,
    /// 算法 / 常量版本（`ALGO_VERSION`）
    pub algo: u32,
}

impl CacheKey {
    /// 从文件系统读一次（取不到元数据时 mtime/size 记 0，仍然可用 —— 只是退化成"按路径缓存"）。
    pub fn of(path: &Path) -> Self {
        let md = std::fs::metadata(path);
        let (mtime_ns, size) = match md {
            Ok(m) => {
                let ns = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                (ns, m.len())
            }
            Err(_) => (0, 0),
        };
        Self {
            path: path.to_path_buf(),
            mtime_ns,
            size,
            algo: ALGO_VERSION,
        }
    }

    /// **磁盘文件名**（不含目录）：键的四样东西折叠成一个 64 位 hash + 版本号。
    ///
    /// 为什么用 hash 而不是"把路径编码进文件名"：路径可能很长、含非法字符、还会撞大小写；
    /// hash 既短又稳定，而"键里本来就有大小 + 修改时间"，碰撞的后果也只是重算一次。
    pub fn file_name(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64 位偏移基准
        for b in self.path.to_string_lossy().as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for b in self.size.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for b in self.mtime_ns.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("wave-base-{h:016x}-v{}.bin", self.algo)
    }
}

/// **落盘缓存目录**：一律走既有的 `Cache\`（沿用回退链），**不写用户文档目录**。
#[derive(Debug, Clone, Default)]
pub struct DiskCache {
    dir: Option<std::path::PathBuf>,
}

impl DiskCache {
    /// `dir = None`（还没解析出 Cache 目录）→ 所有读写都变成 no-op（界面照常工作）。
    pub fn new(dir: Option<std::path::PathBuf>) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    fn path_of(&self, key: &CacheKey) -> Option<std::path::PathBuf> {
        self.dir.as_ref().map(|d| d.join(key.file_name()))
    }

    /// **读**：任何一处不对（文件不存在 / magic 不对 / 版本不对 / 长度不对 / 时长不符）
    /// 都返回 `None`（= 未命中，重算），**绝不 panic**。
    ///
    /// 未命中时按原因记一行 `debug`：缓存"该命中却没命中"是最难查的一类问题，
    /// 而一行原因（`no_dir` / `unreadable` / `corrupt` / `duration_mismatch`）就够定位了。
    pub fn load(&self, key: &CacheKey, duration_ms: i64) -> Option<BaseLayer> {
        let Some(path) = self.path_of(key) else {
            tracing::debug!(reason = "no_dir", "波形基准层缓存未命中");
            return None;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    reason = "unreadable",
                    error = %e,
                    file = %path.display(),
                    "波形基准层缓存未命中"
                );
                return None;
            }
        };
        let Some(o) = decode_base(&bytes) else {
            tracing::debug!(
                reason = "corrupt",
                file = %path.display(),
                bytes = bytes.len(),
                "波形基准层缓存未命中（文件损坏/版本不符，将重算）"
            );
            return None;
        };
        if o.duration_ms != duration_ms.max(0) {
            // 时长对不上：同一个文件被重新封装过，缓存作废
            tracing::debug!(
                reason = "duration_mismatch",
                cached = o.duration_ms,
                want = duration_ms,
                "波形基准层缓存未命中"
            );
            return None;
        }
        Some(o)
    }

    /// **写**：失败（磁盘满 / 只读 / 没目录）只记一行 debug 日志，绝不影响界面。
    /// 只写**完整**的层（半成品落盘毫无意义：下次还是要重算，反而多一次读盘）。
    pub fn save(&self, key: &CacheKey, o: &BaseLayer) {
        if !o.is_complete() {
            return;
        }
        let Some(path) = self.path_of(key) else { return };
        if let Some(dir) = &self.dir {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::debug!(error = %e, "波形缓存目录建不出来（本次不落盘）");
                return;
            }
        }
        match encode_base(o) {
            Some(bytes) => {
                if let Err(e) = std::fs::write(&path, &bytes) {
                    tracing::debug!(error = %e, "波形基准层缓存写盘失败（下次重算即可）");
                }
            }
            None => tracing::debug!("波形基准层缓存编码失败，本次不落盘"),
        }
    }
}

/// **编码基准层**（纯函数，可脱文件系统单测）。
///
/// 布局（全部小端）：
/// `magic[8] | algo:u32 | duration_ms:i64 | bucket_ms:i64 | n:u32 | done:u32 | failed:u32
///  | peaks:n×(f32,f32) | done_bits:ceil(n/8)`
pub(crate) fn encode_base(o: &BaseLayer) -> Option<Vec<u8>> {
    let n = o.peaks.len();
    let bits = n.div_ceil(8);
    let mut out = Vec::with_capacity(CACHE_HEADER_LEN + n * 8 + bits);
    out.extend_from_slice(&CACHE_MAGIC);
    out.extend_from_slice(&ALGO_VERSION.to_le_bytes());
    out.extend_from_slice(&o.duration_ms.to_le_bytes());
    out.extend_from_slice(&o.bucket_ms.to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(o.done_slices as u32).to_le_bytes());
    out.extend_from_slice(&(o.failed_segments as u32).to_le_bytes());
    for p in o.peaks.iter() {
        out.extend_from_slice(&p.min.to_le_bytes());
        out.extend_from_slice(&p.max.to_le_bytes());
    }
    for i in 0..bits {
        let mut byte = 0u8;
        for b in 0..8 {
            let k = i * 8 + b;
            if k < n && o.done[k] {
                byte |= 1 << b;
            }
        }
        out.push(byte);
    }
    Some(out)
}

/// **解码基准层**：校验 magic / 版本 / 长度 / 取值范围，任何一处不对返回 `None`。
///
/// 特意逐个字段校验而不是"直接 from_le_bytes 切一遍"：损坏的缓存文件是**常态**
/// （断电、并发写、磁盘满写了一半），一个 panic 就会让"打开视频"崩掉整个程序。
pub(crate) fn decode_base(bytes: &[u8]) -> Option<BaseLayer> {
    if bytes.len() < CACHE_HEADER_LEN || bytes[..8] != CACHE_MAGIC {
        return None;
    }
    let algo = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if algo != ALGO_VERSION {
        return None;
    }
    let dur = i64::from_le_bytes(bytes[12..20].try_into().ok()?);
    let bucket_ms = i64::from_le_bytes(bytes[20..28].try_into().ok()?);
    // 切片宽必须与当前常量一致（改了 `BASE_BUCKET_MS` 而没改版本号时，这条兜底）
    if dur < 0 || bucket_ms != BASE_BUCKET_MS {
        return None;
    }
    let n = u32::from_le_bytes(bytes[28..32].try_into().ok()?) as usize;
    let done_slices = u32::from_le_bytes(bytes[32..36].try_into().ok()?) as usize;
    let failed_segments = u32::from_le_bytes(bytes[36..40].try_into().ok()?) as usize;
    // 切片个数必须与时长自洽，且不能超过护栏
    if n != base_slice_count(dur) || n > BASE_SLICES_MAX {
        return None;
    }
    let bits = n.div_ceil(8);
    let want = CACHE_HEADER_LEN + n * 8 + bits;
    if bytes.len() != want {
        return None;
    }
    let mut o = BaseLayer::empty(dur);
    let mut p = CACHE_HEADER_LEN;
    for i in 0..n {
        let min = f32::from_le_bytes(bytes[p..p + 4].try_into().ok()?);
        let max = f32::from_le_bytes(bytes[p + 4..p + 8].try_into().ok()?);
        p += 8;
        // 取值范围校验：损坏文件里可能是 NaN/inf/巨大值，画出来会算出 inf 坐标
        if !min.is_finite() || !max.is_finite() || min > max || min < -1.001 || max > 1.001 {
            return None;
        }
        o.peaks[i] = Peak { min, max };
    }
    let mut counted = 0usize;
    for i in 0..bits {
        let byte = *bytes.get(p + i)?;
        for b in 0..8 {
            let k = i * 8 + b;
            if k < n && byte & (1 << b) != 0 {
                o.done[k] = true;
                counted += 1;
            }
        }
    }
    // 位图与摘要必须一致：不一致说明文件被改过 → 当未命中（绝不"信一个自相矛盾的缓存"）
    if done_slices > n || counted != done_slices {
        return None;
    }
    o.done_slices = counted;
    o.failed_segments = failed_segments;
    o.finished = true;
    Some(o)
}

/// 一个桶的峰值：`min` / `max` 都在 `[-1, 1]`（解码后已限幅），`min <= max` 恒成立。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak {
    pub min: f32,
    pub max: f32,
}

impl Peak {
    /// 空桶（还没算到 / 那一段真的是静音）：`min = max = 0`。
    pub const ZERO: Peak = Peak { min: 0.0, max: 0.0 };

    /// 从这个值开始一个桶。
    ///
    /// **`min` 与 `max` 都从一开始的那个样本起算**（而不是从 0 起算）：
    /// 否则"全为正的一段"会得到 `min = 0`（假的负半边），波形会凭空多出下半截。
    fn start(x: f32) -> Self {
        Peak { min: x, max: x }
    }

    /// 把采样点并进峰值（`NaN` 忽略：既不该成为峰值，也不该把桶污染成 NaN）。
    fn merge(&mut self, x: f32) {
        if x.is_nan() {
            return;
        }
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
    }

    /// `min` 里存的就是 0 吗（"这个桶完全没有样本"）。用来区分"静音"与"还没算"。
    pub fn is_zero(&self) -> bool {
        self.min == 0.0 && self.max == 0.0
    }
}

/// 把一个采样切片折成一个桶的峰值；空切片（或全是 `NaN`）→ [`Peak::ZERO`]。
fn fold_peaks<'a>(samples: impl IntoIterator<Item = &'a f32>) -> Peak {
    let mut it = samples.into_iter().filter(|x| !x.is_nan());
    match it.next() {
        None => Peak::ZERO,
        Some(&first) => {
            let mut p = Peak::start(first);
            for &x in it {
                p.merge(x);
            }
            p
        }
    }
}

// ---------------------------------------------------------------------------
// FFmpeg 解码（唯一的 unsafe 部分）
// ---------------------------------------------------------------------------

/// 输入上下文守卫（`avformat_open_input` ↔ `avformat_close_input`）
struct Input(*mut ff::AVFormatContext);
impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: 指针要么为空，要么由 avformat_open_input 成功返回；关闭后置空。
        unsafe {
            if !self.0.is_null() {
                ff::avformat_close_input(&mut self.0);
                self.0 = ptr::null_mut();
            }
        }
    }
}

/// packet 守卫
struct Packet(*mut ff::AVPacket);
impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: 由 av_packet_alloc 分配；av_packet_free 内部会 unref。
        unsafe {
            if !self.0.is_null() {
                ff::av_packet_free(&mut self.0);
            }
        }
    }
}

/// 解码帧守卫
struct Frame(*mut ff::AVFrame);
impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: 由 av_frame_alloc 分配。
        unsafe {
            if !self.0.is_null() {
                ff::av_frame_free(&mut self.0);
            }
        }
    }
}

/// 解码器上下文守卫
struct Codec(*mut ff::AVCodecContext);
impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: 由 avcodec_alloc_context3 分配。
        unsafe {
            if !self.0.is_null() {
                ff::avcodec_free_context(&mut self.0);
            }
        }
    }
}

/// 重采样器守卫（`swr_alloc_set_opts2` ↔ `swr_free`）
struct Swr(*mut ff::SwrContext);
impl Drop for Swr {
    fn drop(&mut self) {
        // SAFETY: 由 swr_alloc_set_opts2 分配（失败时为空，不会进这里）。
        unsafe {
            if !self.0.is_null() {
                ff::swr_free(&mut self.0);
            }
        }
    }
}

/// **单声道输出声道布局**（`AV_CH_LAYOUT_MONO`）的 RAII 包装。
///
/// 为什么要守卫：`av_channel_layout_from_mask` 在 `order == CUSTOM` 时会**分配**
/// 一张声道映射表，必须 `av_channel_layout_uninit` 还回去。标准布局（MONO）其实不分配，
/// 但"按最坏情况写"才不会在将来换布局时漏掉。
struct MonoLayout(ff::AVChannelLayout);
impl Drop for MonoLayout {
    fn drop(&mut self) {
        // SAFETY: 由 av_channel_layout_from_mask 初始化；uninit 可重复调用。
        unsafe { ff::av_channel_layout_uninit(&mut self.0) }
    }
}

fn cpath(p: &Path) -> Result<CString, AppError> {
    CString::new(p.to_string_lossy().as_bytes())
        .map_err(|_| AppError::Audio(format!("路径含 NUL 字节：{}", p.display())))
}

// P49：`av_err` 用 `crate::mux` 那一份（全仓唯一，见文件顶部 import），
// 原先这里还有一份逐字相同的拷贝。

// `AVERROR_EOF` / `AVERROR_EAGAIN` 是宏，bindgen 不导出（与 `edit::mod` 里同一个值）
const AVERROR_EOF: i32 = -541_478_725;
const AVERROR_EAGAIN: i32 = -11;
/// 微秒基准（`AV_TIME_BASE` 的展开值：`av_seek_frame` 传流索引 -1 时用它做单位）
const AV_TIME_BASE: i64 = 1_000_000;

/// **一次有界解码的结论**：是解到区间终点自然结束，还是被取消 / 中途出错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// 解到了请求区间的终点（`end_ms`）—— 正常结束，**不多解一个包**
    ReachedEnd,
    /// 文件比请求区间短（读到 EOF）
    Eof,
    /// `emit` 返回 false（界面要取消：换文件 / 关页 / 关开关）
    Cancelled,
    /// 解码/重采样中途出错（已解出的部分仍然返回）
    Failed,
}

/// **可复用的有界解码器**：把 `[start_ms, end_ms)` 的音频解成 48 kHz 单声道 `f32`。
///
/// 为什么要有"可复用"这一层（**这是"打开就能出图"的关键**）：
/// 密集瓦片要对同一片素材反复解"5 秒一段"。如果每次都"打开文件 + `avformat_find_stream_info` +
/// 建解码器"，那每一次请求的固定开销就压倒了真正的解码量 ——
/// 实测第一版就是这么慢的（5 分钟片 15.9 秒，与"解 256 秒音频"应有的耗时差一个量级）。
/// 所以这里把"打开 / 探测 / 建解码器"和"解某一段"分开：[`RangeDecoder::new`] 只做一次，
/// 之后同一个实例反复 [`RangeDecoder::read_range`]，靠 `seek` 在段之间跳。
/// （基准层那条路是每段**一个** decoder：段之间隔着 5 分钟，复用的收益本来就抵不上
/// "段与段之间必须彻底复位"的复杂度，见 [`base_worker_loop`]。）
///
/// 关键性质（都是"成本与片长无关"的前提）：
/// 1. **有界**：解到 `end_ms` 就 `break`，不因为"文件还有很长的音频"而继续解；
/// 2. **可取消**：`emit` 返回 false 立刻停；单次调用的解码量有上界，所以取消是毫秒级；
/// 3. **起点精确**：`start_ms > 0` 时先 `av_seek_frame(BACKWARD)` 到不晚于它的关键包，
///    再把落在 `start_ms` 之前的样本**丢掉**（`skip`），回调拿到的时间戳因此是真实位置；
/// 4. **样本计数是实例状态**（不是每次调用从 0 开始）：`produced` 记的是"从文件开头
///    一共交出去多少毫秒"，所以 `emit` 拿到的 `at` 在 seek 之后依然正确。
///
/// `emit(chunk, at)`：`at` = 这段样本在时间轴上的起点（毫秒，相对文件开头）。
pub struct RangeDecoder {
    ictx: Input,
    dec: Codec,
    swr: Swr,
    pkt: Packet,
    frame: Frame,
    /// 音频流索引 / 编码名 / 源采样率 / 源声道数（给调用方与日志用）
    aidx: i32,
    codec_name: String,
    src_rate: i32,
    src_channels: i32,
    /// 每帧输出缓冲上限（`swr_convert` 用）
    max_out: usize,
    out_buf: Vec<f32>,
    /// 已经交给 `emit` 的样本数（实例状态，跨调用累积）
    produced: u64,
    /// 路径（只为日志）
    path: std::path::PathBuf,
}

impl Drop for RangeDecoder {
    fn drop(&mut self) {
        // SAFETY: `swr` 是裸指针守卫，这里只是把"显式释放的顺序"写清楚：
        // 先解引用 `swr` 的余量（`swr_close`），再让各守卫按声明顺序释放。
        // 字段的 Drop 顺序是声明顺序（ictx → dec → swr → pkt → frame），
        // 也就是"先关输入、再释放解码器、最后释放重采样器"，与手动顺序一致。
        unsafe {
            if !self.swr.0.is_null() {
                ff::swr_close(self.swr.0);
            }
        }
    }
}

impl RangeDecoder {
    /// 打开文件、探测流、建好解码器与重采样器（**每个文件只做一次**）。
    pub fn new(path: &Path) -> Result<Self, AppError> {
        let c_path = cpath(path)?;
        // SAFETY: 本函数内所有 FFI 指针都由本函数分配、由字段上的守卫释放；
        // `c_path` 的生命周期覆盖整个 unsafe 块，FFmpeg 不会长期保存该指针。
        unsafe {
            let mut raw: *mut ff::AVFormatContext = ptr::null_mut();
            let ret =
                ff::avformat_open_input(&mut raw, c_path.as_ptr(), ptr::null(), ptr::null_mut());
            if ret < 0 || raw.is_null() {
                return Err(AppError::Audio(format!(
                    "打开 {} 失败：{}",
                    path.display(),
                    av_err(ret)
                )));
            }
            let ictx = Input(raw);
            crate::edit::limit_input_probe(ictx.0);
            let ret = ff::avformat_find_stream_info(ictx.0, ptr::null_mut());
            if ret < 0 {
                return Err(AppError::Audio(format!(
                    "读取 {} 的流信息失败：{}",
                    path.display(),
                    av_err(ret)
                )));
            }
            crate::mux::validate_input_streams(ictx.0)
                .map_err(|e| AppError::Audio(e.to_string()))?;

            // 找第一条音频流
            let mut aidx = -1i32;
            for i in 0..(*ictx.0).nb_streams as usize {
                let st = *(*ictx.0).streams.add(i);
                if (*(*st).codecpar).codec_type == ff::AVMediaType::AVMEDIA_TYPE_AUDIO {
                    aidx = (*st).index;
                    break;
                }
            }
            if aidx < 0 {
                return Err(AppError::Audio(format!(
                    "{} 里没有音频流（波形无从计算）",
                    path.display()
                )));
            }
            let astream = *(*ictx.0).streams.add(aidx as usize);
            let par = (*astream).codecpar;
            let codec_id = (*par).codec_id;
            let codec_name = {
                let c = ff::avcodec_get_name(codec_id);
                if c.is_null() {
                    "?".to_string()
                } else {
                    CStr::from_ptr(c).to_string_lossy().into_owned()
                }
            };
            let src_rate = (*par).sample_rate.max(0);
            let src_channels = (*par).ch_layout.nb_channels.max(0);

            // **降级点 1**：这个编码的解码器没编进来（例如最小构建只有 aac/h264）。
            // 这里就把编码名写进错误消息，调用方可以直接记一行"未编入 X 解码器"。
            let codec = ff::avcodec_find_decoder(codec_id);
            if codec.is_null() {
                return Err(AppError::Audio(format!(
                    "本程序没有内置 {codec_name} 解码器（{} 的音轨是 {codec_name}），无法计算波形",
                    path.display()
                )));
            }
            let dec = Codec(ff::avcodec_alloc_context3(codec));
            if dec.0.is_null() {
                return Err(AppError::Audio("avcodec_alloc_context3 失败".into()));
            }
            let ret = ff::avcodec_parameters_to_context(dec.0, par);
            if ret < 0 {
                return Err(AppError::Audio(format!("复制解码参数失败：{}", av_err(ret))));
            }
            (*dec.0).pkt_timebase = (*astream).time_base;
            let ret = ff::avcodec_open2(dec.0, codec, ptr::null_mut());
            if ret < 0 {
                return Err(AppError::Audio(format!(
                    "打开 {codec_name} 解码器失败：{}",
                    av_err(ret)
                )));
            }

            // ---- 重采样：源格式 → 48 kHz 单声道 f32 ----
            let mut mono = MonoLayout(ff::AVChannelLayout {
                order: ff::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC,
                nb_channels: 0,
                u: std::mem::zeroed(),
                opaque: ptr::null_mut(),
            });
            let ret = ff::av_channel_layout_from_mask(&mut mono.0, ff::AV_CH_LAYOUT_MONO);
            if ret < 0 {
                return Err(AppError::Audio(format!(
                    "构造单声道布局失败：{}",
                    av_err(ret)
                )));
            }
            let in_layout = (*dec.0).ch_layout;
            let in_fmt = (*dec.0).sample_fmt;
            let in_rate = if (*dec.0).sample_rate > 0 {
                (*dec.0).sample_rate
            } else {
                src_rate.max(1)
            };
            let mut swr_raw: *mut ff::SwrContext = ptr::null_mut();
            let ret = ff::swr_alloc_set_opts2(
                &mut swr_raw,
                &mono.0,
                ff::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                DECODE_RATE,
                &in_layout,
                in_fmt,
                in_rate,
                0,
                ptr::null_mut(),
            );
            if ret < 0 || swr_raw.is_null() {
                return Err(AppError::Audio(format!(
                    "创建重采样器失败：{}",
                    av_err(ret)
                )));
            }
            let swr = Swr(swr_raw);
            let ret = ff::swr_init(swr.0);
            if ret < 0 {
                return Err(AppError::Audio(format!(
                    "初始化重采样器失败：{}",
                    av_err(ret)
                )));
            }

            let pkt = Packet(ff::av_packet_alloc());
            if pkt.0.is_null() {
                return Err(AppError::Audio("av_packet_alloc 失败".into()));
            }
            let frame = Frame(ff::av_frame_alloc());
            if frame.0.is_null() {
                return Err(AppError::Audio("av_frame_alloc 失败".into()));
            }
            // 每帧最多能产出多少输出样本：按"源样本数 × 48000/源采样率"再留一倍余量
            let max_out = (in_rate as usize).max(1024) * 2 + 4096;
            Ok(Self {
                ictx,
                dec,
                swr,
                pkt,
                frame,
                aidx,
                codec_name,
                src_rate,
                src_channels,
                max_out,
                out_buf: vec![0.0; max_out],
                produced: 0,
                path: path.to_path_buf(),
            })
        }
    }

    /// 源采样率 / 源声道数（给调用方记录素材规格用）。
    pub fn source_format(&self) -> (i32, i32) {
        (self.src_rate, self.src_channels)
    }

    /// 音轨编码名（"aac" / "mp3"…）。
    pub fn codec_name(&self) -> &str {
        &self.codec_name
    }

    /// **解一段**：`[start_ms, end_ms)`，每解出一段就调用 `emit`（返回 false 即刻取消）。
    ///
    /// 和 `decode_range` 的区别只有一个：**不重新打开文件**。所以它可以被
    /// 密集瓦片那条路（"一片 5 秒、一片 5 秒"地反复解）反复调用。
    pub fn read_range(
        &mut self,
        start_ms: i64,
        end_ms: i64,
        mut emit: impl FnMut(&[f32], ChunkRange) -> bool,
    ) -> EndReason {
        let start_ms = start_ms.max(0);
        let end_ms = end_ms.max(start_ms + 1);
        // SAFETY: 所有指针都是 `new` 里建好、由字段守卫持有的有效指针；
        // `self` 的可变借用保证不会有第二个调用同时动它们。
        unsafe {
            // ---- 定位 ----
            //
            // 为什么不用 `avformat_seek_file` 的精确模式：文件级的精确 seek 要求容器有索引
            // （分片 mp4 常常没有），失败时只能从头解 —— 那就又变成"成本与片长有关"了。
            // `AVSEEK_FLAG_BACKWARD` + "丢掉 start 之前的样本"是**任何容器都成立**的做法：
            // 落点不晚于目标，最多多解一个关键包（音频包通常 21 ms）。
            if start_ms > 0 {
                let ts_us = start_ms * (AV_TIME_BASE / 1000);
                let ret = ff::av_seek_frame(self.ictx.0, -1, ts_us, ff::AVSEEK_FLAG_BACKWARD);
                if ret < 0 {
                    tracing::warn!(
                        error = %av_err(ret),
                        "波形 seek 失败，将从当前位置继续解（只影响这一段的耗时）"
                    );
                } else {
                    // 关键：seek 之后必须把解码器内部的参考帧/重采样余量清掉，
                    // 否则解出来的第一帧是"seek 之前那一段"的残留，波形会整体错位。
                    ff::avcodec_flush_buffers(self.dec.0);
                    ff::swr_close(self.swr.0);
                    let ret = ff::swr_init(self.swr.0);
                    if ret < 0 {
                        tracing::warn!(error = %av_err(ret), "seek 后重置重采样器失败");
                        return EndReason::Failed;
                    }
                }
                // **实例状态**：`produced` 记的是"从文件开头交出去多少毫秒"，
                // 与 `start_ms` 对齐之后，`emit` 拿到的 `at` 永远是对的。
                self.produced = (start_ms as u64) * DECODE_RATE as u64 / 1000;
            }

            let out_ptrs: [*mut u8; 1] = [self.out_buf.as_mut_ptr().cast::<u8>()];
            let mut saw_packet = false;
            // 结论：初值就是"读到 EOF 自然结束"，循环里只有"取消 / 出错 / 解到区间终点"
            // 才会改写它。取消路径必然覆盖初值，所以 `unused_assignments` 会盯上这里 ——
            // 用 `#[allow]` 显式写明意图，而不是为了消警告把它改成更难读的写法。
            #[allow(unused_assignments)]
            let mut reason = EndReason::Eof;

            'outer: loop {
                let ret = ff::av_read_frame(self.ictx.0, self.pkt.0);
                if ret < 0 {
                    if ret != AVERROR_EOF {
                        tracing::warn!(
                            error = %av_err(ret),
                            "读音频包结束（非 EOF），已解出的部分仍然可用"
                        );
                    }
                    reason = EndReason::Eof;
                    break;
                }
                if (*self.pkt.0).stream_index != self.aidx {
                    ff::av_packet_unref(self.pkt.0);
                    continue;
                }
                saw_packet = true;
                let send = ff::avcodec_send_packet(self.dec.0, self.pkt.0);
                ff::av_packet_unref(self.pkt.0);
                if send < 0 && send != AVERROR_EOF && send != AVERROR_EAGAIN {
                    tracing::trace!(error = %av_err(send), "音频 send_packet 返回非致命错误");
                }
                loop {
                    let recv = ff::avcodec_receive_frame(self.dec.0, self.frame.0);
                    if recv < 0 {
                        break;
                    }
                    let got = ff::swr_convert(
                        self.swr.0,
                        out_ptrs.as_ptr(),
                        self.max_out as i32,
                        (*self.frame.0).data.as_ptr() as *const *const u8,
                        (*self.frame.0).nb_samples,
                    );
                    if got < 0 {
                        // 重采样失败：如实降级（保留已经解出的部分），不再继续解
                        tracing::warn!(error = %av_err(got), "重采样失败，波形只保留已算出的部分");
                        ff::av_frame_unref(self.frame.0);
                        ff::avcodec_send_packet(self.dec.0, ptr::null());
                        reason = EndReason::Failed;
                        break 'outer;
                    }
                    let n = got.max(0) as u64;
                    if n > 0 {
                        // 只交"落在请求区间里"的那部分输出。
                        //
                        // ⚠️ **单位必须是采样点，不能是毫秒**（这里原来是一个真 bug）：
                        // `start_ms - base_ms` 是**毫秒差**，却直接当成采样点下标去夹 `n`
                        // —— 于是每帧只交出 `end_ms − base_ms` 个**采样点**，
                        // 而正确值是 `(end_ms − base_ms) × 48000 / 1000`（≈ 48 倍）。
                        // 症状：1 秒的有界解码只交出 23954 个采样点（应为 48000）、
                        // 波形被"每帧取一个很窄的窗口再拼起来"弄成一块缩水且错位的波形
                        //（峰值 0.084 而真实是 0.7）。
                        // 现在统一换成采样点起止（`start_s` / `end_s`），全程整数、
                        // 不引入取整误差：`48 个采样点 = 1 ms`。
                        let base_ms = (self.produced * 1000 / DECODE_RATE as u64) as i64;
                        let start_s = start_ms.max(0) as u64 * DECODE_RATE as u64 / 1000;
                        let end_s = end_ms.max(0) as u64 * DECODE_RATE as u64 / 1000;
                        let from = start_s.saturating_sub(self.produced).min(n) as usize;
                        let upto = end_s.saturating_sub(self.produced).min(n) as usize;
                        self.produced += n;
                        // **有界判据同样用采样点**：本帧的最后一个采样点是否已经越过区间终点
                        let covered = self.produced >= end_s;
                        if from < upto {
                            let at = ChunkRange {
                                start_ms: base_ms + (from as i64) * 1000 / DECODE_RATE as i64,
                                len: upto - from,
                            };
                            let chunk = &self.out_buf[from..upto];
                            if !emit(chunk, at) {
                                ff::av_frame_unref(self.frame.0);
                                reason = EndReason::Cancelled;
                                break 'outer;
                            }
                        }
                        // **有界**：这一段已经覆盖到区间终点 → 立刻停手，
                        // 绝不"顺手把整个文件解完"。
                        if covered {
                            ff::av_frame_unref(self.frame.0);
                            reason = EndReason::ReachedEnd;
                            break 'outer;
                        }
                    }
                    ff::av_frame_unref(self.frame.0);
                }
            }

            if reason == EndReason::Cancelled {
                tracing::debug!(path = %self.path.display(), "音频解码被取消（换文件/关页/关开关）");
            }
            if !saw_packet {
                tracing::warn!(
                    path = %self.path.display(),
                    codec = %self.codec_name,
                    start_ms,
                    "这一区间里一个音频包都没有（空音轨或超出流长度）"
                );
            }
            reason
        }
    }

    /// **按包 PTS 绝对定位**的有界解码（**基准层专用**，P22）。
    ///
    /// 与 [`RangeDecoder::read_range`] 的区别只有一条，但它是"并行分段不出断层"的全部原因：
    /// **每交出一帧，它的时间位置由这一帧自己的 PTS 决定**（而不是"从请求起点开始数采样点"）。
    ///
    /// * `keep_from_ms..keep_to_ms`：要保留的绝对时间窗（段边界）；
    /// * `seek_ms`：`av_seek_frame` 的目标（调用方传 `keep_from_ms - `[`AAC_PRIMING_MS`]，
    ///   保证边界前的那个包也被解到）；
    /// * `warmup`：seek 之后先**丢掉**几个包（`WARMUP_PACKETS`）——
    ///   AAC 的帧间重叠（MDCT）只依赖前一帧，丢掉 2 帧之后解码器状态与"顺序解码"完全一致；
    ///   `seek_ms <= 0`（从片头开始）时**不丢弃**：那时没有"历史状态不对"这回事，
    ///   丢反而会把片头真实的 44 ms 音频丢掉。
    ///
    /// 于是"段边界 + priming 补偿"是**精确**的：同一份采样点，无论在顺序解码里还是在
    /// 某一段的并行解码里，都会落到同一个绝对时刻（单测 `p22_*` 钉住这条不变式）。
    ///
    /// `emit(samples, at_sample)`：`at_sample` = 这段样本**第一个采样点在文件里的绝对下标**
    /// （0 号采样点 = 时刻 0；48 kHz 下 48 个采样点 = 1 ms）。
    /// 用采样点下标而不是毫秒，是为了**全程整数**、不在段边界上引入取整误差。
    pub fn read_range_pts(
        &mut self,
        keep_from_ms: i64,
        keep_to_ms: i64,
        seek_ms: i64,
        warmup: usize,
        mut emit: impl FnMut(&[f32], i64) -> bool,
    ) -> EndReason {
        let keep_from_ms = keep_from_ms.max(0);
        let keep_to_ms = keep_to_ms.max(keep_from_ms + 1);
        let seek_ms = seek_ms.max(0);
        // 音频流的时基（换算包 PTS ↔ 采样点）。取不到时退化成微秒基准。
        let (tb_num, tb_den) = unsafe {
            let st = *(*self.ictx.0).streams.add(self.aidx as usize);
            let tb = (*st).time_base;
            if tb.num > 0 && tb.den > 0 {
                (tb.num as i64, tb.den as i64)
            } else {
                (1i64, AV_TIME_BASE)
            }
        };
        let rate = DECODE_RATE as i128;
        let ms_to_pts = |ms: i64| -> i64 {
            // pts = ms / 1000 * den / num（用 i128 防溢出：长片的 pts 可以很大）
            let v = (ms as i128) * (tb_den as i128) / (1000i128 * tb_num as i128);
            v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
        };
        // 包 PTS → **绝对采样点下标**：`pts × num / den × 48000`
        let pts_to_sample = |pts: i64| -> i64 {
            let v = (pts as i128) * (tb_num as i128) * rate / (tb_den as i128);
            v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
        };
        // SAFETY: 所有指针都是 `new` 里建好、由字段守卫持有的有效指针；
        // `self` 的可变借用保证不会有第二个调用同时动它们。
        unsafe {
            if seek_ms > 0 {
                // **按音频流 seek**（P22 补充 2 的口径）：用音频流自己的时基，
                // 比 `stream = -1` + 微秒基准更准（不会有"落到默认流"的歧义）。
                let target = ms_to_pts(seek_ms);
                let ret =
                    ff::av_seek_frame(self.ictx.0, self.aidx, target, ff::AVSEEK_FLAG_BACKWARD);
                if ret < 0 {
                    tracing::warn!(
                        error = %av_err(ret),
                        seek_ms,
                        "基准层分段 seek 失败（这一段按失败处理，不改用兜底扫全片）"
                    );
                    return EndReason::Failed;
                }
                // seek 之后必须清掉解码器/重采样器的残留状态，否则头几帧是"上一段"的内容
                ff::avcodec_flush_buffers(self.dec.0);
                ff::swr_close(self.swr.0);
                if ff::swr_init(self.swr.0) < 0 {
                    tracing::warn!("基准层分段 seek 后重置重采样器失败");
                    return EndReason::Failed;
                }
            } else {
                // 从片头开始：清一次，保证与"复用实例"的前一次调用无关
                ff::avcodec_flush_buffers(self.dec.0);
                ff::swr_close(self.swr.0);
                if ff::swr_init(self.swr.0) < 0 {
                    return EndReason::Failed;
                }
            }

            let out_ptrs: [*mut u8; 1] = [self.out_buf.as_mut_ptr().cast::<u8>()];
            let mut warm = if seek_ms > 0 { warmup } else { 0 };
            // 容器没给 PTS 时的兜底位置（采样点）：从 seek 起点往后数
            let mut fallback_s = keep_from_ms * DECODE_RATE as i64 / 1000;
            let keep_from_s = keep_from_ms * DECODE_RATE as i64 / 1000;
            let keep_to_s = keep_to_ms * DECODE_RATE as i64 / 1000;
            let mut saw_packet = false;
            #[allow(unused_assignments)]
            let mut reason = EndReason::Eof;

            'outer: loop {
                let ret = ff::av_read_frame(self.ictx.0, self.pkt.0);
                if ret < 0 {
                    reason = EndReason::Eof;
                    break;
                }
                if (*self.pkt.0).stream_index != self.aidx {
                    ff::av_packet_unref(self.pkt.0);
                    continue;
                }
                saw_packet = true;
                let send = ff::avcodec_send_packet(self.dec.0, self.pkt.0);
                ff::av_packet_unref(self.pkt.0);
                if send < 0 && send != AVERROR_EOF && send != AVERROR_EAGAIN {
                    tracing::trace!(error = %av_err(send), "基准层 send_packet 返回非致命错误");
                }
                loop {
                    let recv = ff::avcodec_receive_frame(self.dec.0, self.frame.0);
                    if recv < 0 {
                        break;
                    }
                    let n = (*self.frame.0).nb_samples.max(0) as usize;
                    let bpts = (*self.frame.0).best_effort_timestamp;
                    let at_s = if bpts != ff::AV_NOPTS_VALUE {
                        pts_to_sample(bpts)
                    } else {
                        fallback_s
                    };
                    if warm > 0 {
                        // **预热帧**：只推进兜底位置，不交出去（AAC 重叠历史在这里收敛）
                        warm -= 1;
                        fallback_s = at_s + n as i64;
                        ff::av_frame_unref(self.frame.0);
                        continue;
                    }
                    let got = ff::swr_convert(
                        self.swr.0,
                        out_ptrs.as_ptr(),
                        self.max_out as i32,
                        (*self.frame.0).data.as_ptr() as *const *const u8,
                        (*self.frame.0).nb_samples,
                    );
                    if got < 0 {
                        tracing::warn!(error = %av_err(got), "基准层重采样失败，这一段按失败处理");
                        ff::av_frame_unref(self.frame.0);
                        ff::avcodec_send_packet(self.dec.0, ptr::null());
                        reason = EndReason::Failed;
                        break 'outer;
                    }
                    let m = got.max(0) as usize;
                    fallback_s = at_s + m as i64;
                    if m > 0 {
                        // 只交**落在 [keep_from, keep_to) 里**的那部分（绝对定位，整数采样点）
                        let from = (keep_from_s - at_s).max(0) as usize;
                        let upto = (keep_to_s - at_s).max(0) as usize;
                        let from = from.min(m);
                        let upto = upto.min(m);
                        if from < upto && !emit(&self.out_buf[from..upto], at_s + from as i64) {
                            ff::av_frame_unref(self.frame.0);
                            reason = EndReason::Cancelled;
                            break 'outer;
                        }
                        // **有界**：这一帧的起点已经越过区间终点 → 立刻停手
                        if at_s >= keep_to_s {
                            ff::av_frame_unref(self.frame.0);
                            reason = EndReason::ReachedEnd;
                            break 'outer;
                        }
                    }
                    ff::av_frame_unref(self.frame.0);
                }
            }
            if !saw_packet {
                tracing::warn!(
                    path = %self.path.display(),
                    codec = %self.codec_name,
                    keep_from_ms,
                    "基准层这一区间里一个音频包都没有"
                );
            }
            reason
        }
    }
}

/// **一次性的有界解码**（打开 → 解一段 → 关闭）：给"只要一段"的调用方用
/// （密集瓦片、单测、以及需要整段 PCM 的上层）。
///
/// 与 [`RangeDecoder::new`] + [`RangeDecoder::read_range`] 是同一套实现，
/// 只是把"开一次只用一次"的短路径包成一句。**要解很多段时请复用 `RangeDecoder`**。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn decode_range(
    path: &Path,
    start_ms: i64,
    end_ms: i64,
    mut emit: impl FnMut(&[f32], ChunkRange) -> bool,
) -> Result<(EndReason, String, i32, i32), AppError> {
    let mut dec = RangeDecoder::new(path)?;
    let reason = dec.read_range(start_ms, end_ms, &mut emit);
    let codec = dec.codec_name().to_string();
    let (rate, channels) = dec.source_format();
    Ok((reason, codec, rate, channels))
}

/// **基准层的共享计划**（4 个 worker 与界面线程之间**唯一**的共享状态）。
///
/// 为什么是这个形状：
/// * **层本体在 `Mutex` 里**，锁的持有时间 = "写一段切片"（几百微秒）；
///   界面侧用 `snapshot()` 读一份 `Arc<BaseLayer>` 快照（**克隆一个 `Arc`，不是 288 KB**），
///   拿不到锁就用上一帧那份 —— **UI 永远不会等后台**；
/// * **可见区间在 `AtomicI64` 里**：界面每帧写一次（两个 relaxed store），
///   worker 领任务时读一次，用来挑"离可见区间最近的段"（**可见区间最优先**）。
///   它故意不走那把 `Mutex`：那条路径要能在"worker 正写切片"时照常更新，
///   否则 UI 会为了写一个区间去等锁。
#[derive(Debug)]
pub struct BasePlan {
    /// 当前这一层（`Arc` 快照：worker 写完一段就换上一份新的）
    layer: std::sync::Mutex<Arc<BaseLayer>>,
    /// 每一段的状态：0 = 待算，1 = 有人在算，2 = 算完，3 = 失败
    state: std::sync::Mutex<Vec<u8>>,
    /// 段长（毫秒）：生产恒为 [`SEGMENT_MS`]；单测会缩小它来覆盖"多段 + 段边界"
    seg_ms: i64,
    /// worker **打不开素材**时记下的原因（界面据此如实降级 + 给一句提示）。
    ///
    /// 为什么放在计划里而不是 `Result`：worker 是 4 条独立线程，谁先发现"打不开"就把原因
    /// 写在这儿，界面每帧 `error()` 读一次 —— 于是"没有音轨 / 没编入解码器"这类降级
    /// 仍然是**有提示的**（与旧 L0 那条路的行为一致），绝不会变成"什么都没有、也没有提示"。
    error: std::sync::Mutex<Option<String>>,
    /// 可见区间（毫秒）：`[start, end)`；界面每帧写，worker 领任务时读
    visible_start: std::sync::atomic::AtomicI64,
    visible_end: std::sync::atomic::AtomicI64,
}

impl BasePlan {
    /// 按时长建一个空计划（**不解码**），段长 = `SEGMENT_MS`。
    pub fn new(duration_ms: i64) -> Self {
        Self::with_segment_ms(duration_ms, SEGMENT_MS)
    }

    /// 按时长与**显式段长**建一个空计划（单测用；生产走 [`BasePlan::new`]）。
    pub fn with_segment_ms(duration_ms: i64, seg_ms: i64) -> Self {
        let seg_ms = seg_ms.max(BASE_BUCKET_MS);
        let n = segment_count_of(duration_ms, seg_ms);
        Self {
            layer: std::sync::Mutex::new(Arc::new(BaseLayer::empty(duration_ms))),
            state: std::sync::Mutex::new(vec![0u8; n]),
            seg_ms,
            error: std::sync::Mutex::new(None),
            visible_start: std::sync::atomic::AtomicI64::new(0),
            visible_end: std::sync::atomic::AtomicI64::new(duration_ms.max(0)),
        }
    }

    /// 段长（毫秒）。
    pub fn segment_ms(&self) -> i64 {
        self.seg_ms
    }

    /// **worker 打不开素材**的原因（`None` = 正常）。
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|g| g.clone())
    }

    /// 记下"打不开素材"的原因（第一个写进来的生效）。
    pub fn set_error(&self, e: String) {
        if let Ok(mut g) = self.error.lock() {
            if g.is_none() {
                *g = Some(e);
            }
        }
    }

    /// 把**所有还没算完的段**标成失败（"打不开素材"时用：不该让界面永远停在"算到一半"）。
    ///
    /// ⚠️ 两把锁**顺序获取、不嵌套**：`take_next` 是"先 state 再 layer"，
    /// 所以这里也必须先 state 后 layer（中途不重叠持锁），避免锁序成环。
    pub fn fail_all_pending(&self) {
        let pending = match self.state.lock() {
            Ok(mut st) => {
                let n = st.iter().filter(|s| **s < 2).count();
                for s in st.iter_mut() {
                    if *s < 2 {
                        *s = 3;
                    }
                }
                n
            }
            Err(_) => 0,
        };
        if pending > 0 {
            if let Ok(mut g) = self.layer.lock() {
                let mut next = (**g).clone();
                next.failed_segments = next.failed_segments.saturating_add(pending);
                *g = Arc::new(next);
            }
        }
    }

    /// 层的当前快照（`Arc` 克隆，**不算访问、不阻塞**）。
    pub fn snapshot(&self) -> Option<Arc<BaseLayer>> {
        self.layer.lock().ok().map(|g| g.clone())
    }

    /// **写可见区间**（界面每帧调用；两个 relaxed store，代价可忽略）。
    pub fn set_visible(&self, a: i64, b: i64) {
        use std::sync::atomic::Ordering;
        let dur = self.duration_ms();
        let a = a.clamp(0, dur.max(0));
        let b = b.clamp(a, dur.max(0));
        self.visible_start.store(a, Ordering::Relaxed);
        self.visible_end.store(b, Ordering::Relaxed);
    }

    /// 时长（从层里读；层永远在，所以这一步不会失败）。
    pub fn duration_ms(&self) -> i64 {
        self.layer.lock().map(|g| g.duration_ms).unwrap_or(0)
    }

    /// 段数。
    pub fn segment_count(&self) -> usize {
        self.state.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// 所有段都尝试过了（算完或失败）。
    pub fn all_attempted(&self) -> bool {
        self.state
            .lock()
            .map(|g| g.iter().all(|s| *s >= 2))
            .unwrap_or(true)
    }

    /// 每段的状态快照（单测用：`2` = 算完、`3` = 失败）。
    pub fn states(&self) -> Vec<u8> {
        self.state.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 领一段：返回"离可见区间最近"的待算段号，并把它标成"在算"。
    ///
    /// **优先级规则**（这就是"可见区间最优先"）：
    /// 距离 = 段区间与 `[visible_start, visible_end)` 的距离（重叠 = 0），
    /// 同距离时取**段号小的**（从片头往片尾走，观感更自然，也让结果可复现）。
    /// 于是：
    /// * 打开时可见区间 = 视口那一段 ⇒ 那几段先算（**首屏立刻有数据**）；
    /// * 平移/缩放换了区间 ⇒ 新的可见段距离变成 0，下一次领任务就插到最前面；
    /// * 在算的那一段**不打断**（8 min 音频 ≈ 百毫秒级），但也不会再领远处的段。
    pub fn take_next(&self) -> Option<usize> {
        use std::sync::atomic::Ordering;
        let vis_a = self.visible_start.load(Ordering::Relaxed);
        let vis_b = self.visible_end.load(Ordering::Relaxed);
        let dur = self.duration_ms();
        let mut st = self.state.lock().ok()?;
        let mut best: Option<(u64, usize)> = None;
        for (i, s) in st.iter().enumerate() {
            if *s != 0 {
                continue;
            }
            let (a, b) = segment_span_ms_of(i, dur, self.seg_ms);
            let d = if b <= vis_a {
                (vis_a - b) as u64
            } else if a >= vis_b {
                (a - vis_b) as u64
            } else {
                0
            };
            match best {
                Some((bd, _)) if bd <= d => {}
                _ => best = Some((d, i)),
            }
        }
        if let Some((_, i)) = best {
            st[i] = 1;
            Some(i)
        } else {
            None
        }
    }

    /// 把一段的结果写进层里，并把该段标成"算完"。
    ///
    /// `peaks` 必须与 `segment_slice_range` 给出的下标区间一一对应。
    pub fn store_segment(&self, seg: usize, peaks: &[Peak]) {
        let dur = self.duration_ms();
        let (start_slice, _) = segment_slice_range_of(seg, dur, self.seg_ms);
        if let Ok(mut g) = self.layer.lock() {
            let mut next = (**g).clone();
            next.write_slices(start_slice, peaks);
            *g = Arc::new(next);
        }
        if let Ok(mut st) = self.state.lock() {
            if seg < st.len() {
                st[seg] = 2;
            }
        }
    }

    /// 把一段标成**失败**（打不开 / seek 失败 / 解码失败 / 被取消）。
    ///
    /// 失败的段**不会重试**：重试一个"打不开的段"只会无限烧 CPU，
    /// 而它留下的空洞在界面上就是"这里没波形"（如实呈现，不假装有数据）。
    pub fn fail_segment(&self, seg: usize) {
        if let Ok(mut g) = self.layer.lock() {
            let mut next = (**g).clone();
            next.failed_segments = next.failed_segments.saturating_add(1);
            *g = Arc::new(next);
        }
        if let Ok(mut st) = self.state.lock() {
            if seg < st.len() {
                st[seg] = 3;
            }
        }
    }

    /// 收尾：把层标成"渐进结束"（所有段都尝试过了）。
    pub fn finish(&self) {
        if let Ok(mut g) = self.layer.lock() {
            if !g.finished {
                let mut next = (**g).clone();
                next.finished = true;
                *g = Arc::new(next);
            }
        }
    }
}

/// **解一段基准层**（不碰共享状态，方便单测直接调用）。
///
/// 流程（P22 补充 2 的原文口径）：
/// 1. `av_seek_frame`(音频流) 到 `seg_start − `[`AAC_PRIMING_MS`]（`AVSEEK_FLAG_BACKWARD`）；
/// 2. **预热** [`WARMUP_PACKETS`] 个包（丢掉，让 AAC 的重叠历史收敛）；
/// 3. **按 PTS 绝对定位**，丢弃段边界之前的采样、截断段边界之后的采样；
/// 4. 折成 [`BASE_BUCKET_MS`] 一片的 min/max。
///
/// 返回该段的切片数组（长度 = [`segment_slice_range`] 的区间长度）。
/// `None` = 取消 / 失败（调用方不该把它当成"算完了"）。
pub(crate) fn decode_segment(
    dec: &mut RangeDecoder,
    seg: usize,
    duration_ms: i64,
    seg_ms: i64,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<Vec<Peak>> {
    use std::sync::atomic::Ordering;
    let (seg_start, seg_end) = segment_span_ms_of(seg, duration_ms, seg_ms);
    if seg_end <= seg_start {
        return Some(Vec::new());
    }
    let (s0, s1) = segment_slice_range_of(seg, duration_ms, seg_ms);
    let n = s1.saturating_sub(s0);
    // 一片 = `BASE_BUCKET_MS` 毫秒 = 4800 个采样点（48 kHz）；切片边界就是采样点边界的整数倍
    let per = (BASE_BUCKET_MS.max(1) * DECODE_RATE as i64 / 1000).max(1);
    let mut acc: Vec<Option<Peak>> = vec![None; n];
    let mut cancelled = false;
    let reason = dec.read_range_pts(
        seg_start,
        seg_end,
        seg_start - AAC_PRIMING_MS,
        WARMUP_PACKETS,
        |chunk, at_sample| {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return false;
            }
            let mut off: i64 = 0;
            let len = chunk.len() as i64;
            while off < len {
                let abs = at_sample + off;
                let slice = abs.div_euclid(per) as usize;
                if slice < s0 {
                    // **丢弃边界前的采样**：跳到 s0 的第一片起点
                    let skip = (s0 as i64 * per - abs).max(1);
                    off += skip;
                    continue;
                }
                if slice >= s1 {
                    break;
                }
                let take = (((slice as i64 + 1) * per - abs).max(1)).min(len - off);
                let a = off as usize;
                let b = (off + take) as usize;
                let seg_p = fold_peaks(chunk[a..b].iter());
                let k = slice - s0;
                acc[k] = Some(match acc[k] {
                    Some(p) => Peak {
                        min: p.min.min(seg_p.min),
                        max: p.max.max(seg_p.max),
                    },
                    None => seg_p,
                });
                off += take;
            }
            true
        },
    );
    if cancelled || reason == EndReason::Cancelled || reason == EndReason::Failed {
        return None;
    }
    Some(acc.into_iter().map(|p| p.unwrap_or(Peak::ZERO)).collect())
}

/// **跑一个基准层 worker 循环**（每个线程一次）。
///
/// * 领任务 → 解一段 → 写进共享层 → **唤醒界面**（`wake()`，数据驱动重绘）→ 领下一段；
/// * **取消纪律**（P22 的重中之重）：`cancel` 一置，最多再解完**一个包**就退出；
///   循环里不持有任何 `Sender`，所以调用方"置取消 → join"**一定**能返回；
/// * `wake` 只在"真的写进了一段数据"之后调用：**没有心跳**，没数据时界面就是静止的。
pub fn base_worker_loop(
    path: &Path,
    plan: &Arc<BasePlan>,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    wake: &(dyn Fn() + Send + Sync),
) {
    use std::sync::atomic::Ordering;
    let duration = plan.duration_ms();
    // **一个 worker 一个解码器实例**：段之间靠 `read_range_pts` 里的 seek + flush 复位。
    let mut dec = match RangeDecoder::new(path) {
        Ok(d) => Some(d),
        Err(e) => {
            // **降级点**：打不开 / 没有音轨 / 解码器没编进来。
            // 把原因写进计划（界面据此给一句提示），并把所有段标成失败 ——
            // 否则界面会永远停在"算到一半"（既没有波形、也没有任何解释）。
            tracing::warn!(error = %e, "波形基准层无法打开素材（整片降级为不画波形）");
            plan.set_error(e.to_string());
            plan.fail_all_pending();
            return;
        }
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(seg) = plan.take_next() else { return };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(dec) = dec.as_mut() else {
            plan.fail_segment(seg);
            continue;
        };
        let t0 = Instant::now();
        match decode_segment(dec, seg, duration, plan.segment_ms(), cancel) {
            Some(peaks) if !cancel.load(Ordering::Relaxed) => {
                let (a, b) = segment_span_ms_of(seg, duration, plan.segment_ms());
                plan.store_segment(seg, &peaks);
                wave_phase(
                    "base_seg",
                    t0.elapsed(),
                    format!(
                        "seg={seg} range={a}..{b} slices={} bucket_ms={BASE_BUCKET_MS}",
                        peaks.len()
                    ),
                );
                wake();
            }
            _ => {
                // 取消 → 立刻退（不写半个段）；真失败 → 标记该段（不重试）
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                tracing::warn!(
                    path = %path.display(),
                    seg,
                    "波形基准层这一段没解出来（该段留空，不重试）"
                );
                plan.fail_segment(seg);
                wake();
            }
        }
    }
}

/// **算整片基准层（阻塞版）**：给单测与"只跑一条线程"的短路径用。
///
/// `workers == 1` 时**不开线程**，直接在调用线程里跑同一个 [`base_worker_loop`] ——
/// 于是"1 线程 vs N 线程"比的是**同一份逻辑**，不是两套实现。
/// `seg_ms` 一般传 [`SEGMENT_MS`]；单测传小值来覆盖"多段 + 段边界"。
/// 返回 `(层, 音轨编码名, 源采样率, 源声道数)`。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn compute_base_sync(
    path: &Path,
    duration_ms: i64,
    workers: usize,
    seg_ms: i64,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<(BaseLayer, String, i32, i32), AppError> {
    // 先探一次素材：没有音轨 / 没有解码器 → 如实报错（界面据此降级）
    let (codec, rate, channels) = {
        let d = RangeDecoder::new(path)?;
        let c = d.codec_name().to_string();
        let (r, ch) = d.source_format();
        (c, r, ch)
    };
    let plan = Arc::new(BasePlan::with_segment_ms(duration_ms, seg_ms));
    plan.set_visible(0, duration_ms);
    let nw = workers.max(1);
    let nop = |_: ()| {};
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || nop(()));
    if nw == 1 {
        base_worker_loop(path, &plan, cancel, wake.as_ref());
    } else {
        let mut hs = Vec::with_capacity(nw);
        for _ in 0..nw {
            let p = path.to_path_buf();
            let plan = plan.clone();
            let cancel = cancel.clone();
            let wake = wake.clone();
            hs.push(std::thread::spawn(move || {
                base_worker_loop(&p, &plan, &cancel, wake.as_ref());
            }));
        }
        for h in hs {
            let _ = h.join();
        }
    }
    plan.finish();
    let layer = plan
        .snapshot()
        .map(|a| (*a).clone())
        .unwrap_or_else(|| BaseLayer::empty(duration_ms));
    Ok((layer, codec, rate, channels))
}

/// **整片顺序解码的参照**（P22 一致性单测的"标准答案"）。
///
/// 与分段路径的区别：**一次 seek 都不用**（`seek_ms = 0`、不做预热），
/// 从 0 号包一路顺序解到片尾，按 [`BASE_BUCKET_MS`] 折片。
/// 于是"并行分段 == 整片顺序"这条断言**真的在拷问**段边界与 priming 补偿：
/// 段起点算错、预热丢多了/丢少了、边界采样重复计入，都会让交界处的切片与它不一致。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) fn compute_base_reference(
    path: &Path,
    duration_ms: i64,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<Peak>, AppError> {
    use std::sync::atomic::Ordering;
    let mut dec = RangeDecoder::new(path)?;
    let n = base_slice_count(duration_ms);
    let per = (BASE_BUCKET_MS.max(1) * DECODE_RATE as i64 / 1000).max(1);
    let mut acc: Vec<Option<Peak>> = vec![None; n];
    let mut cancelled = false;
    let _reason = dec.read_range_pts(0, duration_ms.max(1), 0, 0, |chunk, at_sample| {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            return false;
        }
        let mut off: i64 = 0;
        let len = chunk.len() as i64;
        while off < len {
            let abs = at_sample + off;
            let slice = abs.div_euclid(per);
            if slice < 0 {
                off += 1;
                continue;
            }
            let slice = slice as usize;
            if slice >= n {
                break;
            }
            let take = (((slice as i64 + 1) * per - abs).max(1)).min(len - off);
            let a = off as usize;
            let b = (off + take) as usize;
            let p = fold_peaks(chunk[a..b].iter());
            acc[slice] = Some(match acc[slice] {
                Some(q) => Peak {
                    min: q.min.min(p.min),
                    max: q.max.max(p.max),
                },
                None => p,
            });
            off += take;
        }
        true
    });
    if cancelled {
        tracing::debug!(path = %path.display(), "基准层参照解码被取消");
    }
    Ok(acc.into_iter().map(|p| p.unwrap_or(Peak::ZERO)).collect())
}

/// **密集解出一片瓦片**（后台线程调用的那一个）：`seek` 到片起点、**逐包解满整片**。
///
/// * **密集**（P18）：这一片 5 秒的全部音频都过 `fold_peaks` ——
///   不走"只取桶中心一小段"的稀疏窗口。
///   用户要的是"放到最大时看到**完整**波形"，所以可见区间必须逐包解满；
///   代价与**可见时长**成正比（一片 5 s 音频），这是明确接受的取舍。
/// * **有界**：单次解一条 5 s 的区间，与片长无关 —— 取消（`cancel`）最坏等一个包。
/// * `tier_ms`：这一片按哪个档位分桶（`DENSE_TIERS_MS` 之一，由 [`tier_for_visible_ms`] 选出）；
///   它只影响"折成几个桶"，不影响"解多少音频"。
///
/// `dec` 由调用方持有并复用（细化调度线程一次创建、多片共用）——
/// 否则每片都要重新"打开 + 探测"一次文件，那点固定开销会和"每片只要 5 秒音频"一样大。
pub fn compute_tile(
    dec: &mut RangeDecoder,
    tile: usize,
    tier_ms: i64,
    cancel: &std::sync::atomic::AtomicBool,
) -> Option<DetailTile> {
    use std::sync::atomic::Ordering;
    let w = tier_ms.max(1);
    let (ts, te) = tile_span_ms(tile);
    let t0 = Instant::now();
    let mut samples: Vec<f32> = Vec::new();
    let mut chunk_start = ts;
    let reason = dec.read_range(ts, te, |chunk, at| {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        if samples.is_empty() {
            chunk_start = at.start_ms;
        }
        samples.extend_from_slice(chunk);
        true
    });
    let dur = t0.elapsed();
    if reason == EndReason::Cancelled {
        wave_phase(
            "refine",
            dur,
            format!("tile={tile} tier={w}ms range={ts}..{te} cancelled"),
        );
        return None;
    }
    let tile_out = scan_to_tile(
        w,
        tile,
        ChunkRange {
            start_ms: chunk_start,
            len: samples.len(),
        },
        &samples,
    );
    wave_phase(
        "refine",
        dur,
        format!(
            "tile={tile} tier={w}ms dense=true range={ts}..{te} buckets={} audio_ms={} reason={reason:?}",
            tile_out.peaks.len(),
            samples.len() as u64 * 1000 / DECODE_RATE as u64
        ),
    );
    Some(tile_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ffmpeg_exe;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    /// 本机 FFmpeg 9.0.2 的 `sine` 滤镜**默认峰值**（实测 `volumedetect` 报 -18.1 dB）。
    ///
    /// 用它把"想要的峰值"折算成 `volume=` 的增益 —— `sine` 没有 `amplitude` 选项，
    /// 而默认幅度又不是 1.0，不折算就会得到一条很扁的素材。
    const SINE_DEFAULT_PEAK: f64 = 0.1245;

    fn asset_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cap-waveform-tests");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 现场生成一个测试素材（参数与理由见上一版注释，未改动）。
    ///
    /// * `tone` 为 `Some((freq, secs, amp))` 时生成 **48 kHz 立体声 AAC**，峰值约 `amp`；
    /// * `tone` 为 `None` 时生成**纯视频、无音轨**的 mp4（降级路径的素材）。
    ///
    /// 生成失败必须让测试失败（只有 `Unknown encoder` / `openh264` 才 skip）：
    /// 否则"滤镜参数写错"会伪装成"测试通过"。
    fn gen_asset(name: &str, tone: Option<(u32, u32, f64)>) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(name);
        let _ = std::fs::remove_file(&out);
        let mut cmd = Command::new(&ff);
        cmd.args([
            "-y", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "testsrc2=size=160x120:rate=10",
        ]);
        match tone {
            Some((freq, secs, amp)) => {
                let src = format!("sine=frequency={freq}:sample_rate=48000:duration={secs}");
                let gain = (amp / SINE_DEFAULT_PEAK).max(0.0001);
                let filter = format!("volume={gain:.6},pan=stereo|c0=c0|c1=c0");
                let dur = secs.to_string();
                cmd.args([
                    "-f", "lavfi", "-i", &src, "-af", &filter, "-t", &dur,
                    "-c:v", "libopenh264", "-b:v", "200k", "-g", "10",
                    "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "96k",
                ]);
            }
            None => {
                cmd.args(["-t", "2", "-c:v", "libopenh264", "-b:v", "200k", "-pix_fmt", "yuv420p"]);
            }
        }
        cmd.arg(&out);
        match cmd.output() {
            Ok(o) if o.status.success() && out.is_file() => Some(out),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                let lower = err.to_ascii_lowercase();
                if lower.contains("unknown encoder") || lower.contains("openh264") {
                    println!("[skip] 随包 ffmpeg 不具备 libopenh264 编码能力：{err}");
                    None
                } else {
                    panic!("生成测试素材 {name} 失败（不该发生的错误，别静默跳过）：{err}");
                }
            }
            Err(e) => panic!("无法运行随包 ffmpeg：{e}"),
        }
    }


    // ---------------- P22 纯逻辑：全片密集基准层 ----------------

    /// **"打不开素材"要如实收尾**（P22）：`fail_all_pending` 把没算完的段全标失败，
    /// 于是界面能收尾 + 给提示，而不是永远停在"算到一半"。
    #[test]
    fn p22_unopenable_source_marks_all_segments_failed_with_a_reason() {
        let dur = 600_000i64;
        let plan = BasePlan::new(dur);
        assert!(plan.error().is_none());
        assert_eq!(plan.states(), vec![0u8; 2], "10 分钟 / 8 分钟 = 1.25 → 2 段");
        plan.set_error("没有音频流".into());
        plan.fail_all_pending();
        assert_eq!(plan.error().as_deref(), Some("没有音频流"));
        assert!(plan.states().iter().all(|s| *s == 3), "所有段都要标失败");
        assert!(plan.all_attempted(), "界面才会认为「这一轮结束了」");
        assert_eq!(plan.take_next(), None, "没有待算的段了");
        let l = plan.snapshot().expect("层永远在");
        assert_eq!(l.failed_segments, 2, "失败段数要如实记下");
        assert!(!l.has_peaks(), "一个峰值都没有 → 界面不会假装有波形");
    }

    /// 造一层"整片都算好了"的基准层（每片峰值恒定），供段级单测当输入。
    fn full_base(dur_ms: i64) -> BaseLayer {
        let mut b = BaseLayer::empty(dur_ms);
        let n = b.peaks.len();
        for i in 0..n {
            b.peaks[i] = Peak { min: -0.5, max: 0.5 };
            b.done[i] = true;
        }
        b.done_slices = n;
        b
    }

    /// **切片宽是常量 100 ms**（不像旧 L0 的"总时长/1024"）：切片个数只随时长变，
    /// 切片区间无缝、单调、覆盖全片。
    #[test]
    fn p22_base_slice_geometry_is_constant_and_tiles_the_duration() {
        assert_eq!(BASE_BUCKET_MS, 100);
        for dur in [3_000i64, 300_000, 3_600_000, 7_200_000] {
            let n = base_slice_count(dur);
            assert_eq!(n, (dur / BASE_BUCKET_MS) as usize, "时长 {dur} ms 的片数 = 时长/100");
            let b = BaseLayer::empty(dur);
            assert_eq!(b.slice_count(), n);
            for i in [0usize, 1, n / 2, n - 1] {
                let (s, e) = b.span_of(i);
                assert_eq!(e - s, BASE_BUCKET_MS, "每片恒为 100 ms");
                assert_eq!(s, i as i64 * BASE_BUCKET_MS, "第 i 片的起点 = i × 100 ms");
            }
            // 相邻片首尾相接
            for i in 1..n {
                assert_eq!(b.span_of(i - 1).1, b.span_of(i).0, "切片必须无缝");
            }
        }
        // 1 小时 ≈ 36,000 片 ≈ 288 KB（用户给的量级）
        assert_eq!(base_slice_count(3_600_000), 36_000);
        assert_eq!(base_slice_count(3_600_000) * 8, 288_000, "1 小时的峰值字节数");
        // 退化输入不 panic
        assert_eq!(base_slice_count(0), 0);
        assert_eq!(base_slice_count(-5), 0);
        assert!(BaseLayer::empty(0).peaks.is_empty());
    }

    /// **段与切片严格对齐**：`SEGMENT_MS` 是 `BASE_BUCKET_MS` 的整数倍，
    /// 于是"写一段 = 写一段连续下标"，段与段之间不重叠、不留缝。
    #[test]
    fn p22_segments_align_with_base_slices() {
        assert_eq!(SEGMENT_MS % BASE_BUCKET_MS, 0);
        assert_eq!(SEGMENT_MS, 480_000, "分段长度 = 8 分钟");
        assert_eq!(SEGMENT_MS % BASE_BUCKET_MS, 0, "480000/100 = 4800 整");
        let dur = 3_600_000i64; // 1 小时 → ceil(1h/段长) 段
        let n = segment_count(dur);
        assert_eq!(
            n,
            ((dur + SEGMENT_MS - 1) / SEGMENT_MS) as usize,
            "段数 = ceil(1 小时 / 8 分钟)"
        );
        let mut expected_start = 0usize;
        for seg in 0..n {
            let (a, b) = segment_span_ms(seg, dur);
            assert_eq!(a, seg as i64 * SEGMENT_MS, "段起点 = 段号 × 段长");
            if seg + 1 < n {
                assert_eq!(b - a, SEGMENT_MS, "整段都是 8 分钟（末段允许截断）");
            }
            let (s0, s1) = segment_slice_range(seg, dur);
            assert_eq!(s0, expected_start, "段 {seg} 的起始切片必须紧接上一段");
            assert_eq!(
                s1 - s0,
                ((b - a) / BASE_BUCKET_MS) as usize,
                "每段切片数 = 段长 / 桶宽"
            );
            expected_start = s1;
        }
        assert_eq!(expected_start, base_slice_count(dur), "所有段正好铺满整片");
        // 最后一段会被片尾截断（不满一整段）
        let dur2 = 3_650_000i64; // 1 小时 + 50 秒
        let n2 = segment_count(dur2);
        let last = n2 - 1;
        let (a, b) = segment_span_ms(last, dur2);
        assert_eq!(a, last as i64 * SEGMENT_MS);
        assert!(b - a < SEGMENT_MS, "片尾那段不满一整段");
        let (s0, s1) = segment_slice_range(last, dur2);
        assert_eq!(s1 - s0, ((b - a) / BASE_BUCKET_MS) as usize);
        assert_eq!(s1, base_slice_count(dur2), "最后一段正好到片尾");
        // 退化输入
        assert_eq!(segment_count(0), 0);
        assert_eq!(segment_slice_range(0, 0), (0, 0));
    }

    /// **写一段只动它自己那几片**（片数随 [`SEGMENT_MS`] 走）：`done` 位图与 `done_slices` 同步推进，
    /// 重复写不会把计数写坏（幂等）。
    #[test]
    fn p22_write_segment_marks_only_its_slices() {
        let dur = 600_000i64; // 10 分钟 = 2 段（8 分钟一段）
        let mut b = BaseLayer::empty(dur);
        assert!(!b.has_peaks());
        assert_eq!(b.coverage(), 0.0);
        let (s0, s1) = segment_slice_range(0, dur);
        let first_slices = s1 - s0; // 片数随 SEGMENT_MS 走，不写死
        b.write_slices(s0, &vec![Peak { min: -1.0, max: 1.0 }; s1 - s0]);
        assert_eq!(b.done_slices, first_slices);
        assert_eq!(b.count_done(), first_slices, "done_slices 必须等于位图里的 true 数");
        assert!(!b.is_complete(), "还有一段没算");
        assert_eq!(b.covered_ms(), SEGMENT_MS, "连续覆盖到第一段的末尾");
        // 再写一次同一段：计数不该翻倍
        b.write_slices(s0, &vec![Peak { min: -1.0, max: 1.0 }; s1 - s0]);
        assert_eq!(b.done_slices, first_slices, "重复写同一段必须幂等");
        // 第二段
        let (s0, s1) = segment_slice_range(1, dur);
        b.write_slices(s0, &vec![Peak { min: -0.25, max: 0.25 }; s1 - s0]);
        assert!(b.is_complete());
        assert_eq!(b.coverage(), 1.0);
        assert_eq!(b.covered_ms(), dur);
        // 越界写入被丢弃而不是 panic
        b.write_slices(usize::MAX - 2, &[Peak { min: 0.0, max: 0.0 }]);
        // 超出片尾的样本被裁掉
        let mut c = BaseLayer::empty(250);
        c.write_slices(0, &[Peak { min: -1.0, max: 1.0 }; 10]);
        assert_eq!(c.slice_count(), 3, "250 ms → 3 片");
        assert_eq!(c.done_slices, 3, "越界的 7 片不该被记进计数");
    }

    /// **`BasePlan` 的优先级 = 离可见区间最近**（P22 的"可见区间最优先"）。
    #[test]
    fn p22_plan_takes_the_segment_nearest_the_visible_window() {
        let dur = 3_600_000i64;
        let plan = BasePlan::new(dur);
        let n = plan.segment_count();
        let slices_per_seg = (SEGMENT_MS / BASE_BUCKET_MS) as usize;
        // 可见区间落在"目标段"内部（前后都还有段的中间段）
        let target = n / 2;
        let a = target as i64 * SEGMENT_MS + SEGMENT_MS / 4;
        plan.set_visible(a, a + 60_000);
        let mut order = Vec::new();
        while let Some(s) = plan.take_next() {
            order.push(s);
            plan.store_segment(s, &vec![Peak::ZERO; slices_per_seg]);
        }
        assert_eq!(order.len(), n, "每一段都要被领到一次");
        assert!(
            order[..3].contains(&target),
            "前三次领任务必须包含可见区间所在的那一段（实际 {order:?}）"
        );
        assert_eq!(order[0], target, "最优先的就是可见区间自己");
        // 距离次近的是 target-1 / target+1（前后各一段：同距离取小号）
        assert_eq!(order[1], target - 1);
        assert_eq!(order[2], target + 1);
        // **动态插队**：换一个可见区间，下一次领任务立刻改到新位置。
        // （可见窗取在段**内部**，否则"正好贴边"的邻段距离也是 0，会让期望值有歧义。）
        let plan2 = BasePlan::new(dur);
        plan2.set_visible(0, 60_000);
        assert_eq!(plan2.take_next(), Some(0));
        let a2 = (n - 2) as i64 * SEGMENT_MS + SEGMENT_MS / 4; // 倒数第二段内部
        plan2.set_visible(a2, a2 + 60_000);
        assert_eq!(plan2.take_next(), Some(n - 2), "换了可见区间必须立刻插队");
    }

    /// **可见区间先于其它区间完成**（P22 验收的纯逻辑证据）：
    /// 只领 3 段，必须都落在可见区间附近。
    #[test]
    fn p22_visible_window_finishes_before_far_segments() {
        let dur = 3_600_000i64;
        let plan = BasePlan::new(dur);
        let slices_per_seg = (SEGMENT_MS / BASE_BUCKET_MS) as usize;
        // 视口停在 20 分钟附近（落在段内部：20 min = 1200 s，段号 1200/480 = 2）
        let vis = (1_200_000i64, 1_260_000i64);
        plan.set_visible(vis.0, vis.1);
        let mut got = Vec::new();
        for _ in 0..3 {
            let s = plan.take_next().expect("还有待算的段");
            got.push(s);
            plan.store_segment(s, &vec![Peak::ZERO; slices_per_seg]);
        }
        let (va, vb) = (vis.0, vis.1);
        for s in &got {
            let (a, b) = segment_span_ms(*s, dur);
            let d = if b <= va {
                va - b
            } else if a >= vb {
                a - vb
            } else {
                0
            };
            assert!(
                d <= SEGMENT_MS,
                "第 {s} 段（{a}..{b}）离可见区间 {vis:?} 太远（距离 {d} ms）"
            );
        }
        assert!(got.contains(&segment_of_ms(vis.0, dur)), "可见段必须最先算完");
    }

    /// **取消**：循环只看标志，置位之后一段都不该再领（P22 的取消纪律，纯逻辑侧）。
    #[test]
    fn p22_cancelled_plan_stops_handing_out_segments() {
        use std::sync::atomic::Ordering;
        let dur = 600_000i64;
        let plan = BasePlan::new(dur);
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(plan.states(), vec![0u8, 0u8], "还没人领过");
        // `base_worker_loop` 的循环入口就是这个判据：标志为真时直接 return，不领任务
        if !cancel.load(Ordering::Relaxed) {
            let _ = plan.take_next();
        }
        assert_eq!(plan.states(), vec![0u8, 0u8], "取消后不该再领任务");
    }

    /// 密集瓦片与基准层是**两条互不调用**的路径：基准层的值不由 [`scan_to_tile`] 产出，
    /// 反之亦然（P22 的"一层一套口径"）。
    #[test]
    fn p22_base_layer_is_not_produced_by_the_sparse_or_tile_path() {
        let b = full_base(60_000);
        assert_eq!(b.bucket_ms, BASE_BUCKET_MS, "基准层的切片宽只由 BASE_BUCKET_MS 决定");
        // 基准层长 = 时长/100（与可见区间的档位表毫无关系）
        assert_eq!(b.slice_count(), 600);
        // 瓦片是另一套：桶宽 = 档位、片长 = TILE_MS
        let t = scan_to_tile(
            1,
            0,
            ChunkRange { start_ms: 0, len: 48_000 * 5 },
            &vec![0.5; 48_000 * 5],
        );
        assert_eq!(t.bucket_ms, MIN_BUCKET_MS);
        assert_eq!(t.start_ms, 0);
        assert!(t.peaks.len() <= TILE_BUCKETS);
    }

    // ---------------- 纯逻辑：密集瓦片（按档位分桶） ----------------

    /// 瓦片换算：片号 ↔ 时间 ↔ 片内桶，三者必须自洽（且 5 s 一片、1 ms 一桶）。
    #[test]
    fn tile_and_bucket_time_mapping_is_consistent() {
        assert_eq!(TILE_BUCKETS, 5000, "最细档位：5 秒 / 1 毫秒 = 5000 桶");
        for tile in [0usize, 1, 12, 719] {
            let (ts, te) = tile_span_ms(tile);
            assert_eq!(te - ts, TILE_MS);
            assert_eq!(tile_of_ms(ts), tile);
            assert_eq!(tile_of_ms(te - 1), tile);
            for tier in DENSE_TIERS_MS {
                // 档位表里的每一档都必须能整除一片瓦片（否则桶数与"每桶都满"都不成立）
                assert_eq!(TILE_MS % tier, 0, "档位 {tier} ms 必须整除 TILE_MS");
                assert!(tier >= MIN_BUCKET_MS, "档位不能细于最小桶宽");
                let cap = buckets_per_tile(tier);
                // 只查**真实存在**的桶（粗档位下 `cap - 1` 可能就等于 0，不能重复当"最后一桶"用）
                let probes: Vec<usize> =
                    if cap >= 3 { vec![0, 1, cap - 1] } else { (0..cap).collect() };
                for b in probes {
                    let (bs, be) = tile_bucket_span_ms(tier, tile, b);
                    assert_eq!(
                        (bs, be - bs),
                        (ts + b as i64 * tier, tier),
                        "tier={tier} tile={tile} b={b}"
                    );
                }
            }
        }
        assert_eq!(tile_of_ms(-100), 0, "负时刻夹到第 0 片");
    }

    /// 片内桶 → 采样点下标：桶宽 × 48 个采样点 / 1 ms，且**不靠累加**（最后一桶也不漂）。
    #[test]
    fn tile_bucket_sample_range_is_exact() {
        // 1 ms 档：48 个采样点一桶
        assert_eq!(tile_bucket_sample_range(1, 3, 0, 240_000), (0, 48));
        assert_eq!(tile_bucket_sample_range(1, 3, 1, 240_000), (48, 96));
        assert_eq!(tile_bucket_sample_range(1, 3, 4999, 240_000), (239_952, 240_000));
        // 粗档位：桶宽 × 48 个采样点，同样精确
        assert_eq!(tile_bucket_sample_range(100, 3, 1, 240_000), (4_800, 9_600));
        // 数组比"整片应有的样本数"短时，末尾夹住（最后一桶不满是常态）
        assert_eq!(tile_bucket_sample_range(1, 3, 4999, 100), (100, 100));
    }

    /// **`scan_to_tile` 是真分桶**：给一段已知采样点，逐桶 min/max 必须与暴力算法一致。
    #[test]
    fn scan_to_tile_matches_brute_force() {
        // 造 5 s 的单声道样本：每 48 个采样点一组，交替 +0.5 / -0.5 / 0
        let per = (MIN_BUCKET_MS * DECODE_RATE as i64 / 1000) as usize;
        let n = TILE_BUCKETS * per;
        let mut s = vec![0.0f32; n];
        for b in 0..TILE_BUCKETS {
            let v = match b % 3 {
                0 => 0.5,
                1 => -0.5,
                _ => 0.0,
            };
            for i in 0..per {
                s[b * per + i] = v;
            }
        }
        let t = scan_to_tile(MIN_BUCKET_MS, 0, ChunkRange { start_ms: 0, len: n }, &s);
        assert_eq!(t.tile, 0);
        assert_eq!(t.start_ms, 0);
        assert_eq!(t.bucket_ms, MIN_BUCKET_MS);
        assert!(t.dense, "密集路径产出的瓦片必须自带 dense 标志");
        assert_eq!(t.peaks.len(), TILE_BUCKETS);
        assert!(t.complete);
        for b in 0..TILE_BUCKETS {
            let p = t.peaks[b];
            assert_eq!(p.min, p.max, "每桶内是常数，min 必须等于 max（桶 {b}）");
            let want = match b % 3 {
                0 => 0.5,
                1 => -0.5,
                _ => 0.0,
            };
            assert!((p.max - want).abs() < 1e-6, "桶 {b} 的峰值应为 {want}");
        }
        // 同一段样本按粗档位折：桶数变少、每桶跨更多采样点，峰值仍然正确
        let coarse = scan_to_tile(1_000, 0, ChunkRange { start_ms: 0, len: n }, &s);
        assert_eq!(coarse.peaks.len(), buckets_per_tile(1_000));
        assert_eq!(coarse.peaks.len(), 5, "5 秒 / 1 秒 = 5 桶");
        assert!(coarse.peaks.iter().all(|p| p.max > 0.49), "1 秒桶里必然含 +0.5 段");
    }

    /// 只解到一半（渐进/取消）时，`peaks` 短于整片、`complete` 为假。
    #[test]
    fn scan_to_tile_marks_partial_tiles_incomplete() {
        let per = (MIN_BUCKET_MS * DECODE_RATE as i64 / 1000) as usize;
        let half = TILE_BUCKETS / 2;
        let s = vec![0.3f32; half * per];
        let t = scan_to_tile(
            MIN_BUCKET_MS,
            2,
            ChunkRange { start_ms: tile_span_ms(2).0, len: s.len() },
            &s,
        );
        assert_eq!(t.peaks.len(), half);
        assert!(!t.complete, "只解了一半的瓦片不能标记为 complete");
        // 落在本片之前的样本要被丢掉（这里给一段"起点早于本片"的样本）
        let lead = vec![0.9f32; per * 10];
        let mut s2 = lead.clone();
        s2.extend_from_slice(&vec![0.1f32; per * 20]);
        let t2 = scan_to_tile(
            MIN_BUCKET_MS,
            5,
            ChunkRange { start_ms: tile_span_ms(5).0 - 10, len: s2.len() },
            &s2,
        );
        assert!(
            t2.peaks.iter().all(|p| (p.max - 0.1).abs() < 1e-6),
            "片起点之前的样本必须被丢掉，不能混进这一片的峰值"
        );
    }

    // ---------------- P18 纯逻辑：`VISIBLE_BUCKETS` 驱动的档位 ----------------

    /// **P18 ① / P20 口径**：给定可见时长（**门槛之内**），**请求的桶数恒为 600**
    /// （多组时长，跨 3 个数量级）。
    #[test]
    fn p18_visible_bucket_count_is_always_six_hundred() {
        assert_eq!(VISIBLE_BUCKETS, 600, "用户口径：可见区间恒定 600 个切片");
        // 覆盖 0.6 s → 120 s 的真实视口时长（这正是 P20 门槛之内**全部**的缩放级别；
        // 门槛之外由 `segments_for_view` 交给**基准层**，见
        // `p20_threshold_is_exactly_one_twenty_seconds`）
        for vis in [
            600.0, 1_000.0, 3_000.0, 6_000.0, 12_000.0, 60_000.0, 65_000.0, 119_000.0,
            DENSE_MAX_VISIBLE_MS,
        ] {
            let bw = visible_bucket_ms(vis);
            // 桶数 = ceil(可见时长 / 桶宽)，恒为 600（向上取整：最后一桶可以不满）
            let buckets = (vis / bw).ceil();
            assert_eq!(
                buckets, 600.0,
                "可见 {vis} ms、桶宽 {bw} ms 时桶数应为 600，实测 {buckets}"
            );
            // 选择的规则：**不粗于**所需桶宽的**最粗**一档
            let tier = tier_for_visible_ms(vis);
            assert!(
                (tier as f64) <= bw + 1e-9,
                "可见 {vis} ms：档位 {tier} ms 不得粗于所需桶宽 {bw} ms"
            );
            let idx = tier_index_for_visible_ms(vis);
            if idx > 0 {
                let coarser = DENSE_TIERS_MS[idx - 1];
                assert!(
                    (coarser as f64) > bw + 1e-9,
                    "可见 {vis} ms：再粗一档 {coarser} ms 已经超过所需桶宽 {bw} ms（那就不是最粗的满足者）"
                );
            }
        }
        // 档位表里每一档都能被"整数可见时长"精确命中（用户实际会落在这些点上），
        // 且**比它小 1 ms 就必须换到更细的一档**（档位边界是硬的，不是浮点擦边）
        for (i, tier) in DENSE_TIERS_MS.iter().copied().enumerate() {
            let vis = tier as f64 * VISIBLE_BUCKETS as f64;
            assert_eq!(tier_for_visible_ms(vis), tier, "可见 {vis} ms 应正好选中 {tier} ms 档");
            assert_eq!(visible_bucket_ms(vis), tier as f64);
            if i + 1 < DENSE_TIERS_MS.len() {
                assert_eq!(
                    tier_for_visible_ms(vis - 1.0),
                    DENSE_TIERS_MS[i + 1],
                    "可见 {vis} ms 再少 1 ms（{} ms）必须落到更细的一档",
                    vis - 1.0
                );
            }
        }
    }

    /// **P20 档位表口径**：最粗一档 == 门槛处的所需桶宽，且每一档都整除 [`TILE_MS`]。
    ///
    /// 这条不变式是"可见 <= 120 s 时桶数恒为 600"的**代数保证**：
    /// `tier_for_visible_ms(门槛) = 门槛/600 = `[`dense_max_tier_ms`]，而它正好是表头。
    #[test]
    fn p20_dense_tiers_cover_exactly_up_to_the_visible_threshold() {
        assert_eq!(dense_max_tier_ms(), 200, "120 s / 600 个切片 = 200 ms/桶");
        assert_eq!(
            DENSE_TIERS_MS[0], 200,
            "档位表最粗一档必须正好是门槛处所需桶宽（否则门槛内会出现桶宽被夹住、桶数 < 600）"
        );
        assert_eq!(tier_for_visible_ms(DENSE_MAX_VISIBLE_MS), 200);
        assert_eq!(visible_bucket_ms(DENSE_MAX_VISIBLE_MS), 200.0);
        assert!(
            DENSE_TIERS_MS.windows(2).all(|w| w[0] > w[1]),
            "档位表必须严格从粗到细：{:?}",
            DENSE_TIERS_MS
        );
        assert_eq!(
            *DENSE_TIERS_MS.last().unwrap(),
            MIN_BUCKET_MS,
            "档位表必须一路覆盖到封顶的 1 ms"
        );
        for tier in DENSE_TIERS_MS {
            assert_eq!(TILE_MS % tier, 0, "档位 {tier} ms 必须整除 TILE_MS");
            assert_eq!(
                buckets_per_tile(tier) as i64 * tier,
                TILE_MS,
                "档位 {tier}：一片瓦片里的桶数 × 桶宽 必须正好等于瓦片时长（不许有半桶）"
            );
            assert!(tier >= MIN_BUCKET_MS, "档位不能细于最小桶宽");
        }
        // 门槛处的片数上界：120 s / 5 s = 24 片，护栏必须容得下
        let tiles_at_threshold = (DENSE_MAX_VISIBLE_MS / TILE_MS as f64).ceil() as usize + 1;
        assert!(
            tiles_at_threshold <= MAX_DENSE_TILES_PER_VIEW,
            "门槛处一张屏最多 {tiles_at_threshold} 片，护栏只有 {MAX_DENSE_TILES_PER_VIEW} 片不够用"
        );
        // 一屏最多 24 片 < LRU 的 64 片：覆盖永远装得进缓存（否则会"算了又挤掉"抖动）
        assert!(
            tiles_at_threshold <= TILE_CACHE_MAX,
            "门槛内的片数不能超过瓦片缓存容量"
        );
    }

    /// **P20 ①（门槛本身）**：可见时长 **= 120 s → 走密集层、切片数 600**；
    /// **121 s → 走基准层**（P22 起不再是稀疏概览）。
    ///
    /// 这是 P20 要修的根因的**边界断言**：旧口径下 120 s 与 121 s 都只给稀疏概览，
    /// 缩放因此完全没有观感变化。
    #[test]
    fn p20_threshold_is_exactly_one_twenty_seconds() {
        // 10 分钟的片子：120 s 的可见区间落在片内，不会被"夹到片尾"影响
        let dur = 600_000i64;
        let base = full_base(dur);

        // ① 门槛上：密集层，600 个切片
        //    片**全部算满**（门槛处要 24~25 片，正好验证"一屏的片数在护栏与缓存容量之内"）
        let tiles = tiles_covering(200, 0, DENSE_MAX_VISIBLE_MS as i64);
        let r = segments_for_view(&base, &tiles, 0.0, DENSE_MAX_VISIBLE_MS, 1024);
        assert_eq!(r.layer, ViewLayer::Dense, "可见 120 s 必须走密集层（P20 的边界）");
        assert_eq!(r.bucket_ms, 200, "120 s / 600 = 200 ms/桶");
        assert_eq!(
            r.segs.len(),
            VISIBLE_BUCKETS,
            "可见 120 s 应当正好切出 {VISIBLE_BUCKETS} 段，实测 {}",
            r.segs.len()
        );
        assert!(r.segs.iter().all(|s| s.detail), "全部来自密集层");
        assert!(r.refine_ms.is_none(), "片都算满了 → 没有缺口");
        // 120 s / 200 ms = 600，所以段与段之间**首尾相接**、且铺满整段可见区间
        let mut cursor = 0.0f64;
        for s in &r.segs {
            assert!(
                (s.start_ms - cursor).abs() < 1e-9,
                "段之间不能有空隙：期望从 {cursor} 开始，实测 {}",
                s.start_ms
            );
            assert!((s.end_ms - s.start_ms - 200.0).abs() < 1e-9, "密集段宽恒为 200 ms");
            cursor = s.end_ms;
        }
        assert!((cursor - DENSE_MAX_VISIBLE_MS).abs() < 1e-9);

        // ①b 门槛上但**一片瓦片都没有**：段数不足 600 是对的（缺口用基准层兜底），
        //     但必须报出缺口区间 —— 界面拿它去排队细化（见 p20_threshold_view_* 那条）
        let none: std::collections::HashMap<usize, DetailTile> = std::collections::HashMap::new();
        let r0 = segments_for_view(&base, &none, 0.0, DENSE_MAX_VISIBLE_MS, 1024);
        assert_eq!(r0.layer, ViewLayer::Dense);
        assert_eq!(r0.bucket_ms, 200);
        assert!(r0.refine_ms.is_some(), "没有瓦片时必须报出缺口");
        assert!(r0.segs.iter().any(|s| !s.detail), "缺口要用基准层兜底（不留空）");

        // ② 门槛外 1 ms：交给**基准层**（100 ms/片，P22 起不再是稀疏填充）
        let r2 = segments_for_view(&base, &tiles, 0.0, DENSE_MAX_VISIBLE_MS + 1.0, 1024);
        assert_eq!(r2.layer, ViewLayer::Base, "可见 120.001 s 必须走基准层");
        assert_eq!(r2.bucket_ms, BASE_BUCKET_MS, "基准层帧的切片宽 = 100 ms");
        assert!(r2.segs.iter().all(|s| s.detail), "基准层是主数据源 → 画实色");
        assert!(r2.refine_ms.is_none(), "基准层帧不请求密集细化");
        assert!(!r2.segs.is_empty(), "基准层帧必须有东西可画（不能留白）");
        // 桶宽被夹在门槛处所需桶宽（200 ms）：再长的可见区间也不会给出更粗的档
        for vis in [
            DENSE_MAX_VISIBLE_MS + 1.0,
            600_000.0,
            2_000_000.0,
            3_900_000.0,
        ] {
            assert_eq!(visible_bucket_ms(vis), 200.0, "桶宽封顶在 200 ms（{vis} ms）");
            assert_eq!(tier_for_visible_ms(vis), 200, "档位停在最粗一档（{vis} ms）");
        }
    }

    /// **P18 ②**：可见时长减半 ⇒ 所需档位**严格更细**（单调，直到封顶）。
    #[test]
    fn p18_halving_the_view_never_coarsens_the_tier() {
        // 起点取一个**远在门槛之外**的可见时长（20 万秒）：那时档位就该停在最粗一档，
        // 之后一路减半直到 1 ms 封顶 —— 跨档次数必须足够多（口径与 P18 完全一致）
        let mut vis = 200_000_000.0f64;
        let mut last = tier_index_for_visible_ms(vis);
        assert_eq!(last, 0, "门槛之外应当停在最粗一档（{vis} ms）");
        let mut strict = 0usize;
        while vis > 1.0 {
            vis /= 2.0;
            let now = tier_index_for_visible_ms(vis);
            assert!(
                now >= last,
                "可见时长减半后档位不能变粗（{vis} ms：{last} → {now}）"
            );
            if now > last {
                strict += 1;
            }
            last = now;
        }
        assert!(strict >= 8, "一路减半应当反复跨档（实测跨了 {strict} 次）");
    }

    /// **P18 ③**：细到 [`MIN_BUCKET_MS`] 封顶后不再变细；此时可见区间**完整覆盖**（≈600 段）。
    #[test]
    fn p18_floor_caps_resolution_and_covers_the_whole_view_once_dense() {
        // 封顶：再短也不会细于 1 ms
        for vis in [600.0, 300.0, 60.0, 10.0, 1.0, 0.1] {
            assert_eq!(
                tier_for_visible_ms(vis),
                MIN_BUCKET_MS,
                "可见 {vis} ms 应封顶在 {MIN_BUCKET_MS} ms"
            );
            assert_eq!(visible_bucket_ms(vis), MIN_BUCKET_MS as f64);
        }
        // 封顶后"完整覆盖"：可见 600 ms = 600 个 1 ms 桶，段数恰好 600、且首尾相接无空洞
        let dur = 60_000i64;
        let base = full_base(dur);
        let mut tiles = std::collections::HashMap::new();
        // 把可见区间所在的第 0 片（0..5000）算满：48 kHz × 5 s 个采样点
        let n = (5 * DECODE_RATE) as usize;
        let dense = vec![0.8f32; n];
        tiles.insert(
            0,
            scan_to_tile(MIN_BUCKET_MS, 0, ChunkRange { start_ms: 0, len: n }, &dense),
        );
        let r = segments_for_view(&base, &tiles, 0.0, 600.0, 1024);
        assert_eq!(r.layer, ViewLayer::Dense, "600 ms 可见区间必须走密集层");
        assert_eq!(r.bucket_ms, MIN_BUCKET_MS);
        assert_eq!(r.segs.len(), 600, "恰好 600 段（= VISIBLE_BUCKETS）");
        assert!(r.segs.iter().all(|s| s.detail), "全部来自密集层");
        assert!(r.segs.iter().all(|s| s.peak.max > 0.79), "峰值来自真解码的样本");
        // 无空洞：第 i 段的终点 == 第 i+1 段的起点，最后一段到可见区间的右端
        let mut cursor = 0.0f64;
        for s in &r.segs {
            assert!(
                (s.start_ms - cursor).abs() < 1e-6,
                "段之间不能有空隙：期望从 {cursor} 开始，实测 {}",
                s.start_ms
            );
            assert!(s.end_ms > s.start_ms);
            cursor = s.end_ms;
        }
        assert!((cursor - 600.0).abs() < 1e-6, "最后一段必须铺到可见区间右端");
        assert!(r.refine_ms.is_none(), "片已算满 → 没有缺口要细化");
        assert!(r.pending_ms.is_none());
    }

    /// **P18 ④**：可见区间走**密集解码**（不是稀疏窗口路径）——
    /// 数据凭据是瓦片的 `dense` 标志 + 桶数上界（稀疏路径每桶只有 0.25 s 窗口）。
    #[test]
    fn dense_tiles_never_come_from_the_sparse_window_path() {
        // ① 类型层：密集瓦片恒带 `dense = true`
        let s = vec![0.4f32; 48 * 100];
        let t = scan_to_tile(MIN_BUCKET_MS, 0, ChunkRange { start_ms: 0, len: s.len() }, &s);
        assert!(t.dense, "`scan_to_tile` 是密集路径的唯一入口，必须标记 dense");
        // ② 只给 100 ms 的音频时，1 ms 档位只可能折出 100 个桶 ——
        //    稀疏窗口路径不可能做到这一点（它每个桶只看 0.25 s）
        assert_eq!(t.peaks.len(), 100);
        // ③ 反过来：**基准层**的切片宽恒为 `BASE_BUCKET_MS`（100 ms），
        //    与"1 ms 一桶的密集瓦片"是两条互不调用的路径
        assert_eq!(
            BASE_BUCKET_MS, 100,
            "基准层切片宽是 100 ms —— 与上面 1 ms 一桶是两条不同的路径"
        );
        // ④ 可见区间的段全部来自密集层（基准层只出现在"瓦片还没到"的那几段上）
        let dur = 60_000i64;
        let base = full_base(dur);
        let n = (5 * DECODE_RATE) as usize;
        let mut tiles = std::collections::HashMap::new();
        tiles.insert(
            0,
            scan_to_tile(1, 0, ChunkRange { start_ms: 0, len: n }, &vec![0.8f32; n]),
        );
        let r = segments_for_view(&base, &tiles, 0.0, 600.0, 1024);
        assert!(r.segs.iter().all(|s| s.detail), "可见区间必须逐桶密集覆盖");
        // 而没有瓦片的区间：基准层只是**兜底**（段画淡一档，且明确报出缺口）
        let r2 = segments_for_view(&base, &tiles, 20_000.0, 20_600.0, 1024);
        assert_eq!(r2.layer, ViewLayer::Dense);
        assert!(r2.segs.iter().any(|s| !s.detail), "缺口要有基准层兜底，不能留空");
        assert_eq!(r2.refine_ms, Some((20_000, 20_600)));
    }

    /// **P18 ⑤**：档位缓存是**跨档位合计**的有界 LRU，且换档会把别的档位清掉。
    #[test]
    fn tile_cache_keeps_one_tier_and_stays_bounded() {
        let mut c = TileCache::new();
        let mk = |tile: usize, tier: i64| DetailTile {
            tile,
            start_ms: tile as i64 * TILE_MS,
            bucket_ms: tier,
            peaks: vec![Peak { min: 0.0, max: 0.1 }],
            complete: true,
            dense: true,
        };
        c.insert(mk(0, 100));
        c.insert(mk(1, 100));
        assert_eq!(c.tiers(), vec![100]);
        // 换档：清掉旧档，缓存里只剩一个档位
        let dropped = c.retain_tier(10);
        assert_eq!(dropped, 2);
        assert!(c.is_empty());
        assert_eq!(c.tiers(), Vec::<i64>::new());
        // 插入 200 片（同一档位）也不会超过上限
        for t in 0..200 {
            c.insert(mk(t, 10));
        }
        assert_eq!(c.len(), TILE_CACHE_MAX);
        assert_eq!(c.tiers(), vec![10], "缓存里永远只有一个档位");
    }

    // ---------------- 纯逻辑：绘制用的段（P22 换层口径） ----------------

    /// **可见区间长于 120 s → 走全片基准层**（100 ms 一片的**真数据**，不再是稀疏填充）。
    #[test]
    fn p22_segments_use_the_base_layer_for_long_views() {
        let dur = 3_600_000i64;
        let base = full_base(dur);
        let tiles = std::collections::HashMap::new();
        // 整片铺满：可见 3600 s ≫ 120 s
        let r = segments_for_view(&base, &tiles, 0.0, dur as f64, 1000);
        assert_eq!(r.layer, ViewLayer::Base, "整片视图必须走基准层");
        assert_eq!(r.bucket_ms, BASE_BUCKET_MS, "基准层的切片宽恒为 100 ms");
        assert!(
            r.refine_ms.is_none(),
            "长视口不该触发密集瓦片请求（那是几百个 5 s 瓦片）"
        );
        assert!(r.pending_ms.is_none(), "整片都有数据时不该画渐进线");
        assert!(r.segs.iter().all(|s| s.detail), "基准层是主数据源 → 画实色");
        // 36,000 片按"每列一片"合并到 1000 列 → 段数 ≈ 1000（不是 36000）
        assert!(
            r.segs.len() <= 1100 && r.segs.len() >= 900,
            "合并到像素之后段数应 ≈ 列数，实际 {}",
            r.segs.len()
        );
        // 合并**不改时间**：相邻段首尾相接、覆盖整个可见区间
        assert_eq!(r.segs.first().unwrap().start_ms, 0.0);
        assert!(r.segs.last().unwrap().end_ms >= dur as f64 - 0.5);
        for w in r.segs.windows(2) {
            assert!(
                (w[1].start_ms - w[0].end_ms).abs() < 0.5,
                "合并后的段必须无缝：{:?} → {:?}",
                w[0],
                w[1]
            );
        }
        // 不合并（cols = 1）时逐片给段
        let r1 = segments_for_view(&base, &tiles, 0.0, 130_000.0, 1);
        assert_eq!(r1.layer, ViewLayer::Base);
        assert_eq!(r1.segs.len(), 1300, "130 s / 100 ms = 1300 片（不合并）");
        assert_eq!(r1.segs[0].start_ms, 0.0);
        assert_eq!(r1.segs[0].end_ms, 100.0);
    }

    /// **基准层还没补齐时**：只画已经算好的切片，并报出"第一处缺口"给渐进线用
    /// （长视口下 `refine_ms` 必须是 `None` —— 缺口要等后台 worker 补，不是界面排瓦片）。
    #[test]
    fn p22_incomplete_base_reports_the_first_gap_not_a_refine_request() {
        // 10 分钟的片子：长可见区间不会被"夹到片尾"影响
        let dur = 600_000i64;
        let mut base = BaseLayer::empty(dur);
        // 只算前 10 秒（前 100 片）
        base.write_slices(0, &vec![Peak { min: -0.3, max: 0.3 }; 100]);
        let tiles = std::collections::HashMap::new();
        // 可见 50 s（≤120 s → 密集层）→ 缺口要用基准层兜底 + 报 refine
        let r = segments_for_view(&base, &tiles, 0.0, 50_000.0, 1000);
        assert_eq!(r.layer, ViewLayer::Dense, "50 s ≤ 门槛 → 密集层");
        assert!(r.refine_ms.is_some(), "密集层的缺口要报给界面去排瓦片");
        assert_eq!(r.pending_ms, Some(0));
        assert!(
            r.segs.iter().any(|s| !s.detail),
            "没有密集数据时必须用基准层兜底（不能留空）"
        );
        // 可见 5 分钟（> 120 s）→ 基准层；缺口只报渐进线，不报 refine
        let r2 = segments_for_view(&base, &tiles, 0.0, 300_000.0, 1000);
        assert_eq!(r2.layer, ViewLayer::Base);
        assert!(r2.refine_ms.is_none(), "长视口不许排密集瓦片");
        assert_eq!(r2.pending_ms, Some(10_000), "渐进线画在第一处缺口的起点");
        // 已算出的那 10 秒必须画出来（而且只有那 10 秒）
        assert!(!r2.segs.is_empty());
        assert!(r2.segs.iter().all(|s| s.end_ms <= 10_000.0 + 0.5));
    }

    /// **短可见区间 → 密集层 + 瓦片命中时不请求**（P20 口径在 P22 下保持不变）。
    #[test]
    fn p22_segments_switch_to_dense_and_report_missing_gaps() {
        let dur = 3_600_000i64;
        let base = full_base(dur);
        let mut tiles = std::collections::HashMap::new();
        // 放一片（覆盖 10_000..15_000 ms），1 ms 档
        let t = scan_to_tile(
            1,
            2,
            ChunkRange { start_ms: 10_000, len: 48_000 * 5 },
            &vec![0.7f32; 48_000 * 5],
        );
        tiles.insert(2, t);
        // 可见 600 ms（10_000..10_600，正好 600 个 1 ms 桶）→ 全部在已算好的片里
        let r = segments_for_view(&base, &tiles, 10_000.0, 10_600.0, 1024);
        assert_eq!(r.layer, ViewLayer::Dense, "可见 600 ms 必须走密集层");
        assert_eq!(r.bucket_ms, MIN_BUCKET_MS);
        assert_eq!(r.segs.len(), 600, "600 ms / 1 ms = 600 段（= VISIBLE_BUCKETS）");
        assert!(r.segs.iter().all(|s| s.detail));
        assert!(r.refine_ms.is_none(), "整段都在已算好的瓦片里，不该再请求");
        assert!(r.pending_ms.is_none());
        // 再看一段没有瓦片的区间：必须有基准层兜底 + 缺口区间
        let r2 = segments_for_view(&base, &tiles, 60_000.0, 60_600.0, 1024);
        assert_eq!(r2.layer, ViewLayer::Dense);
        assert!(
            r2.segs.iter().any(|s| !s.detail),
            "没有密集数据时必须用基准层兜底（不能留空）"
        );
        let (a, b) = r2.refine_ms.expect("没有瓦片时必须报出缺口区间");
        assert_eq!(a, 60_000);
        assert_eq!(b, 60_600);
        assert_eq!(r2.pending_ms, Some(60_000), "渐进线画在缺口起点");
    }

    /// 造一片**真的算满**的密集瓦片（`tier` 档、片号 `tile`），峰值恒定。
    fn full_tile(tier: i64, tile: usize) -> DetailTile {
        let per = ((tier.max(1) * DECODE_RATE as i64) / 1000).max(1) as usize;
        let n = buckets_per_tile(tier) * per;
        let (ts, _) = tile_span_ms(tile);
        scan_to_tile(
            tier,
            tile,
            ChunkRange { start_ms: ts, len: n },
            &vec![0.8f32; n],
        )
    }

    /// 把 `[a, b)` 覆盖到的每一片都放进 `tiles`（按 `tier` 档算满）。
    fn tiles_covering(tier: i64, a: i64, b: i64) -> std::collections::HashMap<usize, DetailTile> {
        let mut m = std::collections::HashMap::new();
        for t in tile_of_ms(a)..=tile_of_ms(b) {
            m.insert(t, full_tile(tier, t));
        }
        m
    }

    /// **P20 ②**：视口 60 s → 20 s → 5 s → 1 s：**切片宽严格递减**、每一步都 `layer=detail`。
    ///
    /// 这一条直接钉住用户的验收口径「每一步波形都明显变细」：
    /// 旧口径下这四步都落在"可见区间 ≥ 一片瓦片（5 s）"那一侧，**全部走 L0**，
    /// 于是桶宽一动不动（0.6/3.6/14.4/72 ms/桶），观感与缩放无关。
    #[test]
    fn p20_zoom_ladder_shrinks_the_slice_width_at_every_step() {
        let dur = 600_000i64;
        let base = full_base(dur);
        let mut widths: Vec<f64> = Vec::new();
        let mut tiers: Vec<i64> = Vec::new();
        for vis in [60_000.0, 20_000.0, 5_000.0, 1_000.0] {
            let tier = tier_for_visible_ms(vis);
            let tiles = tiles_covering(tier, 100_000, 100_000 + vis as i64);
            let r = segments_for_view(&base, &tiles, 100_000.0, 100_000.0 + vis, 1024);
            assert_eq!(r.layer, ViewLayer::Dense, "可见 {vis} ms 必须走密集层（layer=detail）");
            assert_eq!(r.bucket_ms, tier);
            assert!(
                !r.segs.is_empty() && r.segs.iter().all(|s| s.detail),
                "整段都该由密集瓦片覆盖"
            );
            assert!(r.refine_ms.is_none(), "片都算满了，不该有缺口");
            // 切片宽恒等于档位（这就是"可见区间 / 600 量化到档位"的结果）
            let w = r.segs[0].end_ms - r.segs[0].start_ms;
            assert!((w - tier as f64).abs() < 1e-9, "切片宽应等于档位 {tier} ms");
            // 段数 ≈ 可见时长 / 档位（向上取整；档位比"所需桶宽"更细 → 段数 ≥ 600）
            let expect = (vis / tier as f64).ceil() as usize;
            assert_eq!(r.segs.len(), expect, "可见 {vis} ms / {tier} ms = {expect} 段");
            assert!(r.segs.len() >= VISIBLE_BUCKETS, "可见区间内至少 600 个切片");
            widths.push(w);
            tiers.push(tier);
        }
        for i in 1..widths.len() {
            assert!(
                widths[i] < widths[i - 1],
                "每一步都必须更细：{widths:?}（tiers={tiers:?}）"
            );
        }
        for i in 1..tiers.len() {
            assert!(tiers[i] < tiers[i - 1], "档位必须严格更细：{tiers:?}");
        }
    }

    /// **P20 ④**：绘制按**当前可见区间的时间→x 映射**落位 —— 同一档位下改变缩放，
    /// 切片的时间位置**按时间对齐**（不是"整块拉伸"）。
    ///
    /// 判据（两帧的切片时间戳序列比较），两层各来一遍：
    /// * **同一批数据**：`[20 s, 25 s)` 在 120 s 视口里那 25 段的起止时间，
    ///   在 40 s 视口里**一模一样**（时间对齐）；如果实现是"整块拉伸"，
    ///   这些段的时间宽会随缩放变大（25 段铺满 120 s → 每段 4.8 s）。
    /// * **窗口变化**：可见区间变小 ⇒ 段数变少、且剩下的段严格落在新的可见区间里。
    /// * **基准层一侧同理**：65 分钟片子上基准层切片的时间边界**只由 100 ms 网格决定**，
    ///   60 s → 6 s 的 ×10 缩放后，`[600 s, 630 s)` 里那批切片的 start/end 分毫不动。
    #[test]
    fn p20_painting_is_positioned_by_time_not_stretched() {
        // ---- 密集层：120 s 视口 vs 40 s 视口 ----
        let dur = 600_000i64;
        let base = full_base(dur);
        let t_lo = 20_000.0;
        let t_hi = 25_000.0;
        let tiers = [tier_for_visible_ms(DENSE_MAX_VISIBLE_MS), tier_for_visible_ms(40_000.0)];
        assert_ne!(tiers[0], tiers[1], "前提：两个视口落在不同档位（这正是 P20 要拷问的场景）");

        let mut per_tier: Vec<Vec<(f64, f64)>> = Vec::new();
        for (i, vis) in [DENSE_MAX_VISIBLE_MS, 40_000.0].iter().copied().enumerate() {
            let tiles = tiles_covering(tiers[i], 0, vis as i64);
            let r = segments_for_view(&base, &tiles, 0.0, vis, 1024);
            assert_eq!(r.layer, ViewLayer::Dense);
            per_tier.push(r.bucket_spans());
        }
        let pick = |spans: &[(f64, f64)]| -> Vec<(f64, f64)> {
            spans
                .iter()
                .copied()
                .filter(|(s, e)| *s >= t_lo - 1e-9 && *e <= t_hi + 1e-9)
                .collect()
        };
        let a = pick(&per_tier[0]);
        let b = pick(&per_tier[1]);
        assert!(a.len() >= 20, "120 s 视口里 [20,25] s 应有 ≈25 段，实测 {}", a.len());
        assert!(b.len() >= 20, "40 s 视口里 [20,25] s 应有 ≈125 段，实测 {}", b.len());
        // 时间对齐：同一时刻的段，起止时间分毫不差（切片宽 = 档位，不随视口变化）
        // ① 两帧里"同一时间点"的段宽一致（120 s 档 = 200 ms；40 s 档 = 100 ms，
        //    这是**档位**变了，属于"更细"，而不是"同一批数据被拉伸"）
        assert!(
            (a[0].1 - a[0].0 - 200.0).abs() < 1e-9,
            "120 s 档的切片宽必须是 200 ms（= 120 s / 600），实测 {}",
            a[0].1 - a[0].0
        );
        assert!(
            (b[0].1 - b[0].0 - 50.0).abs() < 1e-9,
            "40 s 档的切片宽必须是 50 ms（40 s / 600 = 66.7 ms → 不粗于它的最粗档是 50 ms），\
             实测 {}",
            b[0].1 - b[0].0
        );
        // ② 关键判据：40 s 视口里**第一段的时刻**与 120 s 视口完全对齐，
        //    且 40 s 视口把 [20,25] s 切得**更密**（段数更多）—— 越放大越细
        assert_eq!(a[0].0, b[0].0, "同一时间点必须落在同一个 start_ms 上（按时间对齐）");
        assert!(
            b.len() > a.len(),
            "放大后同一时间跨度里应当有更多切片：120 s 档 {} 段 → 40 s 档 {} 段",
            a.len(),
            b.len()
        );
        // ③ 反过来证明"不是拉伸"：若把 a 的 25 段拉伸到 120 s 铺满，每段会变成 4800 ms；
        //    实测每段恒为 200 ms（120 s / 600），所以"拉伸"这种画法不可能成立
        assert!(
            (a.last().unwrap().1 - a.first().unwrap().0 - 5_000.0).abs() < 200.0,
            "25 段 200 ms 的切片只能覆盖 ≈5 s，不可能被拉伸成 120 s"
        );

        // ---- 长可见区间（> 门槛）一侧：基准层切片边界**只由 100 ms 网格决定**，与视口无关 ----
        let dur65 = 3_900_000i64;
        let base65 = full_base(dur65);
        let empty: std::collections::HashMap<usize, DetailTile> = std::collections::HashMap::new();
        // 两个都超门槛的可见区间（150 s / 300 s）：重叠时间区间里的切片边界必须分毫不差。
        // **cols = 1**：关掉"合并到像素"，比的是基准层的**原始 100 ms 切片**边界
        // （合并是按列数算的，两个视口的列数不同，合并宽度自然不同 —— 那不是"拉伸"）。
        let wide = segments_for_view(&base65, &empty, 600_000.0, 750_000.0, 1);
        let wider = segments_for_view(&base65, &empty, 500_000.0, 800_000.0, 1);
        assert_eq!(wide.layer, ViewLayer::Base, "两个视口都超门槛 → 都走基准层");
        assert_eq!(wider.layer, ViewLayer::Base);
        let overlaps = |spans: &[(f64, f64)]| -> Vec<(f64, f64)> {
            spans
                .iter()
                .copied()
                .filter(|(s, e)| *s >= 700_000.0 && *e <= 720_000.0)
                .collect()
        };
        let c = overlaps(&wide.bucket_spans());
        let d = overlaps(&wider.bucket_spans());
        assert!(!c.is_empty() && !d.is_empty());
        assert_eq!(
            c, d,
            "基准层切片的时间边界只由 100 ms 网格决定，与可见区间无关（否则就是被拉伸了）"
        );

        // ---- 门槛之内一侧：6 s 视口走密集层，切片宽恒为档位（按时间对齐） ----
        let r6 = segments_for_view(&base65, &tiles_covering(10, 600_000, 606_000), 600_000.0, 606_000.0, 1024);
        assert_eq!(r6.layer, ViewLayer::Dense, "6 s <= 门槛 → 密集层（P20 的核心变化）");
        assert_eq!(r6.bucket_ms, 10, "6 s / 600 = 10 ms/桶，正好命中 10 ms 这一档");
        assert_eq!(r6.segs.len(), 600, "6 s / 10 ms = 600 段");
        let (bs, be) = (r6.segs[0].start_ms, r6.segs[0].end_ms);
        assert!((be - bs - 10.0).abs() < 1e-9, "6 s 视口的切片宽必须是 10 ms，实测 {}", be - bs);
        assert!(
            r6.segs.iter().all(|s| (s.end_ms - s.start_ms - 10.0).abs() < 1e-9),
            "所有密集段的宽度都必须等于档位（按时间对齐，不随视口拉伸）"
        );
        // 若实现是"整块拉伸"，6 s 视口里就会出现 100 ms 宽的段（把粗档的段铺满窄视口）；
        // 实测恒为 10 ms —— 段宽只由**档位**决定，绝不由视口宽度决定
        assert!(
            r6.segs.iter().all(|s| (s.end_ms - s.start_ms) < 50.0),
            "6 s 视口里不可能出现粗档才有的宽段"
        );
    }

    /// **P20 ⑤（数据侧）**：缩放/平移只改变"段的数量与时间窗"，**不触发任何每帧重算**。
    ///
    /// 这条在纯逻辑层能钉住的部分：同样的输入 → 同样的输出（确定性）；
    /// 界面层的"不自激重绘"由 `ui::editor` 的 headless 测试钉住
    /// （`p20_zooming_and_panning_do_not_self_excite_repaints`）。
    #[test]
    fn p20_segments_are_deterministic_for_the_same_view() {
        let dur = 600_000i64;
        let base = full_base(dur);
        let tier = tier_for_visible_ms(30_000.0);
        let tiles = tiles_covering(tier, 0, 30_000);
        let first = segments_for_view(&base, &tiles, 0.0, 30_000.0, 1024);
        for _ in 0..3 {
            let again = segments_for_view(&base, &tiles, 0.0, 30_000.0, 1024);
            assert_eq!(first, again, "同样的可见区间必须给出完全一样的段（无随机/无累积状态）");
        }
    }

    /// **P20 ③（封顶无空洞）**：可见区间短到 [`MIN_BUCKET_MS`] 封顶时，
    /// 600 个切片**首尾相接、无空洞**（缩到最深也要看到完整波形）。
    #[test]
    fn p20_floor_is_gap_free_at_one_millisecond() {
        let dur = 60_000i64;
        let base = full_base(dur);
        // 可见 600 ms → 每片 1 ms → 恰好 600 个切片，全在第 0 片瓦片里
        let tiles = tiles_covering(MIN_BUCKET_MS, 0, 600);
        let r = segments_for_view(&base, &tiles, 0.0, 600.0, 1024);
        assert_eq!(r.layer, ViewLayer::Dense);
        assert_eq!(r.bucket_ms, MIN_BUCKET_MS);
        assert_eq!(r.segs.len(), VISIBLE_BUCKETS, "600 ms / 1 ms = 600 段");
        assert!(r.segs.iter().all(|s| s.detail), "全部来自密集解码（不是基准层兜底）");
        let mut cursor = 0.0f64;
        for s in &r.segs {
            assert!(
                (s.start_ms - cursor).abs() < 1e-9,
                "封顶档位下段与段必须首尾相接：期望 {cursor}，实测 {}",
                s.start_ms
            );
            assert!((s.end_ms - s.start_ms - 1.0).abs() < 1e-9, "每段恒为 1 ms");
            cursor = s.end_ms;
        }
        assert!((cursor - 600.0).abs() < 1e-9, "最后一段必须铺到可见区间右端");
        assert!(r.refine_ms.is_none() && r.pending_ms.is_none(), "无缺口 → 不请求细化");

        // 更短的可见区间（60 ms）：桶宽仍封顶在 1 ms → 只有 60 段，但同样无缝
        let tiles = tiles_covering(MIN_BUCKET_MS, 0, 60);
        let r2 = segments_for_view(&base, &tiles, 0.0, 60.0, 1024);
        assert_eq!(r2.segs.len(), 60);
        let mut cursor = 0.0f64;
        for s in &r2.segs {
            assert!((s.start_ms - cursor).abs() < 1e-9);
            cursor = s.end_ms;
        }
        assert!((cursor - 60.0).abs() < 1e-9);
    }

    /// **P20 ①（缺口/细化一侧）**：可见 120 s、片还没到时，
    /// **每一个没算到的片都进缺口区间 + 用基准层兜底**（不能留空），
    /// 且护栏 [`
    /// MAX_DENSE_TILES_PER_VIEW`] 容得下门槛处的 24~25 片。
    #[test]
    fn p20_threshold_view_reports_gaps_and_stays_within_the_tile_guard() {
        let dur = 600_000i64;
        let base = full_base(dur);
        let empty = std::collections::HashMap::new();
        let r = segments_for_view(&base, &empty, 0.0, DENSE_MAX_VISIBLE_MS, 1024);
        assert_eq!(r.layer, ViewLayer::Dense);
        assert_eq!(r.bucket_ms, 200);
        assert!(!r.segs.is_empty());
        assert!(
            r.segs.iter().any(|s| !s.detail),
            "一片瓦片都没有时必须用基准层兜底（不许留空）"
        );
        let (a, b) = r.refine_ms.expect("必须报出缺口区间以触发细化请求");
        assert_eq!((a, b), (0, DENSE_MAX_VISIBLE_MS as i64));
        assert_eq!(r.pending_ms, Some(0), "渐进线画在缺口起点");
        // 门槛处的片数上界：末片 < 首片 + 护栏（护栏必须是完备的，否则会有一部分永远不细化）
        let tiles = (tile_of_ms(DENSE_MAX_VISIBLE_MS as i64) - tile_of_ms(0)) + 1;
        assert!(
            tiles <= MAX_DENSE_TILES_PER_VIEW,
            "门槛处需要 {tiles} 片，护栏只有 {MAX_DENSE_TILES_PER_VIEW} 片"
        );
        // 反过来：既然护栏容得下，把片都算满之后缺口必须**消失**（证明护栏没有截断覆盖）
        let tiles_map = tiles_covering(200, 0, DENSE_MAX_VISIBLE_MS as i64);
        let full = segments_for_view(&base, &tiles_map, 0.0, DENSE_MAX_VISIBLE_MS, 1024);
        assert!(
            full.segs.iter().all(|s| s.detail),
            "护栏内的片算满之后，可见区间必须全部由密集层覆盖（护栏没有截断）"
        );
        assert!(full.refine_ms.is_none());
    }

    /// **P20 ③（密集路径逐包解满）**：从"可见区间"这条链路走出来的段，
    /// 全部带 `dense` 数据凭据，**不经过**稀疏窗口路径。
    ///
    /// 数据证据（与 `dense_tiles_never_come_from_the_sparse_window_path` 同一条口径）：
    /// 密集路径能在一个 100 ms 的可见区间里折出 100 个 1 ms 桶；
    /// 而**基准层**是另一条独立口径 —— 切片宽恒为 [`BASE_BUCKET_MS`] = 100 ms，
    /// 不管可见区间怎么变（这正是 P22 用"常量切片宽"取代"总时长/1024 变宽切片"的理由）。
    #[test]
    fn p20_dense_slices_decode_every_packet_not_a_sparse_window() {
        // 100 ms 的音频 → 1 ms 档 → 100 个桶
        let n = (48 * 100) as usize;
        let t = scan_to_tile(
            MIN_BUCKET_MS,
            0,
            ChunkRange { start_ms: 0, len: n },
            &vec![0.6f32; n],
        );
        assert!(t.dense, "密集瓦片必须自带 dense 标志");
        assert_eq!(t.peaks.len(), 100, "100 ms / 1 ms = 100 个桶");
        assert!(t.peaks.iter().all(|p| p.max > 0.59), "每个桶都来自真解码的样本");
        // **基准层一侧**：切片宽恒为 100 ms，与时长/视口都无关
        assert_eq!(BASE_BUCKET_MS, 100, "基准层切片宽恒为 100 ms（两条路径分开）");
        for dur in [60_000i64, 3_600_000, 21_600_000] {
            let b = BaseLayer::empty(dur);
            assert_eq!(b.bucket_ms, BASE_BUCKET_MS);
            assert_eq!(b.span_of(1).0 - b.span_of(0).0, BASE_BUCKET_MS);
        }
        // 段级：把可见区间铺满密集瓦片后，**没有一个**兜底段混进来
        let base = full_base(60_000);
        let tiles = tiles_covering(MIN_BUCKET_MS, 0, 600);
        let r = segments_for_view(&base, &tiles, 0.0, 600.0, 1024);
        assert!(r.segs.iter().all(|s| s.detail), "可见区间里不许混进兜底段");
    }

    /// **P20 ⑥**：坏输入（NaN / 倒置 / 零时长 / 超长可见区间）一律安全降级，绝不 panic。
    #[test]
    fn p20_bad_inputs_degrade_without_panic() {
        // 空基准层（一片都没算）：任何可见区间都给出空结果
        let empty_base = BaseLayer::empty(0);
        let tiles = std::collections::HashMap::new();
        for (a, b) in [
            (0.0, DENSE_MAX_VISIBLE_MS),
            (0.0, DENSE_MAX_VISIBLE_MS * 100.0),
            (f64::NAN, 1.0),
            (5.0, 1.0),
            (-1e12, 1e12),
        ] {
            let r = segments_for_view(&empty_base, &tiles, a, b, 1024);
            if r.layer == ViewLayer::Dense {
                assert!(r.segs.is_empty(), "空基准层上不该凭空造出密集段");
            }
            let _ = r.segs.len();
        }
        // 时长 0 的概览 + 门槛内的可见区间：分段逻辑仍然不 panic、且不越界
        let base0 = BaseLayer::empty(0);
        let r = segments_for_view(&base0, &tiles, 0.0, 10_000.0, 1024);
        assert!(r.segs.is_empty());
        // 可见区间超出片尾：夹到片尾，段不越界（150 s > 门槛 → 基准层）
        let base = full_base(600_000);
        let r = segments_for_view(&base, &tiles, 50_000.0, 200_000.0, 1024);
        assert_eq!(r.layer, ViewLayer::Base, "可见 150 s 已超门槛 → 走基准层");
        assert!(!r.segs.is_empty(), "被夹到片尾之后仍要有东西可画");
        for s in &r.segs {
            assert!(s.start_ms >= 0.0 && s.end_ms <= 600_000.0 + 1.0, "段不得越出片长");
        }
        // 极长可见区间（门槛的 100 倍）：仍是基准层，且不尝试密集解码
        let r = segments_for_view(&base, &tiles, 0.0, DENSE_MAX_VISIBLE_MS * 100.0, 1024);
        assert_eq!(r.layer, ViewLayer::Base);
        assert!(r.refine_ms.is_none());
    }

    /// 非法参数（NaN / 倒置区间）→ 空结果，绝不 panic。
    #[test]
    fn segments_with_bad_input_are_empty_not_panic() {
        let base = BaseLayer::empty(1000);
        let tiles = std::collections::HashMap::new();
        for (t0, t1) in [(f64::NAN, 10.0), (10.0, 5.0), (10.0, 10.0), (f64::NAN, f64::NAN)] {
            let r = segments_for_view(&base, &tiles, t0, t1, 1024);
            assert!(r.segs.is_empty(), "非法输入必须给出空结果（{t0}..{t1}）");
        }
    }

    // ---------------- 纯逻辑：瓦片 LRU ----------------

    /// **LRU 上限 64 片**，淘汰最久未用的那一片；`get` 会刷新"最近使用"。
    #[test]
    fn tile_cache_evicts_least_recently_used_at_capacity() {
        let mk = |t: usize| DetailTile {
            tile: t,
            start_ms: t as i64 * TILE_MS,
            bucket_ms: MIN_BUCKET_MS,
            peaks: vec![Peak { min: 0.0, max: 0.1 }],
            complete: true,
            dense: true,
        };
        let mut c = TileCache::new();
        assert!(c.is_empty());
        for t in 0..TILE_CACHE_MAX {
            c.insert(mk(t));
        }
        assert_eq!(c.len(), TILE_CACHE_MAX);
        // 把第 0 片"用"一次（让它变成最近使用），再插一片新的
        assert!(c.get(0).is_some());
        c.insert(mk(999));
        assert_eq!(c.len(), TILE_CACHE_MAX, "上限必须守住，不能无限增长");
        assert!(c.peek(0).is_some(), "刚用过的第 0 片不该被淘汰");
        assert!(c.peek(1).is_none(), "最久未用的第 1 片应被淘汰");
        assert!(c.peek(999).is_some());
        // 超过上限很多：长度仍然等于上限
        for t in 1000..1200 {
            c.insert(mk(t));
        }
        assert_eq!(c.len(), TILE_CACHE_MAX);
    }

    // ---------------- 纯逻辑：落盘缓存 ----------------

    /// 编码 → 解码往返：所有字段原样回来。
    #[test]
    fn p22_base_disk_roundtrip_preserves_every_field() {
        let mut b = BaseLayer::empty(600_000);
        // 造一份"两段算完、其余没算"的半成品：位图与摘要都要原样回来
        b.write_slices(0, &vec![Peak { min: -0.25, max: 0.75 }; 3000]);
        let bytes = encode_base(&b).expect("编码不该失败");
        let back = decode_base(&bytes).expect("自己的编码必须解得回来");
        assert_eq!(back.duration_ms, b.duration_ms);
        assert_eq!(back.bucket_ms, BASE_BUCKET_MS);
        assert_eq!(back.slice_count(), b.slice_count());
        assert_eq!(back.done_slices, b.done_slices);
        assert_eq!(back.count_done(), b.count_done());
        assert_eq!(back.peaks, b.peaks);
        assert_eq!(back.done, b.done);
        assert_eq!(back.failed_segments, b.failed_segments);
        // 完全算完的一层：往返一致
        let full = full_base(120_000);
        let back = decode_base(&encode_base(&full).unwrap()).unwrap();
        assert!(back.is_complete());
        assert_eq!(back.peaks, full.peaks);
    }

    /// **损坏即当未命中**：magic / 版本 / 长度 / 取值任一不对 → `None`，绝不 panic。
    #[test]
    fn p22_base_disk_rejects_corruption_without_panicking() {
        let mut b = BaseLayer::empty(10_000);
        b.write_slices(0, &vec![Peak { min: -0.5, max: 0.5 }; 100]);
        let good = encode_base(&b).unwrap();
        let hdr = CACHE_HEADER_LEN;

        // ① 截断（写盘写了一半）
        for cut in [0usize, 1, 8, 12, 25, hdr, good.len() - 1] {
            assert!(decode_base(&good[..cut]).is_none(), "截断到 {cut} 必须判为未命中");
        }
        // ② magic 错
        let mut bad = good.clone();
        bad[0] ^= 0xFF;
        assert!(decode_base(&bad).is_none());
        // ③ 版本错（换算法之后绝不能读旧口径的数据）
        let mut bad = good.clone();
        bad[8..12].copy_from_slice(&(ALGO_VERSION.wrapping_add(1)).to_le_bytes());
        assert!(decode_base(&bad).is_none());
        // ④ 切片宽错（改了 BASE_BUCKET_MS 却忘了改版本号时的兜底）
        let mut bad = good.clone();
        bad[20..28].copy_from_slice(&(BASE_BUCKET_MS * 2).to_le_bytes());
        assert!(decode_base(&bad).is_none());
        // ⑤ 峰值里塞 NaN / inf / 越界值
        for evil in [f32::NAN, f32::INFINITY, 9.0] {
            let mut bad = good.clone();
            bad[hdr..hdr + 4].copy_from_slice(&evil.to_le_bytes());
            assert!(decode_base(&bad).is_none(), "峰值 {evil} 必须判为未命中");
        }
        // ⑥ 负数时长
        let mut bad = good.clone();
        bad[12..20].copy_from_slice(&(-1i64).to_le_bytes());
        assert!(decode_base(&bad).is_none());
        // ⑦ 垃圾字节（长度对但内容随机）
        let junk = vec![0x5Au8; good.len()];
        assert!(decode_base(&junk).is_none());
        // ⑧ 完全空的文件
        assert!(decode_base(&[]).is_none());
        // ⑨ 位图与摘要自相矛盾（把 done 摘要改大）→ 当未命中，绝不"信一个自相矛盾的缓存"
        let mut bad = good.clone();
        bad[32..36].copy_from_slice(&(u32::MAX / 2).to_le_bytes());
        assert!(decode_base(&bad).is_none());
    }

    /// 磁盘缓存目录为 `None` 时读写都是 no-op（拿不到 Cache 目录也要能正常工作）。
    #[test]
    fn disk_cache_without_dir_is_a_noop() {
        let c = DiskCache::new(None);
        let key = CacheKey::of(Path::new("没有这个文件.mp4"));
        assert!(c.dir().is_none());
        assert!(c.load(&key, 1000).is_none());
        c.save(&key, &BaseLayer::empty(1000)); // 不许 panic
    }

    /// **缓存键**：路径 / 大小 / 修改时间 / 算法版本，任一变化都要得到不同的键与文件名。
    #[test]
    fn cache_key_changes_with_any_input_and_hashes_to_a_stable_name() {
        let key = CacheKey {
            path: PathBuf::from(r"C:\a\b.mp4"),
            mtime_ns: 111,
            size: 42,
            algo: ALGO_VERSION,
        };
        let other_path = CacheKey {
            path: PathBuf::from(r"C:\a\c.mp4"),
            ..key.clone()
        };
        let other_size = CacheKey {
            size: 43,
            ..key.clone()
        };
        let other_mtime = CacheKey {
            mtime_ns: 112,
            ..key.clone()
        };
        let other_algo = CacheKey {
            algo: ALGO_VERSION + 1,
            ..key.clone()
        };
        assert_ne!(key, other_path);
        assert_ne!(key, other_size);
        assert_ne!(key, other_mtime);
        assert_ne!(key, other_algo);
        // 同一个键在同一台机器上必须给出同一个文件名（否则缓存永远命不中）
        assert_eq!(key.file_name(), key.clone().file_name());
        assert_ne!(key.file_name(), other_size.file_name());
        assert!(key.file_name().starts_with("wave-base-"));
        assert!(key.file_name().ends_with(".bin"));
    }

    // ---------------- 真解码（需要随包 ffmpeg 生成素材） ----------------

    /// **单测用的段长**（毫秒）：比生产的 [`SEGMENT_MS`]（8 分钟）小得多，
    /// 于是**中等长度的素材**也能切出很多段，把"段边界 + priming 补偿"这条真的覆盖到
    /// （用户允许的口径："造不出长素材就用中等素材 + 多段覆盖同一条断言"）。
    ///
    /// 取 5 s 而不是 20 s 是有意的：65 s 的素材切出 **13 段 / 12 个内边界** ——
    /// 只要边界算错哪怕一个切片，就有 12/650 ≈ 1.8% 的切片不一致，
    /// 正好越过"≥99% 完全一致"这条线（3 个边界是抓不住的）。
    const TEST_SEG_MS: i64 = 5_000;

    /// **只生成音轨的测试素材**（基准层测试用）：没有视频流，编码比带视频的素材快一个数量级。
    ///
    /// `envelope` = true 时给 440 Hz 正弦叠一个 **3 Hz 的幅度包络**：
    /// 这样**每一个 100 ms 切片的峰值都不一样**，于是"段边界算错几毫秒"会真的改变切片峰值；
    /// 纯正弦则不行（任何窗口的 min/max 都是 ±amp，错位也看不出来）。
    fn gen_audio_asset(name: &str, secs: u32, amp: f64, envelope: bool) -> Option<PathBuf> {
        let ff = ffmpeg_exe()?;
        let out = asset_dir().join(name);
        let _ = std::fs::remove_file(&out);
        let mut cmd = Command::new(&ff);
        cmd.args(["-y", "-hide_banner", "-loglevel", "error"]);
        if envelope {
            let e = format!("{amp}*sin(2*PI*440*t)*(0.15+0.85*sin(2*PI*3*t)*sin(2*PI*3*t))");
            let src = format!("aevalsrc={e}|{e}:s=48000:d={secs}");
            cmd.args(["-f", "lavfi", "-i", &src, "-c:a", "aac", "-b:a", "128k"]);
        } else {
            let src = format!("sine=frequency=440:sample_rate=48000:duration={secs}");
            let gain = (amp / SINE_DEFAULT_PEAK).max(0.0001);
            let filter = format!("volume={gain:.6},pan=stereo|c0=c0|c1=c0");
            cmd.args(["-f", "lavfi", "-i", &src, "-af", &filter, "-c:a", "aac", "-b:a", "128k"]);
        }
        cmd.arg(&out);
        match cmd.output() {
            Ok(o) if o.status.success() && out.is_file() => Some(out),
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                let lower = err.to_ascii_lowercase();
                if lower.contains("unknown encoder") || lower.contains("openh264") {
                    println!("[skip] 随包 ffmpeg 不具备所需编码能力：{err}");
                    None
                } else {
                    panic!("生成音轨素材 {name} 失败（别静默跳过）：{err}");
                }
            }
            Err(e) => panic!("无法运行随包 ffmpeg：{e}"),
        }
    }

    /// **真解码 + 全片基准层（单线程）**：48 kHz 立体声 AAC 素材 → 100 ms 一格的基准层，
    /// 峰值量级正确、整片补齐、编码名与采样率都对。
    #[test]
    fn p22_compute_base_single_thread_fills_the_whole_file() {
        let Some(src) = gen_audio_asset("wave-base-tone.mp4", 3, 0.5, false) else {
            return;
        };
        let dur = 3_000i64;
        let cancel = Arc::new(AtomicBool::new(false));
        let (base, codec, rate, _ch) = compute_base_sync(&src, dur, 1, SEGMENT_MS, &cancel)
            .expect("3 秒的有音轨素材必须能算出基准层");
        assert!(base.has_peaks(), "有声素材必须解出峰值");
        assert_eq!(base.slice_count(), 30, "3 s / 100 ms = 30 片");
        assert!(base.is_complete(), "整片都要补齐");
        assert_eq!(base.failed_segments, 0, "正常素材不该有失败的段");
        assert!(codec.contains("aac"), "编码名应含 aac，实际 {codec}");
        assert_eq!(rate, 48_000);
        // 峰值量级：素材是"峰值 0.5 的 440 Hz 正弦"（见 `SINE_DEFAULT_PEAK` 处对 `sine`
        // 默认幅度的折算），但 AAC 是**有损**的：编解码过冲实测让它落到 0.72 左右。
        // 所以判据只问"是不是同一个量级"。
        let max = base.peaks.iter().fold(0.0f32, |acc, p| acc.max(p.max.abs()));
        assert!(
            max > 0.3 && max < 0.95,
            "峰值量级异常：{max}（素材峰值 0.5 + AAC 过冲，期望 0.5~0.8 量级）"
        );
    }

    /// **并行结果 == 单线程结果 == 整片顺序解**（P22 最关键的一条验证）。
    ///
    /// 三条路径、**同一套分桶**（100 ms/片），区别只在"怎么拿到采样点"：
    /// * **参照**（`compute_base_reference`）：**一次 seek 都不做**，从 0 号包顺序解到片尾；
    /// * **1 线程**：走同一套分段逻辑，但只有一条线程顺序跑完所有段；
    /// * **4 线程**：`WAVE_WORKERS` 条线程各自独立打开 demuxer + decoder、按段 seek + 预热。
    ///
    /// 断言（用户给的判据）：**逐片比较 min/max，≥99% 完全一致、其余在极小容差内**。
    /// 段边界算错 / AAC priming 补偿写错，交界处的切片就会成块地不一致 —— 这条会红。
    ///
    /// 素材 65 秒、段长 [`TEST_SEG_MS`] = 20 秒 ⇒ **4 段、3 个内边界**，
    /// "多段 + 段边界 + 首尾两段"这几种情况都被覆盖到。
    ///
    /// 灵敏度（实测）：把 seek 目标挪错 1 s ⟹ 519/650 = 79.9% 一致（这条立刻变红，
    /// 差异片恰好落在段边界 0~9 / 50~59 …）。而 [`AAC_PRIMING_MS`] / [`WARMUP_PACKETS`]
    /// 取 0 仍然通过 —— 因为**落位是按包 PTS 绝对定位的**，那两个常量是"seek 提前量 +
    /// 预热"的鲁棒性措施，不是"靠猜去平移数据"。
    ///
    /// 1 小时素材上的实测（`_rust\wave-bench`，2026 现场跑）：4 线程 vs 整片顺序解
    /// **35,960 / 36,000 片逐位相同（99.89%）**，其余 ≤ 1.6e-6；1 线程 vs 参照 **36,000/36,000 完全相同**。
    #[test]
    fn p22_parallel_matches_single_thread_slice_by_slice() {
        let Some(src) = gen_audio_asset("wave-base-parallel.mp4", 65, 0.5, true) else {
            return;
        };
        let dur = 65_000i64;
        let cancel = Arc::new(AtomicBool::new(false));
        // 前提：这一份素材必须真的被切成 ≥3 段（否则"段边界"根本没被覆盖）
        assert!(
            segment_count_of(dur, TEST_SEG_MS) >= 3,
            "时长 {dur} ms / 段长 {TEST_SEG_MS} ms 只有 {} 段（太少）",
            segment_count_of(dur, TEST_SEG_MS)
        );
        let reference =
            compute_base_reference(&src, dur, &cancel).expect("整片顺序解码必须成功");
        let (one, _, _, _) = compute_base_sync(&src, dur, 1, TEST_SEG_MS, &cancel)
            .expect("单线程分段解码必须成功");
        let (parallel, _, _, _) = compute_base_sync(&src, dur, WAVE_WORKERS, TEST_SEG_MS, &cancel)
            .expect("4 线程分段解码必须成功");
        let n = reference.len();
        assert_eq!(one.slice_count(), n, "切片数必须一致");
        assert_eq!(parallel.slice_count(), n, "切片数必须一致");
        assert!(one.is_complete() && parallel.is_complete(), "两条路径都要补齐");
        assert!(n > 30, "素材切出的片数太少（{n}），这条测试就没什么说服力");
        // 前提：素材必须让切片峰值**真的有变化**（否则"逐片一致"是一句废话）
        let uniq: std::collections::HashSet<(u32, u32)> = reference
            .iter()
            .map(|p| (p.min.to_bits(), p.max.to_bits()))
            .collect();
        assert!(
            uniq.len() > n / 4,
            "切片峰值过于单一（{} 种 / {n} 片），这条测试没有说服力",
            uniq.len()
        );
        // 三条路径逐片对比：≥99% 逐位一致，其余在极小容差内。
        //
        // 容差取 1e-4（= 相对 0.7 的幅度约 -77 dBFS）是**实测**出来的：
        // 1 小时的素材上，4 线程与整片顺序解有 35/36,000 片差到 1.6e-6
        //（= 十几个 ULP，来自重采样器/解码器在不同缓冲切分下的浮点累加差异，
        //  不是"边界错位"——边界错位的差异量级是 1e-2 ~ 1e0，而且会成片出现）。
        const TOL: f32 = 1e-4;
        for (label, layer) in [("1 线程", &one), ("4 线程", &parallel)] {
            let mut exact = 0usize;
            let mut close = 0usize;
            let mut bad: Vec<usize> = Vec::new();
            for (i, r) in reference.iter().enumerate() {
                if !layer.done[i] {
                    bad.push(i);
                    continue;
                }
                let (a, b) = (*r, layer.peaks[i]);
                if a == b {
                    exact += 1;
                } else if (a.min - b.min).abs() <= TOL && (a.max - b.max).abs() <= TOL {
                    close += 1;
                } else {
                    bad.push(i);
                }
            }
            let ratio = exact as f64 / n as f64;
            assert!(
                ratio >= 0.99,
                "{label} 与整片顺序解的切片必须 ≥99% 完全一致：{exact}/{n} = {ratio:.4}\
                 （差异片：{:?}）",
                bad.iter().take(20).collect::<Vec<_>>()
            );
            assert!(
                exact + close == n,
                "{label} 的其余切片也必须落在极小容差内（差异片：{:?}）",
                bad.iter().take(20).collect::<Vec<_>>()
            );
        }
    }

    /// **段边界无可见断层**（P22 验收）：相邻段交界处不许出现"异常 0 / 极值"。
    ///
    /// 素材是一条连续的 440 Hz 正弦，任何一段边界处的切片峰值都必须**与邻域同量级**
    /// （既不塌成 0，也不冒出 >1 的极值）。这条正是"priming 补偿 / 段边界丢弃"写错的症状探测。
    #[test]
    fn p22_segment_boundaries_have_no_notch() {
        let Some(src) = gen_audio_asset("wave-base-boundary.mp4", 65, 0.5, false) else {
            return;
        };
        let dur = 65_000i64;
        let cancel = Arc::new(AtomicBool::new(false));
        let (base, _, _, _) = compute_base_sync(&src, dur, WAVE_WORKERS, TEST_SEG_MS, &cancel)
            .expect("并行解码必须成功");
        assert!(base.is_complete());
        let n = base.slice_count();
        let amp = |i: usize| -> f32 { base.peaks[i].max.abs().max(base.peaks[i].min.abs()) };
        // 整片的峰值中位数（用中位数而不是均值：个别切片可以是静音/换气，不该影响判据）
        let mut all: Vec<f32> = (0..n).map(amp).collect();
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = all[n / 2];
        assert!(median > 0.3, "素材本身太轻（中位数 {median}），这条测试没有意义");
        // 逐段交界：交界处前后各 20 片（= ±2 s）都必须与中位数同量级
        for seg in 1..segment_count_of(dur, TEST_SEG_MS) {
            let (start_slice, _) = segment_slice_range_of(seg, dur, TEST_SEG_MS);
            for k in start_slice.saturating_sub(20)..(start_slice + 20).min(n) {
                let v = amp(k);
                assert!(
                    v > median * 0.2,
                    "第 {seg} 段边界附近的切片 {k} 塌成 {v}（中位数 {median}）—— 段边界丢音频了"
                );
                assert!(
                    v <= 1.001,
                    "第 {seg} 段边界附近的切片 {k} 冒出极值 {v} —— 段边界重复计入/未限幅"
                );
            }
        }
    }

    /// **可见区间最优先**（P22 验收）：打开后先解可见区间所在的那几段。
    ///
    /// 做法：把基准层 worker 真跑起来，**在"可见段刚算完"的那一刻**取一份状态快照，
    /// 断言"此刻算完的段里没有离可见区间很远的"。更稳的纯逻辑版本在
    /// `p22_visible_window_finishes_before_far_segments`，
    /// 这条补的是"真的起线程 + 真的解码 + 真的取消"这一层。
    ///
    /// ⚠️ **为什么快照必须在 `wake` 回调里取，而不是轮询 `states()`**（这条测试曾经
    /// 5 次里挂 4 次）：`wake()` 是 worker 写完之后**同步**调用的，此刻段号刚变成 2；
    /// 而外部轮询要经过"睡 2 ms + 被调度"两跳，这期间同一批 worker 可能已经把下一段
    /// （更远的那一段）也算完了 —— 于是断言读到"远处也完成了"，其实是**观测太晚**，
    /// 不是优先级失效。`wake` 里取快照 + 立刻置取消，窗口就只有一次快照的宽度。
    ///
    /// ⚠️ **断言口径（2026-10-19 修订，旧断言在全量并行跑时偶发挂过）**：
    /// 旧断言"可见段算完的那一瞬间，远处段一个都不该算完"其实**过强** ——
    /// `take_next` 保证的是**领取顺序**按距离排序，而 4 个 worker 的**算完顺序**
    /// 受调度影响可以乱序（机器满载时远处段先算完完全可能，优先级并没有失效）。
    /// 现在换成两条**由构造保证、不靠运气**的性质：
    /// 1. **领取不变式**：任何已开跑（state ≥ 1）的远处段，比它近的段必须都已开跑；
    /// 2. **第一个算完的段**必在"离可见区间最近的 [`WAVE_WORKERS`] 段"里
    ///    （第 5 次领取必须等某个 worker 先算完一段腾出手，所以第一个算完的
    ///    不可能出自第 5 次以后的领取）。
    #[test]
    fn p22_visible_segment_is_computed_first_with_real_workers() {
        let Some(src) = gen_audio_asset("wave-base-visible.mp4", 65, 0.5, false) else {
            return;
        };
        let dur = 65_000i64;
        let plan = Arc::new(BasePlan::with_segment_ms(dur, TEST_SEG_MS));
        // 视口固定在最后 5 秒（第 12 段 = 60~65 s）
        let (va, vb) = (60_000i64, 65_000i64);
        plan.set_visible(va, vb);
        let want = segment_of_ms_of(va, dur, TEST_SEG_MS);
        assert_eq!(want, 12, "前提：65 s 的素材按 5 s 分段，最后一段是 12");
        let cancel = Arc::new(AtomicBool::new(false));
        // 可见段一算完就在**同一时刻**取快照并叫停（见上面的 ⚠️）
        let captured: Arc<std::sync::Mutex<Option<Vec<u8>>>> =
            Arc::new(std::sync::Mutex::new(None));
        // 算完顺序：每次 wake 增量记录"刚变成 2"的段号（用来断言第一个算完的是谁）。
        // `seen` 初值全 0，长度 = 段数。
        let seen: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(vec![
            0u8;
            plan.segment_count()
        ]));
        let done_seq: Arc<std::sync::Mutex<Vec<usize>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let wake: Arc<dyn Fn() + Send + Sync> = {
            let plan = plan.clone();
            let cancel = cancel.clone();
            let captured = captured.clone();
            let seen = seen.clone();
            let done_seq = done_seq.clone();
            Arc::new(move || {
                let st = plan.states();
                {
                    let mut s = match seen.lock() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    let mut seq = match done_seq.lock() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    for (i, &v) in st.iter().enumerate() {
                        if v == 2 && s[i] != 2 {
                            seq.push(i);
                            s[i] = 2;
                        }
                    }
                }
                if st.get(want) != Some(&2) {
                    return;
                }
                let mut g = match captured.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                if g.is_none() {
                    *g = Some(st);
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            })
        };
        let mut hs = Vec::new();
        for _ in 0..WAVE_WORKERS {
            let p = src.clone();
            let plan = plan.clone();
            let cancel = cancel.clone();
            let wake = wake.clone();
            hs.push(std::thread::spawn(move || {
                base_worker_loop(&p, &plan, &cancel, wake.as_ref());
            }));
        }
        // 等"可见段算完"这个事件（它由 wake 回调写进 `captured`）
        let t_wait = std::time::Instant::now();
        let states = loop {
            if let Some(st) = captured
                .lock()
                .map(|g| g.clone())
                .unwrap_or_else(|e| e.into_inner().clone())
            {
                break st;
            }
            assert!(
                t_wait.elapsed() < std::time::Duration::from_secs(60),
                "可见区间所在的第 {want} 段没有在预算内算完（states={:?}）",
                plan.states()
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        // 段区间与可见区间的距离（重叠/相邻 = 0；和 take_next 的口径一致）。
        let dist = |i: usize| -> i64 {
            let (a, b) = segment_span_ms_of(i, dur, TEST_SEG_MS);
            if b <= va {
                va - b
            } else if a >= vb {
                a - vb
            } else {
                0
            }
        };
        // ── 断言 1（领取不变式）────────────────────────────────
        // take_next 永远挑"离可见区间最近"的待算段，所以任何已开跑的远处段，
        // 比它近的段一定早已开跑。这条只依赖"领取顺序 + 状态单调（0→1→2）"，
        // 与算完顺序无关，机器满载也不该挂。
        for i in 0..states.len() {
            if states[i] == 0 {
                continue; // 没开跑的段不参与
            }
            let di = dist(i);
            if di <= 2 * TEST_SEG_MS {
                continue; // 只查"远处"（离可见区间超过 2 段的）
            }
            for j in 0..states.len() {
                if j != i && dist(j) < di {
                    assert!(
                        states[j] >= 1,
                        "远处段 {i}（距可见区间 {di} ms）已经开跑，但更近的段 {j}\
                         （距 {} ms）还没开始 —— 领取顺序没有按距离排序；states={states:?}",
                        dist(j)
                    );
                }
            }
        }
        // ── 断言 2（第一个算完的段）────────────────────────────
        // 4 个 worker ⇒ 头 4 次领取就是"最近的 4 段"；第 5 次领取必须等某个
        // worker 先算完一段腾出手。所以第一个算完的段不可能出自第 5 次以后的领取。
        let seq = match done_seq.lock() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        };
        let mut by_dist: Vec<(i64, usize)> =
            (0..states.len()).map(|i| (dist(i), i)).collect();
        by_dist.sort_unstable();
        let nearest: Vec<usize> = by_dist
            .iter()
            .take(WAVE_WORKERS)
            .map(|&(_, i)| i)
            .collect();
        assert!(
            seq.first().is_some_and(|s| nearest.contains(s)),
            "第一个算完的是 {:?}，不在最近的 {WAVE_WORKERS} 段 {nearest:?} 里 ——\
             可见区间没有最优先；算完顺序={seq:?}",
            seq.first()
        );
        // 取消（wake 里已经置过一次）之后，4 个 worker 必须都能停下来
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        for h in hs {
            let _ = h.join();
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(30),
            "取消之后 4 个 worker 必须在有限时间内全部退出（实测 {:?}）",
            t0.elapsed()
        );
    }

    /// **取消（真线程 + 看门狗）**：置取消之后 4 个 worker 必须都能停下来，绝不挂住。
    #[test]
    fn p22_workers_stop_after_cancel_within_a_watchdog() {
        let Some(src) = gen_audio_asset("wave-base-cancel.mp4", 65, 0.5, false) else {
            return;
        };
        let dur = 65_000i64;
        let plan = Arc::new(BasePlan::with_segment_ms(dur, TEST_SEG_MS));
        let cancel = Arc::new(AtomicBool::new(false));
        let nop = |_: ()| {};
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || nop(()));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("p22-cancel-watchdog".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let mut hs = Vec::new();
                for _ in 0..WAVE_WORKERS {
                    let p = src.clone();
                    let plan = plan.clone();
                    let cancel = cancel.clone();
                    let wake = wake.clone();
                    hs.push(std::thread::spawn(move || {
                        base_worker_loop(&p, &plan, &cancel, wake.as_ref());
                    }));
                }
                // 跑一小会儿（保证真的在解码），然后置取消
                std::thread::sleep(std::time::Duration::from_millis(50));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                for h in hs {
                    let _ = h.join();
                }
                let _ = tx.send(());
                let _ = t0.elapsed();
            })
            .expect("起看门狗线程");
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(30)).is_ok(),
            "取消之后 4 个 worker 在 30 s 内没有全部退出 —— 挂住了（死锁/忙等）"
        );
    }

    /// **基准层的落盘缓存**：第一次 miss 并落盘，第二次 hit 且内容一致；
    /// 损坏的缓存当未命中重算，**绝不 panic**。
    #[test]
    fn p22_base_disk_cache_miss_then_hit() {
        let Some(src) = gen_audio_asset("wave-base-cache.mp4", 3, 0.5, false) else {
            return;
        };
        let dir = asset_dir().join(format!("base-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = DiskCache::new(Some(dir.clone()));
        let key = CacheKey::of(&src);
        // 第一次：真算 → 落盘
        let cancel = Arc::new(AtomicBool::new(false));
        let (base, _, _, _) = compute_base_sync(&src, 3_000, 1, SEGMENT_MS, &cancel).unwrap();
        assert!(base.is_complete());
        disk.save(&key, &base);
        let file = dir.join(key.file_name());
        assert!(file.is_file(), "基准层必须落盘到 {}", file.display());
        // 第二次：命中缓存，内容逐片一致
        let hit = disk.load(&key, 3_000).expect("第二次必须命中缓存");
        assert_eq!(hit.peaks, base.peaks, "命中后数据必须一致");
        assert_eq!(hit.done, base.done);
        // 不完整的层不落盘（半成品没有意义）
        let mut half = BaseLayer::empty(3_000);
        half.write_slices(0, &[Peak::ZERO; 10]);
        disk.save(&key, &half);
        let again = disk.load(&key, 3_000).expect("完整的层还在（半成品没覆盖它）");
        assert!(again.is_complete());
        // 损坏的缓存必须当未命中（把文件截断）
        let bytes = std::fs::read(&file).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() / 2]).unwrap();
        assert!(disk.load(&key, 3_000).is_none(), "损坏的缓存必须判为未命中并重算");
        // 时长对不上也算未命中
        std::fs::write(&file, &bytes).unwrap();
        assert!(disk.load(&key, 3_001).is_none(), "时长对不上必须判为未命中");
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// **取消**：`emit` 返回 false 必须在单次有界解码里立刻停手（且已解出的部分可用）。
    #[test]
    fn decode_can_be_cancelled_midway() {
        let Some(src) = gen_asset("wave-cancel.mp4", Some((440, 5, 0.5))) else {
            return;
        };
        let mut chunks = 0usize;
        let mut got = 0usize;
        let (reason, _, _, _) = decode_range(&src, 0, 5_000, |c, _at| {
            chunks += 1;
            got += c.len();
            chunks < 3 // 第 3 段就取消
        })
        .unwrap();
        assert_eq!(reason, EndReason::Cancelled);
        assert_eq!(chunks, 3);
        assert!(got > 0, "取消之前必须先交出数据（不能什么都不给）");
    }

    /// **有界解码**：只解 1 秒时，交出的样本数必须**大致就是 1 秒**
    /// （不能"顺手把整条 5 秒的片子解完" —— 那是"成本与片长有关"的老毛病）。
    #[test]
    fn decode_range_is_bounded_by_the_requested_window() {
        let Some(src) = gen_asset("wave-bounded.mp4", Some((440, 5, 0.5))) else {
            return;
        };
        let mut total = 0usize;
        let (_r, _, _, _) = decode_range(&src, 0, 1_000, |c, _at| {
            total += c.len();
            true
        })
        .unwrap();
        // 一秒 48000 个采样点；允许多解一个音频包的余量（AAC 一包 1024 点）
        assert!(
            (48_000..=48_000 + 4096).contains(&total),
            "1 秒的有界解码交出了 {total} 个采样点（期望 ≈48000）"
        );
    }

    /// **seek + 有界解码**：从区间中间取一小段，交出的起点必须**落在请求位置附近**
    /// （这是"放大到某区间 → seek 过去密集解一段"的时间对齐前提）。
    #[test]
    fn decode_range_from_the_middle_starts_near_the_request() {
        let Some(src) = gen_asset("wave-seek.mp4", Some((440, 20, 0.5))) else {
            return;
        };
        let mut first_at: Option<i64> = None;
        let mut total = 0usize;
        let (_r, _, _, _) = decode_range(&src, 12_000, 12_500, |c, at| {
            if first_at.is_none() {
                first_at = Some(at.start_ms);
            }
            total += c.len();
            true
        })
        .unwrap();
        let at = first_at.expect("必须交出至少一段");
        assert!(
            (at - 12_000).abs() <= 60,
            "seek 后第一段的起点 {at} 偏离请求的 12000 ms 超过 60 ms（一个关键包）"
        );
        assert!(
            (24_000..=24_000 + 4096).contains(&total),
            "0.5 秒的窗口交出了 {total} 个采样点（期望 ≈24000）"
        );
    }

    /// **密集瓦片**：真解码一片 5 秒瓦片，桶数/完整性/峰值量级都对。
    #[test]
    fn compute_tile_decodes_a_bounded_five_second_tile() {
        let Some(src) = gen_asset("wave-tile.mp4", Some((440, 12, 0.5))) else {
            return;
        };
        let cancel = AtomicBool::new(false);
        // 细化同样复用**一个**解码器实例（与生产代码里的细化线程同一条路径）
        let mut dec = RangeDecoder::new(&src).expect("素材必须能打开");
        let t = compute_tile(&mut dec, 1, MIN_BUCKET_MS, &cancel)
            .expect("第 1 片（5..10 s）必须能算出来");
        assert_eq!(t.tile, 1);
        assert_eq!(t.start_ms, 5_000);
        assert_eq!(t.bucket_ms, MIN_BUCKET_MS);
        assert!(t.dense, "这一层必须来自密集解码路径");
        assert!(
            t.peaks.len() >= TILE_BUCKETS - 2,
            "整片 5 秒应有 ≈5000 个桶，实际 {}",
            t.peaks.len()
        );
        assert!(t.complete, "素材有 12 秒，第 1 片应该完整");
        let max = t.peaks.iter().fold(0.0f32, |a, p| a.max(p.max.abs()));
        // 量级判据同 `p22_compute_base_single_thread_fills_the_whole_file`（素材峰值 0.5 + AAC 过冲 → 实测 0.72）
        assert!(max > 0.3 && max < 0.95, "瓦片峰值量级异常：{max}");
        // 同一片换粗档位：解的音频一样多、桶数按档位缩水（这就是"缩放越深、切片越细"）
        let coarse = compute_tile(&mut dec, 1, 1_000, &cancel).expect("同一片按 1 s 档也能算");
        assert_eq!(coarse.peaks.len(), 5, "5 秒 / 1 秒档 = 5 桶");
        assert_eq!(coarse.bucket_ms, 1_000);
        // 取消的瓦片必须给出 None（而不是半片数据被当成完整片缓存）
        let killed = AtomicBool::new(true);
        assert!(compute_tile(&mut dec, 2, MIN_BUCKET_MS, &killed).is_none());
    }

    /// **降级：没有音轨的文件**必须返回可读的错误（而不是 panic 或画一条假波形）。
    #[test]
    fn file_without_audio_stream_degrades_cleanly() {
        let Some(src) = gen_asset("wave-noaudio.mp4", None) else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let e = compute_base_sync(&src, 2_000, 1, SEGMENT_MS, &cancel)
            .expect_err("没有音轨的文件必须报错");
        let msg = e.to_string();
        assert!(msg.contains("没有音频流"), "错误消息要能读懂：{msg}");
        // worker 那条路（界面真正走的那条）同样要**如实降级**：
        // 不能"什么都没有、也没有解释"，也不能永远停在"算到一半"
        let plan = Arc::new(BasePlan::new(2_000));
        let cancel2 = Arc::new(AtomicBool::new(false));
        let nop = |_: ()| {};
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || nop(()));
        base_worker_loop(&src, &plan, &cancel2, wake.as_ref());
        assert!(
            plan.error().is_some_and(|e| e.contains("没有音频流")),
            "打不开素材时 worker 必须把原因记进计划（界面据此给提示）"
        );
        assert!(plan.all_attempted(), "所有段都要收尾，界面不能永远停在「算到一半」");
        assert!(!plan.snapshot().expect("层永远在").has_peaks());
    }

    /// 不存在的文件 / 垃圾文件：报错而不是 panic。
    #[test]
    fn missing_and_garbage_inputs_are_errors_not_panics() {
        let cancel = Arc::new(AtomicBool::new(false));
        let missing = asset_dir().join("绝对不存在的素材.mp4");
        let _ = std::fs::remove_file(&missing);
        assert!(compute_base_sync(&missing, 1_000, 1, SEGMENT_MS, &cancel).is_err());
        let garbage = asset_dir().join("wave-garbage.bin");
        std::fs::write(&garbage, b"this is not a media file at all").unwrap();
        assert!(compute_base_sync(&garbage, 1_000, 1, SEGMENT_MS, &cancel).is_err());
        assert!(decode_range(&garbage, 0, 100, |_, _| true).is_err());
    }
}

