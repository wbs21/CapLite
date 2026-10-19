//! Hand-rolled `kernel32` declarations — no `windows` crate, no `libloading`.
//!
//! Only the four functions we actually need are declared. The sandbox has no HTTPS
//! so pulling a dependency is impossible *and* undesirable: these signatures have been
//! stable since Windows 2000 and are part of the documented ABI.
//!
//! `LoadLibraryW`/`FreeLibrary`/`GetProcAddress` all live in `kernel32.dll`, which is
//! linked implicitly by the MSVC CRT startup code, so a bare `extern "system"` block
//! resolves with no `#[link]` attribute and no import library on disk.

#![allow(non_snake_case)]

use core::ffi::{c_char, c_void};

extern "system" {
    /// Loads the specified module into the address space of the calling process.
    ///
    /// Returns a module handle on success, or NULL on failure. The full path is searched
    /// as-is (no `PATH` augmentation), which is why callers should pass absolute paths
    /// where it matters.
    pub fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;

    /// Frees the loaded dynamic-link library module.
    pub fn FreeLibrary(hLibModule: *mut c_void) -> i32;

    /// Retrieves the address of an exported function or variable.
    pub fn GetProcAddress(hModule: *mut c_void, lpProcName: *const c_char) -> *mut c_void;

    /// Formats a system message (used to turn `GetLastError` into text).
    pub fn FormatMessageW(
        dwFlags: u32,
        lpSource: *const c_void,
        dwMessageId: u32,
        dwLanguageId: u32,
        lpBuffer: *mut u16,
        nSize: u32,
        Arguments: *const *const c_char,
    ) -> u32;

    /// Retrieves the calling thread's last-error code value.
    pub fn GetLastError() -> u32;
}

pub(crate) const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
pub(crate) const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;

/// Converts a Rust `&str`/`String` into a NUL-terminated UTF-16 buffer.
pub fn to_wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// Wrapper around [`FreeLibrary`] that rejects a NULL handle.
///
/// # Safety
/// `h` must be either NULL or a handle returned by a successful [`LoadLibraryW`] that
/// has not been freed yet. Passing a stale/dangling handle makes Windows read freed
/// loader bookkeeping — it is **not** a "just returns false" failure mode.
pub unsafe fn free_library(h: *mut c_void) -> bool {
    if h.is_null() {
        return false;
    }
    // SAFETY: `h` is non-null and (caller contract) a live module handle.
    unsafe { FreeLibrary(h) != 0 }
}

/// Safe wrapper around [`GetProcAddress`] that rejects a NULL module handle and
/// returns `None` when the symbol is missing.
///
/// # Safety
/// `module` must be a live module handle. The returned pointer is only meaningful for
/// as long as the module stays loaded, which this crate guarantees by never calling
/// `FreeLibrary` implicitly.
pub unsafe fn get_proc_address(module: *mut c_void, name: &str) -> Option<*mut c_void> {
    if module.is_null() {
        return None;
    }
    let mut cname: Vec<u8> = name.bytes().collect();
    cname.push(0);
    // SAFETY: `module` is live (caller contract) and `cname` is NUL-terminated.
    let p = unsafe { GetProcAddress(module, cname.as_ptr() as *const c_char) };
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

/// Converts a NUL-terminated UTF-16 pointer into an owned `String` (lossy).
///
/// # Safety
/// `p` must be either NULL or a valid NUL-terminated UTF-16 string.
pub(crate) unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // Cap the scan: a corrupt pointer should not walk forever.
    // SAFETY: the caller guarantees a valid NUL-terminated UTF-16 string.
    unsafe {
        while len < 32_768 && *p.add(len) != 0 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(p, len);
        String::from_utf16_lossy(slice)
    }
}

/// Turns the current `GetLastError()` into a human-readable string.
pub fn last_error_message() -> String {
    unsafe {
        let code = GetLastError();
        let mut buf = [0u16; 1024];
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            core::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            core::ptr::null(),
        );
        let text = if n == 0 {
            String::from("<no system message available>")
        } else {
            from_wide_ptr(buf.as_ptr()).trim().to_string()
        };
        format!("GetLastError()={code} ({text})")
    }
}
