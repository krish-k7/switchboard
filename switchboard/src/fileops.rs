//! File operations the bridge and CLI share: private temporary files with
//! random names, atomic replacement with fsync, `flock` locks, and lexical
//! path normalization.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::error::{Error, Result};
use crate::fsutil;

const TEMP_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";

fn random_name(prefix: &str, suffix: &str) -> String {
    let bytes = crate::identity::random_bytes::<8>();
    let middle: String = bytes
        .iter()
        .map(|b| TEMP_CHARS[*b as usize % TEMP_CHARS.len()] as char)
        .collect();
    format!("{prefix}{middle}{suffix}")
}

/// Create a new 0600 file in `dir` named `prefix`, eight random characters
/// from `[a-z0-9_]`, and `suffix` (exclusive create, no link followed,
/// retrying on a name clash); returns the open file and its path.
pub fn mkstemp(dir: &str, prefix: &str, suffix: &str) -> Result<(File, String)> {
    for _ in 0..10000 {
        let path = fsutil::join(dir, &random_name(prefix, suffix));
        let c =
            std::ffi::CString::new(path.clone()).map_err(|_| Error::value("embedded null byte"))?;
        // SAFETY: open(2) with a valid NUL-terminated path.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd >= 0 {
            // SAFETY: fd was just opened and is owned here.
            return Ok((unsafe { File::from_raw_fd(fd) }, path));
        }
        let code = Error::last_os();
        if code == libc::EEXIST {
            continue;
        }
        return Err(Error::errno_path(code, std::path::Path::new(&path)));
    }
    Err(Error::os("[Errno 17] No usable temporary file name found"))
}

/// Write `data` to a fresh temporary file beside `target`, fsync it, and
/// replace `target` with it; the temporary file never outlives the call.
pub fn atomic_write(target: &str, prefix: &str, suffix: &str, data: &[u8]) -> Result<()> {
    let dir = fsutil::parent(target);
    let (mut file, temporary) = mkstemp(&dir, prefix, suffix)?;
    let result = (|| {
        file.write_all(data)
            .map_err(|exc| Error::from_io(&exc, None))?;
        file.flush().map_err(|exc| Error::from_io(&exc, None))?;
        file.sync_all().map_err(|exc| Error::from_io(&exc, None))?;
        drop(file);
        fsutil::replace(&temporary, target)
    })();
    if fsutil::lstat(&temporary).is_ok() {
        let _ = fsutil::unlink(&temporary);
    }
    result
}

/// An exclusive `flock` held until dropped (closing the descriptor).
pub struct FileLock {
    _fd: OwnedFd,
}

/// Open (creating 0600, never following a link) and lock `path` exclusively,
/// blocking until the lock is free.
pub fn lock_exclusive(path: &str) -> Result<FileLock> {
    let fd = open_lock_file(path)?;
    loop {
        // SAFETY: flock on an owned descriptor.
        if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(FileLock { _fd: fd });
        }
        let code = Error::last_os();
        if code != libc::EINTR {
            return Err(Error::errno(code));
        }
    }
}

/// Open `path` read-write, creating it 0600 and never following a link.
pub fn open_lock_file(path: &str) -> Result<OwnedFd> {
    let c = std::ffi::CString::new(path).map_err(|_| Error::value("embedded null byte"))?;
    // SAFETY: open(2) with a valid NUL-terminated path.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(Error::errno_path(
            Error::last_os(),
            std::path::Path::new(path),
        ));
    }
    // SAFETY: fd was just opened and is owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Lexical POSIX path normalization: drop empty and `.` components, fold
/// `name/..`, keep leading `..` of a relative path, drop `..` at the root,
/// keep exactly two leading slashes (three or more become one), and turn
/// an empty result into `.`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// The normalized absolute path, relative paths taken from the current
/// directory (no links resolved).
pub fn abspath(path: &str) -> Result<String> {
    if path.starts_with('/') {
        Ok(normpath(path))
    } else {
        Ok(normpath(&format!("{}/{}", fsutil::cwd()?, path)))
    }
}

/// Join `name` onto `base`; an absolute `name` replaces `base`.
/// (`fsutil::join` always appends, as the helper's relative joins need.)
pub fn path_join(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        fsutil::normalize(name)
    } else {
        fsutil::join(base, name)
    }
}

/// The last component of a path, or empty for `/`, `//`, and `.`.
pub fn name(path: &str) -> String {
    let normal = fsutil::normalize(path);
    if normal == "/" || normal == "//" || normal == "." {
        return String::new();
    }
    normal.rsplit('/').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normpath_name_and_join() {
        assert_eq!(normpath("/a/./b/../c//d/"), "/a/c/d");
        assert_eq!(normpath("//a/b"), "//a/b");
        assert_eq!(normpath("///a/../.."), "/");
        assert_eq!(normpath("a/../../b"), "../b");
        assert_eq!(normpath(""), ".");
        assert_eq!(name("/a/b/"), "b");
        assert_eq!(name("/"), "");
        assert_eq!(path_join("/a/b", "/c/d"), "/c/d");
        assert_eq!(path_join("/a/b", "c//d"), "/a/b/c/d");
    }
}
