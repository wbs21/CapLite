//! 托盘图标（任务栏**通知区域**）+ 全局快捷键 —— **纯 Win32，不引入任何新 crate**。
//!
//! 组成（全部在**主线程**上，也就是 eframe 的事件循环线程）：
//!
//! 1. **message-only 窗口**（`CreateWindowExW(..., HWND_MESSAGE, ...)`）承接托盘回调
//!    （`WM_APP + 1`）与 `WM_HOTKEY`。为什么必须 message-only：它不进 `EnumWindows`、
//!    不占任务栏、不参与可见性，只做消息接收；建在**主线程**上才是关键 —— 见下面
//!    "消息怎么被泵到"。
//! 2. **图标**：`assets/caplite-256.rgba`（与运行时窗口图标同一份字节）→ 盒式
//!    （按 alpha 加权的）降采样到 32×32 → 32bpp **自上而下** DIB → `CreateIconIndirect`
//!    → `Shell_NotifyIconW(NIM_ADD)`。
//! 3. **菜单**（右键 / 左键点托盘图标弹出）：`CreatePopupMenu` + `AppendMenuW` +
//!    `TrackPopupMenu(TPM_RETURNCMD | TPM_NONOTIFY)`，三项：显示窗口 / 停止录制 / 退出。
//!    文案由界面侧用 i18n 推过来（[`set_menu_labels`]），所以菜单也跟着语言走。
//! 4. **全局快捷键**：三选一（[`HotkeyPreset`]）—— `Ctrl+Alt+R`（默认）/ `Ctrl+Shift+R` /
//!    自定义（[`HotkeyPreset::Custom`] + 配置里的文本）。**有效组合只有一个算法**
//!    （[`effective_hotkey`]），别处不许再写死组合。同一个键**切换**开始/停止录制。
//!
//! **消息怎么被泵到**：eframe/winit 在 Windows 上用
//! `PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE)` 取**本线程所有窗口**的消息再
//! `DispatchMessageW` 分发（winit 0.30 `event_loop.rs::dispatch_peeked_messages`），
//! 所以窗口建在主线程就够 —— **不需要、也不该**自己起第二个消息循环
//!（自己起循环会与 winit 抢消息，这才是"托盘 + eframe 冲突"的真正来源）。
//!
//! **唤醒界面**：`WM_HOTKEY` 与菜单回调发生在 egui 帧之外，只把命令塞进队列界面不会知道，
//! 所以每次入队都顺手调一次 `egui::Context::request_repaint`（句柄由 [`Tray::install`] 存下）。
//!
//! **必须同时被 `App::ui` 与 `App::logic` 驱动**：窗口隐藏（`ViewportCommand::Visible(false)`）
//! 之后 eframe **不再调 `App::ui`**，只调 `App::logic`（eframe 的 `update_logic_only`）。
//! 命令队列与"谁隐藏谁恢复"的判定因此放在一个两条路径都会走的函数里（见 `main.rs`）。
//!
//! Win11 默认把新出现的通知区域图标收进**溢出菜单**（`^` 那个小箭头里）：
//! 这是系统行为，**是否在溢出区由系统决定、程序不能强制**（也不能假设它一定在溢出区）；
//! 用户可以在"任务栏设置 → 其他系统托盘图标"里把它固定到任务栏。
//!
//! 所有 Win32 失败只返回 `Err(..)` 或记 warning，**绝不 panic、绝不影响录制**。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;

use eframe::egui;
use serde::{Deserialize, Serialize};
use windows::core::{w, PCWSTR, BOOL};use windows::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ, RGBQUAD,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, EnumWindows, GetClassNameW, GetCursorPos, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, ShowWindow, TrackPopupMenu, HICON, ICONINFO,
    MF_SEPARATOR, MF_STRING, SW_RESTORE,
    SW_HIDE, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_HOTKEY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

// ---------------------------------------------------------------------------
// 虚拟键码（只列我们用得到的；不引 `VIRTUAL_KEY` 常量表，避免多一层类型转换）
// ---------------------------------------------------------------------------

const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_PAUSE: u32 = 0x13;
const VK_CAPITAL: u32 = 0x14;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;
const VK_PRIOR: u32 = 0x21; // PageUp
const VK_NEXT: u32 = 0x22; // PageDown
const VK_END: u32 = 0x23;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_INSERT: u32 = 0x2D;
const VK_DELETE: u32 = 0x2E;
const VK_L: u32 = 0x4C;
const VK_F1: u32 = 0x70;
const VK_F24: u32 = VK_F1 + 23;
const VK_NUMLOCK: u32 = 0x90;
const VK_SCROLL: u32 = 0x91;

/// `RegisterHotKey` 的"该组合已被占用"错误码（`ERROR_HOTKEY_ALREADY_REGISTERED`）。
pub const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

/// 托盘回调消息：`WM_APP + 1`（`WM_APP` 起是给应用自己用的，不会被系统占用）。
pub(crate) const WM_APP_TRAY: u32 = WM_APP + 1;

/// 托盘图标用多少像素：32×32（`SM_CXSMICON` 在 100% DPI 下是 16，
/// 但外壳会按 DPI 缩放，给 32 在高 DPI 下更清楚；给 256 反而会被系统缩得很糊）。
pub(crate) const TRAY_ICON_PX: u32 = 32;

/// `RegisterHotKey` 的 id（同一个窗口上可以有多个热键，靠它区分）。
pub(crate) const HOTKEY_ID: i32 = 0x0CA1;
/// "候选探测"用的另一个 id：探测完立刻注销，**不碰** [`HOTKEY_ID`]。
const HOTKEY_PROBE_ID: i32 = 0x0CA2;

/// 托盘图标 id（同一个窗口可以挂多个图标）。
const TRAY_ICON_ID: u32 = 1;

/// Explorer 重启后会广播 `TaskbarCreated`。收不到它，托盘图标就永久消失，
/// 而 `icon_ok` 还以为是好的 —— 用户把窗口藏起来后可能再也叫不回来。
static TASKBAR_CREATED_MSG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 当前托盘图标是否真的在通知区域（Explorer 重启/`NIM_ADD` 失败会置 false）。
static TRAY_ICON_PRESENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 重新 `NIM_ADD` 需要原 HICON；只存裸值，进程内使用。
static TRAY_HICON_RAW: AtomicIsize = AtomicIsize::new(0);
/// 当前托盘 tooltip（Explorer 重启后 `NIM_ADD` 要用；录制中会带时长）。
static TRAY_TOOLTIP: Mutex<Option<String>> = Mutex::new(None);
/// 当前**真的注册着**的全局热键；`None` = 没注册。
///
/// WM_HOTKEY 的 lParam 校验靠它：跨进程 PostMessage 来的 lParam 里 low/high word
/// 通常是垃圾值，用 `RegisterHotKey` 发过来的才会被系统填成真实的 vk + modifiers。
static TRAY_CURRENT_HOTKEY: Mutex<Option<Hotkey>> = Mutex::new(None);

/// 菜单命令 id（`TPM_RETURNCMD` 直接把选中的 id 返回来，不必走 `WM_COMMAND`）。
const CMD_SHOW: u32 = 1;
const CMD_STOP: u32 = 2;
const CMD_QUIT: u32 = 3;

/// 托盘菜单/快捷键投递过来的命令（由界面侧 [`take_commands`] 取走）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    /// 「显示窗口」：`Visible(true)` + 置前/聚焦
    ShowWindow,
    /// 「停止录制」：与界面「停止」**同一条**路径
    StopRecording,
    /// 全局快捷键：**同一个键切换**（未录制 → 开始；录制中 → 停止）
    ToggleRecording,
    /// 「退出」：先停录制/收尾，再关窗
    Quit,
    /// 系统事件（锁屏/挂起/显示模式变化）：停录并导出，原因显示给用户。
    SystemStop(String),
}

// ---------------------------------------------------------------------------
// 全局快捷键：组合、预设、解析、有效组合
// ---------------------------------------------------------------------------

/// 一个具体的全局快捷键组合（修饰键 + 一个非修饰键）。
///
/// 相等性包含 `MOD_NOREPEAT`（[`parse_hotkey`] 与三个预设都带它），所以
/// "配置里的组合 == 已经注册的组合"这个判断不需要额外的归一化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    /// `MOD_CONTROL | MOD_ALT | ... | MOD_NOREPEAT`
    pub modifiers: HOT_KEY_MODIFIERS,
    /// 虚拟键码（`'R'` = 0x52，`F9` = 0x78……）
    pub vk: u32,
}

impl Hotkey {
    pub const fn new(modifiers: HOT_KEY_MODIFIERS, vk: u32) -> Self {
        Self { modifiers, vk }
    }

