//! 持久化配置：语言、存储位置、编码质量、采集范围、音源等。
//!
//! 设计要点（针对旧版缺陷）：
//! * `#[serde(default)]` —— 配置文件缺字段时**逐字段**回落默认值，不再整份重置；
//! * 保存走"临时文件 + 重命名"的原子写法，失败会**返回错误**而不是静默吞掉；
//! * 所有数值字段统一经 [`Config::sanitized`] 夹紧到合法区间，防止
//!   `fps=100000` 之类的手改配置把采集线程打满；
//! * **路径全部先解析成绝对路径**（见 `paths`）：双击 exe 时工作目录可能是
//!   `C:\Windows\System32`，任何相对路径都不能留到运行期；
//! * **配置文件目录也做可用性探测**：`用户\Documents\cap` 写不进去时退回
//!   `<exe 目录>\cap-recordings` → 系统临时目录，配置始终尽量能落盘（见 [`ConfigStore`]）。

use crate::error::{AppError, Result};
use crate::tray::HotkeyPreset;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod paths;

pub use paths::{
    default_fallback_dirs, plan_output_dir, probe_dir_writable, probe_dir_writable_timeout,
    resolve_against_exe_dir, DirPlan,
};

/// 把命令行/环境变量给的配置目录解析成绝对路径（空或无法解析 → `None`）。
pub fn resolve_config_dir(value: &str) -> Option<PathBuf> {
    paths::resolve_against_exe_dir(value).ok()
}

/// 配置文件名（`paths` 里的回退链也要用它拼完整路径，所以以那里为唯一来源）。
pub const CONFIG_FILE: &str = paths::CONFIG_FILE;
/// 配置 JSON 的大小上限：正常只有几 KB；超过它说明文件损坏或被恶意放大。
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// 配置文件读取的总预算。正常本地磁盘是毫秒级；超过它基本只剩
/// 断网 UNC / 休眠盘 / 被安全软件挂起几种情况，启动不应无限等。
const CONFIG_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// 配置目录回退链的总探测预算。单个候选仍限制 1.5 s，但所有候选共享这个上限。
const CONFIG_DIR_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureMode {
    /// 全屏（整个显示器）
    FullScreen,
    /// 自定义矩形区域
    Region,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentMode {
    /// 画面 + 声音
    VideoAndAudio,
    /// 仅画面
    VideoOnly,
    /// 仅声音
    AudioOnly,
}

impl ContentMode {
    pub fn has_video(self) -> bool {
        matches!(self, ContentMode::VideoAndAudio | ContentMode::VideoOnly)
    }
    pub fn has_audio(self) -> bool {
        matches!(self, ContentMode::VideoAndAudio | ContentMode::AudioOnly)
    }
}

/// 采集区域（虚拟桌面坐标，单位=物理像素）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Default for Region {
    fn default() -> Self {
        // 未知屏幕尺寸时的保守默认：1280x720，随后会被真实显示器尺寸覆盖
        Self {
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
        }
    }
}

impl Region {
    pub fn is_valid(&self) -> bool {
        self.width >= 16 && self.height >= 16
    }
    pub fn right(&self) -> i64 {
        self.x as i64 + self.width as i64
    }
    pub fn bottom(&self) -> i64 {
        self.y as i64 + self.height as i64
    }
    /// 对齐到偶数（H.264 4:2:0 要求宽高为偶数）
    pub fn even(&self) -> Self {
        Self {
            x: self.x,
            y: self.y,
            width: self.width & !1,
            height: self.height & !1,
        }
    }
}

/// 支持录制的帧率档位（**设置页用下拉菜单列出这两档**，默认 [`DEFAULT_FPS`]）。
///
/// 为什么收敛成两档：屏幕录制场景里 30 与 60 已经覆盖"省体积/求顺滑"两种诉求，
/// 中间值（24/25/29.97/48…）既没有真实收益，又会让码率与关键帧间隔的整算变得模糊。
/// 其它值由 [`nearest_fps`] 吸附到最近的档位，所以运行期只会见到这两档。
pub(crate) const FPS_CHOICES: [u32; 2] = [DEFAULT_FPS, 60];
/// 默认帧率（30）：更省 CPU 与磁盘，屏幕内容通常也看不出差别。
pub(crate) const DEFAULT_FPS: u32 = 30;

/// 目标码率（OpenH264 的 `iTargetBitrate`）允许的区间，单位 **kbps**（= 5–20 Mbps）。
///
/// 界面上用 Mbps 显示（保留一位小数），存储仍是 kbps（[`Config::video_bitrate_kbps`]）。
/// 上限与内部固定的最大码率（[`crate::video::encode::MAX_BITRATE_BPS`] = 20 Mbps）对齐，
/// 保证"目标 ≤ 上限"，OpenH264 永远不会因为目标码率超上限而 `cmInitParaError`。
pub const VIDEO_BITRATE_MIN_KBPS: u32 = 5_000;
pub const VIDEO_BITRATE_MAX_KBPS: u32 = 20_000;
/// 默认目标码率：**5 Mbps**（区间下限；屏幕内容足够清晰，又不至于把磁盘写爆）。
pub(crate) const DEFAULT_VIDEO_BITRATE_KBPS: u32 = VIDEO_BITRATE_MIN_KBPS;

/// 把任意帧率吸附到 [`FPS_CHOICES`] 里**最近**的一档；**平局取靠前的那档（30）**。
///
/// 只有两档时它等价于"以中点为界"，但写成"逐档比距离"是为了将来加档位时
/// 这条语义（最近 + 平局取前）不用重写。
pub(crate) fn nearest_fps(fps: u32) -> u32 {
    let mut best = FPS_CHOICES[0];
    let mut best_dist = u32::MAX;
    for &v in FPS_CHOICES.iter() {
        let dist = v.abs_diff(fps);
        if dist < best_dist {
            best_dist = dist;
            best = v;
        }
    }
    best
}

/// 音频码率（AAC-LC，kbps）：**内部固定 192**，界面不再暴露。
///
/// 192 kbps 对屏幕录制（人声/系统声）足够透明，又不会让文件无谓变大；同样是
/// [`Config::sanitized`] 里"外部没有的选项一律归一"的一员。
pub const FIXED_AUDIO_BITRATE_KBPS: u32 = 192;

