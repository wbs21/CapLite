//! **清爽主题（浅色 / 深色两套）**：一套统一的颜色 / 间距 / 圆角 / 字号，三页共用。
//!
//! 设计口径（照着"现代清爽"来，不做花哨装饰）：
//! * **浅色 + 留白**：窗口底 `#F7F8FA`、卡片纯白、分隔只用 **1px 发丝线** `#E5E7EB`；
//!   不堆阴影、不做渐变、不用 2px 重描边。
//! * **单一强调色** `#2563EB`：只出现在主操作按钮、选中项、滑条已填充段、焦点环。
//!   其余一律中性色；成功/警告/错误只在横幅与状态点上用（低饱和底色 + 同色描边）。
//! * **统一栅格**：4/8 的倍数；卡片内边距 16、卡片间距 14、行间距 8~10；
//!   控件高度 30（普通）/ 44（主按钮）；正文 13.5、分区标题 14 半粗、页面标题 20 半粗。
//! * **圆角 8px**，悬停只把底色加深一点点（不发光、不缩放）。
//!
//! **P40 起有深色一套**：用户要求"在界面语言选项后面添加深色模式开关"。
//! 深色不是另做一套设计，而是**同一套口径的暗房版**，三条不变：
//! ① **只有颜色变**：间距 / 圆角 / 字号 / 控件尺寸全部与浅色相同（所以两套主题下
//!    三页的布局一模一样，`layout.rs` 里量出来的必需高度在两种主题下都成立）；
//! ② **色相不跑偏**：强调色仍是蓝、成功仍绿、警告仍琥珀、错误仍红，只是亮度/饱和度
//!    按"深色底上要读得清"重挑（`Palette::dark()` 一行一个出处）；
//! ③ **不用纯黑**：页面底 `#16181D`、卡片 `#212630`，两档之间差一档亮度，
//!    靠亮度分层而不是靠阴影（阴影在深色下几乎看不见，本来就不该靠它）。
//!
//! 默认主题 = **深色**（P42：用户原话"设置默认主题为黑色"；早先"只做浅色即可"的
//! 口径已被这句话取代）：`Config::dark_mode` 缺省为 `true` —— 新装、升级、
//! 老配置缺键，三种情况都是深色；显式写了 `false` 的老配置保持浅色
//! （用户的选择永远赢过默认值）。切换入口是设置页"界面语言"右侧的开关。

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Margin, RichText, Shadow, Stroke, TextStyle, Vec2,
};

// ---------------------------------------------------------------------------
// 应用显示名（**单一来源**）
// ---------------------------------------------------------------------------

/// 应用显示名：窗口标题、界面左上角标题都用它。
///
/// 为什么集中成一个常量：产品改名时只动这一处即可，不会再出现"窗口标题写一个、
/// 界面标题写另一个"的不一致。
/// 注意：包名 / 二进制名 / 目录名现在**与显示名保持一致**（`caplite` / `CapLite.exe`
/// / `CapLite\`），见 `Cargo.toml` 的 `[package] name`、`[lib] name` 与 `[[bin]] name`。
pub const APP_NAME: &str = "CapLite";

/// 应用版本号（**唯一来源 = `Cargo.toml` 的 `[package] version`**，P44）。
///
/// 为什么用 `env!("CARGO_PKG_VERSION")` 而不是再写一个字符串常量：版本号已经在
/// `Cargo.toml` 里存在一份了，第二份必然会在某次发版时漏改（历史教训：文档里的
/// "当时量到多少"说明文字就是这样过期的）。这样改版本只动 `Cargo.toml` 一处，
/// 窗口标题、日志、将来的"关于"面板自动跟上。
#[cfg(test)]
// P54：只有单测在用 → 发布构建不编译（原先是 `pub`，把 dead_code 屏蔽掉了）。
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// **窗口标题**（标题栏左上角那一串）：`CapLite v1.0.0`（P44：用户要求
/// "在左上角 CapLite 处添加版本号，这版就定为 v1.0.0"）。
///
/// 拼法固定为 `"{名} v{版本}"`：`v` 是小写、紧跟一个空格、中间没有别的分隔符 ——
/// 因为托盘要靠 [`APP_NAME`] 前缀认出主窗口（见 `tray::find_main_proc`），
/// 版本号后面也不加任何后缀（加了就会在 Alt+Tab/任务栏里显得啰嗦）。
pub const WINDOW_TITLE: &str = concat!("CapLite v", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// 颜色
// ---------------------------------------------------------------------------

/// **当前主题种类**（P40：用户要求"在界面语言选项后面添加深色模式开关"）。
///
/// **产品默认是深色**（P42）：`Config::dark_mode` 缺省 `true`，`main` 在画第一帧之前
/// 就按配置 `apply_kind`，所以用户看到的默认主题永远是配置给的深色。
/// enum 自身的 `#[default] = Light` 只服务两处"还没应用过主题"的场景：
/// 线程局部 `KIND` 的初值（无窗口单测跑在各自线程上、没调 `apply_kind` 时）
/// 与 `install_cjk_font` 里那次兜底 `apply()` —— 都是"配置读出来之前"的瞬间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeKind {
    /// 浅色清爽（仅线程局部/兜底的默认值，产品默认见 [`Config::dark_mode`](crate::config::Config::dark_mode)）
    #[default]
    Light,
    /// 深色暗房版：同一套设计口径，只换颜色（见 `Palette::dark`）
    Dark,
}

impl ThemeKind {
    /// `Config::dark_mode` → 主题种类（**唯一的翻译处**：配置里存 bool，界面上跑的是 enum）。
    pub fn from_dark_mode(dark: bool) -> Self {
        if dark {
            Self::Dark
        } else {
            Self::Light
        }
    }

    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// egui 自己的主题偏好：**钉死**两套中的一套，不跟随系统 ——
    /// 两套皮肤都是我们自己调过的，跟随系统会在用户深色桌面上给出没调过的那半套。
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }
}

thread_local! {
    /// 当前线程的主题种类，默认浅色。
    ///
    /// 为什么用 thread_local 而不是塞进 `egui::Context` 的 data：本模块的取色函数
    /// （`mono()` / `hairline()` / `text()` …）都**没有** `&Context` 参数，
    /// 给上百个调用点加参数不现实；而 egui 的界面只在主线程绘制，
    /// [`apply_kind`] 与每次取色发生在同一个线程上，所以它等价于 Context data。
    /// （无窗口单测跑在各自线程上，默认值 = 浅色 = 改动前那 34 个常量的取值。）
    static KIND: std::cell::Cell<ThemeKind> = const { std::cell::Cell::new(ThemeKind::Light) };
}

/// 当前主题种类（取色函数的唯一依据）。
pub fn kind() -> ThemeKind {
    KIND.get()
}

/// 只问"是不是深色"：时间线那一堆"深色下要整体提亮一档"的颜色用它。
pub fn is_dark() -> bool {
    KIND.get().is_dark()
}

/// 当前主题的那一整套颜色。`Palette` 很小（Copy），直接返回值，不做缓存。
fn palette() -> Palette {
    match KIND.get() {
        ThemeKind::Light => Palette::light(),
        ThemeKind::Dark => Palette::dark(),
    }
}

