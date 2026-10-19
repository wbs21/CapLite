//! 锁的小工具：**被毒化也照样把数据取出来**（P45，健壮性审查）。
//!
//! ## 为什么需要它
//!
//! 本项目的共享状态几乎都是"状态快照 + 计数器"：录制状态机、帧/字节统计、
//! 音频环形缓冲。它们被采集线程、编码线程、音频线程和 **UI 线程**同时访问。
//!
//! `Mutex::lock()` 在**持锁线程 panic 之后**会返回 `Err(PoisonError)`。如果调用点
//! 用 `.unwrap()`，那么"某个后台线程偶发 panic"会沿着锁**传染**成"UI 线程每帧取状态时
//! 也 panic" —— 一次局部故障升级成整个程序崩溃（而且 release 下没有控制台，
//! 用户只看到窗口消失）。
//!
//! 这些锁里存的东西没有"跨字段不变量"：写入都是一次赋值（枚举/整数/字符串/Vec 的
//! push-clear），Rust 保证不会看到写了一半的内存；即使某个线程在 `push` 中途 panic，
//! 恢复出来的也是一个**合法**的 `Vec`（最多少一个元素）。所以这里的选择是
//! **恢复而不是崩掉**：丢一次计数，好过整个程序消失。
//!
//! ## 用法
//!
//! ```ignore
//! use crate::sync::lock_or_recover;
//! let mut s = lock_or_recover(&self.stats);
//! s.frames += 1;
//! ```
//!
//! 注意：**不要**用它包住"必须整体原子更新"的临界区（本项目目前没有这种）。
//! 真需要"全有或全无"时，正确做法是把不变量收进一个结构体，让一次写入完成整组更新。

use std::sync::{Mutex, MutexGuard};

/// 取锁；锁被毒化时**取回内部数据继续用**，而不是 panic。
///
/// 参数用 `&Mutex<T>`（而不是 `&Arc<Mutex<T>>`）：调用点写 `lock_or_recover(&self.state)`
/// 时，`&Arc<Mutex<T>>` 会自动解引用成 `&Mutex<T>`，不需要在几十个调用点写 `.as_ref()`。
pub fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            // 只记 debug：这是"某个线程已经 panic 过"的二次信号，真正的错误
            // 由那个线程自己的日志/panic hook 负责，这里再刷一行只会重复。
            tracing::debug!("互斥锁曾被毒化（有线程 panic 过），已恢复内部数据继续使用");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// **P45 守门测试**：持锁线程 panic 之后，`lock_or_recover` 仍能拿到数据，
    /// 而且拿到的是 panic 之前**已经写进去**的那一份（不是默认值）。
    ///
    /// 反例（改造前的行为）由这条测试直接证明：同样的场景下 `m.lock().unwrap()` 会 panic。
    #[test]
    fn p45_lock_survives_a_poisoned_mutex() {
        let m = Arc::new(Mutex::new(7u32));
        let m2 = Arc::clone(&m);

        // 在另一个线程里持锁 panic → 锁被毒化
        let h = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 42;
            panic!("故意 panic，把锁毒化");
        });
        assert!(h.join().is_err(), "子线程应当是 panic 结束的");
        assert!(m.lock().is_err(), "锁应当已被毒化（这条是场景前提）");

        // 恢复路径：拿到的还是 panic 前写进去的值
        assert_eq!(*lock_or_recover(&m), 42);
        // 而且能继续写（恢复出来的 guard 是真 guard，不是副本）
        *lock_or_recover(&m) += 1;
        assert_eq!(*lock_or_recover(&m), 43);
    }

    /// 没有毒化时行为与 `lock().unwrap()` 完全一致（不能因为引入工具就改变正常路径）。
    #[test]
    fn p45_lock_behaves_normally_when_not_poisoned() {
        let m = Mutex::new(String::from("ok"));
        assert_eq!(&*lock_or_recover(&m), "ok");
        lock_or_recover(&m).push('!');
        assert_eq!(&*lock_or_recover(&m), "ok!");
    }
}
