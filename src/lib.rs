//! cap —— Windows 屏幕录制库。
//!
//! 架构（详见 `docs/DESIGN.md` §10）：
//! * [`video::capture`] —— DXGI Desktop Duplication 采集（区域裁剪、会话重建）
//! * [`video::cursor`]  —— 鼠标指针合成（DXGI 帧不含指针）
//! * [`video::convert`] —— libyuv BGRA→I420（零拷贝裁剪）
//! * [`video::encode`]  —— 原生 OpenH264 编码（camera_rt + FixedSlcNumSlice）
//! * [`audio`]          —— WASAPI 环回/麦克风采集（读写分离 + 两层环形缓冲）→ 混音 → FFmpeg AAC-LC
//! * [`mux`]            —— FFmpeg 分片 MP4 写入 / 重封装（copy，不重新编码）
//! * [`sys`]            —— 线程优先级（音频链优先，视频可弃）
//! * [`sync`]           —— 抗毒化取锁（后台线程 panic 不再传染成 UI 崩溃）
//! * [`config`] / [`i18n`] / [`error`] —— 配置、多语言、错误

// `slice::as_chunks` 仍是 nightly（P47 实测：照 clippy 的建议改成 `as_chunks::<4>()`
// 在 stable 上直接编译不过）。全仓有 3 处 `chunks_exact(4)`（像素/RGBA 解包），
// 都是"4 字节一个单元"的固定形状，读起来比 `as_chunks` 更直白 —— 整仓关掉这条 lint，
// 免得每次 clippy 都刷 3 条无法照做的建议。
#![allow(clippy::chunks_exact_to_as_chunks)]

pub mod audio;
pub mod cache;
pub mod config;
pub mod disk;
pub mod edit;
pub mod error;
pub mod i18n;
pub mod mux;
pub mod power;
pub mod recorder;
pub mod session;
pub mod single_instance;
pub mod sync;
pub mod sys;
pub mod tray;
pub mod ui;
pub mod video;

/// **只在单测里存在的共享工具**（P49）。
///
/// 为什么要有它：`mux` / `edit` / `edit::audio_out` / `edit::player` / `edit::waveform`
/// 的 `mod tests` 里各自抄了一份**逐字相同**的 `ffmpeg_exe()`（13 行 × 5 份）。
/// 它们是兄弟测试模块，互相看不见对方的私有项，所以只能往上提到 crate 级。
///
/// 为什么是 `#[cfg(test)]`：这些工具**不参与发布构建**，也不该出现在 `cargo doc` 里；
/// 集成测试（`tests/*.rs`）用不到它（它们拿不到 `cfg(test)` 的库），所以 `pub(crate)` 足够。
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};

    /// 随包 ffmpeg 的路径（**不用**系统 PATH 里的其它 ffmpeg；不存在就返回 `None`，
    /// 调用方据此跳过那一组测试，而不是失败）。
    pub fn ffmpeg_exe() -> Option<PathBuf> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ffmpeg-n9.0-latest-win64-lgpl-shared-9.0")
            .join("bin")
            .join("ffmpeg.exe");
        p.is_file().then_some(p)
    }
}
