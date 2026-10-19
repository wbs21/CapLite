//! 录制"流文件"的 Cache 目录：**目录分工**（**不删除、不统计**）。
//!
//! 分工（用户明确要求）：
//! * **流文件**（录制过程中持续落盘的分片 MP4，崩溃也能播）→ **程序所在目录下的 `Cache\`**；
//! * **导出产物**（停止时 remux 出来的普通 MP4）→ **用户设置的输出目录**（`cfg.output_dir`）。
//!
//! 为什么不删也不统计：流文件是"导出失败/忘了另存"时的**唯一找回手段**，用户最后的决定是
//! ——**程序不做任何删除**（既不硬删也不送回收站），也**不统计占用空间**；只在**关闭软件**时
//! 如果 Cache 里还有文件，就弹一次提醒（含完整路径），请用户自己定期清理。
//! 所以本模块只提供：目录选择、**"是否有文件"的廉价判断**、以及"要不要提醒"的纯函数。

use std::path::{Path, PathBuf};

/// Windows `FILE_ATTRIBUTE_REPARSE_POINT`。Cache 目录一旦是 junction/symlink，
/// 后续的写入/清理会跟随到目标目录；在管理员运行时这是提权/任意删除风险。
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// 目录是否是**真实目录**（不存在也算 true；存在但是文件、或带 reparse point 则 false）。
pub fn is_plain_dir(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                meta.is_dir() && (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => meta.is_dir() && !meta.file_type().is_symlink(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }
}

/// 创建并确认目录不是 junction/symlink。拒绝时返回错误，调用方必须回退/拒绝写入。
pub fn ensure_plain_dir(path: &Path) -> std::io::Result<()> {
    // 先查父目录再 create：如果 Cache 本身是 junction，不能先顺着它把子目录建到目标里。
    if let Some(parent) = path.parent() {
        let is_cache_root = parent
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(CACHE_DIR_NAME));
        if is_cache_root && !is_plain_dir(parent) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("拒绝使用 junction/symlink 目录：{}", parent.display()),
            ));
        }
    }
    std::fs::create_dir_all(path)?;
    if is_plain_dir(path) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("拒绝使用 junction/symlink 目录：{}", path.display()),
        ))
    }
}

/// Cache 目录名（程序所在目录下的子目录）
pub(crate) const CACHE_DIR_NAME: &str = "Cache";
/// 录制分片子目录：**永不自动删除**（用户找回录制内容的唯一副本）。
pub(crate) const RECORDINGS_SUBDIR: &str = "recordings";
/// 派生缓存子目录（波形峰值等）：可以安全重新生成，允许自动修剪。
pub(crate) const DERIVED_SUBDIR: &str = "derived";

/// 录制分片目录（`<Cache>\recordings`）。
pub fn recordings_dir(cache_root: &Path) -> PathBuf {
    cache_root.join(RECORDINGS_SUBDIR)
}

/// 派生缓存目录（`<Cache>\derived`）。
pub fn derived_dir(cache_root: &Path) -> PathBuf {
    cache_root.join(DERIVED_SUBDIR)
}

/// 派生缓存是否还有文件（用于关闭提醒/诊断；录制分片用 [`cache_has_recordings`]）。
pub fn cache_has_recordings(cache_root: &Path) -> bool {
    if cache_has_files(&recordings_dir(cache_root)) {
        return true;
    }
    // 旧版本把 `rec-*.mp4` 直接放在 `Cache\` 根目录；升级后不能让这些分片
    // 从“关闭提醒”里消失（它们同样是用户找回录制内容的唯一副本）。
    let Ok(entries) = std::fs::read_dir(cache_root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mp4"))
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// 修剪派生缓存：只保留最近 `max_files` 个、且修改时间不超过 `max_age` 的文件。
///
/// **绝不碰 `recordings`**：这里只接收 [`derived_dir`] 的路径。返回删除的文件数。
/// 失败/读不到目录一律静默返回 0（派生缓存丢失最多是重算一次）。
pub fn prune_derived(dir: &Path, max_files: usize, max_age: std::time::Duration) -> usize {
    // 关键防线：derived（或其父 Cache）可能是用户/攻击者预置的 junction。
    // 绝不跟随它做删除。
    let cache_parent_plain = dir
        .parent()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(CACHE_DIR_NAME))
        })
        .map(is_plain_dir)
        .unwrap_or(true);
    if !is_plain_dir(dir) || !cache_parent_plain {
        tracing::warn!(dir = %dir.display(), "拒绝修剪 junction/symlink 目录");
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        files.push((modified, entry.path()));
    }
    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut removed = 0usize;
    for (i, (modified, path)) in files.into_iter().enumerate() {
        let too_many = i >= max_files;
        let too_old = now
            .duration_since(modified)
            .map(|age| age > max_age)
            .unwrap_or(false);
        if (too_many || too_old) && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 选 Cache 目录：**优先程序所在目录**（`<exe>\Cache`），不可写时退到
/// `<配置目录>\Cache`，再不行就临时目录 —— 全部走"真写一个探测文件"的可写探测。
pub fn resolve_cache_dir(
    exe_dir: Option<&Path>,
    config_dir: Option<&Path>,
    probe: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(d) = exe_dir {
        let c = d.join(CACHE_DIR_NAME);
        if is_plain_dir(&c) && probe(&c) && is_plain_dir(&c) {
            return c;
        }
    }
    if let Some(d) = config_dir {
        let c = d.join(CACHE_DIR_NAME);
        if is_plain_dir(&c) && probe(&c) && is_plain_dir(&c) {
            return c;
        }
    }
    // 程序目录/配置目录都不可写（或被 junction 污染）时，优先每用户 LocalAppData；
    // 它比 %TEMP% 更稳定，也不会被其他会话共享。
    if let Some(base) = directories::BaseDirs::new() {
        let c = base.data_local_dir().join("CapLite").join(CACHE_DIR_NAME);
        if is_plain_dir(&c) && probe(&c) && is_plain_dir(&c) {
            return c;
        }
    }
    std::env::temp_dir().join("CapLite").join(CACHE_DIR_NAME)
}

/// Cache 目录里**是否有文件**（决定"关闭时要不要提醒"）。
///
/// 只看第一个条目就返回：**不遍历、不求和**（用户最终版要求：不统计占用空间）。
/// 目录不存在/读不了 → `false`（没什么可提醒的，直接放行关闭）。
pub fn cache_has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .and_then(|mut it| it.next())
        .is_some()
}

