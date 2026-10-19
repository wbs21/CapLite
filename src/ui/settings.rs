//! 设置页：**界面语言 + 深色模式**、托盘与全局快捷键、剪辑页显示项、视频编码参数。
//!
//! **P40 起四个分区标题（「常规」「剪辑」「视频」…）全部按用户要求去掉**：
//! 卡片直接以内容开头，靠卡片本身的边界分组（信息不丢，只是不再重复占一行）。
//!
//! 约定：
//! * 数值先写进 `Config`，最后统一 `sanitized()` 再交给外壳落盘，避免非法值
//!   （例如手填空目录、滑条语义外的数）跑进录制线程；
//! * **调整即保存**：任何改动立刻进内存并上报外壳，由外壳**节流落盘**（P45：最多 200ms 一次，
//!   拖滑条时不再每帧写盘）；落盘失败走外壳底部栏红条；
//! * 语言切换在**画完这一帧之后**才真正生效：`I18n::set_language` 需要 `&mut`，
//!   而这一帧的文案已经取完了，下一帧自然就是新语言；
//! * **没有"存储位置"这一块**：录制页已经有"保存位置"的选择与可用性提示，设置页再来一份属于重复；
//! * **没有"音频"这一块**（用户要求删除）：音源（系统声音/麦克风）与麦克风设备都在**录制页**选，
//!   那才是实际生效的地方；`Config::audio_system` / `audio_mic` / `mic_device_id` 与内核逻辑不受影响。
//!   音频码率是内部固定值（192 kbps，见 `config::FIXED_AUDIO_BITRATE_KBPS`），界面上不出现。
//! * 帧率提供 **30 / 60** 两档（滑尺两端，默认 30）：`Config::sanitized()` 会把
//!   其它值吸附到最近的档位，所以运行期只会见到这两档。

use crate::config::Config;
use crate::i18n::I18n;
use crate::tray::{
    effective_hotkey, parse_hotkey, HotkeyParseError, HotkeyPreset, PRESET_CTRL_ALT_R,
    PRESET_CTRL_SHIFT_R,
};

use eframe::egui::{self, RichText};

use super::theme;

/// "左标签 / 右控件"对齐时标签列的宽度
const LABEL_W: f32 = 104.0;

/// 设置页画完后要告诉外壳的事
#[derive(Default)]
pub struct SettingsOutcome {
    /// 配置被改过（外壳负责 `sanitized()` + `save()`）
    pub cfg_changed: bool,
    /// 保存失败之类的**可见错误**：交给外壳走底部栏红条（页面上不再画红字）。
    ///
    /// P45 起落盘统一由外壳做（本页不再自己 `store.save`），所以**本页不会再产生**
    /// 这一类错误；字段保留是因为它是"页面 → 外壳"的通用错误通道（录制页也用它）。
    pub error: Option<String>,
    /// 全局快捷键相关配置（开关 / 预设 / 自定义文本）被改过：
    /// 外壳负责 `UnregisterHotKey` 旧的 → 注册新的（失败会回滚 + 出可见提示）。
    pub hotkey_changed: bool,
    /// **正在捕获自定义快捷键**：外壳必须临时 `UnregisterHotKey` 当前热键，
    /// 否则用户按下的正是当前组合 —— 那会直接触发开始/停止录制（本需求的坑）。
    pub hotkey_capturing: bool,
}

pub struct SettingsPage {
    /// 是否正在捕获自定义快捷键（见 [`SettingsOutcome::hotkey_capturing`]）
    capturing: bool,
    /// 捕获时的即时报错（例如"没有修饰键"）；下一次成功捕获或取消时清空
    capture_error: Option<String>,
    /// **仅测试**：最近一帧两个数值框的矩形 `(目标码率的可编辑框, 帧率的只读框)`。
    /// P26 的断言要拿"文本矩形中心"与"框中心"比，所以必须能读到**真实渲染出来的**框矩形
    /// （不是我们算出来的期望值）。
    #[cfg(test)]
    test_value_boxes: (Option<egui::Rect>, Option<egui::Rect>),
}

impl Default for SettingsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsPage {
    pub fn new() -> Self {
        Self {
            capturing: false,
            capture_error: None,
            #[cfg(test)]
            test_value_boxes: (None, None),
        }
    }

