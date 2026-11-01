//! The terminal under the prompts: raw input, keys (with bracketed paste),
//! the window size, and putting the terminal back however the program ends.
//!
//! Prompts read keys from standard input in raw mode and draw on standard
//! error. Ctrl-C while a prompt reads keys arrives as a key, so the prompt
//! can undo what it started; at any other time it arrives as SIGINT, and the
//! handler restores the terminal (echo, cursor, paste mode) before the
//! process exits with status 130 (SIGTERM, SIGHUP, and SIGQUIT the same way,
//! with their own statuses). Work that must not stop halfway runs in a
//! `critical` section: a signal that arrives during it takes effect when the
//! section ends. A panic restores the terminal before its message prints
//! (`crate::panics`). SIGTSTP (Ctrl-Z outside a prompt, or `kill -TSTP`)
//! restores the terminal and suspends; on SIGCONT raw input and paste mode
//! come back as they were, and the prompt redraws at the next key.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The terminal settings from before raw mode, restored on the way out.
static SAVED: OnceLock<libc::termios> = OnceLock::new();
static RAW: AtomicBool = AtomicBool::new(false);
/// Which settings `RAW` means: raw input, or only echo off (`Quiet`).
static RAW_IS_QUIET: AtomicBool = AtomicBool::new(false);
/// The cursor is hidden or bracketed paste is on.
static DRAWN: AtomicBool = AtomicBool::new(false);
/// Bracketed paste is on (a prompt is reading a line).
static PASTE: AtomicBool = AtomicBool::new(false);
static CRITICAL: AtomicUsize = AtomicUsize::new(0);
static PENDING: AtomicI32 = AtomicI32::new(0);
/// The lines the handler prints when a signal ends the program: before and
/// after the run changed anything on this machine.
static STOP_BEFORE: OnceLock<Vec<u8>> = OnceLock::new();
static STOP_AFTER: OnceLock<Vec<u8>> = OnceLock::new();
/// The run has changed something on this machine (a key, a service, files).
static CHANGED: AtomicBool = AtomicBool::new(false);
/// The program is ending (a signal or a panic): nothing more is drawn, so a
/// spinner can't hide the cursor again or erase the last message.
static ENDING: AtomicBool = AtomicBool::new(false);

const RESTORE: &[u8] = b"\x1b[?2004l\x1b[?25h";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
/// The most a bracketed paste may hold; anything longer is discarded whole.
pub const PASTE_LIMIT: usize = 64 * 1024;

/// Whether `fd` is a terminal.
pub fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty on a descriptor number.
    unsafe { libc::isatty(fd) == 1 }
}

/// The terminal's height in rows, if known.
pub fn rows() -> Option<usize> {
    for fd in [2, 0, 1] {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes a winsize for the descriptor.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_row > 0 {
            return Some(size.ws_row as usize);
        }
    }
    None
}

