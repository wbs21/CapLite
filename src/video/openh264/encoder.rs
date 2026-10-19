//! Safe, RAII `Encoder` wrapper over the OpenH264 `ISVCEncoder` C++ interface.
//!
//! # Why this module exists at all
//!
//! `openh264.dll` exports **only six** symbols:
//!
//! ```text
//! WelsCreateSVCEncoder  WelsDestroySVCEncoder
//! WelsCreateDecoder     WelsDestroyDecoder
//! WelsGetCodecVersion   WelsGetCodecVersionEx
//! ```
//!
//! There is no `WelsEncodeFrame`, no `WelsSetEncoderParam`, no flat C API for
//! encoding. Everything else is reached through the `ISVCEncoder` **virtual method
//! table**, which is why this module declares [`ISVCEncoderVtbl`]
//! by hand and calls through it.
//!
//! The bundled `openh264.lib` is an *import library for `openh264-7.dll`*, a different
//! file name than the bundled `openh264.dll`, and it contains no encoder symbols — so
//! linking against it is a dead end. We therefore load the DLL at **runtime** with
//! `LoadLibraryW` + `GetProcAddress`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use super::abi::*;
use super::win;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Oh264Error {
    /// No usable `openh264.dll` was found. Carries every path/name that was tried.
    LibraryNotFound { tried: Vec<String> },
    /// `LoadLibraryW` failed for a path that exists (BadImageFormat / access denied ...).
    LibraryLoadFailed { path: String, os_error: String },
    /// `GetProcAddress` could not find a required export.
    MissingSymbol { symbol: String, path: String },
    /// Any `ISVCEncoder` method returned a non-zero `CM_RETURN`.
    CodecApi { call: &'static str, code: i32 },
    /// A caller-supplied frame did not match the configuration.
    InvalidFrame(String),
    /// Input buffers were too small / strides too tight.
    BufferTooSmall(String),
    /// The bitstream returned by OpenH264 did not have the expected start-code layout.
    BadBitstream(String),
    /// `WelsCreateSVCEncoder` returned non-zero or produced a NULL pointer.
    CreateFailed { code: i32 },
    /// The object has already been torn down.
    Poisoned(&'static str),
}

impl std::fmt::Display for Oh264Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Oh264Error::LibraryNotFound { tried } => {
                write!(
                    f,
                    "could not load the OpenH264 DLL: none of the following candidates could be loaded:\n  {}",
                    tried.join("\n  ")
                )
            }
            Oh264Error::LibraryLoadFailed { path, os_error } => write!(
                f,
                "LoadLibraryW(\"{path}\") failed: {os_error}. \
                 The file exists but is not loadable (wrong architecture? 32-bit DLL? blocked by policy?)"
            ),
            Oh264Error::MissingSymbol { symbol, path } => write!(
                f,
                "GetProcAddress(\"{symbol}\") failed in \"{path}\". \
                 This DLL does not export the OpenH264 encoder entry points."
            ),
            Oh264Error::CodecApi { call, code } => {
                write!(f, "ISVCEncoder::{call} returned {code} ({})", cm_return_str(*code))
            }
            Oh264Error::InvalidFrame(m) => write!(f, "invalid source frame: {m}"),
            Oh264Error::BufferTooSmall(m) => write!(f, "input buffer too small: {m}"),
            Oh264Error::BadBitstream(m) => write!(f, "unexpected output bitstream layout: {m}"),
            Oh264Error::CreateFailed { code } => {
                write!(f, "WelsCreateSVCEncoder failed with code {code}")
            }
            Oh264Error::Poisoned(m) => write!(f, "encoder already destroyed: {m}"),
        }
    }
}

impl std::error::Error for Oh264Error {}

pub type Result<T> = std::result::Result<T, Oh264Error>;

// ---------------------------------------------------------------------------
// NAL units
// ---------------------------------------------------------------------------

/// One NAL unit exactly as OpenH264 emitted it, **including its start code**.
///
/// `data` is a verbatim `00 00 00 01 <header> <payload...>` slice, so concatenating
/// `Nal::data` for every NAL of every frame yields a valid Annex-B elementary stream.
#[derive(Clone, PartialEq, Eq)]
pub struct Nal {
    /// Full NAL bytes including the start code.
    pub data: Vec<u8>,
    /// The 5 low bits of the NAL header byte (`7` = SPS, `8` = PPS, `5` = IDR slice, `1` = non-IDR slice ...).
    pub nalu_type: u8,
    /// `nal_ref_idc`, the 2 bits above `nalu_type`.
    pub nal_ref_idc: u8,
}

impl Nal {
    /// Builds a `Nal` from a buffer that starts with a start code, deriving `nalu_type`.
    pub fn from_annex_b(data: Vec<u8>) -> Result<Self> {
        let (sc_len, header_off) = start_code_len(&data).ok_or_else(|| {
            Oh264Error::BadBitstream(format!(
                "NAL does not begin with a start code (first bytes: {})",
                hex_prefix(&data, 8)
            ))
        })?;
        let _ = sc_len;
        let hdr = *data.get(header_off).ok_or_else(|| {
            Oh264Error::BadBitstream("start code at end of buffer with no NAL header byte".to_string())
        })?;
        Ok(Nal {
            nalu_type: hdr & 0x1F,
            nal_ref_idc: (hdr >> 5) & 0x03,
            data,
        })
    }

    /// Length of the leading start code (3 or 4).
    pub fn start_code_len(&self) -> usize {
        start_code_len(&self.data).map(|(n, _)| n).unwrap_or(0)
    }

    /// The NAL bytes with the start code stripped (header byte first).
    pub fn payload(&self) -> &[u8] {
        match start_code_len(&self.data) {
            Some((_, off)) => &self.data[off..],
            None => &self.data[..],
        }
    }

    /// `nalu_type` as a readable name.
    pub fn type_name(&self) -> &'static str {
        nalu_type_str(self.nalu_type)
    }

    pub fn is_sps(&self) -> bool {
        self.nalu_type == nal_unit_type::NAL_SPS
    }
}

impl std::fmt::Debug for Nal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Nal {{ type: {}({}), ref_idc: {}, len: {}, head: {} }}",
            self.nalu_type,
            self.type_name(),
            self.nal_ref_idc,
            self.data.len(),
            hex_prefix(&self.data, 8)
        )
    }
}

/// Returns `(start_code_len, header_offset)` if `buf` begins with `00 00 01` or `00 00 00 01`.
pub fn start_code_len(buf: &[u8]) -> Option<(usize, usize)> {
    if buf.len() >= 4 && buf[0] == 0 && buf[1] == 0 && buf[2] == 0 && buf[3] == 1 {
        Some((4, 4))
    } else if buf.len() >= 3 && buf[0] == 0 && buf[1] == 0 && buf[2] == 1 {
        Some((3, 3))
    } else {
        None
    }
}

