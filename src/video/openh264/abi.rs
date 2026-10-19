//! `#[repr(C)]` reproductions of the OpenH264 **encoder** C ABI.
//!
//! Every type here is transcribed from the authoritative headers shipped with the
//! project:
//!
//! * `.../openh264/codec/api/wels/codec_api.h`
//! * `.../openh264/codec/api/wels/codec_app_def.h`
//! * `.../openh264/codec/api/wels/codec_def.h`
//!
//! Ground truth for sizes / offsets was produced by compiling those headers with MSVC
//! and printing `sizeof` / `offsetof`. The constants in this module are pinned by
//! `const _: () = assert!(..)` blocks *and* by runtime `assert_eq!`s in
//! `tests/abi_layout.rs`, so any transcription mistake fails the build.
//!
//! Layout rules that matter on Windows x64 / MSVC:
//!
//! * an `enum` in a struct is **4 bytes** (`int`); this matters because OpenH264 is
//!   built as C++ where enumeration underlying type is `int`,
//! * a C++ `bool` is **1 byte** here (OpenH264 is compiled with `/Zc:wchar_t` etc. but
//!   no `-fshort-bool`; MSVC `bool` is 1 byte) — the headers even fall back to
//!   `typedef unsigned char bool` in C mode, and the MSVC-printed ground truth
//!   confirms `sizeof(bool) == 1`,
//! * all of these structs have `align == 4` except `SSourcePicture` (8, because of the
//!   `unsigned char* pData[4]` + `long long`), `SLayerBSInfo` (8) and
//!   `SFrameBSInfo` (8),
//! * `EXTAPI` is `__cdecl`; on x64 there is exactly one calling convention, so
//!   `extern "C"` is equivalent.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

// ---------------------------------------------------------------------------
// Sizes / counts (codec_app_def.h)
// ---------------------------------------------------------------------------

pub const MAX_TEMPORAL_LAYER_NUM: usize = 4;
pub const MAX_SPATIAL_LAYER_NUM: usize = 4;
pub(crate) const MAX_QUALITY_LAYER_NUM: usize = 4;
pub const MAX_LAYER_NUM_OF_FRAME: usize = 128;
pub(crate) const MAX_NAL_UNITS_IN_LAYER: usize = 128;

/// `SAVED_NALUNIT_NUM_TMP = 4*4 + 1 + 4 = 21`
pub(crate) const SAVED_NALUNIT_NUM_TMP: usize = MAX_SPATIAL_LAYER_NUM * MAX_QUALITY_LAYER_NUM + 1 + MAX_SPATIAL_LAYER_NUM;
/// `MAX_SLICES_NUM_TMP = (128 - 21) / 3 = 35`
pub(crate) const MAX_SLICES_NUM_TMP: usize = (MAX_NAL_UNITS_IN_LAYER - SAVED_NALUNIT_NUM_TMP) / 3;

pub const AUTO_REF_PIC_COUNT: i32 = -1;
pub const UNSPECIFIED_BIT_RATE: i32 = 0;

// ---------------------------------------------------------------------------
// Enumerations (transcribed literally; `#[repr(i32)]` is the MSVC C++ default)
// ---------------------------------------------------------------------------

/// `EVideoFormatType` (codec_def.h)
pub mod video_format_type {
    pub const VIDEO_FORMAT_RGB: i32 = 1;
    pub const VIDEO_FORMAT_RGBA: i32 = 2;
    pub const VIDEO_FORMAT_RGB555: i32 = 3;
    pub const VIDEO_FORMAT_RGB565: i32 = 4;
    pub const VIDEO_FORMAT_BGR: i32 = 5;
    pub const VIDEO_FORMAT_BGRA: i32 = 6;
    pub const VIDEO_FORMAT_ABGR: i32 = 7;
    pub const VIDEO_FORMAT_ARGB: i32 = 8;
    pub const VIDEO_FORMAT_YUY2: i32 = 20;
    pub const VIDEO_FORMAT_YVYU: i32 = 21;
    pub const VIDEO_FORMAT_UYVY: i32 = 22;
    /// the same as IYUV — the only format OpenH264's encoder really ingests
    pub const VIDEO_FORMAT_I420: i32 = 23;
    pub const VIDEO_FORMAT_YV12: i32 = 24;
    pub const VIDEO_FORMAT_INTERNAL: i32 = 25;
    pub const VIDEO_FORMAT_NV12: i32 = 26;
}

/// `EVideoFrameType` (codec_def.h)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EVideoFrameType {
    Invalid = 0,
    /// IDR frame in H.264
    Idr = 1,
    IFrame = 2,
    PFrame = 3,
    /// skip the frame based encoder kernel
    Skip = 4,
    IpMixed = 5,
}