/// The terminal's width in columns: the window size of standard error (or
/// input), else `COLUMNS`, else 80.
pub fn columns() -> usize {
    for fd in [2, 0, 1] {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes a winsize for the descriptor.
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
            return size.ws_col as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(80)
}

fn write_raw(bytes: &[u8]) {
    // SAFETY: write is async-signal-safe; a short or failed write is ignored.
    unsafe { libc::write(2, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
}

/// Put the terminal back: settings, cursor, and paste mode. Safe in a
/// signal handler.
fn restore_now() {
    if RAW.swap(false, Ordering::SeqCst) {
        if let Some(saved) = SAVED.get() {
            // SAFETY: tcsetattr is async-signal-safe; the settings were saved
            // before raw mode.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
        }
    }
    PASTE.store(false, Ordering::SeqCst);
    if DRAWN.swap(false, Ordering::SeqCst) {
        write_raw(RESTORE);
    }
}

/// The raw-mode (or echo-off) settings derived from the saved ones; pure
/// computation, so safe in a signal handler.
fn input_settings(saved: &libc::termios, quiet: bool) -> libc::termios {
    let mut settings = *saved;
    if quiet {
        settings.c_lflag &= !libc::ECHO;
    } else {
        settings.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        settings.c_iflag &= !(libc::IXON | libc::ICRNL | libc::INLCR);
        settings.c_cc[libc::VMIN] = 1;
        settings.c_cc[libc::VTIME] = 0;
    }
    settings
}

/// SIGTSTP: put the terminal back, stop, and on SIGCONT take it over again
/// as it was (raw input, paste mode). Only async-signal-safe calls.
extern "C" fn on_suspend(_signal: libc::c_int) {
    let raw = RAW.load(Ordering::SeqCst);
    let quiet = RAW_IS_QUIET.load(Ordering::SeqCst);
    let paste = PASTE.load(Ordering::SeqCst);
    if raw {
        if let Some(saved) = SAVED.get() {
            // SAFETY: tcsetattr is async-signal-safe.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
        }
    }
    if DRAWN.load(Ordering::SeqCst) {
        write_raw(RESTORE);
    }
    // SAFETY: sigaction, sigprocmask, and raise are async-signal-safe. The
    // default action stops the process; the handler is put back after.
    unsafe {
        set_handler(libc::SIGTSTP, libc::SIG_DFL);
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTSTP);
        libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
        libc::raise(libc::SIGTSTP);
        // Stopped here until SIGCONT.
        set_handler(libc::SIGTSTP, on_suspend as *const () as libc::sighandler_t);
    }
    // Only take the terminal back if nothing restored it meanwhile.
    if raw && RAW.load(Ordering::SeqCst) {
        if let Some(saved) = SAVED.get() {
            let settings = input_settings(saved, quiet);
            // SAFETY: tcsetattr is async-signal-safe.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &settings) };
        }
    }
    if paste && PASTE.load(Ordering::SeqCst) {
        write_raw(PASTE_ON);
    }
}

/// Install `handler` for `signal`, restarting interrupted reads.
///
/// # Safety
/// `handler` must be `SIG_DFL`, `SIG_IGN`, or a function that only makes
/// async-signal-safe calls.
unsafe fn set_handler(signal: libc::c_int, handler: libc::sighandler_t) {
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = handler;
    action.sa_flags = libc::SA_RESTART;
    libc::sigemptyset(&mut action.sa_mask);
    libc::sigaction(signal, &action, std::ptr::null_mut());
}

/// What to print if a signal ends the program: `before` while nothing on
/// this machine has changed, `after` once something has (default: nothing).
pub fn set_stop_lines(before: String, after: String) {
    let _ = STOP_BEFORE.set(before.into_bytes());
    let _ = STOP_AFTER.set(after.into_bytes());
}

/// Note whether the run has changed anything on this machine yet (set when
/// something is made, cleared when it is undone).
pub fn set_changed(changed: bool) {
    CHANGED.store(changed, Ordering::SeqCst);
}

/// Whether the run has changed anything on this machine.
pub fn changed() -> bool {
    CHANGED.load(Ordering::SeqCst)
}

extern "C" fn on_signal(signal: libc::c_int) {
    if CRITICAL.load(Ordering::SeqCst) > 0 {
        PENDING.store(signal, Ordering::SeqCst);
        return;
    }
    stop(signal);
}

/// Before an aborting panic prints: stop drawing, put the terminal back, and
/// start the message on a line of its own.
fn restore_before_panic() {
    ENDING.store(true, Ordering::SeqCst);
    let drawn = DRAWN.load(Ordering::SeqCst);
    restore_now();
    if drawn {
        write_raw(b"\r\x1b[K\n");
    }
}

fn stop(signal: libc::c_int) -> ! {
    ENDING.store(true, Ordering::SeqCst);
    restore_now();
    // Clear a spinner or prompt line, then say how to pick up.
    write_raw(b"\r\x1b[K");
    let line = if CHANGED.load(Ordering::SeqCst) {
        STOP_AFTER.get()
    } else {
        STOP_BEFORE.get()
    };
    if let Some(line) = line {
        write_raw(line);
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(128 + signal) }
}

/// Install the handlers for SIGINT, SIGTERM, SIGHUP, SIGQUIT, and SIGTSTP,
/// and have a panic put the terminal back before its message.
pub fn install_handlers() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: the handler only touches atomics and async-signal-safe calls.
        unsafe { set_handler(signal, on_signal as *const () as libc::sighandler_t) };
    }
    // SAFETY: as above. A shell that started this without job control set
    // SIGTSTP to be ignored; that is kept.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGTSTP, std::ptr::null(), &mut current);
        if current.sa_sigaction != libc::SIG_IGN {
            set_handler(libc::SIGTSTP, on_suspend as *const () as libc::sighandler_t);
        }
    }
    crate::panics::before_abort(restore_before_panic);
}

/// Run `work` so that a signal arriving meanwhile takes effect only after it.
pub fn critical<T>(work: impl FnOnce() -> T) -> T {
    CRITICAL.fetch_add(1, Ordering::SeqCst);
    let result = work();
    if CRITICAL.fetch_sub(1, Ordering::SeqCst) == 1 {
        let pending = PENDING.swap(0, Ordering::SeqCst);
        if pending != 0 {
            stop(pending);
        }
    }
    result
}

