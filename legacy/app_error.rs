use std::fmt;
#[derive(Debug)]
pub enum AppError {
    AvBackend(crate::av_backend::AvError),
    Io(std::io::Error),
    WgcCapture(String),
    RegionPick(String),
    Rfd(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::AvBackend(e) => write!(f,"AV: {e}"),
            AppError::Io(e) => write!(f,"IO: {e}"),
            AppError::WgcCapture(s) => write!(f,"WGC采集: {s}"),
            AppError::RegionPick(s) => write!(f,"选区: {s}"),
            AppError::Rfd(s) => write!(f,"文件对话框: {s}"),
        }
    }
}
impl std::error::Error for AppError{}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self { AppError::Io(e) }
}
impl From<crate::av_backend::AvError> for AppError {
    fn from(e: crate::av_backend::AvError) -> Self { AppError::AvBackend(e) }
}
