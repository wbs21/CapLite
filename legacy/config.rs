use serde::{Serialize, Deserialize};
use std::fs;
use std::path::PathBuf;
use directories::ProjectDirs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentConfig {
    pub saved_capture_rect: CaptureRegion,
    pub default_fps: u32,
    pub default_bitrate: i32,
    pub last_output_dir: String,
}

impl Default for PersistentConfig {
    fn default() -> Self {
        Self {
            saved_capture_rect: CaptureRegion { x:0,y:0,width:1920,height:1080 },
            default_fps:30,
            default_bitrate: 4000000,
            last_output_dir: std::env::var("USERPROFILE").unwrap_or_default(),
        }
    }
}

impl PersistentConfig {
    fn get_config_path() -> Option<PathBuf> {
        let proj = ProjectDirs::from("com", "rustrec", "RustScreenRecorder")?;
        let mut p = proj.config_dir().to_path_buf();
        fs::create_dir_all(&p).ok();
        p.push("config.json");
        Some(p)
    }

    pub fn load() -> Self {
        let Some(path) = Self::get_config_path() else { return Self::default() };
        let Ok(text) = fs::read_to_string(&path) else { return Self::default() };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::get_config_path() else {return};
        let Ok(json) = serde_json::to_string_pretty(self) else {return};
        let _ = fs::write(path, json);
    }
}