/// `CM_RETURN` (codec_def.h) — return code of every `ISVCEncoder` method.
pub mod cm_return {
    pub const CM_RESULT_SUCCESS: i32 = 0;
    pub const CM_INIT_PARA_ERROR: i32 = 1;
    pub const CM_UNKNOWN_REASON: i32 = 2;
    pub const CM_MALLOC_MEM_ERROR: i32 = 3;
    pub const CM_INIT_EXPECTED: i32 = 4;
    pub const CM_UNSUPPORTED_DATA: i32 = 5;
}

/// `ENalUnitType` (codec_def.h) — the 5 low bits of the NAL header.
pub mod nal_unit_type {
    pub const NAL_UNKNOWN: u8 = 0;
    pub const NAL_SLICE: u8 = 1;
    pub const NAL_SLICE_DPA: u8 = 2;
    pub const NAL_SLICE_DPB: u8 = 3;
    pub const NAL_SLICE_DPC: u8 = 4;
    pub const NAL_SLICE_IDR: u8 = 5;
    pub const NAL_SEI: u8 = 6;
    pub const NAL_SPS: u8 = 7;
    pub const NAL_PPS: u8 = 8;
    pub const NAL_AUD: u8 = 9;
    pub const NAL_END_SEQUENCE: u8 = 10;
    pub const NAL_END_STREAM: u8 = 11;
    pub const NAL_FILLER_DATA: u8 = 12;
    pub const NAL_SPS_EXT: u8 = 13;
    /// OpenH264's SVC prefix NAL (only emitted when `bPrefixNalAddingCtrl` is on)
    pub const NAL_PREFIX: u8 = 14;
    pub const NAL_SUBSET_SPS: u8 = 15;
}

/// `EUsageType` (codec_app_def.h)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EUsageType {
    CameraVideoRealTime = 0,
    ScreenContentRealTime = 1,
    CameraVideoNonRealTime = 2,
    ScreenContentNonRealTime = 3,
    InputContentTypeAll = 4,
}

/// `ECOMPLEXITY_MODE` (codec_app_def.h) — this is the "encoder speed" knob.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EComplexityMode {
    /// the lowest complexity, the fastest speed
    Low = 0,
    Medium = 1,
    /// high complexity, lowest speed, high quality
    High = 2,
}

/// `RC_MODES` (codec_app_def.h)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcMode {
    Quality = 0,
    Bitrate = 1,
    BufferBased = 2,
    Timestamp = 3,
    BitratePostSkip = 4,
    Off = -1,
}

/// `EProfileIdc` (codec_app_def.h)
pub mod profile_idc {
    pub const PRO_UNKNOWN: i32 = 0;
    pub const PRO_BASELINE: i32 = 66;
    pub const PRO_MAIN: i32 = 77;
    pub const PRO_EXTENDED: i32 = 88;
    pub const PRO_HIGH: i32 = 100;
}

/// `ELevelIdc` (codec_app_def.h)
pub mod level_idc {
    pub const LEVEL_UNKNOWN: i32 = 0;
    pub const LEVEL_3_1: i32 = 31;
    pub const LEVEL_4_0: i32 = 40;
    pub const LEVEL_4_1: i32 = 41;
    pub const LEVEL_5_0: i32 = 50;
}

/// `SliceModeEnum` (codec_app_def.h)
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceMode {
    SingleSlice = 0,
    FixedSlcNumSlice = 1,
    RasterSlice = 2,
    SizeLimitedSlice = 3,
}

/// `EParameterSetStrategy` (codec_app_def.h)
pub mod parameter_set_strategy {
    pub const CONSTANT_ID: i32 = 0;
    pub const INCREASING_ID: i32 = 1;
    pub const SPS_LISTING: i32 = 2;
    pub const SPS_LISTING_AND_PPS_INCREASING: i32 = 3;
    pub const SPS_PPS_LISTING: i32 = 6;
}

/// `ENCODER_OPTION` (codec_app_def.h) — values are pinned by the header's declaration order.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderOption {
    DataFormat = 0,
    IdrInterval = 1,
    SvcEncodeParamBase = 2,
    SvcEncodeParamExt = 3,
    FrameRate = 4,
    Bitrate = 5,
    MaxBitrate = 6,
    InterSpatialPred = 7,
    RcMode = 8,
    RcFrameSkip = 9,
    PaddingPadding = 10,
    Profile = 11,
    Level = 12,
    NumberRef = 13,
    DeliveryStatus = 14,
    LtrRecoveryRequest = 15,
    LtrMarkingFeedback = 16,
    LtrMarkingPeriod = 17,
    Ltr = 18,
    /// "编码速度" 档位: LOW/MEDIUM/HIGH_COMPLEXITY
    Complexity = 19,
    EnableSsei = 20,
    EnablePrefixNalAdding = 21,
    SpsPpsIdStrategy = 22,
    CurrentPath = 23,
    DumpFile = 24,
    TraceLevel = 25,
    TraceCallback = 26,
    TraceCallbackContext = 27,
    /// read only
    GetStatistics = 28,
    StatisticsLogInterval = 29,
    IsLosslessLink = 30,
    BitsVaryPercentage = 31,
}