/// **一整套界面颜色**（浅色 / 深色各一份）。
///
/// 为什么提成结构体而不是 34 个常量：深色主题要求"同一组语义在两种主题下取值不同"，
/// 常量做不到"同一个名字两种值"；提成结构体后，`theme::text()` 这类取色函数读当前主题
/// 那一份即可，**调用点几乎不用改**（这正是 P40 的深色模式能落地的原因）。
///
/// 字段分组即语义分组：中性 → 成功/警告/错误/信息/危险 → 时间线 → 控件级 token。
/// 深色那份的**色相不跑偏**：强调仍蓝、成功仍绿、警告仍琥珀、错误仍红，
/// 只按"深色底上要读得清"重挑亮度与饱和度；**不用纯黑**（页面底 `#16181D`、卡片
/// `#212630`，两档差一档亮度，靠亮度分层而不是阴影 —— 阴影在深色下几乎看不见）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    // ---- 中性 ----
    /// 窗口/页面底色
    pub bg: Color32,
    /// 卡片底
    pub card: Color32,
    /// 发丝线：分隔线 / 卡片描边 / 输入框描边
    pub line: Color32,
    /// 正文
    pub text: Color32,
    /// 次要文字
    pub muted: Color32,
    /// 更淡的提示文字
    pub faint: Color32,
    /// 唯一强调色（主按钮 / 选中 / 滑条已填充 / 焦点环）
    pub accent: Color32,
    /// 强调色悬停
    pub accent_hover: Color32,
    /// 强调色浅底（选中行背景）
    pub accent_soft: Color32,

    // ---- 成功 ----
    pub ok_fg: Color32,
    /// **成功色实心**（与 [`Palette::ok_fg`] 同色系，饱和度略降一档）
    pub ok_solid: Color32,
    /// 成功色实心·悬停
    pub ok_solid_hover: Color32,

    // ---- 警告 ----
    pub warn_bg: Color32,
    pub warn_line: Color32,
    pub warn_fg: Color32,

    // ---- 错误 ----
    pub err_bg: Color32,
    pub err_line: Color32,
    pub err_fg: Color32,

    // ---- 中性信息框 ----
    pub info_bg: Color32,
    pub info_line: Color32,
    pub info_fg: Color32,

    // ---- 危险操作（停止录制）----
    pub danger: Color32,
    pub danger_hover: Color32,

    // ---- 时间线 ----
    pub track_bg: Color32,
    pub ruler_tick: Color32,
    pub ruler_text: Color32,
    /// 当前标尺（浅色深蓝 / 深色亮蓝：都要能和相邻的绿起点、红终点分开）
    pub cursor: Color32,
    /// 关键帧刻度（金色）
    pub keyframe: Color32,
    /// 片尾之后"无内容"带的斜纹颜色
    pub empty_hatch: Color32,
    /// 模态弹窗背后的半透明遮罩
    pub scrim: Color32,

    // ---- 控件级 token（只被 [`apply`] / [`neutral_button`] 读，不开取色函数）----
    /// 普通控件静息底（滑条滑轨、复选框底）
    pub ctrl_bg: Color32,
    /// 普通控件静息描边
    pub ctrl_edge: Color32,
    pub ctrl_bg_hover: Color32,
    pub ctrl_edge_hover: Color32,
    pub ctrl_bg_active: Color32,
    /// 按钮的**可选底色**（egui 的 `weak_bg_fill`：按钮才是它的用户）
    pub ctrl_weak: Color32,
    pub ctrl_weak_hover: Color32,
    pub ctrl_weak_active: Color32,
    /// 输入框底（表格斑马纹一档）
    pub edit_bg: Color32,
    /// 代码/等宽底
    pub code_bg: Color32,
    /// 禁用态填充 + 其上的文字色
    pub disabled: Color32,
    pub disabled_fg: Color32,
}

impl Palette {
    /// **浅色**：程序一直以来那一套（P37 起的紧凑卡片、发丝线都在这一套下调好的）。
    pub fn light() -> Self {
        Self {
            bg: Color32::from_rgb(0xF7, 0xF8, 0xFA),
            card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            line: Color32::from_rgb(0xE5, 0xE7, 0xEB),
            text: Color32::from_rgb(0x11, 0x18, 0x27),
            muted: Color32::from_rgb(0x6B, 0x72, 0x80),
            faint: Color32::from_rgb(0x9C, 0xA3, 0xAF),
            accent: Color32::from_rgb(0x25, 0x63, 0xEB),
            accent_hover: Color32::from_rgb(0x1D, 0x4E, 0xD8),
            accent_soft: Color32::from_rgb(0xEF, 0xF6, 0xFF),

            ok_fg: Color32::from_rgb(0x05, 0x96, 0x69),
            ok_solid: Color32::from_rgb(0x0B, 0x8C, 0x62),
            ok_solid_hover: Color32::from_rgb(0x07, 0x74, 0x50),

            warn_bg: Color32::from_rgb(0xFF, 0xFB, 0xEB),
            warn_line: Color32::from_rgb(0xFD, 0xE6, 0x8A),
            warn_fg: Color32::from_rgb(0xB4, 0x53, 0x09),

            err_bg: Color32::from_rgb(0xFE, 0xF2, 0xF2),
            err_line: Color32::from_rgb(0xFE, 0xCA, 0xCA),
            err_fg: Color32::from_rgb(0xDC, 0x26, 0x26),

            info_bg: Color32::from_rgb(0xF9, 0xFA, 0xFB),
            info_line: Color32::from_rgb(0xE5, 0xE7, 0xEB),
            info_fg: Color32::from_rgb(0x4B, 0x55, 0x63),

            danger: Color32::from_rgb(0xDC, 0x26, 0x26),
            danger_hover: Color32::from_rgb(0xB9, 0x1C, 0x1C),

            track_bg: Color32::from_rgb(0xEE, 0xF0, 0xF3),
            ruler_tick: Color32::from_rgb(0xB6, 0xBD, 0xC7),
            ruler_text: Color32::from_rgb(0x8A, 0x91, 0x9C),
            cursor: Color32::from_rgb(0x1D, 0x4E, 0xD8),
            keyframe: Color32::from_rgb(0xD9, 0xB1, 0x4A),
            empty_hatch: Color32::from_rgb(0xE9, 0xEC, 0xF1),
            scrim: Color32::from_black_alpha(38),

            ctrl_bg: Color32::from_rgb(0xF1, 0xF3, 0xF6),
            ctrl_edge: Color32::from_rgb(0xD8, 0xDC, 0xE1),
            ctrl_bg_hover: Color32::from_rgb(0xDD, 0xE1, 0xE6),
            ctrl_edge_hover: Color32::from_rgb(0xD1, 0xD5, 0xDB),
            ctrl_bg_active: Color32::from_rgb(0xCF, 0xD4, 0xDB),
            ctrl_weak: Color32::WHITE,
            ctrl_weak_hover: Color32::from_rgb(0xF2, 0xF4, 0xF7),
            ctrl_weak_active: Color32::from_rgb(0xE8, 0xEB, 0xEF),
            edit_bg: Color32::from_rgb(0xFA, 0xFB, 0xFC),
            code_bg: Color32::from_rgb(0xF3, 0xF4, 0xF6),
            disabled: Color32::from_rgb(0xBF, 0xC7, 0xD4),
            disabled_fg: Color32::WHITE,
        }
    }

    /// **深色**（P40）：同一套口径的暗房版，三条不变 ——
    /// ① **只有颜色变**：间距/圆角/字号/控件尺寸与浅色完全相同（两套主题下三页布局一模一样，
    ///    `ui::layout` 量出来的必需高度在两种主题下都成立）；
    /// ② **色相不跑偏**：强调仍蓝、成功仍绿、警告仍琥珀、错误仍红；
    /// ③ **不用纯黑**：页面底 `#16181D`、卡片 `#212630`，靠一档亮度分层（深色下看不见阴影）。
    ///
    /// 亮度取值口径：正文/次要文字在卡片底上对比 ≥ 7:1；语义色的"底色"都取到
    /// "看得出是带色的一块、又不抢文字"的低饱和深底；时间线轨道比卡片**更暗**一档
    /// （`#12151C`），这样波形/标尺画上去才显得"在槽里"。
    pub fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(0x16, 0x18, 0x1D),
            card: Color32::from_rgb(0x21, 0x26, 0x30),
            line: Color32::from_rgb(0x36, 0x3D, 0x49),
            text: Color32::from_rgb(0xE9, 0xEC, 0xF2),
            muted: Color32::from_rgb(0xA6, 0xAF, 0xBE),
            faint: Color32::from_rgb(0x7A, 0x84, 0x94),
            accent: Color32::from_rgb(0x3B, 0x82, 0xF6),
            accent_hover: Color32::from_rgb(0x60, 0xA5, 0xFA),
            accent_soft: Color32::from_rgb(0x1B, 0x3A, 0x63),

            ok_fg: Color32::from_rgb(0x4E, 0xD8, 0xA0),
            ok_solid: Color32::from_rgb(0x12, 0xA4, 0x73),
            ok_solid_hover: Color32::from_rgb(0x0F, 0x91, 0x66),

            warn_bg: Color32::from_rgb(0x35, 0x28, 0x0F),
            warn_line: Color32::from_rgb(0x7E, 0x61, 0x18),
            warn_fg: Color32::from_rgb(0xE9, 0xB9, 0x49),

            err_bg: Color32::from_rgb(0x3A, 0x1A, 0x1C),
            err_line: Color32::from_rgb(0x8A, 0x2C, 0x2C),
            err_fg: Color32::from_rgb(0xF8, 0x71, 0x71),

