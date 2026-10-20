//! 系统级小工具：**线程优先级**。
//!
//! 为什么需要它：录屏的三条链（音频采集/搬运、音频消费、视频采集+编码）抢的是同一批
//! CPU 核。参考方案（`add/` 目录的 win_highperf_recorder）的分层结论是
//! **"优先保障音频，视频可丢帧"** —— 用操作系统调度器兑现，而不是靠线程间让锁：
//!
//! | 线程 | 优先级 | 理由 |
//! |---|---|---|
//! | 音频搬运（小环→大环） | `Highest` | 唯一职责是把硬件数据搬进大缓冲，必须随时可跑 |
//! | WASAPI 读取 / 音频消费 | `AboveNormal` | 读取慢一步 → WASAPI 环回缓冲被覆盖 → **爆音丢音**；消费慢 → AAC 断供 |
//! | 视频采集+编码 | `BelowNormal` | 可牺牲：丢帧只表现为画面略卡（pts 按真实时刻，不会快放） |
//!
//! 没有用 `REALTIME`：Windows 上 realtime 优先级有整机卡死风险，参考方案同样规避。

use std::path::Path;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_IDLE,
    THREAD_PRIORITY_NORMAL,
};

/// 线程优先级（刻意不含 `TimeCritical`/`Realtime`：见模块文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPriority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    Highest,
}

impl ThreadPriority {
    /// 给日志/诊断用的短名
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::BelowNormal => "below_normal",
            Self::Normal => "normal",
            Self::AboveNormal => "above_normal",
            Self::Highest => "highest",
        }
    }

    fn to_native(self) -> windows::Win32::System::Threading::THREAD_PRIORITY {
        match self {
            Self::Idle => THREAD_PRIORITY_IDLE,
            Self::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
            Self::Normal => THREAD_PRIORITY_NORMAL,
            Self::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
            Self::Highest => THREAD_PRIORITY_HIGHEST,
        }
    }
}

/// 把**当前线程**的优先级设为 `p`。返回是否成功（失败不致命，记录日志即可）。
///
/// `GetCurrentThread` 返回的是伪句柄，不需要（也不能）关闭。
pub fn set_current_thread_priority(p: ThreadPriority) -> bool {
    unsafe { SetThreadPriority(GetCurrentThread(), p.to_native()).is_ok() }
}

/// 路径本身是否是 reparse point（符号链接 / junction）。
///
/// 受管文件（配置备份、日志）在打开/覆盖前必须查这一项：普通 `OpenOptions`
/// 会跟随链接，管理员进程就可能被诱导去追加/覆盖任意目标文件。
/// 用 `symlink_metadata` 而不是 `metadata`，否则已经顺着链接看到目标了。
pub fn is_reparse_point(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        const ATTR_REPARSE_POINT: u32 = 0x400;
        std::fs::symlink_metadata(path)
            .map(|m| m.file_attributes() & ATTR_REPARSE_POINT != 0)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    }
}

/// 路径自身或其任一已存在父级是否是 reparse point。
///
/// 受管目录（配置/日志）不能只检查最后一级：父级 junction 同样会把写入重定向。
pub fn has_reparse_ancestor(path: &Path) -> bool {
    path.ancestors().skip(1).any(is_reparse_point)
}

/// 文件是否是硬链接（Windows 用句柄信息里的 `nNumberOfLinks` 判断）。
///
/// 只看 `FILE_ATTRIBUTE_REPARSE_POINT` 查不到硬链接；日志以追加方式打开时
/// 必须把硬链接也当作受管路径的拒绝条件。
pub fn is_hardlink(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: 句柄属于仍在作用域的 `file`，`info` 是可写输出结构。
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info).is_ok()
                && info.nNumberOfLinks > 1
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 日志/配置备份等受管文件路径是否安全（不是 reparse point，也不是硬链接）。
pub fn is_safe_managed_file(path: &Path) -> bool {
    !is_reparse_point(path) && !is_hardlink(path) && !has_reparse_ancestor(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个档位都能设上（在普通测试线程上；失败通常是系统策略限制，真机上应全部为真）。
    #[test]
    fn every_priority_level_can_be_set() {
        for p in [
            ThreadPriority::Idle,
            ThreadPriority::BelowNormal,
            ThreadPriority::Normal,
            ThreadPriority::AboveNormal,
            ThreadPriority::Highest,
        ] {
            assert!(
                set_current_thread_priority(p),
                "{} 档位应能设置成功",
                p.label()
            );
        }
        // 收尾：还原普通优先级，避免影响同一测试进程里的其它线程
        assert!(set_current_thread_priority(ThreadPriority::Normal));
    }

    /// 档位名稳定（日志/文档里引用）
    #[test]
    fn labels_are_stable() {
        assert_eq!(ThreadPriority::Highest.label(), "highest");
        assert_eq!(ThreadPriority::AboveNormal.label(), "above_normal");
        assert_eq!(ThreadPriority::BelowNormal.label(), "below_normal");
        assert_eq!(ThreadPriority::Normal.label(), "normal");
        assert_eq!(ThreadPriority::Idle.label(), "idle");
    }

    #[test]
    fn missing_path_is_not_a_reparse_point() {
        assert!(!is_reparse_point(std::path::Path::new(
            "definitely-not-here-caplite"
        )));
    }

    #[cfg(windows)]
    #[test]
    fn hardlink_is_detected() {
        let dir = std::env::temp_dir().join(format!("cap-sys-hardlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.log");
        let b = dir.join("b.log");
        std::fs::write(&a, b"x").unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        assert!(is_hardlink(&a));
        assert!(!is_safe_managed_file(&a));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