/// `OpenH264Version` (codec_app_def.h) — returned *by value* from `WelsGetCodecVersion`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenH264Version {
    pub uMajor: u32,
    pub uMinor: u32,
    pub uRevision: u32,
    pub uReserved: u32,
}

impl std::fmt::Display for OpenH264Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{} (reserved={})", self.uMajor, self.uMinor, self.uRevision, self.uReserved)
    }
}

// ---------------------------------------------------------------------------
// SSliceArgument
// ---------------------------------------------------------------------------

/// `SSliceArgument` (codec_app_def.h)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SSliceArgument {
    /// `SliceModeEnum`, 4 bytes
    pub uiSliceMode: i32,
    /// only used when `uiSliceMode == FixedSlcNumSlice`; 0 means "auto by cpu core count"
    pub uiSliceNum: u32,
    /// only used when `uiSliceMode == RasterSlice`
    pub uiSliceMbNum: [u32; MAX_SLICES_NUM_TMP],
    /// only used when `uiSliceMode == SizeLimitedSlice` (header comment says mode 4)
    pub uiSliceSizeConstraint: u32,
}

const _: () = assert!(core::mem::size_of::<SSliceArgument>() == 152);
const _: () = assert!(core::mem::align_of::<SSliceArgument>() == 4);
const _: () = assert!(core::mem::offset_of!(SSliceArgument, uiSliceMode) == 0);
const _: () = assert!(core::mem::offset_of!(SSliceArgument, uiSliceNum) == 4);
const _: () = assert!(core::mem::offset_of!(SSliceArgument, uiSliceMbNum) == 8);
const _: () = assert!(core::mem::offset_of!(SSliceArgument, uiSliceSizeConstraint) == 148);
const _: () = assert!(MAX_SLICES_NUM_TMP == 35);

impl Default for SSliceArgument {
    fn default() -> Self {
        SSliceArgument {
            uiSliceMode: SliceMode::SingleSlice as i32,
            uiSliceNum: 1,
            uiSliceMbNum: [0; MAX_SLICES_NUM_TMP],
            uiSliceSizeConstraint: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// SSpatialLayerConfig
// ---------------------------------------------------------------------------

/// `SSpatialLayerConfig` (codec_app_def.h)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SSpatialLayerConfig {
    pub iVideoWidth: i32,
    pub iVideoHeight: i32,
    pub fFrameRate: f32,
    pub iSpatialBitrate: i32,
    pub iMaxSpatialBitrate: i32,
    /// `EProfileIdc`, `PRO_UNKNOWN` for auto-detection
    pub uiProfileIdc: i32,
    /// `ELevelIdc`, 0 for auto-detection
    pub uiLevelIdc: i32,
    pub iDLayerQp: i32,
    pub sSliceArgument: SSliceArgument,
    pub bVideoSignalTypePresent: bool,
    pub uiVideoFormat: u8,
    pub bFullRange: bool,
    pub bColorDescriptionPresent: bool,
    pub uiColorPrimaries: u8,
    pub uiTransferCharacteristics: u8,
    pub uiColorMatrix: u8,
    pub bAspectRatioPresent: bool,
    /// `ESampleAspectRatio`
    pub eAspectRatio: i32,
    pub sAspectRatioExtWidth: u16,
    pub sAspectRatioExtHeight: u16,
}

const _: () = assert!(core::mem::size_of::<SSpatialLayerConfig>() == 200);
const _: () = assert!(core::mem::align_of::<SSpatialLayerConfig>() == 4);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, iVideoWidth) == 0);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, iVideoHeight) == 4);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, fFrameRate) == 8);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, iSpatialBitrate) == 12);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, iMaxSpatialBitrate) == 16);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, uiProfileIdc) == 20);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, uiLevelIdc) == 24);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, iDLayerQp) == 28);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, sSliceArgument) == 32);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, bVideoSignalTypePresent) == 184);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, uiVideoFormat) == 185);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, bFullRange) == 186);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, bColorDescriptionPresent) == 187);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, uiColorPrimaries) == 188);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, uiTransferCharacteristics) == 189);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, uiColorMatrix) == 190);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, bAspectRatioPresent) == 191);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, eAspectRatio) == 192);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, sAspectRatioExtWidth) == 196);
const _: () = assert!(core::mem::offset_of!(SSpatialLayerConfig, sAspectRatioExtHeight) == 198);