            info_bg: Color32::from_rgb(0x24, 0x2A, 0x34),
            info_line: Color32::from_rgb(0x3B, 0x43, 0x51),
            info_fg: Color32::from_rgb(0xC7, 0xCE, 0xDA),

            danger: Color32::from_rgb(0xE5, 0x48, 0x4D),
            danger_hover: Color32::from_rgb(0xD3, 0x3A, 0x3F),

            track_bg: Color32::from_rgb(0x12, 0x15, 0x1C),
            ruler_tick: Color32::from_rgb(0x4E, 0x58, 0x67),
            ruler_text: Color32::from_rgb(0x98, 0xA2, 0xB3),
            cursor: Color32::from_rgb(0x6F, 0xA8, 0xFF),
            keyframe: Color32::from_rgb(0xE3, 0xBC, 0x66),
            empty_hatch: Color32::from_rgb(0x27, 0x2D, 0x38),
            scrim: Color32::from_black_alpha(110),

            ctrl_bg: Color32::from_rgb(0x2A, 0x2F, 0x39),
            ctrl_edge: Color32::from_rgb(0x41, 0x4A, 0x59),
            ctrl_bg_hover: Color32::from_rgb(0x34, 0x3B, 0x47),
            ctrl_edge_hover: Color32::from_rgb(0x4C, 0x57, 0x66),
            ctrl_bg_active: Color32::from_rgb(0x3E, 0x46, 0x53),
            ctrl_weak: Color32::from_rgb(0x26, 0x2B, 0x34),
            ctrl_weak_hover: Color32::from_rgb(0x2F, 0x35, 0x3F),
            ctrl_weak_active: Color32::from_rgb(0x39, 0x40, 0x4C),
            edit_bg: Color32::from_rgb(0x1A, 0x1E, 0x25),
            code_bg: Color32::from_rgb(0x1A, 0x1E, 0x25),
            disabled: Color32::from_rgb(0x3B, 0x43, 0x51),
            disabled_fg: Color32::from_rgb(0xC9, 0xD1, 0xDC),
        }
    }
}

// ---------------------------------------------------------------------------
// 取色函数（调用点用它们；**不要在调用点判断主题**）
// ---------------------------------------------------------------------------

/// 窗口/页面底色
pub fn bg() -> Color32 {
    palette().bg
}
/// 卡片底
pub fn card_fill() -> Color32 {
    palette().card
}
/// 发丝线：分隔线 / 卡片描边 / 输入框描边
pub fn line() -> Color32 {
    palette().line
}
/// 正文
pub fn text() -> Color32 {
    palette().text
}
/// 次要文字
pub fn muted() -> Color32 {
    palette().muted
}
/// 更淡的提示文字
pub fn faint() -> Color32 {
    palette().faint
}
/// 唯一强调色
pub fn accent() -> Color32 {
    palette().accent
}
/// 强调色悬停
pub fn accent_hover() -> Color32 {
    palette().accent_hover
}
/// 强调色浅底（选中行背景）
pub fn accent_soft() -> Color32 {
    palette().accent_soft
}

/// 成功：文字
pub fn ok_fg() -> Color32 {
    palette().ok_fg
}
/// 成功色实心
pub fn ok_solid() -> Color32 {
    palette().ok_solid
}
/// 成功色实心·悬停
pub fn ok_solid_hover() -> Color32 {
    palette().ok_solid_hover
}

/// 警告
pub fn warn_bg() -> Color32 {
    palette().warn_bg
}
pub fn warn_line() -> Color32 {
    palette().warn_line
}
pub fn warn_fg() -> Color32 {
    palette().warn_fg
}

/// 错误
pub fn err_bg() -> Color32 {
    palette().err_bg
}
pub fn err_line() -> Color32 {
    palette().err_line
}
pub fn err_fg() -> Color32 {
    palette().err_fg
}

/// 中性信息框
pub fn info_bg() -> Color32 {
    palette().info_bg
}
pub fn info_line() -> Color32 {
    palette().info_line
}
pub fn info_fg() -> Color32 {
    palette().info_fg
}

/// 危险操作（停止录制）
pub fn danger() -> Color32 {
    palette().danger
}
/// 危险操作·悬停
pub fn danger_hover() -> Color32 {
    palette().danger_hover
}

/// 时间线：轨道底 / 刻度 / 尺文字 / 当前标尺 / 关键帧 / 片尾斜纹 / 遮罩
pub fn track_bg() -> Color32 {
    palette().track_bg
}
pub fn ruler_tick() -> Color32 {
    palette().ruler_tick
}
pub fn ruler_text() -> Color32 {
    palette().ruler_text
}
pub fn cursor() -> Color32 {
    palette().cursor
}
pub fn keyframe() -> Color32 {
    palette().keyframe
}
pub fn empty_hatch() -> Color32 {
    palette().empty_hatch
}
pub fn scrim() -> Color32 {
    palette().scrim
}

/// 禁用态填充（按钮变灰用）
pub fn disabled() -> Color32 {
    palette().disabled
}
/// 禁用态上的文字色
pub(crate) fn disabled_fg() -> Color32 {
    palette().disabled_fg
}

// ---------------------------------------------------------------------------
// 间距 / 圆角 / 字号
// ---------------------------------------------------------------------------

/// 卡片内边距
pub(crate) const PAD_CARD: f32 = 14.0;
/// **紧凑卡片**的内边距（录制页用）。
///
/// 为什么录制页要单独一套：这一页的高度**完全由卡片堆叠决定**（没有剪辑页那种"吃满剩余
/// 高度"的流式预览），而它又是"程序最小窗口高度"的两个候选之一 —— 每省下 1px 内边距，
/// 最小窗口就能矮 1px。压缩历史：14 → 10（整页 −32px）→ **8**（P37，四张卡片再省 16px）。
/// 8px 上下留白在浅色主题下控件不贴边，配合 [`GAP_CARD_TIGHT`] 仍然看得出"一张卡"。
pub const PAD_CARD_TIGHT: f32 = 8.0;
/// **紧凑卡片之间的间距**（录制页用）：12 → 8（省 12px）→ **6**（P37，三处再省 6px）。
/// 配合 [`PAD_CARD_TIGHT`]。用户口径"录制页紧凑一些"：卡片间距只留 6px 仍能分辨卡片边界。
pub const GAP_CARD_TIGHT: f32 = 6.0;
/// **紧凑条**的左右内边距（剪辑页底部的"文件信息 / 操作指南"两行用）
pub(crate) const PAD_BAR_X: i8 = 8;
/// **紧凑条**的上下内边距：只留 2px，让这两行的高度"刚好容纳一行文字"。
///
/// 为什么这么抠：它们钉在剪辑页底部**常驻**，每多 1px 上下内边距，就是预览少 1px 高
/// （预览吃的是"扣掉这两行之后"的剩余高度，见 `ui::editor` 的 `bottom_rows`）。
pub const PAD_BAR_Y: i8 = 2;
/// 卡片之间的间距
pub const GAP_CARD: f32 = 12.0;
/// 行间距
pub const GAP_ROW: f32 = 9.0;
/// **录制页专用的行间距**（P37，"录制页紧凑一些"）：9 → 6。
///
/// 为什么不直接改 [`GAP_ROW`]：剪辑页与设置页也用 GAP_ROW，改它会牵动两页的实测必需高度
///（`EDITOR_REQ_H` 是 557 量出来的，动行距就得重新量两页）。录制页的行距是录制页自己的事，
/// 单独一个常量，改动范围锁死在这一页。
pub const GAP_ROW_TIGHT: f32 = 6.0;
/// 水平布局里"最后一个部件右缘"的探针容差：`item_spacing.x = 8`
/// （`record_page_capture_and_audio_rows_fit` 用游标左缘近似最后一个部件右缘，会多算一格）。
pub const ITEM_SPACING_SLACK: f32 = 8.0;
/// 分区之间的间距
pub const GAP_SECTION: f32 = 18.0;
/// 圆角（统一 8px）
pub(crate) const RADIUS: u8 = 8;
/// 普通控件高度
pub const H_CONTROL: f32 = 30.0;
/// 主操作按钮高度
pub const H_PRIMARY: f32 = 44.0;