    /// 规范化的人可读名字（`Ctrl+Alt+R`）—— 界面、托盘提示、文档、日志都用它。
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if self.modifiers.0 & MOD_CONTROL.0 != 0 {
            parts.push("Ctrl");
        }
        if self.modifiers.0 & MOD_ALT.0 != 0 {
            parts.push("Alt");
        }
        if self.modifiers.0 & MOD_SHIFT.0 != 0 {
            parts.push("Shift");
        }
        if self.modifiers.0 & MOD_WIN.0 != 0 {
            parts.push("Win");
        }
        let head = parts.join("+");
        let key = vk_name(self.vk);
        if head.is_empty() {
            key
        } else {
            format!("{head}+{key}")
        }
    }

    /// Windows 保留 / 外壳占用的组合（注册必然失败，或者抢了系统的键）。
    ///
    /// 只列**确定**不能用的那几个：不确定的一律放行 —— 让 `RegisterHotKey` 去失败，
    /// 我们已经有"失败 → 回滚 → 推荐候选"这条完整路径，不需要在这里猜。
    pub fn is_reserved(&self) -> bool {
        let m = self.modifiers.0 & !MOD_NOREPEAT.0;
        let ctrl = MOD_CONTROL.0;
        let alt = MOD_ALT.0;
        let shift = MOD_SHIFT.0;
        let win = MOD_WIN.0;
        match (m, self.vk) {
            // 安全注意序列（SAS）：任何程序都无法注册
            (mm, VK_DELETE) if mm == (ctrl | alt) => true,
            // 任务管理器 / 切换窗口 / 开始菜单 / 锁定工作站 / 任务视图
            (mm, VK_ESCAPE) if mm == (ctrl | shift) => true,
            (mm, VK_ESCAPE) if mm == alt => true,
            (mm, VK_ESCAPE) if mm == ctrl => true,
            (mm, VK_TAB) if mm == alt => true,
            (mm, VK_TAB) if mm == win => true,
            (mm, VK_L) if mm == win => true,
            _ => false,
        }
    }
}

/// 虚拟键码 → 规范化名字（与 [`Hotkey::label`] 同一个口径，可反解回键码）。
pub(crate) fn vk_name(vk: u32) -> String {
    if let Some((name, _)) = NAMED_KEYS.iter().find(|(_, code)| *code == vk) {
        return (*name).to_string();
    }
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        if let Some(c) = char::from_u32(vk) {
            return c.to_string();
        }
    }
    if (VK_F1..=VK_F24).contains(&vk) {
        return format!("F{}", vk - VK_F1 + 1);
    }
    format!("0x{vk:02X}")
}

/// 规范化名字 → 虚拟键码（小写比较）。
const NAMED_KEYS: &[(&str, u32)] = &[
    ("Space", VK_SPACE),
    ("Tab", VK_TAB),
    ("Enter", VK_RETURN),
    ("Esc", VK_ESCAPE),
    ("Backspace", VK_BACK),
    ("Insert", VK_INSERT),
    ("Delete", VK_DELETE),
    ("Home", VK_HOME),
    ("End", VK_END),
    ("PageUp", VK_PRIOR),
    ("PageDown", VK_NEXT),
    ("Up", VK_UP),
    ("Down", VK_DOWN),
    ("Left", VK_LEFT),
    ("Right", VK_RIGHT),
    ("Pause", VK_PAUSE),
    ("CapsLock", VK_CAPITAL),
    ("NumLock", VK_NUMLOCK),
    ("ScrollLock", VK_SCROLL),
];

/// 键名**别名**（只用于解析；`vk_name` 只吐规范名，所以别名不会破坏往返稳定性）。
const KEY_ALIASES: &[(&str, u32)] = &[
    ("del", VK_DELETE),
    ("ins", VK_INSERT),
    ("esc", VK_ESCAPE),
    ("escape", VK_ESCAPE),
    ("return", VK_RETURN),
    ("pgup", VK_PRIOR),
    ("pgdn", VK_NEXT),
    ("pageup", VK_PRIOR),
    ("pagedown", VK_NEXT),
    ("spacebar", VK_SPACE),
];

/// 自定义快捷键文本解析失败的原因（**带结构**，好让界面用 i18n 组文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyParseError {
    /// 一个修饰键都没有（`R`、`F9` 这种裸键不许当全局热键）
    NoModifier,
    /// 只有修饰键，没有非修饰键（`Ctrl+Alt`）
    NoKey,
    /// Windows 保留组合
    Reserved(Hotkey),
    /// 认不出的键名（附带原样 token，便于提示用户）
    Malformed(String),
}

/// 解析自定义快捷键文本（`"Ctrl+Shift+F9"`，大小写与空格容错）。
///
/// 规则（也就是"校验"的全部内容）：
/// * 至少**一个修饰键**：`Ctrl`/`Control`、`Alt`、`Shift`、`Win`/`Windows`/`Meta`/`Super`；
/// * **恰好一个**非修饰键：`A`–`Z`、`0`–`9`、`F1`–`F24`，或 `NAMED_KEYS` 里的名字；
/// * 拒绝 [`Hotkey::is_reserved`] 里的系统保留组合；
/// * 成功时统一补上 `MOD_NOREPEAT`（按住不放只触发一次）。
pub fn parse_hotkey(text: &str) -> Result<Hotkey, HotkeyParseError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(HotkeyParseError::NoKey);
    }
    let mut mods = 0u32;
    let mut vk: Option<u32> = None;
    for raw in trimmed.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            return Err(HotkeyParseError::Malformed(text.trim().to_string()));
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL.0,
            "alt" => mods |= MOD_ALT.0,
            "shift" => mods |= MOD_SHIFT.0,
            "win" | "windows" | "meta" | "super" => mods |= MOD_WIN.0,
            other => {
                if vk.is_some() {
                    return Err(HotkeyParseError::Malformed(token.to_string()));
                }
                match parse_vk(other) {
                    Some(code) => vk = Some(code),
                    None => return Err(HotkeyParseError::Malformed(token.to_string())),
                }
            }
        }
    }
    let Some(vk) = vk else {
        return Err(HotkeyParseError::NoKey);
    };
    if mods == 0 {
        return Err(HotkeyParseError::NoModifier);
    }
    let hk = Hotkey::new(HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0), vk);
    if hk.is_reserved() {
        return Err(HotkeyParseError::Reserved(hk));
    }
    Ok(hk)
}

/// 单个非修饰键 token（已小写）→ 虚拟键码。
fn parse_vk(token: &str) -> Option<u32> {
    if let Some((_, code)) = NAMED_KEYS.iter().find(|(name, _)| name.to_ascii_lowercase() == token) {
        return Some(*code);
    }
    if let Some((_, code)) = KEY_ALIASES.iter().find(|(name, _)| *name == token) {
        return Some(*code);
    }
    let bytes = token.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0].to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(u32::from(c));
        }
        return None;
    }
    if let Some(num) = token.strip_prefix('f') {
        if let Ok(n) = num.parse::<u32>() {
            if (1..=(VK_F24 - VK_F1 + 1)).contains(&n) {
                return Some(VK_F1 + n - 1);
            }
        }
    }
    None
}

/// 设置页里的**三选一**（序列化名就是配置里的字符串，见 `#[serde(rename_all)]`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyPreset {
    /// `Ctrl+Alt+R`（**默认**）
    #[default]
    CtrlAltR,
    /// `Ctrl+Shift+R`
    CtrlShiftR,
    /// 自定义（组合来自配置里的 `hotkey_custom` 文本）
    Custom,
}

/// `Ctrl+Alt+R`：**默认预设**（配置默认值、非法自定义的回退值都指它）。
pub const PRESET_CTRL_ALT_R: Hotkey = Hotkey::new(
    HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_ALT.0 | MOD_NOREPEAT.0),
    b'R' as u32,
);

/// `Ctrl+Shift+R`。
pub const PRESET_CTRL_SHIFT_R: Hotkey = Hotkey::new(
    HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0),
    b'R' as u32,
);

/// 默认/回退组合 = [`PRESET_CTRL_ALT_R`]。
pub const DEFAULT_HOTKEY: Hotkey = PRESET_CTRL_ALT_R;

/// 预设 → 组合（[`HotkeyPreset::Custom`] 返回 `None`：自定义要看配置文本）。
pub const fn preset_hotkey(preset: HotkeyPreset) -> Option<Hotkey> {
    match preset {
        HotkeyPreset::CtrlAltR => Some(PRESET_CTRL_ALT_R),
        HotkeyPreset::CtrlShiftR => Some(PRESET_CTRL_SHIFT_R),
        HotkeyPreset::Custom => None,
    }
}

/// **有效快捷键的单一来源**：预设就取预设组合，自定义就解析配置文本。
///
/// 自定义解析不了时**回退到 [`DEFAULT_HOTKEY`]** 而不是"没有热键"：
/// 手改配置写坏了不该让用户失去热键（设置页保存前会先校验，所以正常路径走不到这里）。
/// 全程序只有这一个函数决定"到底按哪个组合注册"，别处不许再写死组合。
pub fn effective_hotkey(preset: HotkeyPreset, custom: &str) -> Hotkey {
    match preset_hotkey(preset) {
        Some(hk) => hk,
        None => parse_hotkey(custom).unwrap_or(DEFAULT_HOTKEY),
    }
}

/// 注册失败时用来"推荐一个能用组合"的候选（按顺序试，第一个能注册成功的就推荐）。
///
/// 选这四个的理由：都是"左手能按、不与常见软件冲突"的组合，且**覆盖两个预设**
/// （万一冲突只是临时的，用户也能一键换回来）。
pub(crate) const HOTKEY_CANDIDATES: [&str; 4] =
    ["Ctrl+Alt+R", "Ctrl+Shift+R", "Ctrl+Alt+F9", "Ctrl+Shift+F9"];

/// 结构化的注册失败（界面用 i18n 组文案，日志直接用 [`Self::describe`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyRegisterError {
    /// 想注册但没成功的组合
    pub hotkey: Hotkey,
    /// `GetLastError()`（1409 = `ERROR_HOTKEY_ALREADY_REGISTERED`）
    pub code: u32,
    /// 系统给的错误文本
    pub system: String,
    /// 候选探测的结果：**第一个真的能注册成功的**组合（探测完立刻注销，不留痕迹）
    pub suggestion: Option<Hotkey>,
}