impl Default for SSpatialLayerConfig {
    fn default() -> Self {
        SSpatialLayerConfig {
            iVideoWidth: 0,
            iVideoHeight: 0,
            fFrameRate: 0.0,
            iSpatialBitrate: 0,
            iMaxSpatialBitrate: 0,
            uiProfileIdc: profile_idc::PRO_UNKNOWN,
            uiLevelIdc: level_idc::LEVEL_UNKNOWN,
            iDLayerQp: 0,
            sSliceArgument: SSliceArgument::default(),
            bVideoSignalTypePresent: false,
            uiVideoFormat: 0,
            bFullRange: false,
            bColorDescriptionPresent: false,
            uiColorPrimaries: 0,
            uiTransferCharacteristics: 0,
            uiColorMatrix: 0,
            bAspectRatioPresent: false,
            eAspectRatio: 0,
            sAspectRatioExtWidth: 0,
            sAspectRatioExtHeight: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// SEncParamBase / SEncParamExt
// ---------------------------------------------------------------------------

/// `SEncParamBase` (codec_app_def.h) — 6 fields, exactly 24 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SEncParamBase {
    /// `EUsageType`
    pub iUsageType: i32,
    pub iPicWidth: i32,
    pub iPicHeight: i32,
    pub iTargetBitrate: i32,
    /// `RC_MODES`
    pub iRCMode: i32,
    pub fMaxFrameRate: f32,
}

const _: () = assert!(core::mem::size_of::<SEncParamBase>() == 24);
const _: () = assert!(core::mem::align_of::<SEncParamBase>() == 4);
const _: () = assert!(core::mem::offset_of!(SEncParamBase, iUsageType) == 0);
const _: () = assert!(core::mem::offset_of!(SEncParamBase, iPicWidth) == 4);
const _: () = assert!(core::mem::offset_of!(SEncParamBase, iPicHeight) == 8);
const _: () = assert!(core::mem::offset_of!(SEncParamBase, iTargetBitrate) == 12);
const _: () = assert!(core::mem::offset_of!(SEncParamBase, iRCMode) == 16);
const _: () = assert!(core::mem::offset_of!(SEncParamBase, fMaxFrameRate) == 20);

/// `SEncParamExt` (codec_app_def.h) — exactly 924 bytes, align 4.
///
/// **Never build this from all-zeroes**: some fields (notably `iSpatialLayerNum`,
/// the slice mode and the SPS/PPS strategy) have no useful zero default, and
/// `InitializeExt` will reject the result. Always start from
/// `ISVCEncoder::GetDefaultParams` (see `Encoder::new`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SEncParamExt {
    /// `EUsageType`
    pub iUsageType: i32,
    pub iPicWidth: i32,
    pub iPicHeight: i32,
    pub iTargetBitrate: i32,
    /// `RC_MODES`
    pub iRCMode: i32,
    pub fMaxFrameRate: f32,

    pub iTemporalLayerNum: i32,
    pub iSpatialLayerNum: i32,
    pub sSpatialLayers: [SSpatialLayerConfig; MAX_SPATIAL_LAYER_NUM],

    /// `ECOMPLEXITY_MODE`
    pub iComplexityMode: i32,
    pub uiIntraPeriod: u32,
    pub iNumRefFrame: i32,
    /// `EParameterSetStrategy`
    pub eSpsPpsIdStrategy: i32,
    pub bPrefixNalAddingCtrl: bool,
    pub bEnableSSEI: bool,
    pub bSimulcastAVC: bool,
    pub iPaddingFlag: i32,
    /// 0: CAVLC, 1: CABAC
    pub iEntropyCodingModeFlag: i32,

    /* rc control */
    pub bEnableFrameSkip: bool,
    pub iMaxBitrate: i32,
    pub iMaxQp: i32,
    pub iMinQp: i32,
    pub uiMaxNalSize: u32,

    /* LTR settings */
    pub bEnableLongTermReference: bool,
    pub iLTRRefNum: i32,
    pub iLtrMarkPeriod: u32,

    /* multi-thread settings */
    /// 1: 0 => auto (internal dynamic), 1 => multi-thread disabled, >1 => thread count
    pub iMultipleThreadIdc: u16,
    pub bUseLoadBalancing: bool,

    /* deblocking loop filter */
    pub iLoopFilterDisableIdc: i32,
    pub iLoopFilterAlphaC0Offset: i32,
    pub iLoopFilterBetaOffset: i32,

    /* pre-processing feature */
    pub bEnableDenoise: bool,
    pub bEnableBackgroundDetection: bool,
    pub bEnableAdaptiveQuant: bool,
    pub bEnableFrameCroppingFlag: bool,
    pub bEnableSceneChangeDetect: bool,

    pub bIsLosslessLink: bool,
    pub bFixRCOverShoot: bool,
    pub iIdrBitrateRatio: i32,
    pub bPsnrY: bool,
    pub bPsnrU: bool,
    pub bPsnrV: bool,
}

const _: () = assert!(core::mem::size_of::<SEncParamExt>() == 924);
const _: () = assert!(core::mem::align_of::<SEncParamExt>() == 4);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iUsageType) == 0);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iPicWidth) == 4);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iPicHeight) == 8);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iTargetBitrate) == 12);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iRCMode) == 16);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, fMaxFrameRate) == 20);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iTemporalLayerNum) == 24);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iSpatialLayerNum) == 28);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, sSpatialLayers) == 32);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iComplexityMode) == 832);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, uiIntraPeriod) == 836);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iNumRefFrame) == 840);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, eSpsPpsIdStrategy) == 844);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bPrefixNalAddingCtrl) == 848);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableSSEI) == 849);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bSimulcastAVC) == 850);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iPaddingFlag) == 852);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iEntropyCodingModeFlag) == 856);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableFrameSkip) == 860);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iMaxBitrate) == 864);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iMaxQp) == 868);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iMinQp) == 872);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, uiMaxNalSize) == 876);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableLongTermReference) == 880);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iLTRRefNum) == 884);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iLtrMarkPeriod) == 888);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iMultipleThreadIdc) == 892);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bUseLoadBalancing) == 894);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iLoopFilterDisableIdc) == 896);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iLoopFilterAlphaC0Offset) == 900);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iLoopFilterBetaOffset) == 904);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableDenoise) == 908);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableBackgroundDetection) == 909);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableAdaptiveQuant) == 910);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableFrameCroppingFlag) == 911);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bEnableSceneChangeDetect) == 912);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bIsLosslessLink) == 913);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bFixRCOverShoot) == 914);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, iIdrBitrateRatio) == 916);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bPsnrY) == 920);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bPsnrU) == 921);
const _: () = assert!(core::mem::offset_of!(SEncParamExt, bPsnrV) == 922);

