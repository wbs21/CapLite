//! 音频链路：设备枚举 → 采集（系统声音环回 / 麦克风）→ 混音 → AAC 编码。
//!
//! 与视频链路一致的约定：统一 **48 kHz / 2ch / f32 交错**（由 WASAPI autoconvert 保证），
//! 因此混音只是相加+限幅，AAC 编码只需一次 float→FLTP 的搬运。

// ---------------------------------------------------------------------------
// 链路统一格式常量（P54：单一来源）
// ---------------------------------------------------------------------------
//
// 这三个数是**整条音频链路的契约**（见上面模块文档的"统一 48 kHz / 2ch / f32 交错"）。
// P54 之前光是**采样率**一个数就在四个文件里各写一份、值完全相同却互不相干：
//
// | 旧名 | 位置 | 类型 |
// | --- | --- | --- |
// | `SAMPLE_RATE` | `aac.rs` | `i32` |
// | `TARGET_RATE` | `audio/capture.rs` | `usize` |
// | `OUT_RATE` | `edit/audio_out.rs` | `usize` |
// | `DECODE_RATE` | `edit/waveform.rs` | `i32` |
//
// 这是典型的**常量漂移**：改了任何一处，重采样 / 解码 / 编码就各按各的采样率跑，
// 而编译器一声不吭（都是合法的整数字面量），症状只表现为"声音变调/时长对不上"。
// 现在统一从这里取值，各处仍保留自己的名字（读起来更贴上下文），但**只有一个来源**。

/// 链路统一采样率（Hz）。
pub const SAMPLE_RATE: i32 = 48_000;
/// 链路统一声道数。
pub const CHANNELS: usize = 2;
/// 每样本字节数（f32）。
pub const BYTES_PER_SAMPLE: usize = 4;

pub mod aac;
pub mod capture;
pub mod devices;
pub mod mixer;
pub mod ring;

pub use aac::AacEncoder;
pub use capture::{rms, AudioCapture, SourceKind, TARGET_CHANNELS, TARGET_RATE};
pub use devices::AudioDevice;
