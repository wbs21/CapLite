//! 剪辑窗口（egui/eframe 0.36）。
//!
//! 分工：本文件只负责**界面与交互**，所有剪辑语义（关键帧吸附、copy 导出、缩略图解码）
//! 都在 [`crate::edit`] 里，这样核心逻辑可以脱离界面被单测与回归脚本驱动。
//!
//! 交互要点：
//! * **点击才提交，预览只看蓝色当前标尺**（本文件最重要的一条交互契约）：
//!   - **预览 = `cursor_ms` 所在位置最近的那个关键帧**（用户最终语义：
//!     "就显示蓝色标尺所在位置的预览"）。**悬停不驱动预览，拖动也不驱动预览**；
//!   - **左键点击时间线**（刻度尺区域与轨道区域都算）才把**当前标尺** `cursor_ms`
//!     定位到点击处；拖蓝色把手同样只改当前标尺；两者都会让预览跟着变；
//!   - 预览始终有值（`cursor_ms` 稳定），松手/移开都不会闪空或跳回别处。
//!     之所以这么定：以前"悬停即当前位置"，用户从时间线移向按钮的过程中位置一直在变，
//!     "去点按钮时位置已经移动了，定位不到自己想要的位置"。
//! * 三根标尺：**绿色起点** `sel_start_ms`、**红色终点** `sel_end_ms`、
//!   **蓝色当前标尺** `cursor_ms`（初值 = 时长的一半，即默认停在片子中间），都可拖拽；
//!   选中区间用半透明色高亮；
//! * 「设入点/设出点」按钮作用于**当前标尺**（不再是鼠标位置），吸附照旧走
//!   [`edit::snap_start`] / [`edit::snap_end`] / [`edit::effective_range`]；
//!   按钮行**不再显示**"当前帧 xx:xx"与"将吸附到 X"（P36 用户要求去掉 —— 前者时间线上
//!   本来就有，后者的规则是固定的，不需要每帧念一遍）；
//! * **多段合并导出（P36）**：设入点 → 设出点 → 「添加片段」把当前选区追加到片段列表，
//!   `segments` **顺序即播放顺序**（允许乱序/重叠/重复），芯片可拖拽改序、可删除；
//!   列表为空时退回单段导出（`export_copy`），有列表时走 `export_ranges` 逐段合并；
//! * 键盘（鼠标在时间线上或时间线有焦点时）：`[` / `]` = 把当前标尺设为起点/终点；
//!   `←` / `→` = 微调当前标尺（默认一帧，Shift 按关键帧跳动，Ctrl 10 ms）；
//!   `Home` / `End` = 当前标尺到片头/片尾；
//! * 时间线**可缩放**（滚轮，以指针为锚点、每格 1.1~1.25 倍）与**可平移**（右键拖拽）。
//!   **不再提供缩放滑条**：滚轮已经够用，滑条反而把比例锁死在固定区间里；
//!   **「适应全片」按钮与波形开关也已删除（P36）** —— 前者与"缩放回铺满比例"重复，
//!   后者改成常开（`apply_opened_clip` 里 `set_show_waveform(true)`）；
//! * **整片铺满**是一等公民：`open()` 之后缩放因子直接由"可用宽度 ÷ 媒体时长"推导，
//!   "可用宽度 ÷ 媒体时长"推导，窗口宽度一变就跟着变；缩放**不设人为上下限**，
//!   极长（≥1 小时）与极短（<1 秒）视频的兜底完全由像素量决定
//!   （见 `MAX_MS_PER_PX` / `fit_px_per_ms` / `sanitize_px_per_ms`）；
//! * 起点/终点的数字调节窗绑**毫秒**，但显示与手动输入都是**时间码** `HH:MM:SS.mmm`；
//! * 两条缩略图：轨道的**整体缩略图条**（导航用，等距 8 张，`open()` 时后台解一次）与
//!   **离预览位置最近的那个关键帧**的单张缩略图（按需解码 → `HashMap` 缓存 → 后台单张在途）。

use crate::config::Config;
use crate::edit::{self, Clip, ExportReport};
use crate::ui::layout::LayoutProbe;
use crate::ui::theme;
use eframe::egui;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

/// **预览解码尺寸的量化档位**（px）：预览的目标宽度先按 64px 一档向上取整，再去查缓存 /
/// 排解码任务。窗口拖动 1px 会让预览目标宽度变化 1px，但**档位**不变 → 不会重复解码
/// （这就是"拖动不重解"的保证；档位一变才按新尺寸解一张）。
const THUMB_BUCKET_STEP: u32 = 64;
/// **胶片条缩略图的解码请求宽度**（P19/P21 为"块宽"，P25 起只用于解码，逻辑点）。
///
/// P25 之前它同时是"块宽"与"解码宽"：每块固定 160 宽的槽位里做 cover 裁剪，
/// 于是竖屏素材只看得见中间一条横带（用户原话：「高度上只显示了画面一部分」）。
/// **P25 起画面按行高等比缩放、完整显示（contain）**，绘制宽 = `行高 × 源宽高比`
/// （见 [`strip_picture_width`]）—— 布局与它**无关**。
///
/// 它保留下来，只做一件事：**解码时请求的像素宽**（`ThumbJob::max_w` 与缓存键的 `w`
/// 分量）。为什么仍取 160：它正好在 [`THUMB_MAX_DECODE_W`] 之下的一档，
/// 解码纹理足够喂高 DPI 下的绘制宽（16:9 画 92 pt、竖屏画 29 pt），
/// 又和"其它地方的缩略图"共用同一套尺寸纪律；再大纯属浪费内存与 CPU。
const STRIP_TILE_W: f32 = 160.0;
/// **胶片条最多显示几块**（P21，用户口径：可见区间固定 5 个缩略图）。
///
/// 取样时刻 = `可见起点 + k × 可见时长 / (STRIP_TILES - 1)`，k = 0..5 ⇒
/// **最左那个贴左边界、最右那个贴右边界、中间三个均匀**（用户原话）。
/// 放不下时按 [`strip_slot_count`] 的规则依次降为 4→3→2→1。
const STRIP_TILES: usize = 5;
/// **胶片条布局口径的版本号**（P25 新增，进缓存键 [`StripThumbKey::v`]）。
///
/// P25 把"每块固定 160 宽的 cover 裁剪槽位"换成"按行高等比缩放、完整显示（contain），
/// 并首贴左 / 尾贴右 / 等间隙分布" —— **同一个 (时刻, 宽, 高) 解出来的画面含义变了**。
/// 缓存键里带上版本号，旧口径（P24 及之前）解出来的纹理**一个都不会被当成自己的画面**
/// （否则竖屏素材会继续拿"中间那条横带"来贴）。口径再变一次就把它 +1。
const STRIP_THUMB_VERSION: u32 = 2;
/// **胶片条相邻画面之间至少要留出的间隙**（逻辑点，P25）。
///
/// [`strip_slot_count`] 的窄条判据用它：`n` 个画面（每个宽 `行高 × 源宽高比`）
/// 加 `n−1` 个不小于本值的间隙仍然装不下，就减块数（5→4→3→2→1）。
/// 取 4：再小两块就"粘在一起"了，而用户要的正是"5 个画面**均匀分布**" ——
/// 看得见的间隔是这条要求的一部分（间隙最终由 `gap = (可用宽 − Σ画面宽)/(n−1)` 定，
/// 通常比它大得多；本值只是"窄条到什么时候该减块数"的那条线）。
const STRIP_MIN_GAP_PX: f32 = 4.0;
/// 一帧最多为多少个**取样时刻**排解码任务（护栏）。
///
/// P21 起取样时刻最多就是 [`STRIP_TILES`] 个（5），所以这个值只要 >= 5 就够；
/// 留 16 是为了"将来块数规则改了也不用同时改这里"，同时仍然是一个硬上界。
const STRIP_REQ_MAX: usize = 16;
/// 胶片条纹理缓存上限（块）。超了就整体清空重来（绝不无限增长）。
///
/// 5 块/屏 × 160 px，来回缩放/平移会积累不同时刻的块；256 块 × (160×52×4 B) ≈ 8 MB。
const STRIP_CACHE_MAX: usize = 256;
/// **胶片条每块的像素高度**（P24）= **胶片条行高** [`THUMB_BAND_H`]。
///
/// 用户口径（原话）：「缩略图高度适配影片条高度。」并确认「**只适配高度，宽度保持 160**」、
/// 「行高不会随窗口/DPI 变化」。
///
/// **行高的唯一来源就是 [`THUMB_BAND_H`]**：胶片条那一带在
/// `timeline()` 里是 `[rect.top(), rect.top() + THUMB_BAND_H]`（见那里的 `track_top`
/// 推导 —— `track_top = rect.top() + THUMB_BAND_H + THUMB_BAND_GAP`），
/// `TIMELINE_H` / `ui::layout::EDITOR_REQ_H` 也都由同一个常量推导。
/// 所以本常量**不是**第二个"行高来源"，它就是 [`THUMB_BAND_H`] 本身：
/// **块高严格 = 行高**（用户要的正是"贴行高"），块正好铺满胶片条那一带，
/// 与下面的标记行之间由 [`THUMB_BAND_GAP`] 分开，不靠"块自己缩 2 px"来留缝。
///
/// **它是逻辑点、编译期常量**：窗口拉大拉小、DPI 变化都不改它
/// （egui 的 DPI 缩放发生在"逻辑点 → 物理像素"那一步，不进这个常量）。
const STRIP_TILE_H: f32 = THUMB_BAND_H;
/// **胶片条解码缓存键里高度的量化步长**（逻辑点，P24）。
///
/// 当前行高是**常量**（[`STRIP_TILE_H`]），所以这一步现在恒等、看不出效果；
/// 它存在的意义是**将来行高一旦真的会变**（例如改成"行高随设置档位变"）时，
/// 缓存按"高度档"分开 —— 52 px 解出来的纹理绝不能被 40 px 那一档当成自己的画面
/// （那就是把画面纵向压扁/拉长）。取 4 px：误差在缩略图上看不出来，
/// 同时档位数少（不会把缓存切碎成几百份）。
///
/// **纪律**：写缓存与查缓存**必须**都过 [`strip_h_key`]（同一个量化函数）——
/// 这与时刻的 [`strip_key_ms`] 是同一条纪律，否则会出现
/// "每次都请求成功、却永远查不到缓存"的无限请求循环。
const STRIP_H_QUANT_PX: f32 = 4.0;
/// **取样时刻的量化步长**（毫秒）：块的解码键 = `(量化后的时刻, 块宽)`。
///
/// 为什么不直接用量化网格当取样时刻（P19 的做法）：用户明确要求"5 个缩略图、
/// 最左贴左、最右贴右" —— 取样时刻由可见区间直接算出（不吸附到 1 秒网格），
/// 于是左右两端**永远精确贴在边界上**。
///
/// 但**缓存键**仍然要量化：`可见起点 + k×可见时长/4` 是浮点结果，缩放 1 px 就会
/// 变成另一个时刻，不量化等于每帧都要重新解码 5 张图（P19 花大力气避免的就是这件事）。
/// 量化到 **100 ms**（= 解码层 `waveform::BASE_BUCKET_MS` 同一档）之后：
/// 同一个"大致位置"在不同缩放级别下会落到同一个键上，来回缩放**立刻命中缓存**；
/// 而 100 ms 的相对误差在 160 px 宽的块里完全看不出来（一帧 40 ms 都不到）。
const STRIP_QUANT_MS: i64 = 100;
// ---------------------------------------------------------------------------
// P27 · 取样按**视频流时长** + 失败时按方向**有界**回退（用户 2026-10-14 指令）
// ---------------------------------------------------------------------------

/// **P27 的 ε**：取样时刻距"**视频流可用末端**"的最小留白（毫秒）。
///
/// 为什么必须有 ε：容器里最后一帧常常"写了一半"（时间戳正好压在流末尾上，
/// 或末尾那个 GOP 没写完），取样点贴着 `视频流时长` 那一刻 seek 过去**必然**解不出帧。
///
/// 为什么取 100 ms：30 fps 一帧是 33 ms、10 fps 一帧是 100 ms —— 留出 100 ms 之后，
/// 取样点一定落在"最后若干帧**之中**"，而不是压在"最后一帧的边界"上。
/// 写成**单个具名常量**而不是"现算一帧时长"：行为固定、可断言、可一眼改。
const P27_SAMPLE_EPS_MS: i64 = 100;

/// **P27 方向搜索的最大尝试次数**（不含原始取样时刻本身）—— 回退**有界**的第一条保证。
const P27_MAX_ATTEMPTS: usize = 8;

/// **P27 方向搜索的最大距离**（毫秒）：一次回退最多离开取样时刻多远。
///
/// 取 5 s：录制侧的关键帧间隔是秒级，坏一个 GOP 时"跨 2~3 个关键帧"足够脱困；
/// 再大（例如扫到片头）就会把"这一块"显示成很远处的画面 —— 那比灰块更误导。
const P27_MAX_FALLBACK_MS: i64 = 5_000;

/// **P27 方向搜索的兜底步长**（毫秒）：没有可用关键帧（或关键帧都在窗口外）时，
/// 按这个步长逐点试。取 200 ms —— 与波形基准层切片同档，一屏几十次以内必定收敛。
const P27_STEP_MS: i64 = 200;

// ---------------------------------------------------------------------------
// P23 · 「先导入视频，后计算波形」的两阶段常量
// ---------------------------------------------------------------------------

/// **导入阶段允许持续多久**（毫秒）—— 超时即**兜底触发**波形阶段。
///
/// 用户原话：「要采用先导入视频，后计算波形。」落成严格两阶段之后，最大的风险不是"波形早了"
/// 而是"波形**永远**不开始"（某个就绪条件在某种素材上永远不成立）。
/// 这个常量就是那个风险的对冲：到了这一刻，"导入就绪"里还没满足的条件**如实记一条日志**
/// 然后当作就绪 —— 波形只可能晚 3 秒，不可能不来。
///
/// 为什么取 3000：正常导入（探测容器 + 首帧预览 + 胶片条首块）在本机实测是**几十到几百毫秒**
/// 这个量级（见 [`EditorPage::check_wave_phase`] 的注释），3 秒是它一个数量级的余量；
/// 再长（10 s）用户会觉得"打开后一直没波形"，再短（500 ms）会在慢盘/大文件上误触发兜底、
/// 把"两阶段"变成"其实还是一起跑"。
const IMPORT_READY_TIMEOUT_MS: u64 = 3_000;
/// **兜底看门狗的排帧间隔**（毫秒）。
const IMPORT_WATCHDOG_FRAME_MS: u64 = 100;
/// 导入阶段的**阶段名**（写进 `cap.log` 的 `phase=editor_import_ready`）。
const PHASE_EDITOR_IMPORT_READY: &str = "editor_import_ready";
/// 波形阶段的**阶段名**（严格在导入就绪之后才可能出现）。
const PHASE_EDITOR_WAVE_START: &str = "editor_wave_start";

// ---------------------------------------------------------------------------
// P34 · 流式打开（后台探测 + 续扫关键帧）
// ---------------------------------------------------------------------------

/// **打开还在途时的兜底重绘间隔**（毫秒）：没有输入界面本不出帧，
/// "正在打开…"与随后的首帧都靠这个间隔的排帧推进。
///
/// 为什么是 40 ms：比 Import 看门狗（100 ms）密一档 —— 打开长片时那几秒里
/// 用户看到的是"正在打开…"和随后进来的首帧，40 ms 的重绘足够跟手，
/// 又远低于"忙等"（CPU 占用约等于零：一次 try_recv + 一次排帧）。
const OPEN_POLL_MS: u64 = 40;

/// 后台打开线程送回界面的三种消息（P34）。
///
/// **轮次 `epoch`**：发信时记下 `EditorPage::open_epoch`，回收时对不上号就丢 ——
/// "快速连开两个文件"时第一个文件的迟到消息不许盖掉第二个文件的界面。
enum OpenMsg {
    /// 探测完成：clip 立刻可用（关键帧表还是空的）。界面据此起首帧/胶片条/波形。
    Clip(edit::Clip, u64),
    /// 关键帧扫描 + 编码体检回来（P35：两者共用同一次全片遍历）。
    ///
    /// 关键帧表可能因读包出错而是不完整的一部分（语义与改造前一致）；
    /// 体检报告会把那次读错如实记成一段硬坏段（界面标红 + 播放跳过 + 导出剔除）。
    Scan(edit::health::ScanPass, u64),
    /// 探测本身就失败（文件不存在 / 打不开 / 读不出流信息）。
    Failed(String, u64),
}

/// 预览解码宽度档位的下限 / 上限（px）：低于下限放大就糊，高于上限纯属浪费内存与 CPU。
const THUMB_MIN_DECODE_W: u32 = 320;
const THUMB_MAX_DECODE_W: u32 = 1920;
/// 缩略图带高度
const THUMB_BAND_H: f32 = 52.0;
/// 缩略图带与标记行之间的竖直留白
const THUMB_BAND_GAP: f32 = 4.0;
/// **标记行（轨道）高度** —— 音频波形就画在这一行里（背景），
/// 绿色起点 / 红色终点 / 蓝色当前标尺叠在它上面。
///
/// 为什么是 58：波形要能分辨"语音/静音的起伏"，而 min/max 竖线在 58px 的行里
/// 有 50px 的有效绘制高度（上下各留 4px），人声的 -20~-6 dBFS 动态范围能明显看出高低；
/// 再矮（<40）就只能看出"有声音/没声音"，再高（>70）则纯属把最小窗口顶高而没有新信息。
/// 这一行的加高**直接进** `EDITOR_REQ_H`（见 `ui::layout`），所以取值必须"够用就好"。
const TRACK_H: f32 = 58.0;
/// 标记行下面留出的**时间尺文字**高度（刻度线 + 时间码都画在这条带里）
const RULER_BAND_H: f32 = 14.0;
/// **全片位置行**的高度（P14；P17 去掉小方块后语义不变）。
///
/// 这一行放"全片轨道 + 当前位置"：整条深蓝 = 全片，淡蓝色带 = 当前可见区间。
/// **P17：那个 36×12 的小方块（以及它的 48×16 命中框）已经删掉** ——
/// 用户原话「浅蓝色滑动条已经完成了我想要的小滑块的功能，把小滑块去掉吧」。
/// 现在**整行**就是命中区域（高度 = 本常量），点/拖这一行任意位置都能滚动视口
///（P30 起它**只是滚动条**：不定位当前帧），所以不再需要"至少 16 逻辑点便于拖动"
/// 那种"给一个小方块放大命中框"的补偿。
const POS_ROW_H: f32 = 16.0;
/// 位置行那条细轨道的高度（px）。
const POS_TRACK_H: f32 = 6.0;
/// **时间线两套取色**（P40 深色模式）。
///
/// 时间线是深色模式下最需要重新挑颜色的一块：轨道、波形、片段高亮、标尺全部画在
/// 同一块底上，浅色那套按"浅灰底上的中深色"挑，深色那套必须整体提亮一档
/// （否则 `#475569` 的波形画在 `#12151C` 轨道上等于消失）。
/// 所以这里**不用** `theme::*` 取色函数（那些只区分"语义"，不区分"画在哪个槽里"），
/// 而是每个角色各留一深一浅两个值，运行时按 [`crate::ui::theme::is_dark`] 选。
fn tl(light: egui::Color32, dark: egui::Color32) -> egui::Color32 {
    if crate::ui::theme::is_dark() {
        dark
    } else {
        light
    }
}

/// **全片轨道**的颜色：整条铺满 = "这是全片"（P14 的语义核心）。
///
/// 浅色深蓝 `#1D4ED8`（压得住白底）；深色提亮到 `#3B82F6`（深底上要更亮才看得见）。
fn pos_track_color() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0x1D, 0x4E, 0xD8),
        egui::Color32::from_rgb(0x3B, 0x82, 0xF6),
    )
}
/// **当前可见区间**色带的颜色：淡蓝半透明，叠在全片轨道上，
/// 一条色带同时表达"我在全片哪里"与"我正看着哪一段"。
///
/// 浅色是"淡蓝 @ 80%"（`#BFDBFE`）；深色改成蓝色但**更低的不透明度** ——
/// 深色下高透明度的亮色带会把轨道糊成一片，看不清"带子的边界在哪"。
fn pos_band_color() -> egui::Color32 {
    // `from_rgba_unmultiplied` 不是 const fn，但这里不是 const 上下文，直接用
    tl(
        egui::Color32::from_rgba_premultiplied(0xBF, 0xDB, 0xFE, 0xCC),
        egui::Color32::from_rgba_unmultiplied(0x60, 0xA5, 0xFA, 150),
    )
}
/// 时间线整体高度 = 缩略图带 + 行间留白 + 标记行 + 时间尺带 + **全片位置行**（P14）。
///
/// **写成算式而不是字面量**：这样"标记行加高 → 整条时间线加高 → `EDITOR_REQ_H`
/// 加高"这条推导在源码里就是可读的（单测 `timeline_height_is_exactly_its_parts`
/// 再把算式钉死，见 `ui::editor::tests`）。
const TIMELINE_H: f32 = THUMB_BAND_H + THUMB_BAND_GAP + TRACK_H + RULER_BAND_H + POS_ROW_H;
/// **预览高度的下限**（px）：时间线以下、底部两行以上的剩余高度不够时也不低于它
/// （宁可因此出现滚动条，也不把当前帧压成一条）。规则见 [`fluid_preview_size`]。
const PREVIEW_MIN_H: f32 = 200.0;
/// 源尺寸未知时的回落比例（16:9）。
const PREVIEW_FALLBACK_ASPECT: f32 = 16.0 / 9.0;
/// 预览与导出按钮之间的水平间距。
const PREVIEW_EXPORT_GAP: f32 = 20.0;
/// 导出按钮宽度（主操作，撑满一列）。
const EXPORT_BTN_W: f32 = 200.0;
/// "预览 + 导出按钮同排"时预览至少要能拿到这么宽；比这更窄就把按钮**换到预览下方**
/// （这样预览仍能吃到整行宽度，按钮也不会被挤出视野）。
const PREVIEW_SIDE_BTN_MIN_W: f32 = 240.0;
/// 预览画框与它下方那行时间码之间的间距。
const PREVIEW_CAPTION_GAP: f32 = 4.0;
/// 播放中"手动定位"时，相邻两次**真的**发给解码线程的 seek 之间至少隔多少毫秒。
///
/// 拖动全片位置行时每帧都会重新锚定时钟，但每帧都 seek 等于每帧 flush 一次解码器
/// （`avcodec_flush_buffers` + 从关键帧重解），画面反而更卡。
/// 120 ms 这个值：比一帧（33 ms）大得多，又远小于"人眼能察觉的拖动迟滞"。
const PLAY_SEEK_MIN_MS: i64 = 120;
/// 起点/终点把手的抓取半径（像素）：按下点离把手这么近就认为是在拖它
const EDGE_GRAB_PX: f32 = 9.0;
/// 当前标尺把手的半径（像素）
const CURSOR_HANDLE_R: f32 = 5.0;
/// 当前标尺的颜色（主题里的蓝色：浅色深蓝 / 深色亮蓝）
fn cursor_color() -> egui::Color32 {
    crate::ui::theme::cursor()
}
/// 绿色起点标尺（浅色深绿 / 深色亮绿）
fn start_color() -> egui::Color32 {
    crate::ui::theme::ok_fg()
}
/// 红色终点标尺（浅色深红 / 深色亮红）
fn end_color() -> egui::Color32 {
    crate::ui::theme::danger()
}
/// **非当前分段**的底色（时间线上的"这些段也要导出"区间，多段模式）。
///
/// 浅色是"近黑 @ 10%"（比当前段那层蓝更淡的灰蓝）：一眼能看出"也是选区"，
/// 但绝不抢"当前正在编辑哪一段"的注意力。
/// **深色反过来用"白 @ 10%"** —— 底色是深色轨道，再压一层黑什么都看不见；
/// 提亮一档才保持"也是选区"的语义。
// 浅色沿用预乘写法（历史值 0x64/0x74/0x8B @ alpha 26 的等效近黑），深色用非预乘。
fn seg_inactive_fill() -> egui::Color32 {
    tl(
        egui::Color32::from_rgba_premultiplied(10, 12, 14, 26),
        egui::Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 26),
    )
}
/// **非当前分段**的两端线/把手颜色（灰蓝，比当前段的绿/红暗一档）。
fn seg_inactive_edge() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0x7A, 0x86, 0x99),
        egui::Color32::from_rgb(0x8B, 0x97, 0xA8),
    )
}
/// 影片条上**当前选中片段**的高亮（P36 起为蓝，P39 按用户要求改为**桔色**）。
///
/// 为什么改色：影片条上已经有三根必须一眼可辨的线（绿=起点、红=终点、蓝=当前标尺），
/// 片段高亮也是一片蓝色时，用户分不清"选中的是片段还是标尺"—— 改成桔色后
/// **选中片段**与**当前标尺**在色相上彻底分开。
///
/// 选 `#EA580C`（桔）而不是黄：软告警带是琥珀色 `#C28A1E`（更黄），起点绿/终点红/
/// 标尺蓝都与桔 hue 相距足够远；波形（`wave_color`）画在这层**之上**，所以加深底色
/// 不会把波形压掉。
///
/// **浓度沿用蓝色的 150/255 ≈ 59%**（用户两次要求"再加深"后定的档）：86（34%）时太浅，
/// 150 叠在白底上约 `(243,157,112)`，一眼就能看出这一段被选中了。
/// **深色提亮到 `#F97316 @ 170`**：深底上同浓度的桔会发闷，提亮度 + 略提浓度。
// 浅色预乘：0xEA/0x58/0x0C × 150/255 ≈ 138/52/7
fn seg_active_fill() -> egui::Color32 {
    tl(
        egui::Color32::from_rgba_premultiplied(138, 52, 7, 150),
        egui::Color32::from_rgba_unmultiplied(0xF9, 0x73, 0x16, 170),
    )
}
/// 影片条上**其它片段**的底色（P36）。
///
/// 同色系但很淡（34/255 ≈ 13%）：一眼能看出"这一截也是要导出的片段"，
/// 又绝不和当前段抢注意力。**刻意不再是灰色** —— 用户反馈灰带在浅灰轨道上
/// 几乎看不出来。**P39 起当前段改桔色**，这一档仍是蓝色系：
/// 分工变成"桔 = 正在编辑的片段，淡蓝 = 其余片段"。
/// 深色把淡蓝提亮到 `#60A5FA @ 40`（浅色那档在深轨道上几乎不可见）。
// 浅色预乘：0x25/0x63/0xEB × 34/255 ≈ 5/13/31
fn seg_other_fill() -> egui::Color32 {
    tl(
        egui::Color32::from_rgba_premultiplied(5, 13, 31, 34),
        egui::Color32::from_rgba_unmultiplied(0x60, 0xA5, 0xFA, 40),
    )
}
/// 片段芯片的**水平内边距**（px）。自绘芯片没有 `Button` 的默认 padding，靠它撑出
/// "一眼能点中"的面积；也决定芯片行的总高度（见 `layout::SEGMENT_LIST_SLACK`）。
const CHIP_PAD_X: f32 = 10.0;
/// 片段芯片的**垂直内边距**（px）。
const CHIP_PAD_Y: f32 = 4.0;
/// 片段芯片的**悬停底色**（`ACCENT` @ alpha 20）：只用来提示"这个能点、能拖"。
/// 深色换成浅蓝 `#93C5FD @ 30`（深底下同档蓝糊在卡片里看不出来）。
// 浅色预乘：0x25/0x63/0xEB × 20/255 ≈ 3/8/18
fn seg_hover_fill() -> egui::Color32 {
    tl(
        egui::Color32::from_rgba_premultiplied(3, 8, 18, 20),
        egui::Color32::from_rgba_unmultiplied(0x93, 0xCD, 0xFD, 30),
    )
}
/// **音频波形的颜色**：偏蓝的深灰（`#475569` 系）。
///
/// 为什么要单挑一个颜色而不是直接用主题里的某个：
/// 这一行上已经有三根必须一眼可辨的线（绿=起点、红=终点、蓝=当前标尺），
/// 波形是**背景内容**，既不能和它们同色、也不能比它们更显眼。这个蓝灰在浅灰轨道底上
/// 对比足够（画得清），又明显比 `cursor_color()`（`#1D4ED8`）更弱。
/// 深色提亮到 `#94A6BE`（仍比亮蓝标尺弱一档，但在深轨道上画得清）。
fn wave_color() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0x47, 0x55, 0x69),
        egui::Color32::from_rgb(0x94, 0xA6, 0xBE),
    )
}
/// **波形渐进指示色**（那条"算到这儿"的淡竖线）：比波形更淡，明确是"临时状态"。
/// 两个主题下都比**同主题的** `wave_color` 淡一档。
fn wave_pending() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0x9C, 0xA3, 0xAF),
        egui::Color32::from_rgb(0x5C, 0x6A, 0x7E),
    )
}
/// **硬编码错误带**（P35）：播放会跳过、导出会剔除的那一段。
///
/// 用比终点标尺（红）更沉一档的红 —— 它是一条**区域**，不是一根"用户设的线"，
/// 不能抢终点把手的辨识度。深色提亮到 `#E05252`（同口径：比亮红把手沉一档）。
fn bad_hard_color() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0xB4, 0x2B, 0x2B),
        egui::Color32::from_rgb(0xE0, 0x52, 0x52),
    )
}
/// **软告警带**（P35）：时间戳跳变 / 零关键帧 —— 只提示，**不改变播放与导出行为**。
///
/// 琥珀色：一眼看出"这里有事"，但与硬错误（红）明确区分，
/// 免得用户以为"这段播不了"（其实画面完全正常）。深色提亮到 `#E0A83A`。
fn bad_soft_color() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0xC2, 0x8A, 0x1E),
        egui::Color32::from_rgb(0xE0, 0xA8, 0x3A),
    )
}
/// 时间线上"非当前分段"编号文字的颜色（画在片段高亮块上，取同主题的一档灰）。
///
/// 浅色 `#334155`（当前段是白字压在桔块上；非当前段是深灰字压在淡蓝块上）；
/// 深色反过来：非当前段用浅灰字 `#C3CBD8` 压在亮蓝块上。
fn seg_number_color() -> egui::Color32 {
    tl(
        egui::Color32::from_rgb(0x33, 0x41, 0x55),
        egui::Color32::from_rgb(0xC3, 0xCB, 0xD8),
    )
}
/// 坏段带的高度（像素）：贴在标记行**底部**，不盖住波形主体。
const BAD_BAND_H: f32 = 5.0;
/// **片尾之后那条"无内容"带的斜纹间距**（像素，P10）。
///
/// 9 px 是"远看是淡淡一片、近看能看出斜纹"的折中：太小会糊成一块灰（又像黑块），
/// 太大就成了显眼的装饰。
const EMPTY_HATCH_STEP: f32 = 9.0;
/// 时间尺相邻刻度的最小像素间距（抽稀依据）
const RULER_MIN_GAP_PX: f64 = 78.0;
/// 关键帧刻度抽稀：相邻保留刻度至少隔这么多像素
const KEYFRAME_MIN_GAP_PX: f64 = 3.0;
/// 缩放的**数值**上限（px/ms）。这是防溢出护栏，不是产品意义上的缩放上限：
/// 到这一步 1 px 只代表 1 皮秒，滚轮要按 250 格以上才够得着。
const MAX_PX_PER_MS: f64 = 1e9;
/// 1 个像素最多代表多少毫秒 —— 缩放下限由它换算（**像素决定**，不是硬编码比例）。
///
/// 取 24 小时：即便滚到最远，800 px 宽也能铺下约 800 天，任何真实录像都卡不到这个底；
/// 它唯一的职责是保证 `宽度 / 缩放` 不会算出 0 / Inf / NaN。
const MAX_MS_PER_PX: f64 = 24.0 * 3600.0 * 1000.0;
/// 滚轮一格的缩放指数系数：一格 `smooth_scroll_delta` 约 50~100 px，
/// 对应 1.116~1.246 倍（见 `zoom_factor` 的单测），落在要求的 1.1~1.25 倍内。
const ZOOM_PER_SCROLL: f64 = 0.0022;
/// 时间线可用宽度的兜底下限（像素）：窗口被挤到极窄时也不能出现 0 宽
const MIN_AVAIL_W: f32 = 120.0;
/// 时间尺的"整"步长候选（毫秒）
const RULER_STEPS_MS: [f64; 14] = [
    5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 30_000.0,
    60_000.0, 300_000.0,
];

// ---------------------------------------------------------------------------
// 音频波形：后台计算 + 渐进发布 + 取消 + 内存缓存
// ---------------------------------------------------------------------------

use crate::edit::waveform::{self, BaseLayer, BasePlan, DetailTile, ViewLayer};

/// 一次波形任务的**状态**（由 UI 线程持有、后台线程更新）。
///
/// 为什么要 `Arc<Mutex<..>>` 而不是原子：要发布的不只是一整片基准层（1 小时 ≈ 288 KB），
/// 还有"算到第几段""密集缓存到了哪些片"。用"互斥量 + `Arc` 快照换手"是最直白、
/// 也最容易证明"不会读到半写状态"的做法；锁的持有时间 = 换一次 `Arc`（几十纳秒），
/// UI 线程只在 `try_lock`，拿不到就直接用上一帧的快照 —— **UI 永远不会等后台**。
#[derive(Debug, Default)]
struct WaveShared {
    /// 全片密集基准层（`None` = 还没建立任何计划）
    base: Option<Arc<BaseLayer>>,
    /// 密集瓦片（LRU 上限 [`waveform::TILE_CACHE_MAX`] 片，由后台线程维护）
    tiles: waveform::TileCache,
    /// **基准层阶段**是否已经结束（所有段都尝试过 —— 含失败的段）
    finished: bool,
    /// 失败原因（`None` = 正常）
    error: Option<String>,
    /// 音轨编码名（"aac" / "mp3"…），只用于日志
    codec: String,
    /// 基准层是否来自落盘缓存（只用于日志：`cache=hit|miss`）
    cache_hit: bool,
}

/// **细化调度线程**收到的一条请求。
enum RefineReq {
    /// 按 `bucket_ms` 这一档算出这一片（用户可见的那几片，按"离屏幕中心近"的顺序发过来）。
    ///
    /// 为什么请求里要带档位（P18）：调度线程可能**落后于界面**好几帧，
    /// 那时用户已经缩放到另一档了 —— 请求带上自己那一档，算出来的瓦片就**永远自洽**
    /// （桶宽与它的数据一致），缓存侧按"只留当前档"的规则把它收进来。
    Tile { tile: usize, bucket_ms: i64 },
    /// **收工**：线程读到就退出。
    ///
    /// ⚠️ 这个变体是**必须**的，不能用"丢掉 `Sender` 让 `recv()` 返回 `Err`"来代替：
    /// 拆任务的那几处（[`WaveState::cancel_current`]、
    /// [`WaveState::spawn_job`] 的降级分支）在 `join()` 期间
    /// **自己还持有 `req_tx`**，通道不会断开 —— 只靠取消标志的话线程会一直阻塞在
    /// `recv()` 上，`join()` 于是**永不返回**，直接死锁 UI 线程
    ///（症状：进入剪辑页后窗口不再重绘、`Responding=False`）。
    /// 显式的"收工"消息让"线程一定退得出"这件事**不依赖发送端的生命周期**。
    ///
    /// 换文件时不需要"作废在算的那一片但保留线程"这种中间态：取消标志一置，
    /// 线程本来就会在下一次 `recv()` 醒来时退出（见线程里的检查），
    /// 而新文件走的是**另起一个任务**（`cancel_current` → `spawn_job`）。
    Quit,
}

/// 正在跑的波形任务：**4 个基准层 worker** + **1 个可见区间细化线程**（P22）。
///
/// 为什么要这样分工（用户确认的 4 线程口径）：
/// * **基准层 worker（[`waveform::WAVE_WORKERS`] = 4）**：把整片按 8 分钟一段并行解满，
///   每段独立 demuxer + decoder。它们只在"有空段"时干活，算完就自然退出 ——
///   不是常驻轮询，也没有心跳；
/// * **细化线程（1 个）**：可见区间 <= 120 s 时，画面用的是**档位密集瓦片**，
///   这条服务必须独立于基准层（否则"刚放大就要等基准层让出位置"，那就退回 P20 之前的体感了）。
///   它一次只服务**一个可见区间**，且"只解最近"（队列里排在最前面的永远是最新视口）。
///
/// **生命周期（P16/P22）**：基准层 worker 算完全部段后自己退出（句柄在 `poll_waves` 回收）；
/// 任务本体（`shared` + 细化线程 + 请求队列）在文件打开期间一直留着。
/// 只有换文件 / 关开关 / 降级这三条路才会把整个任务拆掉，且拆除一律按
/// **"置取消 → 发 `Quit` → 再 join"**（绝不"持有 `Sender` 时 join"）。
struct WaveJob {
    /// 与**本次任务**绑定的缓存键：完成后由 UI 线程按当时的 `key` 决定要不要入库
    key: waveform::CacheKey,
    /// 置 true = 立刻取消（所有 worker 的解码回调里都会看到）
    cancel: Arc<AtomicBool>,
    /// 基准层进度快照的通道（`()` = "有新数据了"；本体在 `shared.layer` 里）
    rx: Receiver<()>,
    /// 送给细化调度线程的请求队列
    req_tx: Sender<RefineReq>,
    /// **基准层的共享计划**（4 个 worker 与界面之间唯一的共享状态）
    plan: Arc<BasePlan>,
    /// **数据驱动重绘**用的界面上下文槽：UI 线程每帧把 `egui::Context` 填进来
    /// （`Context` 是 `Send + Sync`，后台线程 `request_repaint()` 是标准用法），
    /// 后台线程在"产出一段基准层切片 / 一块密集瓦片"时用它唤醒界面 ——
    /// **没有周期性心跳、没有每帧轮询**，界面只在真的有新数据时才重绘。
    ///
    /// 为什么是"槽"而不是 spawn 时直接捕获：`open()` 可能在剪辑页还没渲染过第一帧时
    /// 就被调用（例如用户在录制页点「在剪辑页打开」、而剪辑页还没渲染过），那一刻拿不到 `Context`；
    /// 用槽的话，界面第一次渲染时把 `Context` 放进去，之后所有产出都能唤醒它。
    ui_ctx: Arc<Mutex<Option<egui::Context>>>,
    /// 基准层 worker 的句柄（全部退出后为空；`take` 走 = 已经回收过）
    base_handles: Vec<std::thread::JoinHandle<()>>,
    refine_handle: Option<std::thread::JoinHandle<()>>,
    cancelled: bool,
}

/// 把已取消的波形线程交给后台 reaper join；UI 线程只负责移交句柄。
fn reap_wave_handles(
    refine: Option<std::thread::JoinHandle<()>>,
    bases: Vec<std::thread::JoinHandle<()>>,
) {
    if refine.is_none() && bases.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("wave-reap".into())
        .spawn(move || {
            if let Some(h) = refine {
                let _ = h.join();
            }
            for h in bases {
                let _ = h.join();
            }
        });
}

/// **后台线程唤醒界面的唯一方式**（数据驱动重绘）。
///
/// 两个波形线程在"产出一段 L0 进度 / 一块细化瓦片 / 收尾"时各调一次，
/// 于是界面**只在真的有新数据时**才重绘一次：
/// * 没有周期性心跳（`request_repaint_after`）、没有每帧轮询、没有活锁风险；
/// * 槽还空着（剪辑页这一帧都没渲染过，例如用户刚点「在剪辑页打开」那一次 `open()`）
///   就什么都不做 —— 那一帧的数据会在用户切到剪辑页时被首次绘制读到，
///   之后的产出才需要唤醒。
fn wake_ui(slot: &Mutex<Option<egui::Context>>) {
    let ctx = match slot.lock() {
        Ok(g) => g.clone(),
        // 锁中毒（另一个线程 panic 过）：退化"不唤醒"，绝不 panic、绝不阻塞
        Err(_) => None,
    };
    if let Some(ctx) = ctx {
        ctx.request_repaint();
    }
}

/// 波形的**状态机**：全片基准层（4 线程后台补齐）+ 可见区间密集瓦片（按需算，都进内存缓存）。
///
/// 三种可用性：
/// * [`Availability::Ready`] —— 基准层已经完整（缓存命中或刚算完）；
/// * [`Availability::Streaming`] —— 基准层还在补（界面照常画已算出的切片 + 一条"算到这儿"）；
/// * [`Availability::Unavailable`] —— 没有音轨 / 解码器没编进来 / 解码失败（**只记日志、界面降级**）。
#[derive(Default)]
struct WaveState {
    /// 后台任务共享状态（`try_lock` 拿不到时用 `last_good`）
    shared: Option<Arc<Mutex<WaveShared>>>,
    /// 最近一次成功读到的基准层快照（"上一次不是半写状态的那一份"；`Arc` 克隆，很便宜）
    last_good: Option<Arc<BaseLayer>>,
    /// 正在跑的任务
    job: Option<WaveJob>,
    /// 已经把哪几片（**哪一档的**哪几片）发给了调度线程（避免每帧重复发同一个请求）。
    ///
    /// 键里带档位（P18）：换档时同一个片号是**另一份数据**，必须重新请求 ——
    /// 缓存侧的 `retain_tier` 已经把旧档的片清掉了。
    requested: std::collections::HashSet<(usize, i64)>,
    /// 上一帧请求细化时用的档位（`None` = 还没请求过）。
    ///
    /// 档位一变就把 [`WaveState::requested`] 清空并让缓存只留新档：
    /// 旧档的"已请求"结论对新档无效，不清就会**永远不重算**（P18 的一类死锁）。
    last_refine_tier: Option<i64>,
    /// 内存缓存：键 = 路径 + 大小 + 修改时间 + 算法版本
    cache: HashMap<waveform::CacheKey, Arc<BaseLayer>>,
    /// 缓存的"读不出来"结论（避免每次重开都重试一遍解码）
    unavailable: HashMap<waveform::CacheKey, String>,
    /// 最近一次失败原因（界面提示 + 日志去重）
    last_error: Option<String>,
    /// 落盘缓存（`Cache\`，由外壳注入；`None` = 不落盘）
    disk: waveform::DiskCache,
    /// 上一次细化请求的时间（用来做"拖完手停一下再细化"的节流，见 `pump_refine`）
    last_refine_at: Option<std::time::Instant>,
    /// **整片基准层补完的耗时**（P22 验收要贴的那个数字；`None` = 还没补完 / 走的缓存）
    base_done_at: Option<std::time::Duration>,
    /// 打开素材到"首屏可见"的耗时（可见区间的第一块密集瓦片 / 第一段基准层到手）
    first_data_at: Option<std::time::Duration>,
    /// 打开素材的起点（上面两个耗时都是相对它）
    opened_at: Option<std::time::Instant>,
    /// 基准层是否已经落盘（避免每帧重复写 288 KB）
    base_saved: bool,
    /// **界面最近一次发布的可见区间**（毫秒）。
    ///
    /// 为什么必须记下来：4 条基准层 worker 是**一起跑**的，一起来就 `take_next()` 领任务。
    /// 若此刻 `BasePlan` 里的可见区间还是默认的 `[0, 0]`，它们会按"离片头最近"领
    /// 0/1/2/3 段 —— 用户明明看着第 50 分钟，却先把片头算出来，这就是
    /// "优先计算当前显示区域"失效的样子。所以 `spawn_job` 在**起线程之前**先用它预热。
    last_visible: Option<(i64, i64)>,
}

/// 波形对界面的**可用性**（见 [`WaveState::availability`]）。
///
/// `Streaming` **不带进度数字**：那条"算到哪画到哪"的截断线由
/// [`edit::waveform::WaveSegments::pending_ms`]（本帧数据自己的缺口起点）给出，
/// 比"某一层的进度"更贴近用户看到的东西。这里的职责只剩
/// "有没有画出东西 / 要不要读快照"。
#[derive(Debug, Clone, PartialEq)]
enum Availability {
    /// 还没算出任何东西（但任务在跑）
    Pending,
    /// 算到一半（界面照常画已有数据，缺口处给一条淡指示线）
    Streaming,
    /// 完整可用
    Ready,
    /// 读不出来（没有音轨 / 没编入解码器 / 损坏），只记日志、不画波形
    ///
    /// **不带文案**（P1）：这一档的具体原因由 `WaveState::last_error` 用**当前语言**
    /// 的 i18n 文案给出（见 `WaveState::start` / `finish_wave`），界面提示读的是那一份。
    Unavailable,
}

/// 缓存条目上限。同一个文件只会留一条（见 `remember`），但用户来回切文件时
/// 多留几条能省掉重新解码；上限到了就整体清空重来，绝不无限增长。
///
/// P22 起一条 = 一整片基准层（1 小时 ≈ 288 KB），比旧 L0（8 KB）大一个半数量级；
/// 16 条的上界 ≈ 4.6 MB（1 小时的片子），仍然很便宜，所以保留这个档位。
const WAVE_CACHE_MAX: usize = 16;

/// **细化请求的节流**：用户拖动/连续缩放时不要每帧都发新请求（否则队列里全是
/// "已经划过去的区间"，真正要看的反而排到后面）。手停 120 ms 才提交新请求。
const REFINE_DEBOUNCE_MS: u128 = 120;

impl WaveState {
    /// 注入落盘缓存目录（`Cache\`，由外壳按既有回退链解析后给过来）。
    fn set_disk_dir(&mut self, dir: Option<PathBuf>) {
        self.disk = waveform::DiskCache::new(dir);
    }

    /// 当前基准层快照（优先用后台刚发布的那一份；拿不到锁就用上一帧的）。
    ///
    /// 返回 `Arc`：克隆一个引用计数，**不是**克隆 288 KB。
    fn snapshot(&self) -> Option<Arc<BaseLayer>> {
        if let Some(s) = &self.shared {
            if let Ok(g) = s.try_lock() {
                if let Some(b) = &g.base {
                    return Some(b.clone());
                }
            }
        }
        self.last_good.clone()
    }

    /// 当前**密集瓦片**的只读快照（与基准层同一个锁；`peek` 不改 LRU 顺序）。
    fn tiles_map(&self) -> HashMap<usize, DetailTile> {
        match self.shared.as_ref().and_then(|s| s.try_lock().ok()) {
            Some(g) => g.tiles.snapshot_map(),
            None => HashMap::new(),
        }
    }

    /// **还活着的基准层 worker 数**（P23 的单测用它钉住"导入阶段一个 worker 都没有"）。
    ///
    /// `job` 为空（还没启动 / 已经收干净）时恒为 0 —— 那正是"没起线程"的可观测形式。
    /// 只给单测用：生产路径上"有没有 worker"由 `poll_waves` 自己看 `base_handles`，
    /// 多一个只在测试里活的访问器比在生产代码里留一个死方法干净。
    #[cfg(test)]
    fn base_handles_count(&self) -> usize {
        self.job.as_ref().map(|j| j.base_handles.len()).unwrap_or(0)
    }

    /// 本帧的可用性。
    fn availability(&self) -> Availability {
        if let Some(s) = &self.shared {
            if let Ok(g) = s.try_lock() {
                if let Some(e) = &g.error {
                    let _ = e; // 具体文案由 `WaveState::last_error` 给（i18n，见 `finish_wave`）
                    return Availability::Unavailable;
                }
                if let Some(b) = &g.base {
                    return if g.finished || b.finished {
                        Availability::Ready
                    } else {
                        Availability::Streaming
                    };
                }
                if g.finished {
                    return Availability::Unavailable;
                }
            }
        }
        if let Some(l0) = &self.last_good {
            return if l0.is_complete() {
                Availability::Ready
            } else {
                Availability::Streaming
            };
        }
        Availability::Pending
    }

    /// UI 线程每帧把当前 `egui::Context` 交给在跑的任务（后台线程用它唤醒界面）。
    ///
    /// 返回值 = **这一帧刚刚把槽填上**（此前是空的）。调用方据此补**一次**重绘：
    /// 后台线程的"数据驱动唤醒"要等 `Context` 到位后才有效，而任务可能在本帧
    /// 才刚被创建（例如刚点完「在剪辑页打开」）—— 不补这一帧，渐进显示会一直等到用户碰鼠标。
    /// 只补一次：槽非空之后这条分支不再成立，**不是心跳**（P15）。
    ///
    /// 只在槽还空着时写入：`Context` 在整个进程里是同一个（`Arc` 克隆，代价可忽略），
    /// 但每帧无脑覆盖也没意义 —— 空着才写能省掉每帧多余的互斥量操作。
    fn set_ui_ctx(&self, ctx: &egui::Context) -> bool {
        let Some(job) = self.job.as_ref() else {
            return false;
        };
        let mut wrote = false;
        if let Ok(mut g) = job.ui_ctx.lock() {
            if g.is_none() {
                *g = Some(ctx.clone());
                wrote = true;
            }
        }
        wrote
    }

    /// **取消当前任务**（换文件 / 关页 / 关掉开关时调用）。
    ///
    /// 取消是协作式的，纪律（P22 的重中之重，**绝不重犯已修好的死锁**）：
    /// 1. **置取消标志**（所有 worker 的解码回调都看得见，最坏再解完一个包）；
    /// 2. **发 `Quit`**（细化线程的 `recv()` 才会醒 —— 光靠取消标志它可能一直阻塞在
    ///    `recv()` 上，而下面 join 期间我们自己还持有 `req_tx`，通道不会断开）；
    /// 3. **再 join**。基准层 worker 不用发消息：它们只在 `take_next()` 与解码里查标志，
    ///    每次查完就退，join **一定有限**（一个包的量级）。
    fn cancel_current(&mut self) {
        if let Some(mut job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            job.cancelled = true;
            // 先让调度线程**收工**；join 交给后台 reaper，绝不在 UI 线程等解码。
            let _ = job.req_tx.send(RefineReq::Quit);
            let refine = job.refine_handle.take();
            let bases = std::mem::take(&mut job.base_handles);
            reap_wave_handles(refine, bases);
        }
        self.shared = None;
        self.last_good = None;
        self.requested.clear();
        self.last_refine_tier = None;
        self.base_saved = false;
    }

    /// **为 `clip` 启动波形计算**（路径、时长、"有没有音轨"都从它来）。
    ///
    /// 顺序（这就是"可见区间最优先 + 渐进 + 缓存 + 取消"的全部）：
    /// 1. 取消上一个任务（同一文件重复打开也不例外：先取消再命中缓存）；
    /// 2. 内存命中 → 基准层立刻可用（**不重算、也不起 worker**），但**仍然起细化任务**：
    ///    "按需细化"是独立的一条服务，放大后要看**密集档位**的波形，它不能因为
    ///    "这次基准层是从缓存来的"就缺席（P15）；
    /// 3. 没有音轨的素材（`clip.audio == None`）→ 直接记一行日志、不建线程；
    /// 4. 其余 → 查一次落盘缓存；未命中就起 **4 个基准层 worker**（后台补齐全片）
    ///    + 细化调度线程。可见区间由 `pump_refine` 每帧写进 `BasePlan`（P22 的"插队"）。
    fn start(&mut self, clip: &Clip, i18n: &crate::i18n::I18n) {
        self.cancel_current();
        self.opened_at = Some(std::time::Instant::now());
        self.first_data_at = None;
        self.base_done_at = None;
        let key = waveform::CacheKey::of(&clip.src);
        if let Some(base) = self.cache.get(&key) {
            self.last_good = Some(base.clone());
            self.last_error = None;
            tracing::debug!(path = %clip.src.display(), "剪辑页：波形命中内存缓存，不再解码");
            // 只跳过"算基准层"，不跳过"按需细化"（见上面第 2 条）
            self.spawn_job(clip, key, i18n, Some(base.clone()));
            return;
        }
        if let Some(reason) = self.unavailable.get(&key) {
            // 记住的**已经是本地化文案**（见下），直接复用，不重复查表
            self.last_error = Some(reason.clone());
            return;
        }
        // **落盘缓存：命中即秒开**（P22）。1 小时的基准层 ≈ 288 KB，
        // 读 + 校验 ≈ 1 ms —— 与"再解一遍整片"（几秒）差三个数量级，
        // 所以这一步放在同步路径上（`open()` 本来就要同步探测容器）是划算的。
        // 任何一处不对（magic / 版本 / 长度 / 取值范围）都当未命中，**绝不 panic**。
        let t_cache = std::time::Instant::now();
        if let Some(base) = self.disk.load(&key, clip.duration_ms) {
            tracing::debug!(
                path = %clip.src.display(),
                slices = base.slice_count(),
                dur_ms = t_cache.elapsed().as_millis(),
                "剪辑页：波形命中落盘缓存（秒开）"
            );
            crate::ui::startup::mark_with(
                "cache_load",
                "wave",
                Some(t_cache.elapsed()),
                Some(format!(
                    "key={} result=hit slices={} bytes={}",
                    key.file_name(),
                    base.slice_count(),
                    base.slice_count() * 8
                )),
            );
            let arc = Arc::new(base);
            self.last_good = Some(arc.clone());
            self.last_error = None;
            self.remember(key.clone(), arc.clone());
            self.spawn_job(clip, key, i18n, Some(arc));
            return;
        }
        crate::ui::startup::mark_with(
            "cache_load",
            "wave",
            Some(t_cache.elapsed()),
            Some(format!("key={} result=miss", key.file_name())),
        );
        // **录制侧不参与**：这里只读文件的音轨。文件里没有音轨就如实降级。
        if clip.audio.is_none() {
            let reason = i18n.tf("err_wave_no_audio", &[&clip.file_name()]);
            tracing::info!(
                path = %clip.src.display(),
                "剪辑页：素材没有音频流，不计算波形（如实降级，不影响剪辑）"
            );
            self.last_error = Some(reason.clone());
            self.unavailable.insert(key, reason);
            return;
        }
        // 落盘也没命中 → 起 4 个基准层 worker，后台把整片补满
        self.spawn_job(clip, key, i18n, None);
    }

    /// 真正起线程（与 [`WaveState::start`] 分开是为了让"缓存命中"那条路一眼看清）。
    ///
    /// `base_ready`：内存 / 落盘缓存已经给出**完整**的基准层 → 一个基准层 worker 都不起
    /// （没什么可算的），只起细化线程（放大后要用它，见 [`WaveState::start`] 第 2 条）。
    ///
    /// 未命中时起 **[`waveform::WAVE_WORKERS`] = 4** 个基准层 worker：它们各自独立地
    /// 打开 demuxer + decoder、按 8 分钟一段把整片解满，写进共享的 [`BasePlan`]。
    /// **可见区间最优先**由 `BasePlan` 的优先级队列保证（界面每帧把当前可见区间写进去，
    /// 见 [`WaveState::publish_visible`]）。
    fn spawn_job(
        &mut self,
        clip: &Clip,
        key: waveform::CacheKey,
        i18n: &crate::i18n::I18n,
        base_ready: Option<Arc<BaseLayer>>,
    ) {
        let duration = clip.duration_ms.max(0);
        let shared = Arc::new(Mutex::new(WaveShared::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel::<()>();
        let (req_tx, req_rx) = channel::<RefineReq>();
        let ui_ctx: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
        let plan = Arc::new(BasePlan::new(duration));
        // **预热可见区间**：4 条 worker 一起跑、一起来就 `take_next()` 领任务，
        // 而界面第一次 `publish_visible` 要等到本帧绘制之后。若不在**起线程之前**
        // 把上一次的可见区间写进计划，它们会按"离片头最近"领走 0/1/2/3 段 ——
        // 用户明明看着第 50 分钟，却先算片头。这就是"优先计算当前显示区域"失效的样子。
        //
        // 夹取是必要的：换成一个更短的素材时，上一次的 `last_visible` 可能已经越过新片尾，
        // 越界的区间会让"离它最近"的排序失去意义（全部等距）。
        if let Some((a, b)) = self.last_visible {
            let a = a.clamp(0, duration);
            let b = b.clamp(a, duration);
            plan.set_visible(a, b);
        }
        self.shared = Some(shared.clone());
        // 有现成基准层时把它留着：万一后面某条线程起不来而走降级，
        // "已经算好的那一层"不该跟着一起丢掉（画面还能照常显示整片波形）。
        self.last_good = base_ready.clone();
        self.base_saved = base_ready.is_some();
        self.requested.clear();

        // **基准层已经在手**（内存/落盘缓存命中）→ 不起 worker：
        // `shared` 直接就是"算完了"的样子，界面这一帧就能画整片波形。
        let have_base = base_ready.is_some();
        if let Some(base) = base_ready {
            if let Ok(mut g) = shared.lock() {
                g.base = Some(base);
                g.finished = true;
                g.cache_hit = true;
            }
        }

        // ---- 基准层 worker：4 条线程把整片按 8 分钟一段解满 ----
        //
        // 线程优先级 / 让出 CPU 的取舍（用户要求里"你自行决定并说明"的那一条）：
        // 解码是**纯 CPU 的批处理**，但每一次调用都**有界**（一段 8 分钟、一块 5 秒），所以
        // 不需要也不应该动线程优先级 —— 降优先级会让"打开文件后波形迟迟不出现"，
        // 升优先级又会和采集/编码抢核。流畅靠的是**结构**：
        //   ① 单次解码量有上界（一段 / 一片），没有"整片解码"这种不可打断的长任务；
        //   ② UI 线程从不同步等待它（只 `try_lock` / `try_recv`，`snapshot()` 只克隆 `Arc`）；
        //   ③ 渲染只遍历可见区间，且长视口下把切片**合并到像素**（见 `segments_for_view`）。
        let wave_panic_error = i18n.t("err_wave_decode_failed").to_string();
        let workers = if have_base { 0 } else { waveform::WAVE_WORKERS };
        let mut base_handles: Vec<std::thread::JoinHandle<()>> = Vec::with_capacity(workers);
        if workers == 0 {
            // 没什么可算的：`tx` 随之被丢弃 → `rx` 断开，
            // `poll_waves` 排空进度消息时自然结束（不会有人发消息）。
            drop(tx);
        } else {
            let wake: Arc<dyn Fn() + Send + Sync> = {
                let shared_w = shared.clone();
                let plan_w = plan.clone();
                let ctx_w = ui_ctx.clone();
                Arc::new(move || {
                    // **数据驱动重绘**：只把"新一份层快照"换上并叫醒界面，
                    // 没有周期性心跳、没有每帧轮询。
                    if let Ok(mut g) = shared_w.lock() {
                        g.base = plan_w.snapshot();
                    }
                    wake_ui(&ctx_w);
                })
            };
            for i in 0..workers {
                let path = clip.src.clone();
                let plan_w = plan.clone();
                let cancel_w = cancel.clone();
                let wake_w = wake.clone();
                let shared_w = shared.clone();
                let ctx_w = ui_ctx.clone();
                let tx_w = tx.clone();
                let panic_error_w = wave_panic_error.clone();
                match std::thread::Builder::new()
                    .name(format!("clip-wave-base-{i}"))
                    .spawn(move || {
                        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || waveform::base_worker_loop(
                                &path,
                                &plan_w,
                                &cancel_w,
                                wake_w.as_ref(),
                            ),
                        ));
                        if ran.is_err() {
                            plan_w.set_error(panic_error_w);
                            plan_w.fail_all_pending();
                        }
                        // 收尾：所有段都尝试过了就标记"渐进结束"，并把最后一份快照换上
                        if plan_w.all_attempted() || ran.is_err() {
                            plan_w.finish();
                        }
                        if let Ok(mut g) = shared_w.lock() {
                            g.base = plan_w.snapshot();
                            g.finished = plan_w.all_attempted();
                            // **打不开素材的原因**（没有音轨 / 解码器没编进来）如实上报：
                            // 界面据此降级 + 给一句提示，而不是"什么都没有、也没有解释"
                            if g.error.is_none() {
                                g.error = plan_w.error();
                            }
                        }
                        // 通道里只传"有新数据 / 结束了"这件事：快照本体已经在 `shared` 里。
                        let _ = tx_w.send(());
                        wake_ui(&ctx_w);
                    }) {
                    Ok(h) => base_handles.push(h),
                    Err(e) => {
                        // 线程起不来 → 如实降级，绝不让界面卡住
                        tracing::warn!(error = %e, "波形基准层线程起不来，界面降级为不画波形");
                        break;
                    }
                }
            }
            drop(tx);
            if base_handles.is_empty() {
                self.shared = None;
                self.last_error = Some(i18n.t("err_wave_thread").to_string());
                return;
            }
        }

        // ---- 细化调度线程：一片一片地算用户当前看得见的那几片 ----
        let src2 = clip.src.clone();
        let cancel_tile = cancel.clone();
        let shared_tile = shared.clone();
        let ctx_tile = ui_ctx.clone();
        let panic_error_tile = wave_panic_error.clone();
        let spawned_tile = std::thread::Builder::new()
            .name("clip-wave-refine".into())
            .spawn(move || {
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // **一个解码器实例服务全部细化请求**（见 `waveform::RangeDecoder`）：
                // 打开 + 探测文件的固定开销只付一次，否则每片 5 秒音频的代价会被它翻倍。
                let mut dec = match waveform::RangeDecoder::new(&src2) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        tracing::warn!(error = %e, "波形细化无法打开素材（视图将一直用基准层）");
                        None
                    }
                };
                while let Ok(req) = req_rx.recv() {
                    // 每一次收到消息都先看一次取消标志：置位即退，不等下一条消息。
                    if cancel_tile.load(Ordering::Relaxed) {
                        break;
                    }
                    match req {
                        // **必定退出**：拆除方发来的收工消息（见 `RefineReq::Quit` 的说明）
                        RefineReq::Quit => break,
                        RefineReq::Tile { tile, bucket_ms } => {
                            if cancel_tile.load(Ordering::Relaxed) {
                                break;
                            }
                            let Some(dec) = dec.as_mut() else { break };
                            // 已经在缓存里（并发/重复请求）→ 不重复算
                            let cached = shared_tile
                                .lock()
                                .map(|g| {
                                    g.tiles
                                        .peek(tile)
                                        .map(|t| t.bucket_ms == bucket_ms)
                                        .unwrap_or(false)
                                })
                                .unwrap_or(true);
                            if cached {
                                continue;
                            }
                            if let Some(t) =
                                waveform::compute_tile(dec, tile, bucket_ms, &cancel_tile)
                            {
                                if let Ok(mut g) = shared_tile.lock() {
                                    // **换档就把别的档位的片丢掉**（P18）：绘制只按片号取值，
                                    // 缓存里留着上一档的同一个片号会被当成这一档画出来。
                                    g.tiles.retain_tier(bucket_ms);
                                    g.tiles.insert(t);
                                }
                                // **数据驱动重绘**：产出一块瓦片就唤醒界面一次。
                                // 没有周期性心跳、没有每帧轮询：没新瓦片时界面就是静止的。
                                wake_ui(&ctx_tile);
                            }
                        }
                    }
                }
                }));
                if ran.is_err() {
                    if let Ok(mut g) = shared_tile.lock() {
                        if g.error.is_none() {
                            g.error = Some(panic_error_tile.clone());
                        }
                    }
                    wake_ui(&ctx_tile);
                }
            });

        match spawned_tile {
            Ok(refine_handle) => {
                self.job = Some(WaveJob {
                    key,
                    cancel,
                    rx,
                    req_tx,
                    plan,
                    ui_ctx,
                    base_handles,
                    refine_handle: Some(refine_handle),
                    cancelled: false,
                });
            }
            Err(e) => {
                // 细化线程起不来 → 如实降级（基准层照常显示；`self.last_good` 已经留好）。
                //
                // 基准层 worker 这时还在跑：`req_tx` 仍在函数本地（通道不会断开），所以按
                // "置取消 → 发 `Quit` → 再 `join`"的**统一口径**收尾 —— 取消标志会被
                // 解码回调在每个包之间看到，最坏也只是一个包解完就退，
                // 绝不留下"不知道要等多久"的 join（P16 的教训）。
                tracing::warn!(error = %e, "波形细化线程起不来（放大后只有基准层）");
                cancel.store(true, Ordering::Relaxed);
                let _ = req_tx.send(RefineReq::Quit);
                reap_wave_handles(None, base_handles);
                self.shared = None;
                self.last_error = Some(i18n.t("err_wave_thread").to_string());
            }
        }
    }
    ///
    /// 调用点在绘制之后（界面已经知道这一帧要画哪一段）。四条节流：
    /// ① **档位由可见时长决定**（[`waveform::tier_for_visible_ms`]，不再由像素决定）——
    /// **按需请求细化**：把可见区间覆盖到、但还没算好的片排进队列（**带档位**，P18）。
    ///
    /// 调用点在绘制之后（界面已经知道这一帧要画哪一段）。三条节流 + 一条 P22 的口径：
    /// ① **档位由可见时长决定**（[`waveform::tier_for_visible_ms`]，不再由像素决定）——
    ///    换档就清空"已请求"并让缓存只留新档；
    /// ② 同一片（同一档）只发一次（`requested`）；③ 手停 [`REFINE_DEBOUNCE_MS`] 才发新请求；
    /// ④ 单次最多排队 [`waveform::TILE_REQ_MAX`] 片，且**离视图中心近的优先**
    ///    （用户眼睛盯着的就是中间那一段）。
    ///
    /// **P22：本函数还是"可见区间最优先"的入口** —— 无论可见区间长短，每帧都把
    /// 当前可见区间写进 [`BasePlan`]（[`WaveState::publish_visible`]），
    /// 于是基准层 worker 下一次领任务时就会挑"离用户正在看的那一段最近"的段。
    fn pump_refine(&mut self, visible: Option<(i64, i64)>, center_ms: i64) {
        let Some((a, b)) = visible else { return };
        self.publish_visible(a, b);
        let Some(job) = self.job.as_ref() else { return };
        if b <= a {
            return;
        }
        // **只有"可见区间 <= 门槛"才发密集瓦片请求**（P22）：更长的视口由基准层渲染，
        // 那里排瓦片等于给 1 小时的片子排几百个 5 秒瓦片，纯属白烧 CPU。
        if (b - a) as f64 > waveform::DENSE_MAX_VISIBLE_MS {
            return;
        }
        // **档位跟着可见时长走**：`VISIBLE_BUCKETS` = 600 的单一来源
        let tier = waveform::tier_for_visible_ms((b - a) as f64);
        if self.last_refine_tier != Some(tier) {
            self.last_refine_tier = Some(tier);
            // 换档：旧档的"已请求"对新档无效；缓存也丢掉别的档位的片（只按片号取值）
            self.requested.clear();
            if let Some(shared) = self.shared.as_ref() {
                if let Ok(mut g) = shared.lock() {
                    g.tiles.retain_tier(tier);
                }
            }
            // 换档是一次"真的有新东西要看"的事件 —— 本帧立刻发请求，不吃防抖
            self.last_refine_at = None;
        }
        let now = std::time::Instant::now();
        if let Some(last) = self.last_refine_at {
            if now.duration_since(last).as_millis() < REFINE_DEBOUNCE_MS {
                return;
            }
        }
        let first = waveform::tile_of_ms(a.max(0));
        let last = waveform::tile_of_ms(b.max(0));
        // **修剪"已请求"**（P20）：瓦片缓存是有界的 LRU（[`waveform::TILE_CACHE_MAX`] 片），
        // 而可见区间放大到门槛（120 s）时要 24 片、平移一下又会换一批 —— 被 LRU 淘汰的片
        // 若还留在"已请求"里，就再也**不会**被重新请求，可见区间里会留下一块永远不细化的兜底。
        // 所以每帧把"既不在缓存里、也不在途"的键删掉，让它们下一轮重新排队。
        //
        // 为什么删掉在途的键没关系：后台线程产出一片会 `wake_ui` 唤醒界面，
        // 下一帧这一片就已经在缓存里了（数据驱动，不是靠"已请求"记着）。
        if !self.requested.is_empty() {
            let cached: std::collections::HashSet<(usize, i64)> = self
                .shared
                .as_ref()
                .and_then(|s| s.try_lock().ok())
                .map(|g| g.tiles.keys().map(|k| (*k, tier)).collect())
                .unwrap_or_default();
            if !cached.is_empty() {
                self.requested.retain(|k| !cached.contains(k));
            }
        }
        // 以"视图中心所在的片"为中心向两侧扩散（近的优先）
        let center_tile = waveform::tile_of_ms(center_ms.max(0));
        let mut want: Vec<usize> = (first..=last).collect();
        want.sort_by_key(|t| (*t as i64 - center_tile as i64).unsigned_abs());
        let mut sent = 0usize;
        for tile in want {
            if sent >= waveform::TILE_REQ_MAX {
                break;
            }
            // 已经算好 / 已经请求过的片直接跳过（这是"平移回来命中缓存"的关键）
            if self.cached_tile(tile, tier) || self.requested.contains(&(tile, tier)) {
                continue;
            }
            if job
                .req_tx
                .send(RefineReq::Tile {
                    tile,
                    bucket_ms: tier,
                })
                .is_ok()
            {
                self.requested.insert((tile, tier));
                sent += 1;
            }
        }
        if sent > 0 {
            self.last_refine_at = Some(now);
        }
    }

    /// **把当前可见区间交给基准层计划**（P22 的"可见区间最优先"）。
    ///
    /// 每帧调用一次，代价是两次 relaxed store（**没有锁、没有分配**）：
    /// 基准层 worker 在下一次领任务时会挑"离这个区间最近"的段，
    /// 于是打开时先算视口那一段、平移/缩放时"新进入的区间"插队。
    ///
    /// 顺手把它记进 [`WaveState::last_visible`]：这是**下一次**起 worker 时的预热值
    /// （见 [`WaveState::spawn_job`]）。不记的话，换文件 / 关掉再打开时
    /// 那 4 条线程会在界面第一次 `publish_visible` 之前就领走 0/1/2/3 段。
    fn publish_visible(&mut self, a: i64, b: i64) {
        self.last_visible = Some((a, b));
        if let Some(job) = self.job.as_ref() {
            job.plan.set_visible(a, b);
        }
    }

    /// 这一片在**这一档**下是否已经在密集缓存里（拿不到锁就当作"没有"，下一帧再说）。
    ///
    /// 档位必须一起比：缓存里可能还留着别的档位的同一片号（换档是异步的），
    /// 只比片号会把"旧档的数据"误判成"这一档已经有了" —— 那就永远不重算了。
    fn cached_tile(&self, tile: usize, bucket_ms: i64) -> bool {
        match self.shared.as_ref().and_then(|s| s.try_lock().ok()) {
            Some(g) => g
                .tiles
                .peek(tile)
                .map(|t| t.bucket_ms == bucket_ms)
                .unwrap_or(false),
            None => false,
        }
    }

    /// **非阻塞地收割波形进度**：把快照留下、回收结束的线程，并回报"这一帧发生了什么"。
    ///
    /// 每帧调用一次。为什么把"日志/缓存/提示"留在 [`EditorPage`] 那边：那些是**页面的**
    /// 副作用（要写 `notice`、要落内存缓存），而本函数只负责"任务状态机"这一件事。
    fn poll_waves(&mut self) -> WavePolled {
        let mut out = WavePolled::default();
        let Some(job) = self.job.as_ref() else {
            return out;
        };
        let key = job.key.clone();
        // 通道里只传"进度变了"；把消息排空即可（快照本体在 `shared` 里取）。
        while job.rx.try_recv().is_ok() {
            out.got_message = true;
        }
        let (finished, err, codec, cache_hit) =
            match self.shared.as_ref().and_then(|p| p.try_lock().ok()) {
                Some(g) => (g.finished, g.error.clone(), g.codec.clone(), g.cache_hit),
                None => (false, None, String::new(), false),
            };
        // 本帧能读到的最新快照先留下（换文件时 `cancel_current` 会清掉它）
        if let Some(s) = self.snapshot() {
            self.last_good = Some(s);
        }

        if finished {
            // **只回收基准层阶段**（P15/P22）：worker 已经退出（`finished` 就是最后一个
            // 退场的 worker 写的），任务本体（`shared` + 细化线程 + 请求队列）**留着** ——
            // "按需细化"必须在基准层之后继续工作（可见区间 <= 120 s 时，
            // 画面用的正是细化线程产出的**密集档位**瓦片）。
            // 旧实现把整个 `job` take 掉，于是"整片算完"那一刻细化就永久停摆。
            if let Some(job) = self.job.as_mut() {
                // worker 已经结束：回收句柄也交给 reaper，避免任何边界情况卡 UI。
                let handles = std::mem::take(&mut job.base_handles);
                reap_wave_handles(None, handles);
            }
            // **整片补齐的耗时**（P22 验收要贴的那个数字）
            if self.base_done_at.is_none() {
                if let Some(t0) = self.opened_at {
                    self.base_done_at = Some(t0.elapsed());
                }
            }
            // **落盘**：只写完整的层，且只写一次（288 KB 的写盘不该每帧做）
            if !self.base_saved {
                if let Some(base) = self.last_good.clone() {
                    if base.is_complete() {
                        let t0 = std::time::Instant::now();
                        self.disk.save(&key, &base);
                        self.base_saved = true;
                        crate::ui::startup::mark_with(
                            "cache_save",
                            "wave",
                            Some(t0.elapsed()),
                            Some(format!(
                                "key={} slices={} dur={}ms",
                                key.file_name(),
                                base.slice_count(),
                                t0.elapsed().as_millis()
                            )),
                        );
                    }
                }
            }
            out.finished = true;
            out.codec = codec;
            out.cache_hit = cache_hit;
            out.set = self.last_good.clone();
            out.error = err;
        }
        out.key = Some(key);
        out
    }

    /// 把结果放进内存缓存。
    ///
    /// **只保留同一个文件的条目**：1 小时的基准层 ≈ 288 KB，但"同一文件只算一次"的语义
    /// 仍然由"键含路径 + 大小 + 修改时间 + 算法版本"保证。上限 [`WAVE_CACHE_MAX`] 只是护栏。
    fn remember(&mut self, key: waveform::CacheKey, base: Arc<BaseLayer>) {
        if self.cache.len() >= WAVE_CACHE_MAX {
            self.cache.clear();
        }
        self.cache.retain(|k, _| k.path == key.path);
        self.unavailable.retain(|k, _| k.path == key.path);
        self.cache.insert(key, base);
    }

    /// 记住"这个文件读不出波形"（避免每次重开都重试一遍解码）。
    fn remember_unavailable(&mut self, key: waveform::CacheKey, reason: String) {
        if self.unavailable.len() >= WAVE_CACHE_MAX {
            self.unavailable.clear();
        }
        self.cache.retain(|k, _| k.path == key.path);
        self.unavailable.insert(key, reason);
    }
}

/// [`WaveState::poll_waves`] 的回报：这一帧波形任务发生了什么。
#[derive(Debug, Default, Clone)]
struct WavePolled {
    /// 本帧收到了后台消息（没有的话 `ui()` 要安排一次兜底重绘）
    got_message: bool,
    /// 任务在本帧结束
    finished: bool,
    /// 结束时的缓存键（用于入库 / 记"读不出来"）
    key: Option<waveform::CacheKey>,
    /// 成功时的完整基准层快照
    set: Option<Arc<BaseLayer>>,
    /// 失败原因
    error: Option<String>,
    /// 音轨编码名（只用于日志）
    codec: String,
    /// 基准层是否来自落盘缓存（只用于日志）
    cache_hit: bool,
}

/// 一张已上传为纹理的缩略图（**P21 起只服务胶片条**：键 = 量化后的取样时刻）。
struct Thumb {
    /// **取样时刻**（毫秒，未量化：落位用它，保证贴边精确）
    at_ms: i64,
    /// 它在本次取样序列里的**下标 k**（0..n）：落位由 [`strip_layout`] 给出的
    /// `lefts[k]` 决定 —— **P25 起不再用"锚点系数 × 块宽"**（画面宽按行高等比，
    /// 落位与宽度解耦，由"首贴左 + 等间隙"这条口径唯一确定）。
    idx: usize,
    /// 这张纹理的像素尺寸（**P27**：兜底链第②级"借用邻块画面"时要把它
    /// 原样复制成一份 [`CachedThumb`]，所以尺寸必须跟着走，不能只留一个句柄）。
    w: u32,
    h: u32,
    tex: egui::TextureHandle,
}

/// 交给解码线程的任务：解哪个文件的哪一刻、按多大的上限解。
///
/// 为什么要带尺寸：预览是**流式**的，窗口越大解码尺寸越大。尺寸按
/// [`THUMB_BUCKET_STEP`] 量化成"档位"，同档位复用缓存，所以拖动窗口不会反复重解。
struct ThumbJob {
    path: PathBuf,
    at_ms: i64,
    /// 解码时的长/宽上限（保持源比例，由 `edit::thumbnail` 负责等比缩放）
    max_w: u32,
    max_h: u32,
    /// 该任务对应的**尺寸档位**（= `max_w`）：缓存 key 的一部分，回包时原样带回
    bucket_w: u32,
    /// **该任务要落到哪个缓存表**（P24）：`None` = 当前帧预览那一侧（写 `kf_thumbs`），
    /// `Some(键)` = 胶片条那一侧（写 `strip_thumbs`，键里已经带好量化后的高度）。
    ///
    /// 为什么让任务自己带：同一个解码线程同时服务两种目标，回包时**必须**知道
    /// "这一张属于胶片条的哪个高度档"，否则只能瞎猜 —— 猜错就是把 52 px 的画面
    /// 写进 40 px 的键（P24 明确禁止的"互相污染"）。
    strip_key: Option<StripThumbKey>,
    /// **P27 ①：方向搜索的探查时刻**（按距离由近到远，**已经是有界的**）。
    ///
    /// `at_ms` 解不出来时，解码线程按这个顺序逐个重试，**命中第一个就停**。
    /// 列表由 [`strip_fallback_probes`] 这一条纯函数算出来（窗口、距离、次数、
    /// "不得跨过相邻块取样点"全在那里定），任务只负责"照着试"——
    /// 这样"回退有没有界"可以在不碰 FFmpeg 的前提下被断言。
    /// 当前帧预览那一侧不搜索（空表）。
    alts: Vec<i64>,
    /// **P27 ②：这一块在同一条（胶片条）里的下标 k**。
    ///
    /// 失败时"就近复用另一个已成功块的画面"要知道自己是第几块；
    /// 预览那一侧为 `None`。
    strip_idx: Option<usize>,
}

/// 解码线程的回报（`frame` 为 Err 时只提示，不影响剪辑）
struct ThumbJobResult {
    path: PathBuf,
    at_ms: i64,
    bucket_w: u32,
    /// 见 [`ThumbJob::strip_key`]
    strip_key: Option<StripThumbKey>,
    /// 见 [`ThumbJob::strip_idx`]
    strip_idx: Option<usize>,
    /// **实际解出画面的时刻**（毫秒）：方向搜索命中时会**不等于** `at_ms`。
    ///
    /// 只用于日志与纹理命名；**缓存键仍然用请求时刻**（`at_ms` / `strip_key`）——
    /// 这是"请求与查表必须走同一个量化函数"那条纪律的直接要求
    /// （否则会出现"每次都请求成功、却永远查不到缓存"的无限请求循环，见 [`strip_key_ms`]）。
    decoded_ms: i64,
    frame: Result<(u32, u32, Vec<u8>), String>,
}

/// 鼠标附近关键帧的缩略图缓存项。
///
/// 为什么存纹理而不是留原始 RGBA：纹理已经在显存/egui 纹理管理器里，
/// 每帧只用 `painter.image(tex.id(), ..)` 贴一下，既不用重新 `load_texture`
/// （每帧上传 160×90×4 字节是纯浪费），也不必再占一份同样的内存。
/// "同一 clip + 同一时间点 + 同一尺寸档只解码一次"由 `kf_thumbs` 的 key
/// `(时间点, 尺寸档)` 保证（尺寸档见 [`THUMB_BUCKET_STEP`]）。
struct CachedThumb {
    w: u32,
    h: u32,
    tex: egui::TextureHandle,
}

/// 当前正在拖动的对象
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    Start,
    End,
    Cursor,
    Pan,
}

/// **播放会话**（`EditorPage::play` 为 `Some` 就是"正在播放"）。
///
/// 时钟口径：`pos_ms` 由**每帧的 `unstable_dt` 累加**推进，而不是"锚点时刻 + 墙钟差"。
/// 两者在正常帧率下等价，但 `dt` 累加有个决定性的好处：**可测** ——
/// 无窗口单测里 `RawInput::time` 是自己给的，播放位置因此完全确定，
/// 不需要真的睡、也不需要注入假时钟。
///
/// 帧的来源是 [`edit::player`] 那条常驻解码线程：它按 pts 顺序吐 RGBA 帧，
/// 这里只负责"按当前播放位置挑一帧贴出去"，**从不等待**它。
///
/// 音频是**平行的另一条链**（`audio` 字段）：它有自己的解码线程与渲染设备，
/// 与视频链只在界面侧的两个点同步——`start_play` / `reanchor_play` 把同一个 `t`
/// 同时发给两条链。音频链的任何失败都只降级为"没声音"，**不影响播放本身**
/// （所以这里连错误出口都没有，见 `edit::audio_out` 模块文档纪律 2）。
struct PlayState {
    /// 解码线程句柄（`None` = 只有时钟、没有画面；单测里就是这一档，不碰真文件）
    player: Option<edit::player::PlayerHandle>,
    /// 音频输出句柄（`None` = 本端没声音：没有音轨 / 输出设备不可用 / 测试档）
    audio: Option<edit::audio_out::AudioTrackHandle>,
    /// 当前播放位置（毫秒，浮点：避免整数累加把 30 fps 的步长截成 33/34 交替）
    pos_ms: f64,
    /// 已解出、还没贴出去的帧（按 `pts_ms` 升序）
    queue: VecDeque<edit::player::FrameRgba>,
    /// 当前贴在预览画框上的纹理（同尺寸时**原地 set**，不重建纹理）
    tex: Option<egui::TextureHandle>,
    /// 当前纹理对应的媒体时刻（毫秒）—— 预览下方那行时间码用它
    shown_ms: i64,
    /// 上一次**真的**发给解码线程的 seek 目标（毫秒）。
    ///
    /// 拖动"全片位置行"时每帧都会重新锚定，但每帧都 `seek` 等于每帧 flush 一次解码器
    /// （画面反而更卡），所以只在目标挪了 [`PLAY_SEEK_MIN_MS`] 以上时才真的发 seek。
    last_seek_ms: i64,
}

/// 仅测试用：最近一帧里几个关键部件的屏幕矩形。
///
/// 为什么需要它：egui 的命中测试用的是**上一帧**登记的矩形，无窗口环境里没有真鼠标，
/// 单测只能靠这些矩形算出"指针该放在哪"，再通过 `RawInput` 喂进去。
#[cfg(test)]
#[derive(Debug, Default)]
struct TestRects {
    timeline: Option<egui::Rect>,
    /// 时间线内部的**刻度尺**区域（点击它也应该设置当前标尺）
    ruler: Option<egui::Rect>,
    /// 时间线内部的**轨道**区域（点击它也应该设置当前标尺）
    track: Option<egui::Rect>,
    /// 蓝色当前标尺的把手矩形（拖它 = 移动当前标尺）
    cursor_handle: Option<egui::Rect>,
    set_start_btn: Option<egui::Rect>,
    set_end_btn: Option<egui::Rect>,
    /// 「添加片段」按钮矩形（P41：与另三颗按钮同台，颜色断言要逐个量）
    add_seg_btn: Option<egui::Rect>,
    /// 「删除片段」按钮矩形（P41）
    del_seg_btn: Option<egui::Rect>,
    /// **当前帧预览**画框（黑底那一块）的实际屏幕矩形：流式尺寸断言全靠它
    preview_frame: Option<egui::Rect>,
    /// 导出按钮矩形（**不能被挤出视野**：它必须落在页面可见矩形里）
    export_btn: Option<egui::Rect>,
    /// 「播放 / 停止」按钮矩形（在导出按钮**正上方**）
    play_btn: Option<egui::Rect>,
    /// 播放按钮**上方**那行时间码的矩形
    play_timecode: Option<egui::Rect>,
    /// 底部"文件信息"行面板的矩形（与下面那行要紧邻）
    file_info: Option<egui::Rect>,
    /// 底部"操作指南"行面板的矩形
    help: Option<egui::Rect>,
    /// 本帧用于算预览尺寸的**可用高度 / 可用宽度**（含下方固定内容的扣除）
    preview_avail_h: Option<f32>,
    preview_avail_w: Option<f32>,
    /// 本帧预览实际用的**解码尺寸档**（窗口拖动没跨档 → 这个值不变 → 不会重解）
    preview_bucket: Option<u32>,
    /// 本帧波形用的**分辨率层**（true = 密集层，false = 基准层）—— P22 起还另存 precise 层
    wave_detail: Option<bool>,
    /// **P22**：本帧波形用的是哪一层（`None` = 没画）
    wave_layer: Option<ViewLayer>,
    /// 本帧真正画出的波形线段数（渐进语义的断言靠它）
    wave_segments: Option<usize>,
    /// 本帧第一段 / 最后一段波形在线上的**屏幕 x**（与刻度尺同源对齐的断言靠它）
    wave_first_x: Option<f32>,
    wave_last_x: Option<f32>,
    /// 本帧需要细化（但还没算好）的时间区间
    wave_refine_ms: Option<Option<(i64, i64)>>,
    /// **P18**：本帧选中的**桶宽档位**（毫秒；0 = 本帧没走密集层）
    wave_tier_ms: Option<i64>,
    /// **P14/P17**：时间码行**下方**那一整行"全片位置行"的矩形
    /// （与时间码行同宽，左右边界对齐；点/拖它 = 移动当前帧）。
    /// **P17 起整行就是命中区域**（小方块与它的 48×16 命中框已经删掉）。
    pos_row: Option<egui::Rect>,
    /// **P5**：底部操作指南行**能用的内宽**（真渲染时拿到的 `max_rect` 宽度）。
    /// 单测用它断言"整句在没有 `…` 的前提下放得下"。
    help_avail_w: Option<f32>,
    /// **P19/P21**：本帧**真正画出来**的胶片条块矩形（每块的宽高必须与缩放无关）。
    strip_tiles: Option<Vec<egui::Rect>>,
    /// **P21**：本帧胶片条用的**取样时刻**（由可见区间直接算出，不再有网格）。
    strip_grid: Option<Vec<i64>>,
    /// **P21**：本帧每块的**左边界锚点系数**（0 = 贴左边界，1 = 右对齐）。
    strip_anchors: Option<Vec<f32>>,
    /// **P21**：本帧按规则算出的块数（5/4/3/2/1）。
    strip_count: Option<usize>,
    /// **P25**：本帧每个画面的宽度 `行高 × 源宽高比`（contain 口径的唯一来源）。
    strip_pic_w: Option<f32>,
    /// **P36**：本帧片段芯片行每个芯片的矩形（拖拽调序的单测靠它定位按下/松手点）。
    /// 片段芯片矩形（拖拽测试按芯片中心定位）；**同一行时所有 top 相同**
    seg_chips: Vec<egui::Rect>,
    /// 「片段列表」行首标签矩形（P39；`None` = 本帧没有片段、标签不画）。
    /// 守门测试用它断"标签在第一个芯片左边、且与芯片同一行"。
    seg_label: Option<egui::Rect>,
}

/// 剪辑页面。
///
/// **对外契约**（主页面按黑盒使用）：
/// * `EditorPage::new(&Config)` —— 用配置初始化（默认导出目录等）；
/// * `EditorPage::ui(&mut Ui)` —— 渲染整页；
/// * `EditorPage::open(&Path)` —— 由主页面在停止录制后把视频路径交给剪辑页。
pub struct EditorPage {
    clip: Option<Clip>,
    /// 选区原始值（毫秒，未吸附）；吸附只用于显示与导出
    sel_start_ms: i64,
    sel_end_ms: i64,

    /// **片段列表（多段合并导出）**：`(start_ms, end_ms)` 原始值（未吸附），
    /// **顺序 = 播放顺序**，由用户拖拽决定（P36 起不再强制按时间排序）。
    ///
    /// 与 `sel_start_ms`/`sel_end_ms` 的关系（镜像，参照 C 版的 `segments_` + `inSec_/outSec_`）：
    /// * `segments[active_seg]` 就是**当前正在编辑的那一段**，与 `sel_*` 两个字段互为镜像
    ///   ——「设为起点/终点」、拖把手、键盘 `[`/`]`、时间码输入框全部只写 `sel_*`，
    ///   每帧由 [`EditorPage::sync_active_segment`] 同步回列表；
    /// * 切换 `active_seg` 时由 [`EditorPage::mirror_active_segment`] 把该段值读回 `sel_*`；
    /// * **列表为空时**导出走历史单段路径（`export_copy`，行为一字不变）；
    ///   **非空时**导出走 `edit::export_ranges`，按列表顺序合并。
    ///
    /// **可以为空**（P36 改）：空 = 单段模式，用户完全没碰过多段功能，
    /// 走的就是改造前那条路径。删除最后一段会回到这个状态。
    /// 与之配套，`active_seg` 只在列表非空时有意义（空时所有读写都被守卫挡掉）。
    ///
    /// **允许乱序 / 重叠 / 重复**：这是播放列表，不是"把影片切成几块"。
    /// 同一段加两次就是播两遍（`edit::resolve_segments_ex` 有单测钉着）。
    segments: Vec<(i64, i64)>,
    /// 当前编辑的分片下标（`segments` 的下标；**`None` = 没有选中任何段**）。
    ///
    /// 为什么是 `Option`（P36 改）：`sel_*` 有两个身份 ——
    /// * 选中某段时，它是**那段值的镜像**（改它 = 改那段）；
    /// * 没有选中时，它是**一段独立的待添加选区**（改它不动列表）。
    ///
    /// 这个区分是"连续选段、连续添加"能work的前提：点「添加区间」后主动清空选中，
    /// 于是用户接着拖把手选下一段时，不会把刚加进去的那段改成新选区。
    /// （改造前 `active_seg` 恒有效且列表恒非空，"加完再选"必然覆盖刚加的段。）
    active_seg: Option<usize>,
    /// **拖拽排序**：正在被拖的那一行下标（`None` = 没在拖）。
    ///
    /// 只在 `segments` 非空时有意义。松手时按指针位置算出的目标位插入，
    /// 然后清空。放在页面上而不是 egui 内存里，是因为拖拽跨越多帧，
    /// 且要能在 `open()` 换文件时被无条件重置（见那里的 `seg_drag = None`）。
    seg_drag: Option<usize>,
    /// **当前标尺**（毫秒）。初值 = 媒体时长的一半（`open()` 时按新素材重置），
    /// 时长未知时为 0。**只有点击时间线、拖蓝色把手、键盘微调会改变它**；
    /// 鼠标悬停/拖动都**不会**写这个字段（预览只看它；见模块文档里的交互契约）。
    cursor_ms: i64,
    /// 用户是否已经**主动定位过**当前标尺（点击时间线 / 拖动把手 / 键盘微调）。
    ///
    /// 为什么要这个标记：`cursor_ms` 的初值是"整片中间"，那是中性占位、不是用户选的点。
    /// 在用户第一次点击之前，「设为起点/终点」按钮**禁用**并提示"请先在时间线上点击选点"——
    /// 这样任何情况下都不可能把"悬停位置"或"默认中间值"误当成用户想标记的点。
    cursor_committed: bool,

    /// 时间线视图：每毫秒像素数（缩放）与视图左端时间（平移）。
    /// 用 f64 而不是 f32：去掉缩放上下限后，1 小时视频的铺满比例是 2.2e-4 这种量级，
    /// f64 让"铺满 / 深度缩放 / 平移"之间的换算不丢精度。
    px_per_ms: f64,
    view_start_ms: f64,
    /// true = "整片铺满"模式：每帧按当前可用宽度重算缩放，窗口宽度一变就跟随。
    /// 滚轮缩放或平移后自动置 false；缩放回到铺满比例时自动回到 true。
    fit_mode: bool,

    /// 本帧指针是否在时间线矩形内（**只用于滚轮缩放与键盘快捷键的生效条件**，
    /// 不驱动任何预览、也不画任何悬停反馈 —— 预览只看蓝色当前标尺）
    hover_live: bool,

    drag: Option<Drag>,

    /// **胶片条（P19/P21/P25）**：本帧要画的画面（可见区间里固定 5 个，放不下才减）。
    /// 取样时刻由 [`strip_samples`] 从可见区间**直接算出**（不再有网格），
    /// 每个画面**按行高等比缩放、完整显示**（contain，`行高 × 源宽高比`，P25）——
    /// 缩放只改变"可见区间里有几个画面"，绝不拉伸图像、也绝不裁掉画面的一部分。
    /// 纹理键 = [`StripThumbKey`]（`(量化时刻, 解码宽, 画面高, 口径版本)`），
    /// 存在下面的 `strip_thumbs`；落位由 [`strip_layout`] 给出（首贴左、尾贴右、等间隙）。
    strip_prev: Vec<Thumb>,
    /// **胶片条解码纹理缓存**（P24 起与"当前帧预览"分开两张表）。
    ///
    /// 键 = [`StripThumbKey`] = `(量化时刻, 160, 量化后的画面高, 口径版本)`。为什么要独立一张表
    /// 而不是复用 `kf_thumbs`：P24 要求**高度进键**，而预览的键里尺寸是"按画框宽度算出来的
    /// 档位"、与胶片条根本不是一个坐标系；合成一张表只会让两边的键语义互相污染。
    /// 两张表共用同一个解码线程与同一条"单张在途"纪律（`thumb_pending`），
    /// 所以"胶片条解过的画面"与"预览解过的帧"仍然是**一次解码**，不会重复花钱。
    strip_thumbs: HashMap<StripThumbKey, CachedThumb>,
    /// **P27 兜底帧表**：方向搜索、邻块、缓存三级都试过之后"借来的画面"。
    ///
    /// 为什么不直接写进 `strip_thumbs`：那张表的语义是"**这一刻真的解出来的画面**"，
    /// 借来的画面属于另一个时刻，混进去会让"缓存里到底有没有这一帧"变得不可判定
    /// （也会污染 [`EditorPage::newest_strip_thumb`] 那条"最新一张真画面"的口径）。
    /// 分成两张表之后：**绘制**先查真画面、再查兜底帧；**解码**只认真画面表。
    ///
    /// 用户口径（P27 补充）：**只要文件里存在任意一帧可解，5 个槽位就不许空着**。
    strip_fallback: HashMap<StripThumbKey, CachedThumb>,
    /// **P27**：`strip_thumbs` 的**插入顺序**（末尾 = 最新一张真画面）。
    ///
    /// `HashMap` 没有顺序，而兜底链第③级要的是"缓存里**最近一张**成功的缩略图"
    /// —— 所以另记一条顺序表。它只存键（很小），并且与 `strip_thumbs` **同生共死**
    /// （同一处清空、同一个上限、查表时再确认键还在）。
    strip_thumb_order: Vec<StripThumbKey>,
    /// 是否允许胶片条**真的排解码任务**。
    ///
    /// 生产里恒为 `true`；headless 测试台把它置 `false`，这样"帧间布局"完全确定，
    /// 测试也不会去碰不存在的素材文件（要测请求行为的用例再单独打开它）。
    strip_requests_enabled: bool,

    /// 缩略图解码管道 + 缓存（**当前帧预览**那一侧；胶片条见 `strip_thumbs`）。
    /// key = `(取样时刻, 解码尺寸档像素宽)` —— 尺寸进 key，窗口拖动 1px 不会重解。
    kf_thumbs: HashMap<(i64, u32), CachedThumb>,
    thumb_job_tx: Option<Sender<ThumbJob>>,
    thumb_job_rx: Option<Receiver<ThumbJobResult>>,
    /// 已投递、还没回来的 `(时间点, 尺寸档)`（同一时刻只允许一张在途，见 `request_thumb`）
    thumb_pending: Option<(i64, u32)>,
    /// **解不出来的键**（P21）：失败过的键**不再重试**。
    ///
    /// 为什么必须有它：取样时刻可能落在"没有任何一帧"的位置（片尾那一格、损坏的 GOP）。
    /// 没有这张表的话，界面每帧都会为同一个时刻再排一次解码、解码再失败，
    /// 于是"在途任务"永远非空 → `ui()` 每 50 ms 安排一次兜底重绘 ——
    /// 观感就是"胶片条把界面拖成了永动机"（P21 明确要求不得引入每帧重绘）。
    /// 失败的那一格如实画成占位块（与"还没解码"同一个样子），绝不假装有画面。
    ///
    /// 键 = `(时刻, 尺寸档像素宽, 量化后的块高)`：**高度也进失败表**（P24）——
    /// 否则"在 52 px 这一档解不出来"会把另一个高度的同一时刻也永久判死。
    thumb_failed: std::collections::HashSet<(i64, u32, u32)>,

    /// 导出
    exporting: bool,
    export_rx: Option<Receiver<(u64, Result<ExportReport, crate::error::AppError>)>>,
    /// 导出世代：打开新素材时递增，旧导出即使晚到也不能更新新素材的界面。
    export_epoch: u64,
    /// 编辑页导出的协作取消令牌（切换素材时置位，旧线程在包边界退出）。
    export_cancel: Option<Arc<AtomicBool>>,
    report: Option<ExportReport>,

    /// **播放**（`Some` = 正在播放）。见 [`PlayState`]。
    play: Option<PlayState>,
    /// 是否允许真的起解码线程。
    ///
    /// 生产里恒为 `true`；无窗口单测把它置 `false` —— 于是"时钟推进 / 翻页 / 停止"
    /// 这些**纯逻辑**可以完全确定地断言，而不必去碰一个不存在的素材文件。
    play_decode_enabled: bool,
    /// 仅测试用：音频链是否真的连接输出设备。
    play_audio_output: bool,
    /// 上一帧预览画框的宽度（逻辑点）。
    ///
    /// 播放的解码尺寸由它决定（`thumb_bucket_w` 量化）：按"画框多大就解多大"，
    /// 1080p 素材在 600 px 的画框里就只解 600 px 宽，省下的都是白得的帧率。
    preview_frame_w: f32,

    error: Option<String>,
    notice: Option<String>,
    /// **录制输出目录**（来自 `Config::output_dir`，由主界面用 [`EditorPage::set_output_dir`]
    /// 保持最新）：它是「打开视频」对话框的起始位置，也是导出对话框的默认目录。
    output_dir: String,
    /// 最近一次录制/打开的视频所在目录（`Config::last_video_path` / [`EditorPage::open`]）：
    /// `output_dir` 不可用时用它给「打开视频」的文件对话框兜底定位（P46 起**只**做这件事，
    /// 不再用来"找最近一条自动载入"）。
    last_video_dir: Option<PathBuf>,

    /// **剪辑页是否显示音频波形**。**P36 起恒为 `true`** —— 用户要求"波形成为必选"，
    /// 编辑器里不再提供开关（`apply_opened_clip` 每次打开素材都强制开启）。
    ///
    /// 字段仍保留（而不是删掉改成常量）：绘制路径、后台任务与 `Config` 之间
    /// 还有一批 `show_waveform` 判断，删掉它要动的地方远多于收益。
    /// 它是 `pub` 的：主界面在外壳那一侧读它回填 `Config`（与 [`EditorPage::wave_cfg_changed`]
    /// 配对使用），避免为了一个 bool 再加一个 getter。
    pub show_waveform: bool,
    /// **剪辑页当前是否可见且不在录制**。
    ///
    /// 不可见时暂停波形/深度检查/播放：它们都是纯 CPU/磁盘的批处理，
    /// 在首页开始录制后继续跑会直接和视频编码抢核。
    active: bool,
    /// 音频波形的后台任务 / 缓存 / 渐进进度（见 [`WaveState`]）。
    wave: WaveState,
    /// **波形绘制的每帧取证聚合**：只有 `phase=wave_ui_draw` 那一条日志读它。
    ///
    /// 为什么要"攒起来每几秒记一条"而不是每帧一行：波形是每帧都画的，
    /// 每帧一行会把 `cap.log` 刷爆；而平均值足以回答"一帧画了多少段、花了多少毫秒"。
    wave_ui_frames: u64,
    wave_ui_segs: u64,
    wave_ui_cost: std::time::Duration,
    wave_ui_last_log: Option<std::time::Instant>,
    /// 波形开关被改过：交给外壳落盘（与设置页同一条路径）。
    ///
    /// **P36 起编辑器里不再有那个复选框**（用户要求"波形成为必选"），
    /// 所以页面自己不会置 `true`；字段与方法保留是因为外壳那侧仍在读它
    /// （恒 `false` = "本页没改过配置"，正是想要的行为）。
    wave_cfg_changed: bool,

    // ---- P23 · 「先导入视频，后计算波形」的阶段状态 ----
    /// **本次导入的起点**。`None` = 当前这份素材不处在这条两阶段链上
    /// （测试台直接塞 `clip`、或者用户把波形关掉了）。
    ///
    /// [`EditorPage::open`] 每次成功载入都置 `Some(..)`，导入就绪后置回 `None`。
    /// 它是"导入阶段是否还在进行"的**唯一权威**：只要它是 `Some`，
    /// [`EditorPage::check_wave_phase`] 就不会让波形任务起任何一个线程。
    import_started_at: Option<std::time::Instant>,
    /// **一次性的兜底看门狗**：到 [`IMPORT_READY_TIMEOUT_MS`] 时叫醒界面一次。
    ///
    /// 为什么它**不是心跳**：它由 [`EditorPage::arm_import_watchdog`] 在导入还没就绪时
    /// 才排一次下一拍，导入就绪（或超时兜底）之后**立刻置回 `None` 并且不再排下一拍**
    /// —— 排帧次数被 `[0, IMPORT_READY_TIMEOUT_MS]` 这个**有限区间**夹住
    /// （`p23_import_watchdog_is_bounded_and_stops` 钉住这一点）。
    /// 与波形那条链的"数据驱动重绘"是同一个目标：只在**还需要再看一眼**的时候要帧。
    import_watchdog: Option<std::time::Instant>,
    /// **已经排进 `WaveState::start` 的启动请求**（"导入就绪后自动开始"的那一笔）。
    ///
    /// `open()` / `set_show_waveform(true)` 置 `true`，两阶段落定时置 `false`。
    /// 用户把波形关掉时它被清掉 ⇒ **关掉就是关掉**，不会在导入就绪时自己长回来。
    wave_start_pending: bool,
    /// 导入阶段是否**已经落定**（判据成立，或走了兜底）。只用于日志去重与单测断言。
    editor_import_ready: bool,
    /// 波形**只有**在这个标记为 `true` 时才可以被启动。
    ///
    /// 这是"严格两阶段"的机械保证，也是"不得因此让波形永远不开始"的另一半：
    /// 若导入过程本身出问题（没有视频流 / 打开失败），
    /// [`EditorPage::finish_import_without_wave`] 会**照样**把它置 `true` ——
    /// 于是波形走既有降级路径收尾（没有音轨就如实记一行、不起线程、不 panic），
    /// 绝不出现"导入失败 → 波形阶段永远卡在门口"。
    wave_phase_done: bool,
    /// 导入开始那一刻的素材名（日志用：`cap.log` 里一行一个文件，便于对照取证）。
    import_log_name: String,
    /// **首帧预览是否已经渲染出一帧**（P23 判据二）。
    ///
    /// 由 [`EditorPage::paint_preview`] 在"真的把一张**已解码的纹理**贴进预览画框"时置位
    /// （画"解码中…"那句占位文字**不算**）。`open()` / 换文件时复位。
    /// 为什么不用"缓存里有这张图"当判据：那只能说"数据到了"，
    /// 用户要的是"**预览已经出画面**" —— 出画面这件事只有绘制那一步知道。
    preview_painted: bool,
    /// **仅测试**：最近一次的导入就绪判定（用了哪几个条件 / 是否兜底）。
    #[cfg(test)]
    import_decision: Option<ImportDecision>,

    /// **在途的流式打开**（P34）：后台线程"探测完成先交 clip、再续扫关键帧"的两条消息
    /// 都从这条通道回来。`None` = 当前没有正在打开的文件。
    ///
    /// 为什么打开要异步：`edit::open` 会 `av_read_frame` **从头读到尾**扫关键帧
    /// （只解封装不解码，但要把全集文件读进来）。长片/大文件上这是秒级甚至十秒级，
    /// 放在 UI 线程就是"整个窗口卡住几秒没反应"（用户 2026-10-18 实机反馈）。
    /// 拆成"探测即交 + 续扫后补"两段之后：首帧、胶片条、波形都不必等扫描。
    open_rx: Option<std::sync::mpsc::Receiver<OpenMsg>>,
    /// 正在打开的文件名（"正在打开…"提示用；`None` = 没有在途打开）。
    open_pending_name: Option<String>,
    /// **打开的第几轮**（每次 `open()` +1）：后台消息带上它，只有对得上号才应用 ——
    /// 否则"快速连开两个文件"时，第一个文件的迟到消息会把第二个文件的界面盖掉。
    open_epoch: u64,
    /// 当前打开/关键帧扫描任务的协作取消令牌（换文件时置位）。
    open_cancel: Option<Arc<AtomicBool>>,
    /// **关键帧扫描是否还在跑**（P34）：`true` 期间 `keyframes_ms` 可能不完整，
    /// **导出必须禁用** —— 不完整的表会让 copy 导出落在非关键帧上（画面起不来）。
    /// 扫描结果到达（或打开失败）时置 `false`。
    keyframes_scanning: bool,
    /// 本轮打开**起手那一刻**（P34 取证用）：关键帧扫回时用它算"扫描段耗时"，
    /// 与 `editor_import_begin` 的导入段耗时对起来就能看出两段各占多少。
    open_started_at: Option<std::time::Instant>,

    // ---- P35：编码体检 ----
    //
    // **结论只有一个真相源：`clip.health`**。快速体检（随打开一起跑，解封装级）
    // 与深度检查（全片解码，能抓花屏）的结果**都写进它**，
    // 于是播放跳过、绘制、导出剔除读的是同一份 —— 不会出现"界面说会跳过、
    // 导出却没剔"这种自相矛盾（这正是最初把深度结果单独放一个字段的坑）。
    /// 深度检查的结果通道（`Ok` = 报告，`Err` = 失败/取消的原因）
    deep_rx: Option<std::sync::mpsc::Receiver<Result<edit::health::HealthReport, String>>>,
    /// 深度检查的取消标志（换文件 / 离开页面时置位，线程最多再解一个包就退）
    deep_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 深度检查是否在跑（决定按钮文案与是否排兜底重绘）
    deep_checking: bool,
    /// 深度检查已解到多少毫秒（进度条用）
    deep_progress_ms: i64,
    /// 深度检查的进度**共享单元**（后台线程写、界面读：无锁，且不必经通道回传）
    deep_progress: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,

    /// **本页当前用的语言包快照**（P1：i18n 全覆盖）。
    ///
    /// 为什么要留一份而不是每次都用参数传：一大半文案是**后台回报时才生成**的
    /// （打开失败、波形不可用、缩略图解码失败、导出失败…），而生成它们的函数
    /// （`open` / `poll_thumbs` / `poll_export` / `start_export` / `finish_wave`）
    /// 由外壳在别处调用、**拿不到 `&I18n`**。留一份快照之后，这些地方就地就能取到
    /// 当前语言，不必把 `i18n` 一路透传进十几个签名。
    ///
    /// 初值按 `cfg.language` 建：**第一次 `open()` 之前**（比如用户点「打开视频」选了个坏文件）
    /// 就会产生"打开视频失败"这类文案，那时 `ui()` 还没跑过、还没机会刷新这个字段，
    /// 用默认语言就会提示错语言。之后每次 `ui()` 只在**语言真的变了**时刷新 ——
    /// 也就是只在设置页切语言那一帧付一次克隆（两张小 map），平时零开销。
    i18n: crate::i18n::I18n,

    /// 仅测试用：给无窗口单测定位部件（见 [`TestRects`]）
    #[cfg(test)]
    test_rects: TestRects,

    /// 本帧的布局测量（内容高度 / 视口高度 / 可用高度）。
    ///
    /// 每帧都记（不是 `cfg(test)`）：最小窗口尺寸的**整窗**验证在 `CapLite` 二进制里，
    /// 那里读不到库的 `cfg(test)` 符号，所以这是常规接口（理由见 [`LayoutProbe`]）。
    probe: LayoutProbe,
}

impl EditorPage {
    /// 构造剪辑页：**只初始化状态，绝不打开任何文件**（P46）。
    ///
    /// ## 为什么这里没有任何"自动载入"
    ///
    /// 改造前有**两条**自动载入路径，现在都按用户要求删掉了：
    /// ① 本函数按 `cfg.last_video_path` 载入"上次的视频"；
    /// ② `ui()` 里"没有素材就载入录制目录里最新一条"的旁路。
    ///
    /// 剪辑页现在**只接受显式打开**：录制页底部的「在剪辑页打开」
    /// （`src/main.rs::open_in_editor` → [`EditorPage::open`]）、本页的「打开视频」/「重新载入」。
    /// 好处不只是省掉启动时的 FFmpeg 开销（`avformat_open_input` + 扫关键帧会把 5 个 DLL
    /// 拉进进程并占用几十到几百毫秒），更重要的是：**剪辑页上看到的永远是用户自己刚刚
    /// 指定的那一条**，不会在启动或切页时被某个旧文件悄悄替换掉。
    ///
    /// `cfg` 仍然要用：它给出语言（本页自己留一份 `I18n` 快照）、输出目录、波形开关。
    pub fn new(cfg: &Config) -> Self {
        let me = Self {
            clip: None,
            sel_start_ms: 0,
            sel_end_ms: 0,
            // 片段列表：初始为空 = 单段模式（走改造前那条导出路径）。
            // 用户点「添加区间」才会往里加，见字段文档。
            segments: Vec::new(),
            active_seg: None,
            seg_drag: None,
            cursor_ms: 0,
            cursor_committed: false,
            // 初值随即可被"整片铺满"覆盖；给个正数是为了任何情况下都不会拿 0 去当除数
            px_per_ms: 0.05,
            view_start_ms: 0.0,
            fit_mode: true,
            hover_live: false,
            drag: None,
            strip_prev: Vec::new(),
            strip_thumbs: HashMap::new(),
            strip_fallback: HashMap::new(),
            strip_thumb_order: Vec::new(),
            strip_requests_enabled: true,
            kf_thumbs: HashMap::new(),
            thumb_job_tx: None,
            thumb_job_rx: None,
            thumb_pending: None,
            thumb_failed: std::collections::HashSet::new(),
            exporting: false,
            export_rx: None,
            export_epoch: 0,
            export_cancel: None,
            report: None,
            play: None,
            play_decode_enabled: true,
            play_audio_output: true,
            preview_frame_w: 0.0,
            error: None,
            notice: None,
            output_dir: cfg.output_dir.clone(),
            // 把"上次录制的目录"也记下来：输出目录万一解析不出来，「打开视频」还能从这里起步
            last_video_dir: cfg
                .last_video_path
                .as_deref()
                .map(Path::new)
                .and_then(|p| p.parent())
                .map(Path::to_path_buf),
            show_waveform: cfg.show_waveform,
            active: false,
            wave: WaveState::default(),
            wave_cfg_changed: false,
            wave_ui_frames: 0,
            wave_ui_segs: 0,
            wave_ui_cost: std::time::Duration::ZERO,
            wave_ui_last_log: None,
            // P23：初始**不在**导入阶段（还没有任何素材）。测试台直接塞 `clip` 时这几个字段
            // 保持这个初值 ⇒ 波形启动那条链完全不参与，既有测试的行为一字不变。
            import_started_at: None,
            import_watchdog: None,
            wave_start_pending: false,
            editor_import_ready: false,
            wave_phase_done: false,
            import_log_name: String::new(),
            preview_painted: false,
            #[cfg(test)]
            import_decision: None,
            // P34：初始没有在途打开、没有扫描中的关键帧表
            open_rx: None,
            open_pending_name: None,
            open_epoch: 0,
            open_cancel: None,
            keyframes_scanning: false,
            open_started_at: None,
            deep_rx: None,
            deep_cancel: None,
            deep_checking: false,
            deep_progress_ms: 0,
            deep_progress: None,
            // P1：**按配置里的语言**建初始语言包（不是 `default()`）——
            // 第一次 `open()` 之前就可能产生"打开视频失败"这类文案（P46 之后不再有
            // 自动载入，但用户手点「打开视频」选错文件是一样的情况），
            // 那时 `ui()` 还没跑过、还没机会刷新这个字段，用默认语言就会提示错语言。
            i18n: crate::i18n::I18n::new(&cfg.language),
            #[cfg(test)]
            test_rects: TestRects::default(),
            probe: LayoutProbe::default(),
        };
        me
    }

    /// 载入视频（分片 mp4 或普通 mp4 都可以，分片文件同样能扫关键帧）。
    ///
    /// **P34：本函数绝不阻塞界面**。它只做毫秒级的同步清理，然后把"探测 + 扫关键帧"
    /// 整段丢给后台线程（`clip-open`）：探测一完成就经通道把 clip 交回来
    /// （`EditorPage::poll_open` 应用它 → 首帧/胶片条/波形就此开工），
    /// 关键帧扫描随后补回来（`keyframes_scanning` 期间导出禁用）。
    ///
    /// 改造前这里是 `edit::open(path)` **同步**扫全文件的关键帧 —— 5 秒录屏是毫秒级，
    /// 长片/大文件就是秒级甚至十秒级，整个窗口卡住没反应（用户 2026-10-18 实机反馈）。
    ///
    /// **P23：本函数处在「导入阶段」，绝不启动波形任务**。导入阶段从"探测结果回来"
    /// 那一刻（`EditorPage::apply_opened_clip`）才正式开始。
    pub fn open(&mut self, path: &Path) {
        self.error = None;
        self.notice = None;
        self.report = None;
        // 换文件会让在途导出的结果失去归属：置取消、断开通道并递增 epoch。
        // 旧线程在包边界退出并删除 `.part`，不会再继续整文件拷贝。
        if let Some(cancel) = self.export_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        // 旧文件的关键帧扫描同样在包边界检查取消：快速连开大文件时，
        // 旧扫描线程不该继续读完整个旧文件、和第一次播放/波形抢磁盘。
        if let Some(cancel) = self.open_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.export_epoch = self.export_epoch.wrapping_add(1);
        self.export_rx = None;
        self.exporting = false;
        // **换片子必须停播放**：否则解码线程还在读上一个文件，界面却已经换成新的
        // （画面会"播着新片子、内容还是旧的"）。放在最前面：后面的清理可能提前 return。
        self.stop_play();
        self.strip_prev.clear();
        // 换片子就把缩略图的缓存与在途请求都作废（在途结果会被路径检查丢掉）
        self.kf_thumbs.clear();
        self.strip_thumbs.clear();
        // P27：兜底帧与"最新真画面"的顺序表与真画面表同生共死（换片子一律作废）
        self.strip_fallback.clear();
        self.strip_thumb_order.clear();
        self.thumb_pending = None;
        // 换片子也把"解不出来的键"作废：新素材的同一时刻完全可能是好的
        self.thumb_failed.clear();
        // 载入的这条往往就是"刚录的那条"，所以它的目录也更新成兜底目录
        // （下次「打开视频」的对话框从这里开始）。
        if let Some(dir) = path.parent() {
            self.last_video_dir = Some(dir.to_path_buf());
        }
        self.hover_live = false;
        // **换文件 → 取消上一个波形任务**（用户要求"不要泄漏线程"）。
        // 取消是协作式的（后台最多再解完当前一个 packet），所以这一步是毫秒级，
        // 绝不会让"打开文件"这个动作卡住界面。
        //
        // P23：这一步之后**没有** `wave.start(..)` —— 上一个文件的任务被收干净，
        // 新文件的波形任务要等"导入就绪"才允许起。
        self.wave.cancel_current();
        // 换片子 → 上一次的可见区间对新片子毫无意义（新片子默认"整片铺满"，
        // 而 `BasePlan` 的默认可见区间正是 `[0, 时长]`）。
        // 不清的话，`spawn_job` 的预热会拿**上一条片子**的第 50 分钟去给新片子排段。
        self.wave.last_visible = None;
        // ---- P23：导入阶段的状态先复位（起点等探测结果回来才记，见 apply_opened_clip）----
        self.import_started_at = None;
        self.import_watchdog = None;
        self.editor_import_ready = false;
        self.wave_phase_done = false;
        self.wave_start_pending = false;
        self.import_log_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.preview_painted = false;
        // ---- P34：界面进入"正在打开"状态（旧素材的显示一律清掉，不赖在那里）----
        self.clip = None;
        self.open_pending_name = Some(self.import_log_name.clone());
        // 新表没扫完之前：导出禁用（不完整的表会让 copy 导出落在非关键帧上）
        self.keyframes_scanning = true;
        // ---- P35：换文件 → 在跑的深度检查立刻停（结论随 `clip.health` 一起换掉）----
        self.cancel_deep();
        self.deep_progress_ms = 0;
        // 轮次 +1：旧文件在途的消息从此对不上号， Poll 时一律丢弃
        self.open_epoch = self.open_epoch.wrapping_add(1);
        self.open_started_at = Some(std::time::Instant::now());
        let epoch = self.open_epoch;
        let (tx, rx) = std::sync::mpsc::channel::<OpenMsg>();
        let p = path.to_path_buf();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        self.open_cancel = Some(cancel);
        let spawned = std::thread::Builder::new()
            .name("clip-open".into())
            .spawn(move || {
                match edit::open_streaming_cancel(
                    &p,
                    |clip| {
                        // 探测即交：界面据此起首帧/胶片条/波形，**不等关键帧扫描**
                        let _ = tx.send(OpenMsg::Clip(clip.clone(), epoch));
                    },
                    Some(&cancel_thread),
                ) {
                    Ok(Some((_clip, pass))) => {
                        let _ = tx.send(OpenMsg::Scan(pass, epoch));
                    }
                    Ok(None) => {
                        // 已被新一轮打开取消：不发送任何"完成"消息。
                    }
                    Err(e) => {
                        let _ = tx.send(OpenMsg::Failed(e.to_string(), epoch));
                    }
                }
            });
        match spawned {
            Ok(_) => self.open_rx = Some(rx),
            Err(e) => {
                // 线程起不来（几乎不可能）：如实降级为一次同步打开，
                // 至少"文件能打开、界面有解释"，绝不静默卡在"正在打开"。
                tracing::warn!(error = %e, "clip-open 线程起不来，退化为同步打开");
                self.open_rx = None;
                self.open_cancel = None;
                self.open_pending_name = None;
                match edit::open(path) {
                    Ok(clip) => {
                        self.keyframes_scanning = false;
                        self.apply_opened_clip(clip, path);
                    }
                    Err(e) => {
                        self.keyframes_scanning = false;
                        self.fail_open(&e.to_string());
                    }
                }
            }
        }
    }

    /// **探测结果应用到界面**（P34：流式打开的第一段落地）。
    ///
    /// 就是改造前 `open()` 里 `Ok(clip)` 那一支：复位视图状态、登记导入阶段起点。
    /// 此刻 `clip.keyframes_ms` **还是空的**（扫描在后台继续）—— 首帧预览与胶片条
    /// 不依赖它（见 [`thumb_keyframe`] 的未扫分支），波形 worker 也不依赖。
    fn apply_opened_clip(&mut self, clip: edit::Clip, path: &Path) {
        // 尊重用户的 `show_waveform` 设置：外壳每帧会把配置同步进来，这里不再强制打开。
        // 如果开关是关的，导入阶段不会登记波形任务，也就不会白烧 4 条解码线程。
        self.sel_start_ms = 0;
        self.sel_end_ms = clip.duration_ms.max(0);
        // 换素材 → 片段列表清空，回到**单段模式**（历史行为：默认整片全选）。
        // 刻意不再塞一段 `(0, duration)`：空列表就是"用户没碰过多段功能"，
        // 导出走的仍是改造前那条 `export_copy` 路径，行为一字不变。
        self.segments.clear();
        self.active_seg = None;
        // 拖拽状态必须一起清：否则换文件后指针一松手，会按旧下标去动新列表
        self.seg_drag = None;
        // 当前标尺默认停在**整片中间**：这是"用户还没点过时间线"时的中性位置，
        // 时长未知（0/负）时退化为 0。换素材就重置，绝不沿用上一条的位置。
        self.cursor_ms = clip.duration_ms.max(0) / 2;
        // 换素材后当前标尺只是"中间占位"：在用户点击时间线之前，
        // 「设为起点/终点」保持禁用（见 `cursor_committed` 的说明）
        self.cursor_committed = false;
        self.view_start_ms = 0.0;
        // 默认全片可见：交给 `fit_mode` 在绘制时按可用宽度算，窗口改宽也跟随
        self.fit_mode = true;
        // P19：胶片条的缩略图**不再**在打开时一次性解一批（那批图会随缩放被拉伸），
        // 改成"按当前可见区间的取样时刻按需解码"（见 `pump_strip`）。
        //
        // P23：这里**登记**而不是执行 —— 波形阶段要等三条就绪判据全中。
        self.clip = Some(clip);
        // **P27：把"取样时刻按谁算"记一行日志**（每个文件一行，不在绘制路径上）。
        // 视频流时长拿不到时**回退到容器时长**并记一条 warn —— 这是用户要求的
        // "拿不到就回退并记一行日志"，也是"这条素材为什么可能仍然取样失败"的取证。
        self.log_p27_sample_span();
        self.begin_import_phase(path);
    }

    /// **打开失败落地**（P34）：与改造前 `Err` 分支一字不差 ——
    /// 照样放行波形阶段（`wave_phase_done = true`），走既有降级路径收尾。
    fn fail_open(&mut self, msg: &str) {
        self.clip = None;
        self.open_pending_name = None;
        self.keyframes_scanning = false;
        self.error = Some(self.i18n.tf("err_open_video_failed", &[&msg]));
        self.finish_import_without_wave("open_failed");
    }

    /// **收在途打开的消息**（P34；界面每帧调一次，绝不阻塞）。
    ///
    /// 三条消息：`Clip`（探测完成，应用）→ `Keyframes`（扫描回来，贴表、解禁导出）
    /// → / `Failed`（探测就失败）。通道断开按失败处理（后台线程 panic 也不留半成品）。
    /// **轮次对不上的消息一律丢弃**（快速连开两个文件时，旧文件的迟到消息不许盖掉新界面）。
    ///
    /// 打开还在途时顺带排一次兜底重绘（没有输入界面本不出帧，
    /// "正在打开…"与随后的首帧都要靠它推进）。
    fn poll_open(&mut self, ctx: Option<&egui::Context>) {
        // **在途（或关键帧还在扫）时排一次兜底重绘**：后台消息是异步到的，
        // 而 egui 没有输入时不出帧 —— 不排这一拍，`Keyframes` 到达时界面不会去收，
        // 导出会一直停在"正在建立关键帧索引…"直到用户动一下鼠标。
        // 这不是心跳：打开一结束（`open_rx` 收掉、`keyframes_scanning` 落下）就不再排。
        if (self.open_rx.is_some() || self.keyframes_scanning) && ctx.is_some() {
            if let Some(ctx) = ctx {
                ctx.request_repaint_after(std::time::Duration::from_millis(OPEN_POLL_MS));
            }
        }
        let msg = match self.open_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match msg {
            Ok(OpenMsg::Clip(clip, epoch)) => {
                if epoch != self.open_epoch {
                    return; // 旧文件的迟到消息：丢
                }
                self.open_pending_name = None;
                let path = clip.src.clone();
                self.apply_opened_clip(clip, &path);
            }
            Ok(OpenMsg::Scan(pass, epoch)) => {
                if epoch != self.open_epoch {
                    return;
                }
                self.open_rx = None; // 终态：这一轮打开结束
                self.open_cancel = None;
                self.keyframes_scanning = false;
                let (kfs, health) = (pass.keyframes_ms, pass.health);
                // 体检结论落 `clip.health`（**唯一真相源**：播放跳过 / 绘制 / 导出剔除都读它）
                let bad_hard = health.hard_ranges().len();
                let bad_all = health.bad.len();
                if let Some(c) = self.clip.as_mut() {
                    c.keyframes_ms = kfs;
                    c.keyframes_scanned = true;
                    c.health = health;
                    let count = c.keyframes_ms.len();
                    // 落 `cap.log` 的取证行：扫描段耗时（与 `editor_import_begin` 对起来看两段占比）
                    let dur = self
                        .open_started_at
                        .map(|t| t.elapsed())
                        .unwrap_or_default();
                    crate::ui::startup::mark_with(
                        "editor_keyframes_scanned",
                        "wave",
                        Some(dur),
                        Some(format!(
                            "file={} count={count} bad={bad_all} bad_hard={bad_hard} elapsed_ms={}",
                            self.import_log_name,
                            dur.as_millis()
                        )),
                    );
                    tracing::info!(
                        phase = "editor_keyframes_scanned",
                        file = %self.import_log_name,
                        count,
                        bad_all,
                        bad_hard,
                        "剪辑页：关键帧扫描 + 编码体检完成（表已定稿，导出解禁）"
                    );
                }
            }
            Ok(OpenMsg::Failed(msg, epoch)) => {
                if epoch != self.open_epoch {
                    return;
                }
                self.open_rx = None;
                self.open_cancel = None;
                let dur = self
                    .open_started_at
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                crate::ui::startup::mark_with(
                    "editor_open_failed",
                    "wave",
                    Some(dur),
                    Some(format!("file={} error={msg}", self.import_log_name)),
                );
                self.fail_open(&msg);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // 还在途：兜底重绘已经在函数开头排过了（见那里的说明）
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // 后台线程意外没了：按失败收尾，绝不永远停在"正在打开"
                self.open_rx = None;
                self.open_cancel = None;
                let msg = self.i18n.t("err_open_worker_gone").to_string();
                self.fail_open(&msg);
            }
        }
    }

    /// 导出按钮是否可点（P34：关键帧扫描期间禁用 —— 表不完整时 copy 导出会落在非关键帧上）。
    fn export_enabled(&self) -> bool {
        !self.exporting && !self.keyframes_scanning
    }

    // -----------------------------------------------------------------------
    // P35：编码体检（快速体检随打开回来；深度检查手动触发）
    // -----------------------------------------------------------------------

    /// **当前体检结论**（唯一真相源：`clip.health`）。
    ///
    /// 快速体检随打开回来，深度检查完成后**并入同一份**（见 [`EditorPage::poll_deep`]）——
    /// 所以播放跳过、坏段绘制、导出剔除读的都是它，口径不可能分叉。
    fn health_report(&self) -> edit::health::HealthReport {
        self.clip
            .as_ref()
            .map(|c| c.health.clone())
            .unwrap_or_default()
    }

    /// **播放要自动跳过的区间**（P35 用户口径：**仅硬错误自动跳**）。
    fn hard_skip_ranges(&self) -> Vec<(i64, i64)> {
        self.health_report().hard_ranges()
    }

    /// 停掉在跑的深度检查（换文件 / 离开页面 / 重新开始时调）。
    ///
    /// 取消是**协作式**的：置位之后后台最多再解一个包就退，所以这一步是毫秒级，
    /// 绝不会让"打开另一个文件"卡住界面（与波形任务同一套纪律）。
    fn cancel_deep(&mut self) {
        if let Some(c) = self.deep_cancel.take() {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.deep_rx = None;
        self.deep_progress = None;
        self.deep_checking = false;
    }

    /// 开始**深度检查**（全片解码，后台线程 + 进度 + 可取消）。
    ///
    /// 只在"用户点了按钮"时跑 —— 它要解完整片（1 小时 1080p 是分钟级），
    /// 自动跑会让"打开视频"变成"打开视频然后风扇转起来"。
    fn start_deep_check(&mut self, ctx: &egui::Context) {
        if self.deep_checking {
            return;
        }
        let Some(clip) = self.clip.as_ref() else {
            return;
        };
        // 关键帧表还没定稿时不许开始：深度检查要独立读一遍全片，
        // 与正在跑的关键帧扫描抢磁盘只会让两边都变慢。
        if self.keyframes_scanning {
            // 点了没反应是最差的反馈：把"为什么"说清楚（复用同一句"正在建立索引…"）
            self.notice = Some(self.i18n.t("keyframes_scanning").to_string());
            return;
        }
        let path = clip.src.clone();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        self.deep_progress_ms = 0;
        self.deep_checking = true;
        self.deep_rx = Some(rx);
        self.deep_cancel = Some(cancel.clone());
        self.deep_progress = Some(progress.clone());
        let ctx_bg = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("clip-health".into())
            .spawn(move || {
                let r = edit::health::deep_scan(&path, &cancel, |ms| {
                    progress.store(ms, std::sync::atomic::Ordering::Relaxed);
                    // 数据驱动重绘：每 256 个包一次，够进度条动、又不至于打满 UI 线程
                    ctx_bg.request_repaint();
                })
                .map_err(|e| e.to_string());
                let _ = tx.send(r);
            });
        if spawned.is_err() {
            // 线程起不来（几乎不可能）：如实收尾，绝不停在"检查中…"
            self.cancel_deep();
            self.error = Some(self.i18n.t("err_export_thread").to_string());
        }
    }

    /// 收深度检查的结果（非阻塞，与 [`EditorPage::poll_open`] 同一套纪律）。
    fn poll_deep(&mut self, ctx: &egui::Context) {
        if self.deep_checking {
            if let Some(p) = self.deep_progress.as_ref() {
                self.deep_progress_ms = p.load(std::sync::atomic::Ordering::Relaxed);
            }
            // 兜底重绘：后台线程自己也会 `request_repaint`，但"最后一帧"未必排到 ——
            // 不排这一拍，进度条会停在 99% 直到用户动鼠标。
            ctx.request_repaint_after(std::time::Duration::from_millis(OPEN_POLL_MS));
        }
        let msg = match self.deep_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match msg {
            Ok(Ok(rep)) => {
                let bad = rep.bad.len();
                let hard = rep.bad.iter().filter(|b| b.kind.is_hard()).count();
                self.deep_progress_ms = rep.scanned_ms;
                if let Some(c) = self.clip.as_mut() {
                    merge_deep_into_clip(c, rep);
                }
                self.deep_rx = None;
                self.deep_progress = None;
                self.deep_cancel = None;
                self.deep_checking = false;
                self.notice = Some(
                    self.i18n
                        .tf("health_deep_done", &[&bad.to_string(), &hard.to_string()]),
                );
                crate::ui::startup::mark_with(
                    "editor_deep_check",
                    "wave",
                    None,
                    Some(format!(
                        "file={} bad={bad} hard={hard}",
                        self.import_log_name
                    )),
                );
            }
            Ok(Err(e)) => {
                self.deep_rx = None;
                self.deep_progress = None;
                self.deep_cancel = None;
                self.deep_checking = false;
                // 取消不是错误（换文件时会走到这里）：只有"不是被我们取消的"才提示
                self.notice = Some(self.i18n.tf("health_deep_failed", &[&e]));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.deep_rx = None;
                self.deep_progress = None;
                self.deep_cancel = None;
                self.deep_checking = false;
                self.notice = Some(self.i18n.t("health_deep_worker_gone").to_string());
            }
        }
    }

    /// **仅测试**：把在途打开"跑完"（P34 之后 `open()` 是异步的）。
    ///
    /// 轮询 [`EditorPage::poll_open`]（不排帧、不等真窗口）直到本轮打开结束
    /// （`open_rx` 被取走 = 终态：关键帧已回 / 已失败）或超时。
    /// 超时直接 panic —— "打开永远不回来"本身就是缺陷，不该被静默放过。
    ///
    /// 这是**测试台**的同步化手段：生产路径里由 `ui()` 每帧调 `poll_open`，
    /// 打开期间自己排兜底重绘，界面不会停。
    #[cfg(test)]
    fn wait_open(&mut self) {
        let t0 = std::time::Instant::now();
        loop {
            self.poll_open(None);
            if self.open_rx.is_none() && self.open_pending_name.is_none() {
                break;
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(60),
                "open did not settle within 60 s (open_rx={} pending={:?})",
                self.open_rx.is_some(),
                self.open_pending_name
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// **进入导入阶段**（P23 阶段一）：记起点、记一行日志、登记"波形待启动"。
    ///
    /// `import_started_at` 就是导入阶段的计时零点，也是"这一阶段还在进行"的唯一权威。
    fn begin_import_phase(&mut self, path: &Path) {
        self.import_started_at = Some(std::time::Instant::now());
        self.import_watchdog = None;
        self.editor_import_ready = false;
        self.wave_phase_done = false;
        self.wave_start_pending = self.show_waveform;
        self.import_log_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = self.arm_import_watchdog();
        crate::ui::startup::mark_with(
            "editor_import_begin",
            "wave",
            None,
            Some(format!("file={}", self.import_log_name)),
        );
        tracing::info!(
            phase = "editor_import_begin",
            file = %self.import_log_name,
            wave_pending = self.wave_start_pending,
            "剪辑页：进入导入阶段（只做探测流信息 + 首帧预览 + 胶片条首块，不启动波形任务）"
        );
    }

    /// **导入阶段收尾但不启动波形**（P23；打开失败 / 没有视频流这一类的降级路径）。
    ///
    /// 语义：把波形阶段**放行**（`wave_phase_done = true`）并清掉所有导入状态。
    /// 为什么不是"什么都不做"：不放行的话 `check_wave_phase` 会一直认为"还在导入"，
    /// 于是每 100 ms 排一次兜底看门狗帧直到超时 —— 那是不必要的空转；
    /// 放行之后波形语义与 P23 之前**完全一致**（没有音轨就如实记一行、不起线程）。
    fn finish_import_without_wave(&mut self, reason: &str) {
        self.import_started_at = None;
        self.import_watchdog = None;
        self.wave_start_pending = false;
        self.editor_import_ready = true;
        self.wave_phase_done = true;
        #[cfg(test)]
        {
            self.import_decision = Some(ImportDecision {
                video_info: false,
                preview: false,
                strip: false,
                fallback: false,
            });
        }
        tracing::info!(
            phase = "editor_import_skipped",
            reason = reason,
            file = %self.import_log_name,
            "剪辑页：导入没有拿到可用视频流 → 不启动波形阶段（如实降级，不 panic）"
        );
    }

    /// **排一次兜底看门狗帧**（P23）：最迟在 [`IMPORT_READY_TIMEOUT_MS`] 叫醒界面一次。
    ///
    /// **这不是心跳**：只在"导入还没就绪"时排，且每次只排 `[`IMPORT_WATCHDOG_FRAME_MS`]` 之后
    /// 那一拍，导入一就绪（判据或超时兜底）就**不再排下一拍**。
    /// 返回"本次是否排了帧"（单测用它钉住"排帧有限、之后静止"）。
    ///
    /// 为什么非得有它：兜底是**按时间**触发的，而没有输入时界面本就不会自己出帧
    /// （P23 要求"兜底不得引入心跳/每帧重绘"，但也不能因为"没有帧"就让兜底永远不生效）。
    /// 折中就是"**有限次数**的看门狗"：`0..=IMPORT_READY_TIMEOUT_MS` 这一段里最多
    /// `IMPORT_READY_TIMEOUT_MS / IMPORT_WATCHDOG_FRAME_MS` 帧，之后彻底安静。
    fn arm_import_watchdog(&mut self) -> bool {
        let Some(t0) = self.import_started_at else {
            return false;
        };
        if self.editor_import_ready || self.wave_phase_done {
            return false;
        }
        if t0.elapsed().as_millis() as u64 >= IMPORT_READY_TIMEOUT_MS {
            return false; // 已经到点：本帧就会走兜底，不需要再排
        }
        self.import_watchdog = Some(std::time::Instant::now());
        true
    }

    /// **P23 的核心：两阶段落定**。
    ///
    /// * 还在导入 → 评估"导入就绪"判据（见 [`import_readiness`]）；没就绪就排一次兜底看门狗帧
    ///   （**绝不启动任何波形线程**），就绪则在**本帧**把波形阶段拉起来；
    /// * 已经落定 → 什么都不做（没有每帧轮询、没有心跳）。
    ///
    /// 返回值 = 本帧是否要求了重绘（只可能是"兜底看门狗"那一种，且有限次）。
    fn check_wave_phase(&mut self, ctx: &egui::Context) -> bool {
        // 不在导入阶段 / 没有素材 / 波形阶段已经落定 → 一个字节都不做
        if self.wave_phase_done || self.import_started_at.is_none() {
            // 收尾口径：落定之后**绝不能**再留着看门狗（它唯一的用途就是导入阶段那几次排帧）
            self.import_watchdog = None;
            return false;
        }
        let Some(clip) = self.clip.clone() else {
            return false;
        };
        // ---- 三条判据的**实取** ----
        //
        // 判据一（视频流信息就绪）：`clip.video` 存在且宽高为正 —— 那就是"探测流信息"的产物。
        let video_info = clip
            .video
            .as_ref()
            .is_some_and(|v| v.width > 0 && v.height > 0);
        // 判据二（首帧预览已渲染出一帧）：本页真的把一张**已解码的**纹理贴进预览画框过
        // （见 `paint_preview` 里的 `preview_painted` 那一句；占位文字不算）。
        let preview = self.preview_painted;
        // 判据三（胶片条首块已显示）：本帧的胶片条里至少有一块是**真画面**（不是灰色占位块）
        // —— `strip_prev` 正是"本帧查表命中的块"。
        let strip = !self.strip_prev.is_empty();
        let elapsed_ms = self
            .import_started_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let decision = import_readiness(
            video_info,
            preview,
            strip,
            elapsed_ms,
            Some(IMPORT_READY_TIMEOUT_MS),
        );
        #[cfg(test)]
        {
            self.import_decision = Some(decision);
        }
        if !decision.ready() {
            // 还没就绪：排一次**有限**的兜底看门狗帧，然后如实等（不启动波形）
            if self.arm_import_watchdog() {
                ctx.request_repaint_after(std::time::Duration::from_millis(
                    IMPORT_WATCHDOG_FRAME_MS,
                ));
                return true;
            }
            return false;
        }
        // ---- 就绪：落定导入阶段，**自动**开始波形（无需用户点按钮）----
        self.import_started_at = None;
        self.import_watchdog = None;
        self.editor_import_ready = true;
        self.wave_phase_done = true;
        crate::ui::startup::mark_dur_extra(
            PHASE_EDITOR_IMPORT_READY,
            "wave",
            std::time::Duration::from_millis(elapsed_ms),
            format!(
                "file={} {} elapsed_ms={}",
                self.import_log_name,
                decision.detail(),
                elapsed_ms
            ),
        );
        tracing::info!(
            phase = PHASE_EDITOR_IMPORT_READY,
            file = %self.import_log_name,
            elapsed_ms,
            video_info = decision.video_info,
            preview = decision.preview,
            strip = decision.strip,
            fallback = decision.fallback,
            "剪辑页：导入就绪 → 波形阶段开始（视频流信息 + 首帧预览 + 胶片条首块；兜底={}）",
            decision.fallback
        );
        if decision.fallback && !(decision.video_info && decision.preview && decision.strip) {
            // 兜底必须**说清用了哪一条**，否则日志上看不出"这次是等出来的还是兜出来的"
            tracing::warn!(
                phase = PHASE_EDITOR_IMPORT_READY,
                file = %self.import_log_name,
                elapsed_ms,
                "剪辑页：导入就绪判据超时 → 用兜底触发波形阶段（未满足的条件见上一行 video_info/preview/strip）"
            );
        }
        // 用户把波形关掉了 → 就绪也不启动（"关掉就是关掉"）
        if !self.wave_start_pending {
            tracing::info!(
                phase = PHASE_EDITOR_WAVE_START,
                file = %self.import_log_name,
                started = false,
                reason = "user_disabled",
                "剪辑页：波形阶段已落定，但用户关着波形 → 不启动（不白烧 CPU）"
            );
            return false;
        }
        self.wave_start_pending = false;
        self.wave.start(&clip, &self.i18n);
        tracing::info!(
            phase = PHASE_EDITOR_WAVE_START,
            file = %self.import_log_name,
            "剪辑页：导入就绪后自动启动波形阶段（基准层 worker + 按需细化）"
        );
        false
    }

    /// 主界面在**配置变化 / 切到剪辑页**时调用，保持"是否显示波形"与配置一致。
    ///
    /// **P36 起编辑器强制显示波形**（用户要求"波形成为必选"）：`apply_opened_clip`
    /// 每次打开素材都会调 `set_show_waveform(true)`，所以设置页里那个开关
    /// 只影响"别的地方"，剪辑页恒有波形。本函数仍保留 `false` 分支 ——
    /// 测试台会用它模拟"波形不可用"的降级路径。
    ///
    /// 关掉时不只是"不画"：还会**取消在跑的后台任务**（用户明确要求不要白烧 CPU），
    /// 打开的瞬间打开则重新起一个（缓存命中时就一个线程都不起）。与
    /// [`EditorPage::set_output_dir`] 同一个口径：**不要每帧调用**。
    ///
    /// 剪辑页可见性 / 是否在录制：不可见时暂停全部重后台任务。
    ///
    /// 由外壳每帧调用（值相同直接返回）。暂停的是：播放、波形基准层/细化、
    /// 深度检查。关键帧扫描没有取消标志，但它的线程本来就有界；下一次打开会换 epoch。
    /// 回到剪辑页时，如果已有素材且波形开关为真，就从缓存/已有进度重新开始。
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        if !active {
            self.stop_playback();
            self.wave.cancel_current();
            self.cancel_deep();
            return;
        }
        if self.show_waveform && self.wave_phase_done && self.wave.job.is_none() {
            if let Some(clip) = self.clip.clone() {
                let i18n = self.i18n.clone();
                self.wave.start(&clip, &i18n);
            }
        }
    }

    /// **P23**：打开的这一侧也受两阶段约束 —— 若此刻还在导入阶段，只是**登记**
    /// `wave_start_pending`（导入就绪后照常自动开始）；不在导入阶段才立刻启动。
    pub fn set_show_waveform(&mut self, on: bool) {
        if self.show_waveform == on {
            return;
        }
        self.show_waveform = on;
        if !on {
            // **关掉就是关掉**：连"导入就绪后自动开始"那一笔也一起作废
            self.wave_start_pending = false;
            self.wave.cancel_current();
            return;
        }
        if self.import_started_at.is_some() {
            // 还在导入阶段：登记，等就绪（`check_wave_phase` 会把它兑现）
            self.wave_start_pending = true;
            return;
        }
        if let Some(clip) = self.clip.clone() {
            let i18n = self.i18n.clone();
            self.wave.start(&clip, &i18n);
        }
    }

    /// **波形绘制耗时的取证日志**（`phase=wave_ui_draw scope=wave`）。
    ///
    /// 记的是"每帧平均"：波形每帧都画，逐帧一行会把 `cap.log` 刷爆；
    /// 每约 2 秒一条平均值既够看趋势（长视频上"一帧画多少段"是否失控），又不刷屏。
    ///
    /// P18 起还记 **`tier`**（本帧选中的桶宽档位）：用户实机"放大后波形没变细"
    /// 这类问题，一眼就能从 `layer=detail tier=…` 看出"到底换档了没有"。
    fn wave_ui_log(
        &mut self,
        frame_t0: std::time::Instant,
        snap_cost: std::time::Duration,
        segs: &edit::waveform::WaveSegments,
        drawn: WaveDrawn,
    ) {
        self.wave_ui_frames += 1;
        self.wave_ui_segs += drawn.segments as u64;
        self.wave_ui_cost += frame_t0.elapsed();
        let now = std::time::Instant::now();
        let due = match self.wave_ui_last_log {
            None => true,
            Some(last) => now.duration_since(last).as_millis() >= 2_000,
        };
        if !due || self.wave_ui_frames == 0 {
            return;
        }
        let frames = self.wave_ui_frames;
        let avg_ms = self.wave_ui_cost.as_secs_f64() * 1000.0 / frames as f64;
        let snap_us = snap_cost.as_micros();
        crate::ui::startup::mark_with(
            "wave_ui_draw",
            "wave",
            Some(self.wave_ui_cost / frames as u32),
            Some(format!(
                "frames={frames} segs_avg={} layer={} tier={}ms fallback_segs={} pending={:?} snapshot_us={snap_us} avg_ms={avg_ms:.2}",
                self.wave_ui_segs / frames,
                match segs.layer {
                    ViewLayer::Dense => "dense",
                    ViewLayer::Base => "base",
                    ViewLayer::None => "none",
                },
                segs.bucket_ms,
                segs.segs.iter().filter(|s| !s.detail).count(),
                segs.pending_ms,
            )),
        );
        self.wave_ui_frames = 0;
        self.wave_ui_segs = 0;
        self.wave_ui_cost = std::time::Duration::ZERO;
        self.wave_ui_last_log = Some(now);
    }

    /// 本帧要显示给用户的波形提示（"没有音轨/解码失败"那句）；没有问题时为 `None`。
    ///
    /// 只用于**底部状态栏那一行**（与其它提示同一条通道），不弹窗、不阻塞。
    pub fn waveform_notice(&self) -> Option<String> {
        self.wave.last_error.clone()
    }

    /// 本帧页面里的「波形」复选框有没有被改过（外壳据此 `sanitized()` + 落盘）。
    ///
    /// 语义与 [`crate::ui::settings::SettingsOutcome::cfg_changed`] 完全一致 ——
    /// 剪辑页这一处开关走的是**同一条**"调整即保存"路径。
    pub fn wave_cfg_changed(&self) -> bool {
        self.wave_cfg_changed
    }

    /// 波形的**收尾**：成功 → 进内存缓存 + 记一条 info；失败 → **如实降级**
    /// （不画波形、留一行 warn、给界面一句提示），并记住这个文件"读不出来"。
    ///
    /// 只由 `ui()` 在 `WaveState::poll_waves()` 回报 `finished` 时调用。
    fn finish_wave(&mut self, polled: WavePolled, i18n: &crate::i18n::I18n) {
        let Some(key) = polled.key else { return };
        match polled.error {
            None => {
                if let Some(base) = polled.set {
                    if base.has_peaks() {
                        tracing::info!(
                            path = %key.path.display(),
                            codec = %polled.codec,
                            duration_ms = base.duration_ms,
                            slices = base.slice_count(),
                            done_slices = base.done_slices,
                            failed_segments = base.failed_segments,
                            cache = if polled.cache_hit { "hit" } else { "miss" },
                            "剪辑页：全片密集基准层完成（{} ms/片，渐进显示结束）",
                            base.bucket_ms
                        );
                        self.wave.remember(key, base);
                    }
                }
                self.wave.last_error = None;
            }
            Some(e) => {
                // **如实降级**：绝不 panic、绝不让界面卡住，剪辑/导出照常可用
                tracing::warn!(
                    path = %key.path.display(),
                    error = %e,
                    "剪辑页：音频波形不可用，已降级为「不画波形」（不影响剪辑）"
                );
                // `e` 来自解码层（技术性中文），只进日志；**界面提示用 i18n 文案**：
                // 底部状态栏那一行是 `waveform_unavailable`（"波形不可用：{}"）套这句话，
                // 所以这里给的必须是"一句人话的原因"，不能再套一层同样的前缀。
                let reason = i18n.t("err_wave_decode_failed").to_string();
                self.wave.remember_unavailable(key, reason.clone());
                self.wave.last_error = Some(reason);
            }
        }
    }

    /// 当前载入文件的路径；没有成功载入任何文件时为 `None`。
    ///
    /// 给界面与单测共用：界面用它显示"正在剪辑哪个文件"，单测用它证明
    /// "`open(&path)` 之后剪辑页真的载入了那个文件"（停止录制 → 剪辑页那条链）。
    pub fn loaded_path(&self) -> Option<&std::path::Path> {
        self.clip.as_ref().map(|c| c.src.as_path())
    }

    /// **是否有一次打开正在进行**（`open()` 已调用、结果还没落地）。
    ///
    /// 为什么需要它：`open()` 是异步的（P34），"打开在途"期间 [`Self::loaded_path`] 仍然是
    /// `None` —— 光看后者区分不出"什么都没打开"和"正在打开一个打不开的文件"。
    /// P46 的守门测试要断言的正是前者，所以把这个状态如实暴露出来。
    pub fn is_opening(&self) -> bool {
        self.open_rx.is_some() || self.open_pending_name.is_some()
    }

    /// 当前载入媒体的时长（毫秒）；没有载入时为 0。
    pub fn loaded_duration_ms(&self) -> i64 {
        self.clip.as_ref().map(|c| c.duration_ms).unwrap_or(0)
    }

    /// 主界面在**配置变化 / 切到剪辑页**时调用，保证文件对话框用的是最新目录。
    ///
    /// 为什么需要它：`EditorPage::ui()` 拿不到 `Config`，而"录完立刻剪刚录的那条"要求
    /// 剪辑页知道录制输出目录（「打开视频」的对话框默认落在这里）。
    pub fn set_output_dir(&mut self, dir: &std::path::Path) {
        self.output_dir = dir.to_string_lossy().to_string();
    }

    /// 注入**波形 L0 的落盘缓存目录**（外壳按既有回退链解析好的 `Cache\`）。
    ///
    /// 为什么由外壳注入而不是剪辑页自己算：缓存目录的回退链（LocalAppData →
    /// 程序目录 → 配置目录 → 临时目录）已经在外壳启动时算过一次
    /// （`cache::resolve_cache_dir`），这里再算一遍就会有两份"同一个目录"的推导，
    /// 早晚会不一致。
    /// `None` = 不落盘（功能照常，只是每次都重算）。
    pub fn set_wave_cache_dir(&mut self, dir: Option<PathBuf>) {
        self.wave.set_disk_dir(dir);
    }

    /// 剪辑页认为的**录制目录**（绝对路径）：两个文件对话框（打开 / 导出）的起始位置。
    ///
    /// 顺序与配置的回退链同口径（见 [`pick_recording_dir`]）：
    /// `Config::output_dir` → 最近一次录制的文件所在目录 → 用户视频目录。
    fn recording_dir(&self) -> Option<PathBuf> {
        let resolved = crate::config::resolve_against_exe_dir(&self.output_dir).ok();
        let videos = crate::config::default_fallback_dirs().into_iter().next();
        pick_recording_dir(
            resolved.as_deref(),
            self.last_video_dir.as_deref(),
            videos.as_deref(),
        )
    }

    /// 把一张解好的缩略图放进**当前帧预览**的缓存
    /// （**上限 [`STRIP_CACHE_MAX`]**，超了整体清空重来）。
    ///
    /// 为什么"清空"而不是"精确淘汰最旧的一张"：这两个缓存条目都很小（160×50 的纹理 ≈ 32 KB），
    /// 而"整体清空"的实现简单到不可能有 bug；上限 256 块 ≈ 8 MB，
    /// 而一屏最多只画 8 块左右，清空的代价只是"重新解几块"。
    fn remember_thumb(&mut self, at_ms: i64, bucket_w: u32, thumb: CachedThumb) {
        if self.kf_thumbs.len() >= STRIP_CACHE_MAX {
            self.kf_thumbs.clear();
        }
        self.kf_thumbs.insert((at_ms, bucket_w), thumb);
    }

    /// 把一张解好的缩略图放进**胶片条**的缓存（P24/P25；键 = `(时刻, 解码宽, 画面高, 口径版本)`）。
    ///
    /// 与 [`EditorPage::remember_thumb`] 同一条淘汰口径（超上限整体清空、绝不无限增长）。
    /// 独立的表 + 独立的键：[`StripThumbKey`] 里带高度与口径版本，于是"行高一变/口径一变
    /// 就是另一份缓存"。
    ///
    /// **P27**：同时维护 [`EditorPage::strip_thumb_order`]（插入顺序）—— 兜底链第③级要
    /// "缓存里最新的一张真画面"，而 `HashMap` 没有顺序。两处清空必须成对（见本函数与 `open`）。
    fn remember_strip_thumb(&mut self, key: StripThumbKey, thumb: CachedThumb) {
        if self.strip_thumbs.len() >= STRIP_CACHE_MAX {
            self.strip_thumbs.clear();
            self.strip_thumb_order.clear();
        }
        // 重复插入 = 刷新它的"最新"位置（末尾）
        if let Some(pos) = self.strip_thumb_order.iter().position(|k| *k == key) {
            self.strip_thumb_order.remove(pos);
        }
        self.strip_thumb_order.push(key);
        self.strip_thumbs.insert(key, thumb);
    }

    /// **P27：本页当前素材的"取样末端"**（毫秒）= **视频流时长**；拿不到就容器时长。
    ///
    /// 绘制路径每帧调一次（纯计算、无日志、无分配）：日志只在 `open()` 里记一行
    /// （见 [`EditorPage::log_p27_sample_span`]），绝不每帧刷屏。
    fn p27_span_end_ms(&self) -> i64 {
        self.clip
            .as_ref()
            .map(|c| p27_sample_span_end_ms(c).0)
            .unwrap_or(0)
    }

    /// **P27 ①：记一行"取样按谁算"**（每个素材一行，`open()` 里调用）。
    ///
    /// 拿到视频流时长 → info（正常路径）；拿不到 → **warn + 回退到容器时长**
    /// （用户明确要求"若拿不到视频流时长 → 回退到容器时长并记一行日志"）。
    ///
    /// **落点是 `cap.log`**（走 [`p27_log`] → `ui::startup::mark_with`）：
    /// 产品没有 tracing 订阅者，只写 `tracing::info!` 的话用户机器上什么都看不见。
    fn log_p27_sample_span(&self) {
        let Some(clip) = self.clip.as_ref() else {
            return;
        };
        let (end_ms, from_video) = p27_sample_span_end_ms(clip);
        // 文件名取 `clip.src`（不取 `import_log_name`）：本函数在 `open()` 里**早于**
        // `begin_import_phase`（那里才填 `import_log_name`），用后者会记出一个空名字。
        let file = clip
            .src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let line = p27_sample_span_line(&file, end_ms, from_video, clip.duration_ms);
        // 拿不到视频流时长 → 提高一档（warn），其余是正常路径（info）
        p27_log("p27_sample_span", !from_video, line);
    }

    /// **P27 兜底链第③级**：缓存里**最近一张**成功的胶片条画面（同解码宽/高/口径版本）。
    ///
    /// 为什么要限制 `w/h/v` 一致：不同高度档 / 不同口径解出来的纹理贴进当前格子会变形或
    /// 语义错位（P24/P25 明确禁止"互相污染"）。
    fn newest_strip_thumb(&self, w: u32, h: u32, v: u32) -> Option<CachedThumb> {
        self.strip_thumb_order
            .iter()
            .rev()
            .find(|k| k.w == w && k.h == h && k.v == v)
            .and_then(|k| self.strip_thumbs.get(k))
            .map(|c| CachedThumb {
                w: c.w,
                h: c.h,
                tex: c.tex.clone(),
            })
    }

    /// 把一张"借来的画面"登记成某个取样键的**兜底帧**（有界，超上限整体清空）。
    fn insert_strip_fallback(&mut self, key: StripThumbKey, thumb: CachedThumb) {
        if self.strip_fallback.len() >= STRIP_CACHE_MAX {
            self.strip_fallback.clear();
        }
        self.strip_fallback.insert(key, thumb);
    }

    /// **P27 兜底链（②③④）**：某个取样键彻底解不出来时，为它找一张"能显示的画面"。
    ///
    /// 顺序（用户确认的口径）：
    /// 1. **①方向搜索**已经在下层（解码线程）试过了 —— 走到这里说明它也没成；
    /// 2. **②同一条上另一个已经成功的块**（`strip_prev`，就近选）→ 直接借它的画面；
    /// 3. **③胶片条缓存里最近一张成功的画面**（同宽/高/版本）→ 借它；
    /// 4. **④都没有** → 返回 [`FallbackSource::None`]（调用方留空 + 记一行警告）。
    ///
    /// 目的（用户原话）：**只要文件里存在任意一帧可解，胶片条就不出现空槽/灰块**。
    /// 注意本函数**不碰界面提示**：兜底与失败都只写 `cap.log`（见 `poll_near_thumbs`）。
    fn p27_strip_fallback(&mut self, key: StripThumbKey, idx: Option<usize>) -> FallbackSource {
        if let Some(t) = nearest_strip_thumb(&self.strip_prev, idx) {
            let thumb = CachedThumb {
                w: t.w,
                h: t.h,
                tex: t.tex.clone(),
            };
            self.insert_strip_fallback(key, thumb);
            return FallbackSource::Neighbor;
        }
        if let Some(c) = self.newest_strip_thumb(key.w, key.h, key.v) {
            self.insert_strip_fallback(key, c);
            return FallbackSource::Cache;
        }
        FallbackSource::None
    }

    /// **P21（仅测试）**：把胶片条会用到的取样时刻预填成 2×2 的假纹理。
    ///
    /// 为什么需要它：胶片条只画"已经在缓存里的时刻"（没解出来的画占位块），
    /// 不预填的话 `test_rects.strip_tiles` 就是空的，量不到"每个画面的尺寸与位置"。
    ///
    /// **P24/P25/P27**：键里带上当前画面高（[`strip_tile_height`]）与**口径版本**
    /// （[`STRIP_THUMB_VERSION`]），与生产**同一个**构造入口 [`strip_thumb_key`]，
    /// 所以生产查表必定命中。
    ///
    /// **P27**：第三个数与生产一致地取**视频流末端**（`p27_sample_span_end_ms`）——
    /// 否则"夹到 `[0, 末端−1]`"这一步会算出与生产不同的键。
    #[cfg(test)]
    fn prefill_strip(&mut self, ctx: &egui::Context, clip: &Clip) {
        // 把整个时长按量化步长都填上（键用与生产**同一个** [`strip_thumb_key`]）：
        // 任何可见区间算出来的取样时刻都会命中
        let q = STRIP_QUANT_MS.max(1);
        let span_end = p27_sample_span_end_ms(clip).0.max(1);
        let n = (span_end / q).min(2000);
        let mut keys: Vec<i64> = (0..=n).map(|k| k * q).collect();
        keys.push(span_end - 1);
        let tile_h = strip_tile_height();
        for t in keys {
            let key = strip_thumb_key(t, span_end, STRIP_TILE_W as u32, tile_h);
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x60u8; 16]);
            let tex = ctx.load_texture(format!("strip{t}"), img, egui::TextureOptions::NEAREST);
            self.strip_thumbs
                .insert(key, CachedThumb { w: 2, h: 2, tex });
        }
    }

    /// **按当前可见区间**把胶片条要用的画面读成纹理（P21 / P24 / P25）。
    ///
    /// 数据侧只有一句话：**要哪 n 个时刻的缩略图**由 [`strip_samples`] 给出
    /// （首贴左、尾贴右、中间均匀），**画面 = [`strip_picture_width`] × [`strip_tile_height`]**
    /// （P25 contain：整幅可见、不变形 —— 落位由 [`strip_layout`] 按"贴边 + 等间隙"给出，
    /// 所以这里只记下每个命中项是**第几个采样点** `idx`）；
    /// 解码键 = [`StripThumbKey`]（时刻量化到 [`STRIP_QUANT_MS`]、高度量化到
    /// [`STRIP_H_QUANT_PX`]、口径版本 [`STRIP_THUMB_VERSION`]），于是
    /// "同一个尺寸只解码一次、跨缩放级别复用"（详细的取舍见 [`EditorPage::pump_strip`]）。
    ///
    /// 这里只做"（时刻, 尺寸, 版本）→ 纹理"的查表：解码与缓存都在
    /// [`EditorPage::pump_strip`] / [`EditorPage::poll_near_thumbs`] 那条既有的单张在途管道上。
    ///
    /// 返回本帧**真的查到了画面**的块数（= P23 判据三「胶片条首块已显示」的依据）。
    ///
    /// **P27**：查表顺序 = ①真画面表 `strip_thumbs` → ②兜底帧表 `strip_fallback`
    /// （方向搜索/邻块/缓存三级兜底借来的画面）。两者都按**同一个** [`strip_thumb_key`]
    /// 构造键，所以"请求"与"查表"仍然只有一个量化函数（P21 那条无限请求循环的教训不变）。
    fn refresh_strip(&mut self, samples: &[(i64, f32)], span_end_ms: i64, tile_h: f32) -> usize {
        let mut out: Vec<Thumb> = Vec::with_capacity(samples.len());
        for (idx, (t, _frac)) in samples.iter().enumerate() {
            let key = strip_thumb_key(*t, span_end_ms, STRIP_TILE_W as u32, tile_h);
            let hit = self
                .strip_thumbs
                .get(&key)
                .or_else(|| self.strip_fallback.get(&key));
            if let Some(c) = hit {
                out.push(Thumb {
                    at_ms: *t,
                    idx,
                    w: c.w,
                    h: c.h,
                    tex: c.tex.clone(),
                });
            }
        }
        let hit = out.len();
        self.strip_prev = out;
        hit
    }

    /// 把一组取样时刻排进缩略图解码队列（胶片条专用；键 = [`StripThumbKey`]）。
    ///
    /// 与"当前帧预览"共用**同一个解码线程**与同一条"单张在途"（`thumb_pending`）纪律，
    /// 但**缓存分两张表**（P24：胶片条的键带高度，与预览的尺寸档不是一个坐标系）。
    ///
    /// 节流沿用既有口径（`request_thumb` 的三条）：
    /// ① 同一时刻只允许**一张在途**（`thumb_pending`）；② 已缓存的时刻直接跳过；
    /// ③ 单次最多排 [`STRIP_REQ_MAX`] 个（平移过快时只解最靠左还没解的那个）。
    ///
    /// **P27**：排队时顺手把"**失败之后往哪个方向、按哪些时刻重试**"算好塞进任务
    /// （[`strip_fallback_probes`]，纯函数、有界、不跨相邻取样点）。
    /// 再失败就由 [`EditorPage::p27_strip_fallback`] 走兜底链；**两条路径都不会重试**。
    fn pump_strip(&mut self, samples: &[(i64, f32)], ctx: &egui::Context) {
        if !self.strip_requests_enabled {
            return; // 测试台默认关掉：不碰真实文件、不起解码线程
        }
        if samples.is_empty() {
            return;
        }
        if self.thumb_pending.is_some() {
            return; // 已经有在途任务：等它回来（"单张在途"）
        }
        let Some(clip) = self.clip.as_ref() else {
            return;
        };
        let path = clip.src.clone();
        // **P27 ①**：键与"片尾留白"一律按**视频流时长**算（不是容器时长）
        let span_end = p27_sample_span_end_ms(clip).0;
        let last_ms = p27_sample_last_ms(span_end);
        let keyframes = clip.keyframes_ms.clone();
        // **P24：本帧的画面高**（唯一来源 `strip_tile_height()`）；
        // **P25**：这里只认"解码请求宽" `STRIP_TILE_W`（绘制宽由行高与源宽高比算出，与它无关）
        let tile_h = strip_tile_height();
        let tile_w = STRIP_TILE_W as u32;
        let n = samples.len();
        let mut sent = 0usize;
        let mut want: Vec<(StripThumbKey, usize, Vec<i64>)> = Vec::new();
        for (k, (t, _)) in samples.iter().enumerate() {
            if sent >= STRIP_REQ_MAX {
                break;
            }
            let key = strip_thumb_key(*t, span_end, tile_w, tile_h);
            if self.strip_thumbs.contains_key(&key)
                || self.strip_fallback.contains_key(&key)
                || self.thumb_pending == Some((key.time, key.w))
                || self.thumb_failed.contains(&(key.time, key.w, key.h))
            {
                continue;
            }
            // **P27 ②/③**：方向 + 有界探查时刻（相邻块取样点是硬边界）
            let dir = strip_fallback_dir(k, n);
            let neighbor = match dir {
                FallbackDir::Right => samples.get(k + 1).map(|(t, _)| *t),
                FallbackDir::Left => k.checked_sub(1).map(|i| samples[i].0),
            };
            let alts = strip_fallback_probes(
                *t,
                dir,
                &keyframes,
                neighbor,
                last_ms,
                P27_MAX_ATTEMPTS,
                P27_MAX_FALLBACK_MS,
                P27_STEP_MS,
            );
            want.push((key, k, alts));
            sent += 1;
        }
        let Some((first, first_idx, first_alts)) = want.first().cloned() else {
            return;
        };
        self.ensure_thumb_worker(ctx);
        let Some(tx) = self.thumb_job_tx.as_ref() else {
            return;
        };
        let (max_w, max_h) = decode_size(STRIP_TILE_W as u32, self.source_aspect());
        let job = ThumbJob {
            path,
            at_ms: first.time,
            max_w,
            max_h,
            bucket_w: STRIP_TILE_W as u32,
            strip_key: Some(first),
            alts: first_alts,
            strip_idx: Some(first_idx),
        };
        if tx.send(job).is_ok() {
            // `thumb_pending` 只带 `(时刻, 解码宽)`：胶片条的解码宽恒为 [`STRIP_TILE_W`]
            // （P25 起绘制宽由行高与源宽高比算出，但**解码请求**仍是这一档），
            // 高度与口径版本那两路由任务自己带回（见 [`ThumbJob::strip_key`]），
            // 所以这里不会因为"多了高度/版本"而失去唯一性。
            self.thumb_pending = Some((first.time, STRIP_TILE_W as u32));
        }
    }

    /// 第一次需要"鼠标附近关键帧缩略图"时才建解码线程。
    ///
    /// 为什么把 `Context` 克隆进线程：解码完要主动 `request_repaint()` 唤醒界面，
    /// 否则结果得等用户下一次动鼠标才显示出来（看起来像卡了）。
    /// 线程只在 `job_rx.recv()` 上阻塞，页面析构时 `Sender::drop` 会让 `recv` 返回 Err、
    /// 线程自然退出——没有任何锁，也就不存在死锁。
    ///
    /// **P23 的额外作用**：这条线程每解完一张就 `request_repaint()` 一次 ——
    /// 它就是导入阶段"不需要心跳也会继续出帧"的来源（首帧预览与胶片条首块解码完成时各一次）。
    /// 波形那条链则相反：它在导入就绪之前**一根线程都没有**。
    fn ensure_thumb_worker(&mut self, ctx: &egui::Context) {
        if self.thumb_job_tx.is_some() {
            return;
        }
        let (job_tx, job_rx) = channel::<ThumbJob>();
        let (res_tx, res_rx) = channel::<ThumbJobResult>();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("clip-kf-thumbs".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    // 这张要么是"当前帧预览"本身、要么是胶片条的一块，所以
                    // **按调用方给的目标尺寸**解码（上限由 `thumb_bucket_w` / 胶片条的
                    // 固定档量化并夹住，见那里的注释）：窗口越大解越大，
                    // 窗口没跨档位 / 行高没变档就一张都不重解。
                    //
                    // **P27 ①：失败时按方向搜索**（`job.alts` 已经是有界、有序、
                    // 不跨相邻取样点的探查时刻）。命中第一个能解的即停；
                    // 全都解不出来时**如实带回第一次的错误**（界面据此走兜底链）。
                    let (frame, decoded_ms) = match p27_search_decode(job.at_ms, &job.alts, |t| {
                        edit::thumbnail(&job.path, t, job.max_w, job.max_h)
                            .map_err(|e| e.to_string())
                    }) {
                        Ok((v, t, _attempts)) => (Ok(v), t),
                        Err((e, _attempts)) => (Err(e), job.at_ms),
                    };
                    let done = ThumbJobResult {
                        path: job.path,
                        at_ms: job.at_ms,
                        bucket_w: job.bucket_w,
                        strip_key: job.strip_key,
                        strip_idx: job.strip_idx,
                        decoded_ms,
                        frame,
                    };
                    if res_tx.send(done).is_err() {
                        return; // 页面已经没了，收工
                    }
                    ctx.request_repaint();
                }
            });
        if spawned.is_ok() {
            self.thumb_job_tx = Some(job_tx);
            self.thumb_job_rx = Some(res_rx);
        } else {
            self.notice = Some(self.i18n.t("notice_thumb_thread").to_string());
        }
    }

    /// 按需把"离预览位置**最近的那一个**关键帧"排进解码队列。
    ///
    /// 取舍（为什么这么做）：
    /// * 解一帧要 seek + 建解码器，几十毫秒是常态，放 UI 线程会明显卡顿，
    ///   所以整体丢给后台线程，界面先用"解码中…"占位；
    /// * **同一时刻只允许一张在途**，即"每次界面刷新最多解码一张"——
    ///   鼠标快速划过时间线时不会一次排几十张把解码线程压垮，也不会抢 UI 的帧时间；
    /// * 结果按 **(时间点, 尺寸档)** 缓存在 `kf_thumbs` 里，命中就直接贴纹理，
    ///   **绝不重复解码**（这就是"不每帧调 FFmpeg"的保证）；
    /// * **尺寸档**（`bucket_w`，按 [`THUMB_BUCKET_STEP`] 量化）也进 key：
    ///   拖动窗口让预览每变化 1px 时档位不变 → 复用缓存、不解码；
    ///   只有真的跨档（或换关键帧）才解一张，且因为"单张在途"，
    ///   连续拖动时只会解**最近**那一档；
    /// * 只要一张（不是一排）：诉求就是"下方只显示离鼠标最近的一个关键帧图片"，
    ///   少解几张也顺带把 CPU 省下来；
    /// * 不用锁、不等待线程：解码线程只经 channel 通信，页面从不同步等待它。
    fn request_thumb(&mut self, at_ms: i64, bucket_w: u32, ctx: &egui::Context) {
        // 已经有在途任务就等它回来，本次刷新不再排新的
        if self.thumb_pending.is_some() {
            return;
        }
        let Some(t) = self.clip.as_ref().and_then(|c| thumb_keyframe(c, at_ms)) else {
            return;
        };
        if self.kf_thumbs.contains_key(&(t, bucket_w)) {
            return; // 该时间点这一档已解码过：直接贴纹理
        }
        // 失败表用的是 `(时刻, 宽, 高)` 三元组（P24 起胶片条那一侧也要记高度）。
        // **预览这一侧用 `bucket_w` 当高度分量**：预览的"高度"是从画框宽度推出来的
        // （`decode_size(bucket_w, 源宽高比)`），宽度定了它也就定了 —— 用它当分量
        // 既确定又不会与胶片条（宽恒为 160、高恒为 [`strip_tile_height`]）撞键。
        if self.thumb_failed.contains(&(t, bucket_w, bucket_w)) {
            return; // 解不出来过：不再重试（见 `thumb_failed` 的说明）
        }
        let path = match self.clip.as_ref() {
            Some(c) => c.src.clone(),
            None => return,
        };
        self.ensure_thumb_worker(ctx);
        let Some(tx) = self.thumb_job_tx.as_ref() else {
            return;
        };
        let (max_w, max_h) = decode_size(bucket_w, self.source_aspect());
        let job = ThumbJob {
            path,
            at_ms: t,
            max_w,
            max_h,
            bucket_w,
            // 预览那一侧：写 `kf_thumbs`
            strip_key: None,
            // **P27 ①只服务胶片条**：预览是"当前标尺那一帧"，不做方向搜索
            // （它的"取样失败"由 [`thumb_keyframe`] 在**选关键帧**那一步就避开了：
            // 一律选视频流之内、且不晚于 `末端 − ε` 的关键帧）。
            alts: Vec::new(),
            strip_idx: None,
        };
        if tx.send(job).is_ok() {
            self.thumb_pending = Some((t, bucket_w));
        }
    }

    /// 收"鼠标附近关键帧"的解码结果（非阻塞），回填缓存。
    fn poll_near_thumbs(&mut self, ctx: &egui::Context) {
        if self.thumb_job_rx.is_none() {
            return;
        }
        // 先把消息取成 owned 值，避免在借住 `self.thumb_job_rx` 的同时改别的字段
        let msg = match self.thumb_job_rx.as_ref() {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match msg {
            Ok(res) => {
                self.thumb_pending = None;
                // 结果可能属于上一个 clip（用户中途换了视频）→ 直接丢掉，不许污染缓存
                let same_clip = self.clip.as_ref().is_some_and(|c| c.src == res.path);
                if same_clip {
                    match res.frame {
                        Ok((w, h, rgba)) => {
                            // **P27 ①：方向搜索命中**（实际解出的时刻 ≠ 请求时刻）——
                            // 画面归位到**请求的键**（那是绘制查表用的键），
                            // 但如实记一行"这块是搜出来的"，便于实机取证。
                            if let Some(k) = res.strip_key {
                                if res.decoded_ms != res.at_ms {
                                    p27_log(
                                        "p27_strip_fallback",
                                        false,
                                        p27_fallback_line(
                                            k.time,
                                            res.decoded_ms,
                                            FallbackSource::Direction,
                                            "",
                                        ),
                                    );
                                }
                            }
                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &rgba,
                            );
                            // 纹理名带上"属于哪张表"（胶片条那一侧用它的三元键），
                            // 免得同一个时刻的两个尺寸档在调试器里看着一模一样。
                            let name = match res.strip_key {
                                Some(k) => format!(
                                    "strip-thumb-{}-{}-{}@{}",
                                    k.time, k.w, k.h, res.decoded_ms
                                ),
                                None => format!("kf-thumb-{}-{}", res.at_ms, res.bucket_w),
                            };
                            let tex = ctx.load_texture(name, img, egui::TextureOptions::LINEAR);
                            // **P24：按任务自带的归属回填**（胶片条 → `strip_thumbs`，键带高度；
                            // 预览 → `kf_thumbs`）。绝不靠猜 —— 猜错就是把一个高度档的画面
                            // 写进另一个高度档的键。
                            match res.strip_key {
                                Some(key) => {
                                    self.remember_strip_thumb(key, CachedThumb { w, h, tex })
                                }
                                None => self.remember_thumb(
                                    res.at_ms,
                                    res.bucket_w,
                                    CachedThumb { w, h, tex },
                                ),
                            }
                        }
                        Err(e) => {
                            // **失败的键不再重试**（见 `thumb_failed` 的说明）：
                            // 不记住它就会"每帧重排一次 → 每帧失败一次 → 永远在途"。
                            if self.thumb_failed.len() >= STRIP_CACHE_MAX {
                                self.thumb_failed.clear();
                            }
                            let fail_key = match res.strip_key {
                                Some(k) => (k.time, k.w, k.h),
                                // 预览那一侧用"宽度"补上高度分量（理由见 `request_thumb`）
                                None => (res.at_ms, res.bucket_w, res.bucket_w),
                            };
                            self.thumb_failed.insert(fail_key);
                            // **P27 ④：只警告不打扰**——这里**不再**写 `self.notice`
                            // （用户实机看到的就是那条"关键帧缩略图解码失败（不影响剪辑）"）。
                            // 胶片条那一侧接着走兜底链（②邻块 → ③缓存 → ④留空），
                            // 保证"只要文件里有任意一帧可解，5 个槽位就不空着"。
                            match res.strip_key {
                                Some(k) => {
                                    let src = self.p27_strip_fallback(k, res.strip_idx);
                                    // **只记一行**（`cap.log` + 开发控制台），界面不弹任何提示
                                    p27_log(
                                        "p27_strip_sample_failed",
                                        true,
                                        p27_fallback_line(k.time, res.decoded_ms, src, &e),
                                    );
                                }
                                None => {
                                    // 预览那一侧同样不打扰用户：`thumb_keyframe` 已经只会选
                                    // 视频流之内、且不晚于 末端 − ε 的关键帧（P27 ①）。
                                    p27_log(
                                        "p27_kf_thumb_failed",
                                        true,
                                        format!("at_ms={} error={e}", res.at_ms),
                                    );
                                }
                            }
                        }
                    }
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => {
                // 有在途任务时会在 `ui()` 里安排一次兜底重绘，这里不必忙等
            }
            Err(TryRecvError::Disconnected) => {
                // 解码线程意外结束：拆掉管道，退回"没有关键帧缩略图"，不再无限重试
                self.thumb_pending = None;
                self.thumb_job_rx = None;
                self.thumb_job_tx = None;
                self.notice = Some(self.i18n.t("notice_kf_thumb_thread").to_string());
            }
        }
    }

    // -----------------------------------------------------------------------
    // 播放：状态机 + 每帧推进（**界面侧绝不等后台**，见 `edit::player` 的模块文档）
    // -----------------------------------------------------------------------

    /// 是否正在播放。
    fn is_playing(&self) -> bool {
        self.play.is_some()
    }

    /// 播放 / 停止（空格键与那颗绿色按钮共用这一个入口）。
    fn toggle_play(&mut self) {
        if self.is_playing() {
            self.stop_play();
        } else {
            self.start_play();
        }
    }

    /// 从**当前标尺**开始播放。
    ///
    /// * 标尺已经停在片尾（`>= duration - 1`）时**从片头重放** —— 按播放却什么都不动
    ///   是最让人困惑的行为；
    /// * 解码线程在这里起（打开文件也在它里面），UI 立刻拿到句柄，一帧都不等磁盘；
    /// * 起线程失败（线程创建失败 / 极少数资源耗尽）时**如实报错并不进入播放状态**，
    ///   绝不出现"按钮变了但画面不动"。
    fn start_play(&mut self) {
        let Some(clip) = self.clip.as_ref() else {
            return;
        };
        let duration = clip.duration_ms.max(1);
        let path = clip.src.clone();
        if self.cursor_ms >= duration - 1 {
            self.cursor_ms = 0;
        }
        let from = self.cursor_clamped();
        let (w, h) = self.play_decode_size();
        // P35：**硬坏段自动跳过**（用户口径：仅硬错误自动跳）。
        // 传空列表时行为与改造前完全一致 —— 干净素材走的正是这条路。
        let skip = self.hard_skip_ranges();
        // 纯音频素材没有 video stream：只启动音频链，由同一个墙钟推进时间线。
        // 以前无条件启动视频播放器，线程报"没有视频流"后会停掉整条播放链。
        let player = if self.play_decode_enabled && clip.video.is_some() {
            match edit::player::PlayerHandle::spawn_skipping(&path, from, w, h, &skip) {
                Ok(p) => Some(p),
                Err(e) => {
                    self.error = Some(self.i18n.tf("err_play_failed", &[&e.to_string()]));
                    return;
                }
            }
        } else {
            None
        };
        self.error = None;
        // ---- 音频输出：与视频链平行起（同一个 from、同一次调用点）----
        //
        // 三种失败都**不冒泡到界面**（纪律：没声音不是播放失败）：
        // ① 素材没有音轨——线程会自行结束，`audio` 停在 Some 也无妨；
        // ② 输出设备打不开——降级为静音播放（日志 + `PlaybackStats::output_live` 留痕）；
        // ③ 线程都没起来——只记日志，画面照播。
        let audio =
            match edit::audio_out::AudioTrackHandle::spawn(&path, from, self.play_audio_output) {
                Ok(a) => Some(a),
                Err(e) => {
                    tracing::warn!(error = %e, "播放：音频输出线程启动失败，本端只有画面");
                    None
                }
            };
        self.play = Some(PlayState {
            player,
            audio,
            pos_ms: from as f64,
            queue: VecDeque::new(),
            tex: None,
            shown_ms: from,
            last_seek_ms: from,
        });
        // 播放也是"主动定位过标尺"：停下后 `[` / `]` 应该能直接用
        self.cursor_committed = true;
    }

    /// 停止播放。**不 join 解码线程**（`PlayerHandle::stop` 只发指令），
    /// 也**不改 `cursor_ms`** —— 调用方负责把标尺留在它想要的位置。
    fn stop_play(&mut self) {
        if let Some(mut p) = self.play.take() {
            if let Some(pl) = p.player.as_mut() {
                pl.stop();
            }
            if let Some(mut a) = p.audio.take() {
                a.stop();
            }
        }
    }

    /// 对外：停止播放（外壳在"离开剪辑页"时调用，避免解码线程在后台空转）。
    pub fn stop_playback(&mut self) {
        self.stop_play();
    }

    /// **播放中手动定位**：把时钟重新锚定到当前标尺，画面从新位置接着播。
    ///
    /// 用户口径（2026-10-17 二次修订）：「缩放影片及拖动滚动条**不影响播放**」——
    /// 也就是说这些操作不再"打断播放"，而是变成**边播边跳**（scrubbing）。
    /// 于是全页只有一条规则：
    /// **播放中任何定位动作 = 跳到那里继续播；只有空格 / 停止按钮 / 播到片尾才停。**
    ///
    /// 不在播放时是空操作（调用方不必先判断）。
    fn reanchor_play(&mut self) {
        // 先算好目标（`cursor_clamped` 借的是 `&self`，必须在拿 `play` 的可变借用之前）
        let t = self.cursor_clamped();
        let Some(p) = self.play.as_mut() else {
            return;
        };
        // 时钟**立刻跟手**：否则下一帧 `tick_play` 会把标尺拉回旧位置，
        // 用户看到的就是"拖了没反应"。
        p.pos_ms = t as f64;
        // 旧位置解出来的帧全部作废（它们与新位置无关，留着会先闪一下旧画面）
        p.queue.clear();
        // 解码线程的 seek 做节流（理由见 [`PLAY_SEEK_MIN_MS`]）
        if (t - p.last_seek_ms).abs() >= PLAY_SEEK_MIN_MS {
            if let Some(pl) = p.player.as_ref() {
                pl.seek(t);
            }
            // 音频链同一个 `t`、同一次发送（两端各自 seek，相位差 < 一个 AAC 预热段）
            if let Some(a) = p.audio.as_ref() {
                a.seek(t);
            }
            p.last_seek_ms = t;
        }
    }

    /// 播放的解码尺寸：跟预览画框走（量化到档位、不超过源尺寸）。
    ///
    /// 为什么按画框而不是按源尺寸：1080p 素材在 600 px 的画框里，
    /// 解 1920 宽只是白烧 CPU —— 缩到画框大小后帧率能高好几倍。
    fn play_decode_size(&self) -> (u32, u32) {
        let (src_w, src_h) = self.source_width_height();
        let bucket = if self.preview_frame_w > 1.0 {
            thumb_bucket_w(self.preview_frame_w, src_w)
        } else if src_w > 0 {
            src_w.min(THUMB_MAX_DECODE_W)
        } else {
            THUMB_MIN_DECODE_W
        };
        let (w, h) = decode_size(bucket, self.source_aspect());
        // 再夹一次源尺寸：`decode_size` 只保证比例，不保证不放大
        if src_w > 0 && src_h > 0 {
            (w.min(src_w).max(1), h.min(src_h).max(1))
        } else {
            (w.max(1), h.max(1))
        }
    }

    /// **每帧推进播放**：走时钟 → 收帧 → 挑一帧贴出去 → 到片尾收尾。
    ///
    /// 调用点在 `ui()` 的**最前面**：这样时间线、预览、时间码读到的都是本帧的新位置。
    fn tick_play(&mut self, ctx: &egui::Context) {
        if self.play.is_none() {
            return;
        }
        let Some(duration) = self.clip.as_ref().map(|c| c.duration_ms.max(1)) else {
            // 素材被换掉了（不该发生）：直接收工，别让时钟悬在半空
            self.stop_play();
            return;
        };
        // 时钟步长：`unstable_dt` 是**真实**的两帧间隔（`stable_dt` 会被平滑，
        // 拖动窗口造成的长帧会被"摊平"，播放位置就会落后于墙钟）。
        // 上限 0.25 s：一次长时间卡顿（比如拖动窗口）之后不该把画面瞬移半分钟。
        let dt = ctx.input(|i| i.unstable_dt).clamp(0.0, 0.25) as f64;
        let mut ended = false;
        let mut pos = 0.0f64;
        if let Some(p) = self.play.as_mut() {
            p.pos_ms += dt * 1000.0;
            if p.pos_ms >= duration as f64 {
                p.pos_ms = duration as f64;
                ended = true;
            }
            pos = p.pos_ms;
            Self::pump_play_frames(p, ctx);
        }
        self.cursor_ms = pos.round() as i64;
        if ended {
            self.stop_play();
        }
        // 播放期间**每帧都要下一帧**：这是唯一的动画驱动（没有它，界面只在有输入时才刷新）
        ctx.request_repaint();
    }

    /// 收帧 + 挑帧 + 贴纹理（**不碰 `self`**，只动 `PlayState`，避免借用冲突）。
    ///
    /// 挑帧规则：取"**最后一个 `pts <= 当前位置`**"的帧。
    /// 为什么不是"取最新一帧"：解码线程可能领先墙钟 200 ms（见 `edit::player::LEAD_MS`），
    /// 取最新一帧就等于把画面提前放出来、音画观感全都错位；
    /// 按 pts 对齐时钟才是播放器该有的行为。
    fn pump_play_frames(p: &mut PlayState, ctx: &egui::Context) {
        let Some(player) = p.player.as_ref() else {
            return;
        };
        // ① 把已经解好的帧全收进来（非阻塞）
        while let Some(f) = player.try_recv() {
            p.queue.push_back(f);
        }
        // ② 丢掉"已经过去"的帧：只留最后一个 pts <= pos 的
        while p.queue.len() >= 2 && (p.queue[1].pts_ms as f64) <= p.pos_ms {
            p.queue.pop_front();
        }
        // ③ 队首这一帧到点了就贴出去（还没到点就继续显示上一帧）
        let due = p
            .queue
            .front()
            .map(|f| (f.pts_ms as f64) <= p.pos_ms)
            .unwrap_or(false);
        if !due {
            return;
        }
        let Some(f) = p.queue.pop_front() else {
            return;
        };
        let size = [f.w.max(1) as usize, f.h.max(1) as usize];
        let image = egui::ColorImage::from_rgba_unmultiplied(size, &f.rgba);
        match p.tex.as_mut() {
            // 尺寸没变 → **原地更新**（不重建纹理，省一次 GPU 分配与一次析构）
            Some(t) if t.size() == size => t.set(image, egui::TextureOptions::LINEAR),
            _ => {
                p.tex =
                    Some(ctx.load_texture("cap-editor-play", image, egui::TextureOptions::LINEAR))
            }
        }
        p.shown_ms = f.pts_ms;
    }

    /// 收解码线程的错误（非阻塞，取一次就清空）。
    fn poll_play_error(&mut self) {
        let err = self
            .play
            .as_ref()
            .and_then(|p| p.player.as_ref())
            .and_then(|pl| pl.take_error());
        if let Some(e) = err {
            self.error = Some(self.i18n.tf("err_play_failed", &[&e]));
            self.stop_play();
        }
    }

    /// 收导出结果（非阻塞）
    fn poll_export(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        let mut outcome: Option<Result<ExportReport, crate::error::AppError>> = None;
        if let Some(rx) = &self.export_rx {
            match rx.try_recv() {
                Ok((epoch, Ok(rep))) if epoch == self.export_epoch => {
                    outcome = Some(Ok(rep));
                    finished = true;
                }
                Ok((epoch, Err(e))) if epoch == self.export_epoch => {
                    outcome = Some(Err(e));
                    finished = true;
                }
                Ok(_) => {
                    // 旧世代结果：丢弃，但不打断当前世代。
                }
                Err(TryRecvError::Empty) => ctx.request_repaint(),
                Err(TryRecvError::Disconnected) => {
                    self.error = Some(self.i18n.t("err_export_thread_died").to_string());
                    finished = true;
                }
            }
        }
        if finished {
            self.export_rx = None;
            self.exporting = false;
            self.export_cancel = None;
            match outcome {
                Some(Ok(rep)) => self.report = Some(rep),
                Some(Err(e)) => {
                    self.error = Some(self.i18n.tf("err_export_failed", &[&e]));
                }
                None => {}
            }
        }
    }

    /// 点"导出"：选保存路径 → 后台线程做 copy 导出
    ///
    /// **单段**走历史路径 `edit::export_copy`（行为一字不变）；
    /// **多段**走 `edit::export_ranges`（先过一次 `resolve_segments` 校验：
    /// 排序/重叠/空段问题在选保存路径之前就报出来，不浪费用户一次对话框）。
    fn start_export(&mut self) {
        self.error = None;
        self.notice = None;
        self.report = None;
        let Some(clip) = self.clip.clone() else {
            self.error = Some(self.i18n.t("err_export_no_video").to_string());
            return;
        };
        let segments = self.export_segments();
        let merged = segments.len() > 1;
        // 先按吸附结果校验一次，避免用户选完保存路径才发现区间非法。
        // **单段也走 `resolve_segments`**（P35）：它比 `effective_range` 多做两件事 ——
        // 剔除编码错误的坏段、以及"选区全在坏区里"的报错。放在对话框**之前**，
        // 用户就不用先选一遍保存路径才知道导不出来。
        if let Err(e) = edit::resolve_segments(&clip, &segments) {
            self.error = Some(self.i18n.tf("err_export_unavailable", &[&e]));
            return;
        }
        let stem = clip
            .src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "clip".into());
        let suffix = if merged { "-merged.mp4" } else { "-clip.mp4" };
        let mut dlg = rfd::FileDialog::new()
            .add_filter(self.i18n.t("dlg_filter_mp4"), &["mp4"])
            .set_file_name(format!("{stem}{suffix}"));
        // 默认落到**录制目录**（与「打开视频」同一个口径）
        if let Some(dir) = self.recording_dir() {
            dlg = dlg.set_directory(dir);
        }
        let Some(dst) = dlg.save_file() else {
            self.notice = Some(self.i18n.t("notice_export_cancelled").to_string());
            return;
        };

        let (tx, rx) = channel();
        let export_epoch = self.export_epoch.wrapping_add(1);
        self.export_epoch = export_epoch;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        self.export_cancel = Some(cancel);
        self.export_rx = Some(rx);
        self.exporting = true;
        // copy 导出不解码，实际就是磁盘拷贝速度；但仍放后台线程避免卡界面
        let spawned = std::thread::Builder::new()
            .name("clip-export".into())
            .spawn(move || {
                let r = if merged {
                    edit::export_ranges_cancel(&clip, &segments, &dst, &cancel_thread)
                } else {
                    edit::export_copy_cancel(
                        &clip,
                        segments[0].start_ms,
                        segments[0].end_ms,
                        &dst,
                        &cancel_thread,
                    )
                };
                let _ = tx.send((export_epoch, r));
            });
        if spawned.is_err() {
            self.export_rx = None;
            self.exporting = false;
            self.export_cancel = None;
            self.error = Some(self.i18n.t("err_export_thread").to_string());
        }
    }

    /// 当前标尺的位置（毫秒），已按素材时长夹紧。
    fn cursor_clamped(&self) -> i64 {
        match self.clip.as_ref() {
            Some(c) => self.cursor_ms.clamp(0, c.duration_ms.max(0)),
            None => self.cursor_ms.max(0),
        }
    }

    /// 缩略图与"最近关键帧"高亮要看的那个时间点（**只用于预览**）。
    ///
    /// 口径（用户最终要求：**"就显示蓝色标尺所在位置的预览"**）：
    /// 预览 = **蓝色当前标尺 `cursor_ms` 所在位置最近的那个关键帧**。
    /// 悬停、拖动都**不再驱动预览**；标尺本身仍按既有规则移动（点击时间线 / 拖蓝色把手），
    /// 那两条路径改了 `cursor_ms`，预览就跟着变 —— 所以"松手后不闪空"是天然成立的。
    ///
    /// 注意：**起终点、当前标尺、`设为起点/终点` 一律只读 `cursor_ms`**，
    /// 与这个预览口径完全分开 —— 不要在任何提交路径上使用本函数。
    fn preview_ms(&self) -> Option<i64> {
        let clip = self.clip.as_ref()?;
        let cursor = self.cursor_clamped();
        // **预览 = 蓝色当前标尺所在位置最近的那个关键帧**（用户最终语义）。
        // 没有任何关键帧信息时退回标尺位置本身，保证"始终有值、不闪空"。
        Some(edit::nearest_keyframe(clip, cursor).unwrap_or(cursor))
    }

    /// 把**当前标尺**记为**起点原始值**。吸附不在这里做：
    /// 显示走 [`edit::snap_start`]、导出走 [`edit::effective_range`]，
    /// 与拖动绿色把手得到的语义完全一致（不破坏既有吸附语义）。
    ///
    /// 为什么作用于 `cursor_ms` 而不是鼠标位置：鼠标一滑动位置就变，
    /// "去点按钮的路上位置已经移动了"，用户永远定位不到自己想要的那一帧。
    fn mark_start_at_cursor(&mut self) {
        self.sel_start_ms = self.cursor_clamped();
    }

    /// 把**当前标尺**记为**终点原始值**（吸附语义同上）。
    fn mark_end_at_cursor(&mut self) {
        self.sel_end_ms = self.cursor_clamped();
    }

    /// 键盘微调当前标尺：`dir` 为 -1/+1，步长规则见 [`nudge_cursor_ms`]。
    fn nudge_cursor(&self, dir: i32, mods: egui::Modifiers, duration: i64) -> i64 {
        match self.clip.as_ref() {
            Some(c) => nudge_cursor_ms(c, self.cursor_clamped(), dir, mods).clamp(0, duration),
            None => self.cursor_ms.clamp(0, duration),
        }
    }

    // -----------------------------------------------------------------------
    // 片段列表（多段合并导出，P36 起是"播放列表"语义）
    //
    // `sel_start_ms/sel_end_ms` 有两个身份：
    // * **选中某段时**：它是那段值的镜像 —— 所有既有编辑入口（设为起点/终点、
    //   拖把手、键盘、时间码输入框）都只写这两个字段，每帧由
    //   `sync_active_segment` 同步回列表，于是"拖把手微调某段"天然可用；
    // * **没选中时**：它是一段独立的"待添加选区"，改它不会动列表。
    //
    // 这个双身份让"连续选段、连续添加"成立：点「添加区间」后主动清空选中，
    // 用户接着选下一段时不会覆盖刚加进去的那段。
    // -----------------------------------------------------------------------

    /// 把 `sel_*`（当前选中段的编辑结果）同步回片段列表。
    ///
    /// **不再重排**（P36）：改造前这里每帧按 `start` 升序重排，于是"把第 3 段拖到
    /// 第 1 位"会在下一帧被打回原状 —— 拖拽调序根本不可能生效。现在列表顺序
    /// **只由用户拖拽决定**，与各段的时间位置无关（顺序 = 播放顺序）。
    ///
    /// 没选中任何段时什么都不做 —— 此时 `sel_*` 是"待添加选区"，不属于列表。
    fn sync_active_segment(&mut self) {
        let Some(i) = self.active_seg else { return };
        if i >= self.segments.len() {
            self.active_seg = None;
            return;
        }
        self.segments[i] = (self.sel_start_ms, self.sel_end_ms);
    }

    /// 切换当前段：先把旧段的最终值存回去，再把新段的值读回 `sel_*`。
    /// 下标非法或就是当前段时什么都不做（调用方不需要判空）。
    fn set_active_segment(&mut self, idx: usize) {
        if idx >= self.segments.len() || self.active_seg == Some(idx) {
            return;
        }
        self.sync_active_segment();
        self.active_seg = Some(idx);
        self.mirror_active_segment();
    }

    /// 把当前段的值读回 `sel_*`（切换段之后，所有编辑入口又只写 `sel_*`）。
    fn mirror_active_segment(&mut self) {
        let Some(i) = self.active_seg else { return };
        if let Some(&(s, e)) = self.segments.get(i) {
            self.sel_start_ms = s;
            self.sel_end_ms = e;
        }
    }

    /// **导出用的分段列表**：列表非空时用列表（顺序 = 播放顺序）；
    /// 空列表时直接用 `sel_*`（历史单段路径，行为一字不变）。
    fn export_segments(&self) -> Vec<edit::Segment> {
        if !self.segments.is_empty() {
            self.segments
                .iter()
                .map(|&(s, e)| edit::Segment::new(s, e))
                .collect()
        } else {
            vec![edit::Segment::new(self.sel_start_ms, self.sel_end_ms)]
        }
    }

    /// 分片表里所有分段的**显示用有效区间**（吸附后的起止，与单段显示同口径）。
    /// 返回与 `self.segments` 等长、一一对应的 Vec（克隆一份避免借用冲突）。
    fn segment_effective_ranges(&self) -> Vec<(i64, i64)> {
        match self.clip.as_ref() {
            Some(clip) => self
                .segments
                .iter()
                .map(|&(s, e)| match edit::effective_range(clip, s, e) {
                    Ok(r) => r,
                    Err(_) => (edit::snap_start(clip, s), edit::snap_end(clip, e)),
                })
                .collect(),
            None => self.segments.clone(),
        }
    }

    /// **合并导出的总时长**（吸附后的精确值，供导出按钮下方显示）。
    ///
    /// 口径与导出结果一致：逐段起点向右、终点向左吸附到关键帧后求和。
    /// 编码错误坏段被剔除掉的那部分不在这里扣（那要跑一遍 `resolve_segments`，
    /// 每帧做太重），导出完成后由结果卡片里的"已跳过 N 秒"结算。
    fn segments_total_ms(&self) -> i64 {
        self.segment_effective_ranges()
            .iter()
            .map(|&(s, e)| (e - s).max(0))
            .sum()
    }

    /// 「添加区间」：把**当前入点→出点**作为一个新片段追加到列表末尾。
    ///
    /// 语义（P36 改）：改造前是"在当前标尺处插 2 秒并钳到邻居之间"，那是在切蛋糕；
    /// 用户要的是"选好一段、加进列表"。所以现在直接取 `sel_*`，**不再钳邻居**
    /// （允许重叠、允许和已有段完全相同 —— 播放列表语义，播两遍是合法需求）。
    fn add_segment(&mut self) {
        let duration = match self.clip.as_ref() {
            Some(c) => c.duration_ms.max(0),
            None => {
                self.error = Some(self.i18n.t("err_export_no_video").to_string());
                return;
            }
        };
        if duration <= 0 {
            self.error = Some(self.i18n.t("err_export_no_video").to_string());
            return;
        }
        // 先把"选中段"的最新编辑结果落回去，再取待添加选区
        self.sync_active_segment();
        let s = self.sel_start_ms.clamp(0, duration);
        let e = self.sel_end_ms.clamp(0, duration);
        // 用**吸附后**的长度判空：吸附前看着有 40 ms、吸附后可能一段都不剩
        let (es, ee) = match self.clip.as_ref() {
            Some(clip) => edit::effective_range(clip, s, e)
                .unwrap_or((edit::snap_start(clip, s), edit::snap_end(clip, e))),
            None => (s, e),
        };
        if ee - es < 50 {
            self.notice = Some(self.i18n.t("notice_segment_too_short").to_string());
            return;
        }
        self.segments.push((s, e));
        // **加完就取消选中**：让 `sel_*` 回到"待添加选区"，用户可以接着选下一段
        // （否则下一帧 sync 会把刚加的这段改成新选区，连续添加永远只能加最后一段）
        self.active_seg = None;
        // 不再弹"已添加第 N 个片段（时长 X）"（用户要求删）：芯片行就在按钮下面，
        // 新芯片立刻出现在那里，横幅只是把同一件事又说一遍。
        // 顺手清掉上一条提示 —— 最常见的是上一次"入点到出点太短"，加成功了还挂着很迷惑。
        self.notice = None;
    }

    /// 「删除区间」：删掉**当前选中**的那一段；没选中时删**最后一行**
    /// （用户刚加的段就在末尾，这个兜底最符合直觉）。
    ///
    /// 删到空是合法的 —— 空列表 = 回到单段模式（`export_segments` 会退回 `sel_*`）。
    fn delete_segment(&mut self) {
        if self.segments.is_empty() {
            return;
        }
        let idx = self
            .active_seg
            .filter(|&i| i < self.segments.len())
            .unwrap_or(self.segments.len() - 1);
        self.segments.remove(idx);
        // 删完不选中任何段：`sel_*` 回到"待添加选区"，用户可以立刻重选重加
        self.active_seg = None;
    }

    /// **片段芯片行**（P36）：一个芯片一段，点击选中、**按住左右拖动改合成顺序**。
    ///
    /// 为什么是横向芯片而不是竖向列表：这一行只占约 26 px，而竖向列表至少要 100+ px
    /// —— 页面高度卡得很死（`MIN_INNER_H` 是实测常量），列表会把最小窗口顶高到
    /// 1366×768 装不下的程度。芯片放不下时 `horizontal_wrapped` 会自动换行。
    ///
    /// 芯片只写**序号**（"片段 3"），**不再写入出点**（用户要求）：起止位置在时间线上
    /// 一眼就能看到，写进芯片只会让每片变宽一倍、一行放不下几段。
    ///
    /// **必须自绘 + `Sense::click_and_drag()`**：第一版用的是 `egui::Button`，而
    /// `Button` 默认 `Sense::click()` —— 只感点击、**根本不产生 drag 事件**，于是
    /// `drag_started()` / `drag_stopped()` 永远为假，"拖拽调序"看着有代码却完全没反应
    /// （用户两次反馈"还是不能移动顺序"就是它）。自绘还能顺手画"松手会插到哪儿"的指示线。
    ///
    /// **「片段列表」标签**（P39，用户要求"在片段前面加'片段列表'"）：与芯片**同一行**
    /// （`horizontal_wrapped` 的第一项），小字次要色 + 8px 间距，不独占一行——
    /// 这一行的高度直接进页面必需高度（`SEGMENT_LIST_SLACK` 只算了单行），
    /// 独占一行会把最小窗口再顶高约 23px。
    fn segment_chips(&mut self, ui: &mut egui::Ui, i18n: &crate::i18n::I18n) {
        let n = self.segments.len();
        if n == 0 {
            return;
        }
        // 循环里只记录意图，循环结束后再改 `self`（避免遍历时又可变借用 `self`）
        let mut clicked: Option<usize> = None;
        let mut want_start: Option<usize> = None;
        let mut want_drop = false;
        let mut rects: Vec<egui::Rect> = Vec::with_capacity(n);
        // 无条件赋值（不只是测试构建读它）：否则发布构建里它是"只写不读"→ `unused_mut` 告警
        let mut seg_label: Option<egui::Rect> = None;
        let drag_from = self.seg_drag;
        let pointer_x = ui.ctx().pointer_interact_pos().map(|p| p.x);
        let font = egui::FontId::proportional(crate::ui::theme::FS_SMALL);

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // 「片段列表」行首标签：跟着芯片一起换行（窄窗口时标签单独占第一行也行），
            // 所以它放在 wrapped 流里而不是外面另起一行。
            #[cfg_attr(not(test), allow(unused_variables))]
            let label_resp = ui.label(
                egui::RichText::new(i18n.t("segments_label"))
                    .size(crate::ui::theme::FS_SMALL)
                    .color(crate::ui::theme::muted()),
            );
            seg_label = Some(label_resp.rect);
            ui.add_space(8.0);
            for i in 0..n {
                let label = i18n.tf("segment_chip_label", &[&(i + 1)]);
                let active = self.active_seg == Some(i);
                let dragging = drag_from == Some(i);
                let fg = if active {
                    egui::Color32::WHITE
                } else if dragging {
                    crate::ui::theme::accent()
                } else {
                    crate::ui::theme::muted()
                };
                // 先量文字再定尺寸：`layout_no_wrap` 只借 `&Painter`，借用在语句末尾就还回去
                let galley = ui.painter().layout_no_wrap(label, font.clone(), fg);
                let size = egui::vec2(
                    galley.size().x + CHIP_PAD_X * 2.0,
                    galley.size().y + CHIP_PAD_Y * 2.0,
                );
                // `click_and_drag` 是这条交互的**全部关键**：`Sense::click()` 不产生 drag
                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                let painter = ui.painter();
                if active {
                    painter.rect_filled(rect, 6.0, crate::ui::theme::accent());
                } else if dragging {
                    painter.rect_filled(rect, 6.0, seg_inactive_fill());
                    painter.rect_stroke(
                        rect,
                        6.0,
                        egui::Stroke::new(1.5, crate::ui::theme::accent()),
                        egui::StrokeKind::Inside,
                    );
                } else if resp.hovered() {
                    painter.rect_filled(rect, 6.0, seg_hover_fill());
                    painter.rect_stroke(
                        rect,
                        6.0,
                        egui::Stroke::new(1.0, seg_inactive_edge()),
                        egui::StrokeKind::Inside,
                    );
                } else {
                    painter.rect_stroke(
                        rect,
                        6.0,
                        egui::Stroke::new(1.0, seg_inactive_edge()),
                        egui::StrokeKind::Inside,
                    );
                }
                painter.galley(rect.center() - galley.size() * 0.5, galley, fg);
                rects.push(rect);
                if resp.clicked() {
                    clicked = Some(i);
                }
                if resp.drag_started() {
                    want_start = Some(i);
                }
                if resp.drag_stopped() {
                    want_drop = true;
                }
            }
        });

        // ---- 意图落地（此时不再借用 `self`）----
        #[cfg(test)]
        {
            // 拖拽调序的单测要按"芯片中心"定位按下/松手点
            self.test_rects.seg_chips = rects.clone();
            self.test_rects.seg_label = seg_label;
        }
        if let Some(i) = want_start {
            self.seg_drag = Some(i);
        }
        if let Some(i) = clicked {
            self.set_active_segment(i);
        }

        // 落点：指针第一次落在某个芯片中线左侧，就插到它之前；全在右侧则插到末尾。
        let drop_at = |px: f32, rects: &[egui::Rect]| -> usize {
            rects
                .iter()
                .position(|r| px < r.center().x)
                .unwrap_or(rects.len())
        };

        // 拖拽中画**插入指示线**（落在目标芯片的左边缝上）：让用户看得见"松手会插到哪"。
        // 只画在目标芯片自己那一行的高度上（芯片多到换行时不跨行乱画）。
        if let (Some(_), Some(px)) = (self.seg_drag, pointer_x) {
            let idx = drop_at(px, &rects);
            let (x, top, bottom) = if idx == 0 {
                let r = rects[0];
                (r.left() - 2.0, r.top(), r.bottom())
            } else if idx >= n {
                let r = rects[n - 1];
                (r.right() + 2.0, r.top(), r.bottom())
            } else {
                let r = rects[idx];
                (r.left() - 2.0, r.top(), r.bottom())
            };
            ui.painter().line_segment(
                [egui::pos2(x, top - 2.0), egui::pos2(x, bottom + 2.0)],
                egui::Stroke::new(2.0, crate::ui::theme::accent()),
            );
        }

        // 松手（或被拖的芯片已经不再被按下）= 落位。
        let released = want_drop || !ui.input(|i| i.pointer.any_down());
        if released {
            if let Some(from) = self.seg_drag {
                let to = pointer_x.map(|px| drop_at(px, &rects)).unwrap_or(from);
                self.move_segment(from, to);
            }
            self.seg_drag = None;
        }
    }

    /// **拖拽排序**：把 `from` 那一段移动到 `to`（插入语义：落在第 `to` 个位置之前）。
    ///
    /// 列表顺序 = 播放顺序，所以这里不做任何时间校验 —— 把 30 秒处的段
    /// 拖到 5 秒处的段前面，导出就是先播 30 秒那段（`remux_ranges` 逐段独立 seek）。
    fn move_segment(&mut self, from: usize, to: usize) {
        let n = self.segments.len();
        if from >= n || to > n || from == to {
            return;
        }
        let item = self.segments.remove(from);
        // remove 之后目标位置要跟着调整：向后移时少了一位
        let insert_at = if to > from { to - 1 } else { to };
        let insert_at = insert_at.min(self.segments.len());
        self.segments.insert(insert_at, item);
        // 选中标记跟着走：被拖的那段落到新位置，其余受影响的段整体挪一位
        if let Some(act) = self.active_seg {
            self.active_seg = Some(if act == from {
                insert_at
            } else if act > from && act <= insert_at {
                act - 1
            } else if act < from && act >= insert_at {
                act + 1
            } else {
                act
            });
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, i18n: &crate::i18n::I18n) {
        // P1：刷新本页的语言包快照。设置页切换语言后下一帧这里就跟着变，
        // 之后 `open` / `poll_*` / `start_export` 生成的文案也都是新语言。
        if self.i18n.language() != i18n.language() {
            self.i18n = i18n.clone();
        }
        // P15：把本帧的 `egui::Context` 交给在跑的波形任务 —— 两个后台线程在
        // "产出 L0 进度 / 产出一块细化瓦片 / 收尾"时用它 `request_repaint()`。
        // **这就是波形唯一的重绘来源**：没有周期性心跳、没有每帧轮询（见 `wake_ui`）。
        self.wave.set_ui_ctx(ui.ctx());
        // 每帧清零：外壳读到的就是"**本帧**那个复选框有没有被动过"
        self.wave_cfg_changed = false;
        self.poll_export(ui.ctx());
        self.poll_near_thumbs(ui.ctx());
        // ---- 播放：推进时钟 + 收帧（**必须在时间线/预览之前**：本帧读到的都是新位置）----
        self.poll_play_error();
        self.tick_play(ui.ctx());
        // ---- 空格 = 播放 / 停止 ----
        // 只在**没有文本输入框获得焦点**时生效：时间码输入框里的空格应该归输入框
        //（那里空格本身没有语义，但抢走按键会让用户觉得"空格失效了"）。
        //
        // **消费掉这个按键**：egui 的按钮在获得焦点时自己也会响应空格，
        // 不消费就会出现"空格既切换了播放、又点了一下当时获得焦点的那个按钮"。
        if !ui.ctx().text_edit_focused() && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            ui.ctx()
                .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
            self.toggle_play();
        }
        // ---- 波形：收后台进度（非阻塞）+ 收尾（入库 / 如实降级）----
        let wave_polled = self.wave.poll_waves();
        if wave_polled.finished {
            self.finish_wave(wave_polled.clone(), i18n);
        }
        // 仅测试用：每帧重记部件矩形，帧末读到的就是"本帧渲染了什么"
        #[cfg(test)]
        {
            self.test_rects = TestRects::default();
        }
        // 有在途的**缩略图**解码任务时给一个兜底重绘节拍：万一线程那次 request_repaint
        // 没被采纳，界面也会在 50 ms 后自己回来看结果（既不卡住也不忙等）。
        //
        // 波形**没有**心跳（P15）：它的重绘完全由后台线程在新数据产出时
        // `request_repaint()` 驱动（L0 每发一档进度、细化每产出一块瓦片各一次），
        // 所以"没有新瓦片时界面就静止"，不会出现"每帧重算"的活锁。
        if self.thumb_pending.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        // ---- **P46：本页不再有任何"自动载入"** ----
        //
        // 这里原来有一条旁路：`clip` 为空时自动把"录制目录里最新的一条 mp4"载进来
        // （配合 `App` 里那条按 `cfg.last_video_path` 的启动自动载入，一共两条）。
        // 用户要求改成**只认显式打开**，两条都删了 —— 现在进入本页而没有素材时，
        // 界面就老老实实显示"还没有打开视频"那句提示，等用户点「打开视频」，
        // 或者回录制页点「在剪辑页打开」。
        //
        // 删掉它顺带解决了 P34 留下的那个守卫（`open_rx`/`open_pending_name`）：
        // 那条守卫存在的唯一理由，就是防止"打开在途时 `clip` 仍为 `None`"被这里
        // 每帧重新塞一条进来、把用户刚选的文件盖掉。没有自动载入了，就没有这个竞态。
        //
        // P15：（下面的按钮）可能**在本帧**才起了波形任务 ——
        // 帧首那次 `set_ui_ctx` 时它还不存在，这里补交一次；真补上了就再要一帧，
        // 好让后台线程的"数据驱动唤醒"接上（只补一次，不是心跳，见 `set_ui_ctx`）。
        if self.wave.set_ui_ctx(ui.ctx()) {
            ui.ctx().request_repaint();
        }

        // ---- **P34：收在途打开的消息**（探测完成 / 关键帧扫回 / 打开失败）----
        //
        // 放在 check_wave_phase **之前**：`Clip` 消息要在本帧就应用掉，
        // 否则同一帧的判据读到的还是"没有素材"，导入就绪要再等一帧。
        // 打开还在途时它自己会排兜底重绘（OPEN_POLL_MS），界面不会停。
        self.poll_open(Some(ui.ctx()));
        // P35：深度检查的结果也在本帧收掉（在跑的期间它自己排兜底重绘）
        self.poll_deep(ui.ctx());

        // ---- **P23：两阶段落定**（导入就绪 → 才允许自动开始波形）----
        //
        // 为什么放在这里（而不是帧首）：判据里有两条要看**上一帧真的画出了什么**
        // ——「首帧预览已渲染出一帧」由 `paint_preview` 在预览绘制时置位，
        // 「胶片条首块已显示」是上一帧 `timeline()` 里查表命中的结果。
        // 所以本帧读它们、在本帧启动波形，语义就是"就绪条件满足的**下一帧**起开始"。
        //
        // 返回值 = 本帧是否排了**兜底看门狗**那一帧（有限次，见 `arm_import_watchdog`；
        // 排帧本身已经在函数里 `request_repaint_after` 掉了）。导入阶段之外恒为 `false`，
        // 所以这条链对既有行为零影响。这里显式读一下，避免"返回值没人用"的告警。
        let _ = self.check_wave_phase(ui.ctx());

        // ---- 底部**紧邻的两行**：文件信息（上）+ 常驻操作指南（下）----
        // 两行都是 `Panel::bottom`：**先声明的先占底部**，所以 help 贴最下、文件信息正好在它
        // 正上方（构造上就没有夹缝，不靠"手算高度差"）；又因为它们**先占掉高度**，
        // 下面的滚动区（预览所在）拿到的 `available_height()` 已经是"扣掉这两行"的剩余空间，
        // 预览于是自动充满 —— 窗口拉高拉矮都跟着变（规则见 [`fluid_preview_size`]）。
        self.bottom_rows(ui, i18n);

        // 内容放进**滚动区**：这样底部两行（文件信息 / 操作指南）永远可见。
        //
        // **纵向滚动条永远不显示**（用户要求："取消上下滚动条"）：
        // * `ScrollBarVisibility::AlwaysHidden` —— 滚动条**一个像素都不画**，
        //   无论内容是否溢出；窗口又不允许缩到 `min_inner_size` 以下，
        //   而 `min_inner_size` 就是按"本页必需高度"实测出来的（见 `ui::layout`），
        //   所以正常状态下滚动区**根本不需要滚动**（单测断言 `content_h <= viewport_h`）；
        // * `.auto_shrink([false, false])` —— 滚动区**占满**可用空间（不按内容收缩），
        //   这样预览拿到的 `available_height()` 就是"真实剩余高度"，
        //   窗口拉高拉矮都跟着变（规则见 [`fluid_preview_size`]）；
        // * 保留滚动区本体（而不是彻底删掉）只为一种**罕见**情况兜底：
        //   导出完成后的结果卡片会插在预览上方，那一刻内容可能超过窗口高度。
        //   那时用户仍可用滚轮看到全部内容，而**不是**被裁掉/被底部两行盖住 ——
        //   代价是零：滚动条不可见，正常状态下也从不滚动。
        let _scroll = egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 页面直接以控件开始：**不再有页面标题**（与标签栏里高亮的"剪辑"重复）
                // 也**不再有那句说明性长句**（用户要求：红色方框内的信息去掉）。
                // ================= 第 1 行 =================
                // `[打开视频] [重新载入]` ＋ **同一行右侧**：起点 / 终点（可编辑时间码）+ 选区时长。
                // （用户给的版式：起点终点与"打开视频"同一行；文件信息挪到最下方，见底部的面板。）
                // 这一行在"还没打开视频"时也要画（按钮禁用），所以这里先取一次时长与吸附结果。
                let duration = self
                    .clip
                    .as_ref()
                    .map(|c| c.duration_ms.max(1))
                    .unwrap_or(1);
                let (eff_ok, selection_ms) = match self.clip.as_ref() {
                    Some(clip) => {
                        match edit::effective_range(clip, self.sel_start_ms, self.sel_end_ms) {
                            Ok((a, b)) => (true, b - a),
                            Err(_) => (false, 0),
                        }
                    }
                    None => (false, 0),
                };
                let has_clip = self.clip.is_some();
                ui.horizontal_wrapped(|ui| {
                    if crate::ui::theme::ghost_button(ui, i18n.t("btn_open_video")).clicked() {
                        let mut dlg = rfd::FileDialog::new()
                            .add_filter(i18n.t("dlg_filter_video"), &["mp4", "m4v", "mov"]);
                        if let Some(dir) = self.recording_dir().filter(|d| d.is_dir()) {
                            dlg = dlg.set_directory(dir);
                        }
                        if let Some(p) = dlg.pick_file() {
                            self.open(&p);
                        }
                    }
                    if crate::ui::theme::ghost_button(ui, i18n.t("btn_reload")).clicked() {
                        if let Some(p) = self.clip.as_ref().map(|c| c.src.clone()) {
                            self.open(&p);
                        }
                    }
                    ui.add_space(24.0);
                    ui.add_enabled_ui(has_clip, |ui| {
                        // 起点 / 终点（绿 / 红）：每侧只留一个可编辑时间码框（`HH:MM:SS.mmm`，可拖可输）
                        ui.label(
                            egui::RichText::new(i18n.t("start_point"))
                                .color(crate::ui::theme::ok_fg()),
                        );
                        timecode_drag(ui, &mut self.sel_start_ms, duration, i18n);
                        ui.add_space(14.0);
                        ui.label(
                            egui::RichText::new(i18n.t("end_point"))
                                .color(crate::ui::theme::danger()),
                        );
                        timecode_drag(ui, &mut self.sel_end_ms, duration, i18n);
                        ui.add_space(14.0);
                        if !self.segments.is_empty() {
                            // 列表模式：这里显示**合并总时长**（Σ 各段吸附后长度）。
                            // 导出按钮下方还有一份（用户要求），这里留着是因为它紧贴起终点，
                            // 改一段就能立刻看到总长变化。
                            ui.label(
                                egui::RichText::new(i18n.tf(
                                    "segments_total_ms",
                                    &[&self.segments.len(), &fmt_time(self.segments_total_ms())],
                                ))
                                .monospace()
                                .color(crate::ui::theme::muted()),
                            );
                        } else if eff_ok {
                            ui.label(
                                egui::RichText::new(
                                    i18n.tf("selection_ms", &[&fmt_time(selection_ms)]),
                                )
                                .monospace()
                                .color(crate::ui::theme::muted()),
                            );
                        } else {
                            ui.label(
                                egui::RichText::new(i18n.t("selection_invalid"))
                                    .color(crate::ui::theme::err_fg()),
                            );
                        }
                        if let Some(notice) = &self.notice {
                            // 缩略图解码失败之类的问题只是"不影响剪辑"的提示：用低调的横幅
                            let text = notice.clone();
                            theme::banner(ui, theme::Banner::Warn, &text);
                        }
                    }); // add_enabled_ui(has_clip)
                });
                self.sel_start_ms = self.sel_start_ms.clamp(0, duration);
                self.sel_end_ms = self.sel_end_ms.clamp(0, duration);

                // 注：**片段芯片行不在这里**，它跟在下面那排彩色按钮之后（`segment_chips`）。
                // P36 曾试过把芯片换成时间线上方的**竖向片段列表**（带时长列 + 行内删除），
                // 实测把最小窗口顶高 118 px（1366×768 装不下），已回退成横向芯片 + 拖拽调序。

                // ---- 错误醒目显示，绝不 panic ----
                if let Some(err) = &self.error {
                    crate::ui::theme::banner(
                        ui,
                        crate::ui::theme::Banner::Error,
                        &format!("⚠ {err}"),
                    );
                    ui.add_space(crate::ui::theme::GAP_ROW);
                }

                if self.clip.is_none() {
                    ui.add_space(8.0);
                    // P34：打开在途时说"正在打开…"，而不是误导性的"还没有打开视频"
                    let hint = if self.open_pending_name.is_some() {
                        i18n.t("opening_video")
                    } else {
                        i18n.t("no_video_hint")
                    };
                    ui.label(egui::RichText::new(hint).color(crate::ui::theme::muted()));
                    return;
                }

                // 时间线的可用宽度：视图行与时间线共用同一个值，保证"铺满"的口径一致
                let avail_w = ui.available_width().max(MIN_AVAIL_W);

                // ---- 设入点 / 设出点 / 添加片段 / 删除片段（一行四个，外观统一）----
                //
                // 全部作用于**当前标尺** `cursor_ms`，不看鼠标位置：鼠标一滑动位置就变，
                // "去点按钮的路上位置已经移动了"，用户定位不到自己想要的那一帧。
                //
                // **P41：四个按钮的配色全部去掉**（用户要求，见 2c.png 红框）。原来是
                // 绿=设入点 / 红=设出点 / 蓝=添加片段 / 橙=删除片段，现在统一成**中性描边按钮**
                // （白底 + 发丝描边 + 正文色文字）—— 与同一页「打开视频 / 重新载入」完全一致。
                // 为什么不保留语义色：这一行的语义已经由**按钮文字本身**说清了（"设入点"就是
                // 设入点），而四种颜色挤在一行里反而让用户先看颜色、后读字；去掉颜色后
                // 四个按钮同权重，破坏性操作（删除片段）靠**位置**（行尾）区分。
                //
                // 这一行**不再显示**"当前帧 xx:xx"和"将吸附到 …"（用户要求去掉）：
                // 前者是蓝色标尺的位置，时间线上本来就有；后者是按钮落点的预览，
                // 而落点规则（起点向右、终点向左吸附）是固定的，不需要每帧念一遍。
                // "为什么按钮是灰的"这条信息改由 `on_disabled_hover_text` 在悬停时说。
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    // `enabled` 单独传进来（不是靠 `add_enabled_ui` 的副作用）：中性按钮的
                    // **禁用态外观**要显式给一块灰底（egui 的禁用态与静息态同色），
                    // 所以调用方得知道自己这会儿是不是禁用的。
                    let mark_btn = |ui: &mut egui::Ui, text: &str, w: f32, enabled: bool| {
                        ui.add_sized(
                            [w, 32.0],
                            crate::ui::theme::neutral_button(ui, text, 15.0, enabled)
                                .corner_radius(egui::CornerRadius::same(6)),
                        )
                    };
                    // **在用户第一次点击时间线之前禁用**：此时 `cursor_ms` 只是"整片中间"的占位值，
                    // 拿它当用户选的点是错的（用户报的"按钮读的不是我要的点"就是这个坑）。
                    // 按钮的唯一数据来源是 `cursor_ms`，这条路径上**没有任何 hover 回退**。
                    let set_start = ui
                        .add_enabled_ui(self.cursor_committed, |ui| {
                            mark_btn(ui, i18n.t("btn_set_start"), 96.0, self.cursor_committed)
                        })
                        .inner
                        .on_disabled_hover_text(i18n.t("hint_pick_cursor_first_disabled"));
                    #[cfg(test)]
                    {
                        self.test_rects.set_start_btn = Some(set_start.rect);
                    }
                    if set_start.clicked() {
                        self.mark_start_at_cursor();
                    }
                    ui.add_space(6.0);
                    let set_end = ui
                        .add_enabled_ui(self.cursor_committed, |ui| {
                            mark_btn(ui, i18n.t("btn_set_end"), 96.0, self.cursor_committed)
                        })
                        .inner
                        .on_disabled_hover_text(i18n.t("hint_pick_cursor_first_disabled"));
                    #[cfg(test)]
                    {
                        self.test_rects.set_end_btn = Some(set_end.rect);
                    }
                    if set_end.clicked() {
                        self.mark_end_at_cursor();
                    }
                    ui.add_space(6.0);
                    // ---- 添加片段 / 删除片段：紧跟在设出点后面（用户要求放这里）----
                    let add_seg = ui
                        .add_enabled_ui(has_clip, |ui| {
                            mark_btn(ui, i18n.t("btn_add_segment"), 108.0, has_clip)
                        })
                        .inner;
                    #[cfg(test)]
                    {
                        self.test_rects.add_seg_btn = Some(add_seg.rect);
                    }
                    if add_seg.clicked() {
                        self.add_segment();
                    }
                    ui.add_space(6.0);
                    // 列表为空时无处可删（按钮禁用比"点了没反应"更诚实）
                    let del_seg = ui
                        .add_enabled_ui(has_clip && !self.segments.is_empty(), |ui| {
                            mark_btn(
                                ui,
                                i18n.t("btn_del_segment"),
                                96.0,
                                has_clip && !self.segments.is_empty(),
                            )
                        })
                        .inner;
                    #[cfg(test)]
                    {
                        self.test_rects.del_seg_btn = Some(del_seg.rect);
                    }
                    if del_seg.clicked() {
                        self.delete_segment();
                    }
                    // ---- "片段 3/5"：紧跟在四个按钮后面 ----
                    if !self.segments.is_empty() {
                        ui.add_space(10.0);
                        let cur = self
                            .active_seg
                            .filter(|&i| i < self.segments.len())
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        ui.label(
                            egui::RichText::new(
                                i18n.tf("segment_active_label", &[&cur, &self.segments.len()]),
                            )
                            .color(crate::ui::theme::muted()),
                        );
                        // P40：片段计数后面跟一句"怎么改顺序"（用户要求）。
                        // 与计数器**同一个条件块**里渲染：`segments` 空时两者一起消失，
                        // 不会给这一行引入新的"条件性部件"（egui 的自动 id 按部件序号递增，
                        // 单独包一个 `if` 会让 id 在有无片段之间错位，见文件末尾那段说明）。
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(i18n.t("segment_drag_hint"))
                                .size(crate::ui::theme::FS_SMALL)
                                .color(crate::ui::theme::faint()),
                        );
                    }
                });

                // ---- **片段芯片行**（有片段时出现）：一个芯片一段 ----
                //
                // 点击芯片 = 选中那一段（`sel_*` 载入它的值，时间线上绿/红标尺贴到它两端，
                // 之后拖把手微调就是改这一段）；**按住芯片左右拖动 = 改合成顺序**。
                //
                // 为什么是横向芯片而不是竖向列表：这一行只占约 26 px，而竖向列表至少要 100+ px
                // —— 页面高度卡得很死（`MIN_INNER_H` 是实测常量），列表会把最小窗口顶高到
                // 1366×768 装不下的程度。芯片一行放不下时会自动换行（`horizontal_wrapped`）。
                if !self.segments.is_empty() {
                    ui.add_space(2.0);
                    self.segment_chips(ui, i18n);
                }

                // 注意：时间线是滚动缩放/拖拽的落点，它依赖"本帧真的被 hover 到"。
                // 所以上面几行不能条件性地增减**直接挂在整页 Ui 上**的部件——egui 的自动 id
                // 是按"第几个部件"递增的，一旦错位一帧，这一帧的悬停就会丢（滚轮事件也就丢了）。
                // 现在只有出错提示是条件渲染的，且它只在真的出错时切换，可以接受。
                self.timeline(ui, avail_w, i18n);

                // ---- 导出结果卡片：放在预览**之前** ----
                // 预览吃掉了"时间线以下"的全部剩余高度，所以它后面放不下任何东西（会被推到底部面板
                // 后面、需要滚动才能看到）。导出汇总放在预览上方，点完「导出」就能直接看到。
                if let Some(rep) = &self.report {
                    ui.add_space(crate::ui::theme::GAP_ROW);
                    crate::ui::theme::card(ui, |ui| {
                        ui.label(
                            egui::RichText::new(i18n.t("export_copy_done"))
                                .color(crate::ui::theme::ok_fg())
                                .strong(),
                        );
                        ui.add_space(4.0);
                        // 多段合并导出：在文件信息之前先说清"合并了几段、总时长多少"，
                        // 否则单看首尾时间码看不出中间的空洞被跳过了
                        if rep.segments > 1 {
                            ui.label(crate::ui::theme::mono_muted(&i18n.tf(
                                "export_result_segments",
                                &[&rep.segments, &rep.total_duration_ms],
                            )));
                        }
                        // **P35：被剔除的坏段**必须明说 —— 否则用户会以为导出的就是完整选区
                        if rep.skipped_bad_ms > 0 {
                            ui.label(
                                egui::RichText::new(i18n.tf(
                                    "export_skipped_bad",
                                    &[&(rep.skipped_bad_ms as f64 / 1000.0).round()],
                                ))
                                .color(bad_hard_color()),
                            );
                        }
                        ui.label(crate::ui::theme::mono_muted(
                            &i18n.tf("export_result_file", &[&rep.dst.display()]),
                        ));
                        ui.label(crate::ui::theme::mono_muted(&i18n.tf(
                            "export_result_range",
                            &[
                                &fmt_time(rep.start_ms),
                                &fmt_time(rep.end_ms),
                                &rep.duration_ms(),
                            ],
                        )));
                        let res_word = if rep.same_resolution {
                            i18n.t("export_result_same")
                        } else {
                            i18n.t("export_result_diff")
                        };
                        ui.label(crate::ui::theme::mono_muted(&i18n.tf(
                            "export_result_resolution",
                            &[
                                &rep.src_width,
                                &rep.src_height,
                                &rep.dst_width,
                                &rep.dst_height,
                                &res_word,
                            ],
                        )));
                        ui.label(crate::ui::theme::mono_muted(&i18n.tf(
                            "export_result_size",
                            &[&rep.dst_duration_ms, &rep.packets, &rep.bytes],
                        )));
                    });
                }

                // ---- 当前帧预览（**流式**）+ 「导出」按钮 ----
                //
                // 尺寸规则（**唯一口径**见 [`fluid_preview_size`]，注释里有完整取舍）：
                // * **以高度为准**：`preview_h = 可用高度`（不足 200 时取 200），
                //   `preview_w = preview_h × 源宽/源高` —— 画框比例恒等于源比例，
                //   所以既不拉伸、不裁剪，也不留黑边；
                // * **可用高度**：滚动区里"时间线以下"的剩余高度（`ui.available_height()`），
                //   减去**预览下方必须留出的固定内容**——预览自己那行时间码（+间距）；
                //   "文件信息行 + 操作指南行"这两个底部面板**在进入本函数前就已经把高度占掉了**，
                //   所以 `available_height()` 里天然不含它们（再减一次反而会在两行之间留出空白）；
                //   窗口太窄、按钮换到预览下方时，还要再给按钮留出高度（否则按钮会被裁掉）。
                // * **宽度受限**：`preview_w` 超过"预览能用的宽度"时，先按宽度收敛
                //   （`preview_w = 可用宽度`、`preview_h = preview_w × 源高/源宽`），
                //   **只有在收敛后的高度仍 ≥ 200 时才接受**；否则**保持"以高度为准"的结果**
                //   （宽度超出可用宽度 → 横向裁切/滚动），两种情况都严格保持源比例。
                ui.add_space(crate::ui::theme::GAP_ROW);
                let btn_w = EXPORT_BTN_W;
                let page_w = ui.available_width().max(MIN_AVAIL_W);
                // 同排时预览右侧要让出"间距 + 按钮列"；让完之后预览连 240px 都不到，就把整列换行到下方
                let side_col_w = PREVIEW_EXPORT_GAP + btn_w;
                // 预览下方固定内容：一行时间码（+间距）；按钮列换行到下方时再加上整列
                let caption_h = ui
                    .ctx()
                    .fonts_mut(|f| f.row_height(&egui::FontId::monospace(12.0)))
                    + PREVIEW_CAPTION_GAP;
                // ---- 右列（自上而下）：**时间码 / 播放·停止 / 导出** ----
                //
                // 用户口径：绿色播放按钮在「导出」**正上方**，时间码在播放按钮**正上方**。
                // 三件东西作为**一列整体**与预览垂直居中 —— 而不是让「导出」单独居中：
                // 那样播放按钮会被顶出预览上边界（用户看到的就是"按钮浮在画框外面"）。
                let btn_h = crate::ui::theme::H_PRIMARY;
                let col_gap = crate::ui::theme::GAP_ROW;
                let code_h = caption_h;
                let col_h = code_h + col_gap + btn_h + col_gap + btn_h;
                // ---- 「导出」按钮放哪：右侧，还是换行到预览下方？----
                //
                // 换行的判据有**两条**，第二条必须用**算出来的预览宽度**判，不能只看可用宽度：
                // ① 页面太窄：让出右侧那一列之后预览连 `PREVIEW_SIDE_BTN_MIN_W` 都不到；
                // ② **极宽画幅**（例如 32:9）保高度下限时，预览宽度由"高度 × 源比例"推出，
                //    可能比可用宽度还宽（`fluid_preview_size` 规则 3 允许）—— 这时右侧那一列
                //    根本放不下，不换行就会把按钮**挤出窗口右边界**（旧代码只有判据 ①，
                //    32:9 + 窄窗口下按钮真的会被挤没，见单测 `headless_editor_*` 的极宽画幅一节）。
                //
                // 试算最多两轮：第一轮按"按钮在右侧"算预览；放不下就翻成"按钮在下方"再算一次
                //（`btn_below` 只会从 false 翻到 true，所以一轮足够，写成循环只是为了不重复代码）。
                let mut btn_below = page_w - side_col_w < PREVIEW_SIDE_BTN_MIN_W;
                let remaining_h = ui.available_height();
                // 一轮试算：给定"按钮是否换行到下方"，算出（预览可用宽度、预览可用高度、预览尺寸）
                let attempt = |btn_below: bool| -> (f32, f32, egui::Vec2) {
                    let max_w = if btn_below {
                        page_w
                    } else {
                        (page_w - side_col_w).max(1.0)
                    };
                    let fixed_below = if btn_below {
                        // 预览下方的固定内容：时间码行 + 整列（时间码 / 播放 / 导出）
                        caption_h + col_gap + col_h
                    } else {
                        caption_h
                    };
                    let avail_h = preview_avail_h(remaining_h, fixed_below);
                    (max_w, avail_h, self.preview_size(avail_h, max_w))
                };
                let (mut _max_w, mut _avail_h, mut size) = attempt(btn_below);
                // 判据 ②：算出来的预览宽度 + "间距 + 按钮"放不进页宽（极宽画幅保高度下限时会发生）
                // → 按钮换行到预览下方，再算一次（下方永远放得下）
                if !btn_below && size.x + side_col_w > page_w + 0.5 {
                    btn_below = true;
                    let (w, h, s) = attempt(true);
                    _max_w = w;
                    _avail_h = h;
                    size = s;
                }
                // `_max_w` / `_avail_h` 只有无窗口单测读（非测试构建里就是"没用到的中间量"，
                // 加下划线前缀是**故意**的：见下面 `#[cfg(test)]` 块）
                #[cfg(test)]
                {
                    self.test_rects.preview_avail_h = Some(_avail_h);
                    self.test_rects.preview_avail_w = Some(_max_w);
                }
                // 整组（预览 + 按钮）仍然**水平居中**：先算组宽再在左侧补一段空白。
                // 这一整行的矩形**自己算**（而不是靠 `ui.horizontal` / `ui.vertical` 推着走）：
                // 预览是流式的，"导出"按钮必须精确落在预览右侧、与预览垂直居中，
                // 而且**绝不能被越变越宽的预览挤出视野** —— 这些都必须是算术上的保证。
                let group_w = if btn_below {
                    size.x
                } else {
                    size.x + side_col_w
                };
                let pad = ((page_w - group_w) / 2.0).max(0.0);
                let row_h = size.y + caption_h;
                let block_h = if btn_below {
                    row_h + col_gap + col_h
                } else {
                    row_h
                };
                let (block, _) =
                    ui.allocate_exact_size(egui::vec2(page_w, block_h), egui::Sense::hover());
                // 子 Ui 只用来放这一行里的部件（不参与父级布局推进，位置全部显式给矩形）
                let mut row = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(block)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                let frame =
                    egui::Rect::from_min_size(egui::pos2(block.left() + pad, block.top()), size);
                self.paint_preview(&mut row, frame, caption_h, i18n);
                // ---- 右列三件：时间码 / 播放·停止 / 导出 ----
                //
                // 常态：整列贴预览右侧、与预览**垂直居中**；窄窗口：整列换行到预览下方。
                // 位置全部由矩形算术给出（不靠布局游标），所以预览再怎么流式变化，
                // 这一列也不会被推着跑、更不会被挤出视野。
                let col_left = if btn_below {
                    frame.left()
                } else {
                    frame.right() + PREVIEW_EXPORT_GAP
                };
                let col_top = if btn_below {
                    frame.bottom() + caption_h + col_gap
                } else {
                    // 预览最矮也有 `PREVIEW_MIN_H`(200) > `col_h`，所以这个 max 只是护栏
                    (frame.center().y - col_h / 2.0).max(frame.top())
                };
                let code_rect = egui::Rect::from_min_size(
                    egui::pos2(col_left, col_top),
                    egui::vec2(btn_w, code_h),
                );
                let play_rect = egui::Rect::from_min_size(
                    egui::pos2(col_left, code_rect.bottom() + col_gap),
                    egui::vec2(btn_w, btn_h),
                );
                let btn_rect = egui::Rect::from_min_size(
                    egui::pos2(col_left, play_rect.bottom() + col_gap),
                    egui::vec2(btn_w, btn_h),
                );
                self.playback_controls(&mut row, code_rect, play_rect, i18n);
                self.export_button(&mut row, btn_rect);
                // **预览块之后不再 add_space**：这一块的高度就是"滚动区里剩下的全部高度"
                // （`block_h = size.y + caption_h = ui.available_height()`），再补一段间距就等于
                // 让内容**必然**比视口高出那一段 —— 那样即便窗口很高，这一页也会**一直**挂着
                // 一条纵向滚动条（用户报的"取消上下滚动条"就是这个现象的根源）。
                // 也就是说：下面这个 `_scroll` 的 `content_size.y` 现在正好等于 `inner_rect.height()`，
                // 单测 `headless_editor_required_height_fits_the_min_window` 断言了这一点。
            }); // ScrollArea
                // 本帧的布局测量（最小窗口尺寸 `ui::layout::MIN_INNER_H` 就是用它实测出来的）
        self.probe = LayoutProbe {
            content_h: _scroll.content_size.y,
            viewport_h: _scroll.inner_rect.height(),
            page_avail_h: ui.max_rect().height(),
        };
    }

    /// 本帧的布局测量（内容高度 / 视口高度 / 可用高度）。
    ///
    /// 常规接口而不是 `cfg(test)`：最小窗口尺寸的**整窗**验证在 `CapLite` 二进制里，
    /// 那里读不到库的 `cfg(test)` 符号（见 [`LayoutProbe`] 的说明）。
    pub fn layout_probe(&self) -> LayoutProbe {
        self.probe
    }

    /// **播放控件**：时间码（上）+ 中性「播放/停止」按钮（下），画在给定矩形里。
    ///
    /// 时间码是**当前标尺**（播放时就是播放位置）的完整时间码 `HH:MM:SS.mmm` ——
    /// 与预览下方那行"这一帧的关键帧时刻"是两个不同口径：
    /// 前者是"我播到哪儿了"，后者是"画面上这张图是几秒的"（关键帧对齐）。
    fn playback_controls(
        &mut self,
        ui: &mut egui::Ui,
        code_rect: egui::Rect,
        play_rect: egui::Rect,
        i18n: &crate::i18n::I18n,
    ) {
        let code = fmt_time(self.cursor_clamped());
        ui.put(
            code_rect,
            egui::Label::new(
                egui::RichText::new(code)
                    .monospace()
                    .size(13.0)
                    .color(crate::ui::theme::cursor()),
            ),
        );
        #[cfg(test)]
        {
            self.test_rects.play_timecode = Some(code_rect);
            self.test_rects.play_btn = Some(play_rect);
        }
        let playing = self.is_playing();
        let label = if playing {
            i18n.t("btn_play_stop")
        } else {
            i18n.t("btn_play")
        };
        let enabled = self.clip.is_some();
        let resp = crate::ui::theme::play_button_in(ui, label, play_rect, playing, enabled);
        if resp.clicked() {
            self.toggle_play();
        }
    }

    /// 导出按钮（**中性按钮** + 「导出中…」提示），画在给定矩形里。
    ///
    /// 位置由 `ui()` 算好（预览右侧、与预览垂直居中；窄窗口时在预览下方）：
    /// 这里只管画，不再依赖布局游标 —— 否则按钮会被流式预览推着往右跑。
    fn export_button(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        // P34：关键帧扫描期间禁用（表不完整时 copy 导出会落在非关键帧上，画面起不来）
        let enabled = self.export_enabled();
        // 语言包先借出来（下面 `start_export` 要可变借 `self`，不能同时借着 `self.i18n`）
        let export_label = self.i18n.t("btn_export").to_string();
        let exporting_label = self.i18n.t("exporting").to_string();
        let scanning_label = self.i18n.t("keyframes_scanning").to_string();
        // **P41：去掉按钮颜色**（用户要求，见 2c.png 红框）—— 原来是强调色实心底的主按钮，
        // 现在与「打开视频」等同的中性描边按钮；"位置由 rect 定"这一半职责不变。
        let btn = crate::ui::theme::neutral_button(ui, &export_label, 15.0, enabled)
            .corner_radius(crate::ui::theme::radius());
        let resp = ui
            .scope(|ui| ui.add_enabled_ui(enabled, |ui| ui.put(rect, btn)).inner)
            .inner;
        #[cfg(test)]
        {
            self.test_rects.export_btn = Some(resp.rect);
        }
        if resp.clicked() {
            self.start_export();
        }
        // 提示贴在按钮正下方（按钮矩形是固定的，提示不会顶动别的东西）
        //
        // **P36**：这里现在**恒定有字** —— 没在导出/扫描时显示"这次导出会有多长"。
        // 用户要求"导出按钮下方显示导出影片的总时长"，放在这个位置是因为它紧贴
        // 那个会让事情发生的按钮：点之前就能确认"合起来是多久"。
        // 列表模式给"共 N 段 · 总时长"，单段模式给"选区时长"，两者同一口径
        // （都是吸附后、实际会写进产物的长度）。
        let hint = if self.exporting {
            Some(exporting_label)
        } else if self.keyframes_scanning {
            Some(scanning_label)
        } else if !self.segments.is_empty() {
            Some(self.i18n.tf(
                "segments_total_ms",
                &[&self.segments.len(), &fmt_time(self.segments_total_ms())],
            ))
        } else {
            // 单段模式：与列表模式同位置、同口径
            let (es, ee) = match self.clip.as_ref() {
                Some(c) => {
                    edit::effective_range(c, self.sel_start_ms, self.sel_end_ms).unwrap_or((
                        edit::snap_start(c, self.sel_start_ms),
                        edit::snap_end(c, self.sel_end_ms),
                    ))
                }
                None => (self.sel_start_ms, self.sel_end_ms),
            };
            Some(self.i18n.tf("selection_ms", &[&fmt_time((ee - es).max(0))]))
        };
        if let Some(hint) = hint {
            ui.put(
                egui::Rect::from_min_size(
                    egui::pos2(rect.left(), rect.bottom() + 6.0),
                    // 两行高度：`共 N 段 · 总时长 00:12` 在按钮宽度内会折成两行，
                    // 给一行高度会被裁掉后半句
                    egui::vec2(rect.width(), crate::ui::theme::FS_SMALL * 2.8),
                ),
                egui::Label::new(
                    egui::RichText::new(hint)
                        .size(crate::ui::theme::FS_SMALL)
                        .color(crate::ui::theme::muted()),
                ),
            );
        }
    }

    /// 页面底部**紧邻的两行**：文件信息（上）+ 常驻操作指南（下）。
    ///
    /// 为什么做成两个 `Panel::bottom`：
    /// * **顺序 = 版面顺序**：面板按声明顺序占底部，所以 help 先占最下、文件信息紧接着占它
    ///   正上方 —— 两行**构造上就紧邻**，不靠"手算高度差"，中间不可能出现大段空白；
    /// * **预览因此自动充满**：它们**先占掉高度**，滚动区（预览所在）拿到的
    ///   `available_height()` 已经是"扣掉这两行之后"的剩余空间（见 `ui()` 里的预览尺寸计算）；
    /// * 两行都**不随内容滚动、不会被挤出可视区**（小窗口时是预览变矮/出现滚动条）。
    ///
    /// 高度口径（用户新要求）：两行都**压到"刚好容纳一行文字"** —— 内边距上下的
    /// `PAD_BAR_Y`（2px）+ `item_spacing.y = 0`，文字上下各留 1~2px、字号一点没变小。
    /// 每省下的 1px 都会原封不动地还给预览。
    fn bottom_rows(&mut self, ui: &mut egui::Ui, i18n: &crate::i18n::I18n) {
        // ---- ① 常驻操作指南（**最下**；先声明 = 先占底部）----
        let help = egui::Panel::bottom("cap_editor_help")
            .show_separator_line(false)
            .frame(crate::ui::theme::compact_plain_frame())
            .show(ui, |ui| {
                // 行间距收到 0：这一条只放一行字，多出来的间距就是预览少掉的高度
                ui.spacing_mut().item_spacing.y = 0.0;
                // **P5**：这一行必须在小窗口下**单行完整显示**（用户不接受末尾 `…`）。
                // 实测口径留给单测：`help_avail_w` 记下"这一行字能用的内宽"
                //（= 面板内宽，已含左右内边距的扣除），单测拿它去比较"整句的自然宽度"。
                // 记的是**真渲染**用的那个矩形的宽度，不是再算一遍 padding 公式。
                #[cfg(test)]
                {
                    self.test_rects.help_avail_w = Some(ui.max_rect().width());
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(i18n.t("editor_help"))
                            .size(crate::ui::theme::FS_SMALL)
                            .color(crate::ui::theme::faint()),
                    )
                    .truncate(),
                );
            });
        // 面板矩形留给无窗口单测（`_` 前缀：非测试构建里没有别的用途，但也不是"没用"）
        let _help_rect = help.response.rect;
        #[cfg(test)]
        {
            self.test_rects.help = Some(_help_rect);
        }

        // ---- ② 文件信息（紧贴指南**正上方**）----
        // 同一事实只出现一次：文件名 / 时长 / 分辨率 / 帧率 / 视频编码 / 音频编码+采样率+声道 / 关键帧数
        if self.clip.is_some() {
            let (file_name, duration_ms, kf_len, video_info, audio_info) = self.file_info_data();
            // ---- P35：编码体检状态 ----
            //
            // 只在"真的查到坏段"或"深度检查在跑"时才出现：
            // 干净素材一个字符都不加，这一行与改造前完全一样（不制造噪音）。
            let health = self.health_report();
            let bad_all = health.bad.len();
            let bad_hard = health.bad.iter().filter(|b| b.kind.is_hard()).count();
            let deep_checking = self.deep_checking;
            let deep_pct = if deep_checking {
                let total = self
                    .clip
                    .as_ref()
                    .map(|c| c.duration_ms.max(1))
                    .unwrap_or(1);
                ((self.deep_progress_ms as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as i32
            } else {
                100
            };
            // 悬停明细：每段一行「错因 + 起止 + 细节」
            let health_detail = health
                .bad
                .iter()
                .map(|b| {
                    format!(
                        "{}  {} – {}  {}",
                        self.i18n.t(&format!("bad_{}", b.kind.key())),
                        fmt_time(b.start_ms),
                        fmt_time(b.end_ms),
                        b.detail
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let status_text = if bad_hard > 0 {
                Some((
                    self.i18n.tf(
                        "health_bad_hard",
                        &[&bad_hard.to_string(), &bad_all.to_string()],
                    ),
                    bad_hard_color(),
                ))
            } else if bad_all > 0 {
                Some((
                    self.i18n.tf("health_bad_soft", &[&bad_all.to_string()]),
                    bad_soft_color(),
                ))
            } else {
                None
            };
            let deep_label = if deep_checking {
                self.i18n
                    .tf("health_deep_running", &[&deep_pct.to_string()])
            } else {
                self.i18n.t("health_deep_btn").to_string()
            };
            let mut want_deep = false;
            let info = egui::Panel::bottom("cap_editor_fileinfo")
                .show_separator_line(false)
                .frame(crate::ui::theme::compact_card_frame())
                .show(ui, |ui| {
                    // 上下内边距已经是 2px，这里把**行间距**与**交互控件的最小高度**都收掉：
                    // egui 的 `ui.horizontal` 默认把行高撑到 `spacing.interact_size.y`（主题里是 30），
                    // 而这一行只有静态文字 —— 不收紧的话，行高会从"文字高度"被抬到 30px，
                    // 白白从预览身上抠掉 ~6px。
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().interact_size.y = 0.0;
                    // **不换行**（单行省略）：换行会把这一行撑成两行高，
                    // 与"刚好一行文字"的口径冲突；窗口过窄时右端被面板裁掉即可
                    // （与上面指南行的 `truncate` 同一个取舍：宁可省略，不要撑高）。
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(file_name).color(crate::ui::theme::text()));
                        ui.add_space(10.0);
                        ui.label(crate::ui::theme::mono_muted(&fmt_time(duration_ms)));
                        if let Some((w, h, fps, codec)) = &video_info {
                            ui.add_space(10.0);
                            ui.label(crate::ui::theme::mono_muted(&format!("{w}×{h}")));
                            ui.add_space(10.0);
                            if *fps > 0.0 {
                                // 用"速率"而不是"帧率"：选区相关的界面里只出现时间码，不出现帧数字样
                                ui.label(crate::ui::theme::mono_muted(&format!("{fps:.2} fps")));
                            } else {
                                ui.label(crate::ui::theme::mono_muted(i18n.t("rate_unknown")));
                            }
                            ui.add_space(10.0);
                            ui.label(crate::ui::theme::mono_muted(codec));
                        }
                        if let Some((codec, rate, ch)) = &audio_info {
                            ui.add_space(10.0);
                            ui.label(crate::ui::theme::mono_muted(&format!(
                                "{codec} {rate}Hz/{ch}ch"
                            )));
                        }
                        ui.add_space(10.0);
                        ui.label(crate::ui::theme::mono_muted(
                            &i18n.tf("keyframe_short", &[&kf_len]),
                        ));
                        // ---- P35：编码体检状态（有坏段才出现）----
                        if let Some((txt, col)) = &status_text {
                            ui.add_space(10.0);
                            let r = ui.add(
                                egui::Label::new(egui::RichText::new(txt).color(*col))
                                    .sense(egui::Sense::hover()),
                            );
                            if !health_detail.is_empty() {
                                r.on_hover_ui(|ui| {
                                    ui.label(
                                        egui::RichText::new(&health_detail)
                                            .size(crate::ui::theme::FS_SMALL),
                                    );
                                });
                            }
                        }
                        // 「深度检查」入口：一处小链接，检查中显示百分比
                        ui.add_space(10.0);
                        let deep_resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&deep_label)
                                    .size(crate::ui::theme::FS_SMALL)
                                    .color(if deep_checking {
                                        crate::ui::theme::muted()
                                    } else {
                                        crate::ui::theme::cursor()
                                    })
                                    .underline(),
                            )
                            .sense(egui::Sense::click()),
                        );
                        if deep_resp.clicked() && !deep_checking {
                            want_deep = true;
                        }
                    });
                });
            // 面板矩形留给无窗口单测（理由同上面的 `_help_rect`）
            let _info_rect = info.response.rect;
            #[cfg(test)]
            {
                self.test_rects.file_info = Some(_info_rect);
            }
            if want_deep {
                self.start_deep_check(ui.ctx());
            }
        }
    }

    /// 文件信息那一行要显示的东西（owned 值，避免长时间借住 `self.clip`）。
    #[allow(clippy::type_complexity)]
    fn file_info_data(
        &self,
    ) -> (
        String,
        i64,
        usize,
        Option<(i32, i32, f64, String)>,
        Option<(String, i32, i32)>,
    ) {
        match self.clip.as_ref() {
            Some(clip) => {
                let v = clip.video.as_ref();
                (
                    clip.file_name(),
                    clip.duration_ms,
                    clip.keyframes_ms.len(),
                    v.map(|v| (v.width, v.height, v.fps, v.codec.clone())),
                    clip.audio
                        .as_ref()
                        .map(|a| (a.codec.clone(), a.sample_rate, a.channels)),
                )
            }
            None => (String::new(), 0, 0, None, None),
        }
    }

    /// 时间线：整体缩略图条 + 关键帧刻度 + 绿/红/黄三根标尺 + 悬停预览辅助线。
    ///
    /// 交互契约（本函数的中心思想，也是这次改动的全部意义）：
    /// * **预览只看蓝色当前标尺**：下方那张关键帧缩略图 = `cursor_ms` 最近的关键帧；
    ///   悬停、拖动都**不驱动预览**（用户最终语义："就显示蓝色标尺所在位置的预览"）。
    ///
    /// `avail_w` 由 `ui()` 传进来（而不是在这里再问一次 `ui.available_width()`）：
    /// "整片铺满"的比例、平移范围、时间线实际宽度必须用同一个宽度，否则会互相打架。
    fn timeline(&mut self, ui: &mut egui::Ui, avail_w: f32, i18n: &crate::i18n::I18n) {
        let duration = match self.clip.as_ref() {
            Some(c) => c.duration_ms.max(1),
            None => return,
        };
        // 悬停状态**每帧重算**：它是"本帧的预览"，故意不粘性
        self.hover_live = false;

        // ---- 分片同步：把上一帧各编辑入口（设为起点/终点、时间码输入框）
        // 写进 `sel_*` 的结果同步回分段表 ----
        // 放在这里（绘制之前）可以一次性接住所有写入点：按钮点击发生在
        // `ui()` 的上一段，键盘/把手拖动都发生在本函数内部、绘制之前。
        self.sync_active_segment();
        // 所有分段的显示用有效区间（吸附后），与 `self.segments` 一一对应。
        // 克隆一份：后面绘制/命中测试都要同时读它和写 `self`。
        let seg_effs = self.segment_effective_ranges();
        // 当前段的有效区间：选中某段时用那段（绿/红标尺贴在该段两端）；
        // 没选中时退回 `sel_*`（"待添加选区"本身也要能看见，否则用户不知道
        // 自己将要加进去的是哪一段）
        let (eff_s, eff_e) = self
            .active_seg
            .and_then(|i| seg_effs.get(i))
            .copied()
            .unwrap_or((self.sel_start_ms, self.sel_end_ms));

        // **一次分配、两段几何**（P14）：
        // 时间线在（视觉上）是**一整块**，但它内部有**两种互不相同的交互语义** ——
        // 上面三段（胶片条 / 标记行 / 时间码行）负责"点哪儿当前帧就到哪儿、拖三根标尺"，
        // 最下面那一行（全片位置行）只负责"点/拖整行移动当前帧"（P17 起无小方块）。
        //
        // 所以这里**按语义拆成两个 egui 部件**，交互矩形**上下相接、不重叠**：
        // * 上面那段是 `timeline_rect`，就是原来的 `resp`（语义一行没改）；
        // * 最下面 `POS_ROW_H` 是 `pos_rect` / `pos_resp`（见本节末尾）。
        //
        // 为什么不"一次分配、靠指针 y 判归属"：
        // 那样两个部件会**重叠**（外层 `resp` 覆盖整条），而 egui 对重叠部件只在
        // press 那一帧认一个"按住谁"，release 那一帧另一个部件就拿不到 clicked ——
        // 实测症状就是"点时间码行没反应、当前帧不动"（`headless_click_ruler_*` 抓到的正是它）。
        // 拆开之后每个部件都只在自己那一段里命中，不存在争抢。
        let timeline_h = TIMELINE_H - POS_ROW_H;
        // 两段**上下相接、中间不留缝**：egui 默认会往两个部件之间塞一行 `item_spacing.y`
        // （主题里是 3px），那 3px 会被算进时间线总高、也会让刻度尺带与位置行之间出现一条缝
        //（单测 `p14_pos_row_*` 断言"时间线高 = 各段之和"时抓到的就是它）。
        ui.spacing_mut().item_spacing.y = 0.0;
        let (timeline_rect, resp) = ui.allocate_exact_size(
            egui::vec2(avail_w, timeline_h),
            egui::Sense::click_and_drag(),
        );
        let (pos_rect, pos_resp) = ui.allocate_exact_size(
            egui::vec2(avail_w, POS_ROW_H),
            egui::Sense::click_and_drag(),
        );
        // `rect` = **整条时间线**（含位置行）：绘制、时间→x 换算、单测的 `test_rects.timeline`
        // 都继续用它，于是"时间线整体"在视觉与坐标上仍然是原来那一整块。
        let rect = egui::Rect::from_min_max(
            timeline_rect.min,
            egui::pos2(timeline_rect.right(), pos_rect.bottom()),
        );
        // **不再挂任何悬停提示**（用户反馈：跟随鼠标弹出来的说明层发虚/重叠/错位）。
        // 操作说明改成页面底部**常驻一行**静态文字，见 `ui()` 末尾的 `editor_help`。
        if !rect.is_positive() {
            return;
        }
        #[cfg(test)]
        {
            self.test_rects.timeline = Some(rect);
            self.test_rects.pos_row = Some(pos_rect);
        }
        let left = rect.left() as f64;
        let width = rect.width() as f64;
        if !(left.is_finite() && width.is_finite() && width > 0.0) {
            // 可用区域被压成 0/NaN（极端布局）时不做任何换算，直接不画
            return;
        }

        // ---- 缩放：整片铺满时每帧按当前宽度重算（窗口改宽也铺满）----
        let fit_px = fit_px_per_ms(avail_w, duration);
        if self.fit_mode {
            self.px_per_ms = fit_px;
            self.view_start_ms = 0.0;
        }
        self.px_per_ms = sanitize_px_per_ms(self.px_per_ms, duration);
        let mut px = self.px_per_ms;
        let mut visible_ms = width / px;
        let mut max_view = (duration as f64 - visible_ms).max(0.0);
        self.view_start_ms = self.view_start_ms.clamp(0.0, max_view);
        let mut vs = self.view_start_ms;

        // 时间 ↔ 像素（px/vs 作为参数传入，避免闭包长期借用 self 字段）。
        // 三种视图状态（整片铺满 / 缩放 / 平移）都只是这组换算里 px、vs 的不同取值，
        // 所以下面所有"点击 → 时间"的推导天然对三者都成立。
        let x_of = |t: f64, px: f64, vs: f64| (left + (t - vs) * px) as f32;
        let t_of = |x: f32, px: f64, vs: f64| vs + (x as f64 - left) / px;

        // ---- 预览**只看蓝色当前标尺**（用户最终语义："就显示蓝色标尺所在位置的预览"）----
        // 悬停与拖动都**不再驱动预览**；标尺本身仍按既有规则移动（点击时间线 / 拖蓝色把手），
        // 那两条路径会改 `cursor_ms`，预览自然跟着变（见 `preview_ms()`）。
        //
        // 指针是否在时间线矩形内：**只用于滚轮缩放与键盘快捷键的生效条件**
        //（不驱动预览、不画任何悬停反馈）
        let hovered_for_wheel = ui
            .ctx()
            .pointer_hover_pos()
            .map(|p| rect.contains(p))
            .unwrap_or(false);
        self.hover_live = hovered_for_wheel;
        // ---- 滚轮缩放（**以鼠标位置为锚点**，触发方式不变：时间线区域内的滚轮）----
        //
        // 用户原话：「时间线放缩应该以鼠标所在位置为中心……我一放到（粉红色标记）就跑到后面
        // 看不见了」。所以锚点必须保住"**鼠标下方那个时刻**"在屏幕上的位置 ——
        // 算法与不变式都写在 [`zoom_anchored`] 里（纯函数，有单测）。
        // 这里只负责：把鼠标位置换算成"相对时间线左边缘的像素横坐标"、拿到缩放倍数、写回状态。
        if hovered_for_wheel {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.01 {
                // **缩放只改视口，绝不改当前帧**（P30，用户口径 2026-10-17 三次修订：
                // 「只有在影片上点击鼠标时，才能改变当前帧的位置；缩放影片……不要改变当前帧」）。
                // 所以这里**只**写 `px_per_ms` / `view_start_ms` 两个字段 —— 下面那段
                // "播放中把当前帧拉回视野"的钳制已经删掉：它正是"缩放会挪当前帧"的观感来源
                // （视口被强行搬去居中当前帧 ⇒ 用户眼里画面跳了一下）。
                //
                // **缩放 = 停止播放**（2026-10-17 四次修订，把 P28 第二轮的"不停播"反转回来）：
                // 用户口径「缩放影片及拖动滚动条时，停止播放」。停播不改 `cursor_ms`，
                // 标尺就停在停播那一刻的位置。
                // 锚点：鼠标相对时间线左边缘的 x；鼠标不在时间线上（滚轮来自键盘/触摸板边界）
                // 时退回"视图中心"，仍然是同一个函数、同一条不变式。
                self.stop_play();
                let anchor_x = resp
                    .hover_pos()
                    .map(|p| (p.x - rect.left()) as f64)
                    .filter(|x| x.is_finite())
                    .unwrap_or(width / 2.0);
                let (new_vs, new_px) = zoom_anchored(
                    vs,
                    px,
                    anchor_x,
                    width,
                    zoom_factor(scroll as f64),
                    duration,
                );
                px = new_px;
                visible_ms = width / px;
                // 缩放后可见时长变了，平移余量必须跟着更新（否则这一帧的平移会被旧的余量夹错）
                max_view = (duration as f64 - visible_ms).max(0.0);
                vs = new_vs.clamp(0.0, max_view);
                self.px_per_ms = px;
                self.view_start_ms = vs;
                self.fit_mode = false;
                // 滚回"整片铺满"的比例时自动回到自适应模式，之后窗口改宽仍会铺满
                if (px - fit_px).abs() <= fit_px * 1e-3 && vs <= 0.5 {
                    self.fit_mode = true;
                }
            }
        }

        // ---- 播放中：影片条**翻页** ----
        //
        // 用户口径：「播放时，上方影片条翻页显示，就是说当前帧走到最右侧时，
        // 影片向左翻动，让当前帧到最左侧」。
        //
        // 所以不是"连续跟随"（那会让画面一直在动、眼睛跟着晃），而是**整页翻**：
        // 只在当前帧**离开可见区间**的那一帧改一次视口，其余时间视口完全静止。
        // **P30**：离开可见区间有两种 —— 越过右边界（正常播放）与落在视口左边
        // （缩放把视口搬走之后，见 [`page_view_start`] 的左边界分支）。
        // 整片铺满（`fit_mode`）时全片都在眼前，没有"翻页"可言 —— 不动。
        if self.is_playing() && !self.fit_mode {
            if let Some(new_vs) = page_view_start(self.cursor_ms as f64, vs, visible_ms, max_view) {
                vs = new_vs;
                self.view_start_ms = vs;
            }
        }

        // ---- 左键点击时间线（刻度尺区域与轨道区域都算）→ **当前标尺**定位到点击处 ----
        // 这是鼠标直接写 `cursor_ms` 的唯一入口（另一条是拖蓝色把手），
        // 悬停/拖动都绝不写它 —— 这正是"移向按钮时位置不再乱跑"的原因。
        //
        // **P14**：`resp` 覆盖的是**整条时间线**（含最下面的全片位置行），
        // 而位置行有自己的拖动语义（见函数末尾的 `pos_resp`）。所以要先把
        // "指针落在位置行里"的帧排除掉，否则点位置行会**既按视口映射 seek 一次、
        // 又按全片映射算一次**，两个语义在同一帧打架。判据用**指针位置**
        // （不是 `resp.hovered()`：拖动过程中指针可能已经移出矩形，那时仍应算在拖动里，
        // 由 `self.drag` 决定）。
        // **P14**：`resp` 只覆盖时间码行及**以上**的部分（位置行是另一个部件），
        // 所以这里不再需要"指针是否落在位置行"的门闩 —— 几何上就不可能点错。
        //
        // **多段**：点到**别的分段**上时，先把它设为当前段（`set_active_segment`
        // 会把该段值镜像进 `sel_*`），当前标尺照常移到点击处 —— 于是一击完成
        // "选段 + 定位"，随后「设为起点/终点」与拖把手作用的都是这一段。
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let t = (t_of(p.x, px, vs).round() as i64).clamp(0, duration);
                self.cursor_ms = t;
                // 用户主动选点了：从这里开始，"设为起点/终点"才有意义
                self.cursor_committed = true;
                // 播放中点时间线 = **跳到那里继续播**（不打断播放，见 `reanchor_play`）
                self.reanchor_play();
                // 点在某个片段范围内就把它设为当前段。
                // 注意 `position` 找的是**列表里第一个**覆盖该时刻的段 ——
                // 列表允许重叠（同一区间可以加两次），此时取排在前面的那个，
                // 与"列表顺序 = 播放顺序"一致（先播的那个优先被选中）。
                if !self.segments.is_empty() {
                    if let Some(idx) = seg_effs
                        .iter()
                        .position(|&(s, e)| t >= s && t < e.max(s + 1))
                    {
                        if self.active_seg != Some(idx) {
                            self.set_active_segment(idx);
                        }
                    }
                }
            }
        }

        // ---- 键盘：全部作用于**当前标尺**（鼠标在时间线上或时间线有焦点时生效）----
        // `text_edit_focused()` 用来避让正在编辑的时间码输入框：那时方向键应该归它。
        // `[` / `]` 与按钮同规矩：**必须已经主动定位过标尺**（`cursor_committed`），
        // 否则会拿"整片中间"这个占位值当用户选的点。
        if (self.hover_live || resp.has_focus()) && !ui.ctx().text_edit_focused() {
            let (open, close, left_key, right_key, home, end, mods) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::OpenBracket),
                    i.key_pressed(egui::Key::CloseBracket),
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::Home),
                    i.key_pressed(egui::Key::End),
                    i.modifiers,
                )
            });
            if open && self.cursor_committed {
                self.mark_start_at_cursor();
            }
            if close && self.cursor_committed {
                self.mark_end_at_cursor();
            }
            // 键盘定位（Home / End / 方向键）同样是"手动定位" → 播放中**跳过去继续播**
            let moved_by_key = home || end || left_key || right_key;
            if home {
                self.cursor_ms = 0;
                self.cursor_committed = true;
            }
            if end {
                self.cursor_ms = duration;
                self.cursor_committed = true;
            }
            if left_key {
                self.cursor_ms = self.nudge_cursor(-1, mods, duration);
                self.cursor_committed = true;
            }
            if right_key {
                self.cursor_ms = self.nudge_cursor(1, mods, duration);
                self.cursor_committed = true;
            }
            if moved_by_key {
                self.reanchor_play();
            }
        }

        // 预览位置最近的那个关键帧（抽稀可能把它滤掉，所以下面单独高亮一遍）
        let preview = self.preview_ms();
        let near: Vec<i64> = match (&self.clip, preview) {
            (Some(c), Some(t)) => edit::nearest_keyframe(c, t).into_iter().collect(),
            _ => Vec::new(),
        };

        // ---- 拖拽交互（拖拽本身就是"提交"，不受悬停语义限制）----
        //
        // **多段**：把手命中测试覆盖**所有分段**（不只是当前段）——
        // 命中非当前段的把手时，先切过去（`set_active_segment` 把该段值
        // 镜像进 `sel_*`），随后的拖动写的仍然是"当前段"这一套语义，
        // 于是"拖任何一段的把手都能直接改它"，与点击切换段的行为一致。
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                if resp.dragged_by(egui::PointerButton::Secondary) {
                    self.drag = Some(Drag::Pan);
                } else {
                    let mut best: Option<(usize, bool, f32)> = None; // (段下标, 是起点, 距离)
                    for (i, &(s, e)) in seg_effs.iter().enumerate() {
                        let ds = (p.x - x_of(s as f64, px, vs)).abs();
                        let de = (p.x - x_of(e as f64, px, vs)).abs();
                        if ds <= EDGE_GRAB_PX && best.is_none_or(|(_, _, d)| ds < d) {
                            best = Some((i, true, ds));
                        }
                        if de <= EDGE_GRAB_PX && best.is_none_or(|(_, _, d)| de < d) {
                            best = Some((i, false, de));
                        }
                    }
                    match best {
                        Some((i, is_start, _)) => {
                            if self.active_seg != Some(i) {
                                self.set_active_segment(i);
                            }
                            self.drag = Some(if is_start { Drag::Start } else { Drag::End });
                        }
                        // 蓝色把手（以及轨道空白处）按下拖动 = 拉着当前标尺走。
                        // 这正是需求里"拖把手也能移动当前标尺，且不依赖悬停语义"那条。
                        None => self.drag = Some(Drag::Cursor),
                    }
                }
            }
        }
        if resp.dragged() {
            match self.drag {
                Some(Drag::Pan) => {
                    vs = (vs - resp.drag_delta().x as f64 / px).clamp(0.0, max_view);
                    self.view_start_ms = vs;
                    // 手动平移过就不再是"铺满"状态（否则下一帧又被拉回 0）
                    self.fit_mode = false;
                }
                Some(Drag::Start) => {
                    if let Some(p) = resp.interact_pointer_pos() {
                        self.sel_start_ms = (t_of(p.x, px, vs).round() as i64).clamp(0, duration);
                    }
                }
                Some(Drag::End) => {
                    if let Some(p) = resp.interact_pointer_pos() {
                        self.sel_end_ms = (t_of(p.x, px, vs).round() as i64).clamp(0, duration);
                    }
                }
                Some(Drag::Cursor) => {
                    if let Some(p) = resp.interact_pointer_pos() {
                        self.cursor_ms = (t_of(p.x, px, vs).round() as i64).clamp(0, duration);
                        // 拖把手也是"主动定位标尺"
                        self.cursor_committed = true;
                        // 播放中拖把手 = **边播边拖**（不打断播放）
                        self.reanchor_play();
                    }
                }
                None => {}
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        // 双击轨道：把起点设到当前标尺处（双击的第一次点击已把标尺移到该点，
        // 所以"双击 = 把起点钉在这里"，配合拖蓝色把手可以快速定位）
        if resp.double_clicked() {
            self.mark_start_at_cursor();
        }

        // ---- P30 · **全片位置行 = 纯滚动条**（不再定位当前帧）----
        //
        // 用户口径（2026-10-17 三次修订，**覆盖** P14 的"点/拖整行 = 定位当前帧"）：
        // 「改变当前帧策略，只有当在影片上点击鼠标时，才能改变当前帧的位置，
        //   缩放影片、拖动滚动条，都不要改变当前帧的位置，
        //   当前帧取消保持在显示区域的中间位置。」
        //
        // 于是这一行只剩**滚动**一件事：
        // * 轨道整条全蓝 = **全片**（宽度恒等于时间码行，左右边界对齐）；
        // * 淡色带 = **当前可见区间**（在全片里的位置）；
        // * **拖整行 = 滚视口**（相对位移：指针走多少，可见区间跟着走多少）；
        // * **点整行 = 把可见区间挪到以点击处为中心**（滚动条"跳到这里"的常规语义）；
        // * 两者都**不写 `cursor_ms` / `cursor_committed`**，也不 `reanchor_play()`
        //   —— 滚动条不是定位控件，当前帧一个毫秒都不该动；
        // * 也**不再有"居中跟随"**：视口只在这里被用户主动搬动，绝不追着当前帧跑。
        //
        // 位置行仍走**自己的全片映射** `pos_row_x_of` / `pos_row_t_of`（静态蓝条 = 全片），
        // 而不是时间线本体的视口映射 —— 理由见这两个函数的文档。
        // `pos_resp` 在函数开头与时间线**一起分配**（两段几何上下相接、不重叠），
        // 这里只处理它的语义与像素。
        let pos_left = pos_rect.left() as f64;
        let pos_w = pos_rect.width() as f64;
        // **用户口径（2026-10-17 四次修订）**：「缩放影片及拖动滚动条时，停止播放」——
        // 这两条在 P28 第二轮曾被改成"边播边看"，现在按用户最新要求**反转回来**：
        // 只要动视口（滚轮缩放 / 点拖位置行），播放立刻停；`cursor_ms` 停在当时的位置。
        // 整片铺满（`max_view == 0`）时位置行没有可滚的余量，碰它不算"改视图"，不停播。
        if (pos_resp.dragged() || pos_resp.clicked()) && max_view > 0.0 {
            self.stop_play();
        }
        if pos_resp.dragged() {
            // **相对位移**，不是"指针位置 → 时刻"的绝对映射：后者会形成反馈回路
            //（视口一动，同一指针位置换算出的时刻就变，拖动时视口会自己往前爬）。
            //
            // **方向**：指针右移 = 可见区间右移（`vs` 变大）。`vs` 变大 ⇒ 淡蓝色带在
            // 位置行上往右走 ⇒ "拖着色带走"是直觉方向。符号写反过一次（带子朝反方向跑，
            // 用户实机报的就是这个），所以单测 `p30_dragging...` 断言的是**拖动中途**
            // 的位移方向，而不是松手后的终值 —— 松手那一帧还有"点一下跳到点击处"的语义，
            // 只看终值会把方向错误掩盖掉。
            let d = (pos_resp.drag_delta().x as f64) / pos_w.max(1.0) * duration as f64;
            if d.is_finite() && d != 0.0 {
                vs = (vs + d).clamp(0.0, max_view);
                self.view_start_ms = vs;
                // 手动滚动过就不再是"整片铺满"（否则下一帧又被拉回 0）。
                // `max_view == 0` 时没有可滚的余量（整片都在眼前），此时**不能**退出
                // 铺满模式，否则窗口改宽时不再自动铺满。
                if max_view > 0.0 {
                    self.fit_mode = false;
                }
            }
        }
        // **点一下 = 把可见区间挪到以点击处为中心**。
        // `!drag_stopped()` 是必需的：一次"按住拖动"在松手那一帧也可能被 egui 记成 click，
        // 那样会把刚拖到的位置又跳成"以松手处为中心"，观感就是"一松手就跳"。
        if pos_resp.clicked() && !pos_resp.drag_stopped() {
            if let Some(p) = pos_resp.interact_pointer_pos() {
                let t = pos_row_t_of(p.x, pos_left, pos_w, duration);
                // 整片铺满时没有可滚的余量 → 点了也不动（此时全片本来就在眼前）
                if max_view > 0.0 {
                    vs = centered_view_start(t, visible_ms, duration as f64);
                    self.view_start_ms = vs;
                    self.fit_mode = false;
                }
            }
        }
        // 悬停/拖动时改成手型光标：这一行是可拖的（时间线本体不是）
        if pos_resp.hovered() || pos_resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        // ---- 绘制 ----
        {
            let painter = ui.painter();
            // 时间线底板：白底 + 发丝描边（浅色主题下不再是一块深灰）
            let bg_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() - 1.0, rect.top() - 1.0),
                egui::pos2(rect.right() + 1.0, rect.bottom() + 1.0),
            );
            painter.rect_filled(bg_rect, theme::radius(), theme::card_fill());
            painter.rect_stroke(
                bg_rect,
                theme::radius(),
                theme::hairline(),
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(rect, 6.0, theme::card_fill());

            let track_top = rect.top() + THUMB_BAND_H + THUMB_BAND_GAP;
            // **P14**：轨道底端 = 刻度尺带的顶端；刻度尺带之上是标记行，之下是**全片位置行**。
            // 三者的顺序（自上而下）：胶片条 → 标记行（波形）→ 刻度尺带 → 全片位置行。
            let ruler_top = rect.bottom() - POS_ROW_H - RULER_BAND_H;
            let track_bottom = ruler_top;
            let track = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 2.0, track_top),
                egui::pos2(rect.right() - 2.0, track_bottom),
            );
            painter.rect_filled(track, 6.0, theme::track_bg());
            // 0 电平基线：有波形时它是"静音线"，看得见波形的上下对称关系
            painter.line_segment(
                [
                    egui::pos2(track.left(), track.center().y),
                    egui::pos2(track.right(), track.center().y),
                ],
                egui::Stroke::new(1.0, theme::line()),
            );
            // ---- 片尾之后：**明确画成"无内容"**（口径 a）----
            //
            // 现象（用户原话）：「视频后面怎么多出来一块黑色块」——胶片条到片尾为止，
            // 其后到视口右边缘什么都不画，深色底板就成了"看起来还有一段黑视频"。
            // 口径：片尾之后不再铺任何"内容底色"，改用**页面底色 + 极淡的斜纹**表示
            // "这里没有内容"，并在片尾画一条**明确的终止线**。
            // 视口右端本身已经夹在 `[0, duration]` 内（`max_view`），所以这一段只在
            // "缩放到片尾附近"时可见 —— 它存在的意义就是不让那一段像黑画面。
            let end_x = x_of(duration as f64, px, vs);
            // 斜纹只画在**轨道那一带**（刻度尺那一带保持干净，时间码要看得清）：
            // 斜纹的跨度就等于轨道高度，所以线是 45°、间距固定，不随窗口大小变形。
            let past = egui::Rect::from_min_max(
                egui::pos2(end_x.max(track.left()), track.top()),
                egui::pos2(track.right(), track.bottom()),
            );
            if past.is_positive() {
                painter.rect_filled(past, 6.0, theme::bg());
                // **片尾分界线**（P10）：斜纹只在"真的在视口里看到片尾之后"时才画，
                // 所以必须有一条**贯穿整个时间线高度**的线把"有内容 / 没内容"分开 ——
                // 只有底色差异时，用户仍然分不清"这里是暗一点的底色"还是"这里还有内容"。
                // 从**胶片条顶部**一路贯穿到轨道底部：把"片子从哪儿结束"一次说清
                //（胶片条那一带也是"内容"，用户看的正是它什么时候到头）。
                painter.line_segment(
                    [
                        egui::pos2(past.left(), rect.top()),
                        egui::pos2(past.left(), track.bottom()),
                    ],
                    egui::Stroke::new(1.0, theme::ruler_tick()),
                );
                let span = past.height();
                let mut x = past.left() - span;
                while x < past.right() {
                    let x0 = x.max(past.left());
                    let x1 = (x + span).min(past.right());
                    if x0 < x1 {
                        // 45° 斜线（egui 没有 hatch 填充，直接用线段画）
                        painter.line_segment(
                            [egui::pos2(x0, past.bottom()), egui::pos2(x1, past.top())],
                            egui::Stroke::new(1.0, theme::empty_hatch()),
                        );
                    }
                    x += EMPTY_HATCH_STEP;
                }
                // 片尾终止线：把"内容到此为止"说清楚
                if end_x >= rect.left() && end_x <= rect.right() {
                    painter.line_segment(
                        [
                            egui::pos2(end_x, rect.top()),
                            egui::pos2(end_x, rect.bottom()),
                        ],
                        egui::Stroke::new(1.0, theme::ruler_tick()),
                    );
                }
            }
            #[cfg(test)]
            {
                // 刻度尺区域 = 轨道下方那条带（时间码就画在这里）；
                // 无窗口单测要分别点这两处，证明"点刻度尺和点轨道都能设置当前标尺"。
                //
                // **P14**：它的底端是**全片位置行的顶端**（不是时间线矩形的底端）——
                // 位置行是独立的一整行、有它自己的交互语义，不能算进"刻度尺区域"，
                // 否则这个矩形会与位置行重叠、单测点出来的归属就分不清了。
                self.test_rects.track = Some(track);
                self.test_rects.ruler = Some(egui::Rect::from_min_max(
                    egui::pos2(rect.left(), track.bottom()),
                    egui::pos2(rect.right(), pos_rect.top()),
                ));
            }

            // ---- 选区高亮：**先于波形画** ----
            //
            // 顺序很重要（本轮修的一个真问题）：`起点→终点` 那一片半透明蓝是**底色**，
            // 波形是**内容**。第一版把波形画在它下面，于是 5 秒全选的素材上，
            // 波形被那层 15% 的蓝压得几乎看不见 —— 内容必须画在底色之上。
            //
            // **多段（P36 改色）**：所有片段都用**蓝色系**（与芯片的选中色同源），
            // 只是浓度分两档 —— 非当前段 34、当前段 86。
            // 改造前非当前段是灰色（`seg_inactive_fill()`），用户反馈"很不醒目"：
            // 灰带在浅灰轨道上几乎看不出来，而芯片里选中的明明是蓝色 —— 两处不一致。
            //
            // **P39 再改（用户要求）**：**当前段**的高亮从蓝改**桔**（`seg_active_fill()`）——
            // 影片条上还有绿=起点 / 红=终点 / 蓝=当前标尺三根线，选中片段再一片蓝
            // 会和当前标尺混。非当前段仍是蓝色系（它们不是"被选中的那一段"），
            // 于是"桔 = 正在编辑的片段、淡蓝 = 其余也要导出的片段"分工明确。
            for (i, &(s, e)) in seg_effs.iter().enumerate() {
                if self.active_seg == Some(i) {
                    continue;
                }
                let x0 = x_of(s as f64, px, vs);
                let x1 = x_of(e as f64, px, vs);
                let r = egui::Rect::from_min_max(
                    egui::pos2(x0.max(rect.left()), track.top()),
                    egui::pos2(x1.min(rect.right()), track.bottom()),
                );
                if r.is_positive() {
                    painter.rect_filled(r, 4.0, seg_other_fill());
                }
            }
            let sx = x_of(eff_s as f64, px, vs);
            let ex = x_of(eff_e as f64, px, vs);
            let sel_rect = egui::Rect::from_min_max(
                egui::pos2(sx.max(rect.left()), track.top()),
                egui::pos2(ex.min(rect.right()), track.bottom()),
            );
            if sel_rect.is_positive() {
                painter.rect_filled(sel_rect, 4.0, seg_active_fill());
            }

            // ---- **音频波形**：画在标记行（轨道）里 ----
            //
            // 三个硬要求都在这一段的参数里：
            // ① **与时间轴同源**：横坐标只用 `x_of(t, px, vs)` —— 与胶片条、刻度尺、
            //    绿/红/蓝三根标尺**同一个闭包、同一份 (px, vs)**，所以缩放/平移后严格对齐；
            // ② **只画可见区间**：段由 `segments_for_view` 按**可见区间**算出来
            //    （可见 <= 120 s：`VISIBLE_BUCKETS` = 600 驱动的档位；
            //     更长：100 ms 一片的**全片密集基准层**，P22 起不再是稀疏填充）；
            // ③ **渐进**：只画已经有数据的切片，缺口处给一条淡指示线（**不留空白**），
            //    后台 worker 一有产出就从那边唤醒重绘（数据驱动，不是心跳）。
            let wave_avail = self.wave.availability();
            // `snapshot()` 返回 `Arc` 克隆（**不是**克隆整层），所以这里的借用不牵扯 `self.wave`
            let wave_ui_t0 = std::time::Instant::now();
            let (snap_owned, tiles_owned) = match wave_avail {
                Availability::Unavailable => (None, HashMap::new()),
                _ => (self.wave.snapshot(), self.wave.tiles_map()),
            };
            let snap_cost = wave_ui_t0.elapsed();
            // 本帧的细化请求（绘制之后才提交，见下面的 `pump_refine`）
            let mut refine_want: Option<(i64, i64)> = None;
            // ---- **可见区间最优先：每帧无条件发布**（P22 / 用户口径）----
            //
            // 为什么必须在这里、且**不看 `snap_owned`**：
            // * 基准层还没出第一份快照时界面什么都画不出来，但那正是 4 条 worker
            //   最需要知道"用户在看哪一段"的时刻 —— 早一帧发布，它们就少白算一段；
            // * 更不能只在 `pump_refine` 里发布：那条路只有 `refine_ms` 有值才走，
            //   而**长视口（含整片铺满）的 `refine_ms` 恒为 `None`** —— 于是"整片视图下
            //   4 条 worker 按 0/1/2/3 顺序算"这个老毛病就会一直存在。
            // 代价是两次 relaxed store，可以每帧做。
            let (t0, t1) = visible_time_range(rect, px, vs);
            self.wave
                .publish_visible(t0.round() as i64, t1.round() as i64);
            if self.show_waveform {
                if let Some(base) = snap_owned.as_ref() {
                    // **分辨率不再看像素**：可见时长决定用哪一层；
                    // `target_cols` = 时间线像素宽，长视口下把基准层切片**合并到像素**
                    // （1 小时整片视图从 36,000 个 shape 降到 ≈ 1,000 个，绘制时间可控）。
                    let cols = (rect.width().ceil() as usize).clamp(1, 4096);
                    let segs = edit::waveform::segments_for_view(base, &tiles_owned, t0, t1, cols);
                    let drawn =
                        paint_timeline_wave(painter, track, &segs.segs, segs.pending_ms, |t| {
                            x_of(t, px, vs)
                        });
                    // 非测试构建也要"读"一下结果，否则这个结构体就是 dead_code。
                    let _ = drawn.drew_anything();
                    // **首屏可见耗时**（P22 验收要贴的那个数字）：第一帧真的画出波形段的时刻
                    if self.wave.first_data_at.is_none() && drawn.drew_anything() {
                        self.wave.first_data_at = self.wave.opened_at.map(|t| t.elapsed());
                    }
                    self.wave_ui_log(wave_ui_t0, snap_cost, &segs, drawn);
                    refine_want = segs.refine_ms;
                    #[cfg(test)]
                    {
                        self.test_rects.wave_layer = Some(segs.layer);
                        self.test_rects.wave_detail = Some(matches!(segs.layer, ViewLayer::Dense));
                        self.test_rects.wave_segments = Some(drawn.segments);
                        self.test_rects.wave_first_x = Some(drawn.first_x);
                        self.test_rects.wave_last_x = Some(drawn.last_x);
                        self.test_rects.wave_refine_ms = Some(segs.refine_ms);
                        self.test_rects.wave_tier_ms = Some(segs.bucket_ms);
                    }
                }
            }
            // ---- **P35：编码错误坏段带** ----
            //
            // 画在波形**之后**（盖在上面）、贴在标记行底部，好处有三个：
            // ① 与波形/刻度/三根标尺**共用同一个 `x_of`**，缩放平移天然对齐；
            // ② 只占底部 5 px，不挡波形主体（波形是"内容"，坏段是"批注"）；
            // ③ **不受 `show_waveform` 影响** —— 用户关掉波形之后仍然要看得到坏段
            //    （那正是最需要提示的时候）。
            //
            // 没有坏段时一次 `rect_filled` 都不会发生（`bad` 为空直接返回），
            // 所以干净素材的绘制开销与改造前完全相同。
            // 坏段的"是什么错、多长"由底部文件信息行那句话负责（悬停有明细），
            // 这里只负责"在时间轴上看得见"。
            //
            // 早退判据（干净素材零开销）：没有快速体检的坏段、也没做过深度检查 → 连合并都不做。
            let has_bad = self
                .clip
                .as_ref()
                .map(|c| !c.health.bad.is_empty())
                .unwrap_or(false);
            if has_bad {
                let health = self.health_report();
                if !health.bad.is_empty() {
                    paint_bad_ranges(painter, track, &health.bad, |t| x_of(t, px, vs));
                }
            }
            // 画完才知道"要看哪一段" → 现在按需请求细化（换文件/关开关时会随任务一起取消）
            if let Some((a, b)) = refine_want {
                let center = ((a + b) / 2).clamp(0, duration);
                self.wave.pump_refine(Some((a, b)), center);
            }
            // 注意：这里**没有**"还在跑就再请求一帧"的兜底（P15 之前有）——
            // 那种写法会让 L0 在算的整段时间里每帧都排下一帧，等于持续满帧重绘。
            // 现在 L0 每发一档进度、细化每产出一块瓦片都会从后台线程
            // `request_repaint()` 一次（数据驱动，见 `wake_ui`）。

            // ---- **胶片条（P19/P21/P24/P25）**：完整显示 + 可见区间里固定 5 个（放不下才减） ----
            //
            // P19 的旧实现按 1 秒网格取样 + 按 `x_of(网格时刻)` 落位，于是：
            // 与可见区间左右边缘**不对齐**、平移时块**跳变**、块数忽多忽少 —— 用户口径是
            // "完全乱了"。P21 改成"取样时刻由可见区间直接算出 + 贴边落位"，P24 把高定成行高，
            // **P25 再改两件事**（用户原话「5 个画面在条中均匀分布，1 在最左，5 在最右」
            // +「高度上只显示了画面一部分」）：
            // * **画面按行高等比缩放、完整显示（contain）**：宽 = `行高 × 源宽高比`
            //   （16:9 ≈ 92 px；竖屏 1080×1920 ≈ 29 px），**不裁切、不变形**
            //   —— 取代 P24 的"160 宽槽位 + cover 裁剪"；
            // * **落位 = 首贴左 + 末贴右 + 相邻间隙相等**（[`strip_layout`]）：
            //   `gap = (条可用宽 − Σ画面宽)/(n−1)`，**不再受 160 槽位约束**；
            //   布局的锚点仍是与波形/刻度**同源**的 `x_of(t_0)`；
            // * **取样时刻一字未改**：`t_k = 可见起点 + k × 可见时长/(n−1)`；
            // * **放不下就减块数**：`n = `[`strip_slot_count`]（5→4→3→2→1），画面永不缩；
            // * **解码键量化到 [`STRIP_QUANT_MS`]**（+ 高度档 + 口径版本）：来回缩放立刻命中
            //   缓存，**不重复解码**；
            // * 没有"逐帧兜底/批线程"：只有"单张在途 + 已缓存的跳过 + 只解最靠左没解的那个"。
            {
                // **P24：画面高 = 胶片条行高**（唯一来源见 [`strip_tile_height`]）；
                // **P25：画面宽 = 行高 × 源宽高比**（contain，[`strip_picture_width`]）
                let tile_h = strip_tile_height();
                let pic_w = strip_picture_width(tile_h, self.source_aspect());
                let n = strip_slot_count(rect.width(), pic_w);
                // **P27 ①**：取样与"片尾留白"一律按**视频流时长**算（不用容器时长）——
                // 容器比视频流长时（尾段只有音频/帧没写完），旧口径的最后一块必然取样失败。
                let span_end = self.p27_span_end_ms();
                let samples = strip_samples(vs, vs + visible_ms, span_end, n);
                // 查表（键带高度与口径版本）；返回值 = 本帧真的查到画面的块数，
                // 它就是 P23 判据三「胶片条首块已显示」的依据。
                let strip_drawn = self.refresh_strip(&samples, span_end, tile_h);
                // **P25 布局**：锚点 = `x_of(t_0)`（与波形/刻度**同一个** `x_of`），
                // 右边界 = 条右边界；等宽时它等价于 `left[k] = x_of(t_k) − frac_k × 画面宽`。
                let left0 = match samples.first() {
                    Some((t, _)) => x_of(*t as f64, px, vs),
                    None => rect.left(),
                };
                let widths = vec![pic_w; samples.len()];
                let slots = strip_layout(&samples, &widths, tile_h, left0, rect.right());
                let rect_of = |s: &StripSlot| {
                    egui::Rect::from_min_max(
                        egui::pos2(s.left, rect.top()),
                        egui::pos2(s.left + s.width, rect.top() + s.height),
                    )
                };
                let drawn: Vec<egui::Rect> = self
                    .strip_prev
                    .iter()
                    .filter_map(|t| {
                        let slot = slots.get(t.idx)?;
                        let full = rect_of(slot);
                        let clipped = full.intersect(rect);
                        if !(clipped.is_positive() && full.width() > 0.5 && full.height() > 0.5) {
                            return None;
                        }
                        // P25：整幅画面（contain）→ uv 就是整张纹理，只有"被条裁掉的部分"折算
                        let uv = strip_contain_uv(full, clipped);
                        painter.image(t.tex.id(), clipped, uv, egui::Color32::WHITE);
                        Some(full)
                    })
                    .collect();
                // 还没解码的取样时刻：画一个**同样尺寸、同样位置**的占位块（只是没有画面），
                // 这样"画面尺寸与位置与缩放无关"这条在数据没到时也成立，不会出现空窗跳动。
                // 占位块与画面严格同矩形：素材到位时是"换成画面"，不会有一次跳动。
                let have: std::collections::HashSet<(i64, usize)> =
                    self.strip_prev.iter().map(|t| (t.at_ms, t.idx)).collect();
                for (k, slot) in slots.iter().enumerate() {
                    if have.contains(&(slot.at_ms, k)) {
                        continue;
                    }
                    let ph = rect_of(slot).intersect(rect);
                    if ph.is_positive() {
                        painter.rect_filled(
                            ph,
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(0x40, 0x46, 0x50, 0x60),
                        );
                    }
                }
                // 按当前可见区间排队解码（"单张在途 + 已缓存的跳过 + 时刻/高度量化"）
                self.pump_strip(&samples, ui.ctx());
                // 非测试构建也要"读"一下，否则这份几何就是 dead_code
                let _ = (&drawn, &slots, pic_w, n, tile_h);
                // `strip_drawn` = 本帧胶片条里**真的有画面**的块数（`drawn.len()` 是同一件事的
                // 绘制侧读数）：P23 的「胶片条首块已显示」判据下一次读的就是它。
                let _ = strip_drawn;
                #[cfg(test)]
                {
                    self.test_rects.strip_tiles = Some(drawn);
                    self.test_rects.strip_grid = Some(samples.iter().map(|(t, _)| *t).collect());
                    self.test_rects.strip_anchors = Some(samples.iter().map(|(_, f)| *f).collect());
                    self.test_rects.strip_count = Some(n);
                    self.test_rects.strip_pic_w = Some(pic_w);
                }
            }

            // 关键帧小刻度：一眼看出能吸附到哪些点。
            // **按像素密度抽稀**：1 小时录像按 1 秒 GOP 就是 3600 个关键帧，
            // 全画出来既糊成一片又白费绘制时间；抽稀后最多约 width/3 条。
            let view_end = vs + visible_ms;
            let ticks = self
                .clip
                .as_ref()
                .map(|c| {
                    visible_keyframe_ticks(&c.keyframes_ms, px, vs, view_end, KEYFRAME_MIN_GAP_PX)
                })
                .unwrap_or_default();
            for k in &ticks {
                let x = x_of(*k as f64, px, vs);
                painter.line_segment(
                    [
                        egui::pos2(x, track.bottom() - 8.0),
                        egui::pos2(x, track.bottom()),
                    ],
                    // 淡一档的金色：和"当前标尺"区分开，别让用户以为到处都是标尺
                    egui::Stroke::new(1.5, theme::keyframe()),
                );
            }
            // 预览位置最近的那个关键帧单独高亮（抽稀可能把它滤掉，所以另画一遍）
            for k in &near {
                let x = x_of(*k as f64, px, vs);
                if x < rect.left() - 2.0 || x > rect.right() + 2.0 {
                    continue;
                }
                painter.line_segment(
                    [egui::pos2(x, track.top()), egui::pos2(x, track.bottom())],
                    egui::Stroke::new(1.5, theme::accent()),
                );
                painter.circle_filled(egui::pos2(x, track.top() + 3.0), 2.5, theme::accent());
            }

            // 时间刻度尺：按像素密度选一个不重叠的"整"步长
            //
            // 时间码文字**必须完整落在时间线矩形内**（最小尺寸下"文字不被裁切"是硬要求）：
            // 默认左对齐画在刻度右侧；右边界放不下时改成**右对齐贴住右边界**；
            // 若那样会和上一个时间码重叠，就**不画这一格的时间码**（刻度线仍然画）——
            // 宁可少一个末刻度文字，也不要一个被切掉一半的数字。
            let mut last_label_right = f32::NEG_INFINITY;
            for t in ruler_ticks(vs, view_end, duration, px, RULER_MIN_GAP_PX) {
                let x = x_of(t, px, vs);
                painter.line_segment(
                    [
                        egui::pos2(x, track.bottom()),
                        egui::pos2(x, track.bottom() + 4.0),
                    ],
                    egui::Stroke::new(1.0, theme::ruler_tick()),
                );
                let galley = painter.layout_no_wrap(
                    fmt_time(t as i64),
                    egui::FontId::monospace(10.0),
                    theme::ruler_text(),
                );
                let w = galley.size().x;
                let mut lx = x + 2.0;
                if lx + w > rect.right() {
                    lx = rect.right() - w - 2.0;
                }
                if lx >= rect.left() && lx >= last_label_right + 6.0 {
                    painter.galley(
                        egui::pos2(lx, track.bottom() + 4.0),
                        galley,
                        theme::ruler_text(),
                    );
                    last_label_right = lx + w;
                }
            }

            // ---- **每段标序号**（P36）：与片段列表里的序号一一对应 ----
            //
            // 用户要求"影片条上显示每段所在影片中的位置"。位置本身由那条浅色带表达
            // （画在它真实的时间位置上），序号则把它和列表里的第 N 行绑起来 ——
            // 于是"列表说第 3 段是 00:21–00:26"和"影片条上标 3 的那一截"能直接对上，
            // 即使列表顺序（播放顺序）与时间顺序不同也不会看错。
            //
            // 只在多段时画（单段时那个"1"没有信息量，反而挡波形）。
            if seg_effs.len() > 1 {
                for (i, &(s, e)) in seg_effs.iter().enumerate() {
                    let x0 = x_of(s as f64, px, vs).max(rect.left());
                    let x1 = x_of(e as f64, px, vs).min(rect.right());
                    // 窄于 14 px 就放不下数字了：宁可空着，也不要糊成一团
                    if x1 - x0 < 14.0 {
                        continue;
                    }
                    painter.text(
                        egui::pos2((x0 + x1) / 2.0, track.bottom() - 2.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{}", i + 1),
                        egui::FontId::proportional(11.0),
                        if self.active_seg == Some(i) {
                            egui::Color32::WHITE
                        } else {
                            seg_number_color()
                        },
                    );
                }
            }

            // ---- 非当前分段的两端：细线 + 小圆点（把手，可拖 —— 见命中测试）----
            // 画在波形与刻度**之上**，保证缩得多小都看得见段的边界；
            // 颜色比当前段的绿/红暗一档，避免"到处都是主标尺"的错觉。
            for (i, &(s, e)) in seg_effs.iter().enumerate() {
                if self.active_seg == Some(i) {
                    continue;
                }
                for t in [s, e] {
                    let x = x_of(t as f64, px, vs);
                    if x < rect.left() - 1.0 || x > rect.right() + 1.0 {
                        continue;
                    }
                    painter.line_segment(
                        [
                            egui::pos2(x, rect.top() + 15.0),
                            egui::pos2(x, track.bottom()),
                        ],
                        egui::Stroke::new(1.5, seg_inactive_edge()),
                    );
                    painter.circle_filled(
                        egui::pos2(x, track.top() + 2.0),
                        3.0,
                        seg_inactive_edge(),
                    );
                }
            }

            // ---- 绿色起点标尺 / 红色终点标尺（带可拖拽把手 + 小标签）----
            for (x, color, label) in [
                (sx, start_color(), i18n.t("start_point")),
                (ex, end_color(), i18n.t("end_point")),
            ] {
                if x >= rect.left() - 1.0 && x <= rect.right() + 1.0 {
                    painter.line_segment(
                        [
                            egui::pos2(x, rect.top() + 15.0),
                            egui::pos2(x, track.bottom()),
                        ],
                        egui::Stroke::new(2.0, color),
                    );
                    // 把手画在轨道顶端：既能看清是哪根标尺，也不挡时间尺刻度
                    painter.circle_filled(egui::pos2(x, track.top() + 2.0), 4.5, color);
                    let lx = (x + 4.0).clamp(rect.left(), (rect.right() - 30.0).max(rect.left()));
                    painter.text(
                        egui::pos2(lx, track.top() + 4.0),
                        egui::Align2::LEFT_TOP,
                        label,
                        egui::FontId::proportional(10.0),
                        color,
                    );
                }
            }

            // ---- **蓝色当前标尺**（默认停在整片中间；点击/拖把手/键盘才会移动它）----
            let cx = x_of(self.cursor_ms as f64, px, vs);
            if cx >= rect.left() - 1.0 && cx <= rect.right() + 1.0 {
                painter.line_segment(
                    [
                        egui::pos2(cx, rect.top() + 15.0),
                        egui::pos2(cx, track.bottom() + 8.0),
                    ],
                    egui::Stroke::new(2.0, cursor_color()),
                );
                // 可拖拽把手；`test_rects.cursor_handle` 把它的矩形留给无窗口单测
                let handle = egui::Rect::from_center_size(
                    egui::pos2(cx, track.top() + 2.0),
                    egui::vec2(CURSOR_HANDLE_R * 2.0, CURSOR_HANDLE_R * 2.0),
                );
                painter.circle_filled(handle.center(), CURSOR_HANDLE_R, cursor_color());
                #[cfg(test)]
                {
                    self.test_rects.cursor_handle = Some(handle);
                }
                // 时间码**不在时间线上显示**（用户要求：鼠标滑动时不弹任何文字，
                // 静态的标尺时间码只在「当前标尺」那一行出现一次），所以这里不画浮标。
            }
        }

        // ---- P14/P17 · 画**全片位置行**（在时间线底板上，位于时间码行下方）----
        //
        // 两样东西，**都走全片映射**（`pos_row_x_of`，见它的文档）：
        // ① 整条**全蓝轨道** = **全片**（语义核心：蓝条的长度就是片子总长）；
        // ② 淡蓝色带 = **当前可见区间在全片里的位置**（缩小/平移时它是蓝条上的一小段）。
        //
        // **P17：小方块已删除** —— 那个"当前帧位置方块"与 ② 的淡蓝色带功能重复
        //（用户原话："浅蓝色滑动条已经完成了我想要的小滑块的功能"）。
        // 整行仍是**可点可拖**的控件，但 P30 起它**只是滚动条**（拖 = 滚视口、点 = 跳到那儿），
        // 不再定位当前帧 —— 见本节末尾 `pos_resp` 的处理。
        //
        // 为什么不用时间线那套 `x_of`：`x_of` 是**视口**映射，用它画出来的色带恒等于整条
        // （`x_of(vs)` 恒在左端、`x_of(vs+可见时长)` 恒在右端）—— 那正是用户最早说的
        // "蓝色看起来像目前显示的进度"这个歧义的来源。全片映射下蓝条是静态的全片概览，
        // 色带才是"我正看着哪一段"。
        {
            let painter = ui.painter();
            let mid_y = pos_rect.center().y;
            let pos_left = pos_rect.left() as f64;
            let pos_w = pos_rect.width() as f64;
            let track = egui::Rect::from_min_max(
                egui::pos2(pos_rect.left(), mid_y - POS_TRACK_H / 2.0),
                egui::pos2(pos_rect.right(), mid_y + POS_TRACK_H / 2.0),
            );
            // ① 全片轨道：**整条**铺满（不按 `vs`/`visible_ms` 裁）—— 这就是"全片"的表达
            painter.rect_filled(track, POS_TRACK_H / 2.0, pos_track_color());
            // ② 当前可见区间色带：把 [vs, vs+visible_ms] 夹到 [0, duration] 再按全片比例落位
            let band_x0 = pos_row_x_of(vs.max(0.0), pos_left, pos_w, duration);
            let band_x1 = pos_row_x_of(
                (vs + visible_ms).min(duration as f64),
                pos_left,
                pos_w,
                duration,
            );
            if band_x1 > band_x0 + 0.5 {
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(band_x0, track.top()),
                        egui::pos2(band_x1, track.bottom()),
                    ),
                    2.0,
                    pos_band_color(),
                );
            }
        }

        if self
            .clip
            .as_ref()
            .map(|c| c.keyframes_ms.is_empty())
            .unwrap_or(false)
        {
            ui.colored_label(
                egui::Color32::from_rgb(230, 180, 80),
                i18n.t("no_keyframes_hint"),
            );
        }
    }

    /// **离预览位置最近的那一个关键帧**的缩略图（诉求 1：只要一张，不是一排）。
    ///
    /// 看哪个时间点（[`EditorPage::preview_ms`]）：**只看蓝色当前标尺**
    /// （点击时间线 / 拖蓝色把手 / 键盘微调会移动它），悬停与拖动**都不驱动**它。
    ///
    /// 与轨道上的整体缩略图条分工：整体条管"导航"（永远在），这里管"看清这一个点长什么样"。
    /// 解码策略：**(时间点, 尺寸档)** 只解码一次并缓存，同一时刻只允许一张在途，
    /// 连续拖动只解最近那一张（尺寸档按 [`THUMB_BUCKET_STEP`] 量化，拖 1px 不会重解）。
    ///
    /// **画框矩形由调用方算好**（[`EditorPage::preview_size`] 按 [`fluid_preview_size`] 的规则；
    /// 比例恒等于源比例）：预览是流式的 —— 窗口越大画框越大，解码档位跟着变大，
    /// 但档位没跨就不重解。画框下面还有一行时间码（高度 `caption_h`，已含间距）。
    fn paint_preview(
        &mut self,
        ui: &mut egui::Ui,
        frame: egui::Rect,
        caption_h: f32,
        i18n: &crate::i18n::I18n,
    ) {
        // **看当前标尺**（`cursor_ms`）最近的关键帧，不再跟随鼠标悬停：
        // 鼠标滑动时页面不该有任何内容在跳（用户要求）。
        let at = self.cursor_clamped();
        // 记下本帧画框宽度：播放的解码尺寸跟着它走（见 `play_decode_size`）
        self.preview_frame_w = frame.width();
        // **播放中**：画面来自播放线程（帧级、按 pts 对齐时钟），
        // 不再走"最近关键帧"那条缩略图路径 —— 否则画面只会在关键帧上跳变。
        if self.play.is_some() {
            self.paint_playback(ui, frame, caption_h);
            return;
        }
        // 目标解码档位：按 64px 向上取整（**尺寸进缓存 key**），拖动窗口不会反复重解
        let bucket = thumb_bucket_w(frame.width(), self.source_width());
        #[cfg(test)]
        {
            self.test_rects.preview_bucket = Some(bucket);
        }
        // 先把这一张排进解码队列（后台线程），下面只负责显示缓存里已有的
        self.request_thumb(at, bucket, ui.ctx());

        let kf = self.clip.as_ref().and_then(|c| thumb_keyframe(c, at));
        // 只取缓存（只读借用）：解码全在上面的后台线程里，这里绝不做解码。
        // 优先精确档位；还没解好就用**同一关键帧的其它档位**先顶着（换窗口大小时不闪"解码中"）。
        // 取纹理 id 与像素尺寸的**拷贝**（不长期借住 `self`）。
        let cached: Option<(egui::TextureId, u32, u32)> = kf
            .and_then(|k| self.cached_thumb(k, bucket).0)
            .map(|c| (c.tex.id(), c.w, c.h));

        let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        // **P23 判据二**：先用一个局部量记下"这一帧真的贴了一张已解码的纹理"，
        // 等 `painter` 那次借用结束再写回字段（`self` 在那里还被借着，不能就地写）。
        let mut painted_frame = false;
        let painter = ui.painter();
        // 黑底只是兜底（解码中/极端比例时不会露出奇怪的底色）；画框比例与源一致，
        // 正常情况下看不到留边。
        painter.rect_filled(frame, 4.0, egui::Color32::BLACK);
        match cached {
            Some((tex, w, h)) => {
                // 再走一次"等比放进画框并居中"：画框比例与源一致时结果就是画框本身，
                // 但取整/异常比例下也不会拉伸（这条路径由 `fit_thumb_size` 的单测保证）。
                let fit = fit_thumb_size(w, h, frame.width(), frame.height());
                let img = egui::Rect::from_center_size(frame.center(), fit);
                ui.painter().image(tex, img, full_uv, egui::Color32::WHITE);
                // **P23 判据二**：首帧预览**已经渲染出一帧**（真的贴了一张已解码的纹理，
                // 不是"数据在缓存里"、更不是下面那句"解码中…"的占位文字）。
                painted_frame = true;
            }
            None => {
                let text = if kf.is_some() {
                    i18n.t("preview_decoding").to_string()
                } else {
                    i18n.t("preview_no_keyframes").to_string()
                };
                ui.painter().text(
                    frame.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_gray(180),
                );
            }
        }
        #[cfg(test)]
        {
            self.test_rects.preview_frame = Some(frame);
        }
        // 上面那次 `painter` 借用到此结束 → 现在可以写回"这一帧预览出了画面"这件事
        if painted_frame {
            self.preview_painted = true;
        }
        // 画框正下方那一行时间码（高度已包含在 `caption_h` 里，不会被裁掉）
        if let Some(kf) = kf {
            let caption = egui::Rect::from_min_size(
                egui::pos2(frame.left(), frame.bottom() + PREVIEW_CAPTION_GAP),
                egui::vec2(frame.width(), (caption_h - PREVIEW_CAPTION_GAP).max(0.0)),
            );
            ui.put(
                caption,
                egui::Label::new(
                    egui::RichText::new(fmt_time(kf))
                        .monospace()
                        .size(12.0)
                        .color(crate::ui::theme::cursor()),
                ),
            );
        }
    }

    /// **播放中的预览**：把播放线程解出来的那一帧贴进画框。
    ///
    /// 与 [`EditorPage::paint_preview`] 的差别只有一个：**帧源**不同
    /// （那边是"最近关键帧"的缩略图缓存，这里是按 pts 对齐播放位置的真实帧）。
    /// 画框、比例、下方时间码那行、`preview_painted` 的语义**全部保持一致**。
    ///
    /// 还没解出第一帧时如实画"解码中…"（绝不假装有画面，也不闪黑）。
    fn paint_playback(&mut self, ui: &mut egui::Ui, frame: egui::Rect, caption_h: f32) {
        let (tex_id, shown_ms) = match &self.play {
            Some(p) => (p.tex.as_ref().map(|t| t.id()), p.shown_ms),
            None => return,
        };
        let full_uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let mut painted = false;
        {
            let painter = ui.painter();
            painter.rect_filled(frame, 4.0, egui::Color32::BLACK);
            match tex_id {
                Some(id) => {
                    // 解码尺寸本来就是按画框宽算出来的（同一宽高比），直接铺满画框
                    painter.image(id, frame, full_uv, egui::Color32::WHITE);
                    painted = true;
                }
                None => {
                    painter.text(
                        frame.center(),
                        egui::Align2::CENTER_CENTER,
                        "…",
                        egui::FontId::proportional(14.0),
                        egui::Color32::from_gray(180),
                    );
                }
            }
        }
        if painted {
            self.preview_painted = true;
        }
        // 画框正下方那行时间码：播放中显示**这一帧**的时刻（与右列那个"播到哪儿"互补）
        let caption = egui::Rect::from_min_size(
            egui::pos2(frame.left(), frame.bottom() + PREVIEW_CAPTION_GAP),
            egui::vec2(frame.width(), (caption_h - PREVIEW_CAPTION_GAP).max(0.0)),
        );
        ui.put(
            caption,
            egui::Label::new(
                egui::RichText::new(fmt_time(shown_ms))
                    .monospace()
                    .size(12.0)
                    .color(crate::ui::theme::cursor()),
            ),
        );
    }

    /// 源宽高比（**严格按源尺寸**）：`源宽 / 源高`；源尺寸未知（没有视频流 / 宽高为 0）
    /// 时按 16:9 兜底。预览画框与解码档位都用它，保证"不拉伸、不裁剪、不留黑边"。
    fn source_aspect(&self) -> f32 {
        let (w, h) = self.source_width_height();
        let a = if w > 0 && h > 0 {
            w as f32 / h as f32
        } else {
            PREVIEW_FALLBACK_ASPECT
        };
        if a.is_finite() && a > 0.05 {
            a
        } else {
            PREVIEW_FALLBACK_ASPECT
        }
    }

    /// 源尺寸（像素）；未知时 `(0, 0)`（`StreamInfo` 里宽高是 `i32`，这里统一取正数）。
    fn source_width_height(&self) -> (u32, u32) {
        self.clip
            .as_ref()
            .and_then(|c| c.video.as_ref())
            .map(|v| (v.width.max(0) as u32, v.height.max(0) as u32))
            .unwrap_or((0, 0))
    }

    /// 源宽（像素）；未知时 0（调用方用它把解码档位夹到"不超过源宽"）。
    fn source_width(&self) -> u32 {
        self.source_width_height().0
    }

    /// **本帧预览尺寸**：可用高度 / 可用宽度 → [`fluid_preview_size`]（规则都在那里）。
    ///
    /// 高度下限恒为 `PREVIEW_MIN_H`（200）—— 与改造前完全一致。
    fn preview_size(&self, avail_h: f32, max_w: f32) -> egui::Vec2 {
        fluid_preview_size(avail_h, max_w, self.source_aspect(), PREVIEW_MIN_H)
    }

    /// 按 **(关键帧时间点, 尺寸档)** 取缓存；精确档位没有时退回"同一关键帧的其它档位"
    /// （换窗口大小时先用旧尺寸顶一帧，不闪"解码中…"）。返回 `(缓存项, 是否精确命中)`。
    fn cached_thumb(&self, at_ms: i64, bucket_w: u32) -> (Option<&CachedThumb>, bool) {
        if let Some(t) = self.kf_thumbs.get(&(at_ms, bucket_w)) {
            return (Some(t), true);
        }
        let fallback = self
            .kf_thumbs
            .iter()
            .filter(|((t, _), _)| *t == at_ms)
            .map(|(_, c)| c)
            .max_by_key(|c| c.w);
        (fallback, false)
    }
}

/// **预览尺寸规则**（剪辑页"流式预览"的唯一口径，纯函数 → 可脱界面单测）。
///
/// 输入：`avail_h` = 预览可用的高度（调用方已经扣掉下方固定内容，见 [`preview_avail_h`]）、
/// `max_w` = 预览可用的宽度（让出导出按钮那一列之后的宽度）、`aspect` = 源宽高比。
///
/// 规则（严格按用户要求的优先级）：
/// 1. **以高度为准**：`h = max(avail_h, PREVIEW_MIN_H)`，`w = h × aspect`。
///    画框比例**恒等于**源比例，所以既不拉伸、也不裁剪、更不留黑边；
/// 2. **高度下限 200px**：可用高度不足时取 200（宁可因此出现滚动条，也不把画面压成一条），
///    所以返回值**永远**满足 `h ≥ 200`；
/// 3. **宽度受限时按宽度收敛**：`w > max_w` 时先试 `w = max_w`、`h = max_w / aspect`；
///    **只有收敛后的 h 仍然 ≥ 200 才接受**（这时预览不再吃满高度，但比例仍然严格成立）；
///    否则**保持高度下限**（`w = 200 × aspect`），允许横向裁切视口 / 横向滚动。
///
/// 尺寸取整到整数像素（避免半像素边缘发虚）；取整是单调的，
/// 所以"窗口更高 → `preview_h` 更大"这条性质仍然成立（单测 `fluid_preview_follows_window_height`）。
///
/// `floor` = 高度下限：单段模式传 [`PREVIEW_MIN_H`]（200），片段列表在场时传
/// [`PREVIEW_MIN_H_WITH_LIST`]（120）—— 见那个常量的说明。下限只在"可用高度不够"时
/// 起作用，所以两条路径在窗口宽裕时给出完全一样的结果。
fn fluid_preview_size(avail_h: f32, max_w: f32, aspect: f32, floor: f32) -> egui::Vec2 {
    let aspect = if aspect.is_finite() && aspect > 0.05 {
        aspect
    } else {
        PREVIEW_FALLBACK_ASPECT
    };
    let avail = if avail_h.is_finite() { avail_h } else { 0.0 };
    let floor = if floor.is_finite() && floor > 0.0 {
        floor
    } else {
        PREVIEW_MIN_H
    };
    // 规则 1 + 2：以高度为准，且不低于下限
    let h = avail.max(floor);
    let w = h * aspect;
    let maxw = if max_w.is_finite() && max_w > 0.0 {
        max_w
    } else {
        w
    };
    if w > maxw {
        // 规则 3：先按宽度收敛，只有收敛后的高度仍 ≥ 下限才认
        let h_if_fit = maxw / aspect;
        if h_if_fit >= floor {
            return egui::vec2(maxw.round().max(1.0), h_if_fit.round().max(1.0));
        }
        // 收敛会把高度压到下限以下 → 保高度下限（宽度由高度推出，允许横向裁切）
    }
    egui::vec2((h * aspect).round().max(1.0), h.round().max(1.0))
}

/// **预览可用高度** = 滚动区里"时间线以下"的剩余高度 − 预览下方必须留出的固定内容。
///
/// "下方固定内容"只有**预览自己那行时间码**（+间距）：文件信息行与操作指南行是两个
/// `Panel::bottom`，它们在滚动区**之前**就把高度占掉了，所以调用方传进来的
/// `remaining_h`（`ui.available_height()`）里天然不含它们 —— 再扣一次反而会凭空
/// 在"文件信息"与"操作指南"之间留出一段空白（用户明确不要的）。
/// 按钮换行到预览下方时，固定内容里再加按钮那一行（否则按钮会被挤出可视区）。
fn preview_avail_h(remaining_h: f32, fixed_below: f32) -> f32 {
    if !remaining_h.is_finite() {
        return 0.0;
    }
    (remaining_h - fixed_below.max(0.0)).max(0.0)
}

/// **预览解码档位**：目标显示宽度 → 量化后的解码宽度上限。
///
/// * 按 [`THUMB_BUCKET_STEP`]（64px）**向上取整**：窗口拖动让目标宽度变化 1~63px 时档位不变，
///   于是缓存命中、**不会重新解码**（这是"拖 1px 不重解"的关键）；
/// * 夹到 `[THUMB_MIN_DECODE_W, THUMB_MAX_DECODE_W]`，并且**不超过源宽**（比源还大的解码纯浪费）；
/// * 源宽未知（`src_w == 0`）时只做档位夹取。
fn thumb_bucket_w(target_w: f32, src_w: u32) -> u32 {
    let t = if target_w.is_finite() && target_w > 0.0 {
        target_w
    } else {
        THUMB_MIN_DECODE_W as f32
    };
    let step = THUMB_BUCKET_STEP as f32;
    let quantized = (t / step).ceil() * step;
    let mut w = (quantized as u32).clamp(THUMB_MIN_DECODE_W, THUMB_MAX_DECODE_W);
    if src_w > 0 {
        w = w.min(src_w).max(1);
    }
    w
}

/// 解码尺寸（上限）：宽度取档位，高度按源比例换算（保持比例，不会把画面压扁）。
fn decode_size(bucket_w: u32, aspect: f32) -> (u32, u32) {
    let aspect = if aspect.is_finite() && aspect > 0.05 {
        aspect
    } else {
        PREVIEW_FALLBACK_ASPECT
    };
    let w = bucket_w.max(1);
    let h = ((w as f32 / aspect).round() as u32).max(1);
    (w, h)
}

// ---------------------------------------------------------------------------
// 纯逻辑（无界面）：时间码、缩放换算、刻度抽稀 —— 全部有单测
// ---------------------------------------------------------------------------

/// **时间线矩形对应的可见时间窗**（毫秒）：`[视图左端, 视图右端]`。
///
/// 它是波形"只画可见区间"的依据，也是"与刻度尺同源"的一半：
/// 反推用的是**同一套** `(px, vs)` 与**同一个矩形** `rect`，
/// 另一半是画的时候用 `x_of`（见 [`paint_timeline_wave`] 的 `x_of` 参数）。
fn visible_time_range(rect: egui::Rect, px: f64, vs: f64) -> (f64, f64) {
    if !px.is_finite() || px <= 0.0 {
        return (vs, vs);
    }
    let t0 = vs;
    let t1 = vs + rect.width() as f64 / px;
    (t0, t1)
}

/// **胶片条每个画面的宽度**（P25，逻辑点）：`行高 × 源宽高比` —— **contain** 缩放的结果。
///
/// 用户口径（原话）：「缩略图，明显没有按照高度适应影片条高度，高度上只显示了画面一部分。」
/// → 画面**按行高等比缩放、完整显示**（不裁切、不变形）：
/// 16:9 ≈ 92 px、竖屏 1080×1920 ≈ 29 px（行高 52）。
///
/// 非法输入（`NaN` / 0 / 负 / 无穷）一律退化为 16:9（`PREVIEW_FALLBACK_ASPECT`），
/// 绝不产生 0 宽、绝不 panic。
pub(crate) fn strip_picture_width(tile_h: f32, aspect: f32) -> f32 {
    let h = if tile_h.is_finite() && tile_h > 0.0 {
        tile_h
    } else {
        1.0
    };
    // 只挡"病态比值"（直接调用本函数的单测可能给任何数），正常源宽高比远在区间内
    let a = if aspect.is_finite() && (1.0 / 20.0..=100.0).contains(&aspect) {
        aspect
    } else {
        PREVIEW_FALLBACK_ASPECT
    };
    (h * a).max(1.0)
}

/// **胶片条（P25）的窄条判据**：可见宽里放得下几个**完整画面** —— 依次 5 → 4 → 3 → 2 → 1。
///
/// 规则（用户口径「条太窄放不下时仍按 5→4→3→2→1 递减并保持"首贴左、尾贴右"」）：
/// 取**最大的** `n ∈ {5,4,3,2,1}` 使
///
/// ```text
/// n × 画面宽 + (n − 1) × STRIP_MIN_GAP_PX  ≤  可见宽
/// ```
///
/// 也就是"n 个按行高等比缩放后的真实画面宽度之和，再加上 n−1 个不小于
/// `STRIP_MIN_GAP_PX` 的间隙，仍然装得下"。**不再用 P21 的 `n × 160` 口径**：
/// 画面宽现在由行高与源宽高比决定（P25），160 只是解码请求宽、与布局无关。
/// 连一个画面都放不下（可见宽 < 画面宽 + 0 个间隙）时**恒为 1**
/// （宁可让那一个被右边界裁掉一点，也不显示空条）。
///
/// 为什么是"减少块数"而不是"缩小画面"：画面尺寸由"行高 × 源宽高比"这条
/// **contain 口径**唯一确定（既不能裁也不能压），所以放不下时能变的只有**块数**。
pub(crate) fn strip_slot_count(visible_w: f32, picture_w: f32) -> usize {
    if !visible_w.is_finite() || !picture_w.is_finite() || picture_w <= 0.0 {
        return 1;
    }
    let gap = if STRIP_MIN_GAP_PX.is_finite() {
        STRIP_MIN_GAP_PX.max(0.0)
    } else {
        0.0
    };
    let mut n = STRIP_TILES;
    while n > 1 && (n as f32) * picture_w + (n - 1) as f32 * gap > visible_w {
        n -= 1;
    }
    n
}

/// **一帧胶片条的布局结果**（P25，纯逻辑）：每个画面的绘制矩形（屏幕 x）+ 取样时刻。
///
/// 这是"画面画在哪、画多大"的**唯一来源**：绘制与单测都读它，绝不在别处再算一遍。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StripSlot {
    /// 取样时刻（毫秒，未量化）
    pub at_ms: i64,
    /// 归一化位置 `k/(n−1)` ∈ `[0,1]`（P21 的"锚点系数"；P25 只用于同源断言）
    pub frac: f32,
    /// 画面左边界（屏幕 x）
    pub left: f32,
    /// 画面宽（= `行高 × 源宽高比`，[`strip_picture_width`]）
    pub width: f32,
    /// 画面高（= 行高，[`strip_tile_height`]）
    pub height: f32,
}

/// **P25 胶片条布局（纯逻辑）**：首贴左、尾贴右、相邻间隙相等。
///
/// 用户口径（原话）：「5 个画面在条中均匀分布，1 在最左，5 在最右。」
///
/// 入参 `left0` 就是 **`x_of(t_0)`** —— 与波形/刻度**同一个**时间→屏幕 x 映射算出来的
/// 首块落点（正常情况它严格等于条左边界：取样起点就是可见起点）。于是：
///
/// ```text
/// gap       = (条右边界 − left0 − Σ画面宽) / (n − 1)        // 相邻间隙相等
/// left[0]   = left0                                        // 首贴左
/// left[k]   = left[k−1] + 画面宽[k−1] + gap                 // 逐个累加
/// ⇒ left[n−1] + 画面宽[n−1] = 条右边界                      // 尾贴右
/// ```
///
/// * **各画面宽可能不同**时按**各自真实宽度**求和（`widths[k]`；当前一个素材只有一个
///   源宽高比，所以实际都相同 —— 但公式本身不假设相同）；
/// * 「不与波形/刻度跑偏」：位置全部由 `x_of(t_0)` + 条宽 + 画面宽推出，同一个 `(px, vs)`；
///   等宽时它正是 `left[k] = x_of(t_k) − frac_k × 画面宽`（P21 那条"锚点"公式，
///   只是把块宽 160 换成了真实画面宽）—— 有单测钉住这个恒等式；
/// * 非法/退化输入（`nan`、右边界 ≤ 左边界、空列表）**不 panic**：间隙夹到 `≥ 0`
///   （宁可挨着也不重叠），`n = 0` 给空结果。
pub(crate) fn strip_layout(
    samples: &[(i64, f32)],
    widths: &[f32],
    tile_h: f32,
    left0: f32,
    strip_right: f32,
) -> Vec<StripSlot> {
    let n = samples.len().min(widths.len());
    if n == 0 {
        return Vec::new();
    }
    let height = if tile_h.is_finite() && tile_h > 0.0 {
        tile_h
    } else {
        1.0
    };
    let left0 = if left0.is_finite() { left0 } else { 0.0 };
    let right = if strip_right.is_finite() {
        strip_right
    } else {
        left0
    };
    // 每个画面的实际宽度（非法值退化到 1，绝不产生负宽/NaN 宽）
    let w_of = |k: usize| -> f32 {
        let w = widths[k];
        if w.is_finite() && w > 0.0 {
            w
        } else {
            1.0
        }
    };
    let sum: f32 = (0..n).map(w_of).sum();
    // 相邻间隙：等间隙口径；窄到装不下时夹到 0（宁可紧挨，也不重叠/倒序）
    let gap = if n > 1 {
        ((right - left0 - sum) / (n - 1) as f32).max(0.0)
    } else {
        0.0
    };
    let mut out = Vec::with_capacity(n);
    let mut left = left0;
    for (k, &(at_ms, frac)) in samples.iter().enumerate().take(n) {
        let width = w_of(k);
        out.push(StripSlot {
            at_ms,
            frac,
            left,
            width,
            height,
        });
        left += width + gap;
    }
    out
}

/// **胶片条（P21）的取样时刻**：可见区间 `[t0, t1]` → `[(时刻, 归一化位置 k/(n−1))]`。
///
/// 用户原话：「可见区域显示 5 个缩略图，最左侧 1 个、最右侧 1 个、中间均匀 3 个」。
///
/// 1. **取样时刻**（P21 起公式未改，**P27 改了"按谁算"**）：`t_k = t0 + k × (t1 − t0) / (n − 1)`
///    —— 5 块时正好是 `t0 + k × 可见时长 / 4`（与用户给的公式逐字一致）；
/// 2. **落位不再在这里算**：P25 起交给 [`strip_layout`]（首贴左、尾贴右、等间隙）。
///    这里给出的第二项是"该时刻在可见区间里的归一化位置"`frac = k/(n−1)`；
/// 3. **P27 ①（按视频流取样）**：第三个参数是**视频流时长**（不是容器时长），
///    并且所有取样时刻一律夹到 `[0, 视频流时长 − ε]`（ε = [`P27_SAMPLE_EPS_MS`]，
///    见那里的说明）。可见区间的右端也**先**夹到这个末端再等分
///    —— 于是"整片铺满"时 5 个取样点仍然均匀铺在**视频流的那一段**里，
///    而不是把最后一块白白扔进"只有音频"的尾巴（实机那次解码失败就是它）。
///
/// 返回 `(时刻毫秒, 归一化位置)`；后者 ∈ `[0, 1]`。
fn strip_samples(t0: f64, t1: f64, video_end_ms: i64, n: usize) -> Vec<(i64, f32)> {
    if !t0.is_finite() || !t1.is_finite() || t1 <= t0 || n == 0 {
        return Vec::new();
    }
    if video_end_ms <= 0 {
        return Vec::new();
    }
    // **P27 ①**：可用末端 = 视频流时长 − ε（ε 见 [`P27_SAMPLE_EPS_MS`]）
    let last = p27_sample_last_ms(video_end_ms) as f64;
    let a = t0.max(0.0).min(last);
    let b = t1.max(0.0).min(last);
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let frac = if n <= 1 {
            0.0
        } else {
            k as f64 / (n - 1) as f64
        };
        // 取样时刻（**先夹可见区间右端、再夹每个点**：双重保证不越出视频流）
        let t = if b > a {
            a + frac * (b - a)
        } else {
            // 可见区间整段落在视频流之外（用户把视口拖到"只有音频"的尾巴上）：
            // **不退化成空条**，5 块一律取视频流末端那一帧 —— 用户口径是
            // "视觉上永远有 5 张图"（见 P27 补充的兜底链）。
            a
        };
        let t = t.clamp(0.0, last);
        out.push((t.round() as i64, frac as f32));
    }
    out
}

// ---------------------------------------------------------------------------
// P27 · 采样跨度 / 回退方向 / 有界探查（纯逻辑，全部有单测）
// ---------------------------------------------------------------------------

/// **P27：取样用的"视频流末端"**（毫秒）+ 它是不是真的来自视频流。
///
/// 唯一入口。返回 `(末端, from_video_stream)`：
/// * `from_video_stream == true` → 视频流自己写了时长（**正常情况下都走这条**）；
/// * `false` → 拿不到视频流时长（没有视频流 / 流里没写 duration），
///   回退成**容器时长**；调用方（`EditorPage::log_p27_sample_span`）要为此记一行日志。
pub fn p27_sample_span_end_ms(clip: &Clip) -> (i64, bool) {
    match clip.video_duration_ms() {
        Some(d) => (d, true),
        None => (clip.duration_ms.max(0), false),
    }
}

/// **P27 的 ε 落点**：取样时刻允许取到的最大值 = `视频流末端 − ε`（夹到 `≥ 0`）。
pub(crate) fn p27_sample_last_ms(span_end_ms: i64) -> i64 {
    (span_end_ms - P27_SAMPLE_EPS_MS).max(0)
}

/// **P27 ②：第 k 块的失败回退方向**（用户口径：前 3 块向右、后 2 块向左）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackDir {
    /// 向**后**（时间增大）寻找
    Right,
    /// 向**前**（时间减小）寻找
    Left,
}

/// **P27 ②：方向规则**（用户原话：「前 3 个失败 → 向右；后 2 个失败 → 向左」，
/// 「n < 5（窄条递减）时按同一精神推广：`k < n/2` 向右、否则向左」）。
///
/// 实现取 **`2k < n`**，也就是**实数除法**的 `k < n/2`：
/// * `n = 5` → k = 0,1,2 **向右**；k = 3,4 **向左**（用户要求的"3 右 / 2 左"，正好）
/// * `n = 4` → k = 0,1 右；k = 2,3 左
/// * `n = 3` → k = 0,1 右；k = 2 左
/// * `n = 2` → k = 0 右；k = 1 左
/// * `n = 1` → k = 0 右（唯一一块先向右；右边没有就靠兜底链，见 P27 补充）
/// * `n = 0` → 向右（不 panic；调用方本来就不会给空列表下问）
///
/// 为什么不是整数除法 `k < n/2`：`n = 5` 时整数除法给 `k < 2` = **2 右 3 左**，
/// 与用户明确写下的"5 块时正好 3 右 / 2 左"矛盾。整数除法是这里唯一容易写错的地方，
/// 单测把 n = 1..5 的每一格都钉住了。
pub(crate) fn strip_fallback_dir(k: usize, n: usize) -> FallbackDir {
    if n == 0 {
        return FallbackDir::Right;
    }
    if 2 * k < n {
        FallbackDir::Right
    } else {
        FallbackDir::Left
    }
}

/// **P27 ③：为一个失败的块生成"有界"的回退探查时刻**（按距原时刻由近到远）。
///
/// 这是"回退必须有界"的**唯一实现处**，四条边界都在这里（都不依赖界面、不碰 FFmpeg）：
/// 1. **方向**由 [`strip_fallback_dir`] 定（前一半向右、后一半向左）；
/// 2. **不得跨过相邻块的取样点**：`neighbor_ms` 是该方向上相邻块（k±1）的取样时刻，
///    探查窗口严格停在它**之前**（右）/ **之后**（左）—— 这样两个块永远不会互相
///    抢到对方的画面（用户明确要求"避免重复或错序"）；
/// 3. **不得越出视频流**：向右探查的上界是 `last_ms`（= 视频流时长 − ε）——
///    视频流之外本来就没有帧，往那里扫纯属浪费；
/// 4. **次数与距离双上界**：最多 `max_attempts` 个候选，且每个都满足
///    `|候选 − t| ≤ max_dist_ms`。
///
/// 候选怎么来：**方向上的关键帧**（最省事的落点）**加上**等距小步长
/// （`step_ms`；关键帧之间的坏区、或"这片根本没有关键帧索引"时靠它兜住），
/// 合并去重后**按距离升序**排 —— 命中第一个能解的即停，视觉上离原时刻最近。
///
/// 退化输入（`max_attempts = 0`、窗口为空、`neighbor` 就是自己）→ 空表，绝不 panic。
// 纯函数，7 个入参都是"回退策略"的独立旋钮；打包成 struct 反而让测试里写不清意图。
#[allow(clippy::too_many_arguments)]
pub(crate) fn strip_fallback_probes(
    t_ms: i64,
    dir: FallbackDir,
    keyframes: &[i64],
    neighbor_ms: Option<i64>,
    last_ms: i64,
    max_attempts: usize,
    max_dist_ms: i64,
    step_ms: i64,
) -> Vec<i64> {
    if max_attempts == 0 {
        return Vec::new();
    }
    let dist = max_dist_ms.max(1);
    let step = step_ms.max(1);
    let last = last_ms.max(0);
    // ---- 边界 1/3：方向 + 视频流范围 + 最大距离 ----
    let (mut lo, mut hi) = match dir {
        FallbackDir::Right => (t_ms.saturating_add(1), t_ms.saturating_add(dist).min(last)),
        FallbackDir::Left => (t_ms.saturating_sub(dist).max(0), t_ms.saturating_sub(1)),
    };
    // ---- 边界 2：不得跨过相邻块的取样点 ----
    if let Some(nb) = neighbor_ms {
        match dir {
            FallbackDir::Right => hi = hi.min(nb.saturating_sub(1)),
            FallbackDir::Left => lo = lo.max(nb.saturating_add(1)),
        }
    }
    if hi < lo {
        return Vec::new();
    }
    let mut cands: Vec<i64> = Vec::new();
    // 关键帧候选（就在窗口里的那些）
    for k in keyframes {
        if *k >= lo && *k <= hi {
            cands.push(*k);
        }
    }
    // 小步长候选（次数被 max_attempts 夹住，绝不可能变成"扫全片"）
    let signed = match dir {
        FallbackDir::Right => step,
        FallbackDir::Left => -step,
    };
    let mut p = t_ms.saturating_add(signed);
    let mut guard = 0usize;
    while p >= lo && p <= hi && guard <= max_attempts {
        cands.push(p);
        p = p.saturating_add(signed);
        guard += 1;
    }
    cands.retain(|k| *k >= lo && *k <= hi);
    cands.sort_unstable();
    cands.dedup();
    // ---- 边界 4：最近的先试；候选数被 max_attempts 夹住 ----
    cands.sort_by_key(|k| k.saturating_sub(t_ms).saturating_abs());
    cands.truncate(max_attempts);
    cands
}

/// **P27 ①：方向搜索的尝试循环**（**唯一实现处**；真正的解码由闭包给）。
///
/// 语义：先试原时刻 `at_ms`，再按 `alts` 的顺序逐个试，**命中第一个成功的即停**；
/// 全都不成 → 返回**第一次**的错误（最有信息量的那条）与总尝试次数。
///
/// 为什么把它单独抽出来：**"回退有界"这件事必须能在不碰 FFmpeg 的前提下被断言**
/// —— 单测拿一个"只有某个时刻可解"的模拟解码器喂进来，就能钉住
/// "停在该处 / 尝试次数 ≤ 上限 / 不在失败之后继续乱试"这三条。
fn p27_search_decode<T>(
    at_ms: i64,
    alts: &[i64],
    mut decode: impl FnMut(i64) -> Result<T, String>,
) -> Result<(T, i64, usize), (String, usize)> {
    let mut attempts = 0usize;
    let mut first_err: Option<String> = None;
    for t in std::iter::once(at_ms).chain(alts.iter().copied()) {
        attempts += 1;
        match decode(t) {
            Ok(v) => return Ok((v, t, attempts)),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    Err((first_err.unwrap_or_default(), attempts))
}

/// **P27：把一行 P27 事件同时写进两个落点**（开发控制台 + 用户机器上的 `cap.log`）。
///
/// 为什么必须两个都写：**产品（GUI）没有安装 tracing 订阅者**
/// （见 `crate::ui::startup::note` 的说明：`tracing::info!/warn!` 在用户机器上全部丢弃），
/// 真正落地的是 [`crate::ui::startup::mark_with`] 那条启动日志落点。
/// 所以"取样失败只警告、不打扰用户"这句话的实际落地是：
/// **界面不弹提示 + `cap.log` 里一行 `phase=p27_*`**（外加开发控制台一条 tracing）。
fn p27_log(phase: &'static str, warn: bool, line: String) {
    if warn {
        tracing::warn!(phase = phase, detail = %line, "P27");
    } else {
        tracing::info!(phase = phase, detail = %line, "P27");
    }
    crate::ui::startup::mark_with(phase, "wave", Some(std::time::Duration::ZERO), Some(line));
}

/// **P27 兜底链的来源**（只用于日志与单测断言；界面**不**因此打扰用户）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackSource {
    /// ① 方向搜索命中（解码线程里换了时刻才解出来）
    Direction,
    /// ② 同一条上另一个已经成功的块（就近选）
    Neighbor,
    /// ③ 胶片条缓存里最近一张成功的画面
    Cache,
    /// ④ 没有兜底可用 → 留空 + 一行警告
    None,
}

impl FallbackSource {
    fn as_str(self) -> &'static str {
        match self {
            FallbackSource::Direction => "direction",
            FallbackSource::Neighbor => "neighbor",
            FallbackSource::Cache => "cache",
            FallbackSource::None => "none",
        }
    }
}

/// **P27 ①：取样跨度那一行的内容**（纯函数，可单测 —— 日志内容本身也要有断言）。
fn p27_sample_span_line(
    file: &str,
    span_end_ms: i64,
    from_video: bool,
    container_ms: i64,
) -> String {
    format!(
        "file={file} span_end_ms={span_end_ms} container_dur_ms={container_ms} \
         eps_ms={P27_SAMPLE_EPS_MS} source={}",
        if from_video {
            "video_stream"
        } else {
            "container_fallback"
        }
    )
}

/// **P27 兜底/放弃那一行的内容**（纯函数，可单测）。
///
/// 用户口径要求这一行写明"**取样失败、已用兜底帧（来源=邻块/缓存）**"。
/// 文案按本项目的**日志惯例写成 ASCII**（P1 那条口径：只落 `cap.log` 的诊断串不翻译、
/// 也不该被 i18n 扫描器当成"界面硬编码中文"）：
/// * `reason=sample_failed` —— 取样失败（方向搜索到界）；
/// * `used_fallback_frame=<source>` —— 用的是哪一级兜底帧；`none` 表示**留空**；
/// * `slot=empty` —— 只在 `none` 时出现（如实写明"这一格没有画面"）。
fn p27_fallback_line(at_ms: i64, decoded_ms: i64, source: FallbackSource, error: &str) -> String {
    let mut line = format!(
        "reason=sample_failed used_fallback_frame={} at_ms={at_ms} decoded_ms={decoded_ms} \
         max_attempts={P27_MAX_ATTEMPTS}",
        source.as_str()
    );
    if source == FallbackSource::None {
        line.push_str(" slot=empty");
    }
    if !error.is_empty() {
        line.push_str(" error=");
        line.push_str(error);
    }
    line
}

/// **P27 ②：在同一条上"就近"挑一个已经成功的块**（兜底链第②级）。
///
/// `idx` = 失败块的下标 k；返回离它最近的那个成功块（相同距离取下标小的，
/// 保证同一状态永远给同一结果）。`idx` 为 `None`（理论上不会发生）时取中间那一个。
/// 成功块为空 → `None`（交给第③级）。
fn nearest_strip_thumb(prev: &[Thumb], idx: Option<usize>) -> Option<&Thumb> {
    if prev.is_empty() {
        return None;
    }
    let Some(i) = idx else {
        return prev.get(prev.len() / 2);
    };
    prev.iter()
        .min_by_key(|t| ((t.idx as i64 - i as i64).unsigned_abs(), t.idx))
}

/// **胶片条的解码键**：把取样时刻**量化**到 `STRIP_QUANT_MS` 网格（缓存跨缩放复用的关键）。
///
/// 画的时候用的是**没量化**的时刻（贴边才准），只有"要解码哪一帧"用这个键。
pub(crate) fn strip_quantized_ms(t_ms: i64) -> i64 {
    let q = STRIP_QUANT_MS.max(1);
    // 四舍五入到最近的网格点（不是向下取整：向下会让整条块系统性偏左半个网格）
    ((t_ms + q / 2) / q) * q
}

/// **胶片条解码键的最终形态**（请求与查表**必须**用同一个函数）。
///
/// 两件事：
/// 1. 量化到 `STRIP_QUANT_MS` 网格（跨缩放命中缓存）；
/// 2. **夹到 `[0, 时长 − 1]`**：可见区间右端正好是片尾时，取样时刻会等于 `duration`，
///    而"片尾那一刻"是取不到画面的（seek 到末尾必然失败）。夹到 `duration − 1` 之后
///    解码一定能成功 —— 更重要的是：**请求用的键与查表用的键必须是同一个值**，
///    否则会出现"每次请求都成功、却永远查不到缓存"的**无限请求循环**
///    （界面于是每 50 ms 重绘一次，看着就是"胶片条把界面拖成永动机"）。
pub(crate) fn strip_key_ms(t_ms: i64, duration_ms: i64) -> i64 {
    let last = duration_ms.saturating_sub(1).max(0);
    strip_quantized_ms(t_ms).clamp(0, last)
}

/// **胶片条每个画面的 uv 计算**（P25：**contain 完整显示**，取代 P19–P24 的 cover 裁剪）。
///
/// 口径变了：P19 起是"按格子比例裁掉纹理多出来的那一边"（cover），竖屏素材因此
/// 只看得见中间一条横带（用户原话：「高度上只显示了画面一部分」）。P25 改成 **contain**：
/// 画面**整幅**贴进格子 —— 格子本身已经是"行高 × 源宽高比"（[`strip_picture_width`]），
/// 所以 uv 就是**整张纹理** `(0,0)…(1,1)`，不需要裁任何一边。
///
/// 唯一还要做的事：画面被**时间线矩形**裁掉一部分时（极端窄条 / 只剩一块且比条还宽），
/// 把"露出来的那一块"折算进 uv，否则贴图会整体平移（看着像画面滑出去了）。
/// 退化输入（0 宽/0 高）给全量 uv，绝不产生畸形 uv、绝不 panic。
fn strip_contain_uv(full: egui::Rect, clipped: egui::Rect) -> egui::Rect {
    if !(full.width() > 0.0 && full.height() > 0.0) {
        return egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    }
    let rel = egui::Rect::from_min_max(
        egui::pos2(
            ((clipped.left() - full.left()) / full.width()).clamp(0.0, 1.0),
            ((clipped.top() - full.top()) / full.height()).clamp(0.0, 1.0),
        ),
        egui::pos2(
            ((clipped.right() - full.left()) / full.width()).clamp(0.0, 1.0),
            ((clipped.bottom() - full.top()) / full.height()).clamp(0.0, 1.0),
        ),
    );
    // 整幅画面 = 整张纹理：露出的部分直接就是 uv 的那一块
    rel
}

// ---------------------------------------------------------------------------
// P24 · 胶片条块的**尺寸**与**缓存键**（纯逻辑，全部有单测）
// ---------------------------------------------------------------------------

/// **胶片条每个画面的绘制高度**（逻辑点，P24）= 胶片条行高 `THUMB_BAND_H`。
///
/// **行高的唯一来源**：`timeline()` 里胶片条那一带的顶端就是 `rect.top()`，
/// 底端由 `track_top = rect.top() + THUMB_BAND_H + THUMB_BAND_GAP` 定出来
/// —— 也就是说**行高在源码里只有 `THUMB_BAND_H` 一处**，
/// `TIMELINE_H` / `ui::layout::EDITOR_REQ_H` / 本函数全都从它推导。
/// 本函数是"画面高"这一侧的唯一入口（调用方不许自己再写 `THUMB_BAND_H` 或任何减数）。
///
/// **P25**：这个高度同时是 **contain 缩放的基准** —— 画面宽 = `本值 × 源宽高比`
/// （[`strip_picture_width`]），所以"高度适配影片条高度"与"整幅画面都看得见"
/// 由同一个值同时保证。
///
/// 输入与窗口/DPI 无关（`THUMB_BAND_H` 是编译期常量），所以这个值**恒定**
/// —— 用户确认「行高不会随窗口/DPI 变化」。详见 `STRIP_TILE_H`。
pub(crate) fn strip_tile_height() -> f32 {
    STRIP_TILE_H.max(1.0)
}

/// **胶片条缓存键里的高度分量**（P24）：把绘制高度量化到 `STRIP_H_QUANT_PX` 的网格。
///
/// 为什么当前行高是常量也要过这一道：缓存键**必须**包含高度
/// （用户口径：即使当前行高固定也要带上，避免将来行高变化时互相污染）。
/// 这是"时刻量化"那条纪律的同一条：**请求与查表走同一个函数**。
///
/// 非法输入（`NaN` / 负 / 0）退化为 1 —— 绝不产生 0 高度的键（也绝不 panic）。
pub(crate) fn strip_h_key(tile_h: f32) -> u32 {
    if !tile_h.is_finite() || tile_h <= 0.0 {
        return 1;
    }
    let step = STRIP_H_QUANT_PX.max(1.0);
    let q = (tile_h / step).round() * step;
    (q.round().max(1.0)) as u32
}

/// **胶片条渲染纹理的缓存键**（P24；P25 起带**口径版本**）：`(量化时刻, 解码宽, 块高, 版本)`。
///
/// 四个分量各自的来源与量化规则：
/// * `time` = [`strip_key_ms`]（量化到 `STRIP_QUANT_MS`，并夹到 `[0, 时长−1]`）；
/// * `w` = `STRIP_TILE_W`（**解码请求宽**常量 160；P25 起绘制宽由行高与源宽高比算出，
///   与它无关）；
/// * `h` = [`strip_h_key`]（量化到 `STRIP_H_QUANT_PX`，P24 新增）；
/// * `v` = `STRIP_THUMB_VERSION`（**布局口径版本**，P25 新增：cover 槽位 → contain 完整显示）。
///
/// 四者都进键 ⇒ 行高一变就是**另一份缓存**（不会把 52 px 解出来的画面贴进 40 px 的格子），
/// 口径一变也是**另一份缓存**（旧口径的纹理整批作废）。**请求、查表、失败表、预填**
/// 全部走这一个构造函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct StripThumbKey {
    /// 量化后的取样时刻（毫秒）
    pub time: i64,
    /// 解码请求的像素宽（`STRIP_TILE_W`；P25 起**不是**绘制宽）
    pub w: u32,
    /// 量化后的块的像素高（[`strip_h_key`]）
    pub h: u32,
    /// **布局口径版本**（`STRIP_THUMB_VERSION`）：口径变化 ⇒ 旧缓存整批作废
    pub v: u32,
}

/// 构造胶片条缓存键（**唯一入口**，见 [`StripThumbKey`] 的说明）。
fn strip_thumb_key(t_ms: i64, duration_ms: i64, tile_w: u32, tile_h: f32) -> StripThumbKey {
    StripThumbKey {
        time: strip_key_ms(t_ms, duration_ms),
        w: tile_w,
        h: strip_h_key(tile_h),
        v: STRIP_THUMB_VERSION,
    }
}

// ---------------------------------------------------------------------------
// P23 · 「导入就绪」的判据（纯逻辑，有单测）
// ---------------------------------------------------------------------------

/// **"导入就绪"的判据**（P23，用户确认的三条）：
/// 1. `video_info` —— **视频流信息就绪**（`clip.video` 存在且宽高 > 0）；
/// 2. `preview` —— **首帧预览已渲染出一帧**（本帧真的把一张缓存纹理贴进了预览画框）；
/// 3. `strip` —— **胶片条首块已显示**（至少有一块贴出了画面，不是占位块）。
///
/// 三条**全中**才算就绪 ⇒ 导入阶段结束、波形阶段自动开始。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportDecision {
    pub video_info: bool,
    pub preview: bool,
    pub strip: bool,
    /// 是否用了**兜底**（超时；见 `IMPORT_READY_TIMEOUT_MS`）
    pub fallback: bool,
}

impl ImportDecision {
    /// 是否已经"导入就绪"（三条全中，或兜底触发）。
    pub fn ready(&self) -> bool {
        self.fallback || (self.video_info && self.preview && self.strip)
    }

    /// 给 `cap.log` 用的条件摘要。
    pub fn detail(&self) -> String {
        format!(
            "video_info={} preview={} strip={} fallback={}",
            self.video_info, self.preview, self.strip, self.fallback
        )
    }
}

/// **导入就绪判定**（唯一入口；`EditorPage::check_wave_phase` 与单测共用）。
///
/// `elapsed_ms` = 从 [`EditorPage::open`] 成功载入那一刻起的毫秒数。
/// 只有"三条全中"或"超时"两种结果会返回 `ready() == true`；
/// 其余情况返回 `false` 并**如实带上还差哪一条**（日志与单测都读它）。
///
/// `timeout_ms` 传 `None` = 不做超时兜底（单测用来钉住"纯判据"那一半）。
pub(crate) fn import_readiness(
    video_info: bool,
    preview: bool,
    strip: bool,
    elapsed_ms: u64,
    timeout_ms: Option<u64>,
) -> ImportDecision {
    let fallback = matches!(timeout_ms, Some(t) if elapsed_ms >= t);
    ImportDecision {
        video_info,
        preview,
        strip,
        fallback,
    }
}

/// **全片位置行的映射**（P14 / P14 二次修订）：全片中的时刻 → 位置行内的横坐标。
///
/// 它与时间线本体的 `x_of` **不是同一套**，这是刻意的：位置行那条蓝条代表**整片**
/// （静态概览），所以它的横坐标只取决于"时刻占全片的比例"，与当前缩放/平移
/// （`px` / `vs`）**无关**。位置行里两样东西全走这一套（P17 起不再有方块）：
/// * 蓝条本身（整条铺满 = 全片，与 `duration` 无关）；
/// * 淡色带（`[vs, vs + 可见时长]` 夹到 `[0, duration]` = **当前可见区间在全片里的位置**）。
///
/// 为什么二次修订必须把位置行改用全片映射（而不是继续用视口映射）：
/// 1. **语义**：淡色带必须表达"我正看着全片的哪一段"。若用视口映射，它恒等于整条
///    （`x_of(vs)` 恒在左端、`x_of(vs+可见时长)` 恒在右端），于是再也表达不出"我在哪"，
///    蓝条被色带整条盖住。
/// 2. **稳定性**：用视口映射做拖动会形成反馈回路 —— 指针 x 经视口映射换算成时刻、
///    时刻又去改视口，下一帧同一个指针位置换算成**另一个**时刻，视口于是每帧自己往前爬。
///    全片映射与视口无关，回路不存在。
///
/// 整片铺满（fit）时 `vs = 0`、可见时长 = `duration`，两套映射**完全等价**，观感不变。
fn pos_row_x_of(t_ms: f64, left: f64, width: f64, duration_ms: i64) -> f32 {
    let dur = duration_ms as f64;
    if !(t_ms.is_finite() && left.is_finite() && width.is_finite() && dur.is_finite())
        || dur <= 0.0
        || width <= 0.0
    {
        return if left.is_finite() { left as f32 } else { 0.0 };
    }
    (left + (t_ms.clamp(0.0, dur) / dur) * width) as f32
}

/// [`pos_row_x_of`] 的反解：位置行内的横坐标 → 全片中的时刻
///（P30 起用于"点位置行 = 把可见区间挪到以点击处为中心"）。
///
/// 结果恒在 `[0, duration]` 内（指针跑到行外也只给出片内时刻）。
fn pos_row_t_of(x: f32, left: f64, width: f64, duration_ms: i64) -> f64 {
    let dur = duration_ms as f64;
    if !(x.is_finite() && left.is_finite() && width.is_finite() && dur.is_finite())
        || dur <= 0.0
        || width <= 0.0
    {
        return 0.0;
    }
    (((x as f64 - left) / width) * dur).clamp(0.0, dur)
}

/// **滚动条"跳到某处"**（P30）：给定"目标时刻 / 可见时长 / 全片时长"，
/// 算出让该时刻落在视口**正中央**的视口左端。
///
/// 调用点只有一个：[`EditorPage::timeline`] 里"点全片位置行"那一段
/// —— 点哪儿，可见区间就挪到以那儿为中心。它**与当前帧无关**：
/// P30 起当前帧不再居中跟随（用户口径："当前帧取消保持在显示区域的中间位置"），
/// 视口只响应两种**主动**操作：滚轮缩放（锚点 = 鼠标）与拖/点位置行。
///
/// **夹取规则**（用户要求："不要出现 `view_start < 0` 或 `view_start + span > duration`"）：
/// * 可见时长先夹到 `[0, duration]`（一屏最多显示整片）；
/// * `view_start = 目标时刻 − 可见时长/2`，再夹到 `[0, duration − 可见时长]`。
///   靠近片头时左端就是 `0`、靠近片尾时右端正好是 `duration` ——
///   视口**永远在片内**，只是那时目标时刻不再居中（贴边停住）。
fn centered_view_start(t_ms: f64, visible_ms: f64, duration_ms: f64) -> f64 {
    let dur = if duration_ms.is_finite() && duration_ms > 0.0 {
        duration_ms
    } else {
        return 0.0;
    };
    if !t_ms.is_finite() || !visible_ms.is_finite() {
        return 0.0;
    }
    let span = visible_ms.clamp(0.0, dur);
    let max_view = (dur - span).max(0.0);
    let centered = t_ms - span / 2.0;
    if !centered.is_finite() {
        return 0.0;
    }
    centered.clamp(0.0, max_view)
}

/// **播放时的"翻页"规则**：当前帧越过视口右边界时，返回新的视图左端（否则 `None`）。
///
/// 用户口径（原话）：「播放时，上方影片条翻页显示，就是说当前帧走到最右侧时，
/// 影片向左翻动，让当前帧到最左侧」。
///
/// 与 [`centered_view_start`]（滚动条"跳到某处"）是**两种不同语义**，故意分开：
/// * 滚动条是**用户主动搬视口**时用的 —— 手在控制视口，视口跟着动是合理的；
/// * 翻页是**播放时**用的 —— 视口每翻一次就静止一整屏，眼睛不用一直追着画面跑。
///   （P30 起"当前帧居中跟随"已删除，播放时**只剩**这一条视口自移动规则。）
///
/// 边界条件（都有单测）：
/// * 当前帧**在视口内** → `None`（视口一个像素都不动，这是"静止"的来源）；
/// * 越过右边界 → 新左端 = 当前帧（当前帧落在**左边界**），并夹到 `[0, max_view]`；
///   片尾附近夹不住时贴右边界停住（此时当前帧靠右，但视口已经到顶，不能再翻）；
/// * **落在视口左边**（P30 新增）：同样翻到当前帧。这条以前不存在 ——
///   那时缩放会把当前帧强行拉回视野，指针在右端缩放也不会出现"当前帧在视口左边"；
///   P30 删掉那条钳制之后（缩放只改视口），播放中在别处缩放就可能把当前帧甩到视口左侧，
///   若不处理，影片条会**整屏冻住**直到当前帧自己追上右边界 —— 那是"播放看起来卡了"。
///   翻到左边界**不是居中**（用户要求取消的正是居中），只是让播放看得见。
fn page_view_start(cursor_ms: f64, vs: f64, visible_ms: f64, max_view: f64) -> Option<f64> {
    if !(cursor_ms.is_finite() && vs.is_finite() && visible_ms.is_finite()) {
        return None;
    }
    if visible_ms <= 0.0 || !max_view.is_finite() || max_view < 0.0 {
        return None;
    }
    // 视口内 → 一动不动。右边界**含**：走到最右那一列就翻（用户"走到最右侧时翻动"的口径）
    if cursor_ms >= vs && cursor_ms < vs + visible_ms {
        return None;
    }
    let next = cursor_ms.clamp(0.0, max_view);
    // 已经贴着同一位置（片尾夹住了）就别再写一遍，避免无意义的状态抖动
    if (next - vs).abs() < 1e-9 {
        return None;
    }
    Some(next)
}

/// **时间线上的波形绘制结果**。
///
/// 两种构建下都编译（非测试构建里只由 `debug_assert!` 与"没画任何线段"的判定读它，
/// 所以不会有 dead_code 告警）：测试构建里它承载"波形真的画了、画到哪"这些断言依据。
/// 层级/档位的口径不在这里 —— 它是 [`edit::waveform::WaveSegments`] 的事。
#[derive(Debug, Clone, Copy, Default)]
struct WaveDrawn {
    /// 画了多少条竖线段
    segments: usize,
    /// 第一段 / 最后一段的屏幕 x（`NaN` = 一段都没画）
    first_x: f32,
    last_x: f32,
}

impl WaveDrawn {
    /// 有没有真的画出线段（`debug_assert` 与测试共用）。
    fn drew_anything(&self) -> bool {
        self.segments > 0
    }
}

/// **把深度检查的结论并入 `clip.health`**（P35：三处消费只有一个真相源）。
///
/// 并入而不是"另存一份"的理由：播放跳过、坏段绘制、导出剔除读的都是
/// `clip.health`。深度结果若单独放，就会出现"界面说会跳过、导出却没剔"的自相矛盾。
///
/// 重复跑深度检查不会累积：`merge()` 会把同类重叠段合成一条。
fn merge_deep_into_clip(clip: &mut edit::Clip, rep: edit::health::HealthReport) {
    clip.health.bad.extend(rep.bad);
    clip.health.truncated_at_ms = rep.truncated_at_ms.or(clip.health.truncated_at_ms);
    clip.health.full_decode = true;
    clip.health.scanned_ms = clip.health.scanned_ms.max(rep.scanned_ms);
    clip.health.packets = clip.health.packets.max(rep.packets);
    clip.health.merge();
}

/// **在标记行底部画编码错误坏段带**（P35）。
///
/// * `x_of` 必须与波形/刻度/三根标尺**同一个闭包**（同源，绝不自算一套）；
/// * 硬错误用 [`bad_hard_color()`]（实红）、软告警用 [`bad_soft_color()`]（琥珀）——
///   用户一眼就能区分"这段播不了"与"这里只是有点怪"；
/// * 每段至少 2 px 宽：几毫秒的坏段在整片视图下会被压成一个像素，
///   不保底就等于"看不见"（而它恰恰是最需要被看见的）。
///
/// 返回真的画了几段（测试断言用；生产路径不读它）。
fn paint_bad_ranges(
    painter: &egui::Painter,
    track: egui::Rect,
    bad: &[edit::health::BadRange],
    x_of: impl Fn(f64) -> f32,
) -> usize {
    if !track.is_positive() || bad.is_empty() {
        return 0;
    }
    let band = egui::Rect::from_min_max(
        egui::pos2(track.left(), track.bottom() - BAD_BAND_H),
        egui::pos2(track.right(), track.bottom()),
    );
    if !band.is_positive() {
        return 0;
    }
    let clip_rect = painter.clip_rect();
    let mut drawn = 0usize;
    for b in bad {
        let x0 = x_of(b.start_ms as f64);
        let x1 = x_of(b.end_ms as f64);
        if !x0.is_finite() || !x1.is_finite() {
            continue;
        }
        let left = x0.max(band.left());
        let right = x1.max(x0 + 2.0).min(band.right());
        if right <= left {
            continue;
        }
        if left > clip_rect.right() || right < clip_rect.left() {
            continue; // 只画可见区间
        }
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(left, band.top()),
                egui::pos2(right, band.bottom()),
            ),
            1.0,
            if b.kind.is_hard() {
                bad_hard_color()
            } else {
                bad_soft_color()
            },
        );
        drawn += 1;
    }
    drawn
}

/// **把波形画在标记行里**（纯绘制：不做任何时间换算，横坐标全部交给 `x_of`）。
///
/// 参数就是"三条硬要求"本身：
/// * `x_of`：**与胶片条/刻度尺/三根标尺同一个** 时间 → x 映射（同源，绝不自算一套）；
/// * `segs`：由 [`edit::waveform::segments_for_view`] 按"可见区间档位"算好的段，
///   段里自带 `start_ms/end_ms`，所以**绘制这一层完全不需要知道"层级/桶"这些概念**；
/// * `pending_ms`：**渐进**——密集层还在补的时候，在"已经连续覆盖到"的那一刻画一条淡竖线
///   （`None` = 别画：还没进密集层，或者缺口已经补满）。
///
/// 颜色：波形用主题里的次要蓝灰（它一直在那儿，不该抢绿色/红色/蓝色三根标尺的视线）；
/// **L0 兜底的那几段**（密集层还没算到）用淡一档的 [`wave_pending()`]，
/// 一眼能看出"这段还没细算"——但**不留空白**（不能让人以为那里是静音）。
fn paint_timeline_wave(
    painter: &egui::Painter,
    track: egui::Rect,
    segs: &[edit::waveform::WaveSeg],
    pending_ms: Option<i64>,
    x_of: impl Fn(f64) -> f32,
) -> WaveDrawn {
    let mut out = WaveDrawn {
        first_x: f32::NAN,
        last_x: f32::NAN,
        ..WaveDrawn::default()
    };
    if !track.is_positive() || segs.is_empty() {
        return out;
    }
    let lane = track.shrink2(egui::vec2(0.0, 4.0));
    if !lane.is_positive() {
        return out;
    }
    let mid = lane.center().y;
    let half = (lane.height() / 2.0).max(1.0);
    // 密集层比 L0 概览**更实一档**：用户放大之后看到的是"真数据"，
    // 而 L0 兜底的那几段（还没细化到的区间）用淡一档的颜色，一眼能看出"这段还没细算"。
    let clip_rect = painter.clip_rect();
    let mut first = f32::NAN;
    let mut last = f32::NAN;
    for s in segs {
        let x0 = x_of(s.start_ms);
        let x1 = x_of(s.end_ms);
        if !x0.is_finite() || !x1.is_finite() {
            continue;
        }
        let w = (x1 - x0).max(1.0);
        if x0 > clip_rect.right() || x0 + w < clip_rect.left() {
            continue; // 只画可见区间
        }
        let y_top = mid - s.peak.max.clamp(-1.0, 1.0) * half;
        let y_bot = mid - s.peak.min.clamp(-1.0, 1.0) * half;
        // 每段约 1 像素宽就够（`segments_for_view` 让密集段的桶宽 ≈ 可见时长/600，
        // 于是每段 ≈ 1 px；L0 兜底的段也可能比 1 px 窄）；
        // 极小的一段也保底 1px，免得"有声音但看着像一条线"。
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0, y_top.min(y_bot)),
                egui::pos2(
                    x0 + w.max(1.0),
                    y_top.max(y_bot).max(y_top.min(y_bot) + 2.0),
                ),
            ),
            0.0,
            if s.detail {
                wave_color()
            } else {
                wave_pending()
            },
        );
        if first.is_nan() {
            first = x0;
        }
        last = x0 + w;
    }
    out.segments = segs.len();
    out.first_x = first;
    out.last_x = last;

    // ---- 渐进指示：密集层"连续覆盖到"的那一刻画一条淡竖线（**不弹窗、不阻塞、不遮挡**）----
    //
    // 注意这条线的语义（P18 变了）：它**只表示"密集层算到这儿"**，
    // 而不是"整个波形的数据边界"。旧实现用 L0 的 `available_ms` 画，
    // 放大到某一段时它会跑到可见区间左边 → 线画不出来（`cut_ms > segs[0].start_ms` 为假），
    // 用户就完全看不到"还在细化"的提示。
    if let Some(cut_ms) = pending_ms {
        let cut_x = x_of(cut_ms as f64);
        if cut_x.is_finite() && cut_x <= track.right() + 1.0 {
            painter.line_segment(
                [
                    egui::pos2(cut_x, lane.top()),
                    egui::pos2(cut_x, lane.bottom()),
                ],
                egui::Stroke::new(1.0, wave_pending()),
            );
        }
    }
    out
}

/// 选"录制目录"的**纯函数**版本（便于脱界面单测）：
/// 1. 配置里已解析成绝对路径的输出目录（用户明确指定的位置，优先）；
/// 2. 最近一次录制/打开的视频所在目录；
/// 3. 用户视频目录（配置回退链的第一个候选）。
///
/// 空路径一律当作"没有"，避免把 `""` 当成当前目录（双击 exe 时那是 `System32`）。
fn pick_recording_dir(
    resolved_output_dir: Option<&Path>,
    last_video: Option<&Path>,
    user_videos: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(d) = resolved_output_dir.filter(|d| !d.as_os_str().is_empty()) {
        return Some(d.to_path_buf());
    }
    if let Some(d) = last_video
        .and_then(|p| p.parent())
        .filter(|d| !d.as_os_str().is_empty())
    {
        return Some(d.to_path_buf());
    }
    user_videos
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// 取"离 `at_ms` 最近、且**真的能解出来**"的关键帧。
///
/// 为什么要过滤末尾：分片 MP4 重封装成普通 MP4 后，最后一个关键帧的时间戳可能正好等于
/// 文件时长（例如 3946 ms）。对这一刻 seek 会落到文件末尾之后，解码器一帧都拿不到，
/// 界面就会冒出一条"缩略图解码失败"——既不准确也很吓人。这里改成：
/// 先取最近的，但如果它落在末尾（`>= duration_ms`）就退回"末尾之前最后一个关键帧"。
///
/// **P27 ①（同一处根因的第二半）**：这里的"末端"从**容器时长**换成
/// **视频流时长 − ε**（[`p27_sample_last_ms`]）。实机那条报错
/// `在 687695 ms 附近没有可解码的视频帧` 就是一个**正好压在视频流末尾上**的关键帧：
/// 它 `< 容器时长` 所以旧判据放行，却已经越出了视频流 → 必然解不出来。
/// 换判据之后，预览侧同样"只选视频流之内、且不晚于末端 − ε 的关键帧"。
///
/// **P34：关键帧表还没扫完时（`keyframes_scanned == false`）直接取目标时刻**。
/// 理由：流式打开把"探测"与"扫关键帧"拆成两段，首帧预览不该等扫描那几秒 ——
/// 而 `edit::thumbnail` 自己会 seek + 解码，**本来就不依赖这张表**，
/// 表只是用来"挑哪个时刻解"的。表回来之后自然走下面的"最近关键帧"路径
/// （已缓存的按精确时刻存的图仍然有效，最多多解一张）。纯音频不走这条
/// （它的表就是最终态：空，thumbnail 会如实失败并记一行，与改造前一致）。
fn thumb_keyframe(clip: &Clip, at_ms: i64) -> Option<i64> {
    let (span_end, _) = p27_sample_span_end_ms(clip);
    let last = p27_sample_last_ms(span_end);
    let decodable = |k: i64| k >= 0 && k <= last;
    if !clip.keyframes_scanned && clip.video.is_some() {
        return Some(at_ms.clamp(0, last));
    }
    let nearest = edit::nearest_keyframe(clip, at_ms.clamp(0, last));
    match nearest {
        Some(k) if decodable(k) => Some(k),
        // 最近的那个越界了 → 退回"末端之前最后一个可解的关键帧"（有界：只查已扫到的那张表）
        _ => clip
            .keyframes_ms
            .iter()
            .rev()
            .copied()
            .find(|k| decodable(*k)),
    }
}

/// 时间码 `HH:MM:SS.mmm`
fn fmt_time(ms: i64) -> String {
    let ms = ms.max(0);
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let milli = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{milli:03}")
}

/// 解析用户敲进来的时间码文本 → 毫秒。规则（都有单测）：
///
/// * **带冒号** → 时间码：`HH:MM:SS.mmm`、`MM:SS.mmm`、`HH:MM:SS`、`MM:SS`；
///   小数点后不足 3 位按"毫秒的高位"补零（`.4` = 400 ms、`.45` = 450 ms），
///   超过 3 位截断（`.2345` = 234 ms）；逗号当小数点（不少播放器的写法）；
/// * **不带冒号 + 有小数点** → **秒**：`3.4` = 3400 ms（这就是"纯秒数"）；
/// * **不带冒号 + 纯整数** → **毫秒**：`3400` = 3400 ms，与内部单位一致，
///   免得想输 3.4 秒的人打了 `3400` 却得到 56 分钟；
/// * 负数、空串、含非数字字符 → `None`（调用方保持原值，不做猜测）；
/// * 全程 `checked_*`，不会因超大输入 panic。
fn parse_timecode_ms(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 逗号小数点统一成点（"00:00:01,500"）
    let s = trimmed.replace(',', ".");
    let (head, frac) = match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s.as_str(), None),
    };
    let mut frac_ms = 0i64;
    if let Some(f) = frac {
        if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut digits = f.to_string();
        digits.truncate(3);
        while digits.len() < 3 {
            digits.push('0');
        }
        frac_ms = digits.parse::<i64>().ok()?;
    }
    let has_colon = head.contains(':');

    if !has_colon {
        if head.is_empty() || !head.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: i64 = head.parse().ok()?;
        return if frac.is_some() {
            // 带小数点 → 秒（3.4 → 3400 ms）
            n.checked_mul(1000)?.checked_add(frac_ms)
        } else {
            // 纯整数 → 毫秒
            Some(n)
        };
    }

    let mut vals = [0i64; 3];
    let mut n = 0usize;
    for part in head.split(':') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if n >= 3 {
            return None; // 多于 3 段（`1:2:3:4`）不认
        }
        vals[n] = part.parse::<i64>().ok()?;
        n += 1;
    }
    let (h, m, sec) = match n {
        1 => (0, 0, vals[0]),
        2 => (0, vals[0], vals[1]),
        3 => (vals[0], vals[1], vals[2]),
        _ => return None,
    };
    h.checked_mul(3_600_000)?
        .checked_add(m.checked_mul(60_000)?)?
        .checked_add(sec.checked_mul(1000)?)?
        .checked_add(frac_ms)
}

/// 起点/终点的数字调节窗：**值绑毫秒，显示与输入都是时间码**。
///
/// 为什么绑毫秒而不是帧号：帧号依赖帧率，换机器/换素材就对不上；
/// 毫秒是媒体时间轴的真实单位。`.custom_formatter` 把它渲染成 `HH:MM:SS.mmm`，
/// `.custom_parser` 让用户可以直接敲时间码（见 [`parse_timecode_ms`]）。
/// 拖拽步进 10 ms（按住 Shift 时 egui 自动降到 1/10，即 1 ms）。
fn timecode_drag(
    ui: &mut egui::Ui,
    value: &mut i64,
    duration_ms: i64,
    i18n: &crate::i18n::I18n,
) -> egui::Response {
    ui.scope(|ui| {
        // egui 自带的悬停提示会给出行**原始毫秒值**，与"只出现时间码"的诉求冲突；
        // 只在这个控件的作用域里关掉，不影响别处。
        ui.style_mut().explanation_tooltips = false;
        ui.add(
            egui::DragValue::new(value)
                .speed(10.0)
                .range(0..=duration_ms.max(0))
                .custom_formatter(|v, _| fmt_time(v as i64))
                .custom_parser(|s| parse_timecode_ms(s).map(|ms| ms as f64)),
        )
        // 输入格式说明放悬停提示：既不占版面，又能随时查到（三种写法都认）
        .on_hover_text(i18n.t("timecode_hover"))
    })
    .inner
}

/// "整片铺满"时的缩放：可用宽度 ÷ 媒体时长（px/ms）。
///
/// 为什么不再夹到 0.002..4.0：那个区间会让 1 小时以上的长视频永远铺不满宽度
/// （1 小时在 800 px 宽下只需要 2.2e-4 px/ms），也会让 <1 秒的短视频被强行拉大。
/// 这里的下限完全由像素决定（见 [`min_px_per_ms`]），不会妨碍任何真实时长铺满。
fn fit_px_per_ms(avail_w: f32, duration_ms: i64) -> f64 {
    let w = if avail_w.is_finite() {
        (avail_w as f64).max(1.0)
    } else {
        MIN_AVAIL_W as f64
    };
    let d = duration_ms.max(1) as f64;
    let px = w / d;
    if px.is_finite() && px > 0.0 {
        px
    } else {
        min_px_per_ms(duration_ms)
    }
}

/// 缩放的安全下限（px/ms）：由"1 个像素最多代表 [`MAX_MS_PER_PX`] 毫秒"推出。
fn min_px_per_ms(_duration_ms: i64) -> f64 {
    1.0 / MAX_MS_PER_PX
}

/// 把任意来源的缩放值收进合法范围：只保证**有限、正数、不低于像素下限**。
/// **不设产品意义上的上限**；[`MAX_PX_PER_MS`] 只是防 `f32` 绘制坐标溢出的数值护栏。
fn sanitize_px_per_ms(px: f64, duration_ms: i64) -> f64 {
    let lo = min_px_per_ms(duration_ms);
    if !px.is_finite() || px <= 0.0 {
        return lo;
    }
    px.clamp(lo, MAX_PX_PER_MS)
}

/// 滚轮一格 → 缩放倍数。指数映射：一格（约 50~100 px 的 `smooth_scroll_delta`）
/// 对应 1.116~1.246 倍（见单测），落在诉求要求的 1.1~1.25 倍区间里。
fn zoom_factor(scroll: f64) -> f64 {
    if !scroll.is_finite() {
        return 1.0;
    }
    // 先夹一下滚动量：避免 exp 溢出成 inf（极端的高分辨率滚轮/触摸板事件）
    (scroll.clamp(-400.0, 400.0) * ZOOM_PER_SCROLL).exp()
}

/// **以鼠标位置为锚点的缩放**（纯函数，便于单测）。
///
/// 输入：
/// * `view_start`：当前视图左端对应的时刻（毫秒）；
/// * `px_per_ms`：当前缩放（像素 / 毫秒）；
/// * `x`：鼠标相对**时间线左边缘**的像素横坐标（不是屏幕坐标）；
/// * `view_w`：时间线的**可见宽度**（像素）—— 用来算"右端会不会越界"；
/// * `factor`：缩放倍数（>1 放大、<1 缩小，见 [`zoom_factor`]）；
/// * `duration_ms`：素材总时长（毫秒，夹取用）。
///
/// 输出 `(view_start', px_per_ms')`。
///
/// # 不变式（这就是"鼠标下方那个时刻在屏幕上不动"的本体）
///
/// 记 `t = view_start + x / px_per_ms`（鼠标下方的时刻）与
/// `x' = (t − view_start') · px_per_ms'`（缩放后同一个时刻的屏幕横坐标），则：
///
/// * **锚定**：只要没有夹取，`x' == x` —— 由第 3 步的
///   `view_start' = t − x / px_per_ms'` 恒成立（所以鼠标下那条悬停指示线几乎不动）；
/// * **不越界**：`0 ≤ view_start'` 且 `view_start' ≤ duration_ms − view_w / px_per_ms'`
///   （左端不早于 0、右端不晚于片尾，于是既不会缩到"片头之前的空白"，
///   也不会缩到"片尾之后的空白"）；
/// * **最多铺满整片**：`view_w / px_per_ms' ≤ duration_ms` ——
///   一直缩小（`factor < 1`）时比例会被 `view_w / duration_ms` 兜住，
///   **可见时长永远不会超过素材总时长**（否则右端必然越出片尾，见第 2 步的注释）；
/// * **单调**：`factor > 1` ⇒ `px_per_ms' ≥ px_per_ms`（放大不会反而变小）；
/// * **退化输入安全**：任何 `NaN` / `±inf` / 非正数都退回"原样不动"（绝不 panic、
///   也绝不把 `NaN` 写回视图状态）。
///
/// 说明：夹取会**主动放弃锚定**（否则视图会被拖到片外）。夹到边界时锚点会动，
/// 这是"不越界"这条更强的不变式的必然代价 —— 用户看到的是"缩到片头/片尾就停住"。
fn zoom_anchored(
    view_start: f64,
    px_per_ms: f64,
    x: f64,
    view_w: f64,
    factor: f64,
    duration_ms: i64,
) -> (f64, f64) {
    let dur = duration_ms.max(0) as f64;
    // 退化输入：原样退回，绝不把 NaN/inf 写进视图
    if !view_start.is_finite()
        || !px_per_ms.is_finite()
        || px_per_ms <= 0.0
        || !x.is_finite()
        || !view_w.is_finite()
        || view_w <= 0.0
        || !factor.is_finite()
        || factor <= 0.0
        || dur <= 0.0
    {
        // 退化输入：**两项都必须是合法值**再退回。
        // `px_per_ms` 本身可能就是 NaN/inf（调用方传进来就有问题），
        // 所以这里不能原样把它吐回去 —— 那会让"输出永远是有限正数"这条契约失效。
        return (
            if view_start.is_finite() {
                view_start.max(0.0)
            } else {
                0.0
            },
            if px_per_ms.is_finite() && px_per_ms > 0.0 {
                px_per_ms
            } else {
                sanitize_px_per_ms(px_per_ms, duration_ms)
            },
        );
    }
    // 1. 鼠标下方的时刻（并按 [0, duration] 夹一下：鼠标拖到轨道外也不产生片外时刻）
    let t = (view_start + x / px_per_ms).clamp(0.0, dur);
    // 2. 缩放后的新比例。两道下限，缺一不可：
    //    * `sanitize_px_per_ms` 给的是**数值**下限（1 px 最多代表 24 小时），防 0/Inf/NaN；
    //    * **这里**再按 `view_w` 与 `dur` 夹一道**视图语义**下限 ——
    //      "一屏最多只能显示整片"：`view_w / new_px ≤ dur` ⇔ `new_px ≥ view_w / dur`。
    //      少了这一道，一直缩小（factor < 1）会让"可见时长"超过素材总时长，
    //      于是第 4 步的 `max_start` 被夹成 0、而右端 `view_w / new_px` 仍然 > dur ——
    //      "视口右端越出片尾"正是这么来的（用户看到的就是"缩到后面一片空白"）。
    //      `view_w` 非有限/非正数时不做这一道（那时 `view_w / dur` 本身就没有意义，
    //      也绝不能让它把 `new_px` 变成 Inf/NaN —— 上面那条退化输入契约优先）。
    let mut new_px = sanitize_px_per_ms(px_per_ms * factor, duration_ms);
    if view_w.is_finite() && view_w > 0.0 {
        new_px = new_px.max(view_w / dur);
    }
    // 3. 让 t 保持在同一像素位置 —— 这就是"锚定"
    let mut new_start = t - x / new_px;
    // 4. 夹取：左端 ≥ 0；右端 ≤ duration（可见时长已超过整片时锁死在 0）
    let max_start = (dur - view_w / new_px).max(0.0);
    new_start = new_start.clamp(0.0, max_start);
    if !new_start.is_finite() {
        new_start = view_start.max(0.0);
    }
    (new_start, new_px)
}

/// 时间尺步长：从"整"步长表里挑第一个使相邻刻度间距 ≥ `min_gap_px` 的；
/// 表用尽后按 10 倍往上推，**保证** `step * px >= min_gap_px`，
/// 所以极长视频（1 小时以上）也只会画出 `宽度 / min_gap_px` 量级的刻度，不会上万个。
fn ruler_step_ms(px_per_ms: f64, min_gap_px: f64) -> f64 {
    if !(px_per_ms.is_finite() && px_per_ms > 0.0) || !min_gap_px.is_finite() {
        // 缩放非法：给一个"画不出第二根刻度"的步长，绝不进入死循环
        return 1e15;
    }
    let need = min_gap_px / px_per_ms;
    if !need.is_finite() {
        return 1e15;
    }
    for &s in RULER_STEPS_MS.iter() {
        if s >= need {
            return s;
        }
    }
    let mut step = *RULER_STEPS_MS.last().unwrap_or(&600_000.0);
    let mut guard = 0;
    while step < need && guard < 64 {
        step *= 10.0;
        guard += 1;
    }
    if step < need {
        // 理论上到不了（need 得大到 1e63 级别），留个兜底保证间距下界恒成立
        step = need;
    }
    if step.is_finite() && step > 0.0 {
        step
    } else {
        1e15
    }
}

/// 列出 `[view_start, view_end]` 内的时间尺刻度（毫秒，升序）。
/// 抽稀由 [`ruler_step_ms`] 保证：条数 ≤ 可见像素 / `min_gap_px` + 2。
fn ruler_ticks(
    view_start: f64,
    view_end: f64,
    duration_ms: i64,
    px_per_ms: f64,
    min_gap_px: f64,
) -> Vec<f64> {
    let mut out = Vec::new();
    if !view_start.is_finite() || !view_end.is_finite() || view_end <= view_start {
        return out;
    }
    let step = ruler_step_ms(px_per_ms, min_gap_px);
    if !step.is_finite() || step <= 0.0 {
        return out;
    }
    let dur = duration_ms.max(0) as f64;
    let mut t = (view_start / step).ceil() * step;
    // 双保险：即使将来有人改坏了步长推导，也不会画出失控数量的刻度
    const CAP: usize = 4096;
    while t <= view_end && t <= dur && out.len() < CAP {
        out.push(t);
        t += step;
    }
    out
}

/// 关键帧刻度的抽稀：只保留"与上一个保留刻度至少隔 `min_gap_px` 像素"的关键帧
/// （刻度起点是视图左端，第一根永远保留）。返回的条数 ≤ 可见像素 / `min_gap_px` + 1。
///
/// 为什么必须抽稀：1 小时录像按 1 秒 GOP 就是 3600 个关键帧，全画出来既糊成一片
/// 又白费绘制时间；抽稀后 800 px 宽最多约 267 根。
fn visible_keyframe_ticks(
    keyframes: &[i64],
    px_per_ms: f64,
    view_start: f64,
    view_end: f64,
    min_gap_px: f64,
) -> Vec<i64> {
    let mut out = Vec::new();
    if !(px_per_ms.is_finite() && px_per_ms > 0.0)
        || !view_start.is_finite()
        || !view_end.is_finite()
        || view_end <= view_start
        || !min_gap_px.is_finite()
        || min_gap_px <= 0.0
    {
        return out;
    }
    let mut last_px = f64::NEG_INFINITY;
    for &k in keyframes {
        let t = k as f64;
        if t < view_start || t > view_end {
            continue;
        }
        let x = (t - view_start) * px_per_ms;
        if x - last_px >= min_gap_px {
            out.push(k);
            last_px = x;
        }
    }
    out
}

/// 关键帧在 `keyframes_ms` 里的序号（0 基）。列表是升序去重的，二分即可。
///
/// 界面上已经不再显示"第几个关键帧"（预览只看蓝色标尺那一个关键帧），
/// 所以现在只有单测在用（`thumb_size_keeps_aspect` 里那两条断言）；
/// 加 `cfg(test)` 是为了避免非测试构建里的 dead_code 告警。
#[cfg(test)]
fn keyframe_ordinal(keyframes: &[i64], t: i64) -> Option<usize> {
    keyframes.binary_search(&t).ok()
}

/// 一帧的毫秒数（至少 1 ms）。帧率未知时 [`Clip::frame_period_ms`] 已按 1/60 秒兜底。
fn frame_step_ms(clip: &Clip) -> i64 {
    let p = clip.frame_period_ms();
    if p.is_finite() && p >= 1.0 {
        p.round() as i64
    } else {
        1
    }
}

/// `dir > 0` → **严格晚于** `t` 的第一个关键帧；`dir < 0` → **严格早于** `t` 的最后一个。
/// 没有（或没有关键帧信息）时为 `None`，由调用方退回"一帧"。
fn next_keyframe(clip: &Clip, t: i64, dir: i32) -> Option<i64> {
    let kfs = &clip.keyframes_ms;
    if dir > 0 {
        kfs.iter().copied().find(|k| *k > t)
    } else {
        kfs.iter().rev().copied().find(|k| *k < t)
    }
}

/// **当前标尺的键盘微调**（纯函数，便于脱界面单测）。步长规则：
///
/// * 按住 **Ctrl**（Mac 上是 Cmd）→ 固定 **10 ms**：用来做精细对齐；
/// * 按住 **Shift** → 跳到**前/后一个关键帧**（与"能吸附到哪些点"完全一致）；
///   已经在头/尾（没有更远的关键帧）时退回"一帧"，不会卡死不动；
/// * 其余 → **一帧**（`Clip::frame_period_ms`，帧率未知时按 1/60 秒估）。
///
/// 结果一律夹到 `[0, duration]`，并且全程用饱和运算，超大输入也不会 panic/溢出。
fn nudge_cursor_ms(clip: &Clip, cursor_ms: i64, dir: i32, mods: egui::Modifiers) -> i64 {
    let dur = clip.duration_ms.max(0);
    let cur = cursor_ms.clamp(0, dur);
    let step = i64::from(if dir < 0 { -1 } else { 1 });
    let next = if mods.ctrl || mods.command {
        cur.saturating_add(step * 10)
    } else if mods.shift {
        next_keyframe(clip, cur, dir)
            .unwrap_or_else(|| cur.saturating_add(step * frame_step_ms(clip)))
    } else {
        cur.saturating_add(step * frame_step_ms(clip))
    };
    next.clamp(0, dur)
}

/// 缩略图在卡片里的显示尺寸：等比缩到不超过 `max_w × max_h`，保证 16:9 / 4:3
/// 等不同画幅不被拉伸（解码时已经缩过一次，这里只管显示）。
fn fit_thumb_size(w: u32, h: u32, max_w: f32, max_h: f32) -> egui::Vec2 {
    if w == 0 || h == 0 || !max_w.is_finite() || !max_h.is_finite() || max_w <= 0.0 || max_h <= 0.0
    {
        return egui::vec2(max_w.max(1.0), max_h.max(1.0));
    }
    let s = (max_w / w as f32).min(max_h / h as f32);
    egui::vec2((w as f32 * s).max(1.0), (h as f32 * s).max(1.0))
}

// ---------------------------------------------------------------------------
// 测试：无界面（只构造 `EditorPage`，不建 egui 上下文），素材用随包 ffmpeg 现场生成
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::StreamInfo;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Instant;

    /// 造一个"只有时间轴"的假 Clip，用来精确测纯逻辑（不依赖真实媒体）
    fn synthetic_clip(duration_ms: i64, keyframes_ms: Vec<i64>, fps: f64) -> Clip {
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
            // 测试台塞的是"最终态"素材：表已定稿（没有 P34 的"还在扫"中间态）
            keyframes_scanned: true,
            health: edit::health::HealthReport::default(),
        }
    }

    fn asset_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cap-editor-tests");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// 随包 ffmpeg 生成**带 AAC 音轨**的 2 秒素材（波形那条链必须有音轨才会起线程）。
    /// 返回 `None` 表示环境不具备 → 调用方跳过而不是失败。
    fn gen_mp4_with_audio(tag: &str) -> Option<PathBuf> {
        let ff = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
            .join("bin")
            .join("ffmpeg.exe");
        if !ff.is_file() {
            println!("[skip] 找不到随包 ffmpeg：{}", ff.display());
            return None;
        }
        let out = asset_dir().join(format!("editor-wave-audio-{tag}.mp4"));
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
            println!(
                "[skip] 随包 ffmpeg 生成带音轨素材失败：{}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
    }

    /// 随包 ffmpeg 生成 2 秒 320x240@30fps 素材。
    /// 返回 `None` 表示环境不具备（缺 ffmpeg / 缺 libopenh264）→ 调用方跳过而不是失败。
    ///
    /// `tag` 区分文件名：cargo 默认并发跑同一个二进制里的测试，共用素材路径会互相删文件。
    fn gen_mp4(tag: &str) -> Option<PathBuf> {
        let ff = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
            .join("bin")
            .join("ffmpeg.exe");
        if !ff.is_file() {
            println!("[skip] 找不到随包 ffmpeg：{}", ff.display());
            return None;
        }
        let out = asset_dir().join(format!("editor-open-src-{tag}.mp4"));
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
            println!(
                "[skip] 随包 ffmpeg 生成测试素材失败：{}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
    }

    // ---- 无窗口驱动整个剪辑页：验证"点击才提交、预览只看蓝色标尺"这条链 ----

    /// 无窗口（headless）测试台：建页面、预填缩略图缓存、按固定步长推进帧时间。
    ///
    /// 为什么每帧都要推进 `RawInput::time`：egui 用它判断"双击"（默认 0.3 s / 6 px 内）。
    /// 帧时间不动的话，测试里相邻两次点击会被误判成双击，于是"双击 = 设为起点"
    /// 会把断言搅乱。0.5 s 的步长稳稳定在双击窗口之外。
    struct Harness {
        ctx: egui::Context,
        page: EditorPage,
        screen: egui::Rect,
        duration_ms: i64,
        time: f64,
    }

    impl Harness {
        fn new(clip: Clip) -> Self {
            Self::with_screen(clip, 1024.0, 768.0)
        }

        /// 指定"窗口"尺寸的测试台：流式预览的断言要能改窗口高度/宽度（见下面几个 `headless_*` 测试）。
        fn with_screen(clip: Clip, w: f32, h: f32) -> Self {
            let ctx = egui::Context::default();
            // 不加载字体：无窗口环境只需要布局与交互，不需要字形
            ctx.set_fonts(egui::FontDefinitions::empty());
            let mut page = EditorPage::new(&Config {
                last_video_path: None,
                ..Config::default()
            });
            // **P19：胶片条默认不排解码任务**（`strip_requests_enabled = false`）。
            // 测试台不碰真实素材文件、也不起解码线程，帧间布局因此完全确定；
            // 要测"胶片条真的按可见区间排请求"的用例自己把它打开（见 `p19_*`）。
            page.strip_requests_enabled = false;
            // 播放同理：**不建解码线程**。测试台里的素材是合成出来的（`src` 指向不存在的文件），
            // 真起线程只会得到"打开失败"；而"时钟推进 / 翻页 / 停止"这几件事本来就是纯逻辑，
            // 关掉解码反而让它们完全确定（见 `PlayState` 的说明）。
            page.play_decode_enabled = false;
            // 预填缩略图缓存：`request_thumb` 走"已缓存"的快路径，
            // 测试里就不会真的起解码线程去碰不存在的文件（帧间布局因此完全稳定）。
            // key = `(关键帧时间点, 解码尺寸档)`：这里统一用**最大那个档位**填，
            // 预览要别的档位时 `cached_thumb` 会退回"同一时间点的其它档位"，
            // 因此显示内容与档位无关（测试只关心布局与尺寸）。
            for t in clip.keyframes_ms.clone() {
                let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x40u8; 16]);
                let tex = ctx.load_texture(format!("t{t}"), img, egui::TextureOptions::NEAREST);
                // 合成素材宽 320 → `thumb_bucket_w` 会把任何目标宽度夹到 320（不超过源宽），
                // 所以这一档就是界面真正会用的那一档：缓存必命中 → 测试里**永远不会**排解码任务。
                page.kf_thumbs
                    .insert((t, THUMB_MIN_DECODE_W), CachedThumb { w: 2, h: 2, tex });
            }
            // **P19**：把胶片条的**取样时刻**也预填好（键 = `(量化时刻, STRIP_TILE_W, 行高, 口径版本)`
            // —— 与生产同一个 [`strip_thumb_key`] 入口）。
            // 这样默认路径下"每个画面的尺寸/位置/块数"都是可断言的（`strip_tiles` 有真几何），
            // 而不会因为"纹理还没解出来"退化成占位块。
            page.prefill_strip(&ctx, &clip);
            let duration_ms = clip.duration_ms;
            page.clip = Some(clip);
            // 模拟 `open()` 的初值：当前标尺停在**整片中间**。
            // 这里不建解码线程、也不碰文件，保证帧间布局完全稳定；
            // `open()` 本体那条链由 `open_loads_clip_into_page_state`（真实媒体）覆盖。
            page.cursor_ms = duration_ms / 2;
            Self {
                ctx,
                page,
                screen: egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h)),
                duration_ms,
                time: 0.0,
            }
        }

        /// 无窗口（headless）跑一帧 `EditorPage::ui`，喂进合成的输入事件。
        fn frame(&mut self, events: Vec<egui::Event>) {
            let _ = self.frame_capture(events);
        }

        /// 同上，但把这一帧**真正交给绘制器的图形**（`FullOutput.shapes`）还回来。
        ///
        /// 为什么需要它：像"位置行不许再画小方块"这种要求，光看 `test_rects`
        /// （那是**我们自己的**变量）是证不出来的 —— 必须去看 egui 实际收到的矩形。
        /// 返回的是展平后的 `Shape`（`Shape::Vec` 递归拆开），断言写起来最直白。
        fn frame_capture(&mut self, events: Vec<egui::Event>) -> Vec<egui::Shape> {
            let Self {
                ctx,
                page,
                screen,
                time,
                ..
            } = self;
            let mut input = egui::RawInput {
                screen_rect: Some(*screen),
                ..Default::default()
            };
            input.time = Some(*time);
            *time += 0.5;
            input.events = events;
            let mut out = ctx.run_ui(input, |ui| page.ui(ui, &crate::i18n::I18n::new("zh-CN")));
            // `TexturesDelta` 在 drop 时会对"没被应用"这件事 `debug_assert`，
            // 所以先按官方口径清掉（`drop_without_applying_deltas` 做的就是这一句）。
            out.textures_delta.clear();

            fn flatten(s: egui::Shape, into: &mut Vec<egui::Shape>) {
                match s {
                    egui::Shape::Vec(v) => {
                        for s in v {
                            flatten(s, into);
                        }
                    }
                    other => into.push(other),
                }
            }
            let mut all = Vec::new();
            for cs in out.shapes {
                flatten(cs.shape, &mut all);
            }
            all
        }

        /// 只移动、不点击（纯悬停：按最终语义**不驱动预览**，只用于验证"悬停不改任何提交值"）。
        fn hover(&mut self, p: egui::Pos2) {
            self.frame(vec![egui::Event::PointerMoved(p)]);
        }

        fn button_event(p: egui::Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }

        /// 键盘按下 / 抬起（`pressed = false` 是抬起；空格这类"切换键"必须成对发，
        /// 否则 egui 会认为它一直按着，第二次 press 会被当成 repeat）
        fn key_event(key: egui::Key, pressed: bool) -> egui::Event {
            egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }
        }

        /// 完整敲一次键：按下（本帧）→ 抬起（下一帧）
        fn tap_key(&mut self, key: egui::Key) {
            self.frame(vec![Self::key_event(key, true)]);
            self.frame(vec![Self::key_event(key, false)]);
        }

        /// 帧内按下/松开（同一帧完成，命中测试仍用上一帧登记的矩形）
        fn press(&mut self, key: egui::Key, mods: egui::Modifiers, at: egui::Pos2) {
            self.frame(vec![
                egui::Event::ModifiersChanged(mods),
                egui::Event::PointerMoved(at),
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: mods,
                },
            ]);
        }

        /// 完整的一次左键点击：按下、松开各一帧
        fn click(&mut self, p: egui::Pos2) {
            self.frame(vec![
                egui::Event::PointerMoved(p),
                Self::button_event(p, true),
            ]);
            self.frame(vec![
                egui::Event::PointerMoved(p),
                Self::button_event(p, false),
            ]);
        }

        /// 一次**按住拖动**：在 `from` 按下，移动到 `to`，最后松开（每步一帧）。
        fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) {
            self.frame(vec![
                egui::Event::PointerMoved(from),
                Self::button_event(from, true),
            ]);
            let mid = egui::pos2((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
            self.frame(vec![
                egui::Event::PointerMoved(mid),
                Self::button_event(mid, true),
            ]);
            self.frame(vec![
                egui::Event::PointerMoved(to),
                Self::button_event(to, true),
            ]);
            self.frame(vec![
                egui::Event::PointerMoved(to),
                Self::button_event(to, false),
            ]);
        }

        /// **真实鼠标序列**的拖动：按下只发**一次**，移动帧只发 `PointerMoved`。
        ///
        /// 为什么不能直接用 [`Self::drag`]：它每帧都重发 `PointerButton{pressed:true}`，
        /// egui 的拖动状态会被搅乱 —— 位置行这种"按下后立刻读 `drag_delta()`"的控件
        /// 在 `drag()` 下**根本收不到拖动**，于是单测只能看到"松手那一下的 click"，
        /// 方向写反了也测不出来（P30 就踩过：真机上色带朝反方向跑，单测却是绿的）。
        fn drag_like_a_mouse(&mut self, from: egui::Pos2, to: egui::Pos2) {
            self.frame(vec![
                egui::Event::PointerMoved(from),
                Self::button_event(from, true),
            ]);
            const STEPS: usize = 3;
            for i in 1..=STEPS {
                let t = i as f32 / STEPS as f32;
                let p = egui::pos2(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
                self.frame(vec![egui::Event::PointerMoved(p)]);
            }
            self.frame(vec![Self::button_event(to, false)]);
        }

        fn timeline(&self) -> egui::Rect {
            self.page.test_rects.timeline.expect("应登记时间线矩形")
        }

        /// "整片铺满"下 时间(ms) → 时间线内的 x 像素（与界面用的换算同源）。
        ///
        /// **P14 二次修订后它也是位置行的落点算法**：位置行走的是"全片比例"映射，
        /// 而位置行与时间线**同宽同左边界**，于是"整片铺满的比例"恰好就是那条映射。
        fn x_at(&self, t_ms: f64) -> f32 {
            let tl = self.timeline();
            (tl.left() as f64 + (t_ms / self.duration_ms as f64) * tl.width() as f64) as f32
        }

        /// 时间(ms) 在某个子区域里的落点
        fn pos_at(&self, t_ms: f64, area: egui::Rect) -> egui::Pos2 {
            egui::pos2(self.x_at(t_ms), area.center().y)
        }
    }

    // ---- 最小窗口尺寸的**实测**（见 `crate::ui::layout`）----
    //
    // 这一组量的是**绝对尺寸**，所以必须装"真字体 + 真主题"：
    // `Harness::with_screen` 故意不装字体（只跑交互与相对尺寸），拿它量绝对高度是不作数的
    // —— 没有汉字字形时，中文会被当成缺字，行高/行宽都不是真机上的值。

    /// 装**和真机同一套**字体：内置中文子集（含界面全部汉字）+ egui 自带拉丁/符号字体。
    fn install_measure_fonts(ctx: &egui::Context) {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            crate::ui::startup::SUBSET_FONT_NAME.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(
                crate::ui::startup::SUBSET_FONT_BYTES,
            )),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, crate::ui::startup::SUBSET_FONT_NAME.to_owned());
        ctx.set_fonts(fonts);
    }

    /// 16:9 的合成素材（320×180）：真机录像绝大多数是屏幕比例 16:9，
    /// 所以"最小尺寸下放得下"的断言用它（`synthetic_clip` 是 4:3，另有用处）。
    fn synthetic_clip_169() -> Clip {
        let mut c = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        if let Some(v) = c.video.as_mut() {
            v.width = 320;
            v.height = 180;
        }
        c
    }

    /// 在 `w × h` 的窗口里量一帧剪辑页（真字体 + 真主题 + 指定语言）的结果。
    struct Measured {
        /// 内容高度 / 视口高度 / 可用高度
        probe: LayoutProbe,
        /// 预览画框（真实渲染出来的矩形）
        preview: egui::Rect,
        /// 导出按钮矩形（必须完整落在页面宽度内）
        export_btn: egui::Rect,
        /// 预览可用高度 / 可用宽度（`fluid_preview_size` 的两个输入）
        avail_h: f32,
        avail_w: f32,
        /// 片段芯片矩形（P39 标签守门测试用：标签在不在第一个芯片左边、同不同行）
        seg_chips: Vec<egui::Rect>,
        /// 「片段列表」标签矩形（有片段时才画；`None` = 本帧没片段）
        seg_label: Option<egui::Rect>,
    }

    /// 量一帧剪辑页（真字体 + 真主题 + 指定语言）。
    ///
    /// 语言只影响文字，而文字影响换行 —— 所以"必需高度"要两种内置语言各量一遍取大者。
    fn measure_editor_with(clip: Clip, w: f32, h: f32, lang: &str) -> Measured {
        measure_editor_impl(clip, w, h, lang, &[])
    }

    /// [`measure_editor_with`] 的完整版：可以预置**片段列表**。
    ///
    /// 为什么要这个：片段列表是条件渲染的（空列表不占高度），于是"最小窗口够不够"
    /// 有两种状态要量 —— 刚打开文件（列表空）和用户加满 4 段（列表占满）。
    /// 后者才是真正的高度需求上界，`MIN_INNER_H` 要按它来定，否则用户一加区间就出滚动条。
    fn measure_editor_impl(
        clip: Clip,
        w: f32,
        h: f32,
        lang: &str,
        segments: &[(i64, i64)],
    ) -> Measured {
        let ctx = egui::Context::default();
        install_measure_fonts(&ctx);
        crate::ui::theme::apply(&ctx);
        let mut page = EditorPage::new(&Config {
            last_video_path: None,
            ..Config::default()
        });
        // P19：量尺寸的测试台同样不排胶片条的解码任务（不碰文件、布局完全确定）
        page.strip_requests_enabled = false;
        for t in clip.keyframes_ms.clone() {
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x40u8; 16]);
            let tex = ctx.load_texture(format!("mt{t}"), img, egui::TextureOptions::NEAREST);
            page.kf_thumbs
                .insert((t, THUMB_MIN_DECODE_W), CachedThumb { w: 2, h: 2, tex });
        }
        page.clip = Some(clip);
        // 预置片段列表（空 = 单段模式，与真机刚打开文件时一致）
        page.segments = segments.to_vec();
        page.active_seg = if segments.is_empty() { None } else { Some(0) };
        // 测量取**更宽的那个状态**：用户点过时间线之后，这一行会显示
        // "当前标尺 00:00:05.000 … 将吸附到 00:00:05.000 / 00:00:10.000"，
        // 比"未选点"那一版更长 —— 最小宽度要按最长的排法算。
        page.cursor_committed = true;
        let i18n = crate::i18n::I18n::new(lang);
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h));
        let mut time = 0.0;
        // 两帧：第 1 帧建立布局，第 2 帧的读数才是稳定的（与其它 `headless_*` 同一口径）
        for _ in 0..2 {
            let mut input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            input.time = Some(time);
            time += 0.5;
            ctx.run_ui(input, |ui| page.ui(ui, &i18n))
                .drop_without_applying_deltas();
        }
        let t = &page.test_rects;
        Measured {
            probe: page.layout_probe(),
            preview: t.preview_frame.expect("应画出预览画框"),
            export_btn: t.export_btn.expect("应画出导出按钮"),
            avail_h: t.preview_avail_h.expect("应记录预览可用高度"),
            avail_w: t.preview_avail_w.expect("应记录预览可用宽度"),
            seg_chips: t.seg_chips.clone(),
            seg_label: t.seg_label,
        }
    }

    /// 量一帧剪辑页（默认 16:9 素材 + 中文界面）。
    fn measure_editor(w: f32, h: f32) -> Measured {
        measure_editor_with(synthetic_clip_169(), w, h, "zh-CN")
    }

    /// **剪辑页必需高度 ≤ 最小窗口给它的高度**（用户要求里剪辑页那一半）。
    ///
    /// * 必需高度怎么来的：把窗口压到 400（预览必然触底取 200），读 `ScrollArea` 的
    ///   `content_size` —— 那就是"再矮 1px 就得滚动"的真实需求，**不是估算**；
    /// * 它还必须是**与窗口高度无关**的常量（预览已经触底，再矮只会溢出、不会变矮）；
    /// * 最小尺寸下再看一次：内容高度 ≤ 视口高度 → **不需要纵向滚动**（滚动条恒不显示）。
    #[test]
    fn headless_editor_required_height_fits_the_min_window() {
        use crate::ui::layout::{CHROME_H, EDITOR_REQ_H, MIN_INNER_H, MIN_INNER_W};
        let page_avail_at_min = MIN_INNER_H - CHROME_H;

        // ① 必需高度：两种内置语言的**较大者**（英文/中文的换行点不同，哪个高按哪个算）
        let mut req = 0.0f32;
        for lang in ["zh-CN", "en-US"] {
            let m = measure_editor_with(synthetic_clip_169(), MIN_INNER_W, 340.0, lang);
            assert_eq!(
                m.preview.height(),
                PREVIEW_MIN_H,
                "[{lang}] 窗口压到 340 时预览必须触底取 {PREVIEW_MIN_H}（实测 {}）",
                m.preview.height()
            );
            assert!(
                m.avail_h < PREVIEW_MIN_H,
                "[{lang}] 这个窗口高度必须让预览的可用高度不足下限（实测 {:.1}），\
                 否则量到的不是「必需高度」而是「当前窗口能给的」",
                m.avail_h
            );
            // 与窗口高度无关：换个也"触底"的窗口高度，必需高度一模一样
            let m2 = measure_editor_with(synthetic_clip_169(), MIN_INNER_W, 400.0, lang);
            assert!(
                (m.probe.required_page_h() - m2.probe.required_page_h()).abs() <= 0.5,
                "[{lang}] 必需高度必须与窗口高度无关（两帧都触底）：{:.1} vs {:.1}",
                m.probe.required_page_h(),
                m2.probe.required_page_h()
            );
            println!(
                "[layout] 剪辑页必需高度（{lang}，宽 {MIN_INNER_W}）= {:.1} \
                 （滚动区内容 {:.1} + 页内底部两行 {:.1}；预览 {:.1}，触底时可用高度 {:.1}）",
                m.probe.required_page_h(),
                m.probe.content_h,
                m.probe.page_avail_h - m.probe.viewport_h,
                m.preview.height(),
                m.avail_h
            );
            req = req.max(m.probe.required_page_h());
        }
        // 常量必须就是实测值：改布局让它变高 → 这条立刻失败，而不是"窗口悄悄出现滚动条"。
        // （这里量的是**页面**必需高度；"外壳 + 预览下限差值"那一份由
        //  `ui::layout::MIN_INNER_H` 与真机单测 `main.rs::tests::headless_min_window_*` 负责。）
        assert!(
            (req - EDITOR_REQ_H).abs() <= 0.5,
            "实测剪辑页必需高度 {req:.1} 与常量 EDITOR_REQ_H={EDITOR_REQ_H} 不一致"
        );
        assert!(
            req <= page_avail_at_min + 0.5,
            "剪辑页必需高度 {req:.1} 超过最小窗口给它的 {page_avail_at_min:.1}"
        );

        // ② 最小尺寸下：内容刚好放得下 → 不需要纵向滚动；预览 ≥ 200；且吃满剩余高度
        // 顺带证明"最小宽度已经够宽"：再宽也不会变矮（= 没有行在换行）
        let at_min = measure_editor(MIN_INNER_W, page_avail_at_min);
        let wide = measure_editor(1600.0, page_avail_at_min);
        assert!(
            (wide.probe.content_h - at_min.probe.content_h).abs() <= 0.5,
            "最小宽度下剪辑页仍有行在换行：宽 {MIN_INNER_W} → {:.1}，宽 1600 → {:.1}",
            at_min.probe.content_h,
            wide.probe.content_h
        );
        for lang in ["zh-CN", "en-US"] {
            let m = measure_editor_with(synthetic_clip_169(), MIN_INNER_W, page_avail_at_min, lang);
            assert!(
                m.probe.no_vertical_scroll(),
                "[{lang}] 最小窗口下剪辑页内容不得溢出滚动区：内容 {:.1} > 视口 {:.1}",
                m.probe.content_h,
                m.probe.viewport_h
            );
            assert!(
                m.preview.height() >= PREVIEW_MIN_H,
                "[{lang}] 最小窗口下预览必须 ≥ {PREVIEW_MIN_H}（实测 {}）",
                m.preview.height()
            );
            assert!(
                (m.preview.height() - m.avail_h).abs() <= 1.0,
                "[{lang}] 最小窗口下预览应吃满剩余高度（可用 {:.1}，实测 {:.1}）",
                m.avail_h,
                m.preview.height()
            );
            // 导出按钮必须完整落在窗口宽度内（**不能被流式预览挤出视野**）
            assert!(
                m.export_btn.left() >= -0.5 && m.export_btn.right() <= MIN_INNER_W + 0.5,
                "[{lang}] 最小窗口下导出按钮 {:?} 超出页宽 {MIN_INNER_W}",
                m.export_btn
            );
            println!(
                "[layout] 最小窗口({MIN_INNER_W}×{MIN_INNER_H})下剪辑页：内容 {:.1} / 视口 {:.1} / \
                 预览 {:.1}（可用 {:.1}×{:.1}）/ 导出按钮右边界 {:.1}",
                m.probe.content_h,
                m.probe.viewport_h,
                m.preview.height(),
                m.avail_w,
                m.avail_h,
                m.export_btn.right()
            );
        }

        // ③ 极宽画幅（21:9 / 32:9）在最小宽度下也不能把「导出」按钮挤出视野
        for (w, h) in [(2560i32, 1080i32), (3840, 1080)] {
            let mut clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
            if let Some(v) = clip.video.as_mut() {
                v.width = w;
                v.height = h;
            }
            let m = measure_editor_with(clip, MIN_INNER_W, page_avail_at_min, "zh-CN");
            assert!(
                m.export_btn.left() >= -0.5 && m.export_btn.right() <= MIN_INNER_W + 0.5,
                "{w}×{h}（{:.2}:1）在最小宽度下把导出按钮挤出了视野：{:?}（预览 {:.1}×{:.1}）",
                w as f32 / h as f32,
                m.export_btn,
                m.preview.width(),
                m.preview.height()
            );
            println!(
                "[layout] {w}×{h}（{:.2}:1）：预览 {:.1}×{:.1}，导出按钮右边界 {:.1}（页宽 {MIN_INNER_W}）",
                w as f32 / h as f32,
                m.preview.width(),
                m.preview.height(),
                m.export_btn.right()
            );
        }
    }

    // ---- 流式预览（本轮改动）：尺寸规则 / 充满剩余空间 / 底部两行紧邻 ----
    //
    // 断言用的都是**本帧真实渲染出来的矩形**（`test_rects.*`），不是"再算一遍公式"：
    // 公式的单测在下面 `fluid_preview_size_rules`，这里保证"界面真的按公式画了"。

    /// 合成素材的源宽高比（`synthetic_clip` 是 320×240 → 4:3）
    const SYNTH_ASPECT: f32 = 320.0 / 240.0;

    /// **片段芯片行在场时，最小窗口依然放得下**（P36）。
    ///
    /// 芯片行是条件渲染的（没有片段时不占高度），所以"最小窗口够不够"有两个状态：
    /// ① 刚打开文件、还没有片段 —— 由 `headless_editor_required_height_fits_the_min_window`
    ///    钉住；② 用户加了片段、芯片行出现 —— 由本测试钉住。
    ///
    /// **②才是高度需求的上界**：`MIN_INNER_H` 必须按它来定，否则用户一加片段就出
    /// 滚动条（那正是用户之前抱怨过的现象）。这条测试的价值在于"如果有人把芯片行
    /// 做得更高（比如改回竖向列表），这里立刻失败，而不是让滚动条悄悄出现"。
    #[test]
    fn headless_editor_fits_the_min_window_with_segment_chips() {
        use crate::ui::layout::{CHROME_H, MIN_INNER_H, MIN_INNER_W};
        let page_avail_at_min = MIN_INNER_H - CHROME_H;
        // 4 个芯片：在最小宽度下正好排成一行
        let segs = [(0, 2_000), (2_000, 4_000), (4_000, 6_000), (6_000, 10_000)];
        for lang in ["zh-CN", "en-US"] {
            let m = measure_editor_impl(
                synthetic_clip_169(),
                MIN_INNER_W,
                page_avail_at_min,
                lang,
                &segs,
            );
            assert!(
                m.probe.no_vertical_scroll(),
                "[{lang}] 片段芯片行出现时最小窗口出现纵向滚动：内容 {:.1} > 视口 {:.1}\
                 （差 {:.1}）—— 调大 MIN_INNER_H 的 SEGMENT_LIST_SLACK",
                m.probe.content_h,
                m.probe.viewport_h,
                m.probe.content_h - m.probe.viewport_h
            );
            assert!(
                m.preview.height() >= PREVIEW_MIN_H,
                "[{lang}] 芯片行占位后预览不得低于 {PREVIEW_MIN_H}（实测 {}）",
                m.preview.height()
            );
            println!(
                "[layout] 最小窗口 + 片段芯片行（{lang}）：内容 {:.1} / 视口 {:.1} / 预览 {:.1}",
                m.probe.content_h,
                m.probe.viewport_h,
                m.preview.height()
            );
        }
    }

    /// **P39「片段列表」标签的守门测试**（用户要求"在片段前面加'片段列表'"）：
    /// 标签必须 ① 存在、② 在第一个芯片**左边**、③ 与芯片**同一行**。
    ///
    /// 为什么单测它：标签是 `horizontal_wrapped` 流里的第一项，一旦文案变长
    /// （英文 "Clip list" 比中文长、或以后改成"片段列表（可拖拽调序）"），
    /// 芯片就可能被挤到第二行 —— **页面高度 +23px**，而 `SEGMENT_LIST_SLACK`
    /// 只按单行算，最小窗口立刻出滚动条。这条断言把"标签长短有没有挤爆这一行"钉住。
    #[test]
    fn segment_label_sits_before_the_first_chip() {
        use crate::ui::layout::{CHROME_H, MIN_INNER_H, MIN_INNER_W};
        let page_avail_at_min = MIN_INNER_H - CHROME_H;
        let segs = [(0, 2_000), (2_000, 4_000), (4_000, 6_000), (6_000, 10_000)];
        for lang in ["zh-CN", "en-US"] {
            let m = measure_editor_impl(
                synthetic_clip_169(),
                MIN_INNER_W,
                page_avail_at_min,
                lang,
                &segs,
            );
            let label = m.seg_label.expect("有片段时必须画出「片段列表」标签");
            assert_eq!(m.seg_chips.len(), segs.len(), "[{lang}] 芯片数不对");
            // ① 标签在第一个芯片左边；② 同一行（标签垂直中心落在第一个芯片的上下缘之间）
            let first = m.seg_chips[0];
            assert!(
                label.right() <= first.left() + 0.5,
                "[{lang}] 「片段列表」标签应在第一个芯片左边：标签右缘 {:.1} > 芯片左缘 {:.1}",
                label.right(),
                first.left()
            );
            assert!(
                label.center().y >= first.top() - 0.5 && label.center().y <= first.bottom() + 0.5,
                "[{lang}] 「片段列表」标签与芯片不同行（被挤到第二行？）：标签 y {:.1}，芯片 {:.1}..{:.1}",
                label.center().y,
                first.top(),
                first.bottom()
            );
            // ③ 所有芯片同一行（换行会顶高页面，`SEGMENT_LIST_SLACK` 只认单行）
            for (i, c) in m.seg_chips.iter().enumerate().skip(1) {
                assert!(
                    (c.top() - first.top()).abs() <= 0.5,
                    "[{lang}] 第 {i} 个芯片换行了：top {:.1} vs 第一个 {:.1}",
                    c.top(),
                    first.top()
                );
            }
        }
    }

    /// **P41 守门测试**（用户要求"去掉红色方框内按钮的颜色"，见 2c.png 红框）：
    /// 六颗按钮 —— 设入点 / 设出点 / 添加片段 / 删除片段 / 播放 / 导出 ——
    /// **一律中性外观**：静息底色 = 主题的 `widgets.inactive.weak_bg_fill`（= 卡片/白底），
    /// **不允许**再出现语义色（绿 `OK_FG` / 绿实心 `OK_SOLID` / 红 `DANGER` /
    /// 蓝 `ACCENT` / 琥珀 `WARN_FG`）。
    ///
    /// 为什么量"这一帧真正交给绘制器的图形"而不是看源码：颜色是**绘制期**才定的事，
    /// 源码里没有 `.fill(..)` 也可能被上游样式改掉；只有 `FullOutput.shapes` 里的
    /// `Shape::Rect` 能证明"画出来的确实是中性底"。（P17 的位置行方块就是这么钉住的。）
    ///
    /// 两个必须做对的准备（否则量到的是别的东西）：
    /// ① **先给测试台应用主题** —— `Harness` 默认不吃 CapLite 主题（egui 自带一套），
    ///    而本测试断言的正是主题里的中性色值；
    /// ② **把六颗按钮全部置于"可用"状态** —— `Ui::disable()` 会给整块 `multiply_opacity(0.5)`，
    ///    禁用态量到的是半透明色（`#F1F3F680` 这种），没法跟静息色直接比。
    #[test]
    fn p41_red_box_buttons_have_no_semantic_fill() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        crate::ui::theme::apply(&h.ctx);
        // 六颗按钮全部可用：点过时间线（设入点/设出点）+ 有片段（添加/删除片段）
        h.page.cursor_committed = true;
        h.page.segments = vec![(0, 2_000), (2_000, 4_000)];
        h.page.active_seg = Some(0);
        h.frame(vec![]);
        let shapes = h.frame_capture(vec![]);
        let t = &h.page.test_rects;
        // 中性底（全部来自主题的控件样式，不是语义色）：静息白底 / 悬停浅灰
        let neutral = [
            crate::ui::theme::card_fill(),
            egui::Color32::from_rgb(0xF2, 0xF4, 0xF7),
        ];
        let semantic = [
            crate::ui::theme::ok_fg(),
            crate::ui::theme::ok_solid(),
            crate::ui::theme::ok_solid_hover(),
            crate::ui::theme::danger(),
            crate::ui::theme::danger_hover(),
            crate::ui::theme::accent(),
            crate::ui::theme::accent_hover(),
            crate::ui::theme::warn_fg(),
        ];
        // 六颗按钮的矩形（缺一个都算失败：说明按钮被挪走 / 改名 / 条件渲染没了）
        let names: [(&str, egui::Rect); 6] = [
            ("设入点", t.set_start_btn.expect("应登记设入点按钮")),
            ("设出点", t.set_end_btn.expect("应登记设出点按钮")),
            ("添加片段", t.add_seg_btn.expect("应登记添加片段按钮")),
            ("删除片段", t.del_seg_btn.expect("应登记删除片段按钮")),
            ("播放", t.play_btn.expect("应登记播放按钮")),
            ("导出", t.export_btn.expect("应登记导出按钮")),
        ];
        for (name, want) in names {
            // 按钮可能有"外壳（egui Frame）+ 我们自己画的底"两层矩形，也可能有选中/悬停层；
            // 这里把所有**覆盖整颗按钮**的填充都收上来，一并断言。
            let mut fills: Vec<egui::Color32> = Vec::new();
            for s in &shapes {
                if let egui::Shape::Rect(r) = s {
                    let inside = r.rect.center().x >= want.left() - 1.0
                        && r.rect.center().x <= want.right() + 1.0
                        && r.rect.center().y >= want.top() - 1.0
                        && r.rect.center().y <= want.bottom() + 1.0;
                    let covers = r.rect.width() >= want.width() - 2.0
                        && r.rect.height() >= want.height() - 2.0;
                    if inside && covers {
                        fills.push(r.fill);
                    }
                }
            }
            assert!(
                !fills.is_empty(),
                "[{name}] 本帧没画出覆盖按钮矩形的填充块（按钮没了？）"
            );
            for fill in &fills {
                assert!(
                    !semantic.contains(fill),
                    "[{name}] 按钮底色仍是语义色 {fill:?} —— P41 要求去掉红框内按钮的颜色"
                );
            }
            assert!(
                fills.iter().any(|f| neutral.contains(f)),
                "[{name}] 按钮底色里没有中性控件色（实测 {fills:?}，应为 {neutral:?} 之一）"
            );
        }
    }

    /// 文字形状的**屏幕矩形**：逐行 `PlacedRow::rect()` 求并集，再平移 `TextShape::pos`。
    ///
    /// 为什么不直接用 `galley.rect`：`ctx.run_ui` 交出来的 `Shape::Text` **还没过分帧化**，
    /// `galley.rect` 是空的（零宽零高），做不了宽度断言。逐行取逻辑矩形是分帧化之前
    /// 唯一可靠的尺寸来源。
    fn text_rect(t: &egui::epaint::TextShape) -> egui::Rect {
        let mut acc: Option<egui::Rect> = None;
        for row in &t.galley.rows {
            let r = row.rect().translate(t.pos.to_vec2());
            acc = Some(match acc {
                Some(prev) => prev.union(r),
                None => r,
            });
        }
        acc.unwrap_or_else(|| egui::Rect::from_min_size(t.pos, egui::Vec2::ZERO))
    }

    /// 把本帧所有 `Shape::Text` 收成 `(文案, 屏幕矩形)` 列表。
    ///
    /// 为什么直接读 galley：位置/文案是**这一帧真正交给绘制器**的东西（与 P41 同一种口径），
    /// 比"源码里有没有这行 label"更硬 —— 后者挡不住"被条件渲染/被裁剪掉"。
    fn text_shapes(shapes: &[egui::Shape]) -> Vec<(String, egui::Rect)> {
        let mut v = Vec::new();
        for s in shapes {
            if let egui::Shape::Text(t) = s {
                v.push((t.galley.job.text.clone(), text_rect(t)));
            }
        }
        v
    }

    /// **P40 守门测试（剪辑页那句"拖拽可以改变片段顺序"）**：
    /// 用户要求在「片段 1/3」后面加这句提示，所以这里断言三件事 ——
    /// ① 有片段时它**真的被画出来**（不是只改了语言包）；
    /// ② 它紧跟计数**右侧、同一行**（"后面"不是"另起一行"）；
    /// ③ 片段为空时它一起消失（与计数器同一个条件块，不给这一行引入条件性部件）。
    #[test]
    fn p40_drag_hint_follows_the_segment_counter() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        crate::ui::theme::apply(&h.ctx);
        h.page.segments = vec![(0, 2_000), (2_000, 4_000)];
        h.page.active_seg = Some(0);
        let shapes = h.frame_capture(vec![]);
        let texts = text_shapes(&shapes);
        let counter = texts
            .iter()
            .find(|(s, _)| s.contains("片段 1/2"))
            .map(|(_, r)| *r)
            .expect("有片段时应画出「片段 1/2」计数");
        let (_, hint) = texts
            .iter()
            .find(|(s, _)| s.contains("拖拽可以改变片段顺序"))
            .expect("计数后面应画出「拖拽可以改变片段顺序」提示（P40）");
        assert!(
            hint.left() >= counter.right() - 0.5,
            "提示应在计数右侧：计数右缘 {:.1}，提示左缘 {:.1}",
            counter.right(),
            hint.left()
        );
        assert!(
            hint.center().y >= counter.top() - 0.5 && hint.center().y <= counter.bottom() + 0.5,
            "提示应与计数同一行（被挤到第二行就违背「跟在后面」）：计数 y {:.1}..{:.1}，提示中心 y {:.1}",
            counter.top(),
            counter.bottom(),
            hint.center().y
        );
        // ②b 最小宽度下也不能换行：这一行是 `horizontal_wrapped`，宽度不够会把提示
        //     挤到第二行 → 剪辑页内容变高 → `MIN_INNER_H` 不够用（`layout` 的换行阈值
        //     就是这么盯的）。这里用手工台在最小宽度附近再量一次。
        let mut narrow = Harness::with_screen(
            synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0),
            760.0,
            768.0,
        );
        crate::ui::theme::apply(&narrow.ctx);
        narrow.page.segments = vec![(0, 2_000), (2_000, 4_000)];
        narrow.page.active_seg = Some(0);
        let shapes = narrow.frame_capture(vec![]);
        let texts = text_shapes(&shapes);
        let n_counter = texts
            .iter()
            .find(|(t, _)| t.contains("片段 1/2"))
            .map(|(_, r)| *r)
            .expect("窄宽度下也应画出计数");
        let (_, n_hint) = texts
            .iter()
            .find(|(t, _)| t.contains("拖拽可以改变片段顺序"))
            .expect("窄宽度下也应画出提示");
        assert!(
            (n_hint.center().y - n_counter.center().y).abs() <= 0.5,
            "窄宽度（760）下提示换到了第二行：计数中心 y {:.1}，提示中心 y {:.1}",
            n_counter.center().y,
            n_hint.center().y
        );

        // 没有片段：计数与提示都不出现
        h.page.segments.clear();
        let shapes = h.frame_capture(vec![]);
        let texts = text_shapes(&shapes);
        assert!(
            !texts.iter().any(|(s, _)| s.contains("片段 1/2")),
            "片段为空时不应再画计数"
        );
        assert!(
            !texts
                .iter()
                .any(|(s, _)| s.contains("拖拽可以改变片段顺序")),
            "片段为空时提示应一起消失（与计数器同一个条件块）"
        );
    }

    /// **P40 守门测试（时间线那套 `tl()` 取色）**：
    /// 深色不是"把浅色那套抄一遍"，每个角色都要有**自己的深色值**；
    /// 且画在深色轨道上的那几个（波形 / 非当前段 / 编号）必须比浅色**更亮**，
    /// 否则"提亮一档"这件事就等于没做。
    #[test]
    fn p40_timeline_colors_flip_with_dark_mode() {
        fn lum(c: egui::Color32) -> f32 {
            let lin = |v: u8| {
                let x = v as f32 / 255.0;
                if x <= 0.04045 {
                    x / 12.92
                } else {
                    ((x + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
        }
        let ctx = egui::Context::default();
        crate::ui::theme::apply_kind(&ctx, crate::ui::theme::ThemeKind::Light);
        let light = [
            pos_track_color(),
            pos_band_color(),
            cursor_color(),
            start_color(),
            end_color(),
            seg_inactive_fill(),
            seg_inactive_edge(),
            seg_active_fill(),
            seg_other_fill(),
            seg_hover_fill(),
            wave_color(),
            wave_pending(),
            bad_hard_color(),
            bad_soft_color(),
            seg_number_color(),
        ];
        crate::ui::theme::apply_kind(&ctx, crate::ui::theme::ThemeKind::Dark);
        let dark = [
            pos_track_color(),
            pos_band_color(),
            cursor_color(),
            start_color(),
            end_color(),
            seg_inactive_fill(),
            seg_inactive_edge(),
            seg_active_fill(),
            seg_other_fill(),
            seg_hover_fill(),
            wave_color(),
            wave_pending(),
            bad_hard_color(),
            bad_soft_color(),
            seg_number_color(),
        ];
        let names = [
            "pos_track_color",
            "pos_band_color",
            "cursor_color",
            "start_color",
            "end_color",
            "seg_inactive_fill",
            "seg_inactive_edge",
            "seg_active_fill",
            "seg_other_fill",
            "seg_hover_fill",
            "wave_color",
            "wave_pending",
            "bad_hard_color",
            "bad_soft_color",
            "seg_number_color",
        ];
        for (i, name) in names.iter().enumerate() {
            assert_ne!(
                light[i], dark[i],
                "{name} 在深浅两套主题下取值相同 —— 深色那套是白改的"
            );
        }
        // 画在深色轨道上的内容必须**提亮**：这几个在浅色下是"中深色压浅灰底"，
        // 深色下若沿用原值等于看不见。
        for name in [
            "wave_color",
            "bad_hard_color",
            "bad_soft_color",
            "seg_number_color",
        ] {
            let i = names.iter().position(|n| *n == name).unwrap();
            assert!(
                lum(dark[i]) > lum(light[i]),
                "{name} 深色应更亮（浅 {:.3} → 深 {:.3}）",
                lum(light[i]),
                lum(dark[i])
            );
        }
        // 还原：线程局部主题只在本线程有效，但仍保持严谨
        crate::ui::theme::apply_kind(&ctx, crate::ui::theme::ThemeKind::Light);
    }

    /// 预览画框（本帧真实渲染出来的）
    fn preview_rect(h: &Harness) -> egui::Rect {
        h.page.test_rects.preview_frame.expect("本帧应画出预览画框")
    }

    /// 比例断言：实测比例与源比例相差不超过 `tol`（相对误差）
    fn assert_aspect(rect: egui::Rect, aspect: f32, tol: f32, what: &str) {
        let measured = rect.width() / rect.height();
        let err = (measured - aspect).abs() / aspect;
        assert!(
            err <= tol,
            "{what}：实测 {:.1}×{:.1} → 比例 {measured:.4}，源比例 {aspect:.4}，误差 {:.2}% 超过 {:.0}%",
            rect.width(),
            rect.height(),
            err * 100.0,
            tol * 100.0
        );
    }

    /// **以高度为准 + 单调**：三个窗口高度 → 预览随之变高（严格单调）、高度 ≥ 200、
    /// 比例严格等于源比例（容差 1%），并且**恰好吃掉"时间线以下"的全部剩余高度**。
    ///
    /// 窗口宽度也跟着放大：宽度足够时预览才会被"高度"决定；
    /// 固定 1024 宽时 1400 高的预览会先撞到宽度上限（那一条由下面的收敛测试覆盖）。
    #[test]
    fn headless_fluid_preview_follows_window_height() {
        let mut heights = Vec::new();
        for (w, h) in [(1400.0f32, 600.0f32), (1600.0, 900.0), (1800.0, 1400.0)] {
            let mut hh = Harness::with_screen(
                synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0),
                w,
                h,
            );
            for _ in 0..2 {
                hh.frame(vec![]);
            }
            let rect = preview_rect(&hh);
            let avail_h = hh.page.test_rects.preview_avail_h.expect("应记录可用高度");
            assert!(
                rect.height() >= PREVIEW_MIN_H,
                "窗口 {w}×{h}：预览高 {:.0} 不得低于下限 {PREVIEW_MIN_H}",
                rect.height()
            );
            assert_aspect(rect, SYNTH_ASPECT, 0.01, &format!("窗口 {w}×{h}"));
            // "剩余空间全部交给预览窗充满"：高度没有被宽度限住时，画框高度 == 可用高度
            assert!(
                (rect.height() - avail_h).abs() <= 1.0,
                "窗口 {w}×{h}：预览高 {:.0} 应吃满可用高度 {avail_h:.0}",
                rect.height()
            );
            heights.push((h, rect.height()));
        }
        for pair in heights.windows(2) {
            assert!(
                pair[1].1 > pair[0].1,
                "窗口越高预览应越高（单调）：{:?} → {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    /// **高度下限 200**：窗口矮到"可用高度 159px"时预览仍是 200 高（宁可出现滚动条），
    /// 比例照旧；同时底部两行仍然完整可见（不会被裁掉）。
    #[test]
    fn headless_preview_respects_the_200px_floor() {
        let mut hh = Harness::with_screen(
            synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0),
            1024.0,
            400.0,
        );
        for _ in 0..2 {
            hh.frame(vec![]);
        }
        let rect = preview_rect(&hh);
        let avail_h = hh.page.test_rects.preview_avail_h.expect("应记录可用高度");
        assert!(
            avail_h < PREVIEW_MIN_H,
            "这个窗口高度下可用高度应不足下限（实测 {avail_h:.0}）"
        );
        assert_eq!(rect.height(), PREVIEW_MIN_H, "可用高度不足时预览取高度下限");
        assert_aspect(rect, SYNTH_ASPECT, 0.01, "高度下限");
        // 底部两行仍然在页面里（面板先占高度，所以矮窗口也不会把它们挤没）
        let help = hh.page.test_rects.help.expect("操作指南行始终渲染");
        assert!(
            help.top() < 400.0 && help.height() > 0.0,
            "操作指南行应完整可见"
        );
    }

    /// **宽度受限时按宽度收敛**：窗口很宽但很矮（可用高度 659）→ 预览先按宽度收敛，
    /// 收敛后的高度仍然 ≥ 200，所以**接受**收敛（此时不再吃满高度），比例照旧成立。
    #[test]
    fn headless_preview_converges_on_width_when_needed() {
        for h in [900.0f32, 1400.0] {
            let mut hh = Harness::with_screen(
                synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0),
                1024.0,
                h,
            );
            for _ in 0..2 {
                hh.frame(vec![]);
            }
            let rect = preview_rect(&hh);
            let max_w = hh.page.test_rects.preview_avail_w.expect("应记录可用宽度");
            let avail_h = hh.page.test_rects.preview_avail_h.expect("应记录可用高度");
            assert!(
                (rect.width() - max_w).abs() <= 1.0,
                "窗口高 {h}：宽度受限时预览宽应收到可用宽度 {max_w:.0}（实测 {:.0}）",
                rect.width()
            );
            assert!(
                rect.height() < avail_h,
                "窗口高 {h}：按宽度收敛后高度（{:.0}）必然小于可用高度（{avail_h:.0}）",
                rect.height()
            );
            assert!(rect.height() >= PREVIEW_MIN_H, "收敛后高度仍不得低于下限");
            assert_aspect(rect, SYNTH_ASPECT, 0.01, &format!("窗口高 {h}"));
        }
    }

    /// **宽度收敛不能把高度压到下限以下**：`fluid_preview_size` 的纯逻辑边界。
    #[test]
    fn fluid_preview_size_rules() {
        let aspect = SYNTH_ASPECT; // 4:3
                                   // ① 可用高度 120（< 200）→ 高度取下限 200，宽度由比例推出（允许横向裁切）
        let s = fluid_preview_size(120.0, 1000.0, aspect, PREVIEW_MIN_H);
        assert_eq!(s.y, PREVIEW_MIN_H, "可用高度 120 → 预览高取 200");
        assert!(
            (s.x - PREVIEW_MIN_H * aspect).abs() <= 1.0,
            "宽度按比例推出：{s:?}"
        );
        assert_aspect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), s),
            aspect,
            0.01,
            "下限",
        );

        // ② 高度为准：够高就用满，宽度 = 高 × 比例
        let s = fluid_preview_size(600.0, 5000.0, aspect, PREVIEW_MIN_H);
        assert_eq!((s.x, s.y), (800.0, 600.0));

        // ③ 宽度受限：收到 804 宽 → 高 = 804 / (4/3) = 603 ≥ 200 → 接受收敛
        let s = fluid_preview_size(1159.0, 804.0, aspect, PREVIEW_MIN_H);
        assert_eq!((s.x, s.y), (804.0, 603.0));
        assert_aspect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), s),
            aspect,
            0.01,
            "宽收敛",
        );

        // ④ 宽度收敛会把高度压到**下限以下** → **保高度下限**（宽度允许超出可用宽度）。
        //    注意口径：这里的"保下限"是指高度**不被宽度收敛带走**（仍取 max(可用高度, 200)），
        //    而不是"把高度改成 200"。所以可用高度 120 → 高 200、宽 267 > 240。
        let s = fluid_preview_size(120.0, 240.0, aspect, PREVIEW_MIN_H); // 240/(4/3) = 180 < 200
        assert_eq!(s.y, PREVIEW_MIN_H);
        assert_eq!(s.x, 267.0, "宽度由高度下限推出");
        assert!(
            s.x > 240.0,
            "此时宽度允许超出可用宽度（横向裁切/滚动）：{s:?}"
        );
        assert_aspect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), s),
            aspect,
            0.01,
            "保下限",
        );

        // ⑤ 可用高度很高但可用宽度很窄：**保持高度为准**（不因为宽度受限就把高度砍到 200）
        let s = fluid_preview_size(1000.0, 240.0, aspect, PREVIEW_MIN_H);
        assert_eq!(s.y, 1000.0, "高度仍以可用高度为准");
        assert!(s.x > 240.0, "宽度超出可用宽度（横向裁切/滚动）：{s:?}");
        assert_aspect(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), s),
            aspect,
            0.01,
            "高宽冲突",
        );
        // 16:9 同理
        let s = fluid_preview_size(1000.0, 300.0, 16.0 / 9.0, PREVIEW_MIN_H);
        assert_eq!(s.y, 1000.0, "300/(16/9)=168.75 < 200 → 不收敛，保高度");

        // ⑥ 非法输入不 panic、不产生 0/NaN，且高度永远 ≥ 下限
        for (avail, max_w, asp) in [
            (f32::NAN, 800.0f32, 1.5f32),
            (500.0, f32::NAN, 1.5),
            (500.0, 800.0, f32::NAN),
            (f32::NEG_INFINITY, 0.0, 0.0),
        ] {
            let s = fluid_preview_size(avail, max_w, asp, PREVIEW_MIN_H);
            assert!(
                s.x.is_finite() && s.y.is_finite() && s.x > 0.0 && s.y > 0.0,
                "{s:?}"
            );
            assert!(s.y >= PREVIEW_MIN_H);
        }
    }

    /// **预览下方固定内容的扣除**：剩余高度减掉固定内容；剩余不足/非法时不为负。
    #[test]
    fn preview_avail_h_subtracts_the_fixed_rows() {
        assert_eq!(preview_avail_h(500.0, 21.0), 479.0);
        assert_eq!(preview_avail_h(10.0, 21.0), 0.0, "不够也不能是负数");
        assert_eq!(preview_avail_h(f32::NAN, 21.0), 0.0);
        assert_eq!(preview_avail_h(500.0, -5.0), 500.0, "负数按 0 处理");
    }

    /// **解码档位量化**：目标宽度按 64px 向上取整并夹住 → 相邻 1px 变化落在同一档，
    /// 因此**不会**为拖动窗口重新解码；换档才需要重解（缓存 key 含档位）。
    #[test]
    fn thumb_bucket_quantizes_the_decode_size() {
        // ① 同一档：目标宽度差 1px 时档位不变（只要没跨过 64 的倍数）→ 缓存 key 不变 → 不重解。
        //    这里刻意避开恰好落在 64 倍数上的值（那是"换档点"，由 ② 覆盖）。
        for x in [320.5f32, 321.0, 383.0, 400.0, 640.5, 1000.25, 1919.5] {
            assert_eq!(
                thumb_bucket_w(x, 4096),
                thumb_bucket_w(x + 1.0, 4096),
                "目标宽度 {x} → {x}+1 应落在同一档"
            );
        }
        assert_eq!(thumb_bucket_w(320.0, 4096), 320);
        assert_eq!(thumb_bucket_w(321.0, 4096), 384, "向上取整到 64 的倍数");
        assert_eq!(thumb_bucket_w(385.0, 4096), 448);
        // ② 换档：跨过 64 的倍数才会重新解码
        assert_ne!(thumb_bucket_w(384.0, 4096), thumb_bucket_w(385.0, 4096));
        // ③ 夹到上下限，并且**不超过源宽**（比源还大的解码纯浪费）
        assert_eq!(thumb_bucket_w(10.0, 4096), THUMB_MIN_DECODE_W);
        assert_eq!(thumb_bucket_w(9999.0, 4096), THUMB_MAX_DECODE_W);
        assert_eq!(thumb_bucket_w(9999.0, 320), 320);
        assert_eq!(thumb_bucket_w(9999.0, 200), 200);
        // ④ 非法/非有限输入不 panic：拿不到目标宽度时退回**最小档**
        assert_eq!(thumb_bucket_w(f32::NAN, 4096), THUMB_MIN_DECODE_W);
        assert_eq!(thumb_bucket_w(f32::INFINITY, 0), THUMB_MIN_DECODE_W);
        assert_eq!(thumb_bucket_w(0.0, 0), THUMB_MIN_DECODE_W);
        assert_eq!(thumb_bucket_w(-100.0, 0), THUMB_MIN_DECODE_W);
        // ⑤ 解码尺寸保持源比例（不会把画面压扁）
        let (w, h) = decode_size(640, 16.0 / 9.0);
        assert_eq!((w, h), (640, 360));
        let (w, h) = decode_size(640, 4.0 / 3.0);
        assert_eq!((w, h), (640, 480));
        assert_eq!(
            decode_size(0, 4.0 / 3.0).1,
            1,
            "0 尺寸也不产生 0（egui 会画出畸形矩形）"
        );
    }

    /// **拖动窗口 1px 不重新解码**（页面级）：解码档位不变 → 缓存命中 → 不排新任务。
    /// 同时预览确实跟着变了 1px（"窗口变 → 预览变"与"不重解"两件事同时成立）。
    #[test]
    fn headless_one_pixel_resize_keeps_the_cached_thumb() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        let mut hh = Harness::with_screen(clip, 1024.0, 700.0);
        for _ in 0..2 {
            hh.frame(vec![]);
        }
        let before = preview_rect(&hh);
        let bucket_before = hh.page.test_rects.preview_bucket.expect("应记录解码档位");
        let cache_before = hh.page.kf_thumbs.len();
        assert!(
            hh.page.thumb_pending.is_none(),
            "预填过缓存的档位不该再排解码任务"
        );

        // 窗口高 +1px
        hh.screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1024.0, 701.0));
        for _ in 0..2 {
            hh.frame(vec![]);
        }
        let after = preview_rect(&hh);
        assert!(
            after.height() > before.height(),
            "窗口高 +1px → 预览应跟着变高（{:.1} → {:.1}）",
            before.height(),
            after.height()
        );
        assert_eq!(
            hh.page.test_rects.preview_bucket.expect("应记录解码档位"),
            bucket_before,
            "1px 的变化不得跨解码档位"
        );
        assert!(
            hh.page.thumb_pending.is_none(),
            "档位没变 → 不得重新解码（拖动窗口不该反复解帧）"
        );
        assert_eq!(
            hh.page.kf_thumbs.len(),
            cache_before,
            "档位没变 → 缓存不应增长"
        );
    }

    /// 本帧的预览是否"被宽度限住"（宽 = 可用宽度，高 < 可用高度）
    fn rect_is_width_limited(t: &TestRects) -> bool {
        let (Some(preview), Some(max_w), Some(avail_h)) =
            (t.preview_frame, t.preview_avail_w, t.preview_avail_h)
        else {
            return false;
        };
        (preview.width() - max_w).abs() <= 1.0 && preview.height() < avail_h - 1.0
    }

    /// **文件信息与操作指南紧邻**，且**预览不越过文件信息行的顶部**（顺序：预览 → 文件信息 → 指南）。
    #[test]
    fn headless_file_info_and_help_are_adjacent_below_the_preview() {
        for (w, h) in [(1040.0f32, 740.0f32), (1024.0, 1400.0), (1024.0, 400.0)] {
            let mut hh = Harness::with_screen(
                synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0),
                w,
                h,
            );
            for _ in 0..2 {
                hh.frame(vec![]);
            }
            let t = &hh.page.test_rects;
            let preview = t.preview_frame.expect("应画出预览");
            let info = t.file_info.expect("应画出文件信息行");
            let help = t.help.expect("应画出操作指南行");

            // 紧邻：两行之间的缝隙只有面板自身的 1~2px 内边距（这里要求 < 4px）
            let gap = help.top() - info.bottom();
            assert!(
                gap.abs() < 4.0,
                "窗口 {w}×{h}：文件信息行与操作指南行必须紧邻（缝隙 {gap:.1}px）"
            );
            // 顺序：预览 → 文件信息 → 指南（预览底部不越过文件信息行顶部）。
            // 例外：可用高度连下限都不够时预览保 200 高，内容整体溢出滚动区（规格允许"出现滚动"），
            // 此时预览底会伸到底部两行下面 —— 但底部两行是 `Panel::bottom`，**仍然完整可见**。
            let avail_h = t.preview_avail_h.expect("应记录可用高度");
            if avail_h >= PREVIEW_MIN_H {
                assert!(
                    preview.bottom() <= info.top() + 0.5,
                    "窗口 {w}×{h}：预览底 {:.1} 不得越过文件信息行顶 {:.1}",
                    preview.bottom(),
                    info.top()
                );
            } else {
                assert!(
                    help.top() >= 0.0 && help.bottom() <= h + 0.5,
                    "窗口 {w}×{h}：溢出滚动时底部两行仍要完整可见（{help:?}）"
                );
            }
            assert!(info.top() < help.top(), "文件信息在指南之上");
            // "充满剩余空间"：**高度为准**时预览与文件信息行之间只允许有"预览那行时间码"。
            // 宽度受限（很宽的大窗口 + 很矮的可用高度）是**规格里明确优先**的例外：
            // 那时预览按宽度收敛，剩下的竖直空间就留在文件信息行上方（不再硬拉高、也不变形）。
            let width_limited = rect_is_width_limited(t);
            if width_limited {
                assert!(
                    preview.height() < avail_h,
                    "窗口 {w}×{h}：按宽度收敛后高度应小于可用高度"
                );
            } else {
                assert!(
                    info.top() - preview.bottom() < 40.0,
                    "窗口 {w}×{h}：预览与文件信息行之间不该留出大段空白（实测 {:.1}px）",
                    info.top() - preview.bottom()
                );
            }
        }
    }

    /// **导出按钮不被挤出视野**：常态在预览右侧、与预览垂直居中；窗口太窄时换行到预览下方，
    /// 两种情况下按钮都必须完整落在页面宽度内。
    #[test]
    fn headless_export_button_stays_visible_beside_the_preview() {
        for (w, h) in [
            (1040.0f32, 740.0f32),
            (1024.0, 1400.0),
            (600.0, 1400.0),
            (420.0, 900.0), // 极窄：按钮换行到预览下方
        ] {
            let mut hh = Harness::with_screen(
                synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0),
                w,
                h,
            );
            for _ in 0..2 {
                hh.frame(vec![]);
            }
            let t = &hh.page.test_rects;
            let preview = t.preview_frame.expect("应画出预览");
            let btn = t.export_btn.expect("应画出导出按钮");
            let play = t.play_btn.expect("应画出播放按钮");
            let code = t.play_timecode.expect("应画出播放按钮上方的时间码");
            assert!(
                btn.left() >= -0.5 && btn.right() <= w + 0.5,
                "窗口 {w}×{h}：导出按钮 {btn:?} 被挤出视野（页面宽 {w}）"
            );
            assert_eq!(
                (btn.width(), btn.height()),
                (EXPORT_BTN_W, crate::ui::theme::H_PRIMARY)
            );
            // **用户口径的版式**：时间码 → 播放（绿）→ 导出，自上而下，宽度一致、左边界对齐
            assert_eq!(
                (play.width(), play.height()),
                (EXPORT_BTN_W, crate::ui::theme::H_PRIMARY)
            );
            assert!(
                (play.left() - btn.left()).abs() < 0.5 && (code.left() - btn.left()).abs() < 0.5,
                "时间码/播放/导出必须左边界对齐（{:.1} / {:.1} / {:.1}）",
                code.left(),
                play.left(),
                btn.left()
            );
            assert!(
                play.bottom() <= btn.top() + 0.5,
                "播放按钮必须在导出按钮**上方**（播放底 {:.1}，导出顶 {:.1}）",
                play.bottom(),
                btn.top()
            );
            assert!(
                code.bottom() <= play.top() + 0.5,
                "时间码必须在播放按钮**上方**（时间码底 {:.1}，播放顶 {:.1}）",
                code.bottom(),
                play.top()
            );
            if btn.top() >= preview.bottom() {
                // 换行：整列在预览（含时间码）下方
                assert!(btn.top() > preview.bottom(), "窄窗口时整列应在预览下方");
                assert!(
                    code.top() > preview.bottom(),
                    "窄窗口时时间码也应在预览下方"
                );
            } else {
                // 常态：整列在预览右侧，且**整列**与预览垂直居中（±1px）
                assert!(
                    btn.left() >= preview.right(),
                    "按钮列应在预览右侧（预览右 {:.1}，列左 {:.1}）",
                    preview.right(),
                    btn.left()
                );
                let col_cy = (code.top() + btn.bottom()) / 2.0;
                let dy = (col_cy - preview.center().y).abs();
                assert!(dy <= 1.0, "按钮列应与预览垂直居中（误差 {dy:.1}px）");
            }
        }
    }

    /// **播放的"翻页"规则**（纯逻辑，用户口径："当前帧走到最右侧时，影片向左翻动，
    /// 让当前帧到最左侧"）。
    #[test]
    fn page_view_start_flips_when_the_cursor_leaves_the_view() {
        // 视口 [0, 2000]：还在视口里 → **一个像素都不动**（这就是"静止一屏"的来源）
        assert_eq!(page_view_start(1_500.0, 0.0, 2_000.0, 8_000.0), None);
        assert_eq!(page_view_start(1_999.0, 0.0, 2_000.0, 8_000.0), None);
        // 正好走到最右 → 翻，且当前帧落在**左边界**
        assert_eq!(
            page_view_start(2_000.0, 0.0, 2_000.0, 8_000.0),
            Some(2_000.0)
        );
        // 越过去（一帧 33 ms 很正常）→ 同样翻到当前帧
        assert_eq!(
            page_view_start(2_600.0, 0.0, 2_000.0, 8_000.0),
            Some(2_600.0)
        );
        // **P30**：当前帧落在视口**左边**（缩放把视口搬走之后）→ 也翻到它，
        // 否则影片条会整屏冻住，直到当前帧自己追上右边界（看起来就是"播放卡了"）
        assert_eq!(
            page_view_start(500.0, 3_000.0, 2_000.0, 8_000.0),
            Some(500.0)
        );
        // 片尾：`max_view` 夹住，视口贴右边界停住（翻不动了）
        assert_eq!(
            page_view_start(9_500.0, 7_000.0, 2_000.0, 8_000.0),
            Some(8_000.0)
        );
        // 已经贴着 `max_view`：不再重复写（避免状态抖动）
        assert_eq!(page_view_start(10_200.0, 8_000.0, 2_000.0, 8_000.0), None);
        // 病态输入一律不动
        assert_eq!(page_view_start(f64::NAN, 0.0, 2_000.0, 8_000.0), None);
        assert_eq!(page_view_start(2_500.0, 0.0, 0.0, 8_000.0), None);
        assert_eq!(page_view_start(2_500.0, 0.0, f64::INFINITY, 8_000.0), None);
    }

    /// **空格切换播放 / 停止**，且播放时时钟真的在走（无解码线程，纯时钟）。
    #[test]
    fn space_toggles_playback_and_the_clock_advances() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        hh.page.cursor_ms = 0;
        hh.frame(vec![]);
        assert!(!hh.page.is_playing(), "初始不在播放");

        // 空格 → 开始播放（起播那一帧位置还在起点，下一帧才开始走）
        hh.frame(vec![Harness::key_event(egui::Key::Space, true)]);
        assert!(hh.page.is_playing(), "空格应开始播放");
        assert_eq!(hh.page.cursor_ms, 0, "刚开始播放时位置就在起点");
        hh.frame(vec![Harness::key_event(egui::Key::Space, false)]);

        // 下一帧：时钟推进（测试台每帧 +0.5 s，`tick_play` 夹到 0.25 s）
        hh.frame(vec![]);
        assert!(
            hh.page.cursor_ms > 0,
            "播放中时钟必须推进（实测 {} ms）",
            hh.page.cursor_ms
        );

        // 再敲空格 → 停止，且位置**停在原地**（不回跳、不归零）
        let stopped_at = hh.page.cursor_ms;
        hh.tap_key(egui::Key::Space);
        assert!(!hh.page.is_playing(), "空格应停止播放");
        assert!(
            hh.page.cursor_ms >= stopped_at,
            "停止后标尺应停在当前播放位置（{} → {}）",
            stopped_at,
            hh.page.cursor_ms
        );
        // 停止之后时钟不再自己走
        let after = hh.page.cursor_ms;
        hh.frame(vec![]);
        hh.frame(vec![]);
        assert_eq!(hh.page.cursor_ms, after, "停止后时钟不得继续推进");
    }

    /// **播到片尾自动停止**，且停在片尾那一格（不会越过时长）。
    #[test]
    fn playback_stops_at_the_end_of_the_clip() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        hh.page.cursor_ms = 9_700;
        hh.frame(vec![]);
        hh.frame(vec![Harness::key_event(egui::Key::Space, true)]);
        assert!(hh.page.is_playing());
        // 每帧 250 ms：两帧就过 10 s
        for _ in 0..4 {
            hh.frame(vec![]);
        }
        assert!(!hh.page.is_playing(), "播到片尾应自动停止");
        assert_eq!(hh.page.cursor_ms, 10_000, "应停在片尾，不得越过时长");
    }

    /// **播放时影片条翻页**：当前帧越过右边界的那一帧，视口整页左移、当前帧落到左边界。
    #[test]
    fn playback_pages_the_strip_when_the_cursor_hits_the_right_edge() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        hh.frame(vec![]);
        // 把视口调到"正好看到 2000 ms"（不铺满，才有翻页可言）
        let tw = hh.page.test_rects.timeline.expect("应画出时间线").width() as f64;
        hh.page.px_per_ms = tw / 2_000.0;
        hh.page.fit_mode = false;
        hh.page.view_start_ms = 0.0;
        hh.page.cursor_ms = 0;
        hh.frame(vec![]);
        assert!(hh.page.view_start_ms.abs() < 0.5, "起播时视口在片头");

        hh.tap_key(egui::Key::Space);
        assert!(hh.page.is_playing());

        // 一帧 250 ms：第 8 帧到 2000 ms（右边界）→ 翻页
        let mut flipped: Option<(f64, i64)> = None;
        for _ in 0..12 {
            hh.frame(vec![]);
            if hh.page.view_start_ms > 0.5 {
                flipped = Some((hh.page.view_start_ms, hh.page.cursor_ms));
                break;
            }
        }
        let (vs, cur) = flipped.expect("当前帧越过右边界后必须翻页（视口一直没动）");
        assert!(
            (vs - cur as f64).abs() <= 1.0,
            "翻页后当前帧应落在**左边界**（视口左端 {vs:.1}，当前帧 {cur} ms）"
        );
        // 翻页之后视口**静止**：下一帧（还没再越过右边界）不许动
        let before = hh.page.view_start_ms;
        hh.frame(vec![]);
        assert_eq!(
            hh.page.view_start_ms, before,
            "翻页之间视口必须静止（不能连续跟随）"
        );
    }

    /// **点绿色播放按钮**同样能起停（与空格同一条路径）。
    #[test]
    fn headless_play_button_starts_and_stops_playback() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        // 两帧之后布局才落定（首帧的预览尺寸还要按字体行高再算一次）
        for _ in 0..2 {
            hh.frame(vec![]);
        }
        let btn = hh.page.test_rects.play_btn.expect("应画出播放按钮");
        hh.click(btn.center());
        assert!(hh.page.is_playing(), "点播放按钮应开始播放");
        hh.frame(vec![]);
        let btn = hh.page.test_rects.play_btn.expect("播放中按钮仍在原位");
        hh.click(btn.center());
        assert!(!hh.page.is_playing(), "再点一次应停止播放");
    }

    /// **拖动滚动条（全片位置行）只滚视口，不动当前帧**（P30 用户口径）：
    /// 「只有在影片上点击鼠标时，才能改变当前帧的位置……拖动滚动条不要改变当前帧」。
    ///
    /// **方向断言写在"拖动中途"**：指针右移 ⇒ 可见区间右移（淡蓝色带跟着手指走）。
    /// 松手那一帧还有"点一下 = 跳到点击处"的语义，只看终值会把方向写反的 bug 掩盖掉
    /// （用户实机报的正是"色带朝反方向跑"）。
    #[test]
    fn dragging_the_pos_row_scrolls_the_view_without_moving_the_current_frame() {
        let mut hh = Harness::new(synthetic_clip(
            60_000,
            vec![0, 10_000, 20_000, 30_000],
            30.0,
        ));
        hh.frame(vec![]);
        // 放大 10 倍：可见时长只剩 6 s，才有可滚的余量
        hh.page.fit_mode = false;
        hh.page.px_per_ms = fit_px_per_ms(1024.0, 60_000) * 10.0;
        hh.page.view_start_ms = 0.0;
        hh.frame(vec![]);

        let cur = hh.page.cursor_ms;
        let vs0 = hh.page.view_start_ms;
        let row = hh.page.test_rects.pos_row.expect("应画出全片位置行");
        let from = row.center();
        let mid = egui::pos2(row.center().x + row.width() * 0.10, row.center().y);

        // 按下 → 右移 10% → 松开（逐帧、按真实输入序列：按下只发一次，
        // 移动帧不再重复发 Pressed —— 重复按下去会把 egui 的拖动状态搅乱）
        hh.frame(vec![
            egui::Event::PointerMoved(from),
            Harness::button_event(from, true),
        ]);
        hh.frame(vec![egui::Event::PointerMoved(mid)]);
        let vs_mid = hh.page.view_start_ms;
        let want = vs0 + 60_000.0 * 0.10;
        assert!(
            vs_mid > vs0 + 1_000.0,
            "拖动方向反了：指针右移，可见区间应跟着右移（{} → {}）",
            vs0,
            vs_mid
        );
        assert!(
            (vs_mid - want).abs() <= 60_000.0 * 0.02,
            "右移 10% 行宽应滚 10% 全片（期望 ≈{want:.0}，实测 {vs_mid:.0}）"
        );
        assert_eq!(hh.page.cursor_ms, cur, "拖动滚动条**不得**改变当前帧");

        hh.frame(vec![Harness::button_event(mid, false)]);
        assert_eq!(hh.page.cursor_ms, cur, "松手也不得改变当前帧");
        assert!(
            !hh.page.cursor_committed,
            "滚动条不是「主动定位标尺」，不该把标尺标成已提交"
        );
        assert!(
            hh.page.view_start_ms >= vs_mid - 1.0,
            "松手不该把刚拖到的位置再跳走（{} → {}）",
            vs_mid,
            hh.page.view_start_ms
        );
        assert!(!hh.page.fit_mode, "滚动过就不再是「整片铺满」");
    }

    /// **缩放影片及拖动滚动条 → 停止播放**（用户口径 2026-10-17 四次修订，
    /// 把 P28 第二轮的"边播边看"**反转回来**）：播放中拖一下滚动条，播放立刻停，
    /// 标尺停在停播那一刻。
    #[test]
    fn dragging_the_pos_row_stops_playback() {
        let mut hh = Harness::new(synthetic_clip(
            60_000,
            vec![0, 10_000, 20_000, 30_000],
            30.0,
        ));
        hh.frame(vec![]);
        // 放大 10 倍：位置行才有可滚的余量（铺满时"碰它"不算改视图，不停播）
        hh.page.fit_mode = false;
        hh.page.px_per_ms = fit_px_per_ms(1024.0, 60_000) * 10.0;
        hh.page.view_start_ms = 0.0;
        hh.frame(vec![]);
        hh.tap_key(egui::Key::Space);
        assert!(hh.page.is_playing());

        let row = hh.page.test_rects.pos_row.expect("应画出全片位置行");
        let from = row.center();
        let to = egui::pos2(row.left() + row.width() * 0.8, row.center().y);
        hh.drag_like_a_mouse(from, to);
        assert!(!hh.page.is_playing(), "拖动滚动条必须**停止播放**");
        // 停播之后时钟不再自己走
        let stopped = hh.page.cursor_ms;
        hh.frame(vec![]);
        assert_eq!(hh.page.cursor_ms, stopped, "停播后时钟不得继续推进");
    }

    /// **点一下滚动条同样停播**（同一条口径：动视口 = 停播）。
    #[test]
    fn clicking_the_pos_row_stops_playback() {
        let mut hh = Harness::new(synthetic_clip(
            60_000,
            vec![0, 10_000, 20_000, 30_000],
            30.0,
        ));
        hh.frame(vec![]);
        hh.page.fit_mode = false;
        hh.page.px_per_ms = fit_px_per_ms(1024.0, 60_000) * 10.0;
        hh.page.view_start_ms = 0.0;
        hh.frame(vec![]);
        hh.tap_key(egui::Key::Space);
        assert!(hh.page.is_playing());

        let row = hh.page.test_rects.pos_row.expect("应画出全片位置行");
        hh.click(egui::pos2(row.left() + row.width() * 0.75, row.center().y));
        assert!(!hh.page.is_playing(), "点滚动条必须**停止播放**");
    }

    /// **缩放影片不动当前帧**（P30 用户口径）：滚轮缩放只改视口，
    /// `cursor_ms` 一个毫秒都不许动 —— 否则用户只是"看一眼放大后的波形"就丢了当前帧。
    #[test]
    fn zooming_the_strip_does_not_move_the_current_frame() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        hh.frame(vec![]);
        let cur = hh.page.cursor_ms;
        let px_before = hh.page.px_per_ms;

        let tl = hh.page.test_rects.timeline.expect("应画出时间线");
        // 指针放在**离当前帧很远**的左端再放大：缩放锚点在那里，视口会挪，
        // 但当前帧的时间值必须原封不动。
        let p = egui::pos2(tl.left() + 20.0, tl.center().y);
        hh.frame(vec![egui::Event::PointerMoved(p)]);
        hh.frame(vec![
            egui::Event::PointerMoved(p),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 240.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        assert!(
            hh.page.px_per_ms > px_before * 1.05,
            "缩放应当真的生效（{} → {}）",
            px_before,
            hh.page.px_per_ms
        );
        assert_eq!(hh.page.cursor_ms, cur, "缩放**不得**改变当前帧");
        assert!(
            !hh.page.cursor_committed,
            "缩放不是定位动作，不该把标尺标成已提交"
        );
    }

    /// **缩放影片条 = 停止播放**（用户口径 2026-10-17 四次修订，
    /// 把 P28 第二轮的"不停播"**反转回来**）：缩放照常生效，播放立刻停，
    /// 标尺停在停播那一刻（缩放本身不改 `cursor_ms`）。
    #[test]
    fn zooming_the_strip_stops_playback() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        hh.frame(vec![]);
        hh.tap_key(egui::Key::Space);
        assert!(hh.page.is_playing());
        let px_before = hh.page.px_per_ms;

        let tl = hh.page.test_rects.timeline.expect("应画出时间线");
        let p = egui::pos2(tl.left() + 20.0, tl.center().y);
        hh.frame(vec![egui::Event::PointerMoved(p)]);
        hh.frame(vec![
            egui::Event::PointerMoved(p),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 240.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        assert!(!hh.page.is_playing(), "缩放影片条必须**停止播放**");
        assert!(
            hh.page.px_per_ms > px_before * 1.05,
            "缩放应当真的生效（{} → {}）",
            px_before,
            hh.page.px_per_ms
        );
        // 停播之后时钟不再自己走
        let stopped = hh.page.cursor_ms;
        hh.frame(vec![]);
        assert_eq!(hh.page.cursor_ms, stopped, "停播后时钟不得继续推进");
    }

    /// 预览缩略图仍然**只看蓝色当前标尺**：换个窗口尺寸不该改变"显示哪一帧"。
    #[test]
    fn headless_fluid_preview_still_shows_the_ruler_keyframe() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        let mut hh = Harness::with_screen(clip.clone(), 1400.0, 600.0);
        hh.frame(vec![]);
        assert_eq!(
            hh.page.preview_ms(),
            Some(4_000),
            "标尺 5000 → 最近关键帧 4000"
        );
        let first = preview_rect(&hh);
        hh.screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1400.0, 900.0));
        hh.frame(vec![]);
        assert_eq!(
            hh.page.preview_ms(),
            Some(4_000),
            "窗口变高只改尺寸，不改「显示哪一帧」"
        );
        assert!(
            preview_rect(&hh).height() > first.height(),
            "尺寸确实跟着变大了"
        );
    }

    /// **用户最终语义**：预览 = **蓝色当前标尺所在位置最近的关键帧**。
    ///
    /// 逐步钉住三件事：
    /// ① `preview_ms()` 就是 `cursor_ms` 最近的关键帧；
    /// ② 指针在时间线上**悬停**（不按键）或**拖动**（画选区，不动蓝色把手）都**不改变预览**；
    /// ③ 点击时间线改变 `cursor_ms` 后，预览**随之变化**。
    #[test]
    fn headless_preview_shows_the_keyframe_at_the_blue_ruler() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        let mut h = Harness::new(clip.clone());
        h.frame(vec![]);

        let tl = h.timeline();
        let y = tl.center().y;
        let a = egui::pos2(h.x_at(2_400.0), y);
        let b = egui::pos2(h.x_at(7_600.0), y);

        // ① 初值：标尺停在整片中间（5000 ms），预览 = 最近关键帧（4000 ms）
        assert_eq!(h.page.cursor_ms, 5_000, "当前标尺初值 = 时长的一半");
        assert_eq!(
            h.page.preview_ms(),
            edit::nearest_keyframe(&clip, h.page.cursor_ms),
            "预览 = 当前标尺最近的关键帧"
        );
        assert_eq!(h.page.preview_ms(), Some(4_000));

        // ② 悬停（只移动指针，不按键）：预览**不变**、提交值不变
        let before = (h.page.cursor_ms, h.page.sel_start_ms, h.page.sel_end_ms);
        h.hover(a);
        h.hover(b);
        assert_eq!(
            h.page.preview_ms(),
            Some(4_000),
            "悬停不驱动预览（用户最终要求：只显示蓝色标尺所在位置的预览）"
        );
        assert_eq!(
            (h.page.cursor_ms, h.page.sel_start_ms, h.page.sel_end_ms),
            before,
            "悬停绝不能改当前标尺/起点/终点"
        );

        // ②b 在时间线上**按住拖动**：本应用的既有语义是"拖轨道空白 = 拉着蓝色标尺走"，
        //     所以标尺（因而预览）会跟着动 —— 关键不变量是：
        //     **预览永远等于"当前标尺最近的那个关键帧"**，而不是指针本身。
        h.drag(a, egui::pos2(b.x, y));
        assert_eq!(
            h.page.preview_ms(),
            edit::nearest_keyframe(&clip, h.page.cursor_ms),
            "拖轨道时预览仍严格等于当前标尺最近的关键帧（预览不是'指针处的画面'）"
        );
        assert_eq!(
            (h.page.sel_start_ms, h.page.sel_end_ms),
            (before.1, before.2),
            "拖轨道空白绝不动起点/终点"
        );

        // ②c 先把标尺放回中间，再**拖起点把手**（选区把手，不是蓝色把手）：
        //     起点变了，但蓝色标尺与预览**一点不动** —— 这正是"拖动别处不改变预览"。
        h.click(egui::pos2(h.x_at(5_000.0), y));
        assert_eq!(h.page.cursor_ms, 5_000);
        // ②c **起点/终点**（选区把手拖动提交的结果）变化时，预览**一点不动**：
        //     这正是"拖动别处不会改变预览"—— 预览只看蓝色标尺。
        let cursor_before = h.page.cursor_ms;
        let preview_before = h.page.preview_ms();
        h.page.sel_start_ms = 1_500;
        h.page.sel_end_ms = 8_500;
        assert_eq!(
            h.page.cursor_ms, cursor_before,
            "改起点/终点不得移动蓝色当前标尺"
        );
        assert_eq!(
            h.page.preview_ms(),
            preview_before,
            "改起点/终点不得改变预览（预览 = 蓝色标尺最近的关键帧）"
        );

        // ③ 点击 B → 当前标尺到 B，预览随之变到 B 最近的关键帧（6000 ms）
        h.click(b);
        assert!(
            (h.page.cursor_ms - 7_600).abs() <= 2,
            "点击后当前标尺应到 B（约 7600 ms），实际 {}",
            h.page.cursor_ms
        );
        assert_eq!(
            h.page.preview_ms(),
            Some(6_000),
            "预览随蓝色标尺变化（7600 ms 最近的关键帧是 6000 ms）"
        );

        // 再点回 A 附近：预览跟着回到 2000 ms —— 且起终点始终没被碰过
        h.click(a);
        assert_eq!(h.page.preview_ms(), Some(2_000));
        assert_eq!(
            (h.page.sel_start_ms, h.page.sel_end_ms),
            (1_500, 8_500),
            "预览/标尺的变化绝不影响起终点（提交口径独立）"
        );
    }

    /// 拖动**蓝色把手**移动标尺时，预览同样跟着走（这条是"标尺变了预览就变"的另一半）。
    #[test]
    fn headless_dragging_the_blue_handle_moves_the_preview() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        let mut h = Harness::new(clip.clone());
        h.frame(vec![]);
        // 先点一下把标尺放到 1000 ms（这一帧的把手矩形下一帧才可用于命中）
        let tl = h.timeline();
        let y = tl.center().y;
        h.click(egui::pos2(h.x_at(1_000.0), y));
        assert_eq!(h.page.cursor_ms, 1_000);
        assert_eq!(h.page.preview_ms(), Some(0), "1000 ms 最近的关键帧是 0");

        // 拖把手：从标尺当前像素位置拖到 5400 ms 处
        let from = egui::pos2(h.x_at(h.page.cursor_ms as f64), y);
        let to = egui::pos2(h.x_at(5_400.0), y);
        h.drag(from, to);
        assert!(
            (h.page.cursor_ms - 5_400).abs() <= 20,
            "拖蓝色把手应把当前标尺带到 5400 ms 附近，实际 {}",
            h.page.cursor_ms
        );
        assert_eq!(
            h.page.preview_ms(),
            edit::nearest_keyframe(&clip, h.page.cursor_ms),
            "预览始终等于当前标尺最近的关键帧"
        );
    }

    /// 严格照用户的操作序列走一遍（这是本测试的重点——只断言"点完立刻取值"会漏掉它）：
    /// ① 移到 A（不点）→ 标尺不变；② 在 A 点击 → 标尺 = A；
    /// ③ 移到很远的 B（不点）→ 标尺仍是 A；④ 移到**按钮上**并点击 → 起点 = 以 A 为准吸附；
    /// ⑤ 对终点再走一遍 ③④（移到 B 再点按钮）。
    #[test]
    fn headless_mark_buttons_read_the_committed_ruler_not_the_last_hover() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        let mut h = Harness::new(clip.clone());
        h.frame(vec![]);

        let y = h.timeline().center().y;
        let a = egui::pos2(h.x_at(2_400.0), y); // A：用户想要的点
        let b = egui::pos2(h.x_at(7_600.0), y); // B：最后悬停的位置（绝不能拿它当选中点）
        let btn_before = h
            .page
            .test_rects
            .set_start_btn
            .expect("按钮始终渲染（未选点时禁用）");

        // ① 只移动不点击：标尺不变，且此时按钮是**禁用**的（默认中间值不算"用户选的点"）
        h.hover(a);
        assert_eq!(h.page.cursor_ms, 5_000, "悬停不得改变当前标尺");
        assert!(!h.page.cursor_committed, "还没点过，按钮应处于禁用态");
        assert!(
            h.page.preview_ms().is_some(),
            "预览始终有值（只看蓝色标尺，不因悬停而变化）"
        );
        // 禁用态下点它 → 什么都不该发生（更不该拿悬停位置顶替）
        h.click(btn_before.center());
        assert_eq!(
            (h.page.sel_start_ms, h.page.sel_end_ms),
            (0, 0),
            "未选点时按「设为起点」不得写入任何值"
        );

        // ② 在 A 点击 → 标尺 = A（这一步之后按钮才可用）
        h.click(a);
        assert!(
            (h.page.cursor_ms - 2_400).abs() <= 2,
            "点击后当前标尺应在 A（约 2400 ms），实际 {}",
            h.page.cursor_ms
        );
        assert!(h.page.cursor_committed);

        // ③ 移到远处的 B（不点击）→ 标尺仍是 A
        h.hover(b);
        assert!(
            (h.page.cursor_ms - 2_400).abs() <= 2,
            "悬停到 B 不得把标尺带走，实际 {}",
            h.page.cursor_ms
        );

        // ④ 把指针移到「设为起点」按钮上（此时 B 是"最后悬停位置"）并点击 → 记的是 A
        h.hover(b);
        let btn = h.page.test_rects.set_start_btn.expect("按钮应登记矩形");
        h.click(btn.center());
        assert!(
            (h.page.sel_start_ms - 2_400).abs() <= 2,
            "「设为起点」必须用当前标尺 A（约 2400 ms），实际 {} ms —— 这就是用户报的 bug",
            h.page.sel_start_ms
        );
        assert_eq!(
            edit::snap_start(&clip, h.page.sel_start_ms),
            4_000,
            "以 A 为准应吸附到 4000"
        );
        assert_ne!(
            edit::snap_start(&clip, h.page.sel_start_ms),
            6_000,
            "绝不能吸附成 6000（那是拿最后悬停位置 B 去吸附的结果）"
        );

        // ⑤ 「设为终点」同样：先移到 B（不点），再点按钮 → 仍然以 A 为准
        h.hover(b);
        let btn_end = h.page.test_rects.set_end_btn.expect("按钮应登记矩形");
        h.click(btn_end.center());
        assert!(
            (h.page.sel_end_ms - 2_400).abs() <= 2,
            "「设为终点」必须用当前标尺 A（约 2400 ms），实际 {} ms",
            h.page.sel_end_ms
        );
        assert_eq!(
            edit::snap_end(&clip, h.page.sel_end_ms),
            2_000,
            "以 A 为准终点应吸附到 2000"
        );
        assert_ne!(
            edit::snap_end(&clip, h.page.sel_end_ms),
            6_000,
            "绝不能吸附成 6000（B 的结果）"
        );
        // 全程 `cursor_ms` 只被"那一次点击"改过：悬停（含移到按钮上）一次都没写它
        assert!(
            (h.page.cursor_ms - 2_400).abs() <= 2,
            "最后当前标尺仍应是 A，实际 {}",
            h.page.cursor_ms
        );
    }

    /// 点击**刻度尺区域**与点击**轨道区域**都能设置当前标尺。
    #[test]
    fn headless_click_ruler_and_track_both_set_cursor() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        let ruler = h.page.test_rects.ruler.expect("应登记刻度尺区域");
        let track = h.page.test_rects.track.expect("应登记轨道区域");
        assert!(ruler.top() >= track.bottom() - 0.5, "刻度尺画在轨道下方");

        h.click(h.pos_at(2_500.0, ruler));
        assert!(
            (h.page.cursor_ms - 2_500).abs() <= 2,
            "点刻度尺应把当前标尺定到 2500 ms，实际 {} ms",
            h.page.cursor_ms
        );
        h.click(h.pos_at(7_500.0, track));
        assert!(
            (h.page.cursor_ms - 7_500).abs() <= 2,
            "点轨道应把当前标尺定到 7500 ms，实际 {} ms",
            h.page.cursor_ms
        );
    }

    // ---- P14 · 全片位置行（时间码行下方新增的那一整行）----

    // ---- P5 · 底部操作指南行必须在最小宽度下单行完整显示 ----

    /// **P5 的防回归**：底部那行操作指南在**最小窗口宽度**下必须
    /// **一行放得下、末尾不出现 `…`**（用户不接受被省略的句子）。
    ///
    /// 两条口径都是**真渲染**来的，不是再算一遍公式：
    /// * `help_avail_w`：那一行拿到的内宽（`Panel` 的内边距已经扣掉）；
    /// * 自然宽度：用**同一套字体/字号** `layout_no_wrap` 量出来；
    /// * 另外再量一次"按可用宽度换行"的高度，确认它**仍然是一行**。
    ///
    /// 中文与英文**各量一遍**：两种语言的行宽差得很远（中文按字计宽、英文按字母），
    /// 只量一种就会出现"中文放得下、英文被 `…`"。
    #[test]
    fn p5_help_row_fits_the_min_width_without_ellipsis() {
        use crate::ui::layout::MIN_INNER_W;
        for lang in ["zh-CN", "en-US"] {
            let ctx = egui::Context::default();
            install_measure_fonts(&ctx);
            crate::ui::theme::apply(&ctx);
            let i18n = crate::i18n::I18n::new(lang);
            let mut page = EditorPage::new(&Config {
                last_video_path: None,
                ..Config::default()
            });
            page.clip = Some(synthetic_clip_169());
            let screen =
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(MIN_INNER_W, 768.0));
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                ctx.run_ui(input, |ui| page.ui(ui, &i18n))
                    .drop_without_applying_deltas();
            }
            let avail = page
                .test_rects
                .help_avail_w
                .expect("应记录操作指南行拿到的内宽");
            let text = i18n.t("editor_help").to_string();
            let (natural_w, wrapped_h, one_line_h, font_h) = {
                let mut out = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                let t = text.clone();
                ctx.run_ui(input, |ui| {
                    let font = egui::FontId::proportional(crate::ui::theme::FS_SMALL);
                    let painter = ui.painter();
                    let g =
                        painter.layout_no_wrap(t.clone(), font.clone(), crate::ui::theme::faint());
                    let wrapped =
                        painter.layout(t.clone(), font.clone(), crate::ui::theme::faint(), avail);
                    out = (g.size().x, wrapped.size().y, g.size().y, g.size().y);
                })
                .drop_without_applying_deltas();
                out
            };
            // ① 不换行：按可用宽度排版后高度仍是"一行"
            assert!(
                (wrapped_h - one_line_h).abs() <= 1.0,
                "[{lang}] 操作指南行在最小宽度 {MIN_INNER_W} 下换行了：\
                 一行高 {one_line_h:.1}，实排 {wrapped_h:.1}（可用宽 {avail:.1}）"
            );
            // ② 不省略：整句的自然宽度必须 ≤ 可用宽度（否则 `truncate()` 会加 `…`）
            assert!(
                natural_w <= avail + 0.5,
                "[{lang}] 操作指南行在最小宽度 {MIN_INNER_W} 下会被 `…` 截断：\
                 整句需要 {natural_w:.1}，只有 {avail:.1}（差 {:.1}；字号 {font_h:.1}）",
                natural_w - avail
            );
            println!(
                "[p5] {lang}: 整句 {natural_w:.1} ≤ 可用 {avail:.1}（余量 {:.1}）；\
                 文本 {} 个字符",
                avail - natural_w,
                text.chars().count()
            );
        }
    }

    /// **滚动条"跳到某处"不变式**（P30；函数本体沿用 P14 的夹取规则）：
    /// 给定"目标时刻 / 可见时长 / 全片时长"，视口左端必须让目标时刻落在**正中央**
    /// （没被夹取时误差 ≤ 1 px），并且在片头/片尾**被正确夹取**
    /// （`view_start ≥ 0`、`view_start + 可见时长 ≤ duration`）。
    ///
    /// 抽成纯函数断言（而不是靠无窗口点击去凑状态）：跟随的输入输出只有
    /// `(目标时刻, 可见时长, 总时长)`，把这几条钉死就够；界面那一路的
    /// "点哪儿滚到哪儿"由 `p30_clicking_the_scrollbar_centers_the_clicked_time` 覆盖。
    ///
    /// **P30 语义变更**：这个函数不再服务于"当前帧居中跟随"（那条已按用户要求删除），
    /// 只服务于"点位置行 = 把可见区间挪到以点击处为中心"。
    #[test]
    fn p30_scrollbar_click_centers_the_clicked_time_in_the_viewport() {
        let dur = 60_000.0f64;
        // 三种可见时长：放大 10 倍（6 s）、放大 2 倍（30 s）、整片铺满（60 s）
        for span in [6_000.0f64, 30_000.0, 60_000.0] {
            let width = 1024.0f64; // 位置行宽度（px）
            let px = width / span; // 1 px 代表多少毫秒
            let max_view = (dur - span).max(0.0);
            for t in [
                0.0f64, 100.0, 1_000.0, 3_000.0, 10_000.0, 30_000.0, 54_000.0, 57_000.0, 59_900.0,
                60_000.0,
            ] {
                let vs = centered_view_start(t, span, dur);
                // ① 视口永远在片内 —— 用户明确要求的两条边界
                assert!(
                    vs >= 0.0,
                    "t={t} span={span}：view_start 不得 < 0（得到 {vs}）"
                );
                assert!(
                    vs + span <= dur + 1e-6,
                    "t={t} span={span}：view_start + 可见时长不得 > duration（得到 {}）",
                    vs + span
                );
                // ② 目标时刻必须可见（居中或贴边都可见）
                assert!(
                    t >= vs - 1e-6 && t <= vs + span + 1e-6,
                    "t={t} span={span} → 视口 [{vs}, {}]：目标时刻不在视口里",
                    vs + span
                );
                // ③ 没被夹取时必须**正中**（容差 ≤ 1 px）
                if vs > 0.0 && vs < max_view {
                    let offset_px = (t - vs) * px;
                    assert!(
                        (offset_px - width / 2.0).abs() <= 1.0,
                        "t={t} span={span}：目标时刻应落在视口正中（实测偏离中心 {:.2} px）",
                        offset_px - width / 2.0
                    );
                }
            }
            // ④ 片头 / 片尾是**精确**贴边（不是"差不多"）
            assert_eq!(
                centered_view_start(0.0, span, dur),
                0.0,
                "片头必须把视口夹在 0（不得为负）"
            );
            assert_eq!(
                centered_view_start(dur, span, dur),
                max_view,
                "片尾必须把视口夹住（view_start = duration − 可见时长）"
            );
        }
        // ⑤ 退化输入不得产出 NaN / 负数（绝不把视口推到片外）
        for (t, span, d) in [
            (f64::NAN, 6_000.0, 60_000.0),
            (1_000.0, f64::INFINITY, 60_000.0),
            (1_000.0, 6_000.0, 0.0),
            (-5_000.0, 6_000.0, 60_000.0),
        ] {
            let vs = centered_view_start(t, span, d);
            assert!(
                vs.is_finite() && vs >= 0.0,
                "退化输入 ({t}, {span}, {d}) → {vs}：必须是有限非负数"
            );
        }
    }

    /// **几何**：位置行在**时间码行下方**、与时间码行**同宽**（左右边界对齐），
    /// 且在时间线矩形**之内**（不能跑到下一行去）。
    ///
    /// 这三条正是用户修订版的要求：「放到下方时间码行会遮挡时间码，也不合适。
    /// 在时间码行下方再设一行吧，与时间码行一样宽。」
    #[test]
    fn p14_pos_row_sits_below_the_ruler_and_matches_its_width() {
        for w in [760.0f32, 1024.0, 1600.0] {
            let mut h =
                Harness::with_screen(synthetic_clip(10_000, vec![0, 2000, 4000], 30.0), w, 768.0);
            h.frame(vec![]);
            let t = &h.page.test_rects;
            let ruler = t.ruler.expect("应登记刻度尺区域");
            let pos = t.pos_row.expect("P14：应登记全片位置行");
            let tl = t.timeline.expect("应登记时间线矩形");

            assert!(
                pos.top() >= ruler.bottom() - 0.5,
                "宽 {w}：位置行必须在时间码行**下方**（ruler 底 {:.1}，位置行顶 {:.1}）",
                ruler.bottom(),
                pos.top()
            );
            assert!(
                (pos.width() - ruler.width()).abs() <= 0.5,
                "宽 {w}：位置行必须与时间码行**同宽**（{:.1} vs {:.1}）",
                pos.width(),
                ruler.width()
            );
            assert!(
                (pos.left() - ruler.left()).abs() <= 0.5
                    && (pos.right() - ruler.right()).abs() <= 0.5,
                "宽 {w}：位置行左右边界必须与时间码行**对齐**"
            );
            assert!(
                pos.bottom() <= tl.bottom() + 0.5 && pos.top() >= tl.top() - 0.5,
                "宽 {w}：位置行必须落在时间线矩形内（不越界、不压到下面的预览）"
            );
            assert!(
                (pos.height() - POS_ROW_H).abs() <= 0.5,
                "宽 {w}：位置行高度应当是 POS_ROW_H = {POS_ROW_H}（实测 {:.1}）",
                pos.height()
            );
            // 时间线整体高度 = 各段之和（算式就是常量定义，这里把它钉成断言）
            assert!(
                (tl.height() - TIMELINE_H).abs() <= 0.5,
                "时间线高度应等于各段之和 {TIMELINE_H}（实测 {:.1}）",
                tl.height()
            );
        }
    }

    /// **整片铺满时位置行没有可滚的余量**（P30）：拖它不动视口、不动当前帧，
    /// 也不破坏"铺满"状态（否则窗口改宽时不再自动铺满）。
    #[test]
    fn p30_dragging_the_scrollbar_in_fit_mode_changes_nothing() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        assert!(h.page.fit_mode, "起始就是整片铺满");
        assert_eq!(h.page.cursor_ms, 5_000, "起始：当前标尺停在整片中间");
        assert!(!h.page.cursor_committed, "还没点击过 → 未提交");

        let pos = h.page.test_rects.pos_row.expect("应登记全片位置行");
        h.drag_like_a_mouse(h.pos_at(2_500.0, pos), h.pos_at(8_000.0, pos));

        assert_eq!(h.page.cursor_ms, 5_000, "拖滚动条不得改变当前帧");
        assert!(!h.page.cursor_committed, "滚动条不是「主动定位」");
        assert_eq!(h.page.view_start_ms, 0.0, "全片都在眼前，视口无处可滚");
        assert!(h.page.fit_mode, "没有可滚余量时不该退出铺满模式");
    }

    /// **P17 ① + P30：小方块已彻底删除，整行现在只滚视口**。
    ///
    /// 这一条同时钉住三件事，任何一件退回旧实现都会红：
    /// 1. **画面上没有小方块**：遍历本帧真交给 egui 的图形，
    ///    位置行区域内不许出现任何"白色填充的矮矩形"（旧可视方块是 36×12 白底 + 描边）；
    /// 2. **整行都是命中区域**：在行**最左 / 正中 / 最右**三处点击都能把可见区间滚到那儿
    ///    （P30：点它**不**改当前帧，只滚视口）；
    /// 3. **纵向整行高都能点**：贴着行上沿、行下沿各点一次也必须生效
    ///    （命中区域 = `POS_ROW_H` 高，不是那个 12 px 高的方块）。
    #[test]
    fn p30_position_row_scrolls_instead_of_seeking() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        let pos = h.page.test_rects.pos_row.expect("应登记全片位置行");

        // ---- 1. 画面里没有方块 ----
        // 先摆一个"最容易被误画成方块"的状态：当前帧停在片尾（旧实现的方块会贴到行右端）
        h.page.cursor_ms = h.duration_ms;
        let shapes = h.frame_capture(vec![]);
        let mut white_boxes_in_row = 0usize;
        for s in &shapes {
            if let egui::Shape::Rect(r) = s {
                let touches_row =
                    r.rect.center().y >= pos.top() - 1.0 && r.rect.center().y <= pos.bottom() + 1.0;
                let looks_like_the_old_block = r.fill == egui::Color32::WHITE
                    && r.stroke.width > 0.0
                    && r.rect.height() <= 14.0;
                if touches_row && looks_like_the_old_block {
                    white_boxes_in_row += 1;
                }
            }
        }
        assert_eq!(
            white_boxes_in_row, 0,
            "P17：位置行里不该再画那个 36×12 白色小方块（实测 {white_boxes_in_row} 个）"
        );

        // 放大 4 倍：可见区间只占全片 1/4，点位置行才有可滚的余量
        h.page.fit_mode = false;
        h.page.px_per_ms = fit_px_per_ms(1024.0, 10_000) * 4.0;
        h.page.view_start_ms = 0.0;
        h.frame(vec![]);
        let visible = 1024.0 / h.page.px_per_ms;
        let max_view = (10_000.0 - visible).max(0.0);
        let cur = h.page.cursor_ms;
        let pos = h.page.test_rects.pos_row.expect("应登记全片位置行");

        // ---- 2. 整行都能点：左端 / 正中 / 右端 → 可见区间滚到那儿（当前帧不动）----
        let dur = h.duration_ms;
        for (frac, want) in [
            (0.0f64, 0.0f64),
            (0.5, dur as f64 / 2.0 - visible / 2.0),
            (0.999, max_view),
        ] {
            let x = if frac >= 0.999 {
                pos.right() - 1.0
            } else {
                pos.left() + (pos.width() as f64 * frac) as f32
            };
            h.click(egui::pos2(x, pos.center().y));
            let got = h.page.view_start_ms;
            assert!(
                (got - want).abs() <= (dur as f64 * 0.01).max(30.0),
                "整行滚动：x 比例 {frac} 处点击应把视口滚到 {want:.0} 附近（实测 {got:.0}）"
            );
            assert_eq!(h.page.cursor_ms, cur, "点滚动条**不得**改变当前帧");
        }

        // ---- 3. 纵向整行高都能点：行上沿 / 行下沿 ----
        let mid_x = h.pos_at(dur as f64 * 3.0 / 4.0, pos).x;
        let want = (dur as f64 * 0.75 - visible / 2.0).clamp(0.0, max_view);
        for y in [pos.top() + 0.5, pos.bottom() - 0.5] {
            h.page.view_start_ms = 0.0;
            h.frame(vec![]);
            h.click(egui::pos2(mid_x, y));
            let got = h.page.view_start_ms;
            assert!(
                (got - want).abs() <= (dur as f64 * 0.02).max(30.0),
                "整行高命中：y={y:.1}（行 {:.1}..{:.1}）点击应生效（期望 ≈{want:.0}，实测 {got:.0}）",
                pos.top(),
                pos.bottom()
            );
            assert_eq!(h.page.cursor_ms, cur, "点滚动条**不得**改变当前帧");
        }
    }

    /// **P17 ⑤：位置行"整行可操作"的几何证据**（替代原先的"方块 36×12 / 命中框 48×16"那条）。
    ///
    /// * 整行高 = [`POS_ROW_H`]，且这一行**就是**交互控件（`Sense::click_and_drag`）；
    /// * 淡蓝色"可见区间"带仍按**全片比例**落位（它是唯一表达"我正看着哪一段"的东西）；
    /// * 各种窗口宽度下都成立（不是某一档布局的巧合）。
    #[test]
    fn p17_position_row_is_the_only_hit_area() {
        for w in [760.0f32, 1024.0, 1600.0] {
            let mut h = Harness::with_screen(
                synthetic_clip(60_000, vec![0, 10_000, 20_000, 30_000], 30.0),
                w,
                768.0,
            );
            h.frame(vec![]);
            let row = h.page.test_rects.pos_row.expect("应登记全片位置行");
            assert!(
                (row.height() - POS_ROW_H).abs() <= 0.5,
                "宽 {w}：位置行高度 = POS_ROW_H = {POS_ROW_H}（实测 {:.1}）",
                row.height()
            );
            const _: () = assert!(POS_ROW_H >= 16.0, "整行高至少要 16 逻辑点：好点好拖");
            // 缩放到一半可见时长：色带应当只占行的一部分（全片比例），且落在行内
            h.page.fit_mode = false;
            h.page.px_per_ms = fit_px_per_ms(w, h.duration_ms) * 2.0;
            h.page.view_start_ms = 0.0;
            h.frame(vec![]);
            let row = h.page.test_rects.pos_row.expect("应登记全片位置行");
            let band_w = pos_row_x_of(
                (row.width() as f64 / h.page.px_per_ms).min(h.duration_ms as f64),
                row.left() as f64,
                row.width() as f64,
                h.duration_ms,
            ) - row.left();
            assert!(
                band_w > 1.0 && band_w <= row.width() + 0.5,
                "宽 {w}：可见区间色带宽度 {band_w:.1} 应落在 (0, 行宽] 之内"
            );
        }
    }

    /// **拖动不自激**（P30；口径沿用 P14 二次修订）：拖完滚动条静置，界面**不再请求重绘**，
    /// 且 `cursor_ms` / `view_start_ms` 都**不再变化** —— 证明"拖动 → 状态变 → 再请求重绘"
    /// 的活锁不存在，也没有周期性心跳
    /// （口径同 `p15_tile_wakes_the_ui_once_without_a_heartbeat`：静置时必须静止，**不新增心跳**）。
    #[test]
    fn p30_dragging_the_scrollbar_does_not_self_excite_repaints() {
        let mut h = Harness::new(synthetic_clip(
            60_000,
            vec![0, 10_000, 20_000, 30_000],
            30.0,
        ));
        h.frame(vec![]);
        // 放大 10 倍：滚动才有"可移动的余量"
        h.page.fit_mode = false;
        h.page.px_per_ms = fit_px_per_ms(1024.0, 60_000) * 10.0;
        h.page.view_start_ms = 0.0;
        h.frame(vec![]);

        /// 连跑 3 帧无输入，返回"其中请求过重绘的帧数"
        fn idle_repaints(h: &mut Harness) -> usize {
            let mut n = 0;
            for _ in 0..3 {
                h.frame(vec![]);
                if h.ctx.has_requested_repaint() {
                    n += 1;
                }
            }
            n
        }
        let before = idle_repaints(&mut h);
        assert_eq!(
            before, 0,
            "前提：没有输入时界面本身就是静止的（实测 {before}/3 帧在请求重绘）"
        );

        // 真拖一段：从全片 20%（12 s）拖到 60%（36 s）—— 视口必然跟着滚
        let pos = h.page.test_rects.pos_row.expect("应登记全片位置行");
        h.drag_like_a_mouse(h.pos_at(12_000.0, pos), h.pos_at(36_000.0, pos));
        assert!(
            h.page.view_start_ms > 0.0,
            "前提：拖动滚动条必须真的移动过视口（实测 view_start = {}）",
            h.page.view_start_ms
        );

        // 松手之后静置：egui 的欠帧要一两帧才排空（口径同 P15），所以先允许它排空，
        // 但**必须停下来**：排空之后再连跑 3 帧，一帧都不许再请求重绘，状态也不许动。
        let cursor = h.page.cursor_ms;
        let vs = h.page.view_start_ms;
        let mut settled = false;
        for _ in 0..3 {
            h.frame(vec![]);
            if !h.ctx.has_requested_repaint() {
                settled = true;
                break;
            }
        }
        assert!(
            settled,
            "拖完静置 3 帧后界面仍在请求重绘 —— 有自激 / 周期性心跳"
        );
        for i in 0..3 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "静置第 {i} 帧又开始请求重绘（拖动 → 状态变 → 再重绘 = 自激/心跳）"
            );
            assert_eq!(
                h.page.cursor_ms, cursor,
                "静置时当前帧不得自己漂移（滚动条不得变成定位/自我滚动）"
            );
            assert_eq!(
                h.page.view_start_ms, vs,
                "静置时视口不得自己漂移（滚动条不得自我滚动）"
            );
        }
    }

    /// **点位置行 = 把可见区间滚到那儿**（P30 的界面侧落点，**覆盖** P14 的"定位当前帧"）。
    ///
    /// 三件事一起钉住：
    /// * 位置行是**全片概览**：`t → x` 与缩放/平移无关（全片正中就落在行中央），
    ///   点击落点按**全片比例**取时刻（而不是像视口映射那样永远落在行中央）；
    /// * 视口把**点击处**居中，且在片头/片尾被夹取在 `[0, duration]` 内；
    /// * **当前帧一动不动**（这是 P30 的核心口径）。
    #[test]
    fn p30_clicking_the_scrollbar_centers_the_clicked_time() {
        let mut h = Harness::new(synthetic_clip(
            60_000,
            vec![0, 10_000, 20_000, 30_000],
            30.0,
        ));
        h.frame(vec![]);
        // 放大 10 倍：可见时长只剩全片的 1/10（6 s）
        h.page.fit_mode = false;
        h.page.px_per_ms = fit_px_per_ms(1024.0, 60_000) * 10.0;
        h.page.view_start_ms = 0.0;
        h.frame(vec![]);
        let visible = 1024.0 / h.page.px_per_ms;
        assert!(visible < 60_000.0, "前提：放大后可见时长必须短于全片");
        let cur = h.page.cursor_ms;

        let pos = h.page.test_rects.pos_row.expect("应登记全片位置行");
        // 位置行是全片映射：全片正中（30 s）必须落在**行中央**（`pos_at` 用的就是这条映射）
        let half = h.pos_at(30_000.0, pos);
        assert!(
            (half.x - pos.center().x).abs() <= 1.0,
            "全片正中应落在位置行中央（实测 {:.1} vs {:.1}）",
            half.x,
            pos.center().x
        );

        // ① 点位置行全片正中：可见区间以 30 s 为中心，当前帧**不动**
        h.click(half);
        assert_eq!(h.page.cursor_ms, cur, "点滚动条**不得**改变当前帧");
        let vs = h.page.view_start_ms;
        let center_ms = vs + visible / 2.0;
        assert!(
            (center_ms - 30_000.0).abs() <= 60.0,
            "可见区间中心应落在点击处 30 s 附近（实测 {center_ms:.0} ms）"
        );
        assert!(
            vs >= 0.0 && vs + visible <= 60_000.0 + 1e-6,
            "视口不得越出片外（[{vs}, {}]）",
            vs + visible
        );

        // ② 片头夹取：点到片头 → 视口贴住 0，绝不出现负的 view_start
        h.click(h.pos_at(0.0, pos));
        assert_eq!(
            h.page.view_start_ms, 0.0,
            "片头必须把视口夹在 0（不得 < 0）"
        );
        assert_eq!(h.page.cursor_ms, cur, "点滚动条**不得**改变当前帧");

        // ③ 远离片尾时**始终**居中：54 s（90%）处视口右端还在片内
        h.click(h.pos_at(54_000.0, pos));
        let center_ms = h.page.view_start_ms + visible / 2.0;
        assert!(
            (center_ms - 54_000.0).abs() <= 60.0,
            "54 s 处点击应把可见区间中心滚到 54 s 附近（实测 {center_ms:.0} ms）"
        );
        assert!(
            h.page.view_start_ms + visible < 60_000.0 - 1.0,
            "前提：54 s 处视口右端还没贴到片尾（否则测不出「中间也居中」）"
        );

        // ④ 片尾夹取：点到 96.7%（58 s）→ 视口右端正好落在 duration 上（不得越出片尾）
        h.click(h.pos_at(58_000.0, pos));
        let vs_tail = h.page.view_start_ms;
        assert!(
            (vs_tail + visible - 60_000.0).abs() <= 1.0,
            "片尾必须把视口夹住（view_start + 可见时长 = duration，实测 {:.1}）",
            vs_tail + visible
        );
        assert_eq!(h.page.cursor_ms, cur, "点滚动条**不得**改变当前帧");
        // **P17/P30**：位置行的落点算法仍是**全片比例**（走视口映射的话 58 s 会贴在行右端）
        let want_x = pos_row_x_of(
            h.page.view_start_ms + visible / 2.0,
            pos.left() as f64,
            pos.width() as f64,
            60_000,
        );
        let by_ratio = pos.left() as f64
            + ((h.page.view_start_ms + visible / 2.0) / 60_000.0) * pos.width() as f64;
        assert!(
            (want_x as f64 - by_ratio).abs() <= 1.5,
            "位置行必须按全片比例落位（期望 x≈{by_ratio:.1}，实测 {want_x:.1}）"
        );
        assert!(
            (want_x - pos.right()).abs() > 20.0,
            "96.7% 处不该贴在行右端（贴住说明位置行还在用视口映射）"
        );
    }

    /// 悬停（只移动指针）**绝不**改变起点/终点；点击时间线也只改当前标尺。
    #[test]
    fn headless_hover_never_touches_selection() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        h.page.sel_start_ms = 1_000;
        h.page.sel_end_ms = 9_000;
        let y = h.timeline().center().y;
        for t in [0.0f64, 400.0, 2_400.0, 5_000.0, 9_999.0] {
            h.hover(egui::pos2(h.x_at(t), y));
            assert_eq!(h.page.sel_start_ms, 1_000, "悬停在 {t} ms 处不得改起点");
            assert_eq!(h.page.sel_end_ms, 9_000, "悬停在 {t} ms 处不得改终点");
        }
        // 点击时间线同样只移动当前标尺
        h.click(egui::pos2(h.x_at(3_000.0), y));
        assert!((h.page.cursor_ms - 3_000).abs() <= 2);
        assert_eq!(
            (h.page.sel_start_ms, h.page.sel_end_ms),
            (1_000, 9_000),
            "点击时间线只移动当前标尺，不动起点/终点"
        );
    }

    /// 拖动**蓝色把手**能移动当前标尺（拖拽本身就是明确的"提交"动作）。
    #[test]
    fn headless_drag_yellow_handle_moves_cursor() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        let handle = h
            .page
            .test_rects
            .cursor_handle
            .expect("默认在可见范围内，应登记蓝色把手");
        assert!(
            (handle.center().x - h.x_at(5_000.0)).abs() <= 1.0,
            "把手画在当前标尺处（默认 = 时长的一半），实际 x={} / 标尺 x={}",
            handle.center().x,
            h.x_at(5_000.0)
        );

        let p1 = handle.center();
        let p2 = egui::pos2(h.x_at(8_000.0), p1.y);
        h.frame(vec![
            egui::Event::PointerMoved(p1),
            Harness::button_event(p1, true),
        ]);
        h.frame(vec![egui::Event::PointerMoved(p2)]);
        h.frame(vec![
            egui::Event::PointerMoved(p2),
            Harness::button_event(p2, false),
        ]);
        assert!(
            (h.page.cursor_ms - 8_000).abs() <= 3,
            "拖动把手应把当前标尺带到 8000 ms，实际 {} ms",
            h.page.cursor_ms
        );
    }

    /// 键盘：`[` / `]` 作用于当前标尺；`←` / `→` 微调（默认一帧、Shift 关键帧、Ctrl 10 ms）；
    /// `Home` / `End` 到片头/片尾。
    #[test]
    fn headless_keyboard_drives_cursor_and_marks_ends() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        let at = egui::pos2(h.x_at(5_000.0), h.timeline().center().y);
        let none = egui::Modifiers::NONE;

        // 指针先落在时间线上，快捷键才具备生效条件
        h.hover(at);
        assert!(h.page.hover_live);

        h.press(egui::Key::ArrowRight, none, at);
        assert_eq!(h.page.cursor_ms, 5_033, "→ 默认走一帧（30 fps ≈ 33 ms）");
        h.press(egui::Key::ArrowLeft, none, at);
        assert_eq!(h.page.cursor_ms, 5_000, "← 走回原处");

        h.press(egui::Key::ArrowRight, egui::Modifiers::CTRL, at);
        assert_eq!(h.page.cursor_ms, 5_010, "Ctrl + → 走 10 ms");
        h.press(egui::Key::ArrowRight, none, at);
        assert_eq!(h.page.cursor_ms, 5_043, "松开 Ctrl 后又回到一帧");

        h.press(egui::Key::ArrowRight, egui::Modifiers::SHIFT, at);
        assert_eq!(h.page.cursor_ms, 6_000, "Shift + → 跳到后一个关键帧");
        // 已在最后一个关键帧之后 → 退回一帧，而不是卡住不动
        h.press(egui::Key::ArrowRight, egui::Modifiers::SHIFT, at);
        assert_eq!(h.page.cursor_ms, 6_033);
        h.press(egui::Key::ArrowLeft, egui::Modifiers::SHIFT, at);
        assert_eq!(h.page.cursor_ms, 6_000, "Shift + ← 跳到前一个关键帧");

        h.press(egui::Key::Home, none, at);
        assert_eq!(h.page.cursor_ms, 0, "Home = 当前标尺到片头");
        h.press(egui::Key::End, none, at);
        assert_eq!(h.page.cursor_ms, 10_000, "End = 当前标尺到片尾");
        h.press(egui::Key::Home, none, at);

        // 微调 → [ 设为起点 → 再微调 → ] 设为终点：都在**当前标尺**上生效
        for _ in 0..3 {
            h.press(egui::Key::ArrowRight, none, at);
        }
        h.press(egui::Key::OpenBracket, none, at);
        assert_eq!(h.page.sel_start_ms, 99, "[ 把当前标尺设为起点");
        h.press(egui::Key::End, none, at);
        h.press(egui::Key::CloseBracket, none, at);
        assert_eq!(h.page.sel_end_ms, 10_000, "] 把当前标尺设为终点");
        assert_eq!(edit::snap_start(&h.page.clip.clone().unwrap(), 99), 2_000);

        // 指针不在时间线上、时间线也没有焦点时，快捷键不生效
        let away = egui::pos2(h.timeline().left() - 50.0, h.timeline().top() - 50.0);
        h.press(egui::Key::Home, none, away);
        assert_eq!(h.page.cursor_ms, 10_000, "鼠标离开时间线后快捷键不再生效");
    }

    /// `nudge_cursor_ms` 的步长规则与边界（纯逻辑，脱界面）。
    #[test]
    fn nudge_step_rules() {
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        let none = egui::Modifiers::NONE;
        // 一帧（30 fps → 33 ms）
        assert_eq!(nudge_cursor_ms(&clip, 1_000, 1, none), 1_033);
        assert_eq!(nudge_cursor_ms(&clip, 1_033, -1, none), 1_000);
        // Ctrl（Mac 上是 Cmd）→ 10 ms
        assert_eq!(
            nudge_cursor_ms(&clip, 1_000, 1, egui::Modifiers::CTRL),
            1_010
        );
        assert_eq!(
            nudge_cursor_ms(&clip, 1_000, -1, egui::Modifiers::CTRL),
            990
        );
        assert_eq!(
            nudge_cursor_ms(&clip, 1_000, 1, egui::Modifiers::COMMAND),
            1_010
        );
        // Shift → 前/后一个关键帧（严格大于/小于，正好落在关键帧上也不会原地不动）
        assert_eq!(
            nudge_cursor_ms(&clip, 2_500, 1, egui::Modifiers::SHIFT),
            4_000
        );
        assert_eq!(
            nudge_cursor_ms(&clip, 2_500, -1, egui::Modifiers::SHIFT),
            2_000
        );
        assert_eq!(
            nudge_cursor_ms(&clip, 2_000, 1, egui::Modifiers::SHIFT),
            4_000
        );
        assert_eq!(nudge_cursor_ms(&clip, 2_000, -1, egui::Modifiers::SHIFT), 0);
        // 已经在头/尾 → 退回一帧；没有关键帧信息时也退回一帧
        assert_eq!(
            nudge_cursor_ms(&clip, 6_000, 1, egui::Modifiers::SHIFT),
            6_033
        );
        assert_eq!(nudge_cursor_ms(&clip, 0, -1, egui::Modifiers::SHIFT), 0);
        let no_kf = synthetic_clip(10_000, vec![], 30.0);
        assert_eq!(
            nudge_cursor_ms(&no_kf, 1_000, 1, egui::Modifiers::SHIFT),
            1_033
        );
        // 结果一律夹到 [0, duration]
        assert_eq!(nudge_cursor_ms(&clip, 10_000, 1, none), 10_000);
        assert_eq!(nudge_cursor_ms(&clip, 0, -1, none), 0);
        assert_eq!(nudge_cursor_ms(&clip, 99_999, 1, none), 10_000);
        assert_eq!(nudge_cursor_ms(&clip, -5, -1, none), 0);
        // 帧率未知 → 按 1/60 秒（≈17 ms）
        let unknown = synthetic_clip(10_000, vec![0, 2000], 0.0);
        assert_eq!(nudge_cursor_ms(&unknown, 1_000, 1, none), 1_017);
        // 极端输入不 panic、不溢出
        let huge = synthetic_clip(i64::MAX, vec![], 30.0);
        assert!(nudge_cursor_ms(&huge, i64::MAX, 1, none) > 0);
        assert!(nudge_cursor_ms(&huge, i64::MIN, -1, none) >= 0);
    }

    /// 点击的"像素 → 时间"换算在**缩放 + 平移**状态下也要对
    /// （"整片铺满"是另一条路径，已由上面的测试覆盖）。
    #[test]
    fn headless_click_maps_correctly_when_zoomed_and_panned() {
        let mut h = Harness::new(synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0));
        h.frame(vec![]);
        let tl = h.timeline();

        // 手动进入"放大 4 倍 + 视图左端 4 s"的状态（与滚轮缩放、拖平移的落点等价）
        h.page.fit_mode = false;
        h.page.px_per_ms *= 4.0;
        h.page.view_start_ms = 4_000.0;
        h.frame(vec![]);
        let px = h.page.px_per_ms;
        let vs = h.page.view_start_ms;

        // 按界面同一套换算算出 5.5 s 落在哪一像素，点它
        let target = 5_500.0f64;
        let x = (tl.left() as f64 + (target - vs) * px) as f32;
        assert!(tl.left() <= x && x <= tl.right(), "目标点应落在可见范围内");
        h.click(egui::pos2(x, tl.center().y));
        assert!(
            (h.page.cursor_ms as f64 - target).abs() <= 2.0,
            "缩放+平移状态下点击 5.5 s 应得到约 5500 ms，实际 {} ms",
            h.page.cursor_ms
        );
        assert!(!h.page.fit_mode, "点一下不该把视图拉回整片铺满");
    }

    /// `pick_recording_dir`：输出目录 → 最近视频的父目录 → 用户视频目录；空路径当作"没有"。
    #[test]
    fn recording_dir_falls_back_in_the_documented_order() {
        let out = Path::new(r"C:\rec\cap");
        let video = Path::new(r"C:\rec\2024-01-01 10-00-00.mp4");
        let videos = Path::new(r"C:\Users\Limit li\Videos\cap");

        // 1. 有输出目录就用它
        assert_eq!(
            pick_recording_dir(Some(out), Some(video), Some(videos)),
            Some(out.to_path_buf())
        );
        // 2. 输出目录为空/没有 → 最近视频所在目录
        assert_eq!(
            pick_recording_dir(None, Some(video), Some(videos)),
            Some(PathBuf::from(r"C:\rec"))
        );
        assert_eq!(
            pick_recording_dir(Some(Path::new("")), Some(video), Some(videos)),
            Some(PathBuf::from(r"C:\rec")),
            "空路径不能被当成当前目录"
        );
        // 3. 都没有 → 用户视频目录
        assert_eq!(
            pick_recording_dir(None, None, Some(videos)),
            Some(videos.to_path_buf())
        );
        assert_eq!(
            pick_recording_dir(Some(Path::new("")), None, Some(videos)),
            Some(videos.to_path_buf())
        );
        // 4. 全都没有 → None（调用方据此不给对话框设起始目录）
        assert_eq!(pick_recording_dir(None, None, None), None);
        // 没有父目录的"最近视频"（例如盘符根）也要能安全跳过
        assert_eq!(
            pick_recording_dir(None, Some(Path::new("C:")), Some(videos)),
            Some(videos.to_path_buf())
        );
    }

    // ---- 时间码：格式化 / 解析 ----

    /// 格式化与解析必须**往返一致**（界面显示什么，用户照着敲回来就得是什么）。
    #[test]
    fn timecode_round_trip() {
        for ms in [
            0i64, 1, 7, 40, 999, 1000, 3400, 59_999, 60_000, 61_234, 3_599_999, 3_600_000,
            86_399_999,
        ] {
            let text = fmt_time(ms);
            assert_eq!(
                parse_timecode_ms(&text),
                Some(ms),
                "{ms} ms 格式化成 {text} 后应能解析回原值"
            );
        }
        // 负值一律按 0 显示，解析 0 也得到 0
        assert_eq!(fmt_time(-5), "00:00:00.000");
        assert_eq!(parse_timecode_ms("00:00:00.000"), Some(0));
        // 具体样例（用户看得见的那种）
        assert_eq!(fmt_time(3400), "00:00:03.400");
    }

    #[test]
    fn timecode_parse_accepts_expected_forms() {
        // 完整时间码 / 简写 / 逗号小数点 / 缺小数
        assert_eq!(parse_timecode_ms("00:00:03.400"), Some(3400));
        assert_eq!(parse_timecode_ms(" 00:00:03.400 "), Some(3400));
        assert_eq!(parse_timecode_ms("00:00:03,400"), Some(3400));
        assert_eq!(parse_timecode_ms("03:04"), Some(184_000));
        assert_eq!(parse_timecode_ms("1:02:03"), Some(3_723_000));
        assert_eq!(parse_timecode_ms("00:00:03"), Some(3000));
        // 不带冒号：带小数点按**秒**，纯整数按**毫秒**
        assert_eq!(parse_timecode_ms("3.4"), Some(3400));
        assert_eq!(parse_timecode_ms("03.400"), Some(3400));
        assert_eq!(parse_timecode_ms("3400"), Some(3400));
        // 小数位不足按高位补零、超出按毫秒截断
        assert_eq!(parse_timecode_ms("00:00:00.4"), Some(400));
        assert_eq!(parse_timecode_ms("00:00:00.45"), Some(450));
        assert_eq!(parse_timecode_ms("00:00:00.2345"), Some(234));
    }

    #[test]
    fn timecode_parse_rejects_garbage() {
        for bad in [
            "",
            "   ",
            "abc",
            "00:00",
            "-5",
            "00:-1",
            "1:2:3:4",
            "00:00:03.",
            "00:00:03.x",
            "12:",
            ":12",
        ] {
            // "00:00" 是合法的 MM:SS！单独排除掉再断言
            if bad == "00:00" {
                assert_eq!(parse_timecode_ms(bad), Some(0));
                continue;
            }
            assert_eq!(parse_timecode_ms(bad), None, "{bad:?} 不应被解析成时间码");
        }
        // 超大数值不能 panic（checked_* 全程兜底）
        assert_eq!(parse_timecode_ms("99999999999999999999:00"), None);
        assert_eq!(parse_timecode_ms("99999999999999999999"), None);
    }

    // ---- 缩放：整片铺满 / 极端时长不退化 ----

    #[test]
    fn fit_scale_covers_whole_clip() {
        // 1 秒、5 秒、1 小时、10 小时：铺满比例都必须"宽度 × 比例 = 时长"
        for duration in [1_000i64, 5_000, 3_600_000, 36_000_000] {
            for width in [120.0f32, 640.0, 1_920.0] {
                let px = fit_px_per_ms(width, duration);
                assert!(px.is_finite() && px > 0.0, "{duration}ms/{width}px → {px}");
                let visible_ms = width as f64 / px;
                assert!(
                    (visible_ms - duration as f64).abs() <= 1e-6 * duration as f64,
                    "整片应恰好铺满：{duration}ms / {width}px → 可见 {visible_ms}ms"
                );
            }
        }
        // 旧硬编码下限 0.002 会把 1 小时视频卡住（800/3.6e6 = 2.22e-4 < 0.002）；
        // 现在必须远小于它，即"取消比例约束"
        assert!(fit_px_per_ms(800.0, 3_600_000) < 0.002);
        // 极短视频也不退化
        assert!(fit_px_per_ms(800.0, 1) > 0.0);
    }

    #[test]
    fn scale_sanitize_never_degenerates() {
        for duration in [1i64, 1_000, 3_600_000, i64::MAX] {
            let lo = min_px_per_ms(duration);
            assert!(lo.is_finite() && lo > 0.0);
            for input in [
                0.0f64,
                -1.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                1e-300,
                1e300,
            ] {
                let px = sanitize_px_per_ms(input, duration);
                assert!(
                    px.is_finite() && px > 0.0,
                    "duration={duration} input={input} → {px} 必须是有限正数"
                );
                assert!(px >= lo - 1e-18, "不得低于像素下限");
            }
        }
        // 合法值原样保留（不设人为上限）
        assert_eq!(sanitize_px_per_ms(0.05, 5_000), 0.05);
        assert_eq!(sanitize_px_per_ms(4.0, 5_000), 4.0);
        assert_eq!(sanitize_px_per_ms(50.0, 5_000), 50.0);
        // 可用宽度为 0 / NaN 时，"整片铺满"依然给出有限正数
        for w in [0.0f32, -10.0, f32::NAN, f32::INFINITY] {
            let px = fit_px_per_ms(w, 5_000);
            assert!(px.is_finite() && px > 0.0, "width={w} → {px}");
        }
    }

    /// 滚轮步进必须落在 1.1~1.25 倍（一格 50~100 px 的 smooth_scroll_delta）
    #[test]
    fn zoom_step_is_in_expected_range() {
        for scroll in [50.0f64, 100.0] {
            let f = zoom_factor(scroll);
            assert!((1.1..=1.25).contains(&f), "scroll={scroll} → {f}");
        }
        // 方向对称
        assert!((zoom_factor(-50.0) - 1.0 / zoom_factor(50.0)).abs() < 1e-12);
        // 非法输入不产生 NaN/Inf
        assert_eq!(zoom_factor(f64::NAN), 1.0);
        assert!(zoom_factor(f64::INFINITY).is_finite());
        assert_eq!(zoom_factor(0.0), 1.0);
    }

    // ---- 锚点缩放（P9）：鼠标下方的时刻必须几乎不动 ----

    /// 鼠标下的时刻在缩放前后**停在同一个像素位置**（≤1px），
    /// 且左右都不越出 `[0, duration]`（视口右端不越片尾）。
    ///
    /// 这条就是用户诉求的原话：「以鼠标所在位置为中心……我明显是要看此处前后的情况，
    /// 可是现在一放到，就跑到后面看不见了」。
    #[test]
    fn zoom_is_anchored_at_the_mouse_position() {
        let dur = 600_000i64; // 10 分钟
        let width = 900.0f64;
        let px0 = fit_px_per_ms(width as f32, dur);
        let vs0 = 0.0f64;
        // 覆盖视图各个位置（含最左、最右、正中）与两个缩放方向、多档倍数
        for frac in [0.0f64, 0.07, 0.25, 0.5, 0.5, 0.83, 0.99, 1.0] {
            let x = frac * width;
            for factor in [1.2f64, 1.25, 2.0, 4.0, 0.8, 0.5, 1.0 / 1.25] {
                let (vs, px) = zoom_anchored(vs0, px0, x, width, factor, dur);
                // ① 不越界：左端 ≥ 0，右端 ≤ duration
                assert!(vs >= 0.0, "view_start 必须 ≥ 0（得到 {vs}）");
                let right = vs + width / px;
                assert!(
                    right <= dur as f64 + 1e-6,
                    "视口右端必须 ≤ duration（{right} > {dur}）"
                );
                // ② 锚定：鼠标下的时刻仍在同一个像素位置（没被夹取时应当精确相等）
                let t_before = (vs0 + x / px0).clamp(0.0, dur as f64);
                let x_after = (t_before - vs) * px;
                let clamped = vs <= 1e-9 || vs >= (dur as f64 - width / px).max(0.0) - 1e-9;
                let tol = if clamped { 2.0 } else { 1.0 };
                assert!(
                    (x_after - x).abs() <= tol + 1e-6,
                    "鼠标下的时刻跑了：x={x} → x'={x_after}（frac={frac} factor={factor} clamped={clamped}）"
                );
                // ③ 单调：放大不会反而变小
                if factor > 1.0 {
                    assert!(px >= px0 - 1e-12, "factor={factor} 应当放大");
                }
            }
        }
    }

    /// 缩放后视口**不越片尾**：先把视图放大并平移到片尾附近，再继续放大 ——
    /// `view_start + width/px` 必须始终 ≤ duration（"标记能拖到视频之外"的同类困惑）。
    #[test]
    fn zoom_never_lets_the_viewport_pass_the_end() {
        let dur = 60_000i64;
        let width = 800.0f64;
        let mut vs = 0.0f64;
        let mut px = fit_px_per_ms(width as f32, dur);
        for _ in 0..40 {
            // 每次都锚在视图右端：最容易把视口顶出片尾的位置
            let (nv, np) = zoom_anchored(vs, px, width, width, 1.2, dur);
            vs = nv;
            px = np;
            let right = vs + width / px;
            assert!(
                right <= dur as f64 + 1e-6,
                "视口右端越出片尾：{right} > {dur}"
            );
            assert!(vs >= 0.0);
        }
        // 缩小回去也必须合法
        for _ in 0..40 {
            let (nv, np) = zoom_anchored(vs, px, 0.0, width, 0.8, dur);
            vs = nv;
            px = np;
            assert!(vs >= 0.0 && vs + width / px <= dur as f64 + 1e-6);
        }
    }

    /// 退化输入（NaN / 0 / 负数 / 未知时长）必须**原样退回**，绝不产生 NaN、绝不 panic。
    #[test]
    fn zoom_anchored_survives_degenerate_input() {
        let cases: [(f64, f64, f64, f64, f64, i64); 7] = [
            (f64::NAN, 0.1, 10.0, 800.0, 1.2, 1000),
            (0.0, f64::NAN, 10.0, 800.0, 1.2, 1000),
            (0.0, 0.0, 10.0, 800.0, 1.2, 1000),
            (0.0, -1.0, 10.0, 800.0, 1.2, 1000),
            (0.0, 0.1, f64::INFINITY, 800.0, 1.2, 1000),
            (0.0, 0.1, 10.0, 800.0, f64::NAN, 1000),
            (0.0, 0.1, 10.0, 800.0, 1.2, 0),
        ];
        for (vs, px, x, w, f, dur) in cases {
            let (nv, np) = zoom_anchored(vs, px, x, w, f, dur);
            assert!(nv.is_finite(), "view_start 不能变成 NaN/inf");
            assert!(nv >= 0.0);
            assert!(np.is_finite(), "px_per_ms 不能变成 NaN/inf");
        }
        // 合法输入下 factor = 1 应当基本不动（只受"下限夹取"影响）
        let (vs, px) = zoom_anchored(3_000.0, 0.05, 200.0, 800.0, 1.0, 60_000);
        assert!(
            (vs - 3_000.0).abs() < 1.0,
            "factor=1 时视图不该跳走（得到 {vs}）"
        );
        assert!((px - 0.05).abs() < 1e-12);
    }

    // ---- 刻度抽稀：数量上限 ----
    #[test]
    fn ruler_ticks_are_thinned() {
        // 1 小时、800 px、整片铺满：刻度数必须远小于"每 5 ms 一根"
        let width = 800.0f64;
        let duration = 3_600_000i64;
        let px = fit_px_per_ms(width as f32, duration);
        let ticks = ruler_ticks(0.0, duration as f64, duration, px, RULER_MIN_GAP_PX);
        assert!(!ticks.is_empty());
        assert!(
            ticks.len() as f64 <= width / RULER_MIN_GAP_PX + 2.0,
            "刻度数 {} 超出抽稀上限",
            ticks.len()
        );
        // 升序且都在范围内
        assert!(ticks.windows(2).all(|w| w[0] < w[1]));
        assert!(ticks.iter().all(|t| *t >= 0.0 && *t <= duration as f64));
        // 匀速尺：相邻间距恒定
        if ticks.len() >= 3 {
            let d = ticks[1] - ticks[0];
            assert!((ticks[2] - ticks[1] - d).abs() < 1e-9);
        }

        // 极端缩放（滚到最远、缩到最小）都不能失控或死循环
        for px_now in [min_px_per_ms(duration), MAX_PX_PER_MS, 0.0, f64::NAN] {
            let t = ruler_ticks(0.0, 1e12, duration, px_now, RULER_MIN_GAP_PX);
            assert!(t.len() <= 4096, "px={px_now} 时刻度数 {} 失控", t.len());
        }
        assert!(ruler_ticks(0.0, 0.0, duration, px, RULER_MIN_GAP_PX).is_empty());
    }

    #[test]
    fn keyframe_ticks_are_thinned() {
        // 1 小时、每 1 秒一个关键帧 = 3600 个；整片铺满 800 px
        let kfs: Vec<i64> = (0..3600).map(|i| i * 1000).collect();
        let width = 800.0f64;
        let duration = 3_600_000i64;
        let px = fit_px_per_ms(width as f32, duration);
        let ticks = visible_keyframe_ticks(&kfs, px, 0.0, duration as f64, KEYFRAME_MIN_GAP_PX);
        assert!(!ticks.is_empty());
        assert!(
            ticks.len() as f64 <= width / KEYFRAME_MIN_GAP_PX + 1.0,
            "关键帧刻度 {} 条超出抽稀上限",
            ticks.len()
        );
        // 抽稀结果一定要包含第一个关键帧（否则"最左能吸附到哪"会看不出来）
        assert_eq!(ticks[0], 0);
        // 抽稀只删不加：结果必须是原关键帧的子序列
        let mut it = kfs.iter();
        for t in &ticks {
            assert!(it.any(|k| k == t), "抽稀结果 {t} 不在原始关键帧里");
        }

        // 放大到 1 px/ms 时不再抽稀（间距足够）
        let dense = visible_keyframe_ticks(&kfs, 1.0, 0.0, 2000.0, KEYFRAME_MIN_GAP_PX);
        assert_eq!(dense, vec![0, 1000, 2000]);
        // 不在视图里的不画（关键帧都是整秒，这里取一段整秒之间没有关键帧的视图）
        assert!(visible_keyframe_ticks(&kfs, 1.0, 10_100.0, 10_900.0, 1.0).is_empty());
        // 非法缩放 → 空（不 panic）
        assert!(visible_keyframe_ticks(&kfs, f64::NAN, 0.0, 1000.0, 1.0).is_empty());
    }

    /// 附近缩略图卡片：等比不拉伸、极端输入不退化
    #[test]
    fn thumb_size_keeps_aspect() {
        let s = fit_thumb_size(320, 240, 134.0, 74.0);
        assert!(
            (s.x / s.y - 320.0 / 240.0).abs() < 1e-3,
            "4:3 不应被拉伸：{s:?}"
        );
        assert!(s.x <= 134.0 && s.y <= 74.0);
        let s = fit_thumb_size(1920, 1080, 134.0, 74.0);
        assert!((s.x / s.y - 1920.0 / 1080.0).abs() < 1e-3);
        // 0 尺寸/非法上限：给个正数，不产生 0/NaN（否则 egui 会画出畸形矩形）
        for (w, h, mw, mh) in [
            (0u32, 0u32, 134.0f32, 74.0f32),
            (10, 10, 0.0, 0.0),
            (10, 10, f32::NAN, 74.0),
        ] {
            let s = fit_thumb_size(w, h, mw, mh);
            assert!(s.x.is_finite() && s.y.is_finite() && s.x > 0.0 && s.y > 0.0);
        }
        assert_eq!(keyframe_ordinal(&[0, 1000, 2000], 1000), Some(1));
        assert_eq!(keyframe_ordinal(&[0, 1000, 2000], 1500), None);
    }

    // ---- 当前标尺 → 起点/终点：标记动作必须作用于 cursor_ms，与鼠标位置无关 ----

    /// 没有素材时预览为空（界面不画缩略图）；关键帧列表为空时退回标尺位置本身
    /// —— 这两条保证"预览始终有值、不闪空"的边界成立。
    #[test]
    fn preview_ms_needs_a_clip_and_falls_back_without_keyframes() {
        let mut page = EditorPage::new(&Config {
            last_video_path: None,
            ..Config::default()
        });
        assert_eq!(page.preview_ms(), None, "没有素材时不预览");

        // 有关键帧：预览 = 标尺最近的关键帧
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        page.clip = Some(clip);
        page.cursor_ms = 2_100;
        assert_eq!(page.preview_ms(), Some(2_000));

        // 没有扫到关键帧：退回标尺位置本身（不是 None —— 不能闪空）
        let mut bare = synthetic_clip(10_000, vec![], 30.0);
        bare.keyframes_ms.clear();
        page.clip = Some(bare);
        page.cursor_ms = 3_333;
        assert_eq!(
            page.preview_ms(),
            Some(3_333),
            "关键帧为空时预览退回标尺位置（保证始终有值）"
        );
    }

    #[test]
    fn mark_start_end_uses_cursor_and_keeps_snap_semantics() {
        let mut page = EditorPage::new(&Config {
            last_video_path: None,
            ..Config::default()
        });
        let clip = synthetic_clip(10_000, vec![0, 2000, 4000, 6000], 30.0);
        page.clip = Some(clip.clone());

        // 标记动作**只认 `cursor_ms`**：预览/悬停/拖动都不参与（预览现在等于标尺最近的关键帧）
        page.cursor_ms = 3_400;
        page.mark_start_at_cursor();
        page.mark_end_at_cursor();
        assert_eq!(page.sel_start_ms, 3_400, "标记的是当前标尺");
        assert_eq!(page.sel_end_ms, 3_400);
        assert_eq!(
            edit::snap_start(&clip, page.sel_start_ms),
            4_000,
            "起点吸附到 ≥ 该点"
        );
        assert_eq!(
            edit::snap_end(&clip, page.sel_end_ms),
            2_000,
            "终点吸附到 ≤ 该点"
        );
        // 预览也只是标尺附近的关键帧，与标记互不影响
        assert_eq!(
            page.preview_ms(),
            edit::nearest_keyframe(&clip, page.cursor_ms),
            "预览 = 当前标尺最近的关键帧"
        );

        // 当前标尺越界（直接赋值，不经过点击）也会被夹到 [0, duration]
        page.cursor_ms = 99_999;
        page.mark_end_at_cursor();
        assert_eq!(page.sel_end_ms, 10_000);
        page.cursor_ms = -500;
        page.mark_start_at_cursor();
        assert_eq!(page.sel_start_ms, 0);
        // 夹紧后仍然走同一套吸附语义
        assert_eq!(edit::snap_end(&clip, page.sel_end_ms), 6_000);
    }

    /// 交接链：`EditorPage::open(&path)` 之后，剪辑页内部状态**确实**载入了该文件
    /// （`loaded_path` 是它、时长为正）。`src/main.rs::open_in_editor` 走的就是这个调用。
    #[test]
    fn open_loads_clip_into_page_state() {
        let Some(src) = gen_mp4("open") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过剪辑页载入测试");
            return;
        };
        let cfg = Config {
            last_video_path: None,
            ..Config::default()
        };
        let mut page = EditorPage::new(&cfg);
        assert_eq!(
            page.loaded_path(),
            None,
            "新建且无 last_video_path 时不该有载入"
        );
        assert_eq!(page.loaded_duration_ms(), 0);

        page.open(&src);
        // P34：`open()` 只做毫秒级清理 + 起后台线程；测试台把它跑完再断言
        page.wait_open();
        assert_eq!(
            page.loaded_path(),
            Some(src.as_path()),
            "open() 之后剪辑页应持有该文件路径"
        );
        assert!(
            page.loaded_duration_ms() > 0,
            "载入后时长应 > 0，实际 {} ms",
            page.loaded_duration_ms()
        );
        assert!(
            (page.loaded_duration_ms() - 2000).abs() <= 120,
            "2 秒素材载入后时长应约 2000 ms，实际 {} ms",
            page.loaded_duration_ms()
        );
        // open() 之后默认"整片铺满"，当前标尺停在**整片中间**，
        // 且换片子会把上一条素材的缩略图缓存清干净
        assert!(page.fit_mode, "open() 之后应回到整片铺满");
        assert!(page.kf_thumbs.is_empty());
        assert!(!page.hover_live, "新片子的第一帧不该认为指针在时间线上");
        assert!(
            page.preview_ms().is_some(),
            "预览始终有值（等于标尺最近的关键帧）"
        );
        assert_eq!(
            page.cursor_ms,
            page.loaded_duration_ms() / 2,
            "open() 之后当前标尺应重置到时长的一半（默认停在视频中间）"
        );
    }

    /// **P46**：`new(&Config)` **绝不自动载入**，哪怕 `last_video_path` 指向一个真实文件。
    ///
    /// 这是本轮的契约：剪辑页只认显式打开。所以这里不只断言"没载入"，
    /// 还要断言**连打开都没起头**（`open_rx` / `open_pending_name` 都是空）——
    /// 否则"异步载入还在路上"这种情况会被 `loaded_path() == None` 误判成通过。
    #[test]
    fn p46_new_never_autoloads_last_video_path() {
        let Some(src) = gen_mp4("no-autoload") else {
            println!("[skip] 本机随包 ffmpeg 不可用或缺少 libopenh264，跳过 P46 不自动载入测试");
            return;
        };
        assert!(src.is_file(), "前提：last_video_path 指向的文件真的存在");
        let cfg = Config {
            last_video_path: Some(src.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let mut page = EditorPage::new(&cfg);
        // 给异步链足够的机会起头（真要是自动载入了，这一步就会把它跑完）
        page.wait_open();
        assert_eq!(page.loaded_path(), None, "P46：new() 不得自动载入任何视频");
        assert!(
            !page.is_opening(),
            "P46：不该有在途的打开（连后台线程都不该起）"
        );
        assert!(page.clip.is_none(), "P46：不该持有任何素材");
        assert!(
            page.error.is_none(),
            "P46：不自动载入也不该因此报错（界面保持干净的空状态）"
        );
        // 反向确认：显式 `open()` 仍然正常 —— 删掉的是"自动"，不是"打开"
        page.open(&src);
        page.wait_open();
        assert_eq!(
            page.loaded_path(),
            Some(src.as_path()),
            "显式 open() 必须照常工作"
        );
        assert!(page.loaded_duration_ms() > 0);
    }

    // =======================================================================
    // P34：打开异步化（探测即交 + 续扫关键帧），界面零等待
    // =======================================================================

    /// **P34 核心契约**：`open()` 立刻返回（不扫全文件），clip 由 `poll_open` 异步落地；
    /// 关键帧表在扫描期间"未定稿"（导出禁用），扫完自动解禁。
    #[test]
    fn p34_open_returns_immediately_and_applies_on_poll() {
        let Some(src) = gen_mp4_with_audio("p34-async-open") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P34 异步打开测试");
            return;
        };
        let mut page = EditorPage::new(&Config {
            last_video_path: None,
            ..Config::default()
        });
        let t0 = Instant::now();
        page.open(&src);
        // ① `open()` 本身必须毫秒级返回：只做内存清理 + 起后台线程
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "open() 不得阻塞（实测 {:?}）",
            t0.elapsed()
        );
        // ② 此刻还没有应用 clip：那是后台探测消息的活
        assert!(page.clip.is_none(), "clip 要等探测消息回来才应用");
        assert!(page.open_pending_name.is_some(), "应处于「正在打开」状态");
        assert!(
            !page.export_enabled(),
            "关键帧扫描期间导出必须禁用（表不完整 → copy 导出会落在非关键帧上）"
        );
        // ③ 收消息（生产里由每帧 `poll_open` 做）→ 应用 → 扫描完成
        page.wait_open();
        assert!(page.clip.is_some(), "探测消息回来后应载入 clip");
        assert!(
            page.open_pending_name.is_none(),
            "打开结束后不该还挂着「正在打开」"
        );
        assert!(page.open_rx.is_none(), "终态之后通道要收掉");
        let c = page.clip.as_ref().expect("上面刚断言过");
        assert!(c.keyframes_scanned, "落定之后关键帧表必须是最终态");
        assert!(!c.keyframes_ms.is_empty(), "带视频的素材应扫到关键帧");
        assert!(page.export_enabled(), "扫描完成 → 导出解禁");
    }

    // =======================================================================
    // P35：编码体检（合并结论 / 播放跳过区间）
    // =======================================================================

    /// **干净素材：既没有坏段，也没有要跳的区间**。
    ///
    /// 这条保证"加了体检之后，正常文件的播放路径与改造前完全相同"
    /// （`hard_skip_ranges()` 为空 → `spawn_skipping` 退化成 `spawn`）。
    #[test]
    fn p35_clean_clip_has_no_skip_ranges() {
        let h = Harness::new(synthetic_clip(10_000, vec![0, 2_000], 30.0));
        assert!(h.page.health_report().is_clean());
        assert_eq!(h.page.hard_skip_ranges(), Vec::new());
    }

    /// **深度检查的结论会并入 `clip.health`**（唯一真相源 → 三处消费口径一致）。
    ///
    /// 这条钉住的正是"界面说会跳过、导出却没剔"那个坑：深度结果若另存一份，
    /// 导出（读 `clip.health`）就永远看不到它。
    #[test]
    fn p35_deep_result_is_merged_into_clip_health() {
        let mut clip = synthetic_clip(10_000, vec![0, 2_000], 30.0);
        clip.health.bad.push(edit::health::BadRange::new(
            3_000,
            4_000,
            edit::health::BadKind::ReadError,
            "快速体检",
        ));
        let mut deep = edit::health::HealthReport {
            full_decode: true,
            scanned_ms: 10_000,
            ..Default::default()
        };
        deep.bad.push(edit::health::BadRange::new(
            6_000,
            7_000,
            edit::health::BadKind::DecodeError,
            "深度检查",
        ));
        merge_deep_into_clip(&mut clip, deep);

        let h = Harness::new(clip);
        let rep = h.page.health_report();
        assert_eq!(rep.bad.len(), 2, "两边的坏段都要在：{:?}", rep.bad);
        assert!(rep.full_decode, "做过深度检查就要标记出来");
        assert_eq!(
            h.page.hard_skip_ranges(),
            vec![(3_000, 4_000), (6_000, 7_000)],
            "两段都是硬错误 → 都要跳（导出读的也是同一份）"
        );
    }

    /// **重复跑深度检查不会累积坏段**（否则连点几次按钮，坏段会翻倍）。
    #[test]
    fn p35_merging_the_same_deep_report_twice_is_idempotent() {
        let mut clip = synthetic_clip(10_000, vec![0, 2_000], 30.0);
        let mut r = edit::health::HealthReport {
            full_decode: true,
            ..Default::default()
        };
        r.bad.push(edit::health::BadRange::new(
            6_000,
            7_000,
            edit::health::BadKind::DecodeError,
            "深度检查",
        ));
        merge_deep_into_clip(&mut clip, r.clone());
        merge_deep_into_clip(&mut clip, r);
        assert_eq!(
            clip.health.bad.len(),
            1,
            "同一段深检结论并入两次必须还是一条：{:?}",
            clip.health.bad
        );
    }

    /// **只有硬错误才进"播放跳过"列表**（用户口径：仅硬错误自动跳）。
    #[test]
    fn p35_skip_ranges_exclude_soft_kinds() {
        let mut clip = synthetic_clip(10_000, vec![0, 2_000], 30.0);
        clip.health.bad.push(edit::health::BadRange::new(
            1_000,
            2_000,
            edit::health::BadKind::TimestampJump,
            "时间戳跳变",
        ));
        clip.health.bad.push(edit::health::BadRange::new(
            0,
            10_000,
            edit::health::BadKind::NoKeyframe,
            "无关键帧",
        ));
        let h = Harness::new(clip);
        assert!(!h.page.health_report().is_clean(), "软告警也要出现在报告里");
        assert_eq!(
            h.page.hard_skip_ranges(),
            Vec::new(),
            "软告警绝不能让播放跳着走"
        );
    }

    /// **P36：连续添加两个片段，第一个不会被第二个的选区改掉**。
    ///
    /// 这条守的是一个真实的坑：`sel_*` 在"选中某段"时是那段的镜像，
    /// 所以「添加」之后**必须取消选中**（`active_seg = None`）——
    /// 否则用户接着拖把手选下一段时，每帧的 `sync_active_segment`
    /// 会把刚加进去的那一段改成新选区，结果列表里永远只有一个"最新"的片段。
    #[test]
    fn p36_adding_twice_keeps_both_and_clears_selection() {
        let mut page = EditorPage::new(&Config::default());
        page.clip = Some(synthetic_clip(10_000, vec![0, 2_000, 4_000, 6_000], 30.0));
        page.cursor_committed = true;

        // 第一次：入点 0、出点 2000
        page.sel_start_ms = 0;
        page.sel_end_ms = 2_000;
        page.add_segment();
        assert_eq!(page.segments, vec![(0, 2_000)], "第一段应进列表");
        assert_eq!(
            page.active_seg, None,
            "添加之后必须取消选中 —— 否则下一段选区会覆盖它"
        );

        // 第二次：入点 4000、出点 6000（中间还过了一帧 sync，模拟真实渲染循环）
        page.sel_start_ms = 4_000;
        page.sel_end_ms = 6_000;
        page.sync_active_segment();
        assert_eq!(
            page.segments[0],
            (0, 2_000),
            "第一段不许被第二段的选区改掉（这正是 active_seg 必须清空的原因）"
        );
        page.add_segment();
        assert_eq!(
            page.segments,
            vec![(0, 2_000), (4_000, 6_000)],
            "两段都该在列表里，顺序 = 添加顺序"
        );
    }

    /// **P36：拖拽改顺序**（`move_segment` 的插入语义与选中跟随）。
    ///
    /// 列表顺序 = 播放顺序，所以"把第 3 段拖到最前"必须真的让它在导出时排第一；
    /// 同时 `active_seg` 要跟着那一段走，不能让它指向别人。
    #[test]
    fn p36_move_segment_reorders_and_follows_the_selection() {
        let mut page = EditorPage::new(&Config::default());
        page.segments = vec![(0, 1_000), (2_000, 3_000), (4_000, 5_000)];
        page.active_seg = Some(2);

        // 第 3 段拖到最前
        page.move_segment(2, 0);
        assert_eq!(
            page.segments,
            vec![(4_000, 5_000), (0, 1_000), (2_000, 3_000)],
            "第 3 段应排到第一"
        );
        assert_eq!(page.active_seg, Some(0), "选中标记跟着被拖的那一段走");

        // 再把它拖回末尾
        page.move_segment(0, 3);
        assert_eq!(
            page.segments,
            vec![(0, 1_000), (2_000, 3_000), (4_000, 5_000)],
            "拖回末尾应恢复原顺序"
        );
        assert_eq!(page.active_seg, Some(2));

        // 非法/无操作的下标不 panic、不改列表
        page.move_segment(9, 0);
        page.move_segment(1, 1);
        assert_eq!(page.segments.len(), 3);
    }

    /// **P36：芯片必须真的能拖** —— 走**真实鼠标事件序列**，不绕过 widget。
    ///
    /// 这条测试存在的唯一理由：第一版芯片用的是 `egui::Button`，而 `Button` 默认
    /// `Sense::click()` —— **根本不产生 drag 事件**，于是 `drag_started()` /
    /// `drag_stopped()` 永远为假。代码看着齐全、`move_segment` 的纯逻辑单测也全绿，
    /// 但用户在界面上怎么拖都没反应（连报两次"还是不能移动顺序"）。
    /// 所以必须按"按下 → 移动 → 松手"喂事件，断言**列表顺序真的变了**。
    #[test]
    fn p36_dragging_a_chip_with_a_real_mouse_reorders_the_playlist() {
        let mut hh = Harness::new(synthetic_clip(10_000, vec![0, 2_000, 4_000, 6_000], 30.0));
        hh.page.segments = vec![(0, 1_000), (2_000, 3_000), (4_000, 5_000)];
        hh.page.active_seg = None;
        hh.frame(vec![]);
        let chips = hh.page.test_rects.seg_chips.clone();
        assert_eq!(chips.len(), 3, "三个片段应有三个芯片");

        // 把第 1 个芯片拖到第 3 个芯片的右半边 → 它应排到最后
        let from = chips[0].center();
        let to = egui::pos2(chips[2].right() - 1.0, chips[2].center().y);
        hh.drag_like_a_mouse(from, to);

        assert_eq!(
            hh.page.segments,
            vec![(2_000, 3_000), (4_000, 5_000), (0, 1_000)],
            "拖拽后顺序必须真的变（第一段落到最后）；拖不动 = 芯片的 Sense 还是 click"
        );
        assert_eq!(hh.page.seg_drag, None, "松手后拖拽状态必须清掉");
    }

    /// **P34 首帧优先**：关键帧表还没扫完时，预览**直接取目标时刻**（不解关键帧表）；
    /// 表定稿后回到旧语义（最近关键帧 / 没有关键帧就不请求）。
    #[test]
    fn p34_thumb_keyframe_falls_back_to_raw_time_while_unscanned() {
        // 未扫 + 空表（流式打开的第一段）：直接取目标时刻，首帧不必等扫描
        let mut clip = synthetic_clip(10_000, vec![], 30.0);
        clip.keyframes_scanned = false;
        assert_eq!(thumb_keyframe(&clip, 4_321), Some(4_321));
        // 越界也要夹在视频流之内（与既有 P27 口径一致）
        let (span_end, _) = p27_sample_span_end_ms(&clip);
        let last = p27_sample_last_ms(span_end);
        assert_eq!(thumb_keyframe(&clip, 99_999), Some(last));
        assert_eq!(thumb_keyframe(&clip, -5), Some(0));

        // 表定稿但为空（纯音频 / 扫完确实没有关键帧）→ 回到旧语义：不请求
        clip.keyframes_scanned = true;
        assert_eq!(thumb_keyframe(&clip, 4_321), None);

        // 有表时照常取"最近的关键帧"
        let with_kf = synthetic_clip(10_000, vec![0, 2_000, 4_000], 30.0);
        assert!(with_kf.keyframes_scanned);
        assert_eq!(thumb_keyframe(&with_kf, 3_000), Some(2_000));
    }

    // =======================================================================
    // P15 / P16：波形任务的生命周期（**真的有后台线程**这一条链）
    // =======================================================================
    //
    // 为什么单列一组：`Harness` 是"直接塞 clip"的（不起线程、不碰文件），
    // 所以 `open()` → `WaveState::start()` → 两个后台线程 → `poll_waves()` 这条链
    // 一直**没有任何测试覆盖** —— P16 那次"进剪辑页就卡死"就是这么漏出去的。

    /// 波形那条链的测试台：真开一个**带音轨**的素材（因此真的起 L0 + 细化两条线程），
    /// 用真 `egui::Context` 跑帧（只有真跑帧才会走到 `poll_waves` 的拆除路径）。
    struct WaveHarness {
        ctx: egui::Context,
        page: EditorPage,
        screen: egui::Rect,
        time: f64,
    }

    impl WaveHarness {
        fn open(src: &Path) -> Self {
            let ctx = egui::Context::default();
            // 不加载字体：这一组测的是任务生命周期与重绘，不需要字形
            ctx.set_fonts(egui::FontDefinitions::empty());
            let mut page = EditorPage::new(&Config {
                last_video_path: None,
                ..Config::default()
            });
            page.open(src);
            // P34：打开是异步的；测试台先把它跑完（等价于改造前"同步打开"的起点）
            page.wait_open();
            Self {
                ctx,
                page,
                screen: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1024.0, 768.0)),
                time: 0.0,
            }
        }

        fn frame(&mut self) {
            let mut input = egui::RawInput {
                screen_rect: Some(self.screen),
                ..Default::default()
            };
            input.time = Some(self.time);
            self.time += 0.5;
            self.ctx
                .run_ui(input, |ui| {
                    self.page.ui(ui, &crate::i18n::I18n::new("zh-CN"))
                })
                .drop_without_applying_deltas();
        }

        /// 跑到 `done` 为真（每帧之间睡 `step`）；返回是否在 `budget` 内达成。
        fn run_until(
            &mut self,
            budget: std::time::Duration,
            step: std::time::Duration,
            mut done: impl FnMut(&EditorPage) -> bool,
        ) -> bool {
            let t0 = Instant::now();
            while t0.elapsed() < budget {
                self.frame();
                if done(&self.page) {
                    return true;
                }
                std::thread::sleep(step);
            }
            false
        }

        /// **P23**：一直跑帧直到**两阶段链落定**（导入就绪 → 波形阶段已经开始）。
        ///
        /// 为什么要它：P23 之后 `open()` **不再**当场起波形任务 —— 必须先"导入就绪"
        /// （视频流信息 + 首帧预览 + 胶片条首块），波形才在**下一帧**自动开始。
        /// 旧测试里那句"`open()` 之后 `wave.job.is_some()`"因此要改成"等它就绪"。
        ///
        /// 返回是否在预算内等到（等不到时调用方照常断言失败并给出原因）。
        fn wait_wave_phase(&mut self, budget_secs: u64) -> bool {
            self.run_until(
                std::time::Duration::from_secs(budget_secs),
                std::time::Duration::from_millis(5),
                |p| p.wave_phase_done,
            )
        }
    }

    /// **基准层阶段**是否已经结束。
    ///
    /// **`job == None`（任务被整个拆掉）也算"结束"**：这样 P15 的断言会落在
    /// "基准层完成后任务本体必须还在"那一句上（失败信息直指根因），
    /// 而不是先干等一个超时。
    fn wave_l0_done(p: &EditorPage) -> bool {
        match p.wave.job.as_ref() {
            Some(j) => j.base_handles.is_empty(),
            None => true,
        }
    }

    /// 在**看门狗**下跑一段界面代码。
    ///
    /// 为什么必须有它：卡死是"永远不返回"，直接跑会把整条测试进程挂住 ——
    /// 连"失败"都报不出来（P16 就是这么在测试里藏了很久）。
    /// 超时 = 断言失败；闭包里的 panic 照常穿出来（不吞、不误报成超时）。
    fn with_watchdog<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        type Panic = Box<dyn std::any::Any + Send>;
        let (tx, rx) = channel::<Result<T, Panic>>();
        std::thread::Builder::new()
            .name("editor-wave-watchdog".into())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
                let _ = tx.send(r);
            })
            .expect("起看门狗线程");
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(Ok(v)) => v,
            Ok(Err(p)) => std::panic::resume_unwind(p),
            Err(_) => panic!(
                "界面帧循环 {secs}s 内没有返回 —— UI 线程又卡死了（看门狗超时；\
                 检查波形任务的 join / 锁 / 忙等）"
            ),
        }
    }

    /// **P16 防回归**：打开一个带音轨的真实录制后，界面帧循环必须一直返回；
    /// 并且 **L0 算完的那一刻**（修复前正是在这一帧死锁）不能再拆掉任务本体。
    #[test]
    fn p16_entering_the_editor_with_a_recording_never_blocks() {
        let Some(src) = gen_mp4_with_audio("enter") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P16 进入剪辑页防回归");
            return;
        };
        with_watchdog(90, move || {
            let mut h = WaveHarness::open(&src);
            // **P23**：导入就绪之后波形才自动开始（见 `WaveHarness::wait_wave_phase`）
            assert!(
                h.wait_wave_phase(30),
                "导入阶段应在预算内落定（导入就绪 → 自动开始波形）"
            );
            assert!(
                h.page.wave.job.is_some(),
                "有音轨的素材：导入就绪后应自动起波形任务"
            );
            let done = h.run_until(
                std::time::Duration::from_secs(40),
                std::time::Duration::from_millis(5),
                wave_l0_done,
            );
            assert!(done, "L0 阶段应在预算内结束（波形算完）");
            // L0 之后继续跑一批帧：修复前正是在"L0 完成"这一帧永久阻塞
            for _ in 0..40 {
                h.frame();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(
                h.page.wave.job.is_some(),
                "L0 完成后任务本体应继续留着（P15：细化还要用它）"
            );
            assert!(
                h.page.wave.shared.is_some(),
                "L0 完成后 shared 应继续留着（密集瓦片读它）"
            );
        });
    }

    /// **P16 防回归**：连续打开第二个视频不能卡（走 `cancel_current`，
    /// 修复前那里是同一个 `join` 死锁：自己还攥着 `req_tx` 就等线程退出）。
    #[test]
    fn p16_opening_a_second_video_never_blocks() {
        let (Some(a), Some(b)) = (
            gen_mp4_with_audio("second-a"),
            gen_mp4_with_audio("second-b"),
        ) else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P16 连续打开防回归");
            return;
        };
        with_watchdog(120, move || {
            let mut h = WaveHarness::open(&a);
            // **P23**：先等第一个文件的导入阶段落定（波形自动开始）—— 这一步之后
            // 任务本体才是"活着但已知"的状态，下面 `open(&b)` 才会走 `cancel_current`。
            assert!(h.wait_wave_phase(30), "第一个文件应导入就绪并开始波形");
            // 等基准层结束（此刻任务仍然活着 → 再 open 会走 cancel_current 里那条 join）
            assert!(
                h.run_until(
                    std::time::Duration::from_secs(40),
                    std::time::Duration::from_millis(5),
                    wave_l0_done,
                ),
                "第一个文件的基准层应在预算内结束"
            );
            h.page.open(&b); // ← 修复前这里永久阻塞
                             // P34：打开异步化之后，先等这一轮打开落定再看 loaded_path
            h.page.wait_open();
            for _ in 0..20 {
                h.frame();
            }
            assert_eq!(h.page.loaded_path(), Some(b.as_path()));
            // **P23**：换文件之后同样要等"导入就绪"才轮到波形阶段
            assert!(h.wait_wave_phase(30), "第二个文件应导入就绪");
            assert!(h.page.wave.job.is_some(), "第二个文件应重新起波形任务");
        });
    }

    /// **P16 防回归**：关掉波形开关（= 取消在跑的任务）不能卡。
    #[test]
    fn p16_turning_the_waveform_off_never_blocks() {
        let Some(src) = gen_mp4_with_audio("off") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P16 关开关防回归");
            return;
        };
        with_watchdog(90, move || {
            let mut h = WaveHarness::open(&src);
            h.frame();
            h.page.set_show_waveform(false); // ← 修复前这里也永久阻塞
            for _ in 0..10 {
                h.frame();
            }
            assert!(
                h.page.wave.job.is_none(),
                "关掉波形后不该还留着任务（线程要被真的收掉）"
            );
        });
    }

    /// **P16 防回归**：无音轨素材（如实降级、不起任何后台任务）不能卡。
    #[test]
    fn p16_video_without_audio_degrades_without_blocking() {
        let Some(src) = gen_mp4("no-audio") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P16 无音轨防回归");
            return;
        };
        with_watchdog(60, move || {
            let mut h = WaveHarness::open(&src);
            // **P23**：导入就绪之后才会走到"没有音轨 → 如实降级"那一条
            assert!(h.wait_wave_phase(30), "无音轨素材的导入阶段也应落定");
            for _ in 0..20 {
                h.frame();
            }
            assert!(
                h.page.wave.job.is_none(),
                "没有音轨就不该起波形线程（如实降级）"
            );
            assert!(
                h.page.waveform_notice().is_some(),
                "没有音轨应给出一句界面提示"
            );
        });
    }

    /// **P15 防回归**：L0 算完之后"按需细化"仍然工作 ——
    /// 放大后要为可见区间发出细化请求，瓦片真的算出来、入库、并被画出来；
    /// 已经缓存的片不再重复排队。
    #[test]
    fn p15_refine_still_works_after_l0_finishes() {
        let Some(src) = gen_mp4_with_audio("refine") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P15 细化防回归");
            return;
        };
        with_watchdog(120, move || {
            let mut h = WaveHarness::open(&src);
            // **P23**：先等导入就绪（波形那条链才开始跑）
            assert!(h.wait_wave_phase(30), "导入阶段应先落定（波形才被启动）");
            assert!(
                h.run_until(
                    std::time::Duration::from_secs(40),
                    std::time::Duration::from_millis(5),
                    wave_l0_done,
                ),
                "L0 应在预算内结束"
            );
            assert!(
                h.page.wave.job.is_some(),
                "L0 完成后任务本体必须还在（P15）"
            );
            assert!(h.page.wave.shared.is_some(), "L0 完成后 shared 必须还在");
            // 放大到"该用密集层"的档位（P18：档位由**可见时长**决定，`VISIBLE_BUCKETS` = 600）
            h.page.fit_mode = false;
            h.page.px_per_ms = 8.0;
            h.page.view_start_ms = 0.0;
            h.page.wave.last_refine_at = None; // 绕过"手停 120ms"的节流（测试不等节流）
            h.frame();
            assert_eq!(
                h.page.test_rects.wave_detail,
                Some(true),
                "放大后应切到密集层（档位由可见时长决定）"
            );
            assert!(
                !h.page.wave.requested.is_empty(),
                "放大后必须为可见区间发出细化请求（修复前 job 被 take 掉，一个请求都发不出去）"
            );
            // 瓦片真的算出来并被读到 —— 这是"按需细化真的生效"的硬证据
            assert!(
                h.run_until(
                    std::time::Duration::from_secs(40),
                    std::time::Duration::from_millis(5),
                    |p| !p.wave.tiles_map().is_empty(),
                ),
                "细化瓦片应在预算内产出入库"
            );
            h.frame();
            assert_eq!(
                h.page.test_rects.wave_detail,
                Some(true),
                "瓦片到手后仍走密集层"
            );
            assert!(
                h.page.test_rects.wave_segments.unwrap_or(0) > 0,
                "密集层应真的画出波形段"
            );
            // 缓存的片不再重复排队（`pump_refine` 的节流口径）：已经算好的片**只会**让
            // "已请求"集合变小（被修剪），绝不会让它再长回来。
            let before = h.page.wave.requested.len();
            for i in 0..3 {
                h.page.wave.last_refine_at = None;
                h.frame();
                assert!(
                    h.page.wave.requested.len() <= before,
                    "第 {i} 帧：已经算好的片不该再排新请求（{before} → {}）",
                    h.page.wave.requested.len()
                );
            }
        });
    }

    /// **P15 防回归（重绘策略）**：细化瓦片产出时由**后台线程**唤醒界面一次，
    /// 没有新数据时界面完全静止（**没有周期性心跳**，也就没有每帧重算的活锁）。
    #[test]
    fn p15_tile_wakes_the_ui_once_without_a_heartbeat() {
        let Some(src) = gen_mp4_with_audio("wake") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P15 重绘防回归");
            return;
        };
        with_watchdog(120, move || {
            let mut h = WaveHarness::open(&src);
            // **P23**：先让导入阶段落定，再观察"波形算完之后界面静止"——
            // 否则测的会是导入阶段那几次兜底看门狗帧，而不是波形那条链。
            assert!(h.wait_wave_phase(30), "导入阶段应先落定（波形才被启动）");
            assert!(
                h.run_until(
                    std::time::Duration::from_secs(40),
                    std::time::Duration::from_millis(5),
                    wave_l0_done,
                ),
                "L0 应在预算内结束"
            );
            // 静置：界面应当最终**自己不再要求重绘**（egui 的欠帧要两帧才排空）
            let mut settled = false;
            let t0 = Instant::now();
            while t0.elapsed() < std::time::Duration::from_secs(10) {
                h.frame();
                if !h.ctx.has_requested_repaint() {
                    settled = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                settled,
                "波形算完后界面应当静止下来（不该有持续心跳 / 每帧重绘）；\
                 thumb_pending={:?} kf_thumbs={} requested={:?} base_handles={:?}",
                h.page.thumb_pending,
                h.page.kf_thumbs.len(),
                h.page.wave.requested,
                h.page.wave.job.as_ref().map(|j| j.base_handles.len())
            );
            // 只发请求、不跑帧：观察"后台线程产出一块瓦片时把界面叫醒"这件事本身
            h.page.fit_mode = false;
            h.page.px_per_ms = 8.0;
            h.page.view_start_ms = 0.0;
            h.page.wave.last_refine_at = None;
            let tiles_before = h.page.wave.tiles_map().len();
            h.page.wave.pump_refine(Some((0, 200)), 100);
            // ⚠️ 这一条只在"瓦片还没落地"时可观察：整套测试并行跑时主线程会在
            // `send` 之后被抢占，后台可能**先**算完并**合法地**请求了重绘。
            // 那时这条断言要问的"pump_refine 自己会不会请求重绘"已经无从区分，
            // 所以用"瓦片是否入库"把这种情况排除掉（其余情况下断言照旧生效）。
            let tile_landed = h.page.wave.tiles_map().len() > tiles_before;
            assert!(
                tile_landed || !h.ctx.has_requested_repaint(),
                "刚排队时还没有新数据，界面不该被重绘"
            );
            let t0 = Instant::now();
            while !h.ctx.has_requested_repaint()
                && t0.elapsed() < std::time::Duration::from_secs(40)
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                h.ctx.has_requested_repaint(),
                "瓦片产出必须唤醒界面一次（数据驱动重绘，P15）"
            );
            // 消化掉这次唤醒 + 那块瓦片，再静置一次：又该静止（证明它不是心跳）
            let mut settled2 = false;
            let t0 = Instant::now();
            while t0.elapsed() < std::time::Duration::from_secs(10) {
                h.frame();
                if !h.ctx.has_requested_repaint() {
                    settled2 = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                settled2,
                "瓦片到手之后界面应再次静止（说明唤醒是数据驱动、不是周期性心跳）"
            );
        });
    }

    /// **P18 防回归（界面侧的核心一条）：缩放真的会换档、并真的重新发起请求。**
    ///
    /// 这正是用户截图那个 bug 的落点：可见 20 秒的一段，`DETAIL_START_PX`（1.5 px）那道旧门槛
    /// 判定"还不够细"，于是一帧都不换层、一个细化请求都不发，画面永远是 L0 的粗块（稀疏窗口）。
    /// 现在档位由**可见时长**决定（`VISIBLE_BUCKETS` = 600），所以：
    /// 可见区间越短 → 档位越小（严格更细）→ "已请求"被清空并重新排队。
    ///
    /// 这里直接驱动"绘制之后那一步"（`pump_refine`）—— 它是唯一的请求出口，
    /// 也正好是这条链路上曾经出错的一环。
    #[test]
    fn p18_zooming_in_switches_tier_and_re_requests_tiles() {
        let mut h = Harness::with_screen(
            synthetic_clip(60_000, vec![0, 10_000, 20_000, 30_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]);
        h.page.fit_mode = false;
        // 让"请求细化"这条服务看起来是活的（这里只测发不发请求，不真解码）。
        // ⚠️ 通道的两端都要**真的绑住**：写成 `req_tx: channel().0` 的话
        // `Receiver` 是临时值、当场就被丢掉，`send` 一律失败 —— 测出来的"没发请求"是假的。
        let (tx, _refine_rx) = std::sync::mpsc::channel::<RefineReq>();
        let (_progress_tx, progress_rx) = std::sync::mpsc::channel::<()>();
        h.page.wave.job = Some(WaveJob {
            key: waveform::CacheKey::of(Path::new("p18-synthetic.mp4")),
            cancel: Arc::new(AtomicBool::new(false)),
            rx: progress_rx,
            req_tx: tx,
            plan: Arc::new(BasePlan::new(60_000)),
            ui_ctx: Arc::new(Mutex::new(None)),
            base_handles: Vec::new(),
            refine_handle: None,
            cancelled: false,
        });

        let mut tiers: Vec<i64> = Vec::new();
        // 以视图中心为锚，连放大 8 次：可见 1024 ms → 8 ms
        for _ in 0..8 {
            h.page.wave.last_refine_at = None; // 绕过"手停 120 ms"的防抖（测试不等它）
            h.frame(vec![
                egui::Event::PointerMoved(h.timeline().center()),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, 240.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            let visible = 1024.0 / h.page.px_per_ms;
            // 走一遍"绘制之后提交请求"的那一步（与 `timeline()` 里的调用完全同参）
            let a = h.page.view_start_ms;
            let b = (a + visible).min(h.duration_ms as f64);
            h.page
                .wave
                .pump_refine(Some((a as i64, b as i64)), ((a + b) / 2.0) as i64);
            let tier = h.page.wave.last_refine_tier.expect("缩放后必须算出档位");
            assert_eq!(
                tier,
                waveform::tier_for_visible_ms(visible),
                "档位必须由可见时长唯一决定（可见 {visible:.1} ms）"
            );
            tiers.push(tier);
            assert!(
                !h.page.wave.requested.is_empty(),
                "档位 {tier}：这一次缩放必须为可见区间发出细化请求（这正是 P18 要修的链路）；\
                 px_per_ms={} visible={visible} vs={} requested={:?} tiers={tiers:?}",
                h.page.px_per_ms,
                h.page.view_start_ms,
                h.page.wave.requested,
            );
            assert!(
                h.page.wave.requested.iter().all(|(_, t)| *t == tier),
                "已请求的键必须都带当前档位（换档后旧档的结论作废）"
            );
        }
        // **单调变细**：随着放大，档位只会变小（更细）或不变，绝不回退
        for w in tiers.windows(2) {
            assert!(w[1] <= w[0], "放大后档位不能变粗：{tiers:?}");
        }
        let first = *tiers.first().unwrap();
        let last = *tiers.last().unwrap();
        assert!(
            last < first,
            "连放大 8 次之后档位必须严格更细（{first} ms → {last} ms），实测序列 {tiers:?}"
        );
        assert!(
            last <= 10,
            "放到可见 ~8 ms 时档位应当已经到 10 ms 或更细（实测 {last} ms）"
        );
    }

    // ---------------- P20：可见区间（<= 120 s）恒定 600 切片 ----------------

    /// 给 headless 页装一个"看起来是活的"细化服务（只测"发不发请求"，不真解码）。
    ///
    /// ⚠️ 通道的两端都要**真的绑住**：写成 `req_tx: channel().0` 的话 `Receiver`
    /// 是临时值、当场被丢掉，`send` 一律失败 —— 测出来的"没发请求"是假的。
    fn fake_wave_job(page: &mut EditorPage, tag: &str) -> std::sync::mpsc::Receiver<RefineReq> {
        let (tx, rx) = std::sync::mpsc::channel::<RefineReq>();
        let (_progress_tx, progress_rx) = std::sync::mpsc::channel::<()>();
        page.wave.job = Some(WaveJob {
            key: waveform::CacheKey::of(Path::new(&format!("{tag}.mp4"))),
            cancel: Arc::new(AtomicBool::new(false)),
            rx: progress_rx,
            req_tx: tx,
            plan: Arc::new(BasePlan::new(60_000)),
            ui_ctx: Arc::new(Mutex::new(None)),
            base_handles: Vec::new(),
            refine_handle: None,
            cancelled: false,
        });
        rx
    }

    /// 在 headless 页里按"可见时长"设定缩放（`fit_mode` 关掉，视口左端固定）。
    fn set_visible_ms(h: &mut Harness, visible_ms: f64) {
        let w = h.timeline().width() as f64;
        h.page.fit_mode = false;
        h.page.px_per_ms = w / visible_ms.max(1.0);
        h.page.view_start_ms = 0.0;
    }

    /// 给 headless 页塞一份"已经算完的**全片基准层**"（`last_good`）。
    ///
    /// 为什么必须塞：`show_waveform` 那条分支要求 `self.wave.snapshot()` 有值，
    /// 而测试台**故意不碰真实媒体**（没有音轨就没有基准层）。要塞的是"每片都有值"的
    /// 假层 —— 这样"密集层 vs 基准层"的判定就完全由可见时长决定，与素材无关。
    fn seed_l0(page: &mut EditorPage, duration_ms: i64) {
        let mut b = waveform::BaseLayer::empty(duration_ms.max(1));
        for i in 0..b.peaks.len() {
            b.peaks[i] = waveform::Peak {
                min: -0.5,
                max: 0.5,
            };
            b.done[i] = true;
        }
        b.done_slices = b.peaks.len();
        b.finished = true;
        page.wave.last_good = Some(Arc::new(b));
    }

    /// **P20 ②（界面侧）**：视口 120 s → 60 s → 20 s → 5 s → 1 s 每一步都
    /// **走密集层（layer=detail）**、切片宽严格递减，且**每次都真的重排瓦片请求**。
    ///
    /// 这是用户验收口径「每一步波形都明显变细」的界面侧证据：
    /// 旧口径（"可见区间短于一片瓦片才用密集层"）下，前四步里有三步只给 L0，
    /// 段宽不动、请求也不发 —— 正是"缩放时波形不更新"。
    #[test]
    fn p20_zoom_ladder_switches_layer_and_re_requests_tiles() {
        // 10 分钟的片子：120 s 的可见区间完全落在片内（不会被"夹到片尾"影响）
        let mut h = Harness::with_screen(
            synthetic_clip(600_000, vec![0, 10_000, 20_000, 30_000], 30.0),
            1024.0,
            768.0,
        );
        let _rx = fake_wave_job(&mut h.page, "p20-ladder");
        seed_l0(&mut h.page, 600_000);
        h.frame(vec![]);

        let mut widths: Vec<f64> = Vec::new();
        let mut tiers: Vec<i64> = Vec::new();
        for vis in [120_000.0, 60_000.0, 20_000.0, 5_000.0, 1_000.0] {
            set_visible_ms(&mut h, vis);
            h.page.wave.last_refine_at = None; // 绕过"手停 120 ms"的防抖（测试不等节流）
            h.page.wave.requested.clear();
            h.frame(vec![]);
            let t = &h.page.test_rects;
            assert_eq!(
                t.wave_detail,
                Some(true),
                "可见 {vis} ms 必须走密集层（layer=detail），实测 {:?}",
                t.wave_detail
            );
            let tier = t.wave_tier_ms.expect("密集层必须给出档位");
            assert_eq!(
                tier,
                waveform::tier_for_visible_ms(vis),
                "档位必须由可见时长唯一决定（可见 {vis} ms）"
            );
            assert!(
                !h.page.wave.requested.is_empty(),
                "可见 {vis} ms（档位 {tier} ms）：必须为可见区间排细化请求"
            );
            assert!(
                h.page.wave.requested.iter().all(|(_, k)| *k == tier),
                "已请求的键必须都带当前档位（换档后旧档的结论作废）"
            );
            assert!(
                h.page.wave.requested.len() <= waveform::MAX_DENSE_TILES_PER_VIEW + 2,
                "一屏的片数必须落在护栏附近（{} 片）",
                h.page.wave.requested.len()
            );
            tiers.push(tier);
            widths.push(waveform::visible_bucket_ms(vis));
        }
        for i in 1..tiers.len() {
            assert!(
                tiers[i] < tiers[i - 1],
                "每一步都必须换到更细的档位：{tiers:?}"
            );
        }
        for i in 1..widths.len() {
            assert!(
                widths[i] < widths[i - 1],
                "每一步切片宽都必须更小：{widths:?}"
            );
        }
        // 门槛上一档是 200 ms（= 120 s / 600），封顶是 1 ms
        assert_eq!(tiers, vec![200, 100, 25, 5, 1], "档位序列必须严格递减");
    }

    /// **P20 ④（界面侧）**：同一时间跨度在**不同缩放级别**下，切片**时间位置**按 `x_of` 落位
    /// —— 缩放不会把"同一批数据"拉伸（段宽随档位变细，而不是随视口变宽）。
    #[test]
    fn p20_painting_follows_x_of_not_a_stretched_block() {
        let mut h = Harness::with_screen(
            synthetic_clip(600_000, vec![0, 10_000, 20_000, 30_000], 30.0),
            1024.0,
            768.0,
        );
        let _rx = fake_wave_job(&mut h.page, "p20-painting");
        seed_l0(&mut h.page, 600_000);
        h.frame(vec![]);

        // 视口 120 s：档位 200 ms；视口 60 s：档位 100 ms（同一段素材，切得更细）
        let mut samples: Vec<(i64, Vec<f64>)> = Vec::new();
        for vis in [120_000.0f64, 60_000.0] {
            set_visible_ms(&mut h, vis);
            h.page.wave.last_refine_at = None;
            h.frame(vec![]);
            let tier = h.page.test_rects.wave_tier_ms.expect("必须有档位");
            // 屏幕上的"每段像素宽" = 时间宽 × px_per_ms（与刻度尺同源）
            let px_per_ms = h.page.px_per_ms;
            let seg_px = tier as f64 * px_per_ms;
            samples.push((tier, vec![seg_px]));
        }
        assert_eq!(samples[0].0, 200, "120 s 视口 → 200 ms 档");
        assert_eq!(samples[1].0, 100, "60 s 视口 → 100 ms 档");
        // 关键判据：**屏幕上的段宽不随缩放变宽两倍**。若实现是"整块拉伸"，
        // 60 s 视口的段宽（px）会是 120 s 视口的 2 倍；实测两边的段宽**基本一样**
        // （每个切片 ≈ 1 px 上下），因为时间宽减半、像素密度加倍。
        let a = samples[0].1[0];
        let b = samples[1].1[0];
        assert!(
            (a - b).abs() <= a.max(b) * 0.35 + 0.5,
            "段宽（px）不该随缩放翻倍：120 s 视口 {a:.2} px vs 60 s 视口 {b:.2} px"
        );
    }

    /// **P20 ⑤（界面侧）**：缩放/平移停下来之后界面**彻底静止** ——
    /// 既不请求重绘，也不每帧重排同一批片（口径同 P15/P18：无心跳、无活锁）。
    #[test]
    fn p20_zooming_and_panning_do_not_self_excite_repaints() {
        let mut h = Harness::with_screen(
            synthetic_clip(600_000, vec![0, 10_000, 20_000, 30_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]);
        let _rx = fake_wave_job(&mut h.page, "p20-idle");
        seed_l0(&mut h.page, 600_000);
        // 可见 60 s：档位 100 ms、一屏 12 片
        set_visible_ms(&mut h, 60_000.0);
        h.page.wave.last_refine_at = None;
        h.frame(vec![]);
        let after_zoom = h.page.wave.requested.len();
        assert!(after_zoom > 0, "前提：这一次确实排过请求");
        // 平移（可见时长不变 → 档位不变）：只允许"新进入视野的片"被排一次
        h.page.view_start_ms = 30_000.0;
        h.page.wave.last_refine_at = None;
        h.frame(vec![]);
        let after_pan = h.page.wave.requested.len();
        assert!(
            after_pan > after_zoom,
            "平移之后新进入视野的片必须被请求（{after_zoom} → {after_pan}）"
        );
        assert!(
            after_pan <= 2 * (waveform::MAX_DENSE_TILES_PER_VIEW + 2),
            "平移只该影响「新进入视野」的那几片，实测 {after_pan} 片"
        );
        // 停下来：连跑 5 帧，一帧都不许再请求重绘、也不许再排新的片
        let mut settled = false;
        for _ in 0..4 {
            h.frame(vec![]);
            if !h.ctx.has_requested_repaint() {
                settled = true;
                break;
            }
        }
        assert!(
            settled,
            "缩放/平移停下后界面仍在请求重绘 —— 有自激 / 周期性心跳"
        );
        for i in 0..5 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "静置第 {i} 帧：界面不该再请求重绘（自激 / 心跳）"
            );
            assert_eq!(
                h.page.wave.requested.len(),
                after_pan,
                "静置第 {i} 帧：同一批片已经排过了，不该每帧重排一遍"
            );
        }
    }

    // ---------------- P21：胶片条 = 可见区间里固定 N 个缩略图 ----------------

    /// 量一帧里**真正画出来**的胶片条块：每块的矩形。
    fn strip_rects_of(h: &Harness) -> Vec<egui::Rect> {
        h.page
            .test_rects
            .strip_tiles
            .clone()
            .expect("本帧应登记胶片条块矩形")
    }

    /// 量一帧里**真正画出来**的胶片条块：块数与每块的 (宽, 高)。
    fn strip_tiles_of(h: &Harness) -> (usize, Vec<(f32, f32)>) {
        let rects = strip_rects_of(h);
        (
            rects.len(),
            rects.iter().map(|r| (r.width(), r.height())).collect(),
        )
    }

    /// **P21 ①**：**3 个不同缩放级别**下，量真实绘制的画面矩形 → **宽高完全一致**
    /// （只允许 1px 取整误差）—— 用户口径「时间线缩放时，缩略图不应缩放」。
    ///
    /// **P25**：宽不再是 160 槽位，而是 `行高 × 源宽高比`（contain，[`strip_picture_width`]）；
    /// "与缩放无关"这条口径本身一字未改（三个级别下同一个数）。
    #[test]
    fn p21_strip_tile_size_is_constant_across_zoom_levels() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]); // 整片铺满（120 s）
                         // 合成素材是 320×240（4:3）→ 画面宽 = 行高 × 4/3
        let pic_w = strip_picture_width(strip_tile_height(), 320.0 / 240.0);
        let mut measured: Vec<(f64, usize, f32, f32)> = Vec::new();
        for vis in [120_000.0, 20_000.0, 4_000.0] {
            set_visible_ms(&mut h, vis);
            h.frame(vec![]);
            let (n, sizes) = strip_tiles_of(&h);
            assert!(n >= 1, "可见 {vis} ms 时至少要画出 1 块，实测 {n}");
            let w0 = sizes[0].0;
            let h0 = sizes[0].1;
            assert!(
                (w0 - pic_w).abs() <= 1.0,
                "可见 {vis} ms：画面宽必须是「行高 × 源宽高比」= {pic_w:.1}（实测 {w0:.1}）"
            );
            for (w, hh) in &sizes {
                assert!(
                    (w - w0).abs() <= 1.0 && (hh - h0).abs() <= 1.0,
                    "同一帧内所有画面必须一样大（{w:.1}×{hh:.1} vs {w0:.1}×{h0:.1}）"
                );
            }
            measured.push((vis, n, w0, h0));
        }
        // 三个级别的宽高必须完全一致（±1px）：**缩放不拉伸**
        for i in 1..measured.len() {
            assert!(
                (measured[i].2 - measured[0].2).abs() <= 1.0,
                "不同缩放级别下画面宽必须一致：{measured:?}"
            );
            assert!(
                (measured[i].3 - measured[0].3).abs() <= 1.0,
                "不同缩放级别下画面高必须一致：{measured:?}"
            );
        }
    }

    /// **P21 ② / P25 ③**：**放不下就按比例减少块数** 5 → 4 → 3 → 2 → 1（画面永不缩），
    /// 判据是 P25 的 [`strip_slot_count`]（`n × 画面宽 + (n−1) × 最小间隙 ≤ 可见宽`）。
    #[test]
    fn p21_strip_tile_count_follows_the_visible_width() {
        // 纯逻辑：画面宽 100、最小间隙 4（[`STRIP_MIN_GAP_PX`]）→ 每条线都正好卡在边界上
        let p = 100.0f32;
        assert_eq!(strip_slot_count(1000.0, p), 5);
        assert_eq!(
            strip_slot_count(516.0, p),
            5,
            "正好放下 5 个画面 + 4 个最小间隙"
        );
        assert_eq!(strip_slot_count(515.9, p), 4, "差一点点就降到 4 个");
        assert_eq!(strip_slot_count(412.0, p), 4, "正好放下 4 个 + 3 个间隙");
        assert_eq!(strip_slot_count(411.9, p), 3);
        assert_eq!(strip_slot_count(308.0, p), 3);
        assert_eq!(strip_slot_count(307.9, p), 2);
        assert_eq!(strip_slot_count(204.0, p), 2);
        assert_eq!(strip_slot_count(203.9, p), 1);
        assert_eq!(strip_slot_count(100.0, p), 1, "连一个画面都放不下也给 1 个");
        assert_eq!(strip_slot_count(0.0, p), 1);
        // 竖屏素材（画面只有 ~29 px）：同样宽度下能放下更多（不再被 160 槽位卡住）
        let portrait = 29.25f32;
        assert_eq!(strip_slot_count(162.25, portrait), 5);
        assert_eq!(strip_slot_count(162.0, portrait), 4);
        // 非法输入：1（绝不 panic、绝不 0）
        assert_eq!(strip_slot_count(f32::NAN, p), 1);
        assert_eq!(strip_slot_count(1000.0, 0.0), 1);
        assert_eq!(strip_slot_count(1000.0, f32::NAN), 1);
        // 界面侧：不同窗口宽度 → 块数按同一个规则（画面宽取该素材的 contain 宽）
        for w in [1024.0f32, 700.0, 500.0, 420.0, 320.0] {
            let mut h = Harness::with_screen(
                synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
                w,
                768.0,
            );
            h.frame(vec![]);
            let rect = h.timeline();
            let pic_w = strip_picture_width(strip_tile_height(), 320.0 / 240.0);
            let expect = strip_slot_count(rect.width(), pic_w);
            let (n, _) = strip_tiles_of(&h);
            assert_eq!(
                n,
                expect,
                "窗口宽 {w}：可见宽 {:.1} 应画出 {expect} 个画面，实测 {n}",
                rect.width()
            );
            assert_eq!(h.page.test_rects.strip_count, Some(expect));
            assert_eq!(
                h.page.test_rects.strip_pic_w,
                Some(pic_w),
                "画面宽只有一个来源"
            );
            // **窄条也要"首贴左、尾贴右"**（P25 明确：递减块数时这条不许破）
            let rects = strip_rects_of(&h);
            assert_eq!(rects.len(), expect);
            assert!(
                (rects[0].left() - rect.left()).abs() <= 1.0,
                "窗口宽 {w}：第 1 个画面必须贴左边界（{:.2} vs {:.2}）",
                rects[0].left(),
                rect.left()
            );
            if expect >= 2 {
                assert!(
                    (rects[expect - 1].right() - rect.right()).abs() <= 1.0,
                    "窗口宽 {w}：第 {expect} 个画面必须贴右边界（{:.2} vs {:.2}）",
                    rects[expect - 1].right(),
                    rect.right()
                );
            }
        }
    }

    /// **P21 ③**：取样时刻 = `可见起点 + k×可见时长/(n−1)`（5 块时就是 `…/4`），
    /// 落位**最左贴左、最右贴右、中间均匀**（与波形/刻度共用同一套时间→x 映射）。
    #[test]
    fn p21_strip_samples_hug_both_edges_and_are_evenly_spaced() {
        // 纯逻辑：5 块 → t0..t4 正是用户给的公式
        let s = strip_samples(10_000.0, 30_000.0, 600_000, 5);
        assert_eq!(
            s.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            vec![10_000, 15_000, 20_000, 25_000, 30_000],
            "取样时刻 = 可见起点 + k×可见时长/4"
        );
        assert_eq!(
            s.iter().map(|(_, f)| *f).collect::<Vec<_>>(),
            vec![0.0, 0.25, 0.5, 0.75, 1.0],
            "归一化位置必须线性内插（0 = 可见左端，1 = 可见右端）"
        );
        // 4 块 / 1 块：同一公式按 n−1 等分
        assert_eq!(
            strip_samples(0.0, 12_000.0, 600_000, 4)
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>(),
            vec![0, 4_000, 8_000, 12_000]
        );
        assert_eq!(
            strip_samples(0.0, 12_000.0, 600_000, 1)
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>(),
            vec![0],
            "单块取可见起点（左贴左）"
        );
        // 非法/退化输入：空结果，绝不 panic
        for (a, b, dur, n) in [
            (f64::NAN, 1.0, 1000, 5),
            (1.0, f64::NAN, 1000, 5),
            (5.0, 1.0, 1000, 5),
            (0.0, 1_000.0, 0, 5),
            (0.0, 1_000.0, 1000, 0),
        ] {
            assert!(
                strip_samples(a, b, dur, n).is_empty(),
                "非法输入必须给出空结果（{a}..{b} dur={dur} n={n}）"
            );
        }
        // 界面侧：量真实矩形 —— 首贴左、尾贴右、相邻间隙相等（三个缩放级别各来一遍）
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]);
        for vis in [120_000.0, 20_000.0, 4_000.0] {
            set_visible_ms(&mut h, vis);
            h.frame(vec![]);
            let rects = strip_rects_of(&h);
            let rect = h.timeline();
            assert!(!rects.is_empty());
            assert!(
                (rects[0].left() - rect.left()).abs() <= 1.0,
                "可见 {vis} ms：第 1 个画面必须贴条左边界（{:.2} vs {:.2}）",
                rects[0].left(),
                rect.left()
            );
            // **画面完整落在行内**（不越界、不裁切）：上下贴住行高、左右不越出条
            for r in &rects {
                assert!(
                    (r.top() - rect.top()).abs() <= 1.0
                        && (r.bottom() - (rect.top() + strip_tile_height())).abs() <= 1.0,
                    "可见 {vis} ms：画面必须正好占满行高（{:.2}..{:.2} vs {:.2}..{:.2}）",
                    r.top(),
                    r.bottom(),
                    rect.top(),
                    rect.top() + strip_tile_height()
                );
                assert!(
                    r.left() >= rect.left() - 1.0 && r.right() <= rect.right() + 1.0,
                    "可见 {vis} ms：画面完整落在条内（{:.2}..{:.2} vs {:.2}..{:.2}）",
                    r.left(),
                    r.right(),
                    rect.left(),
                    rect.right()
                );
            }
            if rects.len() >= 2 {
                assert!(
                    (rects[rects.len() - 1].right() - rect.right()).abs() <= 1.0,
                    "可见 {vis} ms：第 n 个画面必须贴条右边界（{:.2} vs {:.2}）",
                    rects[rects.len() - 1].right(),
                    rect.right()
                );
                // 均匀分布：**相邻画面之间的间隙相等**（按各自真实宽度算）
                let gap = rects[1].left() - rects[0].right();
                for w in rects.windows(2) {
                    let g = w[1].left() - w[0].right();
                    assert!(
                        (g - gap).abs() <= 1.0,
                        "可见 {vis} ms：相邻间隙必须相等（{gap:.2} vs {g:.2}）"
                    );
                }
                assert!(gap >= -0.5, "可见 {vis} ms：间隙不能为负（实测 {gap:.2}）");
            }
            // 取样时刻随可见区间变化（不是固定网格）
            let times = h
                .page
                .test_rects
                .strip_grid
                .clone()
                .expect("应登记取样时刻");
            assert_eq!(times.len(), rects.len());
            assert_eq!(times[0], 0, "视口从 0 开始 → 第 1 个画面的时刻就是 0");
            // **P27 ①**：末端从"容器/素材时长"改成"**视频流时长 − ε**"（本合成素材两者相同）
            let expect_last = (vis as i64).min(p27_sample_last_ms(h.duration_ms));
            assert_eq!(
                *times.last().unwrap(),
                expect_last,
                "最后一个画面的时刻 = 可见区间右端（夹到视频流 − ε）"
            );
        }
    }

    /// **P25 ②b（同源恒等式）**：等宽画面下，[`strip_layout`] 给出的左边界必须与
    /// `x_of(t_k) − frac_k × 画面宽` **逐点相等**（±0.01 px）——
    /// 这就是"落位与波形/刻度共用 `x_of`"这条口径的可判定形式。
    #[test]
    fn p25_strip_layout_matches_x_of_when_widths_are_equal() {
        // 模拟一帧：条 [100, 1100]，视图 x_of(t) = 100 + (t − 0) × 0.05
        let (left, right, px_per_ms) = (100.0f32, 1100.0f32, 0.05f64);
        let (t0, t1) = (0.0f64, 20_000.0f64);
        let x_of = |t: f64| (left as f64 + (t - t0) * px_per_ms) as f32;
        let samples = strip_samples(t0, t1, 600_000, 5);
        let pic_w = 120.0f32;
        let lefts_expected: Vec<f32> = samples
            .iter()
            .map(|(t, frac)| x_of(*t as f64) - *frac * pic_w)
            .collect();
        let slots = strip_layout(&samples, &[pic_w; 5], 52.0, x_of(0.0), right);
        assert_eq!(slots.len(), 5);
        for (k, s) in slots.iter().enumerate() {
            assert!(
                (s.left - lefts_expected[k]).abs() <= 0.01,
                "第 {k} 个画面：落位必须与 x_of 同源（{:.3} vs {:.3}）",
                s.left,
                lefts_expected[k]
            );
            assert_eq!(s.at_ms, samples[k].0, "取样时刻原样带出");
            assert_eq!(s.width, pic_w);
            assert_eq!(s.height, 52.0);
        }
        assert!((slots[0].left - left).abs() <= 0.01, "首贴左");
        assert!(
            (slots[4].left + slots[4].width - right).abs() <= 0.01,
            "尾贴右"
        );
        // 退化输入不 panic：空列表 / NaN / 右边界在左边界左边
        assert!(strip_layout(&[], &[], 52.0, 0.0, 100.0).is_empty());
        let bad = strip_layout(&[(0, 0.0), (1, 1.0)], &[50.0, 50.0], 52.0, f32::NAN, 10.0);
        assert_eq!(bad.len(), 2);
        assert!(bad.iter().all(|s| s.left.is_finite() && s.width > 0.0));
        // 装不下时（右边界在左边界左边 / 太窄）：间隙夹到 0，画面顺序不倒、位置不 NaN
        let narrow = strip_layout(&[(0, 0.0), (1, 1.0)], &[80.0, 80.0], 52.0, 0.0, 100.0);
        assert_eq!(narrow.len(), 2);
        assert!(
            narrow[1].left >= narrow[0].left + narrow[0].width - 0.01,
            "画面不许重叠倒序"
        );
    }

    /// **P21 ④**：平移改变取样时刻（跟着可见区间走），块数不变；
    /// 同一批取样时刻**跨缩放级别命中缓存**（不重复解码）。
    #[test]
    fn p21_strip_panning_moves_the_samples_and_cache_hits_across_zooms() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.page.strip_requests_enabled = true;
        h.page.kf_thumbs.clear();
        h.frame(vec![]);
        // 把 0..40 s 的**量化取样时刻**预先填好（模拟"这些块已经解过了"）。
        // P24：键 = [`strip_thumb_key`]（时刻 + 宽 + **高**），与生产同一个构造入口。
        let tile_h = strip_tile_height();
        for t in (0..=40_000i64).step_by(STRIP_QUANT_MS as usize) {
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x80u8; 16]);
            let tex = h
                .ctx
                .load_texture(format!("fake{t}"), img, egui::TextureOptions::NEAREST);
            h.page.strip_thumbs.insert(
                strip_thumb_key(t, 120_000, STRIP_TILE_W as u32, tile_h),
                CachedThumb { w: 2, h: 2, tex },
            );
        }
        // 上面那句 `kf_thumbs.clear()` 把测试台预填的**预览**纹理也清掉了。
        // 预览与胶片条**共用同一条"单张在途"**（`thumb_pending`），所以这里必须把预览
        // 那一侧的缓存补回来 —— 否则本帧预览会先占住在途名额，胶片条一个请求都发不出去，
        // ①②③ 就变成"因为预览在解码所以没请求"的假通过。
        // （P24 起两侧是**两张表**：胶片条键带高度、宽恒为 160；预览键 = `(关键帧, 预览档)`。）
        for t in h.page.clip.as_ref().unwrap().keyframes_ms.clone() {
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x40u8; 16]);
            let tex = h
                .ctx
                .load_texture(format!("kf{t}"), img, egui::TextureOptions::NEAREST);
            h.page
                .kf_thumbs
                .insert((t, THUMB_MIN_DECODE_W), CachedThumb { w: 2, h: 2, tex });
        }
        // ① 可见 20 s（从 0 开始）：5 块的量化时刻全在缓存里 → 一个新请求都不该发
        //    （只看**胶片条**的在途：预览的键是 `(关键帧, 预览尺寸档)`，与胶片条无关）
        set_visible_ms(&mut h, 20_000.0);
        h.page.thumb_pending = None;
        h.frame(vec![]);
        assert_eq!(
            h.page
                .thumb_pending
                .filter(|(_, w)| *w == STRIP_TILE_W as u32),
            None,
            "取样时刻 0/5/10/15/20 s 全在缓存里，不该再排胶片条解码"
        );
        // ② 平移 10 s：取样时刻整体右移，块数不变（缩放没变）
        h.page.view_start_ms = 10_000.0;
        h.page.thumb_pending = None;
        h.frame(vec![]);
        let times = h.page.test_rects.strip_grid.clone().unwrap();
        assert_eq!(
            times,
            vec![10_000, 15_000, 20_000, 25_000, 30_000],
            "平移之后取样时刻必须跟着可见区间走"
        );
        assert_eq!(times.len(), 5, "平移不改缩放 → 块数不变");
        // ③ 更窄的级别（4 s）落在缓存覆盖过的区间里：仍是 5 块、全部命中
        set_visible_ms(&mut h, 4_000.0);
        h.page.view_start_ms = 20_000.0;
        h.page.thumb_pending = None;
        for i in 0..4 {
            h.frame(vec![]);
            let strip_pending = h
                .page
                .thumb_pending
                .filter(|(_, w)| *w == STRIP_TILE_W as u32);
            assert_eq!(
                strip_pending, None,
                "更窄级别第 {i} 帧：取样时刻全在缓存里（0..40 s），不该再排胶片条解码"
            );
        }
        // ④ 反过来：跳到**没缓存过**的一段 → 必须重新请求（命中缓存不是"永远不请求"）
        h.page.view_start_ms = 90_000.0;
        h.page.thumb_pending = None;
        h.frame(vec![]);
        // 这一帧请求到**哪个键**：键 = [`strip_thumb_key`] = `(时刻, 宽, 高)`（P24）。
        // 90 s 起的取样时刻是 [90, 91, …, 94] s，其中 90/91/92/93 s 在预填的 0..40 s **之外**，
        // 所以"最靠左还没解的那个"就是 90 s 那一块。
        let want_key = strip_thumb_key(90_000, 120_000, STRIP_TILE_W as u32, strip_tile_height());
        assert_eq!(want_key.time, 90_000);
        assert_eq!(want_key.w, STRIP_TILE_W as u32);
        assert_eq!(want_key.h, strip_h_key(strip_tile_height()));
        // 这一张请求**确实发出去了**。三种观测点任一为真即可（同一帧内结果可能已经回来）：
        // * 仍在途：`thumb_pending == (90_000, 160)`；
        // * 已经回来：`strip_thumbs` 有那个带高度的键；
        // * 回来了但失败（测试台的"素材"是假路径，解码必然失败）：进了 `thumb_failed`。
        //
        // **为什么不再死等"帧末仍在途"**：解码线程与 UI 帧是并发的，"结果在同一帧内回来"
        // 是合法的；旧写法把"还没回来"当成硬条件，等于在赌线程调度。
        let h = &h.page;
        let issued = h.thumb_pending == Some((90_000, STRIP_TILE_W as u32))
            || h.strip_thumbs.contains_key(&want_key)
            || h.thumb_failed
                .contains(&(want_key.time, want_key.w, want_key.h));
        assert!(
            issued,
            "没缓存过的区间（90 s 起）必须重新请求缩略图；实测 pending={:?} strip_keys={} failed={}",
            h.thumb_pending,
            h.strip_thumbs.len(),
            h.thumb_failed.len()
        );
    }

    /// **P21 ⑤**：缩放/平移停下后界面**不自激重绘**（沿用 P15/P18/P19 的口径）。
    ///
    /// P21 明确要求"不得引入每帧重绘、删除任何旧的批线程/逐帧兜底"：
    /// 所以这里既查"停下来之后不再请求重绘"，也查"同一批取样时刻不会每帧重排"。
    #[test]
    fn p21_strip_does_not_self_excite_repaints() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]);
        // 先让"欠帧"排空
        let mut settled = false;
        for _ in 0..4 {
            h.frame(vec![]);
            if !h.ctx.has_requested_repaint() {
                settled = true;
                break;
            }
        }
        assert!(settled, "胶片条让界面永远在请求重绘（有周期性心跳）");
        for i in 0..5 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "静置第 {i} 帧：界面不该请求重绘（胶片条不许有心跳）"
            );
        }
        // 换缩放后仍然能停下来（不是"只在初始状态安静"）
        set_visible_ms(&mut h, 6_000.0);
        h.frame(vec![]);
        let mut settled = false;
        for _ in 0..4 {
            h.frame(vec![]);
            if !h.ctx.has_requested_repaint() {
                settled = true;
                break;
            }
        }
        assert!(settled, "换缩放后界面也必须能停下来（无自激）");
    }

    /// **P21（纯逻辑）**：量化键只做"圆整到 [`STRIP_QUANT_MS`]"，且**不改变画的时候的时刻**
    /// （画用未量化的时刻 → 贴边精确；解码用量化键 → 跨缩放命中缓存）。
    #[test]
    fn p21_strip_quantization_is_stable_and_snapped() {
        for t in [0i64, 1, 49, 50, 51, 99, 100, 12_345, 999_999] {
            let q = strip_quantized_ms(t);
            assert_eq!(q % STRIP_QUANT_MS, 0, "{t} 的量化键必须落在网格上");
            assert!(
                (q - t).abs() <= STRIP_QUANT_MS / 2 + 1,
                "{t} 的量化误差不能超过半个网格（得到 {q}）"
            );
            assert_eq!(strip_quantized_ms(q), q, "量化必须幂等");
        }
    }

    /// **P18 防回归（不自激）**：缩放停下来之后界面**完全静止** ——
    /// 既不请求重绘，也不再排新的细化请求（没有"缩放 → 请求 → 又重绘 → 又请求"的活锁，
    /// 也没有周期性心跳）。口径与 P15 / P14 那两条一致。
    #[test]
    fn p18_zooming_does_not_self_excite_repaints_or_requests() {
        let mut h = Harness::with_screen(
            synthetic_clip(60_000, vec![0, 10_000, 20_000, 30_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]);
        h.page.fit_mode = false;
        h.page.px_per_ms = fit_px_per_ms(1024.0, 60_000) * 40.0;
        h.page.view_start_ms = 5_000.0;
        let (tx, _refine_rx) = std::sync::mpsc::channel::<RefineReq>();
        let (_progress_tx, progress_rx) = std::sync::mpsc::channel::<()>();
        h.page.wave.job = Some(WaveJob {
            key: waveform::CacheKey::of(Path::new("p18-idle.mp4")),
            cancel: Arc::new(AtomicBool::new(false)),
            rx: progress_rx,
            req_tx: tx,
            plan: Arc::new(BasePlan::new(60_000)),
            ui_ctx: Arc::new(Mutex::new(None)),
            base_handles: Vec::new(),
            refine_handle: None,
            cancelled: false,
        });
        h.page.wave.last_refine_at = None;
        let a = h.page.view_start_ms;
        let b = (a + 1024.0 / h.page.px_per_ms).min(h.duration_ms as f64);
        h.page
            .wave
            .pump_refine(Some((a as i64, b as i64)), ((a + b) / 2.0) as i64);
        let sent_after_zoom = h.page.wave.requested.len();
        assert!(sent_after_zoom > 0, "前提：这一次确实排过请求");
        // egui 的"欠帧"要一两帧才排空（口径同 P15）：先允许它排空，但**必须停下来**
        let mut settled = false;
        for _ in 0..4 {
            h.frame(vec![]);
            if !h.ctx.has_requested_repaint() {
                settled = true;
                break;
            }
        }
        assert!(settled, "缩放停下后界面仍在请求重绘 —— 有自激 / 周期性心跳");
        // 排空之后再连跑 5 帧：一帧都不许再请求重绘，也不许再排新的细化请求
        for i in 0..5 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "静置第 {i} 帧：缩放已经停了，界面不该再请求重绘（自激 / 心跳）"
            );
            assert_eq!(
                h.page.wave.requested.len(),
                sent_after_zoom,
                "静置第 {i} 帧：同一档位下的片已经排过了，不该每帧重排一遍"
            );
        }
    }

    // =======================================================================
    // P23：先导入视频，后计算波形（严格两阶段）
    // =======================================================================

    /// **P23 的测试台**：真跑帧（真 `egui::Context`）、真走"进入导入阶段"那一句，
    /// 但**不碰真实素材**（`Harness::with_screen` 已经预填了预览与胶片条两侧的纹理缓存），
    /// 所以判据能不能成立完全由测试自己控制、帧间行为完全确定。
    ///
    /// 为什么**不调 `open()`**：`open()` 里的 `edit::open` 要真解封装一个文件，
    /// 而测试台的 `synthetic.mp4` 不存在 —— 那会走"打开失败"那条分支（另一条测试专门覆盖它）。
    /// 走 `begin_import_phase` 覆盖的正是 `open()` **成功**时干的那一件事
    /// （`open_loads_clip_into_page_state` 与 `p23_open_never_starts_the_waveform_task` 一起
    /// 把 `open()` 本体的两半也钉住了）。
    fn import_harness(duration_ms: i64, keyframes: Vec<i64>) -> Harness {
        let clip = synthetic_clip(duration_ms, keyframes, 30.0);
        let mut h = Harness::with_screen(clip.clone(), 1024.0, 768.0);
        h.page.strip_requests_enabled = false;
        h.page.begin_import_phase(clip.src.as_path());
        h
    }

    /// 把导入起点往前拨到"兜底点之后"（模拟"三个条件迟迟不就绪"）。
    fn push_import_past_the_fallback(page: &mut EditorPage) {
        page.import_started_at = Some(
            std::time::Instant::now()
                - std::time::Duration::from_millis(IMPORT_READY_TIMEOUT_MS + 500),
        );
    }

    /// **可见区间最优先：跨任务的"记忆"**（用户口径「优先计算当前视频显示区域波形」）。
    ///
    /// 要钉死的时序：4 条基准层 worker 是**一起跑**的，一起来就 `take_next()` 领任务；
    /// 而界面第一次 `publish_visible` 要等到**本帧绘制之后**。所以"上一次发布的可见区间"
    /// 必须在 `spawn_job` **起线程之前**写进新计划 —— 否则它们按"离片头最近"领走
    /// 0/1/2/3 段，用户明明看着第 50 分钟却先把片头算出来。
    ///
    /// 这里测两件事（纯逻辑：不起线程、不解码）：
    /// ① `publish_visible` 会把区间记进 `last_visible`（**没有任务时也要记** ——
    ///    "关掉波形再打开"那条路正是这种情况）；
    /// ② 用它预热出来的计划，**第一条** `take_next()` 就落在可见段上。
    #[test]
    fn visible_window_is_remembered_and_prewarms_the_next_plan() {
        let mut w = WaveState::default();
        assert!(w.last_visible.is_none(), "初始不该有可见区间");
        // ① 没有 job 时也要记住
        w.publish_visible(60_000, 65_000);
        assert_eq!(
            w.last_visible,
            Some((60_000, 65_000)),
            "publish_visible 必须把区间记下来（下一次起 worker 要预热）"
        );
        // ② 预热：与 `spawn_job` 里同一份口径（先夹取，再 set_visible）
        let dur = 65_000i64;
        let seg = 5_000i64;
        let plan = BasePlan::with_segment_ms(dur, seg);
        assert_eq!(plan.segment_count(), 13, "前提：65 s 按 5 s 分段 = 13 段");
        let (a, b) = w.last_visible.expect("刚记过");
        let a = a.clamp(0, dur);
        plan.set_visible(a, b.clamp(a, dur));
        // 判据是"第一条任务落在可见区间上"，不是"必须正好是第 12 段"：
        // 段边界与可见边界**重合**时，左右相邻的两段距离都是 0（`take_next` 同距离取小段号），
        // 所以第 11 段（55~60 s）与第 12 段（60~65 s）都算"贴着视口"。
        let first = plan.take_next().expect("13 段都待算，必须领得到");
        let (fa, fb) = waveform::segment_span_ms_of(first, dur, seg);
        assert!(
            fb >= 60_000 && fa <= 65_000,
            "第一条任务必须与可见区间 [60000, 65000) 相接（实测第 {first} 段 = {fa}..{fb}）——\
             绝不能是片头的第 0 段"
        );
        assert!(first >= 11, "第 {first} 段离视口太远，可见区间没有最优先");
        // 越过片尾的旧区间要能被夹住（换成一个更短的素材时会发生）
        let mut w2 = WaveState::default();
        w2.publish_visible(900_000, 960_000);
        let (a, b) = w2.last_visible.expect("刚记过");
        let a = a.clamp(0, dur);
        let plan2 = BasePlan::with_segment_ms(dur, seg);
        plan2.set_visible(a, b.clamp(a, dur));
        assert_eq!(
            plan2.take_next(),
            Some(12),
            "越界的旧区间夹取后应落在最后一段，而不是越界后失去排序意义"
        );
    }

    /// **P23 ①**：**导入阶段绝不启动波形**。
    ///
    /// 用户原话「要采用先导入视频，后计算波形」落成机械保证：`open()` 之后、
    /// "导入就绪"之前，波形**一个线程都没起**、**一次音频都没解**。
    /// 这里用四个可观测点钉死它：
    /// * `import_started_at.is_some()` —— 还在导入阶段；
    /// * `wave.job.is_none()` —— 连任务本体（含细化线程）都还没有；
    /// * `wave.opened_at.is_none()` —— `WaveState::start` **一次都没被调用过**
    ///   （它是启动的唯一入口，第一件事就是记 `opened_at`）；
    /// * `wave.base_handles_count() == 0` —— 基准层 worker 数为 0。
    #[test]
    fn p23_import_phase_never_starts_the_waveform_task() {
        let mut h = import_harness(60_000, vec![0, 10_000, 20_000, 30_000]);
        assert!(
            h.page.import_started_at.is_some(),
            "进入导入阶段之后应处在导入阶段"
        );
        assert!(!h.page.wave_phase_done, "导入阶段还没落定");
        assert!(
            h.page.wave_start_pending,
            "波形应登记成「导入就绪后自动开始」，而不是当场开始"
        );
        assert!(
            h.page.wave.opened_at.is_none(),
            "导入阶段绝不允许调用 WaveState::start（opened_at 是它的第一笔写入）"
        );
        assert!(h.page.wave.job.is_none(), "导入阶段不该有波形任务本体");
        assert_eq!(
            h.page.wave.base_handles_count(),
            0,
            "导入阶段基准层 worker 数必须为 0"
        );
        // 每一帧都要**先**检查"还没就绪"这一半：判据成立前一个线程都不许起。
        // （测试台预填了两侧纹理，所以第 1 帧就会判为就绪 —— 这正是"就绪即开始"的另一半，
        //  由 `p23_wave_starts_automatically_on_the_frame_after_import_is_ready` 覆盖。）
        let mut checked_before_ready = false;
        for i in 0..3 {
            if !h.page.editor_import_ready && !h.page.wave_phase_done {
                assert!(
                    h.page.wave.job.is_none() && h.page.wave.opened_at.is_none(),
                    "导入第 {i} 帧：还没就绪 → 波形一个线程都不许起"
                );
                assert_eq!(h.page.wave.base_handles_count(), 0, "worker 数必须一直是 0");
                checked_before_ready = true;
            }
            h.frame(vec![]);
        }
        assert!(
            checked_before_ready,
            "前提：至少要有一帧是「导入阶段尚未就绪」的状态"
        );
        // 就绪之后也必须**如实**：没有音轨的合成素材不起 worker（降级路径不变）
        assert!(h.page.editor_import_ready, "预填的纹理应当让判据成立");
        let d = h.page.import_decision.expect("应记下判据");
        assert!(d.ready() && !d.fallback, "应当是判据成立而不是兜底：{d:?}");
        assert!(
            d.video_info && d.preview && d.strip,
            "三条都要真的成立：{d:?}"
        );
        assert_eq!(
            h.page.wave.base_handles_count(),
            0,
            "没有音轨 → 起 0 个 worker（如实降级）"
        );
    }

    /// **P23 ①（另一半）**：`open()` **本体**也绝不启动波形 ——
    /// 它只登记"就绪后要开始的那一笔"。
    ///
    /// 用随包 ffmpeg 现做一段带音轨的真素材（缺 ffmpeg 就跳过）：
    /// 若 `open()` 还会当场 `wave.start`，那么这里 `wave.opened_at` 就会立刻有值、
    /// `base_handles_count()` 就会是 4 —— 两句话都抓得住这个回归。
    #[test]
    fn p23_open_never_starts_the_waveform_task() {
        let Some(src) = gen_mp4_with_audio("p23-open") else {
            println!("[skip] 本机随包 ffmpeg 不可用，跳过 P23 open() 两阶段防回归");
            return;
        };
        with_watchdog(60, move || {
            let mut page = EditorPage::new(&Config {
                last_video_path: None,
                ..Config::default()
            });
            // 这一条走的是**真** `open()`（真解封装、真探测流信息；P34 起在后台线程做）
            page.open(&src);
            page.wait_open();
            assert_eq!(
                page.loaded_path(),
                Some(src.as_path()),
                "open() 应载入该素材"
            );
            assert!(
                page.import_started_at.is_some(),
                "open() 之后应处在导入阶段（两阶段的第一阶段）"
            );
            assert!(
                page.wave_start_pending,
                "open() 应把波形登记成「导入就绪后自动开始」"
            );
            assert!(
                page.wave.opened_at.is_none(),
                "open() 绝不能当场启动波形（opened_at 必须还是 None）"
            );
            assert!(page.wave.job.is_none(), "open() 之后不该有波形任务本体");
            assert_eq!(
                page.wave.base_handles_count(),
                0,
                "open() 之后基准层 worker 数必须是 0（一次音频都不许解）"
            );
        });
    }

    /// **P23 ②**：**导入就绪之后自动开始波形**（无需用户点按钮），
    /// 且**可见区间最优先的细化请求已经发出**。
    ///
    /// 判据的三条在这里用测试台的既有手段显式满足：
    /// * 视频流信息 —— `synthetic_clip` 自带 `video`（320×240）；
    /// * 首帧预览已渲染出一帧 —— `Harness::with_screen` 预填了 `kf_thumbs`；
    /// * 胶片条首块已显示 —— `Harness::with_screen` 调了 `prefill_strip`。
    ///
    /// 所以第 1 帧画完、第 2 帧就该判为就绪并**当帧**把 `wave.start` 叫起来。
    #[test]
    fn p23_wave_starts_automatically_on_the_frame_after_import_is_ready() {
        let mut h = import_harness(60_000, vec![0, 10_000, 20_000, 30_000]);
        let mut started_at_frame = None;
        for i in 0..4 {
            h.frame(vec![]);
            if h.page.wave.opened_at.is_some() {
                started_at_frame = Some(i);
                break;
            }
        }
        let frame = started_at_frame.expect("导入就绪之后必须自动开始波形（无需用户操作）");
        assert!(
            frame <= 2,
            "就绪条件第 1 帧就已画出来 → 最迟第 2 帧就该开始（实测第 {frame} 帧）"
        );
        assert!(
            h.page.editor_import_ready && h.page.wave_phase_done,
            "两阶段应已落定"
        );
        let d = h.page.import_decision.expect("应记下判据");
        assert!(d.ready() && !d.fallback, "应当是判据成立而不是兜底：{d:?}");
        assert!(
            d.video_info && d.preview && d.strip,
            "三条都要真的成立：{d:?}"
        );
        assert!(
            !h.page.wave_start_pending,
            "启动请求一旦兑现就应当清掉（不会每帧重来一次）"
        );
        assert_eq!(
            h.page.wave.base_handles_count(),
            0,
            "没有音轨 → 不起 worker（如实降级）"
        );
    }

    /// **P23 ②（追加口径）**：可见区间优先请求**真的发出**了 ——
    /// 导入落定之后，放大到密集档位时 `WaveState::requested` 必须非空。
    ///
    /// 为什么单列一条：这是验收里"可见区间优先请求已发出"那句的可观测证据。
    #[test]
    fn p23_visible_range_refine_request_is_issued_after_import_is_ready() {
        let mut h = import_harness(60_000, vec![0, 10_000, 20_000, 30_000]);
        let mut ready = false;
        for _ in 0..4 {
            h.frame(vec![]);
            if h.page.wave.opened_at.is_some() {
                ready = true;
                break;
            }
        }
        assert!(ready, "导入就绪之后波形应自动启动");
        // 合成素材没有音轨 → 上面那次 `wave.start` 走的是"如实降级"分支
        // （`cancel_current` 把 `last_good` 清掉、并且不起任务本体）。
        // 这里把测试台的假数据与假任务**重新**装上（口径同 P18/P20 的测试台）：
        // 于是"本帧选哪一层 / 会不会为可见区间排请求"就只由可见时长决定。
        let _rx = fake_wave_job(&mut h.page, "p23-visible");
        seed_l0(&mut h.page, 60_000);
        // 放大到"该走密集层"的档位（档位由可见时长决定）
        set_visible_ms(&mut h, 4_000.0);
        h.page.wave.last_refine_at = None; // 绕过"手停 120ms"的防抖
        h.page.wave.requested.clear();
        h.frame(vec![]);
        let t = &h.page.test_rects;
        assert_eq!(
            t.wave_detail,
            Some(true),
            "可见 4 s 必须走密集层（可见区间优先请求的前提）"
        );
        assert!(
            !h.page.wave.requested.is_empty(),
            "导入就绪之后必须为可见区间发出细化请求（可见区间最优先）"
        );
        assert!(
            h.page
                .wave
                .requested
                .iter()
                .all(|(_, k)| *k == t.wave_tier_ms.expect("密集层必须给出档位")),
            "已请求的键必须都带当前档位"
        );
    }

    /// **P23 ③**：**兜底触发** —— 某个就绪条件永远不满足时，波形**仍然会开始**，
    /// 并且判据里如实记下"这次是兜出来的"（`ImportDecision::fallback`）。
    ///
    /// 构造手法（不改生产代码、也不加测试专用开关）：
    /// 1. 先让 `strip_prev` 永远为空（把胶片条缓存清掉、它就不会往 `strip_prev` 里放东西）；
    /// 2. 把 `import_started_at` 往前拨过兜底点 —— 与真机上"某个条件迟迟不就绪"走的是
    ///    **同一个判定入口**（`import_readiness`）。
    #[test]
    fn p23_fallback_timeout_still_starts_the_waveform() {
        let mut h = import_harness(60_000, vec![0, 10_000, 20_000, 30_000]);
        // 让判据③（胶片条首块已显示）永远不成立：清掉胶片条那侧的缓存
        h.page.strip_thumbs.clear();
        h.page.strip_prev.clear();
        h.frame(vec![]);
        let before = h.page.import_decision.expect("应记下判据");
        assert!(
            !before.ready(),
            "前提：判据不该成立（胶片条那一侧没有画面）{before:?}"
        );
        assert!(!before.strip, "前提：胶片条那一侧确实不成立");
        assert!(
            h.page.wave.opened_at.is_none(),
            "前提：还没到兜底时间 → 波形还没开始"
        );
        // 把导入起点往前拨过兜底点
        push_import_past_the_fallback(&mut h.page);
        h.frame(vec![]);
        let d = h.page.import_decision.expect("应记下判据");
        assert!(d.fallback, "超时之后必须走兜底：{d:?}");
        assert!(d.ready(), "兜底也算就绪（否则波形永远不开始）");
        assert!(
            h.page.wave.opened_at.is_some(),
            "兜底触发之后波形必须已经开始（不得永远不开始）"
        );
        assert!(
            h.page.editor_import_ready && h.page.wave_phase_done,
            "兜底同样要把两阶段落定"
        );
        assert!(
            h.page.import_started_at.is_none(),
            "落定之后不该再留在导入阶段（否则看门狗会一直排帧）"
        );
    }

    /// **P23 ③（纯逻辑）**：兜底的时间判据 —— 只有 `elapsed >= timeout` 才算兜底；
    /// `timeout_ms = None`（单测用）时永远不兜底，判据就只有那三条。
    #[test]
    fn p23_import_readiness_is_pure_and_timeout_is_the_only_fallback() {
        // 三条全中：就绪，且不是兜底
        let d = import_readiness(true, true, true, 0, Some(IMPORT_READY_TIMEOUT_MS));
        assert!(d.ready() && !d.fallback);
        // 差一条：不就绪（时间也没到）
        let d = import_readiness(true, true, false, 10, Some(IMPORT_READY_TIMEOUT_MS));
        assert!(!d.ready() && !d.fallback, "差一条就不算就绪：{d:?}");
        // 差一条 + 到点：兜底就绪，且如实标出"用了兜底"
        let d = import_readiness(
            true,
            true,
            false,
            IMPORT_READY_TIMEOUT_MS,
            Some(IMPORT_READY_TIMEOUT_MS),
        );
        assert!(d.ready() && d.fallback, "到点必须兜底：{d:?}");
        assert!(
            d.detail().contains("fallback=true"),
            "日志摘要要写明兜底：{}",
            d.detail()
        );
        // 恰好差 1 ms：还不兜底（边界口径）
        let d = import_readiness(
            false,
            false,
            false,
            IMPORT_READY_TIMEOUT_MS - 1,
            Some(IMPORT_READY_TIMEOUT_MS),
        );
        assert!(!d.ready(), "还没到点、判据也不成立 → 不许就绪");
        // 不做超时兜底时：时间再大也不就绪
        let d = import_readiness(false, false, false, u64::MAX, None);
        assert!(!d.ready() && !d.fallback, "timeout=None 时不存在兜底");
    }

    /// **P23 ④**：**没有视频流 → 不启动波形、不 panic**，并按既有降级路径收尾。
    #[test]
    fn p23_no_video_stream_does_not_start_waveform_and_does_not_panic() {
        let mut h =
            Harness::with_screen(synthetic_clip(60_000, vec![0, 10_000], 30.0), 1024.0, 768.0);
        h.page.strip_requests_enabled = false;
        // 把 clip 换成"没有视频流"的那一种（模拟探测不到视频流）
        let mut clip = synthetic_clip(60_000, vec![], 30.0);
        clip.video = None;
        assert!(clip.audio.is_none(), "前提：这条素材也没有音轨");
        clip.src = PathBuf::from("synthetic-no-video.mp4");
        h.page.clip = Some(clip.clone());
        // 走真正的"进入导入阶段"那一句（`open()` 里成功载入那条分支用的就是它）
        h.page.begin_import_phase(&clip.src);
        h.frame(vec![]);
        // 三条判据里"视频流信息"不成立 → 到点前不启动
        assert!(
            h.page.wave.job.is_none() && h.page.wave.opened_at.is_none(),
            "判据不成立时不许启动波形"
        );
        // 到点兜底 → 启动（`WaveState::start` 对"没有音轨/没有视频流"本来就是如实降级）
        push_import_past_the_fallback(&mut h.page);
        h.frame(vec![]);
        assert!(
            h.page.wave.opened_at.is_some(),
            "兜底之后波形阶段必须落定（走既有降级路径，不留在门口）"
        );
        assert_eq!(
            h.page.wave.base_handles_count(),
            0,
            "没有音轨 → 一个 worker 都不起（如实降级）"
        );
        assert!(
            h.page.wave.job.is_none(),
            "没有可用的底层数据 → 任务本体也不该留着"
        );
        // 再连跑几帧：不 panic、也不反复启动
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(h.page.wave.job.is_none(), "降级之后不该又冒出任务来");
    }

    /// **P23 ④（另一条降级路径）**：**打开失败** → 不启动波形、不 panic，
    /// 而且**波形阶段照样落定**（`wave_phase_done = true`）——
    /// 不然 `check_wave_phase` 会一直认为"还在导入"，看门狗会白排帧。
    #[test]
    fn p23_open_failure_finishes_import_without_starting_waveform() {
        let mut h = Harness::with_screen(synthetic_clip(60_000, vec![0], 30.0), 1024.0, 768.0);
        h.page.strip_requests_enabled = false;
        h.page.clip = None;
        // P46：这里原来要先把输出目录指到一个空目录，才能"关掉自动载入最近录制"这条旁路
        // （否则 `ui()` 每帧看到 `clip.is_none()` 就会从本机录制目录里塞一条进来，
        // 把"打开失败"的状态盖掉）。自动载入已经删掉，这段绕行也就不需要了。
        // 这个路径不存在 → `edit::open_streaming` 必然失败（P34：在后台线程里失败）
        h.page.open(Path::new("does-not-exist-p23.mp4"));
        h.page.wait_open();
        assert!(h.page.clip.is_none(), "打开失败 → 不持有 clip");
        assert!(
            h.page.error.is_some(),
            "打开失败应给出一句界面提示（既有行为不变）"
        );
        assert!(
            h.page.wave_phase_done,
            "打开失败也必须把波形阶段放行（否则看门狗会一直排帧）"
        );
        assert!(h.page.import_started_at.is_none(), "不该留在导入阶段");
        assert!(
            h.page.wave.opened_at.is_none() && h.page.wave.job.is_none(),
            "打开失败 → 不启动波形、不起任何线程"
        );
        // **没有心跳**的判据在这里要看"有没有排过兜底看门狗"（那是本页唯一一处
        // 会自己 `request_repaint_after` 的地方）；egui 自己的"欠帧"要一两帧才排空，
        // 用 `has_requested_repaint()` 直接断言会把它误判成心跳（口径同 P15/P21）。
        assert!(
            h.page.import_watchdog.is_none(),
            "失败路径不该留着兜底看门狗"
        );
        for i in 0..3 {
            h.frame(vec![]);
            assert!(
                h.page.import_watchdog.is_none(),
                "失败路径第 {i} 帧：不该再排兜底看门狗帧"
            );
            assert!(
                h.page.wave_phase_done && h.page.import_started_at.is_none(),
                "失败路径第 {i} 帧：阶段状态必须一直是「已放行、不在导入」"
            );
        }
        // 排空 egui 的欠帧之后，界面必须彻底静止
        for _ in 0..4 {
            h.frame(vec![]);
        }
        for i in 0..3 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "失败路径静置第 {i} 帧：不该请求重绘（没有心跳）"
            );
        }
    }

    /// **P23 ⑥**：**兜底看门狗是有限次的、并且一定会停** ——
    /// 既保证"波形不会因为没帧而永远不开始"，也保证"没有每帧重绘/心跳"。
    ///
    /// 观测：判据不成立时每帧排一次看门狗（`check_wave_phase` 返回 `true`），
    /// 但一旦时间走过兜底点，函数立刻**不再排帧**（返回 `false`）并落定。
    #[test]
    fn p23_import_watchdog_is_bounded_and_stops() {
        // 纯逻辑口径：把时间轴按看门狗间隔离散化，数"会排帧的次数"。
        let ticks = IMPORT_READY_TIMEOUT_MS / IMPORT_WATCHDOG_FRAME_MS;
        assert!(
            ticks >= 1,
            "看门狗间隔必须小于兜底时间（否则一拍都排不出来）"
        );
        assert!(
            ticks <= 64,
            "看门狗排帧次数必须有界且不大（现在 {ticks} 拍）—— 它不是心跳"
        );
        // 界面口径：判据不成立 → 排帧；到点 → 落定且不排帧
        let mut h = import_harness(60_000, vec![0, 5_000, 10_000]);
        h.page.strip_thumbs.clear();
        h.page.strip_prev.clear();
        h.frame(vec![]);
        assert!(
            h.ctx.has_requested_repaint(),
            "判据未成立时应排一次兜底看门狗帧（否则兜底永远没机会生效）"
        );
        assert!(!h.page.wave_phase_done, "还没到点，不该落定");
        // 到点：落定 → 不再排帧
        push_import_past_the_fallback(&mut h.page);
        h.frame(vec![]);
        assert!(h.page.wave_phase_done, "到点必须落定");
        // 排空 egui 的欠帧之后，界面必须彻底静止
        for _ in 0..4 {
            h.frame(vec![]);
        }
        assert!(
            !h.ctx.has_requested_repaint(),
            "落定之后界面必须静止（兜底看门狗不许变成心跳）"
        );
        for i in 0..3 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "落定后静置第 {i} 帧：不该再请求重绘"
            );
        }
    }

    /// **P23 附加**：用户把波形关掉时，**导入就绪也不会把它长回来**
    /// （"关掉就是关掉"，与既有 `set_show_waveform(false)` 的口径一致）。
    #[test]
    fn p23_turning_the_waveform_off_wins_over_the_auto_start() {
        let mut h = import_harness(60_000, vec![0, 10_000, 20_000]);
        assert!(h.page.wave_start_pending, "前提：已登记「就绪后自动开始」");
        h.page.set_show_waveform(false);
        assert!(
            !h.page.wave_start_pending,
            "关掉波形必须连「就绪后自动开始」那一笔一起作废"
        );
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(
            h.page.editor_import_ready,
            "判据仍然会被评估（导入照常落定）"
        );
        assert!(
            h.page.wave.opened_at.is_none() && h.page.wave.job.is_none(),
            "用户关着波形 → 就绪也不许启动（不白烧 CPU）"
        );
    }

    // =======================================================================
    // P24 + P25：画面高 = 行高，画面按行高等比缩放（contain，完整显示）
    // =======================================================================

    /// 造一个指定**源宽高比**的合成素材（宽高只是数字，用来定 contain 的画面宽）。
    fn synthetic_clip_sized(video_w: i32, video_h: i32) -> Clip {
        let mut c = synthetic_clip(10_000, vec![0, 2_000, 4_000, 6_000], 30.0);
        if let Some(v) = c.video.as_mut() {
            v.width = video_w;
            v.height = video_h;
        }
        c
    }

    /// **P24 ① + P25 ①**：**画面高 = 行高、画面宽 = 行高 × 源宽高比**，且**与窗口尺寸无关**。
    ///
    /// * 行高的唯一来源是 [`THUMB_BAND_H`]（`strip_tile_height()` 直接返回它）；
    /// * 实际**绘制出来的矩形**高必须等于它、宽必须等于 [`strip_picture_width`] 的结果；
    /// * 换窗口大小（宽/高都换）之后这两个数**一个都不许变**（用户确认行高不随窗口变）；
    /// * `STRIP_TILE_W`（160）仍然存在，但它只是**解码请求宽**，不再是布局宽。
    #[test]
    fn p24_strip_tile_height_is_the_row_height_and_width_stays_160() {
        let row_h = strip_tile_height();
        let pic_w = strip_picture_width(row_h, 320.0 / 240.0);
        assert_eq!(
            row_h, THUMB_BAND_H,
            "画面高必须严格等于胶片条行高（P24 用户口径：只适配高度）"
        );
        assert_eq!(
            STRIP_TILE_W, 160.0,
            "160 只用于解码请求宽（P25：布局与它无关）"
        );
        assert!(
            (pic_w - row_h * 4.0 / 3.0).abs() <= 0.01,
            "画面宽 = 行高 × 源宽高比（4:3 素材实测 {pic_w:.2}）"
        );
        for (w, hh) in [(1024.0f32, 768.0f32), (1400.0, 900.0), (900.0, 600.0)] {
            let mut h = Harness::with_screen(
                synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
                w,
                hh,
            );
            h.frame(vec![]);
            let rects = strip_rects_of(&h);
            assert!(!rects.is_empty(), "窗口 {w}×{hh}：应画出胶片条画面");
            for r in &rects {
                assert!(
                    (r.height() - row_h).abs() <= 1.0,
                    "窗口 {w}×{hh}：画面绘制高必须是行高 {row_h}（实测 {:.2}）",
                    r.height()
                );
                assert!(
                    (r.width() - pic_w).abs() <= 1.0,
                    "窗口 {w}×{hh}：画面绘制宽必须是「行高 × 源宽高比」= {pic_w:.2}（实测 {:.2}）",
                    r.width()
                );
            }
            // P21/P25 的"5→4→3→2→1 块数规则"照旧生效（判据换成 P25 的窄条判据）
            assert_eq!(
                h.page.test_rects.strip_count,
                Some(strip_slot_count(h.timeline().width(), pic_w)),
                "窗口 {w}×{hh}：5→4→3→2→1 的块数规则不受 P24/P25 影响"
            );
            assert_eq!(h.page.test_rects.strip_pic_w, Some(pic_w));
        }
    }

    /// **P25 ①（contain 完整显示）**：竖屏 1080×1920 与横屏 1920×1080 各一例 ——
    /// * 绘制画面的**宽高比 = 源宽高比**（±1%，不变形）；
    /// * **高 = 行高 ±1px**（贴行高）；
    /// * **画面完整落在行内**（上下贴住行、左右不越出条 → 没有被裁掉任何一边）。
    ///
    /// 旧口径（cover：160 宽槽位 + 纵向裁）下，竖屏这一例会得到一个宽高比 ~3.1 的
    /// "中间横带"，`r.width()/r.height()` 与 0.5625 相差 5 倍以上 —— 这条测试就是那个回归的钉子。
    #[test]
    fn p25_strip_shows_the_whole_picture_in_both_orientations() {
        for (vw, vh) in [(1080i32, 1920i32), (1920, 1080)] {
            let src_ar = vw as f32 / vh as f32;
            let mut h = Harness::with_screen(synthetic_clip_sized(vw, vh), 1024.0, 768.0);
            h.frame(vec![]);
            let row_h = strip_tile_height();
            let rect = h.timeline();
            let rects = strip_rects_of(&h);
            assert_eq!(rects.len(), 5, "{vw}×{vh}：全片视图下应画出 5 个画面");
            let expect_w = strip_picture_width(row_h, src_ar);
            for (i, r) in rects.iter().enumerate() {
                let drawn_ar = r.width() / r.height();
                assert!(
                    (drawn_ar - src_ar).abs() / src_ar <= 0.01,
                    "{vw}×{vh} 第 {i} 个：绘制宽高比必须等于源宽高比 {src_ar:.4}（实测 {drawn_ar:.4}）"
                );
                assert!(
                    (r.height() - row_h).abs() <= 1.0,
                    "{vw}×{vh} 第 {i} 个：高必须是行高 {row_h}（实测 {:.2}）",
                    r.height()
                );
                assert!(
                    (r.width() - expect_w).abs() <= 1.0,
                    "{vw}×{vh} 第 {i} 个：宽必须是行高 × 源宽高比 = {expect_w:.2}（实测 {:.2}）",
                    r.width()
                );
                // 完整落在行内：上下贴住行高那一带、左右不越出条 → 四个边一个都没被切
                assert!(
                    (r.top() - rect.top()).abs() <= 1.0
                        && (r.bottom() - (rect.top() + row_h)).abs() <= 1.0,
                    "{vw}×{vh} 第 {i} 个：上下必须正好占满行高（{:.2}..{:.2}）",
                    r.top(),
                    r.bottom()
                );
                assert!(
                    r.left() >= rect.left() - 1.0 && r.right() <= rect.right() + 1.0,
                    "{vw}×{vh} 第 {i} 个：左右必须在条内（{:.2}..{:.2} vs {:.2}..{:.2}）",
                    r.left(),
                    r.right(),
                    rect.left(),
                    rect.right()
                );
            }
            // 竖屏画面比横屏窄得多（这正是"完整显示"的直接后果）
            let portrait_w = strip_picture_width(row_h, 1080.0 / 1920.0);
            let landscape_w = strip_picture_width(row_h, 1920.0 / 1080.0);
            assert!(
                portrait_w < 32.0 && landscape_w > 90.0,
                "竖屏 ≈29px、16:9 ≈92px"
            );
        }
    }

    /// **P24 ② + P25 ④**：**高度与口径版本进缓存键** —— `(量化时刻, 解码宽, 高, 版本)`。
    ///
    /// 行高当前是常量，所以这条测试要证明的是"键里**真的**带了高度与版本"：
    /// 同一个时刻、同一个宽度，只要高度不同（或口径版本不同）就是**两个不同的键**
    /// （行高/口径一变，缓存不会互相污染）。
    #[test]
    fn p24_thumb_cache_key_includes_the_height() {
        let w = STRIP_TILE_W as u32;
        let h52 = strip_tile_height();
        let key_a = strip_thumb_key(10_000, 60_000, w, h52);
        let key_b = strip_thumb_key(10_000, 60_000, w, 40.0);
        let key_c = strip_thumb_key(10_000, 60_000, w, 80.0);
        assert_eq!(
            (key_a.time, key_a.w, key_a.h, key_a.v),
            (10_000, 160, strip_h_key(h52), STRIP_THUMB_VERSION)
        );
        assert_ne!(key_a, key_b, "同一个时刻/宽度、不同高度必须是不同的键");
        assert_ne!(key_a, key_c, "同一个时刻/宽度、不同高度必须是不同的键");
        assert_ne!(key_b, key_c, "不同高度之间也不能撞键");
        // 时刻与宽度也还都在键里（P21 的两条纪律没被 P24 顶掉）
        assert_ne!(key_a, strip_thumb_key(10_100, 60_000, w, h52));
        assert_ne!(key_a, strip_thumb_key(10_000, 60_000, 320, h52));
        // **P25 ④：口径版本在键里**（cover 槽位 → contain 完整显示）
        assert_eq!(
            STRIP_THUMB_VERSION, 2,
            "P25 把口径从 cover 改成 contain 完整显示 → 版本必须是 2（旧缓存整批作废）"
        );
        assert_ne!(
            key_a,
            StripThumbKey {
                v: STRIP_THUMB_VERSION + 1,
                ..key_a
            },
            "同一个 (时刻, 宽, 高) 在另一个口径版本下必须是另一个键"
        );
        // 高度量化：同一个"档"里的两个高度必须落成同一个键（否则缓存会被切碎）
        let h_a = strip_h_key(52.0);
        let h_b = strip_h_key(52.0 + STRIP_H_QUANT_PX * 0.4);
        assert_eq!(h_a, h_b, "同一个高度档内的两个高度必须同键");
        // 非法输入不 panic、也不产生 0 键
        assert_eq!(strip_h_key(f32::NAN), 1);
        assert_eq!(strip_h_key(0.0), 1);
        assert_eq!(strip_h_key(-5.0), 1);
        assert!(strip_h_key(f32::INFINITY) >= 1);
        // 请求与查表必须是同一个量化函数（P21 那条纪律在 P24 上的延续）：
        // `strip_thumb_key` 是唯一入口，所以"键相等"这件事天然成立。
        assert_eq!(
            strip_thumb_key(10_000, 60_000, w, strip_tile_height()).h,
            strip_h_key(strip_tile_height())
        );
        // **P25：画面宽不是键的一部分，但它由 (高, 源宽高比) 唯一决定** ——
        // 所以"同一时刻 + 同一高度 + 同一版本"不会把竖屏的整幅画面与横屏的混用
        // （换文件时 `open()` 会整表清空，见那里的注释）。
        let portrait = strip_picture_width(h52, 1080.0 / 1920.0);
        let landscape = strip_picture_width(h52, 1920.0 / 1080.0);
        assert!(
            portrait < landscape,
            "源宽高比不同 → 画面宽不同（{portrait} vs {landscape}）"
        );
        // 非法输入不 panic、不产生畸形宽
        assert!(strip_picture_width(f32::NAN, 1.7778) > 0.0);
        assert!(strip_picture_width(52.0, f32::NAN) > 0.0);
        assert!(strip_picture_width(52.0, 0.0) > 0.0);
        assert!(strip_picture_width(-1.0, -1.0) > 0.0);
    }

    /// **P24 ②（界面侧）**：**换了高度档 → 查表查不到旧高度的缓存**（不串画面），
    /// 而且**画出来的矩形仍然是行高**（源尺寸/裁剪逻辑不受影响）。
    ///
    /// 手法：把胶片条缓存里塞一张"只属于另一个高度档"的纹理，本帧的查表必须**查不到**
    /// （返回 0 个可画块），从而证明高度确实参与了查表。
    #[test]
    fn p24_strip_cache_does_not_mix_heights() {
        let mut h = Harness::with_screen(
            synthetic_clip(60_000, vec![0, 10_000, 20_000], 30.0),
            1024.0,
            768.0,
        );
        h.page.strip_requests_enabled = false;
        // 默认测试台已经预填了"当前高度"那一档的块 → 第一帧能画出块
        h.frame(vec![]);
        assert!(
            !strip_rects_of(&h).is_empty(),
            "前提：当前高度档的块已经在缓存里 → 能画出画面"
        );
        // 清掉当前这一档、只留"另一个高度档"的同一批时刻
        let other_h = strip_tile_height() + STRIP_H_QUANT_PX * 4.0;
        let samples = strip_samples(0.0, 60_000.0, 60_000, 5);
        h.page.strip_thumbs.clear();
        for (t, _) in &samples {
            let key = strip_thumb_key(*t, 60_000, STRIP_TILE_W as u32, other_h);
            assert_ne!(
                key.h,
                strip_h_key(strip_tile_height()),
                "前提：确实是另一个高度档"
            );
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x11u8; 16]);
            let tex = h
                .ctx
                .load_texture(format!("other{t}"), img, egui::TextureOptions::NEAREST);
            h.page
                .strip_thumbs
                .insert(key, CachedThumb { w: 2, h: 2, tex });
        }
        h.frame(vec![]);
        assert!(
            strip_rects_of(&h).is_empty(),
            "只有「别的高度档」的缓存时，本帧必须一块都画不出来（高度不许串）"
        );
        assert!(
            h.page
                .test_rects
                .strip_tiles
                .as_ref()
                .is_some_and(|v| v.is_empty()),
            "`strip_tiles` 应当如实登记「这一帧一块都没画」"
        );
        // 把当前档补回来 → 立刻又能画，且矩形高 = 行高
        for (t, _) in &samples {
            let key = strip_thumb_key(*t, 60_000, STRIP_TILE_W as u32, strip_tile_height());
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0x22u8; 16]);
            let tex = h
                .ctx
                .load_texture(format!("cur{t}"), img, egui::TextureOptions::NEAREST);
            h.page
                .strip_thumbs
                .insert(key, CachedThumb { w: 2, h: 2, tex });
        }
        h.frame(vec![]);
        let rects = strip_rects_of(&h);
        assert!(!rects.is_empty(), "当前档补回来之后应当又能画出块");
        for r in &rects {
            assert!(
                (r.height() - strip_tile_height()).abs() <= 1.0,
                "块的绘制高必须还是行高（实测 {:.2}）",
                r.height()
            );
        }
    }

    /// **P25 ①（uv 侧）**：**contain = 整张纹理**，绝不裁掉画面的一部分。
    ///
    /// 取代 P24 的 `strip_cover_uv`（那个是"按格子比例裁掉多出来的一边"，竖屏素材因此
    /// 只剩中间一条横带）。现在格子本身就是"行高 × 源宽高比"，所以：
    /// * 没被条裁到时 uv 必须是**整张纹理** `(0,0)-(1,1)`（四个边一个都不裁）；
    /// * 被条裁掉一部分时，uv 按比例只露出那一块（否则贴图会整体平移）；
    /// * 退化输入不 panic、不产生畸形 uv。
    #[test]
    fn p24_strip_cover_uv_never_distorts() {
        // 竖屏画面：格子 29.25×52（= 行高 × 1080/1920），纹理同比例 → 整张纹理
        let full = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(29.25, 52.0));
        let uv = strip_contain_uv(full, full);
        assert_eq!(
            (uv.left(), uv.top(), uv.right(), uv.bottom()),
            (0.0, 0.0, 1.0, 1.0),
            "contain：完整显示 = 用整张纹理（一个像素都不裁）"
        );
        // 被条裁掉右边一半：uv 的宽也只有一半（画面被切掉的那部分不画）
        let half = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(14.625, 52.0));
        let uv_half = strip_contain_uv(full, half);
        assert!(
            (uv_half.width() - 0.5).abs() <= 0.01 && (uv_half.height() - 1.0).abs() <= 0.01,
            "只露出左半 → uv 宽也是一半：{uv_half:?}"
        );
        // 只露出纵向中间一条：uv 高按比例（这是"条比画面还窄"的极端窄条）
        let band = egui::Rect::from_min_size(egui::pos2(0.0, 13.0), egui::vec2(29.25, 26.0));
        let uv_band = strip_contain_uv(full, band);
        assert!(
            (uv_band.height() - 0.5).abs() <= 0.01 && (uv_band.width() - 1.0).abs() <= 0.01,
            "露出中间一半高 → uv 高也是一半：{uv_band:?}"
        );
        // 退化输入不许 panic
        let bad = strip_contain_uv(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.0, 0.0)),
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(0.0, 0.0)),
        );
        assert!(
            bad.width() > 0.0 && bad.height() > 0.0,
            "退化输入也不产生畸形 uv"
        );
    }

    // =======================================================================
    // P27 · 取样按视频流时长 + 失败时按方向有界回退（+ 兜底链：邻块 / 缓存 / 留空）
    // =======================================================================

    /// **P27**：造一个"**视频流比容器短**"的合成素材（实机那条报错的成因）。
    ///
    /// 容器时长 = `container_ms`，视频流自报时长 = `video_ms`（`< container_ms`）：
    /// 于是"按容器时长取样"的最后一块必然落到只有音频的尾巴里。
    fn synthetic_clip_split(container_ms: i64, video_ms: i64) -> Clip {
        let mut c = synthetic_clip(
            container_ms,
            vec![0, 1_000, 2_000, 3_000, 4_000, 5_000],
            30.0,
        );
        if let Some(v) = c.video.as_mut() {
            v.duration_ms = video_ms;
        }
        c
    }

    /// **P27 复现素材**：`视频流 6 s + 音轨 12 s` —— 容器总时长明显长于视频流。
    ///
    /// 返回 `None` 表示环境不具备（缺随包 ffmpeg / 缺 libopenh264）→ 调用方跳过而不是失败。
    fn gen_mp4_split(tag: &str) -> Option<PathBuf> {
        let ff = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
            .join("bin")
            .join("ffmpeg.exe");
        if !ff.is_file() {
            println!("[skip] 找不到随包 ffmpeg：{}", ff.display());
            return None;
        }
        let out = asset_dir().join(format!("p27-split-{tag}.mp4"));
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
                "testsrc2=size=320x240:rate=30:duration=6",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=12",
                "-map",
                "0:v",
                "-map",
                "1:a",
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
            ])
            .arg(&out)
            .output()
            .ok()?;
        if o.status.success() && out.is_file() {
            Some(out)
        } else {
            println!(
                "[skip] 生成 P27 复现素材失败：{}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
    }

    /// **P27**：跑帧，直到"胶片条那一侧没有在途解码**且**兜底/失败计数不再变"。
    ///
    /// 兜底链每一级都要等后台解码线程回包（失败是异步的），所以测试必须"跑帧 + 等稳定"，
    /// 不能只跑一帧就断言。
    ///
    /// **为什么按"墙钟预算"而不是"帧数"**：进程内的**第一次** FFmpeg 调用要付
    /// "延迟加载 5 个 DLL + 冷盘"的钱（P22 实测 **0.88–1.21 s**）。按帧数给预算时，
    /// 单独跑某一条用例（没有别的用例先替它把 FFmpeg 热起来）就会把预算全花在那一次上，
    /// 变成"看运气的偶发失败"。这里给 20 s 墙钟 + 稳定即早退：正常路径 **几十毫秒**返回。
    fn p27_run_strip_idle(h: &mut Harness, budget: std::time::Duration) -> usize {
        let t0 = Instant::now();
        let mut last = (usize::MAX, usize::MAX);
        let mut stable = 0usize;
        let mut frames = 0usize;
        while t0.elapsed() < budget {
            h.frame(vec![]);
            frames += 1;
            let now = (h.page.strip_fallback.len(), h.page.thumb_failed.len());
            let pending = h
                .page
                .thumb_pending
                .filter(|(_, w)| *w == STRIP_TILE_W as u32);
            if pending.is_none() && now == last {
                stable += 1;
                if stable >= 3 {
                    return frames;
                }
            } else {
                stable = 0;
            }
            last = now;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        frames
    }

    /// **P27 ①：取样时刻按视频流时长算，并夹到 `[0, 视频流时长 − ε]`**。
    ///
    /// 这是实机那条报错的**正面修复**：容器总时长比视频流长时，旧口径的最后一块
    /// 取在容器末尾 → 落在"只有音频"的尾巴里 → 必然解不出帧。
    #[test]
    fn p27_samples_follow_the_video_stream_and_never_pass_its_end() {
        // ---- 纯逻辑：容器 120 s、视频流 6 s ----
        let video_end = 6_000i64;
        let last = p27_sample_last_ms(video_end);
        assert_eq!(last, video_end - P27_SAMPLE_EPS_MS, "末端 = 视频流时长 − ε");
        let s = strip_samples(0.0, 120_000.0, video_end, 5);
        assert_eq!(s.len(), 5, "仍然 5 块");
        assert_eq!(
            s.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            vec![0, 1_475, 2_950, 4_425, 5_900],
            "取样铺在**视频流**那一段里（末端 5900 = 6000 − 100）"
        );
        for (t, _) in &s {
            assert!(*t <= last, "取样时刻绝不越出视频流：{t} ≤ {last}");
        }
        // 旧口径会取到 120000（容器末尾）——那正是解不出来的那一点
        assert!(
            !s.iter().any(|(t, _)| *t >= video_end),
            "任何一个取样时刻都不许落在视频流之外"
        );
        // 可见区间整段落在"只有音频"的尾巴里：**不退化成空条**，5 块一律取末端
        let tail = strip_samples(7_000.0, 12_000.0, video_end, 5);
        assert_eq!(tail.len(), 5, "尾巴里也要给出 5 块（视觉上永远有 5 张图）");
        assert!(
            tail.iter().all(|(t, _)| *t == last),
            "整段在视频流之外时一律取末端那一帧：{tail:?}"
        );
        // ---- 跨度来源：视频流时长；拿不到才回退容器时长 ----
        let split = synthetic_clip_split(120_000, 6_000);
        assert_eq!(
            p27_sample_span_end_ms(&split),
            (6_000, true),
            "有视频流时长时以它为准（不用容器时长）"
        );
        let mut no_stream_dur = synthetic_clip_split(120_000, 0);
        assert_eq!(
            p27_sample_span_end_ms(&no_stream_dur),
            (120_000, false),
            "拿不到视频流时长 → 回退容器时长（并记一行日志，见 log_p27_sample_span）"
        );
        no_stream_dur.video = None;
        assert_eq!(
            p27_sample_span_end_ms(&no_stream_dur),
            (120_000, false),
            "没有视频流同样回退容器时长"
        );
        // ---- 界面侧：整片铺满时，5 个取样时刻一个都不许越出视频流 ----
        let mut h = Harness::with_screen(synthetic_clip_split(120_000, 6_000), 1024.0, 768.0);
        h.frame(vec![]);
        let times = h
            .page
            .test_rects
            .strip_grid
            .clone()
            .expect("应登记取样时刻");
        assert_eq!(times.len(), 5);
        assert!(
            times.iter().all(|t| *t <= last),
            "界面侧取样同样夹到视频流 − ε：{times:?}"
        );
        assert_eq!(*times.last().unwrap(), last);
    }

    /// **P27 ②：方向规则**（用户口径：5 块时前 3 向右 / 后 2 向左，n<5 按同一精神推广）。
    #[test]
    fn p27_fallback_direction_rule_matches_the_user_table() {
        let dirs =
            |n: usize| -> Vec<FallbackDir> { (0..n).map(|k| strip_fallback_dir(k, n)).collect() };
        assert_eq!(
            dirs(5),
            vec![
                FallbackDir::Right,
                FallbackDir::Right,
                FallbackDir::Right,
                FallbackDir::Left,
                FallbackDir::Left
            ],
            "用户口径：前 3 块（k=0,1,2）向右、后 2 块（k=3,4）向左"
        );
        assert_eq!(
            dirs(4),
            vec![
                FallbackDir::Right,
                FallbackDir::Right,
                FallbackDir::Left,
                FallbackDir::Left
            ]
        );
        assert_eq!(
            dirs(3),
            vec![FallbackDir::Right, FallbackDir::Right, FallbackDir::Left]
        );
        assert_eq!(dirs(2), vec![FallbackDir::Right, FallbackDir::Left]);
        assert_eq!(dirs(1), vec![FallbackDir::Right]);
        // 5 块的"3 右 / 2 左"是用户写下的**唯一验收数字**，单独再钉一次
        let n = 5;
        let right = (0..n)
            .filter(|k| strip_fallback_dir(*k, n) == FallbackDir::Right)
            .count();
        assert_eq!((right, n - right), (3, 2), "5 块必须正好 3 右 / 2 左");
        // 退化输入不 panic
        assert_eq!(strip_fallback_dir(0, 0), FallbackDir::Right);
        assert_eq!(strip_fallback_dir(usize::MAX, 0), FallbackDir::Right);
    }

    /// **P27 ③：方向搜索有界、不跨相邻取样点、按距离由近到远**。
    #[test]
    fn p27_direction_search_is_bounded_and_never_crosses_neighbours() {
        let kf = vec![0i64, 1_000, 2_000, 3_000, 4_000, 5_000];
        let last = 5_900; // 视频流 6 s − ε
                          // ---- 后 2 块：向左；邻居是前一个取样点（硬边界）----
        let left = strip_fallback_probes(
            5_000,
            FallbackDir::Left,
            &kf,
            Some(4_000),
            last,
            P27_MAX_ATTEMPTS,
            P27_MAX_FALLBACK_MS,
            P27_STEP_MS,
        );
        assert!(!left.is_empty(), "左边有关键帧/步长候选");
        assert!(left.len() <= P27_MAX_ATTEMPTS, "候选数 ≤ 次数上限");
        for p in &left {
            assert!(
                *p < 5_000 && *p > 4_000,
                "必须严格落在 (邻居, 原时刻) 之间：{p}"
            );
            assert!((p - 5_000).abs() <= P27_MAX_FALLBACK_MS, "距离 ≤ 上限");
            assert!(*p >= 0 && *p <= last, "不许越出视频流：{p}");
        }
        for w in left.windows(2) {
            assert!(
                (w[0] - 5_000).abs() <= (w[1] - 5_000).abs(),
                "必须按距离由近到远：{left:?}"
            );
            assert_ne!(w[0], w[1], "不许有重复候选");
        }
        // ---- 前 3 块：向右；上界同时受"邻居"和"视频流末端"两家夹 ----
        let right = strip_fallback_probes(
            1_000,
            FallbackDir::Right,
            &kf,
            Some(2_000),
            last,
            P27_MAX_ATTEMPTS,
            P27_MAX_FALLBACK_MS,
            P27_STEP_MS,
        );
        assert!(!right.is_empty());
        for p in &right {
            assert!(
                *p > 1_000 && *p < 2_000,
                "严格落在 (原时刻, 邻居) 之间：{p}"
            );
        }
        // 邻居很远时，上界由"视频流末端"接管（不许扫到视频流之外）
        let far = strip_fallback_probes(
            1_000,
            FallbackDir::Right,
            &kf,
            Some(999_999),
            last,
            P27_MAX_ATTEMPTS,
            P27_MAX_FALLBACK_MS,
            P27_STEP_MS,
        );
        assert!(
            far.iter().all(|p| *p <= last),
            "右边即使邻居很远也不许越出视频流：{far:?}"
        );
        assert!(far.len() <= P27_MAX_ATTEMPTS);
        // ---- 双上界：距离上限比"次数上限"更早生效时不许多试 ----
        let near = strip_fallback_probes(5_000, FallbackDir::Left, &kf, None, last, 64, 300, 200);
        assert!(
            near.iter().all(|p| (p - 5_000).abs() <= 300),
            "距离上限是硬边界：{near:?}"
        );
        // ---- 退化输入 ----
        assert!(
            strip_fallback_probes(
                5_000,
                FallbackDir::Left,
                &[],
                Some(5_000),
                last,
                8,
                5_000,
                200
            )
            .is_empty(),
            "窗口为空（邻居就是自己）→ 不试"
        );
        assert!(
            strip_fallback_probes(5_000, FallbackDir::Left, &kf, None, last, 0, 5_000, 200)
                .is_empty(),
            "次数上限为 0 → 不试"
        );
        assert!(
            strip_fallback_probes(0, FallbackDir::Left, &kf, None, last, 8, 5_000, 200).is_empty(),
            "已经在片头 → 左边没有东西可试"
        );
        // 没有关键帧时靠小步长兜住，仍然有界
        let no_kf = strip_fallback_probes(
            5_000,
            FallbackDir::Left,
            &[],
            None,
            last,
            P27_MAX_ATTEMPTS,
            P27_MAX_FALLBACK_MS,
            P27_STEP_MS,
        );
        assert_eq!(no_kf.len(), P27_MAX_ATTEMPTS, "全部由步长候选填满");
        assert_eq!(no_kf[0], 4_800, "最近的步长候选先试");
    }

    /// **P27 ③（尝试循环）**：给一个"只有某处可解"的模拟解码器 → 断言**停在该处**、
    /// 尝试次数 ≤ 上限、失败之后不再乱试（这就是"回退有界"的可判定形式）。
    #[test]
    fn p27_direction_search_stops_at_the_first_decodable_probe() {
        let probes = vec![4_800i64, 4_600, 4_400, 4_200];
        // 只有 4_400 可解：必须停在第 3 次尝试（原时刻 + 2 个候选）
        let mut calls: Vec<i64> = Vec::new();
        let r = p27_search_decode(5_000, &probes, |t| {
            calls.push(t);
            if t == 4_400 {
                Ok("hit")
            } else {
                Err(format!("no frame near {t}"))
            }
        });
        let (v, decoded, attempts) = r.expect("应当命中");
        assert_eq!(v, "hit");
        assert_eq!(decoded, 4_400, "必须停**在该处**");
        assert_eq!(attempts, 4, "原时刻 + 3 个候选 = 4 次，命中即停");
        assert_eq!(
            calls,
            vec![5_000, 4_800, 4_600, 4_400],
            "顺序 = 原时刻 → 候选（最近的先）"
        );
        // 原时刻就能解 → 一次都不搜（不浪费）
        let mut calls: Vec<i64> = Vec::new();
        let r = p27_search_decode(5_000, &probes, |t| {
            calls.push(t);
            Ok::<_, String>(t)
        });
        assert_eq!(r.expect("应当命中").2, 1);
        assert_eq!(calls, vec![5_000]);
        // 全都不成 → 尝试次数 = 1 + 候选数（有界），错误取**第一次**那条
        let r = p27_search_decode(5_000, &probes, |t| {
            Err::<(), _>(format!("no frame near {t}"))
        });
        let (err, attempts) = r.expect_err("全失败");
        assert_eq!(
            attempts,
            1 + probes.len(),
            "总尝试次数 = 1 + 候选数（有界）"
        );
        assert!(
            err.contains("5000"),
            "带回来的应当是**第一次**的错误（最有信息量）：{err}"
        );
        // 空候选（预览那一侧就是空表）→ 只试一次
        let r = p27_search_decode(5_000, &[], |_| Err::<(), _>("boom".into()));
        assert_eq!(r.expect_err("必然失败").1, 1);
    }

    /// **P27 ④（控制层）**：`thumb_keyframe` 只选**视频流之内、且不晚于 末端 − ε** 的关键帧。
    ///
    /// 实机那条报错的关键帧正好压在视频流末尾上（`687695 ms`）：
    /// 它 `< 容器时长` 所以旧判据放行，却已经越出视频流 → 必然解不出来。
    #[test]
    fn p27_preview_keyframe_never_lands_past_the_video_stream() {
        // 容器 120 s、视频流 6 s；关键帧里有一个"正好压在视频流末尾上"的 6000
        let mut clip = synthetic_clip_split(120_000, 6_000);
        clip.keyframes_ms = vec![0, 1_000, 2_000, 3_000, 4_000, 5_000, 6_000];
        let last = p27_sample_last_ms(6_000);
        assert_eq!(
            thumb_keyframe(&clip, 119_999),
            Some(5_000),
            "末尾那一段 → 退回最后一个可解关键帧"
        );
        assert_eq!(
            thumb_keyframe(&clip, 6_000),
            Some(5_000),
            "正好压在末尾上的关键帧不许选"
        );
        assert_eq!(thumb_keyframe(&clip, 5_900), Some(5_000));
        assert_eq!(
            thumb_keyframe(&clip, 4_100),
            Some(4_000),
            "视频流之内的照旧选最近的"
        );
        assert!(
            clip.keyframes_ms.iter().all(|k| *k <= last || *k == 6_000),
            "前提：6000 是唯一越界的那个关键帧"
        );
        // 没有视频流时长（回退容器）时，行为退回"容器之内"的老口径
        let mut no_video_dur = clip.clone();
        if let Some(v) = no_video_dur.video.as_mut() {
            v.duration_ms = 0;
        }
        assert_eq!(
            thumb_keyframe(&no_video_dur, 119_999),
            Some(6_000),
            "拿不到视频流时长 → 回退容器时长（6000 < 120000 仍算可选）"
        );
    }

    /// **P27 兜底链第①②级（界面侧）**：方向搜索失败后**借用同一条上邻近块的画面** ——
    /// **5 个槽位一个都不许空着**。
    #[test]
    fn p27_fallback_chain_level2_reuses_a_neighbour_frame() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.page.strip_requests_enabled = true;
        h.frame(vec![]);
        let times = h.page.test_rects.strip_grid.clone().expect("取样时刻");
        assert_eq!(times.len(), 5);
        let span_end = 120_000i64;
        let tile_h = strip_tile_height();
        let key_of = |t: i64| strip_thumb_key(t, span_end, STRIP_TILE_W as u32, tile_h);
        // 抽掉**第 5 块**（k=4，方向 = 向左）的画面：其余 4 块都还有画面 → 第②级有得借
        let k4 = key_of(times[4]);
        let neighbour_tex = h
            .page
            .strip_thumbs
            .get(&key_of(times[3]))
            .expect("前提：第 4 块有画面")
            .tex
            .id();
        h.page.strip_thumbs.remove(&k4);
        h.page.thumb_pending = None;
        let frames = p27_run_strip_idle(&mut h, std::time::Duration::from_secs(20));
        let diag = format!(
            "frames={frames} samples={times:?} prev={:?} fallback={:?} failed={:?} \
             pending={:?} worker={} enabled={}",
            h.page.strip_prev.iter().map(|t| t.idx).collect::<Vec<_>>(),
            h.page
                .strip_fallback
                .keys()
                .map(|k| k.time)
                .collect::<Vec<_>>(),
            h.page.thumb_failed.iter().collect::<Vec<_>>(),
            h.page.thumb_pending,
            h.page.thumb_job_tx.is_some(),
            h.page.strip_requests_enabled,
        );
        assert!(
            h.page.strip_fallback.contains_key(&k4),
            "第②级：最后一块失败后必须借到就近块的画面（{diag}）"
        );
        assert_eq!(
            h.page.strip_fallback[&k4].tex.id(),
            neighbour_tex,
            "借的必须是**就近那个**已成功块（k=3）的画面"
        );
        assert!(
            h.page.thumb_failed.contains(&(k4.time, k4.w, k4.h)),
            "失败的键必须进 thumb_failed（不再重试）"
        );
        // **不产生空槽**：兜底帧到位后 5 块都有画面
        h.frame(vec![]);
        assert_eq!(strip_rects_of(&h).len(), 5, "兜底之后 5 个槽位都必须有画面");
        assert!(
            h.page.notice.is_none(),
            "兜底不许打扰用户（只记 cap.log）：{:?}",
            h.page.notice
        );
    }

    /// **P27 兜底链第③级（界面侧）**：邻块也没有 → 用**缓存里最近一张**成功画面；
    /// 第④级：全都没有 → 留空 + 一行警告（仍然不打扰用户、不重复重试、不 panic）。
    #[test]
    fn p27_fallback_chain_level3_then_gives_up_without_disturbing() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.page.strip_requests_enabled = true;
        h.frame(vec![]);
        let times = h.page.test_rects.strip_grid.clone().expect("取样时刻");
        let span_end = 120_000i64;
        let tile_h = strip_tile_height();
        let key_of = |t: i64| strip_thumb_key(t, span_end, STRIP_TILE_W as u32, tile_h);
        // ---- 第③级：胶片条缓存里**全部取样点都没有**，只剩一张"别处的"画面 ----
        // 挑选一个**不属于任何取样时刻**的诱饵（样本是 0/30/60/90/119.9 s）：
        // 这样本帧一块都画不出来 → 第②级（邻块）也没有对象 → 只能走第③级。
        let decoy_key = strip_thumb_key(45_000, span_end, STRIP_TILE_W as u32, tile_h);
        let decoy_tex = {
            let img = egui::ColorImage::from_rgba_unmultiplied([2, 2], &[0xABu8; 16]);
            h.ctx
                .load_texture("p27-decoy", img, egui::TextureOptions::NEAREST)
        };
        h.page.strip_thumbs.clear();
        h.page.strip_thumb_order.clear();
        h.page.strip_prev.clear();
        h.page.remember_strip_thumb(
            decoy_key,
            CachedThumb {
                w: 2,
                h: 2,
                tex: decoy_tex.clone(),
            },
        );
        h.page.thumb_pending = None;
        let first_key = key_of(times[0]);
        p27_run_strip_idle(&mut h, std::time::Duration::from_secs(20));
        assert!(
            h.page.strip_fallback.contains_key(&first_key),
            "第③级：邻块没有时，必须用缓存里最近一张成功画面兜底"
        );
        assert_eq!(
            h.page.strip_fallback[&first_key].tex.id(),
            decoy_tex.id(),
            "兜底帧就是缓存里那一张"
        );
        assert_eq!(
            h.page.strip_fallback[&first_key].w, 2,
            "兜底帧带上它自己的尺寸"
        );
        assert!(
            h.page.notice.is_none(),
            "兜底不许打扰用户（只记 cap.log）：{:?}",
            h.page.notice
        );
        // **不产生空槽**：第③级把第一块补齐之后，其余块走第②级（邻块）也补齐 ——
        // 兜底链跑完时 5 个槽位都应当有画面。
        h.frame(vec![]);
        assert_eq!(
            strip_rects_of(&h).len(),
            5,
            "兜底链①/②/③跑完之后 5 个槽位都不许空着（fallback={} failed={}）",
            h.page.strip_fallback.len(),
            h.page.thumb_failed.len()
        );
        // ---- 第④级：缓存也清空 → 留空 + 只记日志，不重复重试 ----
        h.page.strip_thumbs.clear();
        h.page.strip_thumb_order.clear();
        h.page.strip_fallback.clear();
        h.page.thumb_failed.clear();
        h.page.strip_prev.clear();
        h.page.thumb_pending = None;
        p27_run_strip_idle(&mut h, std::time::Duration::from_secs(20));
        assert!(
            h.page.strip_fallback.is_empty(),
            "第④级：确实没有任何可借的画面时不许伪造一张"
        );
        assert!(
            !h.page.thumb_failed.is_empty(),
            "失败的键必须进 thumb_failed（防重复重试）"
        );
        let failed = h.page.thumb_failed.len();
        assert!(
            h.page.notice.is_none(),
            "到界仍失败 → 只警告不打扰（界面提示必须为空）：{:?}",
            h.page.notice
        );
        // 再跑若干帧：失败键不再重排（计数不涨、也不留"在途"）
        for _ in 0..5 {
            h.frame(vec![]);
        }
        assert_eq!(
            h.page.thumb_failed.len(),
            failed,
            "到界仍失败之后不许反复重试"
        );
        assert!(
            h.page
                .thumb_pending
                .filter(|(_, w)| *w == STRIP_TILE_W as u32)
                .is_none(),
            "到界仍失败之后不许留一个「永远在途」的任务"
        );
    }

    /// **P27 ⑤：布局不变** —— 兜底只改"取哪一帧"，P21/P25 的 5 块贴边等距一个数都不动。
    #[test]
    fn p27_fallback_keeps_the_layout_identical() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.frame(vec![]);
        let times = h.page.test_rects.strip_grid.clone().expect("取样时刻");
        let all_real = strip_rects_of(&h);
        assert_eq!(all_real.len(), 5);
        // 把最后一块"换成兜底帧"（借用第 4 块的画面）
        let span_end = 120_000i64;
        let tile_h = strip_tile_height();
        let k4 = strip_thumb_key(times[4], span_end, STRIP_TILE_W as u32, tile_h);
        let borrowed = h.page.strip_thumbs
            [&strip_thumb_key(times[3], span_end, STRIP_TILE_W as u32, tile_h)]
            .tex
            .clone();
        h.page.insert_strip_fallback(
            k4,
            CachedThumb {
                w: 2,
                h: 2,
                tex: borrowed,
            },
        );
        h.frame(vec![]);
        let with_fallback = strip_rects_of(&h);
        assert_eq!(with_fallback.len(), 5, "兜底帧也要占住它自己的槽位");
        for (a, b) in all_real.iter().zip(with_fallback.iter()) {
            assert!(
                (a.left() - b.left()).abs() <= 0.01
                    && (a.right() - b.right()).abs() <= 0.01
                    && (a.top() - b.top()).abs() <= 0.01
                    && (a.bottom() - b.bottom()).abs() <= 0.01,
                "布局必须一模一样（回退只改画面）：{a:?} vs {b:?}"
            );
        }
        // P21/P25 的口径逐条复述：首贴左、尾贴右、相邻间隙相等、高 = 行高
        let rect = h.timeline();
        assert!(
            (with_fallback[0].left() - rect.left()).abs() <= 1.0,
            "首贴左"
        );
        assert!(
            (with_fallback[4].right() - rect.right()).abs() <= 1.0,
            "尾贴右"
        );
        let gap = with_fallback[1].left() - with_fallback[0].right();
        for w in with_fallback.windows(2) {
            assert!(
                (w[1].left() - w[0].right() - gap).abs() <= 1.0,
                "相邻间隙相等"
            );
        }
        for r in &with_fallback {
            assert!((r.height() - strip_tile_height()).abs() <= 1.0, "高 = 行高");
        }
    }

    /// **P27 ⑥：兜底链不自激重绘**（无心跳、无每帧重排）。
    #[test]
    fn p27_fallback_does_not_self_excite_repaints() {
        let mut h = Harness::with_screen(
            synthetic_clip(120_000, vec![0, 5_000, 10_000, 15_000], 30.0),
            1024.0,
            768.0,
        );
        h.page.strip_requests_enabled = true;
        h.frame(vec![]);
        let times = h.page.test_rects.strip_grid.clone().expect("取样时刻");
        let span_end = 120_000i64;
        let tile_h = strip_tile_height();
        // 抽掉两块（一块靠邻块兜底、一块靠缓存兜底），把兜底链真的跑起来
        h.page.strip_thumbs.remove(&strip_thumb_key(
            times[4],
            span_end,
            STRIP_TILE_W as u32,
            tile_h,
        ));
        h.page.thumb_pending = None;
        p27_run_strip_idle(&mut h, std::time::Duration::from_secs(20));
        // 兜底结束后：界面必须彻底静止（不再请求重绘、也不再排队）
        let mut settled = false;
        for _ in 0..6 {
            h.frame(vec![]);
            if !h.ctx.has_requested_repaint() {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(settled, "兜底链让界面永远在请求重绘（有周期性心跳）");
        for i in 0..5 {
            h.frame(vec![]);
            assert!(
                !h.ctx.has_requested_repaint(),
                "静置第 {i} 帧：兜底之后界面不该请求重绘"
            );
        }
        assert!(
            h.page
                .thumb_pending
                .filter(|(_, w)| *w == STRIP_TILE_W as u32)
                .is_none(),
            "静置时不该还有在途的胶片条解码"
        );
    }

    /// **P27（日志内容）**：那一行必须写明"取样失败 + 用了哪一级兜底帧（或留空）"。
    ///
    /// 落点那一侧（`cap.log`）由 [`p27_log`] → `ui::startup::mark_with` 保证
    /// —— 产品**没有 tracing 订阅者**，只写 `tracing::warn!` 在用户机器上会丢。
    #[test]
    fn p27_log_lines_name_the_fallback_source() {
        // 取样跨度那一行：正常路径 / 回退路径都要能一眼看出"按谁算"
        let ok = p27_sample_span_line("a.mp4", 6_000, true, 12_000);
        assert!(
            ok.contains("span_end_ms=6000") && ok.contains("source=video_stream"),
            "{ok}"
        );
        assert!(ok.contains(&format!("eps_ms={P27_SAMPLE_EPS_MS}")), "{ok}");
        let fb = p27_sample_span_line("a.mp4", 12_000, false, 12_000);
        assert!(fb.contains("source=container_fallback"), "{fb}");
        // 兜底那一行：三级来源各写清楚
        for (src, tag) in [
            (FallbackSource::Direction, "direction"),
            (FallbackSource::Neighbor, "neighbor"),
            (FallbackSource::Cache, "cache"),
        ] {
            let line = p27_fallback_line(5_000, 4_400, src, "no frame");
            assert!(line.contains("reason=sample_failed"), "{line}");
            assert!(
                line.contains(&format!("used_fallback_frame={tag}")),
                "{line}"
            );
            assert!(!line.contains("slot=empty"), "有兜底帧时不许写留空：{line}");
        }
        // 彻底失败那一行：如实写"这一格没有画面"
        let none = p27_fallback_line(5_000, 5_000, FallbackSource::None, "no frame");
        assert!(
            none.contains("used_fallback_frame=none") && none.contains("slot=empty"),
            "{none}"
        );
    }

    /// **P27（真素材真解码）**：造一个"视频流 6 s + 音轨 12 s"的素材，
    /// 断言"**旧口径的末尾取样必然失败、新口径必然成功**"，并让方向搜索真的解出画面。
    ///
    /// 这是实机报错（`在 687695 ms 附近没有可解码的视频帧`）的最小复现与修复验证：
    /// 用户的文件正是"容器 687696 ms / 视频流 687640 ms"，比值与这里同一量级。
    #[test]
    fn p27_real_split_container_reproduces_the_failure_and_the_new_span_fixes_it() {
        let Some(path) = gen_mp4_split("span") else {
            return; // 环境不具备 → 跳过（不是失败）
        };
        let clip = edit::open(&path).expect("打开 P27 复现素材");
        let video_end = clip.video_duration_ms().expect("视频流应当自报时长");
        assert!(
            clip.duration_ms > video_end + 3_000,
            "前提：容器明显长于视频流（容器 {} ms / 视频流 {} ms）",
            clip.duration_ms,
            video_end
        );
        let last = p27_sample_last_ms(video_end);
        // ---- 旧口径：按容器时长取样 → 末尾那一块必然解不出画面 ----
        let old_probe = clip.duration_ms - 50;
        let old = edit::thumbnail(&path, old_probe, 160, 160);
        println!(
            "[P27] 旧口径取样 {old_probe} ms -> {}",
            match &old {
                Ok((w, h, _)) => format!("Ok({w}x{h})"),
                Err(e) => format!("Err({e})"),
            }
        );
        assert!(
            old.is_err(),
            "旧口径（容器时长）的末尾取样必须失败 —— 这就是实机那条报错"
        );
        // ---- 新口径：视频流时长 − ε → 能解出画面 ----
        // （视频流末端"那一刻"本身是否可解与素材有关：本素材实测是不可解的，
        //   所以一律**只用末端 − ε** 作为取样上界，见 `P27_SAMPLE_EPS_MS`。）
        let at_video_end = edit::thumbnail(&path, video_end, 160, 160);
        println!(
            "[P27] 视频流末端 {video_end} ms -> {}",
            match &at_video_end {
                Ok((w, h, _)) => format!("Ok({w}x{h})"),
                Err(e) => format!("Err({e})"),
            }
        );
        assert!(
            edit::thumbnail(&path, last, 160, 160).is_ok(),
            "视频流时长 − ε = {last} ms 必须能解出画面"
        );
        // ---- 5 个取样时刻（按视频流取样）逐点真解码 ----
        for (t, _) in strip_samples(0.0, clip.duration_ms as f64, video_end, 5) {
            assert!(t <= last, "取样时刻不越出视频流：{t} ≤ {last}");
            assert!(
                edit::thumbnail(&path, t, 160, 160).is_ok(),
                "按视频流取样时 {t} ms 必须可解"
            );
        }
        // ---- P27 ①：从"只有音频的尾巴"里往回搜，有界且真的解出画面 ----
        let tail_probe = video_end + 1_000;
        let probes = strip_fallback_probes(
            tail_probe,
            FallbackDir::Left,
            &clip.keyframes_ms,
            None,
            last,
            P27_MAX_ATTEMPTS,
            P27_MAX_FALLBACK_MS,
            P27_STEP_MS,
        );
        assert!(!probes.is_empty(), "尾巴里必须给出有界候选");
        println!("[P27] 方向搜索候选（{tail_probe} ms 向左）: {probes:?}");
        let hit = p27_search_decode(tail_probe, &probes, |t| {
            edit::thumbnail(&path, t, 160, 160).map_err(|e| e.to_string())
        });
        let (_, decoded, attempts) = hit.expect("方向搜索应当命中一个可解时刻");
        println!("[P27] 方向搜索命中 {decoded} ms（尝试 {attempts} 次）");
        assert!(decoded <= last, "命中点不许越出视频流：{decoded}");
        assert!(
            attempts <= 1 + P27_MAX_ATTEMPTS,
            "尝试次数必须有界（实测 {attempts}）"
        );
    }
    #[test]
    fn audio_only_clip_keeps_audio_playback_without_a_video_player() {
        let mut page = EditorPage::new(&Config::default());
        page.clip = Some(edit::Clip {
            src: std::env::temp_dir().join("cap-no-such-audio-only.mp4"),
            duration_ms: 1_000,
            video: None,
            audio: Some(edit::StreamInfo {
                index: 0,
                codec: "aac".into(),
                width: 0,
                height: 0,
                fps: 0.0,
                time_base: (1, 48_000),
                sample_rate: 48_000,
                channels: 2,
                duration_ms: 1_000,
            }),
            keyframes_ms: Vec::new(),
            keyframes_scanned: true,
            health: edit::health::HealthReport::default(),
        });
        page.play_audio_output = false;
        page.start_play();
        let playing = page.play.as_ref().expect("纯音频也应进入播放态");
        assert!(playing.player.is_none(), "无视频流时不应创建视频播放器");
        assert!(playing.audio.is_some(), "音频链必须启动");
        page.stop_play();
    }

    #[test]
    fn opening_a_new_file_invalidates_the_previous_export() {
        let mut page = EditorPage::new(&Config::default());
        let (_tx, rx) = std::sync::mpsc::channel();
        page.export_rx = Some(rx);
        page.exporting = true;
        let old_epoch = page.export_epoch;
        page.open(Path::new("cap-no-such-next-file.mp4"));
        assert_ne!(page.export_epoch, old_epoch, "打开新素材必须递增导出世代");
        assert!(page.export_rx.is_none(), "旧导出通道必须立即断开");
        assert!(!page.exporting, "新素材不能被旧导出锁住导出按钮");
    }
}