/// **录制页状态卡片里那个主操作按钮**（「开始录制」/「停止录制」）的宽 × 高。
///
/// 它**不是** [`H_PRIMARY`]（44）那档：用户要求这个按钮"明显变小"，
/// 与时间码同一行时用 34 高、132 宽更协调，也**不能被计时器字号牵着走**。
///
/// 为什么提成常量：本轮"把状态卡片那一行压低"**明确不动这个按钮**，
/// 而"不动"必须有据可查 —— 常量集中在这里，探针单测就能断言"行高压下来、按钮尺寸不变"。
/// 这两个值也是那一行的**高度下限**（行高 ≥ 按钮高）。
pub const START_BTN_W: f32 = 132.0;
/// 见 [`START_BTN_W`]。
pub const START_BTN_H: f32 = 34.0;

/// 页面标题字号
pub(crate) const FS_TITLE: f32 = 20.0;
/// 分区标题字号
pub const FS_SECTION: f32 = 14.0;
/// 正文字号
pub const FS_BODY: f32 = 13.5;
/// 次要说明字号
pub const FS_SMALL: f32 = 12.0;
/// **录制页状态文字**（"就绪 / 录制中 / 暂停"）的字号。
///
/// 为什么单列一个常量：它是**录制页状态卡片那一行**（状态文字 + 时间码 + 主按钮）
/// 的高度贡献者之一，而那一行的高度直接进"录制页必需高度"（见 [`crate::ui::layout`]）。
/// 原来它散落在 `main_page.rs` 里的字面量 `14.0`（与 [`FS_SECTION`] 数值相同但**语义不同**：
/// 那是"分区标题"，这是"状态文字"），所以提到这里，保持"字号只有一处"。
pub const FS_STATUS: f32 = 14.0;
/// **录制计时器**（`MM:SS` 大号等宽）的字号 —— **单一来源**。
///
/// 它是"录制页状态卡片那一行"（状态文字 + 时间码 + 主按钮）的高度贡献者之一，
/// 而那一行的高度直接进"录制页必需高度"（见 [`crate::ui::layout`]）。
/// 之前这个数字是写在 `mono_big` 里的字面量 `34.0`，现在提成常量：
/// 调字号只动这一处，不会在下游散落魔法数字。
///
/// **34.0 → 29.0**（用户要求："`00:00` 计时器字号调小一档" + "把那一行压低一点"）。
/// 为什么正好是 29.0（而不是"凭感觉小一档"）：这一行的高度**就是时间码自己的行高**，
/// 所以要压行高只能降字号；而它**不能低于主按钮**（[`START_BTN_H`]，34px）——
/// 再低也只是按钮说了算，行高不会再降，而数字会小得没必要。
/// 实测（`main_page::tests::probe_timer_font_heights`，等宽 `Monospace` + `layout_no_wrap`）：
///
/// | 字号 | 时间码行高 | 那一行的高度 |
/// |---|---|---|
/// | 34.0（改前） | 40 | **40**（时间码撑高，按钮 34 被架空） |
/// | 31.0 | 36 | 36 |
/// | 30.0 | 35 | 35 |
/// | **29.0（改后）** | **34** | **34**（= 按钮高度，两者刚好齐平） |
/// | 28.0 及以下 | 33… | 34（按钮说了算，行高不再降） |
///
/// 所以 29.0 就是"**行高压到最低、字号又只降一档**"的那个点：那一行 40 → 34（少 6px），
/// 时间码 34 → 29。等宽字体族与对齐方式**与字号无关**（`mono_big` 用 `FontId::monospace`），
/// 实测 `00:00`/`00:01`/`09:59`/`59:59` 在 29.0 下**宽度完全相同** → 录制中数字不会跳动。
pub const FS_TIMER: f32 = 29.0;

/// 统一圆角
pub fn radius() -> CornerRadius {
    CornerRadius::same(RADIUS)
}

/// 发丝线（1px，颜色随主题）
pub fn hairline() -> Stroke {
    Stroke::new(1.0, line())
}

/// 把主题应用到整个 `Context`：**启动时 / 切换深色模式时**调用，三页共用。
///
/// 用**当前线程的主题种类**（见 [`kind`]，默认浅色）—— 绝大多数调用点
/// （三页的无窗口单测、启动路径）只想"按当前主题重刷一遍样式"，不需要指定种类；
/// 要同时切种类的调用方（设置页的深色模式开关、启动时按配置初始化）用 [`apply_kind`]。
pub fn apply(ctx: &egui::Context) {
    apply_kind(ctx, kind())
}

/// 切到 `kind` 这一套主题并应用（设置页的深色模式开关走这里）。
///
/// 主题偏好**钉死**为 `kind` 对应的那一个（不跟随系统）：两套皮肤都是我们自己调过的，
/// 跟随系统只会在用户深色桌面上给出没调过的那半套。样式同时写进 egui 的
/// Light 与 Dark 两个键 —— 偏好被钉死了，写两边只是让"谁来问都能问到同一套"。
pub fn apply_kind(ctx: &egui::Context, kind: ThemeKind) {
    KIND.set(kind);
    ctx.set_theme(kind.preference());
    let base = match kind {
        ThemeKind::Light => egui::Theme::Light,
        ThemeKind::Dark => egui::Theme::Dark,
    };
    let mut style = (*ctx.style_of(base)).clone();
    let p = palette();

    // ---- 颜色 ----
    // **不从 `Visuals::light()/dark()` 的默认值起步**：默认值里灰阶 token 的取向
    // （按钮底、滑轨、悬停）与我们的设计口径不一致，逐项覆盖又容易漏。
    // 这里只用"两个主题各自的骨架"（文字/交互的默认对比），其余全部显式写。
    let mut v = egui::Visuals::light();
    v.dark_mode = kind.is_dark();
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = hairline();
    v.window_corner_radius = radius();
    v.menu_corner_radius = radius();
    v.extreme_bg_color = p.card; // 输入框底
    v.text_edit_bg_color = Some(p.edit_bg);
    v.faint_bg_color = p.edit_bg; // 表格斑马纹
    v.code_bg_color = p.code_bg;
    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.muted);
    v.warn_fg_color = p.warn_fg;
    v.error_fg_color = p.err_fg;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    // 不堆阴影：现代清爽靠留白和发丝线分层，不靠投影
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    // 滑条：细瘦滑轨（6px）+ 圆形滑块 + 已填充段用强调色（Trailing fill 取 selection.bg_fill）
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    // 选中文字用"强调色底 + 白字"：两个主题下都一样清楚（白字压在饱和蓝上）
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);

    let line = hairline();
    // 无交互背景（标签、网格底）
    v.widgets.noninteractive.bg_fill = p.card;
    v.widgets.noninteractive.weak_bg_fill = p.card;
    v.widgets.noninteractive.bg_stroke = line;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.corner_radius = radius();
    v.widgets.noninteractive.expansion = 0.0;

    // 普通（按钮的静息态）：卡片底 + 发丝描边。
    // 注意 `bg_fill` 与 `weak_bg_fill` 的分工：`bg_fill` 是"必须有底色"的部件
    // （滑条滑轨、复选框底）用的，`weak_bg_fill` 才是按钮的可选底色。
    // 滑条滑轨要用可见的浅灰（浅色）/ 深灰（深色），否则"底与轨同色"就只能看到一个圆点。
    // 三态 token 全部来自 palette（见 `Palette::ctrl_*`），不再在这里写死色值 ——
    // 写死的那天就是深色主题下"白轨白底"的那天。
    v.widgets.inactive.bg_fill = p.ctrl_bg;
    v.widgets.inactive.weak_bg_fill = p.ctrl_weak;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.ctrl_edge);
    // 文字/勾选标记用正文色：中性为主，强调色只留给选中与主操作
    v.widgets.inactive.fg_stroke = Stroke::new(1.6, p.text);
    v.widgets.inactive.corner_radius = radius();
    v.widgets.inactive.expansion = 0.0;

    // 悬停：底色只加深/提亮一点点（4~6%），描边略深，不发光
    v.widgets.hovered.bg_fill = p.ctrl_bg_hover;
    v.widgets.hovered.weak_bg_fill = p.ctrl_weak_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.ctrl_edge_hover);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, p.accent);
    v.widgets.hovered.corner_radius = radius();
    v.widgets.hovered.expansion = 0.0;

    // 按下：再深一点，并略微缩进（expansion 取负值 = 视觉上略缩）
    v.widgets.active.bg_fill = p.ctrl_bg_active;
    v.widgets.active.weak_bg_fill = p.ctrl_weak_active;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.5, p.accent);
    v.widgets.active.corner_radius = radius();
    v.widgets.active.expansion = -0.5;

    // 展开的折叠/菜单
    v.widgets.open.bg_fill = p.card;
    v.widgets.open.weak_bg_fill = p.card;
    v.widgets.open.bg_stroke = line;
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.open.corner_radius = radius();

    style.visuals = v;

    // ---- 间距 ----
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.menu_margin = Margin::same(6);
    style.spacing.window_margin = Margin::same(12);
    style.spacing.indent = 16.0;
    style.spacing.interact_size = Vec2::new(0.0, H_CONTROL);
    style.spacing.slider_width = 180.0;
    // 细瘦滑轨（4px）
    style.spacing.slider_rail_height = 4.0;
    style.spacing.text_edit_width = 220.0;
    style.spacing.icon_width = 16.0;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating = true;
    // 滑条细瘦（4px）+ 圆形滑块，符合"清爽"口径
    style.visuals.widgets.inactive.corner_radius = radius();
    style.spacing.combo_width = 180.0;

    // ---- 字号 ----
    use egui::{FontFamily::Monospace, FontFamily::Proportional};
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::new(FS_TITLE, Proportional));
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(FS_BODY, Proportional));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::new(FS_BODY, Proportional));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::new(FS_SMALL, Proportional));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::new(FS_BODY, Monospace));

    // 偏好已被 `set_theme` 钉死，两个键写同一套即可（谁来问都是这一套）
    ctx.set_style_of(egui::Theme::Light, style.clone());
    ctx.set_style_of(egui::Theme::Dark, style);
}