/// 关闭软件时是否要弹"请定期清理 Cache"的提醒（纯函数，便于单测）。
///
/// * `cache_nonempty`：Cache 目录里是否还有文件（空目录不打扰用户）；
/// * `already_warned`：**本次运行**是否已经提醒过（内存标志，不写配置文件 ——
///   用户没要求持久化，只要避免同一次运行里反复弹）。
pub fn should_warn_cache(cache_nonempty: bool, already_warned: bool) -> bool {
    cache_nonempty && !already_warned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cap-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建临时目录");
        d
    }

    #[cfg(windows)]
    #[test]
    fn junction_cache_is_rejected_and_not_pruned() {
        let root = test_dir("junction");
        let target = test_dir("junction-target");
        std::fs::write(target.join("old.bin"), b"x").unwrap();
        let cache = root.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&cache).unwrap();
        let junction = cache.join(DERIVED_SUBDIR);
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            // 受限环境不允许建 junction：跳过（不是产品失败）。
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&target);
            return;
        }
        assert!(!is_plain_dir(&junction), "junction 必须被识别");
        assert!(ensure_plain_dir(&junction).is_err(), "junction 必须被拒绝");
        assert_eq!(
            prune_derived(&junction, 0, std::time::Duration::ZERO),
            0,
            "junction 下不得删除"
        );
        assert!(target.join("old.bin").is_file(), "目标文件必须还在");
        let _ = std::fs::remove_dir(&junction); // 只删 junction 本身
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&target);
    }

    /// `cache_has_files`：空目录 / 不存在的目录 → false；有文件 → true（不遍历求和）。
    #[test]
    fn cache_has_files_only_checks_presence() {
        let dir = test_dir("has-files");
        assert!(!cache_has_files(&dir), "空目录不该被认为有缓存");
        assert!(!cache_has_files(&dir.join("no-such")), "不存在的目录也不报错");
        std::fs::write(dir.join("rec.mp4"), b"x").unwrap();
        assert!(cache_has_files(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 关闭提醒的判定：**只有"Cache 非空 且 本次运行还没提醒过"**才提醒；
    /// 空目录直接放行关闭，提醒过之后也放行（点「知道了」= 正常关闭）。
    #[test]
    fn should_warn_cache_only_when_nonempty_and_not_yet_warned() {
        assert!(should_warn_cache(true, false), "有缓存且没提醒过 → 提醒");
        assert!(!should_warn_cache(false, false), "空 Cache 不打扰 → 直接关");
        assert!(!should_warn_cache(true, true), "已提醒过 → 放行关闭");
        assert!(!should_warn_cache(false, true));
    }

    /// 录制分片判定：新 `recordings` 子目录与旧版根目录 `rec-*.mp4` 都算。
    #[test]
    fn cache_has_recordings_covers_new_and_legacy_layouts() {
        let dir = test_dir("recordings-layout");
        assert!(!cache_has_recordings(&dir));
        std::fs::create_dir_all(recordings_dir(&dir)).unwrap();
        std::fs::write(recordings_dir(&dir).join("rec-new.mp4"), b"x").unwrap();
        assert!(cache_has_recordings(&dir));
        std::fs::remove_dir_all(recordings_dir(&dir)).unwrap();
        std::fs::write(dir.join("rec-legacy.mp4"), b"x").unwrap();
        assert!(cache_has_recordings(&dir), "旧版根目录分片不能被忘记");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 派生缓存修剪只影响 `derived`，而且超过数量上限的文件会被删掉。
    #[test]
    fn prune_derived_keeps_only_the_newest_files() {
        let dir = test_dir("prune-derived");
        for i in 0..3 {
            std::fs::write(dir.join(format!("wave-{i}.bin")), b"x").unwrap();
            // 略微错开 mtime，避免同一时刻排序不确定
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let removed = prune_derived(&dir, 1, std::time::Duration::from_secs(3600));
        assert_eq!(removed, 2, "只保留 1 个文件");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cache 目录选择：exe 目录优先 → 配置目录 → 临时目录；探测失败就往下退。
    #[test]
    fn cache_dir_prefers_exe_dir_then_config_dir() {
        let exe = Path::new(r"C:\app");
        let cfg = Path::new(r"C:\cfg");
        assert_eq!(
            resolve_cache_dir(Some(exe), Some(cfg), &|_| true),
            exe.join(CACHE_DIR_NAME)
        );
        assert_eq!(
            resolve_cache_dir(Some(exe), Some(cfg), &|p| p.starts_with(cfg)),
            cfg.join(CACHE_DIR_NAME)
        );
        let d = resolve_cache_dir(Some(exe), Some(cfg), &|_| false);
        assert!(d.ends_with(CACHE_DIR_NAME), "{}", d.display());
    }
}
