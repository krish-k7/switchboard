//! Errors, sorted into the kinds callers handle differently, with the
//! message text shown to people and written into error details. System
//! errors read `[Errno N] <strerror text>`, followed by `: 'path'`
//! (or `'source' -> 'target'`) when a file is involved; clients and tests
//! rely on that exact form.

use std::fmt;
use std::path::Path;

use crate::text::repr_str;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A malformed or refused value (including JSON and UUID errors).
    Value,
    /// A genuine session that is not in the Switchboard selection (handled as
    /// a value error).
    NotJoined,
    /// An operating-system error (files, sockets, timeouts).
    Os,
    /// A native write started; delivery can no longer be called failed
    /// (handled as an operating-system error).
    WriteUnknown,
    /// A child process failed or timed out.
    Subprocess,
    /// A value of the wrong type.
    Type,
    /// Writing an event to the bridge failed (its output pipe is gone). Every
    /// handler that catches it re-emits and fails again, so it always ends the
    /// thread that hit it.
    Output,
}

#[derive(Clone, Debug)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
}

impl Error {
    pub fn value(message: impl Into<String>) -> Self {
        Error {
            kind: Kind::Value,
            message: message.into(),
        }
    }

    pub fn type_error(message: impl Into<String>) -> Self {
        Error {
            kind: Kind::Type,
            message: message.into(),
        }
    }

    pub fn os(message: impl Into<String>) -> Self {
        Error {
            kind: Kind::Os,
            message: message.into(),
        }
    }

    pub fn subprocess(message: impl Into<String>) -> Self {
        Error {
            kind: Kind::Subprocess,
            message: message.into(),
        }
    }

    pub fn not_joined(message: impl Into<String>) -> Self {
        Error {
            kind: Kind::NotJoined,
            message: message.into(),
        }
    }

    pub fn write_unknown() -> Self {
        Error {
            kind: Kind::WriteUnknown,
            message: "native write outcome uncertain".into(),
        }
    }

    /// A socket timeout: an operating-system error reading `timed out`.
    pub fn timed_out() -> Self {
        Error::os("timed out")
    }

    /// `[Errno N] <strerror>`, without a file name (sockets).
    pub fn errno(code: i32) -> Self {
        Error::os(format!("[Errno {}] {}", code, strerror(code)))
    }

    /// `[Errno N] <strerror>: 'path'`.
    pub fn errno_path(code: i32, path: &Path) -> Self {
        Error::os(format!(
            "[Errno {}] {}: {}",
            code,
            strerror(code),
            repr_str(&path.to_string_lossy())
        ))
    }

    /// `[Errno N] <strerror>: 'source' -> 'target'`, for a rename.
    pub fn errno_paths(code: i32, source: &Path, target: &Path) -> Self {
        Error::os(format!(
            "[Errno {}] {}: {} -> {}",
            code,
            strerror(code),
            repr_str(&source.to_string_lossy()),
            repr_str(&target.to_string_lossy())
        ))
    }

    pub fn last_os() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    pub fn from_io(exc: &std::io::Error, path: Option<&Path>) -> Self {
        match (exc.raw_os_error(), path) {
            (Some(code), Some(path)) => Error::errno_path(code, path),
            (Some(code), None) => Error::errno(code),
            _ => Error::os(exc.to_string()),
        }
    }

    /// An operating-system error, including an uncertain native write.
    pub fn is_os(&self) -> bool {
        matches!(self.kind, Kind::Os | Kind::WriteUnknown)
    }

    /// An operating-system, value, not-joined, or child-process error.
    pub fn is_os_value_subprocess(&self) -> bool {
        self.is_os() || matches!(self.kind, Kind::Value | Kind::NotJoined | Kind::Subprocess)
    }

    /// Any expected failure: [`Error::is_os_value_subprocess`] or a type error.
    pub fn is_handled(&self) -> bool {
        self.is_os_value_subprocess() || self.kind == Kind::Type
    }

    /// The message cut to its first `limit` code points.
    pub fn truncated(&self, limit: usize) -> String {
        crate::text::take_chars(&self.message, limit)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The C library's `strerror` message for an errno.
pub fn strerror(code: i32) -> String {
    // SAFETY: strerror returns a pointer to a NUL-terminated static or
    // thread-local buffer; it is copied immediately.
    unsafe {
        let message = libc::strerror(code);
        if message.is_null() {
            return format!("Unknown error {code}");
        }
        std::ffi::CStr::from_ptr(message)
            .to_string_lossy()
            .into_owned()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
