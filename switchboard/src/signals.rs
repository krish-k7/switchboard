//! SIGTERM/SIGINT handling: the handler only records the signal and writes a
//! wake byte (async-signal-safe), so a signal delivered to any thread wakes
//! the blocked main and listener waits without periodic polling.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static SIGNALLED: AtomicBool = AtomicBool::new(false);
static MAIN_FD: AtomicI32 = AtomicI32::new(-1);
static LISTENER_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_signal: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
    let byte = 0u8;
    for fd in [
        MAIN_FD.load(Ordering::SeqCst),
        LISTENER_FD.load(Ordering::SeqCst),
    ] {
        if fd >= 0 {
            // SAFETY: write(2) is async-signal-safe; errors are ignored
            // because a full channel already carries a wakeup.
            unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
        }
    }
}

/// Install handlers that wake `main_fd` and `listener_fd` (descriptors that
/// stay open for the life of the process).
pub fn install(main_fd: RawFd, listener_fd: RawFd) {
    MAIN_FD.store(main_fd, Ordering::SeqCst);
    LISTENER_FD.store(listener_fd, Ordering::SeqCst);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: a zeroed sigaction with a valid handler and empty mask.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Whether SIGTERM or SIGINT has arrived.
pub fn signalled() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}