/// 布尔字段的"缺省即 true"。
///
/// 为什么要单列一个函数而不是让 `bool::default()`（= false）说了算：
/// [`Config`] 上虽然也有容器级 `#[serde(default)]`（缺字段从 `Config::default()` 取），
/// 但这两个开关的语义是"**默认打开**"，把它写成**字段级** `default = "default_true"`
/// 才不依赖容器的解析细节 —— 老配置文件里没有这两个键时，读出来必须还是 true，
/// 而且**不能因为缺键就 load 失败**。
pub(crate) fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub language: String,
    /// **深色模式**（P40）。默认 **true = 深色**（P42：用户要求"设置默认主题为黑色"）。
    ///
    /// 老配置文件里没有这个键 → 读出来是 `true`（新装/升级即深色，与产品默认一致）；
    /// 显式写了 `false` 的老配置**保持浅色** —— 用户自己的选择永远赢过默认值
    /// （设置页"界面语言"右侧那个开关，调整即保存，见 [`crate::ui::settings`]）。
    ///
    /// 为什么不是"跟随系统"：浅/深两套皮肤是我们的**产品口径**（`ui::theme` 的
    /// [`crate::ui::theme::ThemeKind`]），跟不跟随 `egui::ThemePreference::Dark`
    /// 由这里的一个 bool 决定，不存在"第三种自动态"—— 避免出现
    /// "系统是深色但程序仍画浅色"这种说不清的状态。
    pub dark_mode: bool,
    pub output_dir: String,
    /// 配置文件所在目录；`None` = 用默认位置（`%USERPROFILE%\Documents\cap`）。
    /// 允许从配置文件里改，是为了让用户能把配置放进便携目录或受管目录。
    pub config_dir: Option<String>,

    // 采集
    pub capture_mode: CaptureMode,
    pub monitor_index: usize,
    pub region: Region,

    // 内容与音源
    pub content_mode: ContentMode,
    /// 是否把鼠标指针合成进画面（DXGI 帧本身不含指针）
    pub draw_cursor: bool,
    /// 是否采集**系统声音**（WASAPI 环回）。**默认开**（P8：新装默认只录系统声音）。
    ///
    /// 为什么默认开：屏幕录制最常见的诉求是"画面 + 电脑里放出来的声音"，
    /// 而麦克风一旦默认打开，录出来会把环境噪音/键盘声一起收进去，用户还得回头关掉。
    /// 所以默认组合是 `audio_system = true` / `audio_mic = false`；要人声再去勾麦克风。
    pub audio_system: bool,
    /// 是否采集**麦克风**。**默认关**（P8，理由见 [`Config::audio_system`]）。
    ///
    /// 向后兼容：这个字段**只有"缺键"时**才会取默认值（`Config` 上有 `#[serde(default)]`）。
    /// 老配置里显式写了 `audio_mic: true` 的用户**保持其选择** —— [`Config::sanitized`]
    /// 只在"两路音源都关"时补开 `audio_system`，**从不**去改 `audio_mic`。
    pub audio_mic: bool,
    /// 麦克风设备 id（WASAPI endpoint id）；None = 默认设备
    pub mic_device_id: Option<String>,

    // 视频编码
    pub fps: u32,
    /// **目标码率**（`iTargetBitrate`），单位 kbps；界面用 Mbps 显示。
    /// 只在 [`VIDEO_BITRATE_MIN_KBPS`]..=[`VIDEO_BITRATE_MAX_KBPS`] 之间取值。
    pub video_bitrate_kbps: u32,
    /// 熵编码开关（`bEntropyCodingModeFlag`）：**默认开**（CABAC，实测省约一半字节且不降速）。
    ///
    /// 这是留给运维/排障的**非 UI 开关**：故意不在设置页暴露，但写进配置文件，
    /// 万一将来某台机器上 CABAC 得不偿失，改一行配置就能回退到 CAVLC，不必改代码。
    /// 它是熵编码的**唯一**入口（P48 之前还有个镜像字段 `entropy`，没人读，已删）。
    pub prefer_cabac: bool,
    /// 0 = 自动（交给 OpenH264 决定周期）。界面不再暴露它，[`Config::sanitized`] 归一为 0；
    /// 只有测试（`recorder_smoke --gop N`）会在 `sanitized()` **之后**直接覆盖它。
    pub keyframe_interval_sec: u32,

    // 音频
    /// AAC 码率（kbps）。**外部没有这一项了**，[`Config::sanitized`] 一律归一为
    /// [`FIXED_AUDIO_BITRATE_KBPS`]（192）。
    pub audio_bitrate_kbps: u32,

    // 托盘 / 全局快捷键
    /// 录制**真正开始**（`Recorder::start` 成功）后是否把窗口隐藏到托盘。默认 **false**。
    ///
    /// 隐私优先：老配置里没有这个键时回落 false，热键/托盘启动的录制不会静默隐藏窗口。
    /// 用户显式开启后才隐藏；关闭时**一个 viewport 命令都不发**。
    pub hide_to_tray_on_record_start: bool,
    /// 是否启用全局快捷键（同一个键切换开始/停止录制）。默认 **true**。
    #[serde(default = "default_true")]
    pub global_hotkey_enabled: bool,
    /// 全局快捷键预设（三选一）：`ctrl_alt_r`（默认）/ `ctrl_shift_r` / `custom`。
    ///
    /// 缺键 → [`HotkeyPreset::default`]（= `ctrl_alt_r`）。
    #[serde(default)]
    pub hotkey_preset: HotkeyPreset,
    /// 自定义快捷键文本（**只在 `hotkey_preset == custom` 时生效**）。
    ///
    /// 格式由 [`crate::tray::parse_hotkey`] 规范化（例如 `"Ctrl+Shift+F9"`）。
    /// 写坏了不会让程序没有热键：[`crate::tray::effective_hotkey`] 会回退到默认组合。
    #[serde(default)]
    pub hotkey_custom: String,

    // 上次路径（便于对话框记忆）
    pub last_video_path: Option<String>,

    // ---- 剪辑页显示项 ----
    /// **剪辑页时间线是否显示音频波形**（默认 **true**）。
    ///
    /// 只影响**剪辑页的显示**：波形在后台解码音轨、算成 min/max 峰值金字塔，
    /// 录制侧（采集/混音/编码/封装）**完全不参与**，关掉它也不影响任何录制行为。
    ///
    /// 缺键兼容：老配置文件里没有这个键 → `default_true` 让它仍为 true
    /// （与 `hide_to_tray_on_record_start` / `global_hotkey_enabled` 同一做法）。
    #[serde(default = "default_true")]
    pub show_waveform: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: default_language().to_string(),
            dark_mode: true,
            output_dir: default_output_dir(),
            config_dir: None,

            capture_mode: CaptureMode::FullScreen,
            monitor_index: 0,
            region: Region::default(),

            content_mode: ContentMode::VideoAndAudio,
            draw_cursor: true,
            audio_system: true,
            audio_mic: false,
            mic_device_id: None,

            fps: DEFAULT_FPS,
            video_bitrate_kbps: DEFAULT_VIDEO_BITRATE_KBPS,
            // P54：原本这里另写一份 `true`，而 `video::encode::ENTROPY_CABAC` 也是 `true` ——
            // 同一个默认值两处定义（P47 因为 `ENTROPY_CABAC` 是 `pub` 所以没报警）。
            // 改为指向唯一来源：改默认熵编码只动 `ENTROPY_CABAC` 一处。
            prefer_cabac: crate::video::encode::ENTROPY_CABAC,
            keyframe_interval_sec: 0,

            audio_bitrate_kbps: FIXED_AUDIO_BITRATE_KBPS,

            // 隐私默认：热键/托盘开始的录制**不隐藏窗口**。用户显式开启后才隐藏。
            hide_to_tray_on_record_start: false,
            global_hotkey_enabled: true,
            hotkey_preset: HotkeyPreset::CtrlAltR,
            hotkey_custom: String::new(),

            last_video_path: None,
            show_waveform: true,
        }
    }
}