impl HotkeyRegisterError {
    /// 是不是"已被占用"（1409）。
    pub fn already_registered(&self) -> bool {
        self.code == ERROR_HOTKEY_ALREADY_REGISTERED
    }

    /// 语言中立的纯文本（给 `cap.log` 用；界面文案走 i18n）。
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{} 注册失败：GetLastError={}（{}）",
            self.hotkey.label(),
            self.code,
            self.system
        );
        if self.already_registered() {
            s.push_str("，ERROR_HOTKEY_ALREADY_REGISTERED（已被其它程序占用）");
        }
        match &self.suggestion {
            Some(hk) => s.push_str(&format!("；候选探测建议改用 {}", hk.label())),
            None => s.push_str("；候选组合也都不可用"),
        }
        s
    }
}

// ---------------------------------------------------------------------------
// "谁隐藏谁恢复"（纯逻辑，脱 Win32，便于单测）
// ---------------------------------------------------------------------------

/// 本帧对窗口可见性要做的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityAction {
    /// 什么都不做（**开关关闭时不发任何 viewport 命令**也走这里）
    Nothing,
    /// 隐藏到托盘（`ViewportCommand::Visible(false)`）
    Hide,
    /// 恢复显示（`ViewportCommand::Visible(true)`）
    Show,
}

/// "**谁隐藏谁恢复**"的状态机。
///
/// 规则（硬要求）：
/// * 录制**真正开始**（`Recorder::start` 成功）且配置开关为真 → 隐藏，并记下"是我们隐藏的"；
/// * 录制**停止 / 失败 / 导出完成**后 → **仅当"是我们隐藏的"**才恢复 `Visible(true)`，
///   绝不覆盖用户自己隐藏窗口的意图；
/// * 开关**关闭**时一个 viewport 命令都不发（也不会把标志置起来）。
///
/// 做成"每帧问一次"的 `poll`，是因为三条入口（界面按钮 / 托盘菜单 / 全局快捷键）
/// 都会改变 `recorder`，而隐藏/恢复只该有一个判据 —— 每帧对一次账最不容易漏。
#[derive(Debug, Default, Clone, Copy)]
pub struct HideState {
    /// 窗口是**我们**（而不是用户自己）隐藏的
    hid_by_us: bool,
}

impl HideState {
    /// 是不是"我们隐藏的"（外壳据此判断录制结束时该不该恢复可见）。
    pub fn hidden_by_us(&self) -> bool {
        self.hid_by_us
    }

    /// 本帧的对账：`recording` = 现在是否在录制，`hide_to_tray` = 配置开关。
    ///
    /// 幂等：状态没变时一律返回 [`VisibilityAction::Nothing`]（不会每帧重复发命令）。
    pub fn poll(&mut self, recording: bool, hide_to_tray: bool) -> VisibilityAction {
        if recording {
            if hide_to_tray && !self.hid_by_us {
                self.hid_by_us = true;
                return VisibilityAction::Hide;
            }
            VisibilityAction::Nothing
        } else if self.hid_by_us {
            self.hid_by_us = false;
            VisibilityAction::Show
        } else {
            VisibilityAction::Nothing
        }
    }
}

// ---------------------------------------------------------------------------
// 图标像素处理（纯逻辑，便于单测）
// ---------------------------------------------------------------------------

/// RGBA → **BGRA**（`CreateDIBSection` 的 32bpp 位图按 B,G,R,A 存放）。
pub(crate) fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