// ---------------------------------------------------------------------------
// 小组件（三页共用的"积木"）
// ---------------------------------------------------------------------------

/// **横幅的严重度分级**（三个语义，样式必须一眼可辨）：
///
/// | 级别 | 用途 | 外观 |
/// |---|---|---|
/// | [`Banner::Info`] | **正常信息**：目录自动回退、设置已保存、已恢复默认 | 中性蓝灰底 + 细小字 |
/// | [`Banner::Warn`] | 影响结果但程序能继续：本次录制没有声音、编码跟不上目标帧率、CABAC 被忽略 | 琥珀底 |
/// | [`Banner::Error`] | **真正的失败**：录制启动失败、采集/编码/封装错误、导出失败、配置完全存不下 | 红底 + 加粗 |
///
/// 之所以要分开：**"自动回退"是预期行为，不该用红色（= 出错）呈现** ——
/// 用户一打开界面就看到红字，会以为程序坏了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Banner {
    /// 正常信息（中性蓝灰）
    Info,
    /// 警告：能继续，但结果可能受影响（琥珀）
    Warn,
    /// 真失败（红）
    Error,
}

impl Banner {
    fn colors(self) -> (Color32, Color32, Color32) {
        match self {
            Banner::Info => (info_bg(), info_line(), info_fg()),
            Banner::Warn => (warn_bg(), warn_line(), warn_fg()),
            Banner::Error => (err_bg(), err_line(), err_fg()),
        }
    }
}

/// 白色卡片：16px 内边距 + 8px 圆角 + 发丝描边（没有阴影）
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(card_fill())
        .stroke(hairline())
        .corner_radius(radius())
        .inner_margin(Margin::same(PAD_CARD as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// 一帧里量到的**卡片几何**（只在录制页的逐部件高度探针里用，见 [`card_tight`]）。
///
/// * `outer_h` —— 整卡高度：**含**上下内边距与 1px 描边（= 这一行布局真正占掉的高度）；
/// * `inner_h` —— 卡片**内容**高度（不含内边距与描边）；
/// * `pad` —— 卡片内边距（[`PAD_CARD_TIGHT`]）。
///
/// 为什么由 `card_tight` 自己报：这样"逐部件高度表"里的每个数字都来自**真实布局**
/// （`Frame::show` 画出来的那个矩形），而不是探针另算的一份近似值。
#[derive(Debug, Default, Clone, Copy)]
pub struct CardMeasure {
    /// 整卡高度（含内边距 + 描边）
    pub outer_h: f32,
    /// 卡片内容高度（不含内边距 + 描边）
    pub inner_h: f32,
    /// 内边距（上下各这么多）
    pub pad: f32,
}

impl CardMeasure {
    /// 上下内边距合计（`2 × pad`）。
    pub fn pad_total(&self) -> f32 {
        self.pad * 2.0
    }
}

/// **紧凑白卡片**（内边距 8px，P37 起）：录制页专用，见 [`PAD_CARD_TIGHT`]。
///
/// 与 [`card`] 唯一的区别就是内边距（14 → 10 → 8）：录制页的高度**直接决定程序最小窗口高度**
/// （见 `ui::layout`），所以那里的留白按"够用就好"给，而不是按"宽松"给。
/// 8px 是 P37"录制页更紧凑"的落点：四张卡片共省 16px，控件仍有 8px 呼吸空间、不贴卡片边。
///
/// 返回值是 `(闭包的返回值, 卡片几何)`：几何那一项是**录制页逐部件高度探针**的数据源
///（`main_page` 的 `#[ignore]` 探针单测）；常规调用忽略它即可 —— 测量本身只是一次
/// `Response::rect` 读取，没有额外布局开销。
pub fn card_tight<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> (R, CardMeasure) {
    let mut measure = CardMeasure {
        pad: PAD_CARD_TIGHT,
        ..CardMeasure::default()
    };
    let (inner, rect_h) = ui
        .scope(|ui| {
            let r = egui::Frame::NONE
                .fill(card_fill())
                .stroke(hairline())
                .corner_radius(radius())
                .inner_margin(Margin::same(PAD_CARD_TIGHT as i8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    add(ui)
                });
            (r.inner, r.response.rect.height())
        })
        .inner;
    measure.outer_h = rect_h;
    measure.inner_h = (rect_h - measure.pad_total() - 2.0).max(0.0);
    (inner, measure)
}

/// **紧凑白卡片**（左右 8px、上下 2px）：给"刚好一行文字"的常驻信息行用。
///
/// 与 [`card`] 的唯一区别就是上下内边距（14 → 2）。剪辑页底部的**文件信息行**用它：
/// 那一行常驻在预览下方，上下多余的内边距就是白白从预览身上抠掉的高度。
pub fn compact_card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(card_fill())
        .stroke(hairline())
        .corner_radius(radius())
        .inner_margin(Margin::symmetric(PAD_BAR_X, PAD_BAR_Y))
}

/// **紧凑无底色条**（左右 8px、上下 2px）：剪辑页底部**常驻操作指南**用它。
///
/// 不留底色、不带描边（它贴在窗口底色上），只把上下内边距收到 2px，
/// 保证"文字上下沿各留 1~2px"而不被裁掉。
pub fn compact_plain_frame() -> egui::Frame {
    egui::Frame::NONE.inner_margin(Margin::symmetric(PAD_BAR_X, PAD_BAR_Y))
}

/// 带底色与描边的横幅（按 [`Banner`] 分级；不再是一行孤零零的红字）
pub fn banner(ui: &mut egui::Ui, kind: Banner, text: &str) {
    let _ = banner_full(ui, kind, text, None);
}

/// 完整横幅：`close_label` 给定时，右侧有一个"关闭"小按钮
/// （按钮区宽度先算好，长文案换行时不会被文字挤走）。
pub(crate) fn banner_full(
    ui: &mut egui::Ui,
    kind: Banner,
    text: &str,
    close_label: Option<&str>,
) -> bool {
    let (bg, line, fg) = kind.colors();
    let mut dismissed = false;
    egui::Frame::NONE
        .fill(bg)
        .stroke(Stroke::new(1.0, line))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // 错误用正文号 + 加粗（必须醒目），info/warn 用细小字（不抢视线）
            let styled = || match kind {
                Banner::Error => RichText::new(text).size(FS_BODY).color(fg).strong(),
                _ => RichText::new(text).size(FS_SMALL).color(fg),
            };
            match close_label {
                None => {
                    // 竖排（不在 horizontal 里）：长文案正常换行，不会被裁掉
                    ui.add(egui::Label::new(styled()));
                }
                Some(label) => {
                    ui.horizontal(|ui| {
                        let text_w = (ui.available_width() - 52.0 - 6.0).max(120.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(text_w, 0.0),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                ui.add(egui::Label::new(styled()));
                            },
                        );
                        if ui.small_button(label).clicked() {
                            dismissed = true;
                        }
                    });
                }
            }
        });
    dismissed
}