/// Whether a signal arrived during the current critical section.
pub fn interrupted() -> bool {
    PENDING.load(Ordering::SeqCst) != 0
}

/// Note that the cursor was hidden or paste mode turned on.
pub fn mark_drawn() {
    DRAWN.store(true, Ordering::SeqCst);
}

/// Turn bracketed paste on or off (a prompt reading a line).
pub fn paste_mode(on: bool) {
    if on {
        PASTE.store(true, Ordering::SeqCst);
        put("\x1b[?2004h");
        mark_drawn();
    } else {
        put("\x1b[?2004l");
        PASTE.store(false, Ordering::SeqCst);
    }
}

/// Put the terminal back now (also on normal exit).
pub fn restore() {
    restore_now();
}

/// Raw input on standard input while it lives.
pub struct Raw;

impl Raw {
    /// Raw mode: no echo, no line editing, no signals from keys; output
    /// processing stays on so `\n` still starts a new line.
    pub fn enter() -> std::io::Result<Raw> {
        // SAFETY: tcgetattr fills a zeroed termios; tcsetattr reads one.
        unsafe {
            let mut current: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut current) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let saved = *SAVED.get_or_init(|| current);
            let raw = input_settings(&saved, false);
            RAW_IS_QUIET.store(false, Ordering::SeqCst);
            RAW.store(true, Ordering::SeqCst);
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                RAW.store(false, Ordering::SeqCst);
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if RAW.swap(false, Ordering::SeqCst) {
            if let Some(saved) = SAVED.get() {
                // SAFETY: restores the settings saved before raw mode.
                unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
            }
        }
    }
}

/// Echo off (canonical input kept) while it lives: for reading a secret
/// line on a terminal that cannot draw prompts.
pub struct Quiet;

impl Quiet {
    pub fn enter() -> Option<Quiet> {
        // SAFETY: as in Raw::enter.
        unsafe {
            let mut current: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut current) != 0 {
                return None;
            }
            let saved = *SAVED.get_or_init(|| current);
            let quiet = input_settings(&saved, true);
            RAW_IS_QUIET.store(true, Ordering::SeqCst);
            RAW.store(true, Ordering::SeqCst);
            (libc::tcsetattr(0, libc::TCSANOW, &quiet) == 0).then_some(Quiet)
        }
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        if RAW.swap(false, Ordering::SeqCst) {
            if let Some(saved) = SAVED.get() {
                // SAFETY: restores the settings saved before.
                unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
            }
        }
    }
}

/// A key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Tab,
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D.
    EndOfInput,
    /// Ctrl-U: clear the line.
    ClearLine,
    /// Ctrl-W: delete the word before the cursor.
    DeleteWord,
    Escape,
    /// Text pasted in one piece (bracketed paste).
    Paste(String),
    /// A bracketed paste longer than `PASTE_LIMIT`, read to its end and
    /// discarded, so none of it arrives as keys.
    PasteTooLarge,
    Other,
}

/// Bytes waiting on standard input within `millis`.
pub fn input_ready(millis: i32) -> bool {
    let mut poll = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd.
    unsafe { libc::poll(&mut poll, 1, millis) > 0 }
}