impl Config {
    /// 把数值夹紧到合法区间，并丢弃非法区域。
    pub fn sanitized(mut self) -> Self {
        // 帧率**不是夹紧而是吸附**：设置页只有 30/60 两档，手改配置塞进来的
        // 24/45/100000 一律落到最近的一档（平局取 30），免得界面显示出第三档。
        // 顺带保证 `fps != 0`（录制侧 `Recorder::start` 会拒绝 0）。
        self.fps = nearest_fps(self.fps);
        // 目标码率：5–20 Mbps（界面是 Mbps 滑尺，存储是 kbps）
        self.video_bitrate_kbps = self
            .video_bitrate_kbps
            .clamp(VIDEO_BITRATE_MIN_KBPS, VIDEO_BITRATE_MAX_KBPS);
        // 复杂度（固定 Medium）、RC（固定 ABR/Bitrate）、编码线程（固定 auto）、
        // 熵编码（唯一开关是 `prefer_cabac`）**都不在这里归一了** —— P48 之前它们是
        // 只写不读的镜像字段，现在干脆不存在：映射写死在 `video::encode::build_encoder_config`，
        // 老配置文件里残留的 `enc_speed` / `rate_control` / `entropy` / `encoder_threads`
        // 会被 serde **直接忽略**（`Config` 没有 `deny_unknown_fields`），不影响读取。
        // 音频码率：**外部已经没有这一项**，固定 192 kbps（AAC-LC）
        self.audio_bitrate_kbps = FIXED_AUDIO_BITRATE_KBPS;
        // 关键帧间隔：**外部已经没有这一项**了，语义固定为"自动"（0 = 交给 OpenH264）。
        // 只有测试会在 `sanitized()` 之后直接覆盖字段（见 `recorder_smoke --gop`）。
        self.keyframe_interval_sec = 0;
        if !self.region.is_valid() {
            self.region = Region::default();
            if self.capture_mode == CaptureMode::Region {
                self.capture_mode = CaptureMode::FullScreen;
            }
        }
        if !self.audio_system && !self.audio_mic {
            // 音源一个都没选时给一个合理默认，避免"录了但没有声音"。
            //
            // P8 口径：这里**只**补开系统声音，**绝不**顺手打开麦克风 ——
            // 归一化的目的是"至少有一路音源"，不是"替用户决定要不要收环境音"。
            // 所以"用户只要麦克风"（`audio_mic = true`、`audio_system = false`）
            // 不会被这里改成"两路都开"：条件根本不成立。
            self.audio_system = true;
        }
        if self.output_dir.trim().is_empty() {
            self.output_dir = default_output_dir();
        }
        // 输出目录统一成绝对路径：双击 exe 时 CWD 可能是 System32，留着相对路径会写错地方。
        // 这里**不探测可写性**（探测要真建目录/写文件，而 sanitized() 每帧都会被调用），
        // 可写性由启动时的 plan_output_dir 一次性决定。
        self.output_dir = match paths::resolve_against_exe_dir(&self.output_dir) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => self.output_dir.trim().to_string(),
        };
        if self.language.trim().is_empty() {
            self.language = default_language().to_string();
        }
        // 自定义快捷键：**能解析就规范化**（`"ctrl + shift + f9"` → `"Ctrl+Shift+F9"`），
        // 解析不了就原样留着 —— 设置页会据此给出一条可见告警，
        // 而实际注册用的是 `tray::effective_hotkey`（它会回退到默认组合，绝不会"没有热键"）。
        if self.hotkey_preset == HotkeyPreset::Custom {
            if let Ok(hk) = crate::tray::parse_hotkey(&self.hotkey_custom) {
                self.hotkey_custom = hk.label();
            }
        }
        self
    }

    /// 配置里显式指定的配置目录（解析成绝对路径；空/缺失返回 `None`）。
    pub fn explicit_config_dir(&self) -> Option<PathBuf> {
        let raw = self.config_dir.as_deref()?;
        paths::resolve_against_exe_dir(raw).ok()
    }

    /// 读取配置。文件不存在 → 默认值；文件损坏 → 返回错误（调用方应提示用户并回落默认值）。
    ///
    /// 只负责"读"，不负责"挑目录"——挑目录在 [`ConfigStore::detect`] 里做，
    /// 这样既能沿用配置里显式指定的位置，又能在它不可写时退到别处。
    pub fn load_from(path: &Path) -> Result<Self> {
        // 配置是小 JSON；1 MB 上限防止损坏/恶意文件把启动读成 OOM。
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > MAX_CONFIG_BYTES {
                return Err(AppError::Config(format!(
                    "配置文件过大（{} MB，上限 {} MB）：{}",
                    meta.len() / (1024 * 1024),
                    MAX_CONFIG_BYTES / (1024 * 1024),
                    path.display()
                )));
            }
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(AppError::Config(format!(
                    "读取 {} 失败：{e}",
                    path.display()
                )))
            }
        };
        let cfg: Self = serde_json::from_str(&text)
            .map_err(|e| AppError::Config(format!("解析 {} 失败：{e}", path.display())))?;
        Ok(cfg.sanitized())
    }

    /// 原子保存（临时文件 + rename），失败返回错误。
    ///
    /// 关键：**先确认目录真的可写**（创建 + 写探测文件再删掉），失败时错误信息里必须带
    /// 具体路径与系统原因（例如 `拒绝访问。 (os error 5)`），用户才知道该去改哪里。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.save_to_impl(path, true)
    }

    /// 保存的公共实现。
    ///
    /// `probe == true` 保留旧的公开语义（先真写一个探测文件，便于独立调用方
    /// 在没有 [`ConfigStore`] 的情况下得到清晰的错误）；`ConfigStore::save` 走
    /// `probe == false`：目录可用性在启动探测时已经确认过，不该每次保存都
    /// create/write/delete 一个探测文件。
    fn save_to_impl(&self, path: &Path, probe: bool) -> Result<()> {
        let Some(dir) = path.parent() else {
            return Err(AppError::Config(format!(
                "配置路径 {} 没有父目录，无法保存",
                path.display()
            )));
        };
        if probe {
            paths::probe_dir_writable(dir)
                .map_err(|e| AppError::Config(format!("配置目录 {} 不可写：{e}", dir.display())))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| AppError::Config(format!("序列化配置失败：{e}")))?;

        // 临时文件必须落在**同一个目录**，否则 rename 可能变成跨卷拷贝而失去原子性。
        // 名字必须唯一：固定 `config.json.tmp` 在两个实例/两个保存并发时会互相截断。
        // 用 `create_new` 而不是 `write`，避免跟随共享目录里预先放好的链接/硬链接。
        let stem = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.json".to_string());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut tmp: Option<PathBuf> = None;
        let mut last_err: Option<std::io::Error> = None;
        for attempt in 0..16u32 {
            let name = format!("{stem}.{}.{}.{attempt}.tmp", std::process::id(), nanos);
            let candidate = dir.join(name);
            use std::io::Write as _;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(json.as_bytes()) {
                        let _ = std::fs::remove_file(&candidate);
                        return Err(AppError::Config(format!(
                            "写入 {} 失败：{e}",
                            candidate.display()
                        )));
                    }
                    if let Err(e) = f.sync_all() {
                        let _ = std::fs::remove_file(&candidate);
                        return Err(AppError::Config(format!(
                            "同步 {} 失败：{e}",
                            candidate.display()
                        )));
                    }
                    tmp = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => {
                    return Err(AppError::Config(format!(
                        "创建临时配置 {} 失败：{e}",
                        candidate.display()
                    )));
                }
            }
        }
        let Some(tmp) = tmp else {
            return Err(AppError::Config(format!(
                "无法创建唯一临时配置（最后一次错误：{}）",
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "未知".to_string())
            )));
        };

        // 旧文件先留一份 `.bak`：主文件损坏/断电时还有一次恢复机会。
        // 只有**旧文件本身能解析**时才覆盖 bak —— 否则主文件已经损坏时，
        // 这次保存会把唯一一份好的 bak 也踩成坏 JSON。
        if path.is_file() && Config::load_from(path).is_ok() {
            if let Err(e) = refresh_backup(path) {
                tracing::warn!(path = %path.display(), error = %e, "刷新配置备份失败（继续保存主文件）");
            }
        }

        // Windows 上 rename 在杀毒/索引器短暂占用目标时可能失败，做有限重试。
        let mut last_rename_err: Option<std::io::Error> = None;
        for (i, delay) in [0u64, 20, 100, 250].iter().enumerate() {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(*delay));
            }
            match std::fs::rename(&tmp, path) {
                Ok(()) => return Ok(()),
                Err(e) => last_rename_err = Some(e),
            }
        }
        let _ = std::fs::remove_file(&tmp);
        Err(AppError::Config(format!(
            "替换 {} 失败：{}",
            path.display(),
            last_rename_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "未知".to_string())
        )))
    }
}