/// 设置页两列网格用的固定宽度：滑轨宽度 / 数值框宽度（两行共用，保证左边缘与端点对齐）
pub const SETTINGS_SLIDER_W: f32 = 220.0;
/// 数值框宽度（"5.0 Mbps" 与 "30 帧/秒" 用同一个框，看起来是一列整齐的字段）
pub(crate) const SETTINGS_VALUE_W: f32 = 104.0;

/// **"左标签 / 右控件"一行的标签列宽**（逻辑点）：录制页那四行都用它，
/// 保证"录制范围 / 显示器 / 录制内容 / 存储位置"四个标签的右边缘对齐。
///
/// 值 88 原来散落在 `main_page.rs` 的 `field_row(.., 88.0, ..)` 里（四处字面量），
/// 现在集中到这里：它是**列宽**而不是字号，但同属"界面栅格"，所以和间距常量放在一起。
pub const FIELD_LABEL_W: f32 = 88.0;

/// **采集行专用的标签列宽**（P38，仅"显示器"那一行用）：72。
///
/// 为什么不跟 [`FIELD_LABEL_W`]（88）对齐：P38 把「录制范围」整组控件并到了显示器右边，
/// 而「显示器 / Monitor」两个词都短（≤ 48px），88px 里有 40px 是纯浪费——
/// 这 16px 差正是英文界面"控件不超卡片右缘"的余量之一。
/// 其余三行（录制内容 / 存储位置 / 音源）不变，列错位仅此一处、肉眼几乎不可见。
pub const FIELD_LABEL_W_MONITOR: f32 = 72.0;

/// "左标签 / 右控件"对齐的一行（标签固定宽度，右侧控件左对齐）
pub fn field_row<R>(
    ui: &mut egui::Ui,
    label: &str,
    label_w: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(label_w, H_CONTROL),
            egui::Label::new(RichText::new(label).color(muted())).halign(egui::Align::LEFT),
        );
        add(ui)
    })
    .inner
}

/// 主操作按钮（强调色实心、44px）
pub fn primary_button(ui: &mut egui::Ui, text: &str, width: f32) -> egui::Response {
    primary_button_enabled(ui, text, width, true)
}

/// 主操作按钮（可禁用）：**占满一行里的 `width × H_PRIMARY`**（按当前布局推进游标）。
pub(crate) fn primary_button_enabled(
    ui: &mut egui::Ui,
    text: &str,
    width: f32,
    enabled: bool,
) -> egui::Response {
    primary_button_with(ui, text, enabled, |ui, btn| {
        ui.add_sized(Vec2::new(width, H_PRIMARY), btn)
    })
}

/// **中性按钮**（P41：用户要求"去掉按钮的颜色"，见 2c.png 红框）。
///
/// 静息 = 卡片底 + 发丝描边 + 正文色文字（与 [`ghost_button`] 完全同一套默认样式，
/// 差别只在"怎么放"：本函数把按钮交给调用方用 `add_sized` / `put` 摆）；
/// 禁用 = 控件灰底（无描边）+ 次要色文字。
///
/// 为什么禁用态要显式给一块灰底：egui 的禁用态与静息态**同色**（区别只是不响应），
/// 而剪辑页这几颗按钮的禁用是有语义的（还没点过时间线 / 正在导出），必须看得出来。
///
/// 颜色**不写死**：全部取自当前主题的控件样式（`widgets.inactive`），
/// 所以浅色与深色两套主题下都自动成立 —— 写死色值的那天，
/// 就是深色主题下一颗白按钮挂在深灰页面上的那天。
pub fn neutral_button<'a>(
    ui: &egui::Ui,
    text: &'a str,
    size: f32,
    enabled: bool,
) -> egui::Button<'a> {
    let rich = if enabled {
        RichText::new(text).size(size)
    } else {
        RichText::new(text).size(size).color(muted())
    };
    let btn = egui::Button::new(rich);
    if enabled {
        btn
    } else {
        btn.fill(ui.visuals().widgets.inactive.bg_fill)
            .stroke(Stroke::NONE)
    }
}

/// 两个主按钮入口共用的**外观 + 禁用 + 悬停配色**（只有"怎么放"不同）。
fn primary_button_with(
    ui: &mut egui::Ui,
    text: &str,
    enabled: bool,
    place: impl FnOnce(&mut egui::Ui, egui::Button<'_>) -> egui::Response,
) -> egui::Response {
    // 禁用色也走 palette：浅色是浅灰 + 白字，深色是深灰 + 浅字（各主题自己配好一对）
    let (fill, hover, fg) = if enabled {
        (accent(), accent_hover(), Color32::WHITE)
    } else {
        (disabled(), disabled(), disabled_fg())
    };
    let btn = egui::Button::new(RichText::new(text).size(15.0).color(fg).strong())
        .fill(fill)
        .stroke(Stroke::NONE)
        .corner_radius(radius());
    ui.scope(|ui| {
        ui.style_mut().visuals.widgets.hovered.weak_bg_fill = hover;
        ui.style_mut().visuals.widgets.hovered.bg_fill = hover;
        ui.style_mut().visuals.widgets.hovered.bg_stroke = Stroke::NONE;
        ui.style_mut().visuals.widgets.active.weak_bg_fill = hover;
        ui.style_mut().visuals.widgets.active.bg_fill = hover;
        ui.add_enabled_ui(enabled, |ui| place(ui, btn)).inner
    })
    .inner
}

/// **「播放 / 停止」按钮**（**中性描边** + 矢量图标 + 文字），画在给定矩形里。
///
/// 为什么图标要自己画、而不是往文案里塞 `▶` / `■`：
/// 那两个字模是 **U+25B6 / U+25A0**，内嵌字体子集里没有它们（子集只按语言包的字收），
/// 塞进文案就会渲染成豆腐块。用 `Painter` 画三角形/方块则与字体无关，
/// 任何语言、任何 DPI 下都稳定。
///
/// * `playing` = 当前正在播放：图标画成**方块**（停止），文案由调用方给"停止"；
/// * **P41 起按钮颜色全部去掉**（用户要求，见 2c.png 红框）：原来两种状态都是
///   **绿色实心底**，现在改成与「打开视频」等按钮一致的中性外观 ——
///   底色/描边/文字色**直接读当前主题的控件样式**（`widgets.inactive` / `hovered`），
///   所以浅色与深色两套主题下都自动成立，不需要为深色另抄一份；
/// * `enabled = false`（还没打开视频）时按钮变灰且不响应点击。
pub fn play_button_in(
    ui: &mut egui::Ui,
    label: &str,
    rect: egui::Rect,
    playing: bool,
    enabled: bool,
) -> egui::Response {
    // **固定 id，绝不从矩形位置推导**：剪辑页刚出现的那一两帧里预览尺寸还在落定，
    // 按钮矩形会跟着挪几个像素；用位置当 id 就会"按下时是 A 号控件、松开时是 B 号控件"，
    // egui 于是判定这次点击不属于任何控件（实测症状：按钮怎么点都没反应）。
    let id = ui.id().with("cap_editor_play_btn");
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let resp = ui.interact(rect, id, sense);
    // P41：**去掉按钮颜色**（用户要求，见 2c.png 红框）。底色/描边/图标文字色全部取自
    // 当前主题的控件样式（`inactive` 静息 / `hovered` 悬停），与 `ghost_button` 等默认按钮
    // **同一套外观来源**，于是"这颗按钮看起来和其他按钮一样素"这件事不可能跑偏。
    // 禁用态 = 控件灰底 + 不描边，与 [`neutral_button`] 同一套口径（egui 自己的禁用
    // 只是"不响应"，外观与静息态同色，看得出来的那份得自己画）。
    let v = ui.visuals();
    let (fill, edge, fg) = if !enabled {
        (v.widgets.inactive.bg_fill, Stroke::NONE, muted())
    } else if resp.hovered() {
        (v.widgets.hovered.weak_bg_fill, v.widgets.hovered.bg_stroke, text())
    } else {
        (v.widgets.inactive.weak_bg_fill, v.widgets.inactive.bg_stroke, text())
    };
    let painter = ui.painter();
    painter.rect_filled(rect, radius(), fill);
    if edge.width > 0.0 {
        painter.rect_stroke(rect, radius(), edge, egui::StrokeKind::Inside);
    }

    // 图标 + 文字作为**一组**居中（图标在左、文字在右，整体水平居中）
    let galley = painter.layout_no_wrap(label.to_owned(), egui::FontId::proportional(15.0), fg);
    const ICON_W: f32 = 10.0;
    const ICON_GAP: f32 = 7.0;
    let total_w = ICON_W + ICON_GAP + galley.size().x;
    let left = rect.center().x - total_w / 2.0;
    let cy = rect.center().y;
    let icon = egui::Rect::from_center_size(
        egui::pos2(left + ICON_W / 2.0, cy),
        egui::vec2(ICON_W, 11.0),
    );
    if playing {
        // 停止：实心方块
        painter.rect_filled(icon, 1.5, fg);
    } else {
        // 播放：实心三角形（顶点朝右）
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(icon.left(), icon.top()),
                egui::pos2(icon.right(), cy),
                egui::pos2(icon.left(), icon.bottom()),
            ],
            fg,
            Stroke::NONE,
        ));
    }
    painter.galley(
        egui::pos2(left + ICON_W + ICON_GAP, cy - galley.size().y / 2.0),
        galley,
        fg,
    );
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// 次要按钮（白底 + 发丝描边）
pub fn ghost_button(ui: &mut egui::Ui, s: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(s).color(text())).corner_radius(radius()))
}