impl Default for SEncParamExt {
    fn default() -> Self {
        SEncParamExt {
            iUsageType: EUsageType::CameraVideoRealTime as i32,
            iPicWidth: 0,
            iPicHeight: 0,
            iTargetBitrate: 0,
            iRCMode: RcMode::Quality as i32,
            fMaxFrameRate: 0.0,
            iTemporalLayerNum: 1,
            iSpatialLayerNum: 1,
            sSpatialLayers: [SSpatialLayerConfig::default(); MAX_SPATIAL_LAYER_NUM],
            iComplexityMode: EComplexityMode::Medium as i32,
            uiIntraPeriod: 0,
            iNumRefFrame: AUTO_REF_PIC_COUNT,
            eSpsPpsIdStrategy: parameter_set_strategy::CONSTANT_ID,
            bPrefixNalAddingCtrl: false,
            bEnableSSEI: false,
            bSimulcastAVC: false,
            iPaddingFlag: 0,
            iEntropyCodingModeFlag: 0,
            bEnableFrameSkip: false,
            iMaxBitrate: UNSPECIFIED_BIT_RATE,
            iMaxQp: 0,
            iMinQp: 0,
            uiMaxNalSize: 0,
            bEnableLongTermReference: false,
            iLTRRefNum: 0,
            iLtrMarkPeriod: 0,
            iMultipleThreadIdc: 1,
            bUseLoadBalancing: false,
            iLoopFilterDisableIdc: 0,
            iLoopFilterAlphaC0Offset: 0,
            iLoopFilterBetaOffset: 0,
            bEnableDenoise: false,
            bEnableBackgroundDetection: false,
            bEnableAdaptiveQuant: false,
            bEnableFrameCroppingFlag: false,
            bEnableSceneChangeDetect: false,
            bIsLosslessLink: false,
            bFixRCOverShoot: false,
            iIdrBitrateRatio: 0,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        }
    }
}

// ---------------------------------------------------------------------------
// SSourcePicture
// ---------------------------------------------------------------------------