/// 盒式降采样（`src_side` → `dst_side`，RGBA 进 / RGBA 出）。
///
/// RGB **按 alpha 加权**再除回来（等价于"先预乘、平均、再除回去"）：
/// 直接对非预乘的 RGB 求平均会让透明像素的颜色渗进边缘（图标边缘发灰/发黑）。
pub(crate) fn downscale_rgba(src: &[u8], src_side: u32, dst_side: u32) -> Vec<u8> {
    let (sw, dw) = (src_side as usize, dst_side.max(1) as usize);
    if sw == 0 || src.len() < sw * sw * 4 {
        return Vec::new();
    }
    let mut out = vec![0u8; dw * dw * 4];
    for dy in 0..dw {
        let y0 = dy * sw / dw;
        let y1 = ((dy + 1) * sw / dw).max(y0 + 1).min(sw);
        for dx in 0..dw {
            let x0 = dx * sw / dw;
            let x1 = ((dx + 1) * sw / dw).max(x0 + 1).min(sw);
            let (mut a_sum, mut r_sum, mut g_sum, mut b_sum, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * sw + x) * 4;
                    let a = u64::from(src[i + 3]);
                    a_sum += a;
                    r_sum += u64::from(src[i]) * a;
                    g_sum += u64::from(src[i + 1]) * a;
                    b_sum += u64::from(src[i + 2]) * a;
                    n += 1;
                }
            }
            let o = (dy * dw + dx) * 4;
            if n == 0 || a_sum == 0 {
                continue; // 全透明：保持 0,0,0,0
            }
            out[o] = (r_sum / a_sum) as u8;
            out[o + 1] = (g_sum / a_sum) as u8;
            out[o + 2] = (b_sum / a_sum) as u8;
            out[o + 3] = (a_sum / n) as u8;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 消息窗口 / 菜单文案 / 命令队列（全局，因为 WndProc 是自由函数）
// ---------------------------------------------------------------------------

/// 界面侧的 `egui::Context`（`request_repaint` 用）。
///
/// 为什么要有全局：托盘回调发生在 egui 帧之外，那时手上只有 `hwnd`。
/// `Context` 是 `Send + Sync` 的句柄，克隆很便宜。
static REPAINT_CTX: Mutex<Option<egui::Context>> = Mutex::new(None);

/// 托盘菜单 / 全局快捷键投递过来的命令（WndProc 入队，界面帧出队）。
static COMMANDS: Mutex<VecDeque<TrayCommand>> = Mutex::new(VecDeque::new());

/// 菜单三项的文案（UTF-16、**已带结尾 0**）。默认英文，界面起来后按语言覆盖。
struct MenuTexts {
    show: Vec<u16>,
    stop: Vec<u16>,
    quit: Vec<u16>,
}

static MENU_LABELS: Mutex<MenuTexts> = Mutex::new(MenuTexts {
    show: Vec::new(),
    stop: Vec::new(),
    quit: Vec::new(),
});

/// 入队一个命令并唤醒界面帧。
pub(crate) fn push_command(cmd: TrayCommand) {
    if let Ok(mut q) = COMMANDS.lock() {
        q.push_back(cmd);
    } else {
        return; // 队列中毒（几乎不可能）：命令丢掉，但绝不 panic
    }
    request_repaint();
}

/// 给独立会话通知窗口使用：即使托盘窗口没建起来，也能唤醒界面处理 SystemStop。
pub(crate) fn set_repaint_context(ctx: &egui::Context) {
    if let Ok(mut g) = REPAINT_CTX.lock() {
        *g = Some(ctx.clone());
    }
}

/// 让 egui 立刻跑一帧（否则命令会一直躺在队列里）。
fn request_repaint() {
    if let Ok(guard) = REPAINT_CTX.lock() {
        if let Some(ctx) = guard.as_ref() {
            ctx.request_repaint();
        }
    }
}

/// 取走本帧到达的命令（界面帧调用；没装托盘时永远是空）。
pub fn take_commands() -> Vec<TrayCommand> {
    match COMMANDS.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

/// 更新托盘菜单文案（三项，界面侧按当前语言推过来）。
///
/// 为什么不让 WndProc 自己取 i18n：托盘菜单在 Win32 的消息回调里弹出，
/// 那条路径拿不到 `I18n`（也不该为了菜单把语言状态搬进全局）。所以由界面帧推送。
pub fn set_menu_labels(labels: [&str; 3]) {
    let Ok(mut g) = MENU_LABELS.lock() else { return };
    g.show = to_wide(labels[0]);
    g.stop = to_wide(labels[1]);
    g.quit = to_wide(labels[2]);
}

/// UTF-16 + 结尾 0（Win32 的 `LPCWSTR` 口径）。
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 弹出托盘菜单（**从 WndProc 里调**，不走 egui 帧 —— 见模块注释里的重入说明）。
///
/// `TPM_RETURNCMD | TPM_NONOTIFY`：选中的命令 id 直接由返回值给出，不发 `WM_COMMAND`。
/// `SetForegroundWindow(hwnd)` + 结束后 `PostMessage(WM_NULL)` 是 MSDN 的标准配套写法：
/// 不做的话菜单点外面不会消失（"菜单卡住"的经典症状）。
unsafe fn popup_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    // 菜单文案：拿不到锁就退回英文默认（Win32 对空串合法，但条目没字会很怪）
    let (show, stop, quit) = match MENU_LABELS.lock() {
        Ok(g) if !g.show.is_empty() && !g.stop.is_empty() && !g.quit.is_empty() => {
            (g.show.clone(), g.stop.clone(), g.quit.clone())
        }
        _ => (
            to_wide("Show window"),
            to_wide("Stop recording"),
            to_wide("Exit"),
        ),
    };
    let _ = AppendMenuW(menu, MF_STRING, CMD_SHOW as usize, PCWSTR(show.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, CMD_STOP as usize, PCWSTR(stop.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, PCWSTR(quit.as_ptr()));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    let _ = SetForegroundWindow(hwnd);
    let picked = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    // 必须回一次空消息，否则菜单在"点别处"时不会正常消失
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match picked.0 as u32 {
        CMD_SHOW => push_command(TrayCommand::ShowWindow),
        CMD_STOP => push_command(TrayCommand::StopRecording),
        CMD_QUIT => push_command(TrayCommand::Quit),
        _ => {}
    }
}

/// message-only 窗口的窗口过程：托盘回调 + 全局热键。
unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_APP_TRAY => {
            // 低字 = 鼠标消息，高字 = 图标 id
            match (lparam.0 as u32) & 0xFFFF {
                // 右键**和**左键都弹菜单（左键也弹是刻意的：托盘图标没有别的主操作，
                // 而"停止录制"必须是一个随时够得着的入口）
                WM_RBUTTONUP | WM_LBUTTONUP => popup_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            // wParam 是热键 id；lParam 低字 = vk，高字 = MOD_* 修饰键状态
            // （跨进程 PostMessage 伪造的 WM_HOTKEY 的 lParam 通常是垃圾值，
            // 系统 RegisterHotKey 发过来的才会被正确填充）。
            let l_param = lparam.0 as u32;
            let low_vk = l_param & 0xFFFF;
            let hi_mods = (l_param >> 16) & 0xFFFF;

            if wparam.0 as i32 == HOTKEY_ID {
                let ok = TRAY_CURRENT_HOTKEY
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .map(|hk| {
                        let expected_mods = hk.modifiers.0 & !MOD_NOREPEAT.0;
                        hk.vk == low_vk && expected_mods == hi_mods
                    })
                    .unwrap_or(false);
                if ok {
                    push_command(TrayCommand::ToggleRecording);
                } else {
                    tracing::debug!(
                        "忽略 WM_HOTKEY 伪造: id={}, lParam={} (low_vk={}, hi_mods={})",
                        wparam.0 as i32,
                        l_param,
                        low_vk,
                        hi_mods
                    );
                }
            }
            LRESULT(0)
        }
        // Explorer 重启后重新挂图标（否则图标永久消失，`icon_ok` 却还是 true）。
        m if m != 0 && m == TASKBAR_CREATED_MSG.load(Ordering::SeqCst) => {
            let raw = TRAY_HICON_RAW.load(Ordering::SeqCst);
            if raw != 0 {
                let hicon = HICON(raw as *mut core::ffi::c_void);
                let tip = TRAY_TOOLTIP
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_else(|| crate::ui::theme::APP_NAME.to_string());
                if add_icon(hwnd, hicon, &tip) {
                    TRAY_ICON_PRESENT.store(true, Ordering::SeqCst);
                    tracing::info!("Explorer 重启后已重新挂上托盘图标");
                } else {
                    TRAY_ICON_PRESENT.store(false, Ordering::SeqCst);
                    tracing::warn!("Explorer 重启后重新挂托盘图标失败");
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// 全局热键注册状态机（**不依赖托盘图标**，单测能直接走真路径）
// ---------------------------------------------------------------------------

/// 全局热键的注册状态机。
///
/// 拆出来只有两个目的：
/// 1. 单测可以**不碰通知区域**（不装托盘图标）就走完整的"注册 → 失败 → 回滚 → 候选探测"路径；
/// 2. `Tray` 只剩"图标 + 菜单 + 窗口"三件事，职责清楚。
///
/// **换键的顺序**（需求明确要求）：先 `UnregisterHotKey` 旧的 → 再注册新的 →
/// 新的失败就**回滚到旧热键**（旧组合仍然可用）→ 再跑一次候选探测给出建议。
pub(crate) struct HotkeyRegistrar {
    hwnd: HWND,
    current: Option<Hotkey>,
    registered: bool,
    error: Option<HotkeyRegisterError>,
}

impl HotkeyRegistrar {
    /// 绑到一个窗口句柄（托盘用的是 message-only 窗口）。
    pub fn attach(hwnd: HWND) -> Self {
        Self {
            hwnd,
            current: None,
            registered: false,
            error: None,
        }
    }

    /// 当前**真的注册着**的组合（`None` = 没注册）。
    pub fn current(&self) -> Option<Hotkey> {
        self.current
    }

    pub fn is_registered(&self) -> bool {
        self.registered
    }

    /// 最近一次失败的详情（成功时清空）。
    pub fn error(&self) -> Option<&HotkeyRegisterError> {
        self.error.as_ref()
    }

    /// 注销当前热键（没有注册时是空操作）。
    fn unregister(&mut self) {
        if self.registered {
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), HOTKEY_ID) };
        }
        self.registered = false;
        self.current = None;
        if let Ok(mut g) = TRAY_CURRENT_HOTKEY.lock() {
            *g = None;
        }
    }

    /// 单纯地试注册一个组合（成功才认账；失败把 `GetLastError` 留下来）。
    fn try_register(&mut self, hk: Hotkey) -> bool {
        match unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ID, hk.modifiers, hk.vk) } {
            Ok(()) => {
                self.registered = true;
                self.current = Some(hk);
                self.error = None;
                if let Ok(mut g) = TRAY_CURRENT_HOTKEY.lock() {
                    *g = Some(hk);
                }
                true
            }
            Err(e) => {
                // **先取错误码**：`e.to_string()` 内部可能再调 Win32 把 last-error 冲掉
                let code = unsafe { GetLastError() }.0;
                let system = e.to_string();
                self.registered = false;
                self.current = None;
                if let Ok(mut g) = TRAY_CURRENT_HOTKEY.lock() {
                    *g = None;
                }
                self.error = Some(HotkeyRegisterError {
                    hotkey: hk,
                    code,
                    system,
                    suggestion: None,
                });
                false
            }
        }
    }

    /// 候选探测：按 [`HOTKEY_CANDIDATES`] 顺序，返回**第一个真的能注册成功**的组合。
    ///
    /// 探测完**立刻注销**（用独立的 [`HOTKEY_PROBE_ID`]，不碰用户的热键）；
    /// 已经是我们自己注册着的组合直接跳过（自己跟自己抢会误报"不可用"）。
    fn probe_candidates(&self, avoid: &[Option<Hotkey>]) -> Option<Hotkey> {
        for text in HOTKEY_CANDIDATES {
            let Ok(cand) = parse_hotkey(text) else { continue };
            if avoid.contains(&Some(cand)) {
                continue;
            }
            if unsafe {
                RegisterHotKey(
                    Some(self.hwnd),
                    HOTKEY_PROBE_ID,
                    cand.modifiers,
                    cand.vk,
                )
            }
            .is_ok()
            {
                let _ = unsafe { UnregisterHotKey(Some(self.hwnd), HOTKEY_PROBE_ID) };
                return Some(cand);
            }
        }
        None
    }

    /// 把全局热键换成 `want`（`None` = 关掉热键）。
    ///
    /// `fallback`：新组合注册失败时**回滚**到它（通常是"换之前那个还注册着的组合"）。
    /// 传 `None` 表示不回滚。
    ///
    /// 成功返回 `Ok(())`；失败返回结构化错误，**注册状态已经回到 `fallback`**
    ///（也就是说失败不会让用户连旧热键都没了）。
    pub fn set_hotkey(
        &mut self,
        want: Option<Hotkey>,
        fallback: Option<Hotkey>,
    ) -> Result<(), HotkeyRegisterError> {
        let Some(want) = want else {
            self.unregister();
            self.error = None;
            return Ok(());
        };
        if self.registered && self.current == Some(want) {
            return Ok(()); // 已经是它了，不折腾
        }

        // ① 旧的先注销（同一个组合换 id 注册会互相打架，先让位最干净）
        self.unregister();
        // ② 试新的
        if self.try_register(want) {
            return Ok(());
        }
        let failure = self.error.clone().unwrap_or(HotkeyRegisterError {
            hotkey: want,
            code: 0,
            system: "未知原因".to_string(),
            suggestion: None,
        });

        // ③ 回滚到 `fallback`（"保持/回滚到旧热键仍然可用"）
        if let Some(prev) = fallback {
            if prev != want && self.try_register(prev) {
                // 回滚成功：注册状态是 prev，但错误详情仍然是刚才那次失败
                self.error = Some(failure.clone());
            }
        }

        // ④ 候选探测：给出一个**真的能用**的替代组合
        let mut avoid = vec![Some(want), self.current];
        avoid.push(fallback);
        let suggestion = self.probe_candidates(&avoid);
        let mut err = failure;
        err.suggestion = suggestion;
        self.error = Some(err.clone());
        Err(err)
    }

    /// 退出/丢弃：注销热键（幂等）。
    pub fn shutdown(&mut self) {
        self.unregister();
    }
}

// ---------------------------------------------------------------------------
// Tray：安装 / 卸载
// ---------------------------------------------------------------------------

/// 托盘图标 + 全局快捷键的持有者（**Drop 即注销**：`NIM_DELETE` + `UnregisterHotKey`）。
pub struct Tray {
    hwnd: HWND,
    hicon: HICON,
    /// 录制中使用的红点图标（创建失败时退回 `hicon`）。
    recording_icon: HICON,
    recording: bool,
    /// 全局热键的注册状态机（见 [`HotkeyRegistrar`]）
    hotkeys: HotkeyRegistrar,
    /// 是否还挂在通知区域（Drop 幂等用）
    installed: bool,
    /// `NIM_ADD` 失败的原因（只进 `cap.log`）
    icon_error: Option<String>,
}

impl Tray {
    /// 在**主线程**上安装托盘图标与全局热键。
    ///
    /// `ctx` 用来在托盘回调里唤醒界面帧；`labels` 是菜单三项的初始文案；
    /// `initial_hotkey` = 启动时要注册的组合（`None` = 用户把全局快捷键关了）。
    ///
    /// 降级分级（硬要求，越靠后越轻）：
    /// * **窗口建不出来**（`CreateWindowExW` 失败）→ 返回 `Err(String)`：托盘与热键都用不了，
    ///   调用方只记一条 warning，**录制等其它功能照常**；
    /// * **图标挂不上**（`CreateIconIndirect` / `Shell_NotifyIconW` 失败）→ **不返回 Err**：
    ///   消息窗口留着、全局热键照常注册，`icon_ok()` 为 false。调用方据此**不隐藏窗口**
    ///   （否则窗口藏起来了却没有图标能叫回来）；
    /// * **热键注册失败**（被占用）→ 同样不返回 Err，详情在 [`Self::hotkey_error`]。
    pub fn install(
        ctx: &egui::Context,
        icon_rgba: &[u8],
        icon_px: u32,
        tip: &str,
        labels: [&str; 3],
        initial_hotkey: Option<Hotkey>,
    ) -> Result<Self, String> {
        if let Ok(mut g) = REPAINT_CTX.lock() {
            *g = Some(ctx.clone());
        }
        set_menu_labels(labels);

        let hwnd = create_message_window()?;
        // 注册 Explorer 的 `TaskbarCreated` 广播（同一字符串在所有进程里是同一个消息 id）。
        // SAFETY: 静态 UTF-16 字面量，调用只读它。
        let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::SeqCst);

        let mut tray = Self {
            hwnd,
            hicon: HICON(std::ptr::null_mut()),
            recording_icon: HICON(std::ptr::null_mut()),
            recording: false,
            hotkeys: HotkeyRegistrar::attach(hwnd),
            installed: false,
            icon_error: None,
        };

        // 图标：失败只记原因，**不否决**热键
        match create_icon(icon_rgba, icon_px, TRAY_ICON_PX, false) {
            Ok(hicon) => {
                tray.hicon = hicon;
                // 红点图标创建失败不致命：录制时仍改 tooltip，只是缺少颜色提示。
                tray.recording_icon =
                    create_icon(icon_rgba, icon_px, TRAY_ICON_PX, true).unwrap_or(hicon);
                TRAY_HICON_RAW.store(hicon.0 as isize, Ordering::SeqCst);
                if let Ok(mut g) = TRAY_TOOLTIP.lock() {
                    *g = Some(tip.to_string());
                }
                if unsafe { add_icon(hwnd, hicon, tip) } {
                    tray.installed = true;
                    TRAY_ICON_PRESENT.store(true, Ordering::SeqCst);
                } else {
                    TRAY_ICON_PRESENT.store(false, Ordering::SeqCst);
                    tray.icon_error =
                        Some(format!("Shell_NotifyIconW(NIM_ADD) 失败：{}", last_error_text()));
                }
            }
            Err(e) => tray.icon_error = Some(e),
        }

        // 全局热键：**与图标无关**，失败只降级（原因留在 `hotkey_error()`）
        let _ = tray.hotkeys.set_hotkey(initial_hotkey, None);
        Ok(tray)
    }

    /// 图标是否真的挂在通知区域（false 时调用方**不该隐藏窗口**）。
    pub fn icon_ok(&self) -> bool {
        // 以“当前真的在通知区域”为准：初始 NIM_ADD 失败后 Explorer 才启动、
        // 或 Explorer 重启，都会由 `TaskbarCreated` 重新 NIM_ADD 并更新这个全局标志。
        TRAY_ICON_PRESENT.load(Ordering::SeqCst)
    }

    /// 图标挂不上的原因（`None` = 挂上了）；只该进 `cap.log`，不该打扰用户。
    pub fn icon_error(&self) -> Option<&str> {
        self.icon_error.as_deref()
    }

    /// 更新录制指示：图标换成带红点的版本，tooltip 带计时。
    ///
    /// 返回是否真的写入通知区域（图标没挂上/`NIM_MODIFY` 失败 → false）。
    pub fn set_recording(&mut self, recording: bool, elapsed: std::time::Duration) -> bool {
        let hicon = if recording {
            if self.recording_icon.0.is_null() {
                self.hicon
            } else {
                self.recording_icon
            }
        } else {
            self.hicon
        };
        if hicon.0.is_null() {
            return false;
        }
        let tip = if recording {
            format!(
                "{} · Recording {:02}:{:02}",
                crate::ui::theme::APP_NAME,
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60
            )
        } else {
            crate::ui::theme::APP_NAME.to_string()
        };
        // 图标暂时没挂上（例如 Explorer 还没起）：仍记住当前 HICON/tooltip，
        // `TaskbarCreated` 重新 NIM_ADD 时会用这一份，不会丢掉录制状态。
        if !self.installed {
            self.recording = recording;
            TRAY_HICON_RAW.store(hicon.0 as isize, Ordering::SeqCst);
            if let Ok(mut g) = TRAY_TOOLTIP.lock() {
                *g = Some(tip);
            }
            return false;
        }
        if unsafe { modify_icon(self.hwnd, hicon, &tip) } {
            self.recording = recording;
            TRAY_HICON_RAW.store(hicon.0 as isize, Ordering::SeqCst);
            if let Ok(mut g) = TRAY_TOOLTIP.lock() {
                *g = Some(tip);
            }
            true
        } else {
            tracing::warn!("Shell_NotifyIconW(NIM_MODIFY) 失败：录制指示未更新");
            false
        }
    }

    /// 换全局热键（见 `HotkeyRegistrar::set_hotkey`）。
    pub fn set_hotkey(
        &mut self,
        want: Option<Hotkey>,
        fallback: Option<Hotkey>,
    ) -> Result<(), HotkeyRegisterError> {
        self.hotkeys.set_hotkey(want, fallback)
    }

    /// 当前注册着的组合。
    pub fn current_hotkey(&self) -> Option<Hotkey> {
        self.hotkeys.current()
    }

    /// 热键注册失败的原因（`None` = 成功，或用户自己把它关了）。
    pub fn hotkey_error(&self) -> Option<&HotkeyRegisterError> {
        self.hotkeys.error()
    }

    /// 热键当前是否真的注册着（界面可以据此显示状态）。
    pub fn hotkey_registered(&self) -> bool {
        self.hotkeys.is_registered()
    }

    /// 注销托盘图标 + 热键 + 销毁窗口/图标（幂等，**退出时务必走到**，避免"幽灵图标"）。
    fn cleanup(&mut self) {
        let was_present = TRAY_ICON_PRESENT.swap(false, Ordering::SeqCst);
        TRAY_HICON_RAW.store(0, Ordering::SeqCst);
        if self.installed || was_present {
            let nid = notify_data(self.hwnd, self.hicon);
            // NIM_DELETE 只需要 cbSize/hWnd/uID，其余字段忽略
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
            self.installed = false;
        }
        self.hotkeys.shutdown();
        if !self.hwnd.0.is_null() {
            let _ = unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = HWND(std::ptr::null_mut());
        }
        let idle_raw = self.hicon.0;
        if !self.recording_icon.0.is_null() && self.recording_icon.0 != idle_raw {
            let _ = unsafe { DestroyIcon(self.recording_icon) };
        }
        self.recording_icon = HICON(std::ptr::null_mut());
        if !self.hicon.0.is_null() {
            let _ = unsafe { DestroyIcon(self.hicon) };
            self.hicon = HICON(std::ptr::null_mut());
        }
        if let Ok(mut g) = TRAY_TOOLTIP.lock() {
            *g = None;
        }
        if let Ok(mut g) = REPAINT_CTX.lock() {
            *g = None;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// 组装一个 `NOTIFYICONDATAW`（`NIF_ICON | NIF_MESSAGE | NIF_TIP`）。
fn notify_data(hwnd: HWND, hicon: HICON) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: hicon,
        ..Default::default()
    };
    fill_tip(&mut nid, crate::ui::theme::APP_NAME);
    nid
}

/// 把提示文本写进 `szTip`（固定 128 个 UTF-16，超出截断，**保证结尾 0**）。
fn fill_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let wide = to_wide(tip);
    let n = wide.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&wide[..n]);
    if n == nid.szTip.len() {
        nid.szTip[n - 1] = 0;
    }
}

