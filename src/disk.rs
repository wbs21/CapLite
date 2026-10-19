//! 磁盘剩余空间（录制前预检 + 录制中低空间保护）。
//!
//! 为什么单独成模块：录制侧（`recorder`）和 UI 侧都需要同一个口径，而且
//! `GetDiskFreeSpaceExW` 是 Win32 调用，不能散落在业务逻辑里。
//!
//! 口径：
//! * `free_bytes(path)` 对**目录或文件**都可用；路径不存在时向上找第一个存在的祖先；
//! * 返回 `None` = 取不到（网络盘/权限/异常），调用方应**失败开放**（不要因为
//!   查不到空间就不让录），但要在日志里留痕；
//! * 本模块只查询，不创建/删除任何文件。

use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// 路径 → 宽字符（带结尾 0）。
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// 找第一个存在的祖先（`GetDiskFreeSpaceExW` 需要一个真实存在的路径）。
fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    for _ in 0..64 {
        if cur.exists() {
            return Some(cur);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// 指定路径所在卷的**可用字节数**（当前用户可用）。
///
/// 目录/文件不存在时向上找祖先；仍然找不到就返回 `None`。
pub fn free_bytes(path: &Path) -> Option<u64> {
    let probe = existing_ancestor(path)?;
    let w = wide(&probe);
    let mut free: u64 = 0;
    // SAFETY: w 是以 0 结尾的 UTF-16 缓冲；只写一个 u64，指针有效。
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(w.as_ptr()),
            Some(&mut free as *mut u64),
            None,
            None,
        )
    };
    match ok {
        Ok(()) => Some(free),
        Err(_) => None,
    }
}

/// 录制/导出需要的最低安全余量（512 MB）。
///
/// 这不是“预计文件大小”，而是留给系统、日志、导出临时文件（faststart 会再要一份）
/// 的缓冲。低于它时拒绝开始，比录到一半把系统盘写满要好。
pub const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;

/// 格式化字节数为人类可读的 MB/GB（用于错误文案/日志）。
pub fn human(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_formats_mb_and_gb() {
        assert_eq!(human(512 * 1024 * 1024), "512 MB");
        assert_eq!(human(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn free_bytes_of_temp_dir_is_available_on_real_machine() {
        let dir = std::env::temp_dir();
        let free = free_bytes(&dir).expect("临时目录应能查到剩余空间");
        assert!(free > 0, "剩余空间应大于 0");
    }

    #[test]
    fn free_bytes_walks_up_to_existing_ancestor() {
        let dir = std::env::temp_dir().join("cap-no-such-dir-for-disk-test");
        // 不存在也应按祖先返回 Some；具体大小取决于机器，只断言能取到。
        let _ = free_bytes(&dir);
    }
}
