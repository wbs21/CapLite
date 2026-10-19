//! 全局常量，统一管理硬编码值
use std::time::Duration;

/// 默认保存文件名
pub const DEFAULT_SAVE_FILENAME: &str = "recording.mp4";
/// 后台心跳超时阈值
pub const BACKEND_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);
/// FPS 范围
pub const FPS_MIN: u32 = 1;
pub const FPS_MAX: u32 = 60;
/// 码率范围 kbps
pub const BITRATE_MIN: u32 = 100;
pub const BITRATE_MAX: u32 = 50000;
/// 默认视频参数
pub const DEFAULT_FPS: u32 = 30;
pub const DEFAULT_BITRATE_KBPS: u32 = 5000;