unsafe fn add_icon(hwnd: HWND, hicon: HICON, tip: &str) -> bool {
    let mut nid = notify_data(hwnd, hicon);
    fill_tip(&mut nid, tip);
    Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
}

unsafe fn modify_icon(hwnd: HWND, hicon: HICON, tip: &str) -> bool {
    let mut nid = notify_data(hwnd, hicon);
    fill_tip(&mut nid, tip);
    Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
}

/// 系统错误文本（`GetLastError` + 一段可读的格式化消息）。
fn last_error_text() -> String {
    let code = unsafe { GetLastError() };
    format!(
        "GetLastError={}（{}）",
        code.0,
        std::io::Error::from_raw_os_error(code.0 as i32)
    )
}

/// 建**隐藏的顶层窗口**做托盘回调/热键的接收窗口。
///
/// 为什么不是 `HWND_MESSAGE` 的 message-only 窗口（两者需求里都允许）——
/// **实测（本机、跨进程）**：
/// * message-only 窗口**不进 `EnumWindows`**，而另一个进程用
///   `FindWindowExW(HWND_MESSAGE, 0, 类名, NULL)` / `EnumChildWindows(HWND_MESSAGE)`
///   也**找不到**它（实测 `child count = 0`、`FindWindowExW` 返回 0）——
///   于是"用 `PostMessage(WM_HOTKEY)` 打到自己的消息窗口来验证处理逻辑"这条路直接断了
///   （这也是本轮的硬性验证要求之一）；
/// * `Shell_NotifyIcon` 的文档与生态实践（`tray-icon` 等）用的都是**普通窗口**。
///
/// 所以改成：**顶层窗口 + `WS_POPUP` + 永不 `ShowWindow`** —— 不在任务栏、不在 Alt+Tab
///（`WS_EX_TOOLWINDOW`），行为上等价于"只收消息"，但**另一个进程找得到**，可自动化验证。
///
/// **必须由主线程（有消息循环的那条）调用**：消息只会被**创建窗口的线程**的消息循环取走。
pub(crate) fn create_message_window() -> Result<HWND, String> {
    // 类名固定：`RegisterClassW` 在同一进程内第二次会返回 0（类已存在），
    // 那一项**不算失败**（错误 1410 ERROR_CLASS_ALREADY_EXISTS 是正常情况）。
    let class = windows::core::w!("CapLiteTrayMessageWindow");
    let hinstance =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW 失败：{e}"))?;
    let wc = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: HINSTANCE(hinstance.0),
        lpszClassName: class,
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            windows::core::w!("CapLiteTray"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(hinstance.0)),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW(隐藏顶层窗口) 失败：{e}"))?;
    Ok(hwnd)
}