/// 原子刷新 `.bak`。
///
/// 不能直接 `fs::copy(path, bak)`：如果 `bak` 已被预置成符号链接/junction，
/// copy 会跟随它写到目标文件。这里先写同目录唯一临时文件，再**删除旧目录项**
/// （删除链接只会移除链接本身）并 rename，攻击面只剩同目录竞态，且不会再跟随链接写。
fn refresh_backup(path: &Path) -> Result<(), String> {
    use std::io::Write as _;

    let bak = path.with_extension("json.bak");
    let bytes = std::fs::read(path).map_err(|e| format!("读取主配置失败：{e}"))?;
    let dir = path
        .parent()
        .ok_or_else(|| "配置路径没有父目录".to_string())?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.bak.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        nanos
    ));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写备份临时文件失败：{e}"));
    }
    if let Err(e) = std::fs::remove_file(&bak) {
        if e.kind() != std::io::ErrorKind::NotFound {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("移除旧备份失败：{e}"));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, &bak) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("替换备份失败：{e}"));
    }
    Ok(())
}

/// 配置文件位置的**可用性探测**与回退。
///
/// 为什么需要它：实测（用户机器）里 `%APPDATA%\cap\CapRecorder` 这个目录
/// **连创建都被拒绝**（`拒绝访问。 (os error 5)`），界面就会卡在
/// "保存配置失败：…创建 …\config 失败：拒绝访问"上，别的功能都没法试。
///
/// 这里的做法是：启动时探一次，按 `显式指定 → 用户\Documents\cap → <exe 目录>\cap-recordings
/// → 系统临时目录` 挑一个**确实能写**的目录；把结论（原本想用哪、为什么换了、现在用哪）
/// 作为一条提示显示一次，之后所有保存都走这个目录。一个都写不了时**不阻塞界面**，
/// 改用内存配置运行，只提示一次"设置不会被保留"。
#[derive(Debug, Clone)]
pub struct ConfigStore {
    /// 实际使用的配置文件完整路径
    path: PathBuf,
    /// 是否探测成功过（false = 所有候选目录都不可写，只能纯内存运行）
    writable: bool,
    /// 启动读取失败/超时后进入的只读模式：目录仍然可写，但本次运行不再覆盖原配置。
    read_only: bool,
    /// 需要显示一次的提示（例如"配置目录不可写，已改用 X"）
    pub notice: Option<String>,
}

impl ConfigStore {
    /// 探测配置目录。`explicit` 来自命令行 `--config-dir`（优先级最高）。
    pub fn detect(explicit: Option<PathBuf>) -> Self {
        let deadline = std::time::Instant::now() + CONFIG_DIR_PROBE_BUDGET;
        Self::detect_with(explicit, &|dir: &Path| {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            paths::probe_dir_writable_timeout_with_budget(
                dir,
                left.min(std::time::Duration::from_millis(1500)),
            )
        })
    }

    /// 可注入探测函数的版本，便于单测回退链而不碰真实文件系统。
    pub fn detect_with(
        explicit: Option<PathBuf>,
        probe: &dyn Fn(&Path) -> Result<(), String>,
    ) -> Self {
        // 优先级：命令行 → 配置项 `config_dir` → 环境变量；解析不了就当没指定
        let want = explicit.filter(|p| p.is_absolute()).or_else(|| {
            std::env::var("CAP_CONFIG_DIR")
                .ok()
                .and_then(|v| paths::resolve_against_exe_dir(&v).ok())
        });
        let preferred = paths::preferred_config_dir();
        let safe_probe = |dir: &Path| {
            if crate::sys::has_reparse_ancestor(dir) || !crate::cache::is_plain_dir(dir) {
                return Err(format!(
                    "配置目录或其父级是符号链接/junction：{}",
                    dir.display()
                ));
            }
            probe(dir)
        };
        let plan = paths::choose_config_dir(
            want.as_deref(),
            preferred,
            paths::fallback_config_dirs(),
            &safe_probe,
        );

        if plan.chosen.as_os_str().is_empty() {
            // 一个都写不了：不阻塞界面，改用内存配置，只提示一次
            let reason = plan.error.clone().unwrap_or_else(|| "未知原因".to_string());
            let notice = Some(format!(
                "无法写入任何配置目录（{reason}）；本次运行的设置只保存在内存中，关闭后不会保留。"
            ));
            let path = want.unwrap_or_else(|| PathBuf::from(CONFIG_FILE));
            return Self {
                path,
                writable: false,
                read_only: false,
                notice,
            };
        }

        let path = plan.chosen.join(CONFIG_FILE);
        // 只有真的发生了回退才提示；否则用户会以为程序乱改路径
        let notice = plan.fell_back_from.as_ref().map(|from| {
            format!(
                "配置目录 {} 不可写，已改用 {}（设置会保存在这里）。",
                from.display(),
                plan.chosen.display()
            )
        });
        Self {
            path,
            writable: true,
            read_only: false,
            notice,
        }
    }

