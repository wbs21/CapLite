use eframe::egui;
use crate::config::{PersistentConfig, CaptureRegion};
use crate::recorder::{ScreenRecorder, RecorderState, format_duration};
use crate::region_picker;
use rfd::FileDialog;
use crate::app_error::AppError;
use tracing::Level;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

fn main() -> Result<(), eframe::Error> {
    // 日志初始化
    let file_appender = RollingFileAppender::new(
        Rotation::Daily,
        "logs",
        "recorder.log",
    );
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_max_level(Level::INFO)
        .with_file(true)
        .with_line_number(true)
        .init();
    tracing::info!("应用启动");

    let native_options = eframe::NativeOptions::default();
    eframe::run_native("Rust录屏", native_options, Box::new(|_creation_context| Box::new(App::new())))
}
struct App {
    cfg: PersistentConfig,
    recorder: Option<ScreenRecorder>,
    err_msg: Option<String>,
    output_path: String,
}
impl App {
    fn new() -> Self {
        let cfg = PersistentConfig::load();
        Self { cfg, recorder:None, err_msg:None, output_path:String::new() }
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx:&egui::Context, _frame:&mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Rust屏幕录制");
            if let Some(e) = &self.err_msg { ui.colored_label(egui::Color32::RED, e); }
            ui.label(format!("当前选区: x{} y{} w{} h{}",
                self.cfg.saved_capture_rect.x, self.cfg.saved_capture_rect.y,
                self.cfg.saved_capture_rect.width, self.cfg.saved_capture_rect.height));

            if let Some(rec) = &self.recorder {
                let dur = rec.get_record_duration();
                ui.label(format!("录制时长: {}", format_duration(dur)));
            } else {
                ui.label("录制时长: 00:00:00");
            }

            ui.label(format!("帧率:{} 码率:{}", self.cfg.default_fps, self.cfg.default_bitrate));
            if ui.button("选择选区").clicked() {
                match region_picker::pick_region(self.cfg.saved_capture_rect.clone()) {
                    Ok(r) => {
                        self.cfg.saved_capture_rect = r.clone();
                        self.cfg.save();
                        self.err_msg = None;
                    }
                    Err(e) => self.err_msg = Some(e),
                }
            }
            if ui.button("选择保存路径").clicked() {
                if let Some(path) = FileDialog::new().set_directory(&self.cfg.last_output_dir).save_file() {
                    self.output_path = path.to_string_lossy().to_string();
                    self.cfg.last_output_dir = self.output_path.clone();
                    self.cfg.save();
                }
            }
            ui.label(format!("输出:{}", self.output_path));
            let state = self.recorder.as_ref().map(|r| r.get_state()).unwrap_or(RecorderState::Idle);
            match state {
                RecorderState::Idle => {
                    if ui.button("开始录制").clicked() {
                        if self.output_path.is_empty() { self.err_msg = Some("请先选择保存路径".into()); return; }
                        let mut rec = ScreenRecorder::new(self.output_path.clone(), self.cfg.default_fps, self.cfg.default_bitrate);
                        rec.set_capture_region(self.cfg.saved_capture_rect.clone());
                        match rec.start() {
                            Ok(_) => { self.recorder = Some(rec); self.err_msg = None; }
                            Err(e) => self.err_msg = Some(format!("启动录制失败: {e}")),
                        }
                    }
                }
                RecorderState::Recording => {
                    if ui.button("停止录制").clicked() {
                        if let Some(r) = &mut self.recorder {
                            if let Err(e) = r.stop() { self.err_msg = Some(format!("停止失败: {e}")); }
                        }
                        self.recorder.take();
                    }
                }
                RecorderState::Stopping => { ui.label("正在停止录制..."); }
            }
            ui.label("OpenH264: MIT License");
        });
    }
}