/// **"数值框"的统一尺寸**（P26）：设置页那一列的两个数值框 —— 可编辑的**目标码率**
/// （`DragValue`）与只读的**帧率** —— 都必须从这里取尺寸。
///
/// 为什么要有这个函数（P26 的教训）：两行原来是**两份各自写死的 `add_sized([..])`**，
/// 于是只读框（`Frame` + 左对齐 `Label`）与可编辑框（`DragValue` → 居中 `Button`）
/// 在"文本怎么对齐"这件事上**各走各的** —— 用户看到的就是「设置页，30 帧/秒，
/// 没有在框中居中。」。现在两框共用**同一个尺寸来源 + 同一套文本居中口径**：
/// * 尺寸：本函数；
/// * 容器与对齐：[`read_only_value_box`]（只读一侧）与 `DragValue`（可编辑一侧）
///   都走 egui 的**按钮样式**（`Style::button_style` → 同底色/同描边/同圆角/同内边距），
///   文本由 `add_sized` 的**居中布局**摆正（`Ui::add_sized` 用的就是
///   `Layout::centered_and_justified`）—— 一侧改口径，另一侧不可能"没跟上"。
pub fn number_box_size() -> Vec2 {
    Vec2::new(SETTINGS_VALUE_W, H_CONTROL)
}

/// **只读数值框**（P26 重做）：与同一列里可编辑的数值框（`DragValue`）**同尺寸、同容器、
/// 同文本对齐**，但**不可聚焦、无光标、无选中高亮、不进 Tab 焦点链**。
///
/// 为什么需要它（P11 起的需求）：设置页的帧率是"只能由滑尺决定"的两档值（30 / 60），
/// 用 `DragValue` 呈现时它的文本会以"被选中的样子"显示（强调色高亮底），
/// 看起来像个能输入的文本框，而实际上打字没有用 —— 用户明确要求改成只读样式。
///
/// **P26 的实现口径**：用 `egui::Button` + `Sense::hover()`：
/// * `Button` 的框就是 `DragValue` 静息态用的那一套 `button_style`（**同一个来源**），
///   尺寸走 [`number_box_size`] → 两框外观完全一致；
/// * `Button` 的文本由 egui 的原子布局在框内**水平 + 垂直居中**（与 `DragValue` 同一个
///   布局实现），于是"30 帧/秒"与"5.0 Mbps"两个文本矩形分别落在各自框的正中；
/// * `Sense::hover()` = **空 sense**：`Response::widget_state()` 因此是 `Noninteractive`，
///   不可点击、不可聚焦（`Sense::is_focusable()` 为 `false`）、不进 Tab 链、没有光标变化，
///   也不会画出"被选中"的高亮 —— 但**外观**与可编辑框一样（白底 + 发丝描边 + 同圆角）。
pub fn read_only_value_box(ui: &mut egui::Ui, s: &str) -> egui::Response {
    ui.add_sized(
        number_box_size(),
        egui::Button::new(RichText::new(s).size(FS_BODY).color(text()))
            .sense(egui::Sense::hover()),
    )
}

/// 等宽小字（时间码 / 路径 / 统计数字）
pub fn mono(s: &str) -> RichText {
    RichText::new(s).monospace().color(text())
}

/// 等宽次要小字
pub fn mono_muted(text: &str) -> RichText {
    RichText::new(text).monospace().color(muted())
}

/// 大号等宽（录制时长 `MM:SS` 那种主角数字）
///
/// 字号来自 [`FS_TIMER`]（单一来源）；字体族是**等宽**、`RichText` 不做额外对齐设置，
/// 所以计时过程中数字的宽度不变、不会左右跳动。
pub fn mono_big(s: &str) -> RichText {
    RichText::new(s)
        .font(FontId::monospace(FS_TIMER))
        .color(text())
}

/// 统计网格：两列「标签 : 值」，等宽数字右对齐，成组排列（不再挤成一行）。
///
/// 标签参数是 `L: AsRef<str>` 而不是 `&str`：调用方（`main_page::stats_grid`）
/// 手里的标签是 `I18n::t()` 返回的 owned `String`，若这里只收 `&str`，
/// 调用方就得为了"把 String 变成 &str"整表 clone 一遍再收集成第二个 Vec ——
/// 而这段代码**每帧都在跑**（录制中刷新统计）。放宽成泛型后调用方直接传切片即可（P51）。
pub fn stat_grid<L: AsRef<str>>(ui: &mut egui::Ui, rows: &[(L, String)]) {
    const PER_ROW: usize = 3;
    egui::Grid::new(ui.next_auto_id())
        .num_columns(PER_ROW * 2)
        .spacing([12.0, 7.0])
        .show(ui, |ui| {
            for (i, (label, value)) in rows.iter().enumerate() {
                ui.label(RichText::new(label.as_ref()).size(FS_SMALL).color(muted()));
                ui.label(RichText::new(value).monospace().size(FS_BODY).color(text()));
                if (i + 1) % PER_ROW == 0 {
                    ui.end_row();
                }
            }
        });
}