/// 在 RGBA 图标右下角画一个红点（带白边），用于录制指示。
fn paint_recording_badge(rgba: &mut [u8], side: u32) {
    let s = side as i32;
    let r = (s / 5).max(2);
    let cx = s - r - 1;
    let cy = s - r - 1;
    for y in 0..s {
        for x in 0..s {
            let dx = x - cx;
            let dy = y - cy;
            let d2 = dx * dx + dy * dy;
            let idx = ((y * s + x) * 4) as usize;
            if idx + 3 >= rgba.len() {
                continue;
            }
            if d2 <= r * r {
                rgba[idx] = 220;
                rgba[idx + 1] = 32;
                rgba[idx + 2] = 32;
                rgba[idx + 3] = 255;
            } else if d2 <= (r + 1) * (r + 1) {
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
        }
    }
}

/// `assets/caplite-256.rgba` → `HICON`（32bpp 自上而下 DIB + `CreateIconIndirect`）。
fn create_icon(rgba: &[u8], src_side: u32, side: u32, recording: bool) -> Result<HICON, String> {
    let side = side.max(1);
    let mut small = downscale_rgba(rgba, src_side, side);
    if recording {
        paint_recording_badge(&mut small, side);
    }
    if small.len() < (side * side * 4) as usize {
        return Err(format!(
            "图标像素不足：{} 字节（需要 {}）—— assets/caplite-256.rgba 与 WINDOW_ICON_PX 对不上？",
            rgba.len(),
            src_side * src_side * 4
        ));
    }
    let bgra = rgba_to_bgra(&small);
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: side as i32,
                // **负高度 = 自上而下**：与 RGBA 的行序一致，否则图标会上下翻转
                biHeight: -(side as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: side * side * 4,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default()],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbm_color: HBITMAP = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| format!("CreateDIBSection 失败：{e}"))?;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(hbm_color.0));
            return Err("CreateDIBSection 返回了空像素指针".to_string());
        }
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());

        // 32bpp 彩色位图 + 1bpp 掩码（有 alpha 通道时掩码只是历史包袱，但要给）
        let hbm_mask = CreateBitmap(side as i32, side as i32, 1, 1, None);
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let hicon = CreateIconIndirect(&info).map_err(|e| {
            let _ = DeleteObject(HGDIOBJ(hbm_color.0));
            let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
            format!("CreateIconIndirect 失败：{e}")
        })?;
        // 位图已经被 CreateIconIndirect 复制进 HICON，源位图可以立刻删
        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        Ok(hicon)
    }
}

// ---------------------------------------------------------------------------
// 主窗口置前（托盘菜单「显示窗口」的第二步）
// ---------------------------------------------------------------------------

/// `EnumWindows` 回调找到的主窗口句柄（0 = 没找到）。
static FOUND_MAIN: AtomicIsize = AtomicIsize::new(0);

/// **缓存**下来的主窗口句柄（0 = 还没找到）。见 [`find_main_window`]。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// winit 在 Windows 上给顶层窗口用的类名（实测 `class='Window Class'`）。
///
/// 用它**先过滤**是必须的，不是优化 —— 见 [`find_main_proc`] 的注释。
const MAIN_WINDOW_CLASS: &str = "Window Class";

unsafe extern "system" fn find_main_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    // **先比类名**（`GetClassNameW` 只是查询，不发消息），再比进程与标题。
    //
    // 这个顺序是本轮实测逼出来的：`GetWindowTextW` 对**同进程**窗口会
    // `SendMessage(WM_GETTEXT)`，而本线程上还有 IME / MSCTFIME 几个窗口
    //（WndProc 在 msctfime.ime / IMM32 里）。在**自己的绘制流程里**逐个
    // `GetWindowTextW` 会把界面线程卡死 —— 实测症状：开始录制那一帧之后再无任何一帧，
    // `IsWindowVisible` 永远停在 True、第二次 `WM_HOTKEY` 也不再被处理。
    // 先比类名，就只会对 "Window Class"（winit 的顶层窗口）调用 `GetWindowTextW`。
    let mut cbuf = [0u16; 64];
    let cn = unsafe { GetClassNameW(hwnd, &mut cbuf) };
    if cn <= 0 {
        return TRUE;
    }
    if String::from_utf16_lossy(&cbuf[..cn as usize]) != MAIN_WINDOW_CLASS {
        return TRUE;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != unsafe { GetCurrentProcessId() } {
        return TRUE;
    }
    let mut buf = [0u16; 128];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if n <= 0 {
        return TRUE;
    }
    // 窗口标题是 `CapLite v1.0.0`（P44 在标题栏加了版本号），所以这里用**前缀**匹配而不是全等：
    // 版本号一变（或将来加后缀）就不会把"从托盘恢复窗口"悄悄弄坏 —— 而全等匹配时
    // 标题改一个字就会让这条路径静默失效（托盘图标点不动窗口，且没有任何报错）。
    // 前面已经用"窗口类 = 本进程主窗口类 + PID = 本进程"筛过一遍，前缀匹配不会误认别人的窗口。
    if String::from_utf16_lossy(&buf[..n as usize]).starts_with(crate::ui::theme::APP_NAME) {
        FOUND_MAIN.store(hwnd.0 as isize, Ordering::SeqCst);
        return BOOL(0); // 找到就停
    }
    TRUE
}

/// 本进程主窗口（标题以 [`crate::ui::theme::APP_NAME`] **开头**的那个顶层窗口 ——
/// 实际标题是 `CapLite v1.0.0`）。找不到返回 `None`。
///
/// **句柄会被缓存**（`IsWindow` 校验失效后再重找）：隐藏/恢复在"录制开始/结束"这条
/// 关键路径上，不该每次都遍历整张顶层窗口表。
pub(crate) fn find_main_window() -> Option<HWND> {
    let cached = MAIN_HWND.load(Ordering::SeqCst);
    if cached != 0 {
        let hwnd = HWND(cached as *mut core::ffi::c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Some(hwnd);
        }
        MAIN_HWND.store(0, Ordering::SeqCst);
    }
    FOUND_MAIN.store(0, Ordering::SeqCst);
    let _ = unsafe { EnumWindows(Some(find_main_proc), LPARAM(0)) };
    let raw = FOUND_MAIN.load(Ordering::SeqCst);
    if raw == 0 {
        None
    } else {
        MAIN_HWND.store(raw, Ordering::SeqCst);
        Some(HWND(raw as *mut core::ffi::c_void))
    }
}

/// 主窗口当前是否可见（`IsWindowVisible`；找不到窗口返回 `None`）。
///
/// 它是"隐藏到托盘 / 恢复可见"这条链路的**唯一权威读数** —— 有它才能把
/// "我们发了命令"与"窗口真的藏了/真的回来了"分开，而不是靠推断。
pub fn main_window_visible() -> Option<bool> {
    let hwnd = find_main_window()?;
    Some(unsafe { IsWindowVisible(hwnd) }.as_bool())
}

/// **立刻**把主窗口藏起来（原始 Win32 兜底，与 `ViewportCommand::Visible(false)` 合用）。
///
/// 为什么需要它（全部是本机实测结论）：窗口隐藏之后 eframe 只跑 `App::logic`，
/// 而实测**隐藏期间不会再有新的帧**（即使 `logic` 里持续 `request_repaint_after`）——
/// 于是"靠下一帧把 `Visible(true)` 落到窗口上"这条路会把窗口永久留在隐藏状态。
/// 隐藏/恢复因此都**同步**再调一次 Win32：命令那条路径负责 egui/winit 的状态机，
/// 这一条负责"这一帧就真的生效"。两条都做，谁生效都不影响另一个人。
pub fn hide_main_window_now() -> bool {
    match find_main_window() {
        Some(hwnd) => unsafe { ShowWindow(hwnd, SW_HIDE) }.as_bool(),
        None => false,
    }
}