fn read_byte(timeout: Option<i32>) -> std::io::Result<Option<u8>> {
    if let Some(millis) = timeout {
        if !input_ready(millis) {
            return Ok(None);
        }
    }
    let mut byte = 0u8;
    loop {
        // SAFETY: reads one byte into a local.
        let count = unsafe { libc::read(0, &mut byte as *mut u8 as *mut libc::c_void, 1) };
        if count == 1 {
            return Ok(Some(byte));
        }
        if count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the terminal closed",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Read one key from standard input (in raw mode).
pub fn read_key() -> std::io::Result<Key> {
    let Some(first) = read_byte(None)? else {
        return Ok(Key::Other);
    };
    // Test builds: a key that panics, for the terminal-restore test.
    #[cfg(feature = "fault-injection")]
    if std::env::var_os("SWITCHBOARD_FAULT_PANIC_ON_KEY").is_some() {
        panic!("forced panic on a key (fault injection)");
    }
    Ok(match first {
        0x03 => Key::Interrupt,
        0x04 => Key::EndOfInput,
        0x01 => Key::Home,
        0x05 => Key::End,
        0x15 => Key::ClearLine,
        0x17 => Key::DeleteWord,
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x7f | 0x08 => Key::Backspace,
        0x1b => escape()?,
        byte if byte < 0x20 => Key::Other,
        byte => Key::Char(utf8(byte)?),
    })
}

fn utf8(first: u8) -> std::io::Result<char> {
    let extra = match first {
        0x00..=0x7f => 0,
        0xc0..=0xdf => 1,
        0xe0..=0xef => 2,
        0xf0..=0xf7 => 3,
        _ => return Ok('\u{fffd}'),
    };
    let mut bytes = vec![first];
    for _ in 0..extra {
        match read_byte(Some(50))? {
            Some(byte) => bytes.push(byte),
            None => break,
        }
    }
    Ok(String::from_utf8_lossy(&bytes)
        .chars()
        .next()
        .unwrap_or('\u{fffd}'))
}

fn escape() -> std::io::Result<Key> {
    // Long enough that an arrow key over a slow connection isn't read as Esc.
    let Some(second) = read_byte(Some(100))? else {
        return Ok(Key::Escape);
    };
    if second != b'[' && second != b'O' {
        return Ok(Key::Other);
    }
    let mut parameters = String::new();
    loop {
        let Some(byte) = read_byte(Some(40))? else {
            return Ok(Key::Other);
        };
        if byte.is_ascii_digit() || byte == b';' {
            parameters.push(byte as char);
            continue;
        }
        return Ok(match (byte, parameters.as_str()) {
            (b'A', _) => Key::Up,
            (b'B', _) => Key::Down,
            (b'C', _) => Key::Right,
            (b'D', _) => Key::Left,
            (b'H', _) => Key::Home,
            (b'F', _) => Key::End,
            (b'~', "1") | (b'~', "7") => Key::Home,
            (b'~', "4") | (b'~', "8") => Key::End,
            (b'~', "3") => Key::Delete,
            (b'~', "200") => pasted()?,
            _ => Key::Other,
        });
    }
}

/// A bracketed paste, read up to its closing `ESC [ 201 ~`.
fn pasted() -> std::io::Result<Key> {
    paste_from(|| read_byte(Some(2000)))
}

/// A bracketed paste from `next` (a byte, or `None` once input pauses): its
/// text, or `PasteTooLarge` when it holds more than `PASTE_LIMIT` bytes, in
/// which case the rest is still read up to the closing marker (or a pause),
/// so it never spills into the keys that follow.
fn paste_from(mut next: impl FnMut() -> std::io::Result<Option<u8>>) -> std::io::Result<Key> {
    const END: &[u8] = b"\x1b[201~";
    let mut bytes = Vec::new();
    let mut too_large = false;
    while !bytes.ends_with(END) {
        let Some(byte) = next()? else {
            break;
        };
        bytes.push(byte);
        if bytes.len() > PASTE_LIMIT + END.len() {
            // Keep only enough to see the closing marker.
            too_large = true;
            bytes.drain(..bytes.len() - END.len());
        }
    }
    if too_large {
        return Ok(Key::PasteTooLarge);
    }
    if bytes.ends_with(END) {
        bytes.truncate(bytes.len() - END.len());
    }
    Ok(Key::Paste(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Write to standard error, flushed; a closed stream is ignored.
pub fn put(value: &str) {
    if ENDING.load(Ordering::SeqCst) {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(value.as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(
        bytes: Vec<u8>,
    ) -> (
        impl FnMut() -> std::io::Result<Option<u8>>,
        std::rc::Rc<std::cell::Cell<usize>>,
    ) {
        let at = std::rc::Rc::new(std::cell::Cell::new(0));
        let read = at.clone();
        (
            move || {
                let index = read.get();
                read.set(index + 1);
                Ok(bytes.get(index).copied())
            },
            at,
        )
    }

    #[test]
    fn a_paste_is_read_to_its_end() {
        let (next, _) = source(b"sbi1.abc\x1b[201~".to_vec());
        assert_eq!(paste_from(next).unwrap(), Key::Paste("sbi1.abc".into()));
        // Exactly the limit still fits.
        let mut exact = vec![b'a'; PASTE_LIMIT];
        exact.extend_from_slice(b"\x1b[201~");
        let (next, _) = source(exact);
        assert_eq!(
            paste_from(next).unwrap(),
            Key::Paste("a".repeat(PASTE_LIMIT))
        );
    }

    #[test]
    fn a_paste_over_the_limit_is_discarded_to_its_end() {
        let mut bytes = vec![b'x'; PASTE_LIMIT + 5000];
        bytes.extend_from_slice(b"\x1b[201~");
        let end = bytes.len();
        bytes.extend_from_slice(b"ab");
        let (next, at) = source(bytes);
        assert_eq!(paste_from(next).unwrap(), Key::PasteTooLarge);
        // Everything up to the closing marker was read, and nothing after.
        assert_eq!(at.get(), end);
    }
}
