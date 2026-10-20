//! 单实例保护。
//!
//! 为什么需要它：两个 CapLite 会同时写同一个 `config.json.tmp`、抢同一个
//! `rec-<秒>.mp4`、注册同一个全局热键、各放一个托盘图标。配置/录制分片的覆盖
//! 是**不可恢复的数据损坏**，不是“多开一个窗口”的小问题。
//!
//! 做法：进程级命名互斥体（`Local\` 命名空间，按会话隔离；RDP 多用户互不影响）。
//! 第二个实例找到已有主窗口就把它恢复到前台，然后退出；找不到窗口也退出，
//! 避免“看起来启动了、实际没启动”的困惑。

use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS,
};
use windows::Win32::System::Threading::CreateMutexW;

/// 互斥体名字。`Local\` 只在本登录会话可见，正好匹配“一台机器上同一用户只开一个”。
const MUTEX_NAME: windows::core::PCWSTR = w!(r"Local\CapLite.SingleInstance");

/// 是否已经由本进程成功持有互斥体。
static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 尝试认领单实例。
///
/// * `Ok(true)`：本进程持有互斥体，可以继续启动；
/// * `Ok(false)`：已有实例在运行，调用方应激活它并退出；
/// * `Err(_)`：连互斥体都创建不了（极端权限/内核对象限制）。此时**失败关闭**：
///   多开会争用配置、Cache、热键和导出文件，数据损坏的代价高于“暂时打不开”，
///   调用方必须把错误明确告诉用户。
pub fn claim() -> Result<bool, String> {
    use std::sync::atomic::Ordering;
    if CLAIMED.load(Ordering::Acquire) {
        return Ok(true);
    }
    // 先清零 last-error，避免把上一个无关 Win32 调用的 ERROR_ALREADY_EXISTS 误当成本次结果。
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: 名字是静态 UTF-16；不请求初始所有权，句柄在成功路径上有意泄漏到进程结束。
    match unsafe { CreateMutexW(None, false, MUTEX_NAME) } {
        Ok(handle) => {
            // CreateMutexW 在“已存在”时也返回有效句柄，必须用 GetLastError 区分。
            let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            if already {
                let _ = unsafe { CloseHandle(handle) };
                return Ok(false);
            }
            // 成功路径**故意不关闭**：互斥体要活到进程结束。句柄是 Copy 类型、
            // 没有 Drop，直接丢弃即可；进程退出时内核自动回收。
            let _ = handle;
            CLAIMED.store(true, Ordering::Release);
            Ok(true)
        }
        Err(e) => Err(format!("创建单实例互斥体失败：{e}")),
    }
}

/// 把已有实例的主窗口恢复到前台。返回是否成功找到并显示。
///
/// 第一个实例可能还在创建窗口（例如刚双击），所以做有限重试；每次 50 ms，
/// 最多约 1.5 s。找不到也不阻塞第二个实例退出。
pub fn activate_existing() -> bool {
    for _ in 0..30 {
        let (_before, _front, after) = crate::tray::show_main_window_now();
        if after == Some(true) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}