    /// 停止捕获（切走标签页时由外壳调用）：见 [`Self::ui`] 的说明。
    pub fn stop_capture(&mut self) {
        self.capturing = false;
        self.capture_error = None;
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        cfg: &mut Config,
        i18n: &mut I18n,
        // 正在录制（Recording/Stopping）：此时设置项**全部置灰**（本次录制参数已快照）
        recording: bool,
    ) -> SettingsOutcome {
        let mut out = SettingsOutcome::default();
        let mut changed = false;
        // 全局快捷键相关配置被改过（走 `Unregister` → `Register` 那条边沿，不是每帧重注册）
        let mut hotkey_changed = false;
        // 捕获状态只在本页可见时有效：切走标签页由外壳调 `stop_capture()`，
        // 这一帧的 `out.hotkey_capturing` 就是外壳判断"要不要临时注销热键"的唯一依据。
        out.hotkey_capturing = self.capturing;
        // 语言切换在"画完"之后处理（NLL 之后才需要 &mut I18n）。
        // 注意：**已经没有"保存设置"按钮**了 —— 任何改动都会立刻 sanitized + 落盘
        //（用户要求"调整即保存"），所以这里也不再需要 `save` 标记。
        let mut new_lang: Option<String> = None;
        let langs = i18n.available();
        // 录制中：所有设置项**置灰禁用**（本次录制的参数在开始时就已快照，改了也不生效，
        // 与其让用户改了个没用的值，不如直接禁掉 + 一行说明）
        let locked = recording;
        // 录制中不允许捕获（控件已经置灰，这里再兜一层：捕获态不能在录制中生效）
        if locked {
            self.capturing = false;
            self.capture_error = None;
            out.hotkey_capturing = false;
        }

        // 只读别名：语言切换在函数末尾，NLL 之后才需要 &mut I18n
        let texts: &I18n = i18n;
        let t = |k: &str| texts.t(k).to_string();

        // 页面标题「设置」、小字「改动会立即保存」、只读的「当前配置目录」都按用户要求删除：
        // 与标签栏重复、且配置目录已在底部状态栏常驻显示（信息不丢，只是不再重复占版面）。
        //
        // **纵向滚动条永远不显示**（用户要求"取消上下滚动条"，三页一致）：
        // `AlwaysHidden` 连"内容溢出时才出现"的那条也不画。这一页在最小尺寸
        //（`ui::layout::MIN_INNER_W × MIN_INNER_H`）下装得下 —— P40 删掉三个分区标题后
        // 更宽裕；高度由 `ui::layout` 那一族的探针单测盯着（见该文件顶部的高度口径）。
        // 这条设置只是与另外两页保持同一种口径：**滚动条永远不会出现在本程序里**。
        egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .show(ui, |ui| {
            if locked {
                // 一行提示（不是长句）：录制中不可调整
                ui.label(
                    RichText::new(t("recording_locked"))
                        .size(theme::FS_SMALL)
                        .color(theme::warn_fg()),
                );
                ui.add_space(theme::GAP_ROW);
            }
            // 录制中把整页控件包进 `add_enabled_ui(false, ..)`：一条指令禁用全部子控件，
            // 不用在每个控件上各写一遍（新加控件也不会漏）。
            let enabled = !locked;
            ui.add_enabled_ui(enabled, |ui| {

            // ---------------- 语言 + 深色模式（同一行） ----------------
            //
            // 深色模式与语言**同一行**、排在语言选项**右侧**（P40 修订，用户原话"放到界面语言
            // 同一行右侧"）：两者都是"换一套外观"，挨在一起才找得到；原先独占一行，
            // 既把页面顶高、又在那行下面留一大块空白。
            //
            // 勾选即换肤：`theme::apply_kind` 当场把两套 egui 样式 + 线程局部主题都换掉，
            // **同一帧**剩下的控件就按新主题画出来；`cfg.dark_mode` 跟着改，
            // 由调用方在帧末统一落盘（"调整即保存"）。
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    // 界面语言 + 下拉框（与其它行的"标签 + 控件"同一套对齐）
                    theme::field_row(ui, &t("lang_label"), LABEL_W, |ui| {
                        let mut lang = cfg.language.clone();
                        egui::ComboBox::from_id_salt("set_lang")
                            .width(200.0)
                            .selected_text(lang.clone())
                            .show_ui(ui, |ui| {
                                for id in &langs {
                                    ui.selectable_value(&mut lang, id.clone(), id.clone());
                                }
                            });
                        if lang != cfg.language {
                            new_lang = Some(lang);
                        }
                    });
                    // 深色模式紧跟在下拉框右侧（间距比行内 spacer 大一档，免得读成同一个控件）
                    ui.add_space(theme::GAP_SECTION);
                    let mut dark = cfg.dark_mode;
                    if ui
                        .checkbox(&mut dark, t("dark_mode"))
                        .on_hover_text(t("dark_mode_tip"))
                        .changed()
                    {
                        cfg.dark_mode = dark;
                        changed = true;
                        // 立刻换肤（不等到下一帧）：剩余控件这一帧就按新主题画。
                        // 录制中整页已置灰（`add_enabled_ui`），走到这里说明没在录制。
                        let ctx = ui.ctx().clone();
                        theme::apply_kind(&ctx, crate::ui::theme::ThemeKind::from_dark_mode(dark));
                    }
                });
            });
            ui.add_space(theme::GAP_CARD);

            // ---------------- 常规：托盘 + 全局快捷键 ----------------
            //
            // 这一块管两件事，都是"录制之外"的行为：
            // * 录制**真正开始**后把窗口隐藏到通知区域（默认开）；
            // * 全局快捷键：同一个键切换开始/停止，三选一（默认 `Ctrl+Alt+R`）。
            //
            // 交互上的两个硬要求（见 `tray` 模块注释）：
            // ① **捕获自定义组合时必须先注销当前热键**，否则用户按下的就是当前组合，
            //    Windows 会把按键吃掉并直接触发开始/停止录制（而不是送进输入框）；
            // ② 录制中整块**置灰**（`add_enabled_ui(false, ..)` 已经包住这一页）。
            theme::card(ui, |ui| {

                // ① 录制开始后隐藏到托盘（用户要求：不加多余说明文字，只有这一句）
                changed |= ui
                    .checkbox(
                        &mut cfg.hide_to_tray_on_record_start,
                        t("set_hide_to_tray"),
                    )
                    .changed();
                ui.add_space(8.0);

                // ② 启用全局快捷键（默认开）+ 三选一 preset（P40 修订）
                //
                // **「全局快捷键」那个标签按用户要求去掉**，三个选项直接跟在开关
                // **同一行的右侧**；选「自定义」时捕获框跟在选项后面（同一行，
                // 不再单独占一行、也不再在左边补一块等宽空白）。
                // 关掉开关时整组置灰，但**位置不变**（`add_enabled_ui` 只改透明度）。
                ui.horizontal(|ui| {
                    changed |= ui
                        .checkbox(&mut cfg.global_hotkey_enabled, t("set_global_hotkey"))
                        .changed();
                    ui.add_space(theme::GAP_ROW);
                    ui.add_enabled_ui(cfg.global_hotkey_enabled, |ui| {
                        let mut preset = cfg.hotkey_preset;
                        // 预设的组合名来自 `tray` 里的常量（**单一来源**），不在这里重写
                        for (value, label) in [
                            (HotkeyPreset::CtrlAltR, PRESET_CTRL_ALT_R.label()),
                            (HotkeyPreset::CtrlShiftR, PRESET_CTRL_SHIFT_R.label()),
                            (HotkeyPreset::Custom, t("hotkey_preset_custom")),
                        ] {
                            if ui.radio_value(&mut preset, value, label).changed() {
                                cfg.hotkey_preset = value;
                                changed = true;
                                hotkey_changed = true;
                                // 切到自定义就把捕获框直接摆出来（少点一次）
                                self.capturing = value == HotkeyPreset::Custom;
                                self.capture_error = None;
                            }
                        }

                        // 自定义：捕获控件（点一下进入捕获态 → 按组合键 → 规范化显示）
                        if cfg.hotkey_preset == HotkeyPreset::Custom {
                            ui.add_space(theme::GAP_ROW);
                            let idle_text = if cfg.hotkey_custom.trim().is_empty() {
                                t("hotkey_capture_idle")
                            } else {
                                cfg.hotkey_custom.clone()
                            };
                            let button_text = if self.capturing {
                                t("hotkey_capture_prompt")
                            } else {
                                idle_text
                            };
                            if ui
                                .add_sized(
                                    egui::vec2(theme::SETTINGS_SLIDER_W + 60.0, theme::H_CONTROL),
                                    egui::Button::new(
                                        RichText::new(button_text).size(theme::FS_BODY),
                                    ),
                                )
                                .clicked()
                            {
                                self.capturing = true;
                                self.capture_error = None;
                            }
                            if self.capturing {
                                if let Some(hk) = capture_combo(ui) {
                                    match hk {
                                        Ok(text) => {
                                            cfg.hotkey_custom = text;
                                            changed = true;
                                            hotkey_changed = true;
                                            self.capturing = false;
                                            self.capture_error = None;
                                        }
                                        Err(CaptureOutcome::Cancelled) => {
                                            self.capturing = false;
                                            self.capture_error = None;
                                        }
                                        Err(CaptureOutcome::Rejected(err)) => {
                                            self.capture_error = Some(parse_error_text(
                                                texts,
                                                &err,
                                                &t,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    });
                });
                // 捕获失败的提示：单独一行（`banner` 是通栏部件，不能挤在选项那一行里）。
                // 只有真的捕获过才可能出现，且切换预设时会被清掉。
                if let Some(e) = &self.capture_error {
                    ui.add_space(4.0);
                    theme::banner(ui, theme::Banner::Warn, e);
                }

                // ④ 配置里存着的自定义文本**不可用**时的常驻告警（手改配置 / 老配置带进来的）。
                //    实际注册用的 `effective_hotkey` 会回退到默认组合，所以这是"提醒"而不是"故障"。
                if cfg.global_hotkey_enabled
                    && cfg.hotkey_preset == HotkeyPreset::Custom
                    && !cfg.hotkey_custom.trim().is_empty()
                {
                    if let Err(e) = parse_hotkey(&cfg.hotkey_custom) {
                        ui.add_space(6.0);
                        let reason = parse_error_text(texts, &e, &t);
                        let fallback = effective_hotkey(cfg.hotkey_preset, "").label();
                        theme::banner(
                            ui,
                            theme::Banner::Warn,
                            &texts.tf("hotkey_invalid_fallback", &[&reason, &fallback]),
                        );
                    }
                }
            });
            ui.add_space(theme::GAP_CARD);

            // ---------------- 剪辑显示 ----------------
            //
            // **不放在上面那张托盘/热键卡片里**：那一块讲的是"录制之外的行为"，
            // 而这一项的语义是"**剪辑页怎么显示**"，混在一起会让用户以为它跟录制有关。
            // 它也**不参与录制**：波形只在剪辑页打开文件后于后台线程计算（见 `edit::waveform`）。
            theme::card(ui, |ui| {
                changed |= ui
                    .checkbox(&mut cfg.show_waveform, t("set_show_waveform"))
                    .on_hover_text(t("waveform_tip"))
                    .changed();
            });
            ui.add_space(theme::GAP_CARD);

            // ---------------- 视频编码 ----------------
            //
            // 只留用户真正需要决定的：目标码率、帧率、关键帧间隔。
            // 别的（RC=ABR、复杂度=平衡、最大码率=20 Mbps、线程=auto、熵编码=CABAC）
            // 都是**内部固定**的，界面上不再提供选择项；下面那行只读说明负责交代清楚，
            // 免得用户以为"选项丢了"。
            theme::card(ui, |ui| {

                // **两列网格**（用户要求：两行数值左边缘严格对齐）：
                // 左列 = 标签（固定宽度），右列 = 滑轨(固定宽度) + 数值框(固定宽度)。
                // 两行用同一套宽高常量，所以滑轨端点、数值框左边缘、字号都对齐。
                egui::Grid::new("set_video_grid")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        // ---- 目标码率 ----
                        ui.label(
                            RichText::new(t("video_bitrate"))
                                .size(theme::FS_BODY)
                                .color(theme::muted()),
                        );
                        ui.horizontal(|ui| {
                            changed |= ui
                                .add_sized(
                                    [theme::SETTINGS_SLIDER_W, theme::H_CONTROL],
                                    egui::Slider::new(
                                        &mut cfg.video_bitrate_kbps,
                                        crate::config::VIDEO_BITRATE_MIN_KBPS
                                            ..=crate::config::VIDEO_BITRATE_MAX_KBPS,
                                    )
                                    .step_by(100.0)
                                    .show_value(false),
                                )
                                .changed();
                            // 数值框：与帧率那一行**同尺寸、同容器、同文本居中口径**（P26：
                            // 尺寸只有一个来源 `theme::number_box_size()`，见那里的说明）。
                            // 显示 Mbps（存储仍是 kbps）；手输 "7.5" 按 Mbps、"7500" 按 kbps。
                            let bitrate_box = ui
                                .add_sized(
                                    theme::number_box_size(),
                                    egui::DragValue::new(&mut cfg.video_bitrate_kbps)
                                        .speed(100.0)
                                        .update_while_editing(false)
                                        .custom_formatter(|v, _| {
                                            format!("{:.1} Mbps", v / 1000.0)
                                        })
                                        .custom_parser(|s| {
                                            let text = s.trim().trim_end_matches("Mbps").trim();
                                            text.parse::<f64>().ok().map(|v| {
                                                if v <= 100.0 {
                                                    v * 1000.0
                                                } else {
                                                    v
                                                }
                                            })
                                        }),
                                );
                            changed |= bitrate_box.changed();
                            #[cfg(test)]
                            {
                                self.test_value_boxes.0 = Some(bitrate_box.rect);
                            }
                        });
                        ui.end_row();

                        // ---- 帧率（两档）----
                        ui.label(
                            RichText::new(t("fps"))
                                .size(theme::FS_BODY)
                                .color(theme::muted()),
                        );
                        ui.horizontal(|ui| {
                            // 帧率：**滑尺两档**（滑到左边 30、滑到右边 60）。
                            // `step_by(30)` 让拖动/点击/方向键都只能停在 30 与 60 上，不会出现 31…59；
                            // 旧配置里带进来的 25/50 之类由 `Config::sanitized()` 吸附到最近档位（平局取 30）。
                            changed |= ui
                                .add_sized(
                                    [theme::SETTINGS_SLIDER_W, theme::H_CONTROL],
                                    egui::Slider::new(&mut cfg.fps, 30..=60)
                                        .step_by(30.0)
                                        .show_value(false),
                                )
                                .changed();
                            // 数值框（P26）：与码率那一行**同一个容器与对齐** ——
                            // 尺寸来自 `theme::number_box_size()`（唯一来源），
                            // 框与文本居中口径来自 `theme::read_only_value_box`（egui 的按钮样式
                            // + `add_sized` 的居中布局，与可编辑的 `DragValue` **同一套实现**），
                            // 所以两个框的外观与文本落位不可能再跑偏。
                            //
                            // 但这里**不是** `DragValue`（P11）：帧率只有 30 / 60 两档，
                            // 完全由上面那把滑尺决定，用 `DragValue` 呈现时它的数字会以
                            // "被选中的样子"出现（强调色高亮底），看着像个能输入的框，
                            // 而实际上打字无效 —— 用户明确要求改成**只读样式**。
                            // 只读框不可聚焦、无光标、无选中高亮、不进 Tab 焦点链，
                            // 于是"这列两个框长得一样、但只有码率那个能改"一眼可见。
                            let fps30 = t("fps_30");
                            let fps60 = t("fps_60");
                            let unit = t("unit_fps");
                            let fps_text = match cfg.fps {
                                30 => fps30,
                                60 => fps60,
                                other => format!("{other} {unit}"),
                            };
                            let fps_box = theme::read_only_value_box(ui, &fps_text);
                            // 非测试构建里这个矩形没有读者（只有 P26 的单测要拿它量"文本是否居中"），
                            // 显式读一下，免得留一条 `unused variable` 告警
                            let _ = &fps_box;
                            #[cfg(test)]
                            {
                                self.test_value_boxes.1 = Some(fps_box.rect);
                            }
                        });
                        ui.end_row();
                    });
                // 关键帧间隔**不再暴露**：内部自动（见 `video::encode` 与 DESIGN.md §10.7）。
                // 那一行"固定值"只读说明也按用户要求删掉了 —— 实测数字留在文档里，不上界面。
            });


            // ---------------- 提示 ----------------
            // 「恢复默认设置」与「保存设置」两个按钮都已按用户要求删除：
            // 现在是**调整即保存**（任何改动 → sanitized → 外壳节流落盘），没有"保存"这个动作，
            // 也就不再需要"已保存"这类提示（用户明确说不喜欢噪音）。
            // 保存**失败**仍然是可见错误，由外壳统一给出（`main.rs::commit_cfg` 的 notice 通道），
            // 本页不再自己存、也不自己报（P45：落盘只有一条路）。
            });
        });

        // ---------------- 画完之后才动状态 ----------------
        if changed {
            // 所有数值改动统一在这里夹紧，避免非法值进入录制线程
            *cfg = cfg.clone().sanitized();
            // **这里不再自己落盘**（P45）：设置页原来在这里 `store.save(cfg)`，
            // 而外壳（`main.rs`）拿到 `cfg_changed` 之后又会 `commit_cfg()` 存一次 ——
            // 拖一下滑条就是**每帧两次**"序列化 + 临时文件 + 重命名"。
            // 现在落盘只有一条路（外壳的 `commit_cfg`，带 200ms 节流），
            // 本页只负责"改内存 + 上报"，保存失败的提示也由外壳那条通道统一给出。
        }
        // 语言切换：立即生效（下一帧的文案就是新语言），并由外壳保存
        if let Some(lang) = new_lang {
            if i18n.set_language(&lang) {
                cfg.language = i18n.language().to_string();
                changed = true;
                ui.ctx().request_repaint();
            }
        }

        out.cfg_changed = changed;
        // 捕获状态以"画完之后"的值为准（本帧可能刚进来或被按 Esc 退出）
        out.hotkey_capturing = self.capturing && !locked;
        out.hotkey_changed = hotkey_changed;
        out
    }
}

/// 捕获控件的三种结局。
enum CaptureOutcome {
    /// 用户按了 Esc
    Cancelled,
    /// 按下的组合不合法（校验规则在 `crate::tray::parse_hotkey`，这里只转述）
    Rejected(HotkeyParseError),
}

/// 从本帧的输入事件里抓一个"组合键"。
///
/// **只在 `capturing` 时被调用**（调用方负责），且此时外壳已经临时注销了全局热键 ——
/// 否则按下的组合会被 Windows 吃掉、根本到不了 egui。
///
/// 只用 `Event::Key`（不是 `keys_down`）：要的是"按下的那一下"。
/// `Text` 事件不参与（按住修饰键时 egui 不会产生 Text）。
fn capture_combo(ui: &mut egui::Ui) -> Option<Result<String, CaptureOutcome>> {
    let mut outcome = None;
    ui.input(|i| {
        for event in &i.events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if *key == egui::Key::Escape {
                outcome = Some(Err(CaptureOutcome::Cancelled));
                break;
            }
            // 只有"我们认识的非修饰键"才算按下；其余按键忽略，继续等
            let Some(name) = egui_key_name(*key) else {
                continue;
            };
            let text = combo_text(modifiers, name);
            outcome = Some(match parse_hotkey(&text) {
                Ok(hk) => Ok(hk.label()),
                Err(e) => Err(CaptureOutcome::Rejected(e)),
            });
            break;
        }
    });
    outcome
}

