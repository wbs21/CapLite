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

use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL,
    THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_IDLE, THREAD_PRIORITY_NORMAL,
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
}