fn hex_prefix(buf: &[u8], n: usize) -> String {
    buf.iter()
        .take(n)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Splits one layer's byte buffer into NAL units using the `pNalLengthInByte` array.
///
/// `pBsBuf` is a *concatenation* of `iNalCount` NAL units, each prefixed with a start
/// code; `pNalLengthInByte[i]` is the total byte length of the i-th unit **including**
/// that start code. Both facts were confirmed empirically — see `tests/encode_real.rs`.
///
/// # Safety
/// `bs_buf` must point to at least `sum(lengths)` readable bytes and `lengths` must
/// point to at least `lengths.len()` valid `i32`s.
unsafe fn split_nals(bs_buf: *const u8, lengths: &[i32]) -> Result<Vec<Nal>> {
    let mut out = Vec::with_capacity(lengths.len());
    let mut off: usize = 0;
    for (i, &len) in lengths.iter().enumerate() {
        if len <= 0 {
            return Err(Oh264Error::BadBitstream(format!(
                "layer NAL #{i} has non-positive length {len}"
            )));
        }
        let len = len as usize;
        // SAFETY: the caller guarantees `bs_buf` covers `sum(lengths)` bytes; `off`
        // accumulates exactly the previous lengths, so `off + len` is still in bounds.
        let slice = unsafe { core::slice::from_raw_parts(bs_buf.add(off), len) };
        let nal = Nal::from_annex_b(slice.to_vec()).map_err(|e| {
            Oh264Error::BadBitstream(format!("layer NAL #{i} (len {len} at offset {off}): {e}"))
        })?;
        out.push(nal);
        off += len;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The DLL
// ---------------------------------------------------------------------------

/// Candidate file names, tried in order against each candidate directory.
pub const DLL_FILE_NAMES: &[&str] = &["openh264.dll", "openh264-7.dll"];

/// Relative directories (from the crate root and every ancestor) that have historically
/// held the bundled `openh264.dll`. The `CapLite` tree was reorganised while this crate
/// was written, so several generations of paths are probed rather than one hard-coded
/// location. Everything here is read-only: this crate never writes into `CapLite`.
pub(crate) const DLL_RELATIVE_DIRS: &[&str] = &[
    "out",                  // build.rs staging directory (checked first, crate-local)
    "CapLite/src/include", // original layout
    "CapLite/native",      // current layout
    "CapLite/openh264",    // flattened layout
    "CapLite/libyuv",
    "src/include",
    "native",
];

/// A directory that is searched for the DLL.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut exe_dir: Option<PathBuf> = None;

    // 1. next to the running executable — the normal `cargo run` / shipped-app layout.
    //    For `target/<profile>/examples/foo.exe` and `target/<profile>/deps/foo.exe`
    //    this must beat every other candidate, otherwise a release binary can end up
    //    loading the *debug* copy of the DLL.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            exe_dir = Some(d.to_path_buf());
            // 发布版布局：DLL 在 <exe>\runtime；旧布局/测试可执行文件仍在同级。
            dirs.push(d.join("runtime"));
            dirs.push(d.to_path_buf());
            // ...and its parent, i.e. `target/<profile>/` for an example/test binary.
            if let Some(p) = d.parent() {
                dirs.push(p.to_path_buf());
            }
        }
    }
    // 2. an explicit override, for debugging and CI
    if let Ok(p) = std::env::var("OPENH264_DLL_DIR") {
        if !p.is_empty() {
            dirs.insert(0, PathBuf::from(p));
        }
    }
    // 3. the active profile's directory, whichever it is. When the exe already told us
    //    where the profile lives, prefer that over guessing "debug" then "release".
    if let Ok(td) = std::env::var("CARGO_TARGET_DIR") {
        if !td.is_empty() {
            let td = Path::new(&td);
            if let Some(d) = &exe_dir {
                // `target/<profile>/deps` -> profile dir is the parent
                if let Some(prof) = d.parent() {
                    dirs.push(prof.join("deps"));
                }
            }
            // Fall back to probing both profiles, release first so an optimized run
            // does not silently pick up a stale debug DLL.
            for prof in ["release", "debug"] {
                dirs.push(td.join(prof));
                dirs.push(td.join(prof).join("deps"));
            }
        }
    }
    // 4. current working directory (cargo runs tests and examples from the crate root)
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    // 5. the in-repo locations of the bundled binary, relative to this crate and to
    //    every ancestor. Nothing here is modified — the DLL is only ever read.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut cursor: Option<PathBuf> = Some(manifest.to_path_buf());
    while let Some(dir) = cursor {
        for rel in DLL_RELATIVE_DIRS {
            dirs.push(dir.join(rel));
        }
        cursor = dir.parent().map(|p| p.to_path_buf());
    }

    // de-duplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// An `openh264.dll` loaded at runtime, with the six exported symbols resolved.
#[derive(Debug)]
pub struct OpenH264Lib {
    handle: *mut c_void,
    /// Absolute path this module was loaded from.
    pub path: PathBuf,
    pub version: OpenH264Version,
    create_encoder: WelsCreateSVCEncoderFn,
    destroy_encoder: WelsDestroySVCEncoderFn,
    // P53：原先还有 `get_version` / `get_version_ex` 两个字段，只写不读 ——
    // 它们唯一的读者是 `version_by_symbol` / `version_by_ex_symbol` / `has_version_ex`
    // 三个零调用方法，本轮连同方法一起删了。`get_version` 的**函数指针**仍要在
    // `load()` 里取（用来算出公开的 `version` 字段），但不必再存进结构体；
    // `WelsGetCodecVersionEx` 的 `GetProcAddress` 则完全省掉。
}

// The raw module handle is just an address; every entry point we expose is a plain
// function pointer and all of them are thread-safe in OpenH264 (encoder instances are not).
unsafe impl Send for OpenH264Lib {}
unsafe impl Sync for OpenH264Lib {}

impl OpenH264Lib {
    /// Loads the first working `openh264.dll` from the candidate list.
    ///
    /// On failure the error lists every `(path, reason)` combination that was tried.
    pub fn load() -> Result<Self> {
        let dirs = candidate_dirs();
        // release 只允许 exe 同级或 <exe>\runtime：CWD、CARGO_TARGET_DIR、
        // 编译期源码路径、OPENH264_DLL_DIR 都是开发期便利，不能成为发布版的 DLL 注入面。
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let runtime_dir = exe_dir.as_ref().map(|d| d.join("runtime"));
        let mut tried: Vec<String> = Vec::new();
        let mut saw_existing_but_unloadable = false;

        for dir in &dirs {
            if !cfg!(debug_assertions)
                && exe_dir.as_ref() != Some(dir)
                && runtime_dir.as_ref() != Some(dir)
            {
                continue;
            }
            for name in DLL_FILE_NAMES {
                let full = dir.join(name);
                let exists = full.is_file();
                let Some(full_str) = full.to_str().map(|s| s.to_string()) else {
                    tried.push(format!("{} <-- path is not valid UTF-8", full.display()));
                    continue;
                };
                match Self::load_from(&full_str) {
                    Ok(lib) => return Ok(lib),
                    Err(e) => {
                        if exists {
                            saw_existing_but_unloadable = true;
                            tried.push(format!("{full_str}  <-- file EXISTS but: {e}"));
                        } else {
                            tried.push(format!("{full_str}  <-- not found"));
                        }
                    }
                }
            }
        }

        let hint = if saw_existing_but_unloadable {
            "\n(a file was present but could not be loaded — check that the DLL is x64 and not blocked)"
        } else {
            "\n(hint: run `cargo build` first; build.rs copies the DLL into the target and out/ directories)"
        };
        Err(Oh264Error::LibraryNotFound {
            tried: vec![format!("{}{}", tried.join("\n  "), hint)],
        })
    }

    /// Loads exactly one path. Returns a descriptive error rather than panicking.
    pub fn load_from(path: &str) -> Result<Self> {
        let wide = win::to_wide(path);
        let handle = unsafe { win::LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            return Err(Oh264Error::LibraryLoadFailed {
                path: path.to_string(),
                os_error: win::last_error_message(),
            });
        }

        // Resolve the symbols. Any failure means this is not an OpenH264 DLL.
        macro_rules! sym {
            ($name:literal) => {{
                // SAFETY: `handle` is a module handle returned by a successful
                // LoadLibraryW, so it is live for the rest of this function.
                match unsafe { win::get_proc_address(handle, $name) } {
                    Some(p) => p,
                    None => {
                        // SAFETY: `handle` came from a successful LoadLibraryW above and
                        // has not been freed (this is the failure path of symbol lookup).
                        unsafe { win::free_library(handle) };
                        return Err(Oh264Error::MissingSymbol {
                            symbol: $name.to_string(),
                            path: path.to_string(),
                        });
                    }
                }
            }};
        }

        let create_encoder: WelsCreateSVCEncoderFn = {
            let p = sym!("WelsCreateSVCEncoder");
            // SAFETY: GetProcAddress on a known module returned a non-null address of a
            // function whose signature we transcribed from codec_api.h.
            unsafe { std::mem::transmute::<*mut c_void, WelsCreateSVCEncoderFn>(p) }
        };
        let destroy_encoder: WelsDestroySVCEncoderFn = {
            let p = sym!("WelsDestroySVCEncoder");
            // SAFETY: as above.
            unsafe { std::mem::transmute::<*mut c_void, WelsDestroySVCEncoderFn>(p) }
        };
        let get_version: WelsGetCodecVersionFn = {
            let p = sym!("WelsGetCodecVersion");
            // SAFETY: as above. The return type is a 16-byte all-integer POD struct,
            // which the Win64 ABI returns in RAX:RDX — matching extern "C".
            unsafe { std::mem::transmute::<*mut c_void, WelsGetCodecVersionFn>(p) }
        };

        // P53：`WelsGetCodecVersionEx` 的解析已随 `version_by_ex_symbol` 一并删除
        // （它是可选导出，旧版 openh264 没有；既然没有调用方，就别为它多一次 `GetProcAddress`）。
        let version = unsafe { get_version() };

        Ok(OpenH264Lib {
            handle,
            path: PathBuf::from(path),
            version,
            create_encoder,
            destroy_encoder,
        })
    }

}

impl Drop for OpenH264Lib {
    fn drop(&mut self) {
        // Deliberately a no-op. Calling `FreeLibrary` here would decrement the module's
        // reference count, and a host process may hold its own reference to the very
        // same `openh264.dll`; unloading it out from under that host is a classic
        // use-after-unload crash. The process exit path releases it anyway, and
        // `openh264.dll` owns no OS resources that leak meaningfully.
        //
        // `unload_now()` is provided for the rare caller that really wants the unload.
    }
}

impl OpenH264Lib {
    /// Explicitly unloads the DLL. Only safe when no encoder (from any component,
    /// including other threads) is still alive and no other module needs it.
    ///
    /// # Safety
    /// The caller must guarantee that no `Encoder` created from this library — and no
    /// other code in the process — will touch any OpenH264 symbol afterwards.
    pub unsafe fn unload_now(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: this method's own contract (no encoder from this library is alive).
            unsafe { win::free_library(self.handle) };
            self.handle = core::ptr::null_mut();
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder configuration
// ---------------------------------------------------------------------------

/// Everything `Encoder::new` needs. `Default` targets real-time screen content.
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    pub width: i32,
    pub height: i32,
    pub fps: f32,
    /// target bitrate in **bits per second**
    pub bitrate: i32,
    /// **maximum** bitrate in bits per second: the rate-control ceiling that OpenH264
    /// installs as `SEncParamExt::iMaxBitrate` and as every layer's `iMaxSpatialBitrate`.
    ///
    /// OpenH264 rejects a target above this ceiling (`InitializeExt -> cmInitParaError`),
    /// so callers that want to raise the target at runtime should set it here as well as
    /// via [`Encoder::set_max_bitrate`]. `0` means "same as [`Self::bitrate`]".
    pub max_bitrate: i32,
    /// `EUsageType`; use `ScreenContentRealTime` for screen capture
    pub usage_type: EUsageType,
    /// "编码速度" knob: Low / Medium / High complexity
    pub complexity: EComplexityMode,
    /// `RcMode::Bitrate` == CBR-ish, `RcMode::Quality` == VBR-ish
    pub rc_mode: RcMode,
    /// IDR period in frames; 0 or -1 (= `u32::MAX` sentinel `NO_INTRA_PERIOD`) means
    /// "first frame only". Must be a multiple of `2^temporal_layers`.
    pub intra_period: u32,
    /// 0 = CAVLC, 1 = CABAC
    pub cabac: bool,
    /// 1 = single-threaded, 0 = auto (internal dynamic), >1 = thread count
    pub threads: u16,
    /// number of spatial layers (1..=4). Screen capture normally uses exactly 1.
    pub spatial_layers: i32,
    /// temporal layers (1..=4); 1 keeps the stream a plain AVC stream.
    pub temporal_layers: i32,
    /// `false` => SVC syntax for higher layers, `true` => Simulcast AVC.
    /// For a single spatial layer this is irrelevant.
    pub simulcast_avc: bool,
    /// emit prefix NALs (SVC). Off by default.
    pub prefix_nal: bool,
    /// allow the rate controller to drop frames to hold the bitrate
    pub enable_frame_skip: bool,
    /// slice mode; `SingleSlice` is the simplest and the default
    pub slice_mode: SliceMode,
    /// only meaningful for `SliceMode::FixedSlcNumSlice`
    pub slice_num: u32,
    /// `ELevelIdc` forced onto every spatial layer. `LEVEL_UNKNOWN` (0) lets OpenH264
    /// derive the level from the resolution, which is what you almost always want.
    pub level_idc: i32,
    /// `EProfileIdc` forced onto every spatial layer. `PRO_UNKNOWN` (0) lets OpenH264
    /// pick (`PRO_BASELINE`, or `PRO_HIGH` when CABAC is on).
    pub profile_idc: i32,
}

/// The `uiIntraPeriod` value that means "no periodic Intra frame at all".
///
/// `ENCODER_OPTION_IDR_INTERVAL` documents "0/-1 means no Intra period (only the first
/// frame)", and `uiIntraPeriod` is `unsigned int`, so `-1` is `u32::MAX`.
pub const NO_INTRA_PERIOD: u32 = u32::MAX;

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            width: 1280,
            height: 720,
            fps: 30.0,
            bitrate: 4_000_000,
            // 0 = "ceiling == target"，保持历史行为不变；需要更高的天花板就在配置里显式给
            max_bitrate: 0,
            usage_type: EUsageType::ScreenContentRealTime,
            complexity: EComplexityMode::Medium,
            rc_mode: RcMode::Bitrate,
            intra_period: 30,
            cabac: false,
            threads: 1,
            spatial_layers: 1,
            temporal_layers: 1,
            simulcast_avc: false,
            prefix_nal: false,
            enable_frame_skip: false,
            slice_mode: SliceMode::SingleSlice,
            slice_num: 1,
            level_idc: level_idc::LEVEL_UNKNOWN,
            profile_idc: profile_idc::PRO_UNKNOWN,
        }
    }
}

