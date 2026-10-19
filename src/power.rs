//! 录制期间的电源保持。
//!
//! `SetThreadExecutionState` 是**按线程**生效的，而且线程退出时状态会失效。
//! 所以这个 guard 必须由**活得和录制一样久**的线程持有 —— 当前是封装线程
//! `cap-mux`，它在整个录制会话里都存在。**不要**在 `Recorder::start` 的调用线程
//! （可能是 UI 线程，也可能是一次性的启动线程）里调用它。
//!
//! 失败开放：系统策略不允许阻止休眠时，只记日志，录制照常。

use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    EXECUTION_STATE,
};

/// 持有期间阻止系统休眠/关屏；`Drop` 恢复为 `ES_CONTINUOUS`。
pub struct RecordingPowerGuard {
    active: bool,
}

impl RecordingPowerGuard {
    /// 在**当前线程**上请求 `SYSTEM_REQUIRED | DISPLAY_REQUIRED`。
    pub fn keep_awake() -> Self {
        let flags = EXECUTION_STATE(
            ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0 | ES_DISPLAY_REQUIRED.0,
        );
        // SAFETY: 纯 Win32 状态调用，不涉及指针/内存。
        let previous = unsafe { SetThreadExecutionState(flags) };
        let active = previous.0 != 0;
        if !active {
            tracing::warn!("SetThreadExecutionState 失败：本次录制无法阻止系统休眠/关屏");
        }
        Self { active }
    }
}

impl Drop for RecordingPowerGuard {
    fn drop(&mut self) {
        if self.active {
            // 只清本线程的请求；不要用 ES_SYSTEM_REQUIRED 之类的组合。
            // SAFETY: 同 keep_awake。
            let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机探针：创建一个 guard 再 drop，不应 panic；失败时 active=false 也合法。
    #[test]
    fn guard_is_safe_to_create_and_drop() {
        let g = RecordingPowerGuard::keep_awake();
        drop(g);
    }
}
