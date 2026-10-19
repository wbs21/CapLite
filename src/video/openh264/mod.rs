//! 原生 OpenH264 编码器模块。
//!
//! 代码来源：`_rust/proto-oh264`（已交付并实测：32 项测试全绿，产物经 ffmpeg 逐帧独立
//! 解码验证、关键帧位置与 `SFrameBSInfo.eFrameType` 完全一致）。并入产品树后改为子模块
//! 路径，其余未改。
//!
//! 关键事实（实测，勿凭直觉改）：
//! * `openh264.dll` **只导出 6 个符号**，没有 `WelsEncodeFrame`/`WelsSetEncoderParam`；
//!   编码必须走 `ISVCEncoder` **vtable**。
//! * `native/openh264.lib` 是 `openh264-7.dll`（另一个文件名）的导入库且不含编码符号，
//!   **绝不要链接它**；本模块用 `LoadLibraryW` + `GetProcAddress` 运行时加载。
//! * 速度取决于 `usage_type` 与 slice：`CameraVideoRealTime` + `FixedSlcNumSlice` 实测
//!   1080p 达 129 fps；`ScreenContentRealTime` 只有 36 fps 且码率利用率更差（详见
//!   `docs/DESIGN.md` §10.2）。
//! * CABAC：早期版本的本文档说"本 API 上不可用"，**实测结论相反** ——
//!   `CameraVideoRealTime` + `bEntropyCodingModeFlag = 1` 能正常初始化、OpenH264 自动选
//!   High profile，1080p 同素材同码率下产物小 47%（158_688 B vs 299_294 B）而速度几乎不变
//!   （83.2 fps vs 83.7 fps）。真正不可用的是**非实时** usage type：库直接拒绝
//!   （`ParamValidationExt(),Invalid usage type = 2/3` → `cmInitParaError`）。
//!   复现：`cargo test --lib probe_cabac -- --nocapture`。

pub mod abi;
pub mod encoder;
pub mod win;

/// 合成测试帧 + 摘要（集成测试与复现用）。
#[doc(hidden)]
pub mod testutil;

pub use abi::{
    cm_return, frame_type_str, layer_num, level_idc, nal_unit_type, nalu_type_str,
    parameter_set_strategy, profile_idc, video_format_type, EComplexityMode, EUsageType,
    EVideoFrameType, EncoderOption, ISVCEncoder, ISVCEncoderVtbl, OpenH264Version, RcMode,
    SBitrateInfo, SEncParamBase, SEncParamExt, SEncoderStatistics, SFrameBSInfo, SLayerBSInfo,
    SSliceArgument, SSourcePicture, SSpatialLayerConfig, SliceMode, MAX_LAYER_NUM_OF_FRAME,
    MAX_SPATIAL_LAYER_NUM,
};
pub use encoder::{
    annex_b_type_sequence, parse_annex_b, start_code_len, Encoder, EncoderConfig, FrameInfo, Nal,
    Oh264Error, OpenH264Lib, Result, DLL_FILE_NAMES, NO_INTRA_PERIOD,
};
pub use testutil::{digest, synthetic_i420, I420Frame};