/// `SSourcePicture` (codec_app_def.h) — 80 bytes, **align 8**.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SSourcePicture {
    /// `EVideoFormatType`; must be `videoFormatI420` for this crate
    pub iColorFormat: i32,
    pub iStride: [i32; 4],
    pub pData: [*mut u8; 4],
    pub iPicWidth: i32,
    pub iPicHeight: i32,
    /// timestamp of the source picture, unit: millisecond
    pub uiTimeStamp: i64,
    pub bPsnrY: bool,
    pub bPsnrU: bool,
    pub bPsnrV: bool,
}

const _: () = assert!(core::mem::size_of::<SSourcePicture>() == 80);
const _: () = assert!(core::mem::align_of::<SSourcePicture>() == 8);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, iColorFormat) == 0);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, iStride) == 4);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, pData) == 24);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, iPicWidth) == 56);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, iPicHeight) == 60);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, uiTimeStamp) == 64);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, bPsnrY) == 72);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, bPsnrU) == 73);
const _: () = assert!(core::mem::offset_of!(SSourcePicture, bPsnrV) == 74);

impl Default for SSourcePicture {
    fn default() -> Self {
        SSourcePicture {
            iColorFormat: video_format_type::VIDEO_FORMAT_I420,
            iStride: [0; 4],
            pData: [core::ptr::null_mut(); 4],
            iPicWidth: 0,
            iPicHeight: 0,
            uiTimeStamp: 0,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        }
    }
}

// ---------------------------------------------------------------------------
// SLayerBSInfo / SFrameBSInfo
// ---------------------------------------------------------------------------

/// `SLayerBSInfo` (codec_app_def.h) — 56 bytes, **align 8**.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SLayerBSInfo {
    pub uiTemporalId: u8,
    pub uiSpatialId: u8,
    pub uiQualityId: u8,
    /// `EVideoFrameType`, 4 bytes
    pub eFrameType: i32,
    pub uiLayerType: u8,
    pub iSubSeqId: i32,
    pub iNalCount: i32,
    /// length of NAL size in byte from 0 to `iNalCount - 1`; **includes the start code**
    pub pNalLengthInByte: *mut i32,
    /// buffer of bitstream contained; each NAL is prefixed by `00 00 00 01`
    pub pBsBuf: *mut u8,
    pub rPsnr: [f32; 3],
}

const _: () = assert!(core::mem::size_of::<SLayerBSInfo>() == 56);
const _: () = assert!(core::mem::align_of::<SLayerBSInfo>() == 8);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, uiTemporalId) == 0);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, uiSpatialId) == 1);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, uiQualityId) == 2);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, eFrameType) == 4);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, uiLayerType) == 8);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, iSubSeqId) == 12);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, iNalCount) == 16);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, pNalLengthInByte) == 24);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, pBsBuf) == 32);
const _: () = assert!(core::mem::offset_of!(SLayerBSInfo, rPsnr) == 40);

impl Default for SLayerBSInfo {
    fn default() -> Self {
        SLayerBSInfo {
            uiTemporalId: 0,
            uiSpatialId: 0,
            uiQualityId: 0,
            eFrameType: 0,
            uiLayerType: 0,
            iSubSeqId: 0,
            iNalCount: 0,
            pNalLengthInByte: core::ptr::null_mut(),
            pBsBuf: core::ptr::null_mut(),
            rPsnr: [0.0; 3],
        }
    }
}

/// `SBitrateInfo` (codec_app_def.h) — 8 bytes.
///
/// This is the payload of `ENCODER_OPTION_BITRATE` and `ENCODER_OPTION_MAX_BITRATE`.
/// **It is not an `int`**: passing a bare `int*` makes `SetOption` read the bitrate out
/// of `iLayer` and then fail with `cmInitParaError`, logging the garbage it read. That
/// mistake was made and caught here — see the `set_bitrate` regression test.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct SBitrateInfo {
    /// `LAYER_NUM`; `LAYER_ALL` (4) means "all layers, keep the inter-layer ratio"
    pub iLayer: i32,
    /// the target bitrate, in bits per second
    pub iBitrate: i32,
}

const _: () = assert!(core::mem::size_of::<SBitrateInfo>() == 8);
const _: () = assert!(core::mem::offset_of!(SBitrateInfo, iLayer) == 0);
const _: () = assert!(core::mem::offset_of!(SBitrateInfo, iBitrate) == 4);

/// `LAYER_NUM` (codec_app_def.h)
pub mod layer_num {
    pub const SPATIAL_LAYER_0: i32 = 0;
    pub const SPATIAL_LAYER_1: i32 = 1;
    pub const SPATIAL_LAYER_2: i32 = 2;
    pub const SPATIAL_LAYER_3: i32 = 3;
    /// "all layers" — the only value that is safe when the layer count is unknown
    pub const SPATIAL_LAYER_ALL: i32 = 4;
}

