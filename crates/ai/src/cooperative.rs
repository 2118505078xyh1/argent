//! Cooperative scheduling hook for coroutine-based subagent runs.
//!
//! When several child agents advance on one thread (see the delegate
//! scheduler), every blocking read must yield instead of stalling the
//! others. The coroutine body registers a yield closure through
//! [`set_yielder`]; blocking readers (curl-backed streams) call
//! [`yield_now`] whenever the pipe is empty, which suspends the coroutine
//! and lets the scheduler run another one.
//!
//! The hook is per-thread and cleared on drop, so plain blocking runs
//! (no coroutine) behave exactly as before.

use std::cell::RefCell;
use std::rc::Rc;

thread_local! {
    static YIELDER: RefCell<Option<Rc<dyn Fn()>>> = RefCell::new(None);
}

/// Register this thread's yield closure. Must be cleared before the
/// coroutine's stack disappears (the guard does it on drop).
pub struct YieldGuard {
    _private: (),
}

impl Drop for YieldGuard {
    fn drop(&mut self) {
        YIELDER.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Install the yield closure for the current thread. The closure must be
/// valid for as long as the guard lives.
///
/// # Safety
/// The closure may borrow the coroutine's yielder, whose stack stays alive
/// while the coroutine is suspended; the caller guarantees the guard does
/// not outlive the coroutine.
pub unsafe fn set_yielder(closure: Rc<dyn Fn()>) -> YieldGuard {
    YIELDER.with(|slot| *slot.borrow_mut() = Some(closure));
    YieldGuard { _private: () }
}

/// Call the installed yield closure; a no-op outside a coroutine.
pub fn yield_now() {
    YIELDER.with(|slot| {
        if let Some(closure) = slot.borrow().as_ref() {
            closure();
        }
    });
}

/// Whether this thread runs inside a cooperative coroutine.
pub fn is_cooperative() -> bool {
    YIELDER.with(|slot| slot.borrow().is_some())
}