/// 本帧按下的组合 → 待解析文本（`Ctrl+Shift+F9`）。
///
/// 修饰键顺序固定（Ctrl→Alt→Shift），所以同一个组合每次得到的文本都一致；
/// **Win 键拿不到**：egui 的 `Modifiers` 只有 `alt/ctrl/shift/mac_cmd/command`
///（没有 Windows 键），所以 Win 开头的自定义组合需要在配置文件里手写
///（`hotkey_custom: "Win+R"`），解析与注册都支持它。
fn combo_text(modifiers: &egui::Modifiers, key_name: &str) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if modifiers.ctrl {
        parts.push("Ctrl");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    parts.push(key_name);
    parts.join("+")
}

/// egui 的按键 → [`crate::tray::parse_hotkey`] 认识的名字（不认识就返回 `None`）。
fn egui_key_name(key: egui::Key) -> Option<&'static str> {
    use egui::Key as K;
    Some(match key {
        K::A => "A", K::B => "B", K::C => "C", K::D => "D", K::E => "E", K::F => "F",
        K::G => "G", K::H => "H", K::I => "I", K::J => "J", K::K => "K", K::L => "L",
        K::M => "M", K::N => "N", K::O => "O", K::P => "P", K::Q => "Q", K::R => "R",
        K::S => "S", K::T => "T", K::U => "U", K::V => "V", K::W => "W", K::X => "X",
        K::Y => "Y", K::Z => "Z",
        K::Num0 => "0", K::Num1 => "1", K::Num2 => "2", K::Num3 => "3", K::Num4 => "4",
        K::Num5 => "5", K::Num6 => "6", K::Num7 => "7", K::Num8 => "8", K::Num9 => "9",
        K::F1 => "F1", K::F2 => "F2", K::F3 => "F3", K::F4 => "F4", K::F5 => "F5",
        K::F6 => "F6", K::F7 => "F7", K::F8 => "F8", K::F9 => "F9", K::F10 => "F10",
        K::F11 => "F11", K::F12 => "F12", K::F13 => "F13", K::F14 => "F14", K::F15 => "F15",
        K::F16 => "F16", K::F17 => "F17", K::F18 => "F18", K::F19 => "F19", K::F20 => "F20",
        K::F21 => "F21", K::F22 => "F22", K::F23 => "F23", K::F24 => "F24",
        K::Tab => "Tab", K::Enter => "Enter", K::Space => "Space", K::Insert => "Insert",
        K::Delete => "Delete", K::Home => "Home", K::End => "End", K::PageUp => "PageUp",
        K::PageDown => "PageDown", K::ArrowUp => "Up", K::ArrowDown => "Down",
        K::ArrowLeft => "Left", K::ArrowRight => "Right", K::Backspace => "Backspace",
        _ => return None,
    })
}

