//! 统一错误类型：把各子系统的错误聚合起来，供 UI 直接显示。

use std::fmt;

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum AppError {
    /// 用户取消（例如关闭选区窗口、取消保存对话框）
    Cancelled,
    /// 视频采集（WGC / D3D11）
    Capture(String),
    /// 音频采集（WASAPI）与混音
    Audio(String),
    /// 视频编码（OpenH264）
    VideoEncode(String),
    /// 音频编码（Media Foundation AAC）
    AudioEncode(String),
    /// MP4 封装/解析
    Mp4(String),
    /// 配置读写
    Config(String),
    /// 文件对话框
    Dialog(String),
    /// 其它 IO
    Io(std::io::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Cancelled => write!(f, "操作已取消"),
            AppError::Capture(s) => write!(f, "屏幕采集失败：{s}"),
            AppError::Audio(s) => write!(f, "音频采集失败：{s}"),
            AppError::VideoEncode(s) => write!(f, "视频编码失败：{s}"),
            AppError::AudioEncode(s) => write!(f, "音频编码失败：{s}"),
            AppError::Mp4(s) => write!(f, "MP4 处理失败：{s}"),
            AppError::Config(s) => write!(f, "配置读写失败：{s}"),
            AppError::Dialog(s) => write!(f, "文件对话框失败：{s}"),
            AppError::Io(e) => write!(f, "IO 错误：{e}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e)
    }
}
