//! Panics end the process, except in threads that opt in to containment.
//!
//! The release profile unwinds on panic so that the server can survive a
//! panic in one request handler: the handler's thread unwinds, its drop
//! guards release its connection slot and per-machine counters, and the
//! other connections carry on. Everywhere else a panic still aborts the
//! process at once, as it did when the profile aborted: a bridge or helper
//! thread that died silently would leave a process that looks alive but no
//! longer works, while an abort lets the service manager restart it.
//!
//! Before a panic that aborts prints its message, the hook runs what
//! `before_abort` registered: the interactive commands put the terminal back
//! (echo, line editing, cursor, paste mode), so the message is readable and
//! the shell works after the abort.

use std::cell::Cell;
use std::sync::OnceLock;

/// Run before an aborting panic prints (the terminal's restore).
static BEFORE_ABORT: OnceLock<fn()> = OnceLock::new();

/// Have an aborting panic run `restore` before it prints. The first
/// registration stays.
pub fn before_abort(restore: fn()) {
    let _ = BEFORE_ABORT.set(restore);
}

thread_local! {
    static CONTAINED: Cell<bool> = const { Cell::new(false) };
}

/// Mark the current thread as one whose panics end only the thread.
pub fn contain() {
    CONTAINED.with(|flag| flag.set(true));
}

/// Whether a panic on the current thread ends only the thread.
pub fn contained() -> bool {
    CONTAINED.with(Cell::get)
}

/// Install the hook: print the panic as usual, then abort unless the
/// panicking thread is contained (the terminal is put back first).
pub fn install_hook() {
    let print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let aborting = !contained();
        if aborting {
            if let Some(restore) = BEFORE_ABORT.get() {
                restore();
            }
        }
        print(info);
        if aborting {
            std::process::abort();
        }
    }));
}