/// 解析失败 → 走 i18n 的可见文案（键名在 `locales/*.json` 里，中英各一份）。
fn parse_error_text(texts: &I18n, err: &HotkeyParseError, t: &dyn Fn(&str) -> String) -> String {
    match err {
        HotkeyParseError::NoModifier => t("hotkey_err_no_modifier"),
        HotkeyParseError::NoKey => t("hotkey_err_no_key"),
        HotkeyParseError::Reserved(hk) => texts.tf("hotkey_err_reserved", &[&hk.label()]),
        HotkeyParseError::Malformed(token) => texts.tf("hotkey_err_malformed", &[&token]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // P45：设置页不再自己落盘，但**测试台**要模拟外壳那一步（`cfg_changed` → `store.save`），
    // 所以这里显式引入（生产代码里已经不需要它了）。
    use crate::config::ConfigStore;

    #[test]
    fn outcome_defaults_to_no_change() {
        let out = SettingsOutcome::default();
        assert!(!out.cfg_changed);
    }

    #[test]
    fn sanitize_is_applied_before_save_pipeline() {
        // 设置页末尾统一走 sanitized()：这里固化住"非法值/已固定的参数一定会被归一"这个约定
        let cfg = Config {
            fps: 9_999,
            audio_bitrate_kbps: 1,
            ..Config::default()
        }
        .sanitized();
        assert_eq!(cfg.fps, 60);
        // 音频码率已经不在界面上：任何输入都归一为固定值 192
        assert_eq!(cfg.audio_bitrate_kbps, crate::config::FIXED_AUDIO_BITRATE_KBPS);
        assert_eq!(cfg.audio_bitrate_kbps, 192);
        // 关键帧间隔也不再暴露：归一为 0（自动）
        assert_eq!(cfg.keyframe_interval_sec, 0, "关键帧间隔内部自动（0）");
    }

    // ---- 全局快捷键：三选一 / 捕获控件 / 校验提示 ----

    /// 设置页里那三行的**有效组合**（与 `tray` 侧的算法是同一条，设置页不另写一份）。
    #[test]
    fn the_three_presets_map_to_the_expected_combos() {
        let rows = [
            (HotkeyPreset::CtrlAltR, PRESET_CTRL_ALT_R.label()),
            (HotkeyPreset::CtrlShiftR, PRESET_CTRL_SHIFT_R.label()),
            (HotkeyPreset::Custom, "自定义".to_string()),
        ];
        assert_eq!(rows[0].1, "Ctrl+Alt+R");
        assert_eq!(rows[1].1, "Ctrl+Shift+R");
        // 前两项是预设常量，第三项是"自定义"这三个字（不是组合）
        assert_eq!(preset_hotkey_of(rows[0].0), "Ctrl+Alt+R");
        assert_eq!(preset_hotkey_of(rows[1].0), "Ctrl+Shift+R");
        // 自定义：默认值就是"跟默认预设一样"（文本空 → 回退），保证勾上自定义不会没热键
        assert_eq!(
            effective_hotkey(HotkeyPreset::Custom, "").label(),
            PRESET_CTRL_ALT_R.label()
        );
        // 录制中控件置灰：设置页不参与切换，这条只固化"自定义 + 关闭开关"的组合
        let cfg = Config {
            global_hotkey_enabled: false,
            hotkey_preset: HotkeyPreset::Custom,
            hotkey_custom: "Ctrl+Alt+F9".into(),
            ..Config::default()
        };
        assert_eq!(
            effective_hotkey(cfg.hotkey_preset, &cfg.hotkey_custom).label(),
            "Ctrl+Alt+F9",
            "开关关掉只是不注册，有效组合本身仍然是配置里那个"
        );
    }

    fn preset_hotkey_of(p: HotkeyPreset) -> String {
        effective_hotkey(p, "").label()
    }

    /// 捕获控件拼出来的文本：修饰键顺序固定（Ctrl→Alt→Shift），且**都能被解析器认回来**。
    #[test]
    fn capture_builds_canonical_text() {
        let none = egui::Modifiers::NONE;
        let ctrl = egui::Modifiers::CTRL;
        let all = egui::Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            ..egui::Modifiers::NONE
        };
        assert_eq!(combo_text(&none, "F9"), "F9");
        assert_eq!(combo_text(&ctrl, "R"), "Ctrl+R");
        assert_eq!(combo_text(&all, "F9"), "Ctrl+Alt+Shift+F9");
        // 顺序与按键先后无关：同一个组合永远拼成同一个字符串（否则配置会"看着变了"）
        assert_eq!(combo_text(&all, "F9"), combo_text(&all, "F9"));
        assert_eq!(parse_hotkey(&combo_text(&ctrl, "R")).unwrap().label(), "Ctrl+R");
    }

    /// egui 的按键名 → 解析器认得的名字：**每一个**映射都要能真的解析（否则捕获会一直报错）。
    #[test]
    fn every_mapped_key_name_parses_back() {
        use egui::Key as K;
        let keys = [
            K::A, K::Z, K::Num0, K::Num9, K::F1, K::F12, K::F24, K::Tab, K::Enter, K::Space,
            K::Insert, K::Delete, K::Home, K::End, K::PageUp, K::PageDown, K::ArrowUp,
            K::ArrowDown, K::ArrowLeft, K::ArrowRight, K::Backspace,
        ];
        for key in keys {
            let name = egui_key_name(key).unwrap_or_else(|| panic!("{key:?} 应当有名字"));
            // 用三重修饰键：`Ctrl+Alt+Delete` / `Ctrl+Shift+Esc` 那种**保留组合**会被
            // （正确地）拒绝，那样测的就不是"名字能不能反解"了
            let text = format!("Ctrl+Alt+Shift+{name}");
            let hk = parse_hotkey(&text)
                .unwrap_or_else(|e| panic!("捕获出来的 {text} 必须能被解析，实际 {e:?}"));
            assert!(!hk.label().is_empty());
        }
        // 不认识的键（小数点、复制……）不参与捕获，也不该 panic
        assert!(egui_key_name(K::Period).is_none());
        assert!(egui_key_name(K::Copy).is_none());
    }

    /// 校验失败 → 四种原因都有**非空的可见文案**（走 i18n，中英都不能缺）。
    #[test]
    fn parse_errors_have_visible_text_in_both_languages() {
        for lang in ["zh-CN", "en-US"] {
            let i = I18n::new(lang);
            let t = |k: &str| i.t(k).to_string();
            let reserved = match parse_hotkey("Ctrl+Alt+Del") {
                Err(HotkeyParseError::Reserved(hk)) => hk,
                other => panic!("Ctrl+Alt+Del 应当是保留组合，实际 {other:?}"),
            };
            let cases = [
                HotkeyParseError::NoModifier,
                HotkeyParseError::NoKey,
                HotkeyParseError::Reserved(reserved),
                HotkeyParseError::Malformed("媒体键".to_string()),
            ];
            for err in cases {
                let text = parse_error_text(&i, &err, &t);
                assert!(!text.is_empty(), "{lang} 下 {err:?} 没有文案");
                assert!(
                    !text.contains("hotkey_err_"),
                    "{lang} 下 {err:?} 落到了 key 本身（漏翻）：{text}"
                );
            }
        }
    }

    /// 捕获态：默认不捕获；`stop_capture()` 之后一定不捕获（切走标签页时外壳会调它）。
    #[test]
    fn capture_state_can_always_be_reset() {
        let mut page = SettingsPage::new();
        assert!(!page.capturing);
        page.capturing = true;
        page.capture_error = Some("x".into());
        page.stop_capture();
        assert!(!page.capturing);
        assert!(page.capture_error.is_none());
    }

    // =======================================================================
    // P26：设置页数值框的文本必须**在框内居中**
    // =======================================================================

    /// 装**和真机同一套**字体（内置中文子集 + egui 自带拉丁/符号字体）：
    /// 量"文本矩形"必须用真字体 —— 否则中文会落成缺字，矩形的宽高都不是真机上的值。
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

    /// 把这一帧真正交给绘制器的**文本图形**收出来：`(文本, 文本矩形)`。
    fn collect_text_shapes(shape: &egui::Shape, into: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(t) => into.push((t.galley.text().to_string(), t.visual_bounding_rect())),
            egui::Shape::Vec(v) => {
                for s in v {
                    collect_text_shapes(s, into);
                }
            }
            _ => {}
        }
    }

    /// **P26**：设置页那两个数值框 ——「5.0 Mbps」（可编辑）与「30 帧/秒」（只读）——
    /// 各自的**文本矩形中心与框中心偏差 ≤1px**（**两框都断言**），两框尺寸也必须一致；
    /// 且帧率的框**仍然只读**（不可聚焦、不进 Tab 链、不参与点击/拖动）。
    ///
    /// 这条测试量的是**真实渲染出来的**几何（`FullOutput.shapes` 里的 `TextShape` +
    /// `Response::rect`），不是"我们以为的期望值" —— 用户原话「设置页，30 帧/秒，
    /// 没有在框中居中」正是这种"两行各写各的"才会漏掉的偏差。
    #[test]
    fn p26_value_boxes_center_their_text() {
        use egui::{Pos2, Rect, Vec2};
        let ctx = egui::Context::default();
        install_measure_fonts(&ctx);
        theme::apply(&ctx);

        let mut page = SettingsPage::new();
        let mut cfg = Config::default();
        let mut i18n = I18n::new("zh-CN");
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0))),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            let _ = page.ui(ui, &mut cfg, &mut i18n, false);
        });
        // `TexturesDelta` 在 drop 时会对"没被应用"这件事 `debug_assert`（无窗口测试台没有渲染器），
        // 所以按官方口径先清掉（`drop_without_applying_deltas` 做的就是这一句）。
        out.textures_delta.clear();

        let (bitrate_box, fps_box) = page.test_value_boxes;
        let bitrate_box = bitrate_box.expect("本帧应登记「目标码率」数值框矩形");
        let fps_box = fps_box.expect("本帧应登记「帧率」数值框矩形");
        assert!(
            bitrate_box.width() > 1.0 && fps_box.width() > 1.0,
            "两个数值框都应当真的画出来（{bitrate_box:?} / {fps_box:?}）"
        );
        // 两框尺寸必须一致（同一列、同一个 `number_box_size()`）
        let want = theme::number_box_size();
        for (name, r) in [("目标码率", bitrate_box), ("帧率", fps_box)] {
            assert!(
                (r.width() - want.x).abs() <= 0.5 && (r.height() - want.y).abs() <= 0.5,
                "{name}框尺寸必须是 {want:?}（实测 {r:?}）"
            );
        }

        let mut texts: Vec<(String, egui::Rect)> = Vec::new();
        for cs in &out.shapes {
            collect_text_shapes(&cs.shape, &mut texts);
        }
        let find = |needle: &str| -> egui::Rect {
            texts
                .iter()
                .find(|(s, _)| s.contains(needle))
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("本帧找不到文本「{needle}」：{texts:?}"))
        };
        let bitrate_text = find("Mbps");
        let fps_text = find(i18n.t("fps_30"));
        for (name, text, box_rect) in [
            ("目标码率", bitrate_text, bitrate_box),
            ("帧率", fps_text, fps_box),
        ] {
            let dx = (text.center().x - box_rect.center().x).abs();
            let dy = (text.center().y - box_rect.center().y).abs();
            assert!(
                dx <= 1.0 && dy <= 1.0,
                "{name}框的文本必须居中：偏差 dx={dx:.2} dy={dy:.2}（文本 {text:?} / 框 {box_rect:?}）"
            );
        }

        // ---- 帧率框仍然**只读**（P11 的语义不许被 P26 顶掉）----
        let mut fps_resp: Option<egui::Response> = None;
        let input2 = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1024.0, 768.0))),
            ..Default::default()
        };
        let mut out2 = ctx.run_ui(input2, |ui| {
            fps_resp = Some(theme::read_only_value_box(ui, "30 帧/秒"));
        });
        out2.textures_delta.clear();
        let r = fps_resp.expect("只读框应当返回 Response");
        assert!(
            !r.sense.is_focusable(),
            "只读框不许可聚焦（`Sense::hover()` 没有 FOCUSABLE 位 → 不进 Tab 链）"
        );
        assert!(!r.sense.interactive(), "只读框不许参与点击/拖动");
        assert!(!r.has_focus(), "只读框不许拿到焦点");
    }

    /// 无窗口测试台：一帧设置页不需要窗口，只需要 Context + 一块屏幕矩形。
    ///（`use super::*` 已在模块顶部，这里不重复 import。）
    struct Rig {
        ctx: egui::Context,
        page: SettingsPage,
        cfg: Config,
        i18n: I18n,
        store: ConfigStore,
        screen: egui::Rect,
        time: f64,
    }

    impl Rig {
        fn new(tag: &str) -> Self {
            let ctx = egui::Context::default();
            // **装真字体**（与 `main.rs::install_cjk_font` 同一条路径：中文子集 + egui 内置字体）。
            // 为什么不能像别的测试台那样 `FontDefinitions::empty()`：本文件这些守门测试断言的是
            // **部件的屏幕位置**（同一行 / 在右侧）。没有字体时 galley 尺寸全为零，整页被压成
            // "零宽文字"的布局，位置虽然有序但宽度失真（也量不出"这一行放不放得下"）。
            // 装了子集字体后，量到的就是用户在真机上看到的同一套几何。
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
            // 配置落到临时目录：既要验证"调整即保存"真的落了盘，又不能碰用户真实配置
            let dir = std::env::temp_dir().join(format!("caplite-settings-tests-{tag}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self {
                ctx,
                page: SettingsPage::new(),
                // **钉死浅色起步**：`Config::default().dark_mode` 现在是 `true`（P42：
                // 产品默认主题翻成深色），而 `p40_dark_mode_switch_toggles_theme_and_persists`
                // 的脚本是"浅色 → 点一下 → 深色 → 再点一下 → 浅色"。
                // 测试自己声明起始主题，别让它跟着产品默认漂。
                cfg: Config { dark_mode: false, ..Config::default() },
                i18n: I18n::new("zh-CN"),
                store: ConfigStore::detect_with(Some(dir), &|_p| Ok(())),
                screen: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1024.0, 768.0)),
                time: 0.0,
            }
        }

        /// 画一帧，返回这一帧**真正交给绘制器的图形**（`Shape::Vec` 递归展平）。
        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<egui::Shape> {
            let Self {
                ctx,
                page,
                cfg,
                i18n,
                store,
                screen,
                time,
            } = self;
            let mut input = egui::RawInput {
                screen_rect: Some(*screen),
                ..Default::default()
            };
            input.time = Some(*time);
            *time += 0.5;
            input.events = events;
            let mut outcome = SettingsOutcome::default();
            let mut out = ctx.run_ui(input, |ui| {
                outcome = page.ui(ui, cfg, i18n, false);
            });
            // **模拟外壳**（P45）：设置页不再自己落盘 —— 落盘统一由 `main.rs::commit_cfg`
            // 做（带 200ms 节流）。守门测试要验"改动真的到了磁盘"，所以这里补上与外壳
            // 等价的一次保存；不加节流，因为测试里每帧都是一次独立的用户操作。
            if outcome.cfg_changed {
                let _ = store.save(cfg);
            }
            // `TexturesDelta` 在 drop 时会对"没被应用"这件事 `debug_assert`，先清掉
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

        fn click(&mut self, p: egui::Pos2) {
            self.frame(vec![egui::Event::PointerMoved(p)]);
            self.frame(vec![
                egui::Event::PointerMoved(p),
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            self.frame(vec![
                egui::Event::PointerMoved(p),
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
        }
    }

    /// 文字形状的**屏幕矩形**（逐行 `PlacedRow::rect()` 求并集，再平移 `TextShape::pos`）。
    ///
    /// 为什么不直接用 `galley.rect`：`ctx.run_ui` 交出来的 `Shape::Text` **还没过分帧化**，
    /// `galley.rect` 是空的（零宽零高），量出来做不了位置/宽度断言。逐行取逻辑矩形是
    /// 分帧化之前唯一可靠的尺寸来源。
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

    /// 找这一帧里含 `needle` 的文字形状，返回它的屏幕矩形。
    fn find_text(shapes: &[egui::Shape], needle: &str) -> Option<egui::Rect> {
        shapes.iter().find_map(|s| match s {
            egui::Shape::Text(t) if t.galley.job.text.contains(needle) => Some(text_rect(t)),
            _ => None,
        })
    }

    /// 找同一行、标签左边那个**小方框**（egui 的 `Checkbox` 先画方框、再画文字）。
    ///
    /// 取"最靠近标签"的那个：整张卡片的大底也是矩形，但宽度远超小方框，
    /// 被尺寸条件挡掉；同一行其它控件（语言下拉框）宽高也不是方块。
    fn checkbox_box(shapes: &[egui::Shape], label: egui::Rect) -> egui::Rect {
        let mut best: Option<egui::Rect> = None;
        for s in shapes {
            if let egui::Shape::Rect(r) = s {
                let c = r.rect.center();
                let same_row = (c.y - label.center().y).abs() <= 6.0;
                let left_of_label = r.rect.right() <= label.left() + 2.0;
                let squareish = (8.0..=24.0).contains(&r.rect.width())
                    && (8.0..=24.0).contains(&r.rect.height())
                    && (r.rect.width() - r.rect.height()).abs() <= 4.0;
                if same_row && left_of_label && squareish {
                    best = Some(match best {
                        Some(b) if b.right() >= r.rect.right() => b,
                        _ => r.rect,
                    });
                }
            }
        }
        best.expect("复选框左边应有一个小方框")
    }

    /// **P40 守门测试（设置页那颗"深色模式"开关）**。
    ///
    /// 用户原话"在界面语言选项后面添加深色模式开关"，所以从**界面**上验四件事：
    /// ① 开关真的画在「界面语言」那一行**下面**（不是只改了语言包或源码）；
    /// ② **点它** = 立刻换肤（同一帧剩下的控件就按新主题画）+ `cfg.dark_mode` 置位；
    /// ③ **调整即保存**：落盘的配置里真的带着 `dark_mode`；
    /// ④ 再点一次回浅色（往返可靠，`sanitized()` 也不吞这个选择）。
    #[test]
    fn p40_dark_mode_switch_toggles_theme_and_persists() {
        let mut r = Rig::new("p40-dark-mode");
        crate::ui::theme::apply(&r.ctx);
        assert!(!r.cfg.dark_mode, "起步是浅色");

        // ① 画一帧：开关在场，且与「界面语言」**同一行**、在下拉框右侧
        //   （P40 修订：用户要求"放到界面语言同一行右侧"，不再是下面单独一行）
        let shapes = r.frame(vec![]);
        let lang = find_text(&shapes, "界面语言").expect("应画出「界面语言」那一行");
        let sw = find_text(&shapes, "深色模式").expect("应画出「深色模式」开关（P40）");
        assert!(
            (sw.center().y - lang.center().y).abs() <= 6.0,
            "开关应与界面语言**同一行**：语言中心 y {:.1}，开关中心 y {:.1}",
            lang.center().y,
            sw.center().y
        );
        // 语言那一行的控件（下拉框）显示的是当前语言 id（默认 en-US）
        let combo = find_text(&shapes, "en-US").expect("应画出语言下拉框（显示当前语言）");
        assert!(
            sw.left() >= combo.right() - 0.5,
            "开关应在语言下拉框**右侧**：下拉框右缘 {:.1}，开关左缘 {:.1}",
            combo.right(),
            sw.left()
        );
        assert!(!crate::ui::theme::is_dark(), "初始必须是浅色");

        // ② 点一下 → 深色 + 配置置位（连 egui 的全局偏好也一起钉过去）
        r.click(checkbox_box(&shapes, sw).center());
        assert!(r.cfg.dark_mode, "点过开关后配置必须记住深色");
        assert!(
            crate::ui::theme::is_dark(),
            "点过开关后必须**立刻**换肤（而不是等下次启动）"
        );
        assert_eq!(r.ctx.theme(), egui::Theme::Dark, "egui 全局偏好也要钉成深色");

        // ③ 落盘：文件里带着 dark_mode（P45 起由**外壳**落盘，测试台里由 `Rig::frame`
        //    模拟外壳那一步；页面自己不再写盘，所以这里能落盘就证明"改动被如实上报了"）
        let saved = std::fs::read_to_string(r.store.path()).expect("调整即保存应已落盘");
        // `save_to` 走 `to_string_pretty`：冒号后面有一个空格
        assert!(
            saved.contains("\"dark_mode\": true"),
            "落盘内容缺 dark_mode: {saved}"
        );

        // ④ 再点一次 → 回浅色（先重画一帧拿新的方框位置）
        let shapes = r.frame(vec![]);
        let sw = find_text(&shapes, "深色模式").expect("开关应还在");
        r.click(checkbox_box(&shapes, sw).center());
        assert!(!r.cfg.dark_mode, "再点一次应回到浅色");
        assert!(!crate::ui::theme::is_dark(), "换肤也要跟着回到浅色");
        assert_eq!(r.ctx.theme(), egui::Theme::Light);
        // `sanitized()` 不吞这个选择（外壳保存前会过一次 sanitized）
        assert!(!r.cfg.clone().sanitized().dark_mode);
        // 还原线程局部主题：只在本线程有效，但保持严谨
        crate::ui::theme::apply_kind(&r.ctx, crate::ui::theme::ThemeKind::Light);
    }

    /// **P40 修订守门测试（快捷键那一行）**：用户要求去掉「全局快捷键」这个标签，
    /// 并把三个选项放到「启用全局快捷键」开关**同一行的右侧**。从这一帧的图形上验三件事：
    /// ① 不再有**独立的**「全局快捷键」文字（注意"启用全局快捷键"里含这五个字，
    ///    所以必须比"整段相等"，不能比"包含"）；
    /// ② 三个选项与开关**同一行**；
    /// ③ 选项在开关**右侧**。
    #[test]
    fn p40_hotkey_options_share_the_row_with_their_switch() {
        let mut r = Rig::new("p40-hotkey-row");
        crate::ui::theme::apply(&r.ctx);
        let shapes = r.frame(vec![]);

        // ① 独立的标签没了（"启用全局快捷键"是另一段文字，不算）
        let standalone = shapes.iter().any(|s| match s {
            egui::Shape::Text(t) => t.galley.job.text.trim() == "全局快捷键",
            _ => false,
        });
        assert!(!standalone, "「全局快捷键」标签应已删除（P40 修订）");

        // ② ③ 开关与三个选项同一行，且选项在右
        let sw = find_text(&shapes, "启用全局快捷键").expect("应画出「启用全局快捷键」开关");
        for opt in ["Ctrl+Alt+R", "Ctrl+Shift+R", "自定义"] {
            let rect = find_text(&shapes, opt).expect("应画出选项 {opt}");
            assert!(
                (rect.center().y - sw.center().y).abs() <= 6.0,
                "{opt} 应与开关**同一行**：开关中心 y {:.1}，选项中心 y {:.1}",
                sw.center().y,
                rect.center().y
            );
            assert!(
                rect.left() >= sw.right() - 0.5,
                "{opt} 应在开关**右侧**：开关右缘 {:.1}，选项左缘 {:.1}",
                sw.right(),
                rect.left()
            );
        }
    }

    /// **P40 守门测试（三个分区标题真的删了）**：用户要求去掉「常规」「剪辑」「视频」
    /// 三行标题。这里从**这一帧的图形**上验：三个标题词一个都不出现，
    /// 而它们各自卡片里的**内容**还在（托盘、快捷键、波形、码率滑轨）。
    #[test]
    fn p40_section_headers_are_gone_but_their_content_stays() {
        let mut r = Rig::new("p40-headers");
        crate::ui::theme::apply(&r.ctx);
        let shapes = r.frame(vec![]);
        for gone in ["常规", "视频"] {
            assert!(
                find_text(&shapes, gone).is_none(),
                "分区标题「{gone}」应已删除（P40）"
            );
        }
        for kept in [
            "界面语言",
            "深色模式",
            "录制开始后隐藏到托盘",
            "启用全局快捷键",
            "显示音频波形",
            "目标码率",
            "帧率",
        ] {
            assert!(
                find_text(&shapes, kept).is_some(),
                "删标题不能顺手删内容：{kept} 应还在"
            );
        }
    }
}
