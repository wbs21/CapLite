use crate::i18n::I18n;
use crate::recorder::RecorderHandle;
use crate::region_picker::{start_region_picker, SelectOutcome};
use crate::{constants, config};
use crossbeam_channel::Receiver;
use crossbeam_channel::TryRecvError;
use std::time::{Duration, Instant};
use tray_icon::{menu::MenuId, TrayIcon, TrayIconBuilder};

#[derive(Debug, Default)]
pub struct AppUiState {
    pub cfg: config::PersistentConfig,
    pub rt: config::RuntimeState,
    pub i18n: I18n,

    pub rec: RecorderHandle,
    pub rec_error: Option<String>,
    pub rec_finished_notice: bool,
    pub config_error: Option<String>,

    pub sel_rx: Option<Receiver<SelectOutcome>>,
    pub save_rect_to_config: bool,

    pub rec_start: Option<Instant>,
    pub rec_duration: Duration,

    // 托盘
    pub tray: Option<TrayIcon>,
    pub hide_window: bool,
    pub show_item_id: MenuId,
    pub quit_item_id: MenuId,

    // 后台心跳
    pub backend_last_heartbeat: Instant,
    pub show_close_confirm: bool,
    // 休眠锁标记
    pub prevent_sleep_active: bool,
}

impl AppUiState {
    pub fn new() -> Self {
        simplelog::CombinedLogger::init(vec![
            simplelog::TermLogger::new(
                simplelog::LevelFilter::Info,
                simplelog::Config::default(),
                simplelog::TerminalMode::Mixed,
                simplelog::ColorChoice::Auto,
            ),
            simplelog::WriteLogger::new(
                simplelog::LevelFilter::Info,
                simplelog::Config::default(),
                std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open("recorder.log")
                    .unwrap(),
            ),
        ])
        .ok();
        log::info!("程序启动");

        let mut config_error: Option<String> = None;
        let cfg = match config::load_config() {
            Ok(c) => c,
            Err(e) => {
                log::error!("加载配置失败:{}", e);
                config_error = Some(e);
                config::PersistentConfig::default()
            }
        };

        let mut i18n = I18n::new();
        i18n.set_language(&cfg.lang);

        let mut rt = config::RuntimeState::default();
        if cfg.record_mode == config::RecordMode::Region && cfg.saved_capture_rect.is_valid() {
            rt.capture_rect = cfg.saved_capture_rect;
        }

        // 托盘菜单
        let menu = tray_icon::menu::Menu::new();
        let show_item = tray_icon::menu::MenuItem::new("显示窗口", true);
        let quit_item = tray_icon::menu::MenuItem::new("退出程序", true);
        let show_item_id = show_item.id();
        let quit_item_id = quit_item.id();
        menu.append(&show_item).unwrap();
        menu.append(&quit_item).unwrap();

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Rust Screen Recorder")
            .build()
            .ok();

        Self {
            cfg,
            rt,
            i18n,
            rec: RecorderHandle::new(),
            rec_error: None,
            rec_finished_notice: false,
            config_error,
            sel_rx: None,
            save_rect_to_config: false,
            rec_start: None,
            rec_duration: Duration::ZERO,
            tray,
            hide_window: false,
            show_item_id,
            quit_item_id,
            backend_last_heartbeat: Instant::now(),
            show_close_confirm: false,
            prevent_sleep_active: false,
        }
    }

    /// Windows 阻止系统休眠
    fn set_prevent_sleep(&mut self, enable: bool) {
        #[cfg(windows)]
        unsafe {
            use winapi::um::winbase::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
            if enable {
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
                log::info!("开启阻止休眠");
            } else {
                SetThreadExecutionState(ES_CONTINUOUS);
                log::info!("恢复系统休眠");
            }
        }
        self.prevent_sleep_active = enable;
    }

    pub fn launch_region_picker(&mut self) {
        if self.sel_rx.is_some() {
            log::warn!("选区拾取器已在运行，忽略请求");
            return;
        }
        let init_rect = self.rt.capture_rect;
        log::info!("启动选区拾取");
        self.sel_rx = Some(start_region_picker(init_rect));
    }

    pub fn poll_region_picker(&mut self) {
        let Some(rx) = &mut self.sel_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => match outcome {
                SelectOutcome::Done(rect) => {
                    log::info!("选区拾取成功: {:?}", rect);
                    self.rt.capture_rect = rect;
                    if self.save_rect_to_config {
                        self.cfg.saved_capture_rect = rect;
                        self.cfg.record_mode = config::RecordMode::Region;
                    }
                }
                SelectOutcome::Cancelled => {
                    log::info!("选区拾取已取消");
                }
            },
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                log::warn!("选区拾取通道断开");
            }
        }
        self.sel_rx.take();
    }

    pub fn poll_recorder_events(&mut self) {
        if let Some(evt) = self.rec.poll_backend_event() {
            match evt {
                crate::av_backend::BackendEvent::Finished => {
                    log::info!("录制正常结束");
                    self.rec_finished_notice = true;
                    self.rec_start = None;
                    self.set_prevent_sleep(false);
                }
                crate::av_backend::BackendEvent::Error(msg) => {
                    log::error!("后端录制错误:{}", msg);
                    self.rec_error = Some(msg);
                    self.rec_start = None;
                    self.set_prevent_sleep(false);
                }
                crate::av_backend::BackendEvent::Heartbeat => {
                    self.backend_last_heartbeat = Instant::now();
                }
            }
        }

        // 心跳超时检测
        if self.rec_start.is_some() && self.backend_last_heartbeat.elapsed() > constants::BACKEND_HEARTBEAT_TIMEOUT {
            let err_msg = format!(
                "录制后台线程无响应（超过{}秒），录制已终止",
                constants::BACKEND_HEARTBEAT_TIMEOUT.as_secs()
            );
            log::error!("{}", err_msg);
            self.rec_error = Some(err_msg);
            match self.rec.stop_record() {
                Ok(_) => log::info!("心跳超时：stop_record调用成功"),
                Err(e) => log::warn!("心跳超时调用stop_record返回错误:{}", e),
            }
            self.rec_start = None;
            self.set_prevent_sleep(false);
        }
    }

    pub fn open_save_dialog(&self) -> Option<String> {
        let file = rfd::FileDialog::new()
            .set_title(self.i18n.t("save_dialog_title"))
            .add_filter("MP4 Video", &["mp4"])
            .set_file_name(constants::DEFAULT_SAVE_FILENAME)
            .save_file()?;

        let mut path_str = file.to_string_lossy().to_string();
        if !path_str.ends_with(".mp4") {
            path_str.push_str(".mp4");
        }
        log::info!("选中保存路径:{}", path_str);
        Some(path_str)
    }

    pub fn update_timer(&mut self) {
        if let Some(start) = self.rec_start {
            self.rec_duration = start.elapsed();
        }
    }

    pub fn format_duration(d: Duration) -> String {
        let total_sec = d.as_secs();
        let h = total_sec / 3600;
        let m = (total_sec % 3600) / 60;
        let s = total_sec % 60;
        format!("{h:02}:{m:02}:{s:02}")
    }
}
