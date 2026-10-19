//! 视频链路：采集 → 色彩转换 → 编码 → 封装。
//!
//! 各子模块职责：
//! * [`openh264`] —— 原生 OpenH264 编码器（手写 FFI + vtable + RAII 封装，零依赖）
//! * [`capture`]  —— DXGI Desktop Duplication 采集（区域裁剪 + 指针合成 + 会话重建）
//! * [`convert`]  —— libyuv BGRA→I420（含零拷贝区域裁剪）
//! * [`cursor`]   —— 鼠标指针合成（DXGI 帧不含指针，自行 alpha 混合）
//! * [`encode`]   —— 把产品配置映射到 OpenH264 参数，并输出 MP4 需要的 AVCC

pub mod capture;
pub mod convert;
pub mod cursor;
pub mod encode;
pub mod openh264;
