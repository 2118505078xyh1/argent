//! Test-only sink for ACP's stdout protocol.
//!
//! ACP writes JSON-RPC lines to stdout. Unit tests that exercise a single
//! handshake must not print those lines into the test log, so `write_message`
//! diverts them here when a sink is installed. Production code never sees
//! this module.

use std::sync::{Arc, Mutex};

type Sink = Arc<Mutex<Vec<String>>>;

thread_local! {
    static SINK: std::cell::RefCell<Option<Sink>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with ACP output captured instead of written to stdout.
pub(crate) fn with_sink<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    SINK.with(|slot| {
        let previous = slot.borrow().clone();
        *slot.borrow_mut() = Some(Arc::clone(&sink));
        let result = f();
        *slot.borrow_mut() = previous;
        (result, sink.lock().unwrap().clone())
    })
}

/// The sink installed for the current test thread, if any.
pub(crate) fn sink() -> Option<Sink> {
    SINK.with(|slot| slot.borrow().clone())
}