/// `SEncoderStatistics` (codec_app_def.h).
///
/// Note `unsigned long` in the header is **4 bytes** on Windows (LLP64), unlike Linux
/// where it is 8. Getting that wrong shifts the trailing fields.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct SEncoderStatistics {
    pub uiWidth: u32,
    pub uiHeight: u32,
    pub fAverageFrameSpeedInMs: f32,
    pub fAverageFrameRate: f32,
    pub fLatestFrameRate: f32,
    pub uiBitRate: u32,
    pub uiAverageFrameQP: u32,
    pub uiInputFrameCount: u32,
    pub uiSkippedFrameCount: u32,
    pub uiResolutionChangeTimes: u32,
    pub uiIDRReqNum: u32,
    pub uiIDRSentNum: u32,
    pub uiLTRSentNum: u32,
    pub iStatisticsTs: i64,
    /// `unsigned long` on Windows == `u32`
    pub iTotalEncodedBytes: u32,
    pub iLastStatisticsBytes: u32,
    pub iLastStatisticsFrameCount: u32,
}

const _: () = assert!(core::mem::size_of::<SEncoderStatistics>() == 80);
const _: () = assert!(core::mem::align_of::<SEncoderStatistics>() == 8);

impl SEncoderStatistics {
}

/// `SFrameBSInfo` (codec_app_def.h) — 7192 bytes, **align 8**.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SFrameBSInfo {
    pub iLayerNum: i32,
    pub sLayerInfo: [SLayerBSInfo; MAX_LAYER_NUM_OF_FRAME],
    /// `EVideoFrameType`
    pub eFrameType: i32,
    pub iFrameSizeInBytes: i32,
    pub uiTimeStamp: i64,
}

const _: () = assert!(core::mem::size_of::<SFrameBSInfo>() == 7192);
const _: () = assert!(core::mem::align_of::<SFrameBSInfo>() == 8);
const _: () = assert!(core::mem::offset_of!(SFrameBSInfo, iLayerNum) == 0);
const _: () = assert!(core::mem::offset_of!(SFrameBSInfo, sLayerInfo) == 8);
const _: () = assert!(core::mem::offset_of!(SFrameBSInfo, eFrameType) == 7176);
const _: () = assert!(core::mem::offset_of!(SFrameBSInfo, iFrameSizeInBytes) == 7180);
const _: () = assert!(core::mem::offset_of!(SFrameBSInfo, uiTimeStamp) == 7184);

impl Default for SFrameBSInfo {
    fn default() -> Self {
        SFrameBSInfo {
            iLayerNum: 0,
            sLayerInfo: [SLayerBSInfo::default(); MAX_LAYER_NUM_OF_FRAME],
            eFrameType: 0,
            iFrameSizeInBytes: 0,
            uiTimeStamp: 0,
        }
    }
}

impl SFrameBSInfo {
    /// Zeros the *metadata* the encoder writes back, leaving the (large) `sLayerInfo`
    /// array untouched. `SFrameBSInfo` is 7 KiB, so re-zeroing it per frame is pure
    /// memset overhead; the encoder fully overwrites `sLayerInfo[0..iLayerNum]` and
    /// reuses its own static per-layer buffers, so only these fields must be reset.
    #[inline]
    pub fn reset_for_reuse(&mut self) {
        self.iLayerNum = 0;
        self.eFrameType = 0;
        self.iFrameSizeInBytes = 0;
        self.uiTimeStamp = 0;
    }
}

// ---------------------------------------------------------------------------
// ISVCEncoder vtable
// ---------------------------------------------------------------------------

/// The `ISVCEncoder` opaque object. In the Itanium/MSVC C++ ABI the object's first
/// 8 bytes *are* the vtable pointer, so `*mut ISVCEncoderVtbl` is the correct
/// representation of `ISVCEncoder*`.
#[repr(C)]
pub struct ISVCEncoder {
    pub vtable: *const ISVCEncoderVtbl,
}