/// **立刻**把主窗口显示出来并置前（配 [`hide_main_window_now`] 使用）。
///
/// 返回 `(调用前是否可见, 置前是否成功, 调用后是否可见)`；全是 best-effort，
/// 失败只该记一行日志。
///
/// 注意 `ShowWindow` 的返回值是"**调用前**的可见状态"，**不是**成功与否 ——
/// 所以这里明确把它换成前后两次 `IsWindowVisible` 的读数，日志才不会误导人。
pub fn show_main_window_now() -> (Option<bool>, bool, Option<bool>) {
    let Some(hwnd) = find_main_window() else {
        return (None, false, None);
    };
    let before = Some(unsafe { IsWindowVisible(hwnd) }.as_bool());
    // `ShowWindow` 的返回值是"调用前的可见状态"，这里只当它是个副作用调用
    // （真实结果用下面那次 `IsWindowVisible` 读）。
    // 用户自己最小化过时 `SW_SHOW` 不会还原，必须用 `SW_RESTORE`。
    let cmd = if unsafe { IsIconic(hwnd) }.as_bool() {
        SW_RESTORE
    } else {
        SW_SHOW
    };
    let _ = unsafe { ShowWindow(hwnd, cmd) };
    let front = unsafe { SetForegroundWindow(hwnd) }.as_bool();
    let after = Some(unsafe { IsWindowVisible(hwnd) }.as_bool());
    (before, front, after)
}