    /// 实际使用的配置文件路径（界面可以显示它，方便用户排查）
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 配置目录是否可用；false 时保存必然失败，调用方不要再逐帧报错。
    pub fn writable(&self) -> bool {
        self.writable
    }

    /// 配置读取失败或超时后，禁止本次运行继续覆盖原文件。
    pub fn mark_read_only(&mut self) {
        self.read_only = true;
    }

    /// 读取：文件不存在 → 默认值；文件损坏 → 先尝试 `.bak`，再返回错误。
    ///
    /// **不可写模式绝不读文件**：这时 `path` 只是一个裸文件名（例如 `config.json`），
    /// 从当前工作目录读它会把 System32/启动目录里的无关文件当成配置。
    pub fn load(&self) -> Result<Config> {
        if !self.writable {
            return Ok(Config::default());
        }
        // 读取放到后台线程并设总预算：`std::fs::read_to_string` 本身没有超时，
        // `config_dir` 指到不可达 UNC 时会让启动永久停在第一帧之前。
        let path = self.path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("cfg-load".into())
            .spawn(move || {
                let _ = tx.send(Self::load_blocking(&path));
            });
        if let Err(e) = spawned {
            return Err(AppError::Config(format!(
                "无法启动配置读取线程：{e}（{}）",
                self.path.display()
            )));
        }
        match rx.recv_timeout(CONFIG_READ_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(AppError::Config(format!(
                "读取配置超时（超过 {} 秒）：{}",
                CONFIG_READ_TIMEOUT.as_secs(),
                self.path.display()
            ))),
        }
    }

    /// 实际的文件读取（在 `cfg-load` 线程里执行；不直接暴露给启动路径）。
    fn load_blocking(path: &Path) -> Result<Config> {
        // 主文件不存在但 `.bak` 还在（用户误删/替换失败）：优先恢复备份。
        if !path.is_file() {
            let bak = path.with_extension("json.bak");
            if bak.is_file() {
                match Config::load_from(&bak) {
                    Ok(c) => {
                        tracing::warn!(bak = %bak.display(), "主配置不存在，已从 .bak 恢复");
                        return Ok(c);
                    }
                    Err(e) => {
                        return Err(AppError::Config(format!(
                            "主配置不存在，备份 {} 也无法读取：{e}",
                            bak.display()
                        )));
                    }
                }
            }
            return Ok(Config::default());
        }
        match Config::load_from(path) {
            Ok(c) => Ok(c),
            Err(primary) => {
                // 主文件损坏时给 `.bak` 一次机会；bak 也不可用就把原始错误报上去。
                let bak = path.with_extension("json.bak");
                if !bak.is_file() {
                    return Err(primary);
                }
                match Config::load_from(&bak) {
                    Ok(c) => {
                        tracing::warn!(
                            path = %path.display(),
                            bak = %bak.display(),
                            "主配置损坏，已从 .bak 恢复"
                        );
                        Ok(c)
                    }
                    Err(_) => Err(primary),
                }
            }
        }
    }

    /// 保存到已选定的目录（目录可用性在 `detect` 时已确认，不再每次 probe）。
    pub fn save(&self, cfg: &Config) -> Result<()> {
        if self.read_only {
            return Err(AppError::Config(format!(
                "配置读取失败，本次运行处于只读模式，不覆盖原文件：{}",
                self.path.display()
            )));
        }
        if !self.writable {
            return Err(AppError::Config(format!(
                "配置目录不可写：{}",
                self.path.display()
            )));
        }
        cfg.save_to_impl(&self.path, false)
    }
}

/// 默认界面语言：**英文**（`en-US`）。
///
/// 产品默认面向英文用户；`zh-CN` 仍可在设置页切换，且**已有的配置文件不受影响** ——
/// 这里只在"没有配置文件"或"配置里 `language` 为空"时被调用
/// （[`Config::default`] 与 [`Config::sanitized`]），已有配置里写了 `zh-CN` 会被原样保留。
///
/// 注意：**不再看 `LANG` 环境变量**。之前的实现按 `LANG` 猜语言，
/// 会让"同一份 exe 在不同机器上默认语言不同"，与"新装默认英文"的口径冲突。
pub fn default_language() -> &'static str {
    DEFAULT_LANGUAGE
}

/// 默认语言 id 的**单一来源**（`i18n` 的兜底语言也引用它）。
pub const DEFAULT_LANGUAGE: &str = "en-US";

