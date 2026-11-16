//! Interrupt handling: turn Ctrl+C into a cooperative abort.
//!
//! The handler is installed once and only sets a flag. The agent loop polls
//! the flag between steps, so an interrupt never leaves a half-written session
//! or a half-executed tool batch behind.
//!
//! The console hook is bound directly through FFI so argent keeps its
//! zero-dependency promise.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// 0 = idle, 1 = interrupt requested.
static STATE: AtomicU8 = AtomicU8::new(0);

/// True once the operator asked to interrupt.
pub fn is_interrupted() -> bool {
    STATE.load(Ordering::SeqCst) == 1
}

/// Consume a pending interrupt. Returns true when one was pending.
pub fn take() -> bool {
    STATE.swap(0, Ordering::SeqCst) == 1
}

/// Clear any pending interrupt.
pub fn clear() {
    STATE.store(0, Ordering::SeqCst);
}

/// Request an interrupt from any thread.
pub fn request() {
    STATE.store(1, Ordering::SeqCst);
}

/// 可克隆的取消信号，供工具上下文和进程等待循环共享。
///
/// 一个 `Cancellation` 的所有克隆共享同一个关联槽；`install` 把某次
/// 运行的中止标志接进来之后，任一线程触发中止，正在等待子进程的工具
/// 都会在下一个轮询点看到。全局 Ctrl+C 标志也算取消，这样一次运行里
/// 启动的所有命令都能一起停下。
#[derive(Clone, Default)]
pub struct Cancellation {
    /// 外部中止标志（例如 agent 的 `AbortHandle`）。所有克隆共享这个槽。
    linked: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl std::fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Cancellation {
    pub fn new() -> Cancellation {
        Cancellation::default()
    }

    /// 关联一次运行的中止标志。
    pub fn install(&self, flag: Arc<AtomicBool>) {
        if let Ok(mut slot) = self.linked.lock() {
            *slot = Some(flag);
        }
    }

    /// 解除关联；下一次运行会重新关联。
    pub fn clear(&self) {
        if let Ok(mut slot) = self.linked.lock() {
            *slot = None;
        }
    }

    /// 是否已取消：外部中止标志或全局 Ctrl+C 任一成立。
    pub fn is_cancelled(&self) -> bool {
        if is_interrupted() {
            return true;
        }
        let flag = self.linked.lock().ok().and_then(|slot| slot.clone());
        flag.map(|flag| flag.load(Ordering::Relaxed))
            .unwrap_or(false)
    }
}

/// Install the platform interrupt handler.
///
/// Returns `true` when a handler is now active. Installing twice is a no-op.
pub fn install() -> bool {
    #[cfg(windows)]
    {
        extern "system" {
            fn SetConsoleCtrlHandler(
                handler: Option<unsafe extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }

        /// Matches `PHANDLER_ROUTINE`: returning 1 tells the console not to
        /// terminate the process.
        unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
            request();
            1
        }

        unsafe { SetConsoleCtrlHandler(Some(handler), 1) != 0 }
    }
    #[cfg(unix)]
    {
        extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        const SIGINT: i32 = 2;

        extern "C" fn handler(_signum: i32) {
            request();
        }

        unsafe {
            signal(SIGINT, handler as usize);
        }
        true
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_then_take_clears() {
        clear();
        assert!(!is_interrupted());
        request();
        assert!(is_interrupted());
        assert!(take());
        assert!(!is_interrupted());
        assert!(!take());
    }

    #[test]
    fn a_linked_flag_cancels_every_clone() {
        clear();
        let cancellation = Cancellation::new();
        let clone = cancellation.clone();
        assert!(!cancellation.is_cancelled());
        let flag = Arc::new(AtomicBool::new(false));
        cancellation.install(Arc::clone(&flag));
        assert!(!clone.is_cancelled());
        flag.store(true, Ordering::Relaxed);
        assert!(clone.is_cancelled());
        cancellation.clear();
        flag.store(false, Ordering::Relaxed);
        assert!(!clone.is_cancelled());
        clear();
    }
}