// ---------------------------------------------------------------------------
// 单测
//
// 除了纯逻辑，这里还有一条**真机自动化**的核心验收：
// `occupied_hotkey_reports_1409_and_rolls_back` —— 在同一进程里先让另一个线程真的
// `RegisterHotKey` 抢占某个组合，再走我们的注册路径，断言拿到 1409、旧热键回滚、
// 并且给了候选建议。它不装托盘图标（不碰通知区域），但走的是**同一条注册代码**。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 纯逻辑：组合 / 预设 / 校验 ----

    #[test]
    fn recording_badge_paints_red_pixels() {
        let side = 32u32;
        let mut rgba = vec![0u8; (side * side * 4) as usize];
        paint_recording_badge(&mut rgba, side);
        let red = rgba.chunks_exact(4).filter(|p| p[0] > 180 && p[1] < 80).count();
        assert!(red > 10, "应画出红色角标，实际 {red}");
    }

    /// 三种 preset 的**有效组合**（需求里"三选一"的核心算法）。
    #[test]
    fn preset_resolves_to_the_expected_combo() {
        assert_eq!(
            effective_hotkey(HotkeyPreset::CtrlAltR, "").label(),
            "Ctrl+Alt+R"
        );
        assert_eq!(
            effective_hotkey(HotkeyPreset::CtrlShiftR, "").label(),
            "Ctrl+Shift+R"
        );
        // 自定义生效（且大小写/空格被规范化）
        assert_eq!(
            effective_hotkey(HotkeyPreset::Custom, "  ctrl + SHIFT + f9 ").label(),
            "Ctrl+Shift+F9"
        );
        // 自定义写坏了 → 回退到默认组合（不是"没有热键"）
        assert_eq!(effective_hotkey(HotkeyPreset::Custom, "").label(), "Ctrl+Alt+R");
        assert_eq!(
            effective_hotkey(HotkeyPreset::Custom, "R").label(),
            "Ctrl+Alt+R",
            "没有修饰键的自定义必须回退"
        );
        assert_eq!(DEFAULT_HOTKEY.label(), "Ctrl+Alt+R");
        assert_eq!(preset_hotkey(HotkeyPreset::Custom), None);
        assert_eq!(HotkeyPreset::default(), HotkeyPreset::CtrlAltR);
    }

    /// 预设的序列化名就是配置里的字符串（改了这里就等于改了配置文件格式）。
    #[test]
    fn preset_serializes_with_the_documented_names() {
        let cases = [
            (HotkeyPreset::CtrlAltR, "\"ctrl_alt_r\""),
            (HotkeyPreset::CtrlShiftR, "\"ctrl_shift_r\""),
            (HotkeyPreset::Custom, "\"custom\""),
        ];
        for (preset, json) in cases {
            assert_eq!(serde_json::to_string(&preset).unwrap(), json);
            assert_eq!(serde_json::from_str::<HotkeyPreset>(json).unwrap(), preset);
        }
    }

    /// 解析：修饰键归一化 + 键名表（`F1`–`F24`、字母、数字、特殊键）+ 修正常见写法。
    #[test]
    fn parse_accepts_and_normalizes() {
        for text in [
            "Ctrl+Alt+R",
            "ctrl+alt+r",
            "CONTROL + ALT + R",
            "Alt+Ctrl+R",
            "Win+R",
            "Meta+R",
            "Ctrl+Shift+F9",
            "Ctrl+F24",
            "Ctrl+Alt+Space",
            "Ctrl+Alt+PageUp",
        ] {
            let hk = parse_hotkey(text).unwrap_or_else(|e| panic!("{text} 应能解析，实际 {e:?}"));
            assert!(
                hk.modifiers.0 & MOD_NOREPEAT.0 != 0,
                "解析结果必须带 MOD_NOREPEAT"
            );
            // 规范化后的文本必须能再次解析成同一个组合（往返稳定）
            let again = parse_hotkey(&hk.label()).expect("规范化文本应能再解析");
            assert_eq!(again, hk, "{text} → {} 的往返不一致", hk.label());
        }
        assert_eq!(parse_hotkey("ctrl+alt+r").unwrap(), PRESET_CTRL_ALT_R);
        assert_eq!(parse_hotkey("Ctrl+Shift+R").unwrap(), PRESET_CTRL_SHIFT_R);
        assert_eq!(parse_hotkey("ctrl+alt+f9").unwrap().label(), "Ctrl+Alt+F9");
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Space").unwrap().label(),
            "Ctrl+Alt+Space"
        );
    }

    /// 校验：**没有修饰键**、只有修饰键、认不出的键名、系统保留组合，全都要被拒。
    #[test]
    fn parse_rejects_invalid_and_reserved() {
        assert_eq!(parse_hotkey("R"), Err(HotkeyParseError::NoModifier));
        assert_eq!(parse_hotkey("F9"), Err(HotkeyParseError::NoModifier));
        assert_eq!(parse_hotkey("Ctrl"), Err(HotkeyParseError::NoKey));
        assert_eq!(parse_hotkey("Ctrl+Alt"), Err(HotkeyParseError::NoKey));
        assert_eq!(parse_hotkey("Ctrl+Alt+"), Err(HotkeyParseError::Malformed("Ctrl+Alt+".into())));
        assert_eq!(parse_hotkey("Ctrl+Alt+媒体键"), Err(HotkeyParseError::Malformed("媒体键".into())));
        assert!(matches!(
            parse_hotkey("Ctrl+Alt+Q+W"),
            Err(HotkeyParseError::Malformed(_))
        ));

        // 系统保留：Ctrl+Alt+Del / Ctrl+Shift+Esc / Win+L / Alt+Tab / Ctrl+Esc
        for text in ["Ctrl+Alt+Del", "Ctrl+Shift+Esc", "Win+L", "Alt+Tab", "Ctrl+Esc"] {
            let parsed = parse_hotkey(text);
            assert!(
                matches!(parsed, Err(HotkeyParseError::Reserved(_))),
                "{text} 应当被当作系统保留组合拒绝，实际 {parsed:?}"
            );
        }
        // 但同样的键配上别的修饰键是允许的（只有**那一个**组合是保留的）
        assert!(parse_hotkey("Ctrl+Shift+L").is_ok());
        assert!(parse_hotkey("Ctrl+Alt+Tab").is_ok());
    }

    /// 键名的正反查表自洽（`label()` 用的名字必须能被 `parse_hotkey` 认回来）。
    ///
    /// 用 `Ctrl+Alt+Shift+键` 组合来测：`Ctrl+Esc` / `Ctrl+Shift+Esc` 本身就是保留组合，
    /// 拿它们做往返会被（正确地）拒掉，掩盖"名字能不能反解"这一件事。
    #[test]
    fn vk_names_round_trip() {
        for code in [VK_SPACE, VK_TAB, VK_RETURN, VK_ESCAPE, VK_DELETE, VK_F1, VK_F24, 0x41, 0x39] {
            let name = vk_name(code);
            let hk = parse_hotkey(&format!("Ctrl+Alt+Shift+{name}"))
                .unwrap_or_else(|e| panic!("vk_name({code}) = {name} 反解失败：{e:?}"));
            assert_eq!(hk.vk, code, "{name} 应反解回 {code}");
        }
        assert_eq!(vk_name(0x70), "F1");
        assert_eq!(vk_name(0x87), "F24");
        assert_eq!(vk_name(0x5A), "Z");
        assert_eq!(vk_name(0x09), "Tab");
        assert_eq!(vk_name(0x5B), "0x5B", "没覆盖的键码退化成十六进制，绝不瞎猜");
    }

    // ---- 纯逻辑：隐藏 / 恢复 ----

    /// **"谁隐藏谁恢复"**：开关开 → 藏一次；录制中不重复发命令；停止后恢复一次。
    #[test]
    fn hide_state_hides_once_and_restores_once() {
        let mut s = HideState::default();
        assert!(!s.hidden_by_us());
        assert_eq!(s.poll(true, true), VisibilityAction::Hide);
        assert!(s.hidden_by_us());
        assert_eq!(s.poll(true, true), VisibilityAction::Nothing, "不重复发命令");
        assert_eq!(s.poll(true, true), VisibilityAction::Nothing);
        assert_eq!(s.poll(false, true), VisibilityAction::Show);
        assert!(!s.hidden_by_us());
        assert_eq!(s.poll(false, true), VisibilityAction::Nothing);
    }

    /// **开关关闭时一个命令都不发**（也不会偷偷把标志置起来）。
    #[test]
    fn hide_state_sends_nothing_when_switch_is_off() {
        let mut s = HideState::default();
        for _ in 0..3 {
            assert_eq!(s.poll(true, false), VisibilityAction::Nothing);
        }
        assert_eq!(s.poll(false, false), VisibilityAction::Nothing);
        assert!(!s.hidden_by_us(), "开关关闭时不该认为窗口是我们隐藏的");
    }

    /// **不覆盖用户自己隐藏窗口的意图**。
    #[test]
    fn hide_state_never_restores_what_it_did_not_hide() {
        let mut s = HideState::default();
        assert_eq!(s.poll(false, true), VisibilityAction::Nothing);
        assert!(!s.hidden_by_us());
    }

    /// 录制中途把开关从 true 改成 false：已经藏了就保持藏着，结束时仍然要恢复。
    #[test]
    fn hide_state_restores_even_if_switch_flipped_mid_recording() {
        let mut s = HideState::default();
        assert_eq!(s.poll(true, true), VisibilityAction::Hide);
        assert_eq!(s.poll(true, false), VisibilityAction::Nothing, "开关关了也不再发命令");
        assert_eq!(s.poll(false, false), VisibilityAction::Show, "是我们藏的，必须还回来");
    }

    // ---- 纯逻辑：图标像素 ----

    /// 通道交换：`RGBA → BGRA` 只动 R/B。
    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_only() {
        let src = [1, 2, 3, 4, 200, 100, 50, 0];
        assert_eq!(rgba_to_bgra(&src), vec![3, 2, 1, 4, 50, 100, 200, 0]);
    }

    /// 降采样：尺寸对、全透明保持全透明、颜色不被透明像素污染。
    #[test]
    fn downscale_keeps_dimensions_and_alpha_weighting() {
        let mut src = Vec::new();
        for _ in 0..16 {
            src.extend_from_slice(&[10, 20, 30, 255]);
        }
        let out = downscale_rgba(&src, 4, 2);
        assert_eq!(out.len(), 2 * 2 * 4);
        for px in out.chunks_exact(4) {
            assert_eq!(px, [10, 20, 30, 255]);
        }

        let mut src = Vec::new();
        for i in 0..16 {
            if i % 2 == 0 {
                src.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                src.extend_from_slice(&[0, 0, 255, 0]);
            }
        }
        for px in downscale_rgba(&src, 4, 2).chunks_exact(4) {
            assert_eq!(px[0], 255, "R 必须仍是 255（未被透明像素拉低）");
            assert_eq!(px[2], 0, "B 必须仍是 0（透明像素的颜色不许渗进来）");
            assert!(px[3] > 0 && px[3] < 255, "alpha 应当是 0/255 的中间值");
        }

        let zeros = vec![0u8; 16 * 4];
        assert!(downscale_rgba(&zeros, 4, 2)
            .chunks_exact(4)
            .all(|p| p == [0, 0, 0, 0]));
        assert!(downscale_rgba(&[0u8; 5], 4, 2).is_empty());
        assert!(downscale_rgba(&[], 0, 2).is_empty());
    }

    /// 提示文本写进 `szTip` 时**一定以 0 结尾**，超长要截断而不是溢出。
    #[test]
    fn tip_is_always_null_terminated() {
        let mut nid = NOTIFYICONDATAW {
            szTip: [0xFFFFu16; 128],
            ..Default::default()
        };
        fill_tip(&mut nid, "CapLite");
        assert_eq!(&nid.szTip[..8], &to_wide("CapLite")[..]);
        assert_eq!(nid.szTip[7], 0);

        let long = "汉".repeat(300);
        fill_tip(&mut nid, &long);
        assert_eq!(*nid.szTip.last().unwrap(), 0, "超长时最后一位必须是 0");
    }

    /// 命令队列：入队/出队一对一，取空之后再取还是空。
    #[test]
    fn command_queue_round_trips() {
        assert!(take_commands().is_empty());
        if let Ok(mut q) = COMMANDS.lock() {
            q.push_back(TrayCommand::ToggleRecording);
            q.push_back(TrayCommand::Quit);
        }
        assert_eq!(
            take_commands(),
            vec![TrayCommand::ToggleRecording, TrayCommand::Quit]
        );
        assert!(take_commands().is_empty());
    }

    // ---- 真机：注册 / 占用 / 回滚 / 候选探测 ----

    /// 占用线程：在**另一个线程**上真的 `RegisterHotKey` 抢住 `hk`，并一直占着，
    /// 直到测试发信号让它退出（退出时注销，不留痕迹）。
    ///
    /// 为什么要在**另一个线程**上抢：Windows 的热键是**按线程**登记的，
    /// 同线程重复注册同一个组合的语义不保证是"冲突"；换一个线程就一定是冲突，
    /// 这样这条测试在任何机器上都是确定性的。
    fn occupy_on_another_thread(
        hk: Hotkey,
    ) -> (
        std::sync::mpsc::Receiver<Result<(), String>>,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // hwnd = NULL：WM_HOTKEY 会投到本线程队列（本线程不泵消息，所以只会躺着）
            let registered = unsafe { RegisterHotKey(None, HOTKEY_PROBE_ID, hk.modifiers, hk.vk) };
            let ok = registered.is_ok();
            let _ = ready_tx.send(match &registered {
                Ok(()) => Ok(()),
                Err(e) => Err(e.to_string()),
            });
            let _ = stop_rx.recv();
            if ok {
                let _ = unsafe { UnregisterHotKey(None, HOTKEY_PROBE_ID) };
            }
        });
        (ready_rx, stop_tx, handle)
    }

    /// **核心验收：占用检测（1409）+ 回滚 + 候选推荐，全部真机、可自动化。**
    ///
    /// 步骤：① 另一个线程抢住 `Ctrl+Shift+F10`（冷门组合）→ ② 我们按正常路径注册
    /// `Ctrl+Shift+F10` → 必须拿到 `GetLastError()==1409` → ③ 断言错误结构化信息正确、
    /// **旧热键仍然注册着**（回滚成功）→ ④ 断言候选探测给了建议。
    #[test]
    fn occupied_hotkey_reports_1409_and_rolls_back() {
        // 用一个"几乎不可能被别的程序占用"的组合，保证测试自身可重复
        let contested = parse_hotkey("Ctrl+Shift+F10").expect("测试用组合应合法");
        let (ready, stop, handle) = occupy_on_another_thread(contested);
        match ready.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("占用线程自己都没抢到 {contested:?}：{e}"),
            Err(e) => panic!("占用线程没有回报：{e}"),
        }

        let hwnd = create_message_window().expect("单测里也要能建 message-only 窗口");
        let mut reg = HotkeyRegistrar::attach(hwnd);

        // 先注册一个"旧热键"（用另一个冷门组合，避免与占用组合撞车）
        let old = parse_hotkey("Ctrl+Alt+F11").expect("旧热键应合法");
        reg.set_hotkey(Some(old), None)
            .expect("旧热键应当能注册成功（若这台机器恰好占用它，请换一个组合）");
        assert!(reg.is_registered());
        assert_eq!(reg.current(), Some(old));

        // 换成被占用的组合 → 必须失败、必须 1409、必须回滚到旧的
        let err = reg
            .set_hotkey(Some(contested), Some(old))
            .expect_err("被占用的组合不可能注册成功");
        assert_eq!(
            err.code, ERROR_HOTKEY_ALREADY_REGISTERED,
            "必须拿到 1409 ERROR_HOTKEY_ALREADY_REGISTERED，实际 {}（{}）",
            err.code, err.system
        );
        assert!(err.already_registered());
        assert_eq!(err.hotkey, contested);
        assert_eq!(
            reg.current(),
            Some(old),
            "注册失败必须**回滚**，旧热键要仍然可用"
        );
        assert!(reg.is_registered(), "回滚之后热键必须是真的注册着的");
        // 错误详情要能被界面/日志直接用
        let text = err.describe();
        assert!(text.contains("1409"), "详情里必须带错误码：{text}");
        assert!(text.contains("Ctrl+Shift+F10"), "详情里必须带组合名：{text}");
        assert!(
            text.contains("候选探测建议改用") || text.contains("候选组合也都不可用"),
            "详情里必须带候选探测结论：{text}"
        );

        // 关掉 → 什么都不注册
        reg.set_hotkey(None, None).expect("注销永远成功");
        assert!(!reg.is_registered());
        assert_eq!(reg.current(), None);
        assert!(reg.error().is_none(), "主动关闭不算失败");

        reg.shutdown();
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = stop.send(());
        let _ = handle.join();
    }

    /// 正常路径：注册新组合 → 换另一个 → 关掉，全部 `Ok`，且 `current()` 跟着走。
    #[test]
    fn hotkey_registrar_tracks_the_current_combo() {
        let a = parse_hotkey("Ctrl+Shift+F12").expect("合法");
        let b = parse_hotkey("Ctrl+Alt+F12").expect("合法");
        let hwnd = create_message_window().expect("建窗口");
        let mut reg = HotkeyRegistrar::attach(hwnd);

        assert_eq!(reg.current(), None);
        reg.set_hotkey(Some(a), None).expect("注册 a");
        assert_eq!(reg.current(), Some(a));
        // 再设成同一个：空操作也是 Ok
        reg.set_hotkey(Some(a), None).expect("重复设成 a 也是 Ok");
        assert_eq!(reg.current(), Some(a));
        // 换成 b：旧的要先让位，所以必须成功
        reg.set_hotkey(Some(b), Some(a)).expect("换成 b");
        assert_eq!(reg.current(), Some(b));
        // 关掉
        reg.set_hotkey(None, Some(b)).expect("关掉");
        assert_eq!(reg.current(), None);

        reg.shutdown();
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}