/// 状态点（静态小圆点，不做闪烁动画）。返回它占用的矩形（供逐部件高度探针读取）。
pub fn status_dot(ui: &mut egui::Ui, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
    resp
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对亮度（sRGB → 线性 → Rec.709 加权）。深浅两套主题的分层/对比都靠它断言。
    fn lum(c: Color32) -> f32 {
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

    /// **P40 守门测试（深色那份必须是"暗房版"）**。
    ///
    /// 深色不是把浅色反转一下就完事，三条硬约束：
    /// ① **亮度分层方向反过来**：浅色是"底亮、卡更亮、文字暗"，深色是"底暗、卡亮一档、
    ///    文字最亮" —— 两套都必须是同一种"靠亮度分层"的结构；
    /// ② **不用纯黑**：纯黑上再画描边/分隔线会糊成一片，所以每一档都留一点亮度；
    /// ③ **正文对比足够**：深色下 `text` 在 `card` 上的对比度 ≥ 4.5:1（WCAG AA），
    ///    `muted` ≥ 3:1（次要文字底线）。
    #[test]
    fn p40_dark_palette_is_a_dark_room_version() {
        let l = Palette::light();
        let d = Palette::dark();
        // ① 分层方向
        assert!(lum(l.bg) > lum(l.text), "浅色：底必须比文字亮");
        assert!(lum(l.card) > lum(l.bg), "浅色：卡片比页面底再亮一档");
        assert!(lum(d.bg) < lum(d.text), "深色：底必须比文字暗");
        assert!(lum(d.card) > lum(d.bg), "深色：卡片比页面底再亮一档");
        assert!(lum(d.text) > lum(d.card), "深色：文字比卡片再亮一档");
        // ② 不用纯黑（也顺带挡住"整块全 0"的复制粘贴事故）
        for (name, c) in [
            ("bg", d.bg),
            ("card", d.card),
            ("track_bg", d.track_bg),
            ("ctrl_bg", d.ctrl_bg),
            ("edit_bg", d.edit_bg),
            ("line", d.line),
        ] {
            assert!(
                c.r() + c.g() + c.b() > 0,
                "深色 {name} 不能是纯黑（{c:?}）：纯黑上画不出描边与分隔线"
            );
        }
        // ③ 对比度（(L1+0.05)/(L2+0.05)）
        let ratio = |a: Color32, b: Color32| {
            let (hi, lo) = if lum(a) >= lum(b) { (lum(a), lum(b)) } else { (lum(b), lum(a)) };
            (hi + 0.05) / (lo + 0.05)
        };
        assert!(
            ratio(d.text, d.card) >= 4.5,
            "深色正文在卡片上对比不足：{:.2}:1 < 4.5:1",
            ratio(d.text, d.card)
        );
        assert!(
            ratio(d.muted, d.card) >= 3.0,
            "深色次要文字在卡片上对比不足：{:.2}:1 < 3:1",
            ratio(d.muted, d.card)
        );
        // 浅色那份的对比同样要成立（回归护栏：老皮肤不能因为重构退化）
        assert!(ratio(l.text, l.card) >= 4.5, "浅色正文对比不足");
        assert!(ratio(l.muted, l.card) >= 3.0, "浅色次要文字对比不足");
    }

    /// **P40 守门测试（语义色相不跑偏）**：深浅两套里强调仍偏蓝、成功仍偏绿、
    /// 警告仍偏琥珀（红 > 绿 > 蓝）、错误仍偏红。换主题不许把"红色按钮"变成紫色。
    #[test]
    fn p40_semantic_hues_survive_both_themes() {
        for (label, p) in [("浅色", Palette::light()), ("深色", Palette::dark())] {
            let blue_dominant = |c: Color32| c.b() > c.r() && c.b() > c.g();
            assert!(
                blue_dominant(p.accent),
                "{label} 强调色应偏蓝：{:?}",
                p.accent
            );
            assert!(
                p.ok_fg.g() > p.ok_fg.r() && p.ok_fg.g() > p.ok_fg.b(),
                "{label} 成功色应偏绿"
            );
            assert!(
                p.warn_fg.r() > p.warn_fg.b() && p.warn_fg.g() > p.warn_fg.b(),
                "{label} 警告色应偏琥珀"
            );
            assert!(
                p.danger.r() > p.danger.g() && p.danger.r() > p.danger.b(),
                "{label} 危险色应偏红"
            );
        }
    }

    /// **P40 守门测试（浅色那份一个字节都没变）**。
    ///
    /// 深色模式是"加出来"的，不是"改出来"的：老用户升级后看到的浅色皮肤必须与重构前
    /// 完全一致。所以把 P37 定稿的那几个值在这里钉死（它们来自原 34 个常量）。
    #[test]
    fn p40_light_palette_is_byte_identical_to_the_old_constants() {
        let p = Palette::light();
        let cases: [(&str, Color32, [u8; 3]); 12] = [
            ("bg", p.bg, [0xF7, 0xF8, 0xFA]),
            ("card", p.card, [0xFF, 0xFF, 0xFF]),
            ("line", p.line, [0xE5, 0xE7, 0xEB]),
            ("text", p.text, [0x11, 0x18, 0x27]),
            ("muted", p.muted, [0x6B, 0x72, 0x80]),
            ("faint", p.faint, [0x9C, 0xA3, 0xAF]),
            ("accent", p.accent, [0x25, 0x63, 0xEB]),
            ("danger", p.danger, [0xDC, 0x26, 0x26]),
            ("track_bg", p.track_bg, [0xEE, 0xF0, 0xF3]),
            ("cursor", p.cursor, [0x1D, 0x4E, 0xD8]),
            ("keyframe", p.keyframe, [0xD9, 0xB1, 0x4A]),
            ("ctrl_bg", p.ctrl_bg, [0xF1, 0xF3, 0xF6]),
        ];
        for (name, got, want) in cases {
            assert_eq!(
                [got.r(), got.g(), got.b()],
                want,
                "浅色 {name} 与重构前的常量不一致"
            );
        }
    }

    /// `ThemeKind` ↔ `Config::dark_mode` 的往返：配置里的一个 bool 就是唯一真源，
    /// 且偏好被**钉死**（`System` 会让界面跟着操作系统跳，出现说不清的状态）。
    #[test]
    fn p40_theme_kind_round_trips_dark_mode() {
        assert!(!ThemeKind::from_dark_mode(false).is_dark());
        assert!(ThemeKind::from_dark_mode(true).is_dark());
        assert_eq!(
            ThemeKind::from_dark_mode(false).preference(),
            egui::ThemePreference::Light
        );
        assert_eq!(
            ThemeKind::from_dark_mode(true).preference(),
            egui::ThemePreference::Dark
        );
        for k in [ThemeKind::Light, ThemeKind::Dark] {
            assert_ne!(
                k.preference(),
                egui::ThemePreference::System,
                "主题偏好不允许跟随系统：{k:?}"
            );
        }
    }

    /// **P40 守门测试（`apply_kind` 的三件事）**：线程局部主题（`theme::*` 取色函数的
    /// 唯一依据）、egui 的全局主题偏好、以及两套样式都写（这样 egui 内部的
    /// `Theme::Light/Dark` 切换也不会把界面打回 egui 默认皮肤）。
    ///
    /// 线程局部是**每线程一份**：本测试只动自己这条线程，与其它并行测试互不影响。
    #[test]
    fn p40_apply_kind_sets_thread_local_and_pins_preference() {
        let ctx = egui::Context::default();
        apply_kind(&ctx, ThemeKind::Light);
        assert_eq!(kind(), ThemeKind::Light);
        assert!(!is_dark());
        assert_eq!(ctx.theme(), egui::Theme::Light);

        apply_kind(&ctx, ThemeKind::Dark);
        assert_eq!(kind(), ThemeKind::Dark);
        assert!(is_dark());
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        // 主题真换了：取色函数给出的是深色那一份
        assert_eq!(bg(), Palette::dark().bg);
        assert_ne!(bg(), Palette::light().bg);

        apply_kind(&ctx, ThemeKind::Light);
        assert_eq!(bg(), Palette::light().bg);
    }

    /// **P44 守门测试：标题栏的版本号，以及"版本号只有一个来源"。**
    ///
    /// 用户要求"在左上角 CapLite 处添加版本号，这版就定为 v1.0.0"，
    /// 所以从**常量层**钉死三件事：
    /// ① 标题 = `"{名} v{版本}"`（`v` 小写、一个空格、没有别的分隔符/后缀）；
    /// ② 版本号直接来自 `Cargo.toml`（`env!("CARGO_PKG_VERSION")`），不在代码里再写一份；
    /// ③ 版本号本身不带 `v` 前缀、是 `x.y.z` 的 semver —— 否则标题会变成 `CapLite vv1.0.0`。
    ///
    /// 为什么值得一条测试：窗口标题同时被**托盘认主窗口**（`tray::find_main_proc` 用
    /// `APP_NAME` 前缀匹配）和 Alt+Tab/任务栏使用，格式一变就可能悄悄坏掉一处。
    #[test]
    fn p44_window_title_carries_the_package_version() {
        assert_eq!(
            WINDOW_TITLE,
            format!("{APP_NAME} v{APP_VERSION}"),
            "标题必须拼成「名 + 空格 + v + 版本」"
        );
        assert_eq!(APP_VERSION, env!("CARGO_PKG_VERSION"), "版本号只能来自 Cargo.toml");
        assert!(WINDOW_TITLE.starts_with(APP_NAME), "托盘靠 APP_NAME 前缀认窗口");
        assert!(WINDOW_TITLE.ends_with(APP_VERSION));
        assert_eq!(WINDOW_TITLE.trim(), WINDOW_TITLE, "标题不许有前后空白");
        assert_eq!(WINDOW_TITLE.matches(' ').count(), 1, "名与版本之间只有一个空格");
        assert!(!APP_VERSION.starts_with('v'), "v 由 WINDOW_TITLE 拼，别写进版本号");
        let parts: Vec<&str> = APP_VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "版本号应是 x.y.z：{APP_VERSION}");
        assert!(
            parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "版本号每一段都得是数字：{APP_VERSION}"
        );
        // 这一版就是 v1.0.0（用户原话"这版就定为 v1.0.0"）；升级版本时改这里 + Cargo.toml
        assert_eq!(WINDOW_TITLE, "CapLite v1.0.0");
    }
}