/// `ISVCEncoder` virtual method table, in declaration order (10 slots).
///
/// Slot indices are pinned by the ground truth in the task brief:
/// `0 Initialize, 1 InitializeExt, 2 GetDefaultParams, 3 Uninitialize,
///  4 EncodeFrame, 5 EncodeParameterSets, 6 ForceIntraFrame, 7 SetOption,
///  8 GetOption, 9 ~ISVCEncoder`.
///
/// Note slot 6: the header declares `ForceIntraFrame(bool bIDR, int iLayerId = -1)`
/// with a *default argument*. A default argument does **not** create a second entry
/// in the vtable — the real signature has both parameters, so the Rust function
/// pointer below must take both.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ISVCEncoderVtbl {
    pub Initialize: unsafe extern "C" fn(*mut ISVCEncoder, *const SEncParamBase) -> i32,
    pub InitializeExt: unsafe extern "C" fn(*mut ISVCEncoder, *const SEncParamExt) -> i32,
    pub GetDefaultParams: unsafe extern "C" fn(*mut ISVCEncoder, *mut SEncParamExt) -> i32,
    pub Uninitialize: unsafe extern "C" fn(*mut ISVCEncoder) -> i32,
    pub EncodeFrame: unsafe extern "C" fn(*mut ISVCEncoder, *const SSourcePicture, *mut SFrameBSInfo) -> i32,
    pub EncodeParameterSets: unsafe extern "C" fn(*mut ISVCEncoder, *mut SFrameBSInfo) -> i32,
    pub ForceIntraFrame: unsafe extern "C" fn(*mut ISVCEncoder, u8, i32) -> i32,
    pub SetOption: unsafe extern "C" fn(*mut ISVCEncoder, i32, *mut core::ffi::c_void) -> i32,
    pub GetOption: unsafe extern "C" fn(*mut ISVCEncoder, i32, *mut core::ffi::c_void) -> i32,
    /// `~ISVCEncoder()` — this crate never calls it; destruction always goes through
    /// `WelsDestroySVCEncoder` (which runs the destructor itself).
    pub Destructor: unsafe extern "C" fn(*mut ISVCEncoder),
}

const _: () = assert!(core::mem::size_of::<ISVCEncoderVtbl>() == 10 * 8);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, Initialize) == 0);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, InitializeExt) == 8);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, GetDefaultParams) == 16);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, Uninitialize) == 24);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, EncodeFrame) == 32);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, EncodeParameterSets) == 40);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, ForceIntraFrame) == 48);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, SetOption) == 56);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, GetOption) == 64);
const _: () = assert!(core::mem::offset_of!(ISVCEncoderVtbl, Destructor) == 72);

/// `int WelsCreateSVCEncoder(ISVCEncoder** ppEncoder)`
pub type WelsCreateSVCEncoderFn = unsafe extern "C" fn(*mut *mut ISVCEncoder) -> i32;
/// `void WelsDestroySVCEncoder(ISVCEncoder* pEncoder)`
pub type WelsDestroySVCEncoderFn = unsafe extern "C" fn(*mut ISVCEncoder);
/// `OpenH264Version WelsGetCodecVersion(void)` — returns a 16-byte struct **by value**.
///
/// On Windows x64 a 16-byte POD struct containing only integers is returned in `RAX:RDX`
/// (`uMajor`/`uMinor` in RAX low/high, `uRevision`/`uReserved` in RDX), which is exactly
/// what `extern "C"` does for a `#[repr(C)]` 16-byte all-integer struct.
pub type WelsGetCodecVersionFn = unsafe extern "C" fn() -> OpenH264Version;

/// Human-readable name of a `CM_RETURN` code.
pub fn cm_return_str(code: i32) -> &'static str {
    match code {
        cm_return::CM_RESULT_SUCCESS => "cmResultSuccess",
        cm_return::CM_INIT_PARA_ERROR => "cmInitParaError (parameters are invalid)",
        cm_return::CM_UNKNOWN_REASON => "cmUnknownReason",
        cm_return::CM_MALLOC_MEM_ERROR => "cmMallocMemeError",
        cm_return::CM_INIT_EXPECTED => "cmInitExpected",
        cm_return::CM_UNSUPPORTED_DATA => "cmUnsupportedData",
        _ => "unknown CM_RETURN code",
    }
}

/// Human-readable name of an `EVideoFrameType` value.
pub fn frame_type_str(v: i32) -> &'static str {
    match v {
        0 => "videoFrameTypeInvalid",
        1 => "videoFrameTypeIDR",
        2 => "videoFrameTypeI",
        3 => "videoFrameTypeP",
        4 => "videoFrameTypeSkip",
        5 => "videoFrameTypeIPMixed",
        _ => "unknown",
    }
}

/// Human-readable name of a NAL unit type (the 5 low bits of the NAL header byte).
pub fn nalu_type_str(t: u8) -> &'static str {
    match t {
        0 => "UNKNOWN",
        1 => "SLICE",
        2 => "SLICE_DPA",
        3 => "SLICE_DPB",
        4 => "SLICE_DPC",
        5 => "IDR",
        6 => "SEI",
        7 => "SPS",
        8 => "PPS",
        9 => "AUD",
        10 => "END_SEQUENCE",
        11 => "END_STREAM",
        12 => "FILLER",
        13 => "SPS_EXT",
        14 => "PREFIX",
        15 => "SUBSET_SPS",
        _ => "RESERVED",
    }
}