impl EncoderConfig {
    /// Convenience constructor with the most common screen-capture defaults.
    pub fn screen_capture(width: i32, height: i32, fps: f32, bitrate: i32) -> Self {
        EncoderConfig { width, height, fps, bitrate, ..Default::default() }
    }

    fn validate(&self) -> Result<()> {
        if self.width <= 0 || self.height <= 0 {
            return Err(Oh264Error::InvalidFrame(format!(
                "width/height must be positive, got {}x{}",
                self.width, self.height
            )));
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(Oh264Error::InvalidFrame(format!(
                "I420 needs even dimensions, got {}x{}",
                self.width, self.height
            )));
        }
        if !(self.fps.is_finite() && self.fps > 0.0) {
            return Err(Oh264Error::InvalidFrame(format!("fps must be > 0, got {}", self.fps)));
        }
        if self.bitrate <= 0 {
            return Err(Oh264Error::InvalidFrame(format!(
                "bitrate must be > 0 bps, got {}",
                self.bitrate
            )));
        }
        if self.spatial_layers < 1 || self.spatial_layers as usize > MAX_SPATIAL_LAYER_NUM {
            return Err(Oh264Error::InvalidFrame(format!(
                "spatial_layers must be 1..={MAX_SPATIAL_LAYER_NUM}, got {}",
                self.spatial_layers
            )));
        }
        if self.temporal_layers < 1 || self.temporal_layers as usize > MAX_TEMPORAL_LAYER_NUM {
            return Err(Oh264Error::InvalidFrame(format!(
                "temporal_layers must be 1..={MAX_TEMPORAL_LAYER_NUM}, got {}",
                self.temporal_layers
            )));
        }
        // Usage type: `ParamValidationExt` accepts **only** the two real-time types. The
        // non-real-time types are rejected by the library itself, which logs
        // "ParamValidationExt(),Invalid usage type = N" and makes `InitializeExt` return
        // `cmInitParaError` — reproduced on this machine for usage types 2 and 3
        // (`cargo test --lib probe_cabac -- --nocapture`). Turn that cryptic failure into
        // an explicit one here.
        if !matches!(
            self.usage_type,
            EUsageType::CameraVideoRealTime | EUsageType::ScreenContentRealTime
        ) {
            return Err(Oh264Error::InvalidFrame(format!(
                "usage_type {:?} is not supported by this OpenH264 build: ParamValidationExt \
                 only accepts CAMERA_VIDEO_REAL_TIME (0) and SCREEN_CONTENT_REAL_TIME (1); \
                 the non-real-time types fail InitializeExt with cmInitParaError",
                self.usage_type
            )));
        }
        // CABAC (`iEntropyCodingModeFlag = 1`) **is** usable — and only through the
        // real-time types checked above.
        //
        // Earlier revisions of this module rejected it outright, on the theory that
        // `SWelsSvcCodingParam::ParamTranscode` only reaches the CABAC path for
        // non-real-time usage types. That theory does not survive measurement: with
        // `usage_type = CameraVideoRealTime` plus `cabac = true`, `InitializeExt`
        // succeeds, OpenH264 picks the High profile, the same 1080p clip encodes at the
        // same speed (83.2 fps vs 83.7 fps for CAVLC, 30 frames, ABR 5 Mbps), and the
        // bitstream is 47% smaller (158_688 B vs 299_294 B). See
        // `cargo test --lib probe_cabac -- --nocapture`, which also reads
        // `entropy_coding_mode_flag` straight out of the PPS to prove CABAC really is on.
        if self.cabac && self.profile_idc == profile_idc::PRO_BASELINE {
            return Err(Oh264Error::InvalidFrame(
                "profile_idc = PRO_BASELINE cannot carry CABAC; leave profile_idc = PRO_UNKNOWN \
                 and let OpenH264 pick PRO_HIGH when cabac = true"
                    .to_string(),
            ));
        }
        if self.profile_idc != profile_idc::PRO_UNKNOWN
            && self.profile_idc != profile_idc::PRO_BASELINE
            && self.profile_idc != profile_idc::PRO_HIGH
        {
            return Err(Oh264Error::InvalidFrame(format!(
                "profile_idc={} is not usable: OpenH264's encoder derives PRO_BASELINE or \
                 PRO_HIGH from CABAC and rejects other profiles",
                self.profile_idc
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Decoded metadata about one encoded frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameInfo {
    /// `true` iff `SFrameBSInfo.eFrameType == videoFrameTypeIDR`
    pub is_keyframe: bool,
    /// raw `EVideoFrameType` value
    pub frame_type: i32,
    /// `SFrameBSInfo.iFrameSizeInBytes`
    pub frame_size_bytes: i32,
    /// `SFrameBSInfo.iLayerNum`
    pub layer_num: i32,
    /// `SFrameBSInfo.uiTimeStamp` echoed back by the encoder (milliseconds)
    pub timestamp_ms: i64,
}

/// A safe wrapper around one `ISVCEncoder` instance.
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use caplite::video::openh264::{Encoder, EncoderConfig};
///
/// let cfg = EncoderConfig::screen_capture(1280, 720, 30.0, 4_000_000);
/// let mut enc = Encoder::new(&cfg)?;
///
/// let (w, h) = (cfg.width as usize, cfg.height as usize);
/// let y = vec![16u8; w * h];
/// let u = vec![128u8; (w / 2) * (h / 2)];
/// let v = vec![128u8; (w / 2) * (h / 2)];
///
/// let nals = enc.encode_i420(&y, &u, &v, w, w / 2, 0)?;
/// assert!(nals.iter().any(|n| n.is_sps()));
/// # Ok(())
/// # }
/// ```
pub struct Encoder {
    /// Kept alive for as long as the encoder: `WelsDestroySVCEncoder` performs a
    /// virtual call, so the module must still be mapped when we destroy the object.
    lib: std::sync::Arc<OpenH264Lib>,
    /// `ISVCEncoder*` — actually a pointer to the object whose first field is the vtable pointer.
    obj: *mut ISVCEncoder,
    cfg: EncoderConfig,
    /// set once `Uninitialize` has run so `Drop` cannot double-free
    uninitialized: bool,
    /// reusable output struct (7 KiB) so we do not memset it per frame
    bs_info: Box<SFrameBSInfo>,
    frame_index: u64,
    /// Tracks the `iMaxSpatialBitrate` ceiling we have installed, because OpenH264
    /// rejects a target bitrate above it and gives no way to read the ceiling back in a
    /// form it will accept (`GetOption(MAX_BITRATE)` works, but only via an
    /// [`SBitrateInfo`] whose `iLayer` it validates first).
    max_bitrate_hint: i32,
}

// SAFETY: 一个 `Encoder` 实例**只被一个线程拥有**（录制时由视频线程独占，从不共享）。
// 内部的 `ISVCEncoder*` 是裸指针，但 OpenH264 的编码器对象本身不是共享状态：
// 把它移到另一个线程并不会产生数据竞争。这里手动声明 `Send` 以便把它搬进录制线程。
unsafe impl Send for Encoder {}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("dll", &self.lib.path)
            .field("obj", &format_args!("{:p}", self.obj))
            .field("config", &self.cfg)
            .field("frame_index", &self.frame_index)
            .finish()
    }
}

impl Encoder {
    /// Loads the DLL (if not already loaded) and creates an initialized encoder.
    ///
    /// Parameters are obtained from `GetDefaultParams` and then selectively overwritten —
    /// never built from an all-zero `SEncParamExt`.
    pub fn new(cfg: &EncoderConfig) -> Result<Self> {
        cfg.validate()?;
        let lib = std::sync::Arc::new(OpenH264Lib::load()?);
        Self::new_with_lib(lib, cfg)
    }

    /// Same as [`Encoder::new`] but reuses an already-loaded DLL.
    pub fn new_with_lib(lib: std::sync::Arc<OpenH264Lib>, cfg: &EncoderConfig) -> Result<Self> {
        cfg.validate()?;

        // ---- 1. create the object --------------------------------------------------
        let mut obj: *mut ISVCEncoder = core::ptr::null_mut();
        let rc = unsafe { (lib.create_encoder)(&mut obj) };
        if rc != cm_return::CM_RESULT_SUCCESS {
            return Err(Oh264Error::CreateFailed { code: rc });
        }
        if obj.is_null() {
            return Err(Oh264Error::CreateFailed { code: 0 });
        }

        let mut enc = Encoder {
            lib,
            obj,
            cfg: cfg.clone(),
            uninitialized: false,
            bs_info: Box::new(SFrameBSInfo::default()),
            frame_index: 0,
            max_bitrate_hint: Self::effective_max_bitrate(cfg),
        };

        // From here on any early return must destroy the half-built object.
        match enc.initialize_ext() {
            Ok(()) => {}
            Err(e) => {
                unsafe { (enc.lib.destroy_encoder)(enc.obj) };
                enc.obj = core::ptr::null_mut();
                enc.uninitialized = true;
                return Err(e);
            }
        }

        // Tell the encoder the input colour space explicitly (documented step 3).
        enc.set_data_format_i420()?;
        Ok(enc)
    }

    /// The DLL backing this encoder.
    pub fn lib(&self) -> &OpenH264Lib {
        &self.lib
    }

    /// The configuration this encoder was built with.
    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    #[inline]
    fn vt(&self) -> &ISVCEncoderVtbl {
        debug_assert!(!self.obj.is_null(), "use after destroy");
        unsafe { &*(*(self.obj as *const *const ISVCEncoderVtbl)) }
    }

    // -- initialization --------------------------------------------------------

    /// The rate-control **ceiling** actually installed at init: `max_bitrate` when the
    /// caller set one, otherwise the target itself (historical behaviour).
    fn effective_max_bitrate(cfg: &EncoderConfig) -> i32 {
        if cfg.max_bitrate > 0 {
            cfg.max_bitrate
        } else {
            cfg.bitrate
        }
    }

    fn initialize_ext(&mut self) -> Result<()> {
        // Start from real defaults.
        let mut p = SEncParamExt::default();
        let rc = unsafe { (self.vt().GetDefaultParams)(self.obj, &mut p) };
        if rc != cm_return::CM_RESULT_SUCCESS {
            return Err(Oh264Error::CodecApi { call: "GetDefaultParams", code: rc });
        }

        let cfg = &self.cfg;

        // ---- top level ----
        p.iUsageType = cfg.usage_type as i32;
        p.iPicWidth = cfg.width;
        p.iPicHeight = cfg.height;
        p.iTargetBitrate = cfg.bitrate * cfg.spatial_layers.max(1);
        p.fMaxFrameRate = cfg.fps;
        p.iRCMode = cfg.rc_mode as i32;
        p.iComplexityMode = cfg.complexity as i32;
        p.iTemporalLayerNum = cfg.temporal_layers;
        p.iSpatialLayerNum = cfg.spatial_layers;
        p.uiIntraPeriod = cfg.intra_period;
        p.iEntropyCodingModeFlag = i32::from(cfg.cabac);
        p.iMultipleThreadIdc = cfg.threads;
        p.bEnableFrameSkip = cfg.enable_frame_skip;
        p.bPrefixNalAddingCtrl = cfg.prefix_nal;
        p.bSimulcastAVC = cfg.simulcast_avc;
        p.bEnableSSEI = false;
        // Rate-control ceiling. `GetDefaultParams` leaves `iMaxBitrate` at
        // `UNSPECIFIED_BIT_RATE` (-1), which does not act as a usable ceiling; install an
        // explicit one so a target of at most `max_bitrate` is always accepted.
        p.iMaxBitrate = Self::effective_max_bitrate(cfg);
        // `GetDefaultParams` turns on three pre-processing features that OpenH264 then
        // rejects for screen content at InitializeExt time, emitting a warning each:
        //
        //   [OpenH264] Warning:ParamValidation(), AdaptiveQuant(1) is not supported yet
        //              for screen content, auto turned off
        //   [OpenH264] Warning:ParamValidation(), BackgroundDetection(1) is not ...
        //
        // They are library defaults rather than ours, so turn them off explicitly. This
        // is purely noise reduction: it does not change the produced bitstream.
        for ut in [EUsageType::ScreenContentRealTime, EUsageType::ScreenContentNonRealTime] {
            if cfg.usage_type == ut {
                p.bEnableAdaptiveQuant = false;
                p.bEnableBackgroundDetection = false;
            }
        }
        // Keep the SPS/PPS ids constant so a decoder joining mid-stream is not confused.
        p.eSpsPpsIdStrategy = parameter_set_strategy::CONSTANT_ID;
        // Cropping flag stays at whatever OpenH264 chose; forcing it off can make the
        // decoder disagree about dimensions when width/height are not multiples of 16.
        p.bEnableFrameCroppingFlag = true;
        // iNumRefFrame: leave the library default (AUTO_REF_PIC_COUNT). Forcing 1 or 2
        // here is the cause of the classic "InitializeExt -> cmInitParaError" report.

        // ---- per spatial layer ----
        let n = cfg.spatial_layers.max(1) as usize;
        for i in 0..n {
            let shift = (n - 1 - i) as u32; // layer 0 is the smallest in OpenH264's convention
            let lw = cfg.width >> shift;
            let lh = cfg.height >> shift;
            let layer = &mut p.sSpatialLayers[i];
            layer.iVideoWidth = lw;
            layer.iVideoHeight = lh;
            layer.fFrameRate = cfg.fps;
            layer.iSpatialBitrate = cfg.bitrate;
            layer.iMaxSpatialBitrate = Self::effective_max_bitrate(cfg);
            // PRO_UNKNOWN / LEVEL_UNKNOWN let the encoder derive profile and level from
            // the resolution and CABAC setting; hardcoding them is a common cause of
            // `InitializeExt -> cmInitParaError` (e.g. CABAC without PROF_HIGH).
            layer.uiProfileIdc = cfg.profile_idc;
            layer.uiLevelIdc = cfg.level_idc;
            layer.sSliceArgument.uiSliceMode = cfg.slice_mode as i32;
            match cfg.slice_mode {
                SliceMode::FixedSlcNumSlice => {
                    layer.sSliceArgument.uiSliceNum = cfg.slice_num;
                }
                SliceMode::SizeLimitedSlice => {
                    layer.sSliceArgument.uiSliceSizeConstraint = 1500;
                    p.uiMaxNalSize = 1500;
                }
                _ => {}
            }
        }
        // Layers beyond iSpatialLayerNum are ignored by the encoder, but leaving stale
        // defaults (width 0) behind is asking for trouble; blank them out.
        for i in n..MAX_SPATIAL_LAYER_NUM {
            p.sSpatialLayers[i] = SSpatialLayerConfig::default();
            p.sSpatialLayers[i].sSliceArgument.uiSliceMode = SliceMode::SingleSlice as i32;
        }

        let rc = unsafe { (self.vt().InitializeExt)(self.obj, &p) };
        if rc != cm_return::CM_RESULT_SUCCESS {
            return Err(Oh264Error::CodecApi { call: "InitializeExt", code: rc });
        }
        Ok(())
    }

    fn set_data_format_i420(&mut self) -> Result<()> {
        let mut fmt = video_format_type::VIDEO_FORMAT_I420;
        let rc = unsafe {
            (self.vt().SetOption)(
                self.obj,
                EncoderOption::DataFormat as i32,
                &mut fmt as *mut i32 as *mut c_void,
            )
        };
        if rc != cm_return::CM_RESULT_SUCCESS {
            return Err(Oh264Error::CodecApi { call: "SetOption(ENCODER_OPTION_DATAFORMAT)", code: rc });
        }
        Ok(())
    }

    // -- options ---------------------------------------------------------------

    /// Changes the target bitrate at runtime.
    ///
    /// `ENCODER_OPTION_BITRATE` expects an [`SBitrateInfo`], **not** a bare `int` — the
    /// DLL reads `iLayer` and `iBitrate` from that struct. Passing an `int*` makes it
    /// interpret the bitrate as `iLayer`, log the garbage it read, and return
    /// `cmInitParaError`.
    ///
    /// # Two OpenH264 behaviours this method works around
    ///
    /// **(a) Raising the target above the current maximum is rejected.**
    /// `WelsBitRateVerification` (`codec/encoder/core/src/encoder_ext.cpp:116`) returns
    /// `ENC_RETURN_UNSUPPORTED_PARA` when `iMaxSpatialBitrate < iSpatialBitrate`, and
    /// `SetOption` runs that check *before* the new target is honoured. Measured
    /// (measured with a throwaway probe binary, since deleted), on an encoder created
    /// with a 1 Mbps target:
    ///
    /// ```text
    /// set_bitrate(  900_000) -> ok      (below the 1 Mbps maximum)
    /// set_bitrate(1_000_000) -> ok      (equal)
    /// set_bitrate(1_200_000) -> ERR cmInitParaError
    /// set_bitrate(4_000_000) -> ERR cmInitParaError
    /// after set_max_bitrate(20_000_000):
    /// set_bitrate(8_000_000) -> ok
    /// ```
    ///
    /// So this method raises the maximum first (via
    /// [`Encoder::set_max_bitrate`]) whenever the requested target exceeds it. Without
    /// that, "make it 8 Mbps" would fail on a fresh encoder while "make it 900 kbps"
    /// would succeed — an asymmetry that is very easy to misdiagnose.
    ///
    /// **(b) `LAYER_ALL` reads the old per-layer values, then rewrites them.**
    /// `WelsEncoderApplyBitRate` (`encoder_ext.cpp:706`) accumulates
    /// `iOrigTotalBitrate` from the *current* per-layer bitrates and then sets each layer
    /// to `newTarget * (oldLayer / oldTotal)`. `SetOption` assigned the new
    /// `iTargetBitrate` first, so the ratio is computed against a stale total. This
    /// method therefore sets each layer individually, which is equivalent for a single
    /// layer and better defined for several. [`Encoder::set_bitrate_for_layer`] can still
    /// be used to invoke the library's `LAYER_ALL` path explicitly.
    pub fn set_bitrate(&mut self, bps: i32) -> Result<()> {
        if bps <= 0 {
            return Err(Oh264Error::InvalidFrame(format!("bitrate must be > 0 bps, got {bps}")));
        }
        // (a) make sure the ceiling permits the new target.
        if bps > self.max_bitrate_hint {
            self.set_max_bitrate(bps)?;
        }
        let layers = self.cfg.spatial_layers.max(1);
        for i in 0..layers {
            self.apply_bitrate_option(bps, i, EncoderOption::Bitrate, "SetOption(ENCODER_OPTION_BITRATE)")?;
        }
        self.cfg.bitrate = bps;
        Ok(())
    }

    /// Sets the bitrate for one spatial layer (`layer_num::SPATIAL_LAYER_0`..`_3`) or for
    /// all of them (`layer_num::SPATIAL_LAYER_ALL`, ratio-preserving — see the caveat on
    /// [`Encoder::set_bitrate`]).
    ///
    /// Raises the maximum first, for the same reason as [`Encoder::set_bitrate`].
    pub fn set_bitrate_for_layer(&mut self, bps: i32, layer: i32) -> Result<()> {
        if bps <= 0 {
            return Err(Oh264Error::InvalidFrame(format!("bitrate must be > 0 bps, got {bps}")));
        }
        if bps > self.max_bitrate_hint {
            self.set_max_bitrate(bps)?;
        }
        self.apply_bitrate_option(bps, layer, EncoderOption::Bitrate, "SetOption(ENCODER_OPTION_BITRATE)")?;
        if layer == layer_num::SPATIAL_LAYER_ALL {
            self.cfg.bitrate = bps;
        }
        Ok(())
    }

    fn apply_bitrate_option(
        &mut self,
        bps: i32,
        layer: i32,
        option: EncoderOption,
        call: &'static str,
    ) -> Result<()> {
        let mut info = SBitrateInfo { iLayer: layer, iBitrate: bps };
        let rc = unsafe {
            (self.vt().SetOption)(
                self.obj,
                option as i32,
                &mut info as *mut SBitrateInfo as *mut c_void,
            )
        };
        if rc != cm_return::CM_RESULT_SUCCESS {
            return Err(Oh264Error::CodecApi { call, code: rc });
        }
        Ok(())
    }

    /// Changes the maximum bitrate (`ENCODER_OPTION_MAX_BITRATE`), also an
    /// [`SBitrateInfo`]. Applied per layer for the same reason as
    /// [`Encoder::set_bitrate`].
    ///
    /// This is what must be raised before the target can be raised: OpenH264 rejects a
    /// target above `iMaxSpatialBitrate`. The new ceiling is remembered so that
    /// [`Encoder::set_bitrate`] knows whether it needs to raise it again.
    pub fn set_max_bitrate(&mut self, bps: i32) -> Result<()> {
        if bps <= 0 {
            return Err(Oh264Error::InvalidFrame(format!("max bitrate must be > 0 bps, got {bps}")));
        }
        let layers = self.cfg.spatial_layers.max(1);
        for i in 0..layers {
            self.apply_bitrate_option(
                bps,
                i,
                EncoderOption::MaxBitrate,
                "SetOption(ENCODER_OPTION_MAX_BITRATE)",
            )?;
        }
        self.max_bitrate_hint = bps;
        Ok(())
    }

    // -- the actual encode -----------------------------------------------------

    /// Encodes one I420 frame.
    ///
    /// * `y` is `stride_y * height` bytes, `u`/`v` are `stride_uv * (height/2)` bytes.
    /// * `stride_y` may be larger than the width (a cropped/padded capture surface);
    ///   a negative-like tight stride is rejected.
    /// * Returns the NAL units OpenH264 produced, in order. A skipped frame yields an
    ///   empty vector (see [`Encoder::encode_i420_ex`] to distinguish that from an error).
    pub fn encode_i420(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        stride_y: usize,
        stride_uv: usize,
        ts_ms: i64,
    ) -> Result<Vec<Nal>> {
        self.encode_i420_ex(y, u, v, stride_y, stride_uv, ts_ms).map(|(nals, _)| nals)
    }

    /// Like [`Encoder::encode_i420`] but also returns the frame metadata.
    pub fn encode_i420_ex(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        stride_y: usize,
        stride_uv: usize,
        ts_ms: i64,
    ) -> Result<(Vec<Nal>, FrameInfo)> {
        if self.obj.is_null() || self.uninitialized {
            return Err(Oh264Error::Poisoned("EncodeFrame"));
        }
        let w = self.cfg.width as usize;
        let h = self.cfg.height as usize;
        let cw = w / 2;
        let ch = h / 2;

        if stride_y < w {
            return Err(Oh264Error::BufferTooSmall(format!(
                "stride_y ({stride_y}) < width ({w})"
            )));
        }
        if stride_uv < cw {
            return Err(Oh264Error::BufferTooSmall(format!(
                "stride_uv ({stride_uv}) < width/2 ({cw})"
            )));
        }
        let y_need = stride_y.checked_mul(h).ok_or_else(|| Oh264Error::BufferTooSmall("stride_y*height overflow".into()))?;
        let uv_need = stride_uv.checked_mul(ch).ok_or_else(|| Oh264Error::BufferTooSmall("stride_uv*height/2 overflow".into()))?;
        if y.len() < y_need {
            return Err(Oh264Error::BufferTooSmall(format!("y.len()={} < {y_need}", y.len())));
        }
        if u.len() < uv_need {
            return Err(Oh264Error::BufferTooSmall(format!("u.len()={} < {uv_need}", u.len())));
        }
        if v.len() < uv_need {
            return Err(Oh264Error::BufferTooSmall(format!("v.len()={} < {uv_need}", v.len())));
        }

        let mut pic = SSourcePicture {
            iColorFormat: video_format_type::VIDEO_FORMAT_I420,
            iStride: [stride_y as i32, stride_uv as i32, stride_uv as i32, 0],
            pData: [
                y.as_ptr() as *mut u8,
                u.as_ptr() as *mut u8,
                v.as_ptr() as *mut u8,
                core::ptr::null_mut(),
            ],
            iPicWidth: self.cfg.width,
            iPicHeight: self.cfg.height,
            uiTimeStamp: ts_ms,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };

        // Reuse the 7 KiB output struct; only the scalars need clearing.
        self.bs_info.reset_for_reuse();

        let rc = unsafe { (self.vt().EncodeFrame)(self.obj, &pic, self.bs_info.as_mut()) };
        if rc != cm_return::CM_RESULT_SUCCESS {
            return Err(Oh264Error::CodecApi { call: "EncodeFrame", code: rc });
        }
        // The encoder may have retained our plane pointers internally for the duration
        // of the call; clear them so a stale pointer can never be observed through a
        // debugger or a later reuse of this stack slot.
        pic.pData = [core::ptr::null_mut(); 4];
        let _ = pic.iStride;

        let info = FrameInfo {
            is_keyframe: self.bs_info.eFrameType == EVideoFrameType::Idr as i32,
            frame_type: self.bs_info.eFrameType,
            frame_size_bytes: self.bs_info.iFrameSizeInBytes,
            layer_num: self.bs_info.iLayerNum,
            timestamp_ms: self.bs_info.uiTimeStamp,
        };
        // SAFETY: `iLayerNum` and every `sLayerInfo[i]` pointer were filled in by the
        // encoder for the duration of this call, and the buffers it points at remain
        // valid until the next `EncodeFrame`/`Uninitialize` on this instance.
        let nals = unsafe { self.collect_nals(self.bs_info.as_ref()) }?;
        self.frame_index += 1;
        Ok((nals, info))
    }

    /// Walks `SFrameBSInfo.sLayerInfo[0..iLayerNum]` and copies every NAL out.
    ///
    /// # Safety
    /// `info` must have been filled by a successful encoder call on this instance.
    unsafe fn collect_nals(&self, info: &SFrameBSInfo) -> Result<Vec<Nal>> {
        if info.iLayerNum < 0 || info.iLayerNum as usize > MAX_LAYER_NUM_OF_FRAME {
            return Err(Oh264Error::BadBitstream(format!(
                "SFrameBSInfo.iLayerNum = {} is out of range 0..={MAX_LAYER_NUM_OF_FRAME}",
                info.iLayerNum
            )));
        }
        let mut out: Vec<Nal> = Vec::new();
        for li in 0..info.iLayerNum as usize {
            let layer = &info.sLayerInfo[li];
            let count = layer.iNalCount;
            if count < 0 {
                return Err(Oh264Error::BadBitstream(format!(
                    "layer {li}: iNalCount = {count}"
                )));
            }
            if count == 0 {
                continue;
            }
            if layer.pBsBuf.is_null() {
                return Err(Oh264Error::BadBitstream(format!(
                    "layer {li}: iNalCount = {count} but pBsBuf is NULL"
                )));
            }
            if layer.pNalLengthInByte.is_null() {
                return Err(Oh264Error::BadBitstream(format!(
                    "layer {li}: iNalCount = {count} but pNalLengthInByte is NULL"
                )));
            }
            // SAFETY: the caller guarantees `info` came from a successful encoder call on
            // this instance, so `pNalLengthInByte` points at `iNalCount` valid i32s.
            let lengths = unsafe { core::slice::from_raw_parts(layer.pNalLengthInByte, count as usize) };
            // SAFETY: `pBsBuf` covers at least the sum of those lengths (OpenH264
            // guarantees this; `split_nals` re-validates every offset against them).
            out.extend(unsafe { split_nals(layer.pBsBuf, lengths) }?);
        }
        Ok(out)
    }

}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.obj.is_null() {
            return;
        }
        // Order matters: Uninitialize() releases the encoder's internal buffers and
        // threads, WelsDestroySVCEncoder runs ~ISVCEncoder and frees the object.
        if !self.uninitialized {
            // Capture the return code but never panic in Drop.
            let rc = unsafe { (self.vt().Uninitialize)(self.obj) };
            if rc != cm_return::CM_RESULT_SUCCESS {
                eprintln!(
                    "[proto-oh264] warning: ISVCEncoder::Uninitialize() returned {rc} ({})",
                    cm_return_str(rc)
                );
            }
            self.uninitialized = true;
        }
        if let Some(vt) = unsafe { (self.obj as *const *const ISVCEncoderVtbl).as_ref() } {
            let _ = vt; // keep the vtable pointer alive until the destroy call below
        }
        unsafe { (self.lib.destroy_encoder)(self.obj) };
        self.obj = core::ptr::null_mut();
    }
}

// ---------------------------------------------------------------------------
// Annex-B helpers
// ---------------------------------------------------------------------------

/// Parses a complete Annex-B byte stream into NAL units.
///
/// Tolerant of both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes, and of
/// trailing zero padding. This is the *independent* parser used by the tests to
/// validate that the encoder's output really is a well-formed elementary stream.
pub fn parse_annex_b(stream: &[u8]) -> Vec<Result<Nal>> {
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (start_code_pos, header_pos)
    let mut i = 0usize;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 {
            if stream[i + 2] == 1 {
                starts.push((i, i + 3));
                i += 3;
                continue;
            }
            if i + 4 <= stream.len() && stream[i + 2] == 0 && stream[i + 3] == 1 {
                starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    let mut out = Vec::with_capacity(starts.len());
    for (k, &(sc, hdr)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map(|&(p, _)| p).unwrap_or(stream.len());
        if hdr >= end {
            // A start code with nothing after it: only legal for a trailing zero pad.
            let tail = &stream[sc..end];
            if tail.iter().all(|&b| b == 0) {
                continue;
            }
            out.push(Err(Oh264Error::BadBitstream(format!(
                "start code at offset {sc} is followed by no NAL header byte"
            ))));
            continue;
        }
        // Trim the trailing zero bytes that belong to the *next* start code.
        let mut real_end = end;
        while real_end > hdr && stream[real_end - 1] == 0 {
            real_end -= 1;
        }
        if real_end <= hdr {
            out.push(Err(Oh264Error::BadBitstream(format!(
                "empty NAL unit at offset {sc}"
            ))));
            continue;
        }
        out.push(Nal::from_annex_b(stream[sc..real_end].to_vec()));
    }
    out
}

/// Extracts the NAL type sequence of an Annex-B stream (or an error string per bad NAL).
pub fn annex_b_type_sequence(stream: &[u8]) -> Vec<String> {
    parse_annex_b(stream)
        .into_iter()
        .map(|r| match r {
            Ok(n) => n.type_name().to_string(),
            Err(_) => "ERR".to_string(),
        })
        .collect()
}
