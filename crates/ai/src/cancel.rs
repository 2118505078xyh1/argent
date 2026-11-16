//! Interrupt a blocked model stream.
//!
//! `ChildStdout::read` has no read timeout, so a silent model (or a stalled
//! gateway) can hold the agent loop for the whole curl timeout even after the
//! operator pressed Esc. The agent still checks the abort flag between stream
//! events, but that only helps while events are arriving.
//!
//! The watchdog polls the shared abort flag and kills the curl process tree,
//! which closes the pipe and lets the blocked read return. Dropping the guard
//! stops the thread; one thread per request lives only as long as the stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Stops the watchdog when the stream is dropped.
pub struct CancelGuard {
    done: Arc<AtomicBool>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

/// Start a watchdog for a streamed request owned by `pid`.
///
/// Returns `None` when the request carries no abort flag, so a plain
/// one-shot call pays nothing.
pub fn watch(
    cancel: Option<Arc<AtomicBool>>,
    pid: u32,
) -> Option<CancelGuard> {
    let cancel = cancel?;
    let done = Arc::new(AtomicBool::new(false));
    let thread_done = Arc::clone(&done);
    std::thread::spawn(move || loop {
        if thread_done.load(Ordering::Relaxed) {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            argent_core::process::kill_pid_tree(pid);
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    });
    Some(CancelGuard { done })
}

/// True when the operator asked to stop. Used by a stream's `Read::read` so
/// an abort is noticed even before the watchdog closes the pipe.
pub fn cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_stops_the_watchdog() {
        let flag = Arc::new(AtomicBool::new(false));
        let guard = watch(Some(Arc::clone(&flag)), u32::MAX);
        assert!(guard.is_some());
        drop(guard);
        assert!(!cancelled(Some(&flag)));
        flag.store(true, Ordering::Relaxed);
        assert!(cancelled(Some(&flag)));
        assert!(!cancelled(None));
    }
}