/// 默认存储位置：优先"视频"目录，其次用户主目录。
///
/// **绝不返回 "."**：双击 exe 时 CWD 可能是 `C:\Windows\System32`，
/// 一个相对路径会把录制文件写到系统目录里去。拿不到任何用户目录时返回空字符串，
/// 由启动时的 [`plan_output_dir`] 走回退链拿到一个绝对路径。
pub fn default_output_dir() -> String {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(v) = dirs.video_dir() {
            return v.to_string_lossy().to_string();
        }
        return dirs.home_dir().to_string_lossy().to_string();
    }
    match std::env::var("USERPROFILE") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主配置损坏/丢失时：有 `.bak` 就恢复，没有 `.bak` 必须把原始错误报上去，
    /// 不能悄悄返回默认配置（否则用户会以为设置被“重置”了）。
    #[test]
    fn corrupt_primary_recovers_from_bak_but_missing_bak_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cap-cfg-bak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::detect(Some(dir.clone()));
        assert!(store.writable());
        let cfg = Config {
            fps: 60,
            ..Config::default()
        };
        store.save(&cfg).expect("first save");
        store.save(&cfg).expect("second save should create .bak");
        let bak = store.path().with_extension("json.bak");
        assert!(bak.is_file(), "第二次保存后应有 .bak");

        std::fs::write(store.path(), b"{ not json").unwrap();
        let loaded = store.load().expect("有 bak 时应恢复");
        assert_eq!(loaded.fps, 60, "恢复的是备份里的值");

        std::fs::remove_file(&bak).unwrap();
        std::fs::write(store.path(), b"{ not json").unwrap();
        assert!(
            store.load().is_err(),
            "没有 bak 时损坏必须报错，不能静默 default"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_store_refuses_to_overwrite_after_load_failure() {
        let dir = std::env::temp_dir().join(format!("cap-cfg-readonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = ConfigStore::detect(Some(dir.clone()));
        let path = store.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"fps\":60}").unwrap();
        store.mark_read_only();
        assert!(store.save(&Config::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"fps\":60}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_primary_with_corrupt_backup_is_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "cap-cfg-bad-bak-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::detect(Some(dir.clone()));
        let primary = store.path().to_path_buf();
        std::fs::create_dir_all(primary.parent().unwrap()).unwrap();
        std::fs::write(primary.with_extension("json.bak"), b"{ broken").unwrap();
        assert!(store.load().is_err(), "损坏的备份不能被静默当成默认配置");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_config_file_is_rejected_before_read() {
        let dir = std::env::temp_dir().join(format!("cap-cfg-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, vec![b'x'; (MAX_CONFIG_BYTES + 1) as usize]).unwrap();
        let err = Config::load_from(&path).expect_err("超大配置必须拒绝");
        assert!(err.to_string().contains("过大"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **P42（默认深色）的缺键兼容**：老配置文件里**没有** `dark_mode` 键时
    /// 必须读出 `true`（= 产品默认主题是深色），整份配置也不许加载失败。
    ///
    /// 这条是"深色为默认"的唯一硬证据：`#[serde(default)]` 只管"缺字段时取
    /// `Config::default()`"，而这个断言亲自走了一遍反序列化。
    /// （P40 时默认还是浅色，P42 用户一句"设置默认主题为黑色"把默认翻了过来 ——
    /// 显式写 `false` 的用户不受影响，见第二条断言。）
    #[test]
    fn p42_dark_mode_defaults_to_true_for_old_configs() {
        // 老配置：既没有 dark_mode，也没有本轮新增的任何键
        let cfg: Config = serde_json::from_str(r#"{"fps": 60, "language": "zh-CN"}"#).unwrap();
        assert!(
            cfg.dark_mode,
            "老配置没写 dark_mode → 必须是深色（P42 默认翻转）"
        );
        // 空对象（连 fps 都没有）同样
        let empty: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert!(empty.dark_mode);
        assert!(Config::default().dark_mode, "新装默认也是深色");
        // 显式写了 false 的要原样保留（`sanitized()` 不许动它：没有"夹紧"可言，
        // 但也不能被顺手归一掉）—— 用户的选择永远赢过默认值
        let off: Config = serde_json::from_str(r#"{"dark_mode": false}"#).unwrap();
        assert!(!off.dark_mode);
        assert!(
            !off.clone().sanitized().dark_mode,
            "sanitized() 必须原样保留用户的选择"
        );
        // 落盘 → 再读回来：往返一致（写的是 false 就读回 false）
        let rt: Config = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!rt.dark_mode, "写出去再读回来必须是浅色");
        // true 也原样往返
        let on: Config = serde_json::from_str(r#"{"dark_mode": true}"#).unwrap();
        assert!(on.clone().sanitized().dark_mode);
    }

    #[test]
    fn missing_fields_fall_back_per_field() {
        // 旧版配置只写了 fps，其它字段应逐项取默认值而不是整份重置
        let cfg: Config = serde_json::from_str(r#"{"fps": 60}"#).unwrap();
        assert_eq!(cfg.fps, 60);
        assert_eq!(cfg.video_bitrate_kbps, Config::default().video_bitrate_kbps);
        assert_eq!(cfg.content_mode, Config::default().content_mode);
    }

    /// **默认语言 = 英文**：新装（没有配置文件）时界面必须是英文。
    ///
    /// 同时守住"不覆盖用户已有选择"这件事：`sanitized()` 只在 `language` 为空时才回填，
    /// 所以配置文件里写了 `zh-CN` 会被原样保留（下面第二条断言）。
    #[test]
    fn default_language_is_english_and_user_choice_wins() {
        assert_eq!(DEFAULT_LANGUAGE, "en-US");
        assert_eq!(default_language(), "en-US");
        assert_eq!(Config::default().language, "en-US", "新装默认必须是英文");

        // 已有配置写了 zh-CN → 原样保留
        let cfg: Config = serde_json::from_str(r#"{"language": "zh-CN"}"#).unwrap();
        assert_eq!(cfg.clone().sanitized().language, "zh-CN");
        // 空/缺失 → 回填默认
        let blank = Config {
            language: String::new(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(blank.language, "en-US");
        let missing: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(missing.sanitized().language, "en-US");
    }

    /// 音频码率与关键帧间隔：**外部已经没有这两项**，任何输入都归一为内部固定值。
    #[test]
    fn audio_bitrate_and_keyframe_interval_are_normalized() {
        for input in [0u32, 32, 64, 192, 320, 512, 9_999] {
            let c = Config {
                audio_bitrate_kbps: input,
                ..Config::default()
            }
            .sanitized();
            assert_eq!(
                c.audio_bitrate_kbps, FIXED_AUDIO_BITRATE_KBPS,
                "音频码率 {input} 应归一为固定 192 kbps"
            );
            assert_eq!(c.audio_bitrate_kbps, 192);
        }
        for input in [0u32, 1, 2, 7, 60] {
            let c = Config {
                keyframe_interval_sec: input,
                ..Config::default()
            }
            .sanitized();
            assert_eq!(
                c.keyframe_interval_sec, 0,
                "关键帧间隔 {input} 应归一为 0（自动）"
            );
        }
        assert_eq!(Config::default().audio_bitrate_kbps, 192, "默认就是 192");
        assert_eq!(Config::default().keyframe_interval_sec, 0, "默认就是自动");
    }

    #[test]
    fn sanitize_clamps_and_fixes() {
        let cfg = Config {
            fps: 100_000,
            video_bitrate_kbps: 0,
            region: Region {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            capture_mode: CaptureMode::Region,
            audio_system: false,
            audio_mic: false,
            ..Config::default()
        };
        let c = cfg.sanitized();
        assert_eq!(c.fps, 60);
        assert_eq!(c.video_bitrate_kbps, 5_000, "码率下限 5 Mbps");
        assert_eq!(
            c.capture_mode,
            CaptureMode::FullScreen,
            "非法区域应退回全屏"
        );
        assert!(c.audio_system, "音源全空时应至少开启系统声音");
    }

    /// "音源全关"绝不能变成"录出一个没有声音的成功文件"：
    /// `sanitized()` 必须把 `audio_system` 打开，让录制侧至少去开一路音源。
    ///
    /// 回归第 8 步（`--audio none --audio-only` → 产物必须有 aac 且音轨包数 > 0）就是这条
    /// 策略的端到端证据。这里补充**正常内容模式**（画面+声音）也必须归一 ——
    /// 之前只覆盖了 `Config::default()`（`audio_system=true`），显式构造"两个都关"的
    /// 用例正是这次回归暴露出来的缺口。
    #[test]
    fn audio_sources_never_both_off() {
        // ① 纯声音模式：两个音源都关 → 归一为系统声音，麦克风保持关闭（只补一个默认）
        let c = Config {
            content_mode: ContentMode::AudioOnly,
            audio_system: false,
            audio_mic: false,
            ..Config::default()
        }
        .sanitized();
        assert!(c.audio_system, "音源全关时 sanitized() 必须打开系统声音");
        assert!(!c.audio_mic, "归一不应顺手打开麦克风");
        assert!(c.content_mode.has_audio(), "内容模式仍要求声音");

        // ② 画面+声音模式：同样不允许"一个音源都没有"
        let c = Config {
            content_mode: ContentMode::VideoAndAudio,
            audio_system: false,
            audio_mic: false,
            ..Config::default()
        }
        .sanitized();
        assert!(
            c.audio_system,
            "画面+声音模式下音源全关也必须归一为系统声音"
        );

        // ③ **只关系统声音、开着麦克风**时不该被"归一"打扰（用户明确只录麦克风）
        let c = Config {
            audio_system: false,
            audio_mic: true,
            ..Config::default()
        }
        .sanitized();
        assert!(!c.audio_system, "用户只要麦克风时不应强行打开系统声音");
        assert!(c.audio_mic);

        // ④ 这是幂等的（sanitized() 会被反复调用）
        let again = c.clone().sanitized();
        assert_eq!(again.audio_system, c.audio_system);
        assert_eq!(again.audio_mic, c.audio_mic);
    }

    /// 目标码率滑尺区间（5–20 Mbps，默认 5）与"外部没有的选项一律归一"。
    #[test]
    fn video_bitrate_is_clamped_to_five_twenty_mbps() {
        for (input, want) in [
            (4_000u32, 5_000u32),
            (25_000, 20_000),
            (14_000, 14_000),
            (0, 5_000),
        ] {
            let c = Config {
                video_bitrate_kbps: input,
                ..Config::default()
            }
            .sanitized();
            assert_eq!(c.video_bitrate_kbps, want, "{input} kbps 应夹到 {want}");
        }
        // 默认值就是 5 Mbps，且处在区间内
        assert_eq!(Config::default().video_bitrate_kbps, 5_000);
        assert_eq!(DEFAULT_VIDEO_BITRATE_KBPS, VIDEO_BITRATE_MIN_KBPS);
    }

    /// **P48：老配置文件里残留的"已删除字段"必须被静默忽略**。
    ///
    /// `enc_speed` / `rate_control` / `entropy` / `encoder_threads` 在 P48 被删掉了
    /// （它们只写不读）。当年留着它们的理由是"旧配置文件仍能反序列化"——**这个理由是错的**：
    /// serde 默认就忽略未知键（`Config` 上没有 `deny_unknown_fields`）。这条测试把这个
    /// 事实钉住：以后谁再加 `deny_unknown_fields`，老用户升级就会直接读不出配置。
    #[test]
    fn p48_removed_legacy_fields_are_ignored_not_rejected() {
        let text = r#"{
            "fps": 60,
            "enc_speed": "Fast",
            "rate_control": "Vbr",
            "entropy": "Cavlc",
            "encoder_threads": 8,
            "prefer_cabac": false
        }"#;
        let cfg: Config = serde_json::from_str(text).expect("带已删字段的老配置必须仍能解析");
        assert_eq!(cfg.fps, 60, "正常字段照旧生效");
        assert!(!cfg.prefer_cabac, "熵编码只剩 prefer_cabac 这一个入口");
        // 关键：`entropy: "Cavlc"` 这种"看起来该生效"的镜像值不会再被读到，
        // 真正决定码流的是 prefer_cabac（这里显式给了 false，语义一致）。
        assert!(
            !cfg.sanitized().prefer_cabac,
            "sanitized() 不该动 prefer_cabac"
        );
    }

    #[test]
    fn fps_is_snapped_to_the_two_supported_choices() {
        // 设置页只有 30 / 60 两档下拉，`sanitized()` 负责把其它值吸附到最近的一档：
        // 平局（45，正好在中间）取 30；0 与超大值也各有归属，绝不留下 0（录制侧会拒绝）。
        for (input, want) in [
            (25u32, 30u32),
            (29, 30),
            (31, 30),
            (45, 30), // 平局取 30
            (50, 60),
            (55, 60),
            (60, 60),
            (0, 30),
            (1, 30),
            (100_000, 60),
        ] {
            assert_eq!(nearest_fps(input), want, "nearest_fps({input}) 应为 {want}");
            let cfg = Config {
                fps: input,
                ..Config::default()
            }
            .sanitized();
            assert_eq!(cfg.fps, want, "fps={input} 经 sanitized() 后应为 {want}");
        }
        // 两档原样保留，且默认值就是 30
        assert_eq!(Config::default().fps, 30);
        assert_eq!(nearest_fps(30), 30);
        assert_eq!(nearest_fps(60), 60);
        assert_eq!(FPS_CHOICES, [30, 60]);
    }

    #[test]
    fn region_even_and_bounds() {
        let r = Region {
            x: -1920,
            y: -100,
            width: 1921,
            height: 1081,
        };
        assert_eq!(r.right(), 1);
        assert_eq!(r.bottom(), 981);
        let e = r.even();
        assert_eq!((e.width, e.height), (1920, 1080));
        assert!(e.is_valid());
    }

    #[test]
    fn config_roundtrip_json() {
        let cfg = Config::default();
        let text = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.fps, cfg.fps);
        assert_eq!(back.region, cfg.region);
        assert_eq!(back.config_dir, cfg.config_dir);
        // 托盘 / 全局快捷键四项也要能原样往返
        assert_eq!(
            back.hide_to_tray_on_record_start,
            cfg.hide_to_tray_on_record_start
        );
        assert_eq!(back.global_hotkey_enabled, cfg.global_hotkey_enabled);
        assert_eq!(back.hotkey_preset, cfg.hotkey_preset);
        assert_eq!(back.hotkey_custom, cfg.hotkey_custom);
    }

    /// **托盘/快捷键默认值**：热键默认开，但录制开始**不**默认隐藏窗口（隐私）。
    #[test]
    fn tray_and_hotkey_defaults_are_private_by_default() {
        let d = Config::default();
        assert!(
            !d.hide_to_tray_on_record_start,
            "缺省 = 录制开始后保持窗口可见（避免静默录屏）"
        );
        assert!(d.global_hotkey_enabled, "缺省 = 启用全局快捷键");
        assert_eq!(d.hotkey_preset, HotkeyPreset::CtrlAltR);
        assert_eq!(d.hotkey_custom, "");
        // 与 tray 侧的"默认组合"一致（单一来源在 tray.rs）
        assert_eq!(
            crate::tray::effective_hotkey(d.hotkey_preset, &d.hotkey_custom).label(),
            crate::tray::DEFAULT_HOTKEY.label()
        );
    }

    /// **老配置缺键兼容**：`{}`（或只有一两个老字段）仍能解析；隐私开关回落为 false，
    /// 热键/波形仍为 true，且**不能 load 失败**。
    #[test]
    fn legacy_config_without_the_new_keys_still_parses() {
        for text in [
            r#"{}"#,
            r#"{"fps": 60}"#,
            r#"{"language": "zh-CN", "output_dir": "C:\\tmp"}"#,
        ] {
            let cfg: Config = serde_json::from_str(text)
                .unwrap_or_else(|e| panic!("老配置 {text} 必须仍能解析：{e}"));
            assert!(
                !cfg.hide_to_tray_on_record_start,
                "{text} 缺 hide_to_tray_on_record_start 时必须回落 false"
            );
            assert!(
                cfg.global_hotkey_enabled,
                "{text} 缺 global_hotkey_enabled 时必须是 true"
            );
            assert_eq!(cfg.hotkey_preset, HotkeyPreset::CtrlAltR, "{text}");
            assert_eq!(cfg.hotkey_custom, "", "{text}");
            assert!(
                cfg.show_waveform,
                "{text} 缺 show_waveform 时必须是 true（波形默认打开）"
            );
            // sanitized() 也不能把这些开关弄丢
            let s = cfg.sanitized();
            assert!(
                !s.hide_to_tray_on_record_start && s.global_hotkey_enabled,
                "{text}"
            );
            assert!(s.show_waveform, "{text}");
        }
    }

    /// **`show_waveform` 的三条口径**：默认 true、缺键兼容为 true、save/load 往返（true/false 各一次）。
    ///
    /// 这个键只影响**剪辑页的显示**（波形计算与绘制），录制侧完全不参与 ——
    /// 但"默认打开"与"缺键不能变成关闭"必须由测试钉住，否则老配置升级后会静默丢失波形。
    #[test]
    fn show_waveform_defaults_on_and_round_trips() {
        assert!(Config::default().show_waveform, "新装默认必须显示波形");
        // `{}` / 只写了别的字段：都必须是 true，且不能解析失败
        for text in [r#"{}"#, r#"{"fps": 60}"#, r#"{"show_waveform": true}"#] {
            let cfg: Config = serde_json::from_str(text).unwrap();
            assert!(cfg.show_waveform, "{text} 应解析出 show_waveform=true");
        }
        // 显式 false 必须被尊重（用户关掉了波形）
        let off: Config = serde_json::from_str(r#"{"show_waveform": false}"#).unwrap();
        assert!(!off.show_waveform);

        // save → load 往返
        let dir = std::env::temp_dir().join("caplite-cfg-waveform-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let _ = std::fs::remove_file(&path);
        for want in [true, false, true] {
            let cfg = Config {
                show_waveform: want,
                ..Config::default()
            };
            cfg.save_to(&path).expect("保存应成功");
            let back = Config::load_from(&path).expect("读回应成功");
            assert_eq!(back.show_waveform, want, "show_waveform 往返回不来");
        }
        // 关掉波形**不影响**任何录制侧的字段（这条把"只在剪辑页用"钉成事实）
        let off = Config {
            show_waveform: false,
            ..Config::default()
        }
        .sanitized();
        let on = Config::default().sanitized();
        assert_eq!(off.fps, on.fps);
        assert_eq!(off.video_bitrate_kbps, on.video_bitrate_kbps);
        assert_eq!(off.audio_system, on.audio_system);
        assert_eq!(off.audio_bitrate_kbps, on.audio_bitrate_kbps);
        assert_eq!(off.content_mode, on.content_mode);
        let _ = std::fs::remove_file(&path);
    }

    /// **save → load 往返（true / false 各一次）**：真的落盘再读回来。
    #[test]
    fn save_load_roundtrip_for_the_two_switches() {
        let dir = std::env::temp_dir().join("caplite-cfg-tray-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let _ = std::fs::remove_file(&path);

        for (hide, hotkey, preset, custom) in [
            (true, true, HotkeyPreset::CtrlAltR, ""),
            (false, false, HotkeyPreset::Custom, "Ctrl+Shift+F9"),
            (false, true, HotkeyPreset::CtrlShiftR, "忽略我"),
            (true, false, HotkeyPreset::Custom, "Ctrl+Alt+F9"),
        ] {
            let cfg = Config {
                hide_to_tray_on_record_start: hide,
                global_hotkey_enabled: hotkey,
                hotkey_preset: preset,
                hotkey_custom: custom.to_string(),
                ..Config::default()
            };
            cfg.save_to(&path).expect("保存应成功");
            let back = Config::load_from(&path).expect("读回应成功");
            assert_eq!(back.hide_to_tray_on_record_start, hide, "hide 往返回不来");
            assert_eq!(back.global_hotkey_enabled, hotkey, "hotkey 开关往返回不来");
            assert_eq!(back.hotkey_preset, preset);
            if preset == HotkeyPreset::Custom {
                assert_eq!(back.hotkey_custom, custom, "自定义文本往返回不来");
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// 自定义快捷键文本在 `sanitized()` 里被**规范化**；写坏了原样保留（由界面提示）。
    #[test]
    fn custom_hotkey_is_normalized_but_never_dropped() {
        let c = Config {
            hotkey_preset: HotkeyPreset::Custom,
            hotkey_custom: "  ctrl + shift + f9 ".into(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(c.hotkey_custom, "Ctrl+Shift+F9");

        let bad = Config {
            hotkey_preset: HotkeyPreset::Custom,
            hotkey_custom: "R".into(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(bad.hotkey_custom, "R", "非法文本原样保留，交给界面提示");
        assert_eq!(
            crate::tray::effective_hotkey(bad.hotkey_preset, &bad.hotkey_custom).label(),
            "Ctrl+Alt+R",
            "非法自定义的实际注册组合必须回退到默认"
        );

        // 不是自定义时，文本不参与任何计算，也不该被动过
        let preset = Config {
            hotkey_preset: HotkeyPreset::CtrlShiftR,
            hotkey_custom: "乱写的".into(),
            ..Config::default()
        }
        .sanitized();
        assert_eq!(preset.hotkey_custom, "乱写的");
        assert_eq!(
            crate::tray::effective_hotkey(preset.hotkey_preset, &preset.hotkey_custom).label(),
            "Ctrl+Shift+R"
        );
    }

    #[test]
    fn empty_or_relative_output_dir_is_never_left_relative() {
        // 手改配置成空目录/相对路径时：sanitized() 必须给回一个绝对路径，
        // 否则双击 exe（CWD=System32）会把录制文件写到系统目录里
        let c = Config {
            output_dir: "  ".into(),
            ..Config::default()
        }
        .sanitized();
        assert!(!c.output_dir.trim().is_empty(), "空目录应回落到默认位置");
        assert!(
            PathBuf::from(&c.output_dir).is_absolute(),
            "{}",
            c.output_dir
        );

        let c = Config {
            output_dir: "cap-out".into(),
            ..Config::default()
        }
        .sanitized();
        assert!(
            PathBuf::from(&c.output_dir).is_absolute(),
            "{}",
            c.output_dir
        );
        assert!(c.output_dir.ends_with("cap-out"), "{}", c.output_dir);
    }

    #[test]
    fn explicit_config_dir_must_be_absolute_or_none() {
        // 配置项 config_dir 缺失/空白 → None（走默认首选目录）
        let c = Config {
            config_dir: None,
            ..Config::default()
        };
        assert!(c.explicit_config_dir().is_none());
        let c = Config {
            config_dir: Some("   ".into()),
            ..Config::default()
        };
        assert!(c.explicit_config_dir().is_none(), "空白项不能被当成目录");

        // 绝对路径原样返回；相对路径以 exe 目录为基准解析成绝对路径
        let c = Config {
            config_dir: Some(r"C:\Users\Limit li\Documents\cap".into()),
            ..Config::default()
        };
        assert_eq!(
            c.explicit_config_dir().as_deref(),
            Some(Path::new(r"C:\Users\Limit li\Documents\cap"))
        );
        let c = Config {
            config_dir: Some("portable-cfg".into()),
            ..Config::default()
        };
        let resolved = c.explicit_config_dir().expect("相对路径也应能解析");
        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert!(resolved.ends_with("portable-cfg"));
    }
}
