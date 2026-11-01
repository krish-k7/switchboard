//! File-system operations: lexical path normalization, non-strict symlink
//! resolution, and error messages that name the file
//! (`[Errno N] <strerror>: 'path'`).

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::wirejson;

// `mode_t` is 16 bits on macOS and 32 on Linux.
#[allow(clippy::unnecessary_cast)]
pub const S_IFMT: u32 = libc::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
pub const S_IFDIR: u32 = libc::S_IFDIR as u32;
#[allow(clippy::unnecessary_cast)]
pub const S_IFREG: u32 = libc::S_IFREG as u32;
#[allow(clippy::unnecessary_cast)]
pub const S_IFLNK: u32 = libc::S_IFLNK as u32;
#[allow(clippy::unnecessary_cast)]
pub const S_IFSOCK: u32 = libc::S_IFSOCK as u32;
#[allow(clippy::unnecessary_cast)]
pub const S_ISVTX: u32 = libc::S_ISVTX as u32;

pub fn is_type(meta: &Metadata, kind: u32) -> bool {
    meta.mode() & S_IFMT == kind
}

pub fn getuid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// Lexical cleanup without resolving `..`: drop empty and `.` components,
/// keep exactly two leading slashes (three or more become one), and turn an
/// empty result into `.`.
pub fn normalize(value: &str) -> String {
    if value.is_empty() {
        return ".".into();
    }
    let root = if value.starts_with("//") && !value.starts_with("///") {
        "//"
    } else if value.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = value
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = format!("{}{}", root, parts.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// Append `name` to `base` and normalize (an absolute `name` is appended too).
pub fn join(base: &str, name: &str) -> String {
    normalize(&format!("{base}/{name}"))
}

/// The normalized path without its last component (`.` for a bare name, the
/// root for a top-level path).
pub fn parent(value: &str) -> String {
    let value = normalize(value);
    match value.rfind('/') {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(1) if value.starts_with("//") => "//".into(),
        Some(index) => value[..index].to_string(),
    }
}

/// Whether the path starts with `/`.
pub fn is_absolute(value: &str) -> bool {
    value.starts_with('/')
}

/// Whether any component of the normalized path is `..`.
pub fn has_parent_part(value: &str) -> bool {
    normalize(value).split('/').any(|part| part == "..")
}

fn checked(value: &str) -> Result<&Path> {
    if value.contains('\0') {
        return Err(Error::value("embedded null byte"));
    }
    Ok(Path::new(value))
}

pub fn cwd() -> Result<String> {
    let path = std::env::current_dir().map_err(|exc| Error::from_io(&exc, None))?;
    Ok(path.to_string_lossy().into_owned())
}

/// The normalized path made absolute against the current directory, without
/// resolving links or `..`.
pub fn absolute(value: &str) -> Result<String> {
    let value = normalize(value);
    if is_absolute(&value) {
        Ok(value)
    } else if value == "." {
        cwd()
    } else {
        Ok(join(&cwd()?, &value))
    }
}

pub fn lstat(value: &str) -> Result<Metadata> {
    let path = checked(value)?;
    fs::symlink_metadata(path).map_err(|exc| Error::from_io(&exc, Some(path)))
}

pub fn stat(value: &str) -> Result<Metadata> {
    let path = checked(value)?;
    fs::metadata(path).map_err(|exc| Error::from_io(&exc, Some(path)))
}

/// Whether the path exists, following links; errors read as absent.
pub fn exists(value: &str) -> bool {
    stat(value).is_ok()
}

/// Whether the path is a symbolic link; errors read as not a link.
pub fn is_symlink(value: &str) -> bool {
    lstat(value).is_ok_and(|meta| is_type(&meta, S_IFLNK))
}

pub fn readlink(value: &str) -> Result<String> {
    let path = checked(value)?;
    fs::read_link(path)
        .map(|target| target.to_string_lossy().into_owned())
        .map_err(|exc| Error::from_io(&exc, Some(path)))
}

pub fn read_text(value: &str) -> Result<String> {
    let path = checked(value)?;
    let bytes = fs::read(path).map_err(|exc| Error::from_io(&exc, Some(path)))?;
    String::from_utf8(bytes).map_err(|_| Error::value("'utf-8' codec can't decode file contents"))
}

pub fn unlink(value: &str) -> Result<()> {
    let path = checked(value)?;
    fs::remove_file(path).map_err(|exc| Error::from_io(&exc, Some(path)))
}

/// Rename `source` over `target` atomically; an error names both paths.
pub fn replace(source: &str, target: &str) -> Result<()> {
    let (from, to) = (checked(source)?, checked(target)?);
    fs::rename(from, to).map_err(|exc| match exc.raw_os_error() {
        Some(code) => Error::errno_paths(code, from, to),
        None => Error::os(exc.to_string()),
    })
}

/// Non-strict realpath: resolve symbolic links component by component from
/// the root (relative paths from the current directory), folding `..` as it
/// goes. A component that is missing or can't be read is kept as written,
/// and a link loop is left unresolved rather than an error.
pub fn realpath(value: &str) -> Result<String> {
    checked(value)?;
    let mut rest: Vec<Option<String>> = value
        .split('/')
        .rev()
        .map(|s| Some(s.to_string()))
        .collect();
    let mut part_count = rest.len();
    let mut path = if value.starts_with('/') {
        "/".to_string()
    } else {
        cwd()?
    };
    let mut seen: std::collections::HashMap<String, Option<String>> = Default::default();
    while part_count > 0 {
        let name = match rest.pop().expect("part") {
            None => {
                let link = rest.pop().flatten().expect("link marker");
                seen.insert(link, Some(path.clone()));
                continue;
            }
            Some(name) => name,
        };
        part_count -= 1;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            path = match path.rfind('/') {
                Some(0) | None => "/".into(),
                Some(index) => path[..index].to_string(),
            };
            continue;
        }
        let newpath = if path == "/" {
            format!("/{name}")
        } else {
            format!("{path}/{name}")
        };
        let meta = match fs::symlink_metadata(&newpath) {
            Ok(meta) => meta,
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        if !is_type(&meta, S_IFLNK) {
            path = newpath;
            continue;
        }
        if let Some(resolved) = seen.get(&newpath) {
            match resolved {
                Some(resolved) => path = resolved.clone(),
                None => path = newpath,
            }
            continue;
        }
        let target = match fs::read_link(&newpath) {
            Ok(target) => target.to_string_lossy().into_owned(),
            Err(_) => {
                path = newpath;
                continue;
            }
        };
        if target.starts_with('/') {
            path = "/".into();
        }
        seen.insert(newpath.clone(), None);
        rest.push(Some(newpath));
        rest.push(None);
        let parts: Vec<&str> = target.split('/').collect();
        part_count += parts.len();
        rest.extend(parts.into_iter().rev().map(|s| Some(s.to_string())));
    }
    Ok(path)
}

/// [`realpath`] of the normalized path.
pub fn resolve(value: &str) -> Result<String> {
    realpath(&normalize(value))
}

/// Create a directory and any missing parents; an existing directory is fine.
/// The directory gets `mode` and created parents 0o777 (both less the umask).
pub fn mkdir_parents(value: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let path = checked(value)?;
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => Ok(()),
        Err(exc) if exc.raw_os_error() == Some(libc::ENOENT) => {
            let up = parent(value);
            if up == normalize(value) {
                return Err(Error::from_io(&exc, Some(path)));
            }
            mkdir_parents(&up, 0o777)?;
            match fs::DirBuilder::new().mode(mode).create(path) {
                Ok(()) => Ok(()),
                Err(exc) if fs::metadata(path).is_ok_and(|m| m.is_dir()) => {
                    let _ = exc;
                    Ok(())
                }
                Err(exc) => Err(Error::from_io(&exc, Some(path))),
            }
        }
        Err(exc) => {
            if fs::metadata(path).is_ok_and(|m| m.is_dir()) {
                Ok(())
            } else {
                Err(Error::from_io(&exc, Some(path)))
            }
        }
    }
}

/// A private directory: owned by this user, mode 0700, not a symlink.
pub fn private_dir(value: &str, create: bool) -> Result<String> {
    let path = absolute(value)?;
    if create {
        mkdir_parents(&path, 0o700)?;
    }
    let info = lstat(&path)?;
    if !is_type(&info, S_IFDIR) || info.uid() != getuid() || info.mode() & 0o077 != 0 {
        return Err(Error::value("directory must be owned and mode 0700"));
    }
    resolve(&path)
}

/// Native Claude's existing readable config root: owned, not a symlink, and
/// not group- or world-writable. Its sessions directory stays private.
pub fn owned_config_dir(value: &str) -> Result<String> {
    let path = absolute(value)?;
    let info = lstat(&path)?;
    if !is_type(&info, S_IFDIR) || info.uid() != getuid() || info.mode() & 0o022 != 0 {
        return Err(Error::value(
            "config directory must be owned, not symlinked or shared-writable",
        ));
    }
    resolve(&path)
}

/// Read a small JSON object from an owned regular file without following links.
pub fn read_json(value: &str, limit: usize, private: bool) -> Result<Map<String, Value>> {
    let path = checked(value)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|exc| Error::from_io(&exc, Some(path)))?;
    let info = file.metadata().map_err(|exc| Error::from_io(&exc, None))?;
    let forbidden = if private { 0o077 } else { 0o022 };
    if !is_type(&info, S_IFREG) || info.uid() != getuid() || info.mode() & forbidden != 0 {
        return Err(Error::value("unsafe JSON file owner/type/mode"));
    }
    let mut raw = Vec::new();
    (&file)
        .take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|exc| Error::from_io(&exc, None))?;
    if raw.len() > limit {
        return Err(Error::value("JSON file too large"));
    }
    wirejson::object(wirejson::loads(&raw)?, "JSON object required")
}

/// Create a new 0600 file (never replacing one) holding `content`
/// as wire-format JSON ([`wirejson::dumps`]) and a newline; returns its inode.
pub fn write_new(value: &str, content: &Map<String, Value>) -> Result<u64> {
    let path = checked(value)?;
    let mut file: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map_err(|exc| Error::from_io(&exc, Some(path)))?;
    let mut text = wirejson::dumps(&Value::Object(content.clone()));
    text.push('\n');
    file.write_all(text.as_bytes())
        .map_err(|exc| Error::from_io(&exc, None))?;
    drop(file);
    Ok(lstat(value)?.ino())
}

pub fn path_buf(value: &str) -> PathBuf {
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_parent() {
        assert_eq!(normalize("/a//b/./c/"), "/a/b/c");
        assert_eq!(normalize("//a"), "//a");
        assert_eq!(normalize("///a"), "/a");
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("/a/../b"), "/a/../b");
        assert_eq!(parent("/a/b.sock"), "/a");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("a"), ".");
        assert!(has_parent_part("/a/../b"));
    }

    #[test]
    fn realpath_resolves_missing_tail() {
        let resolved = realpath("/tmp/../tmp/switchboard-missing-xyz/q").unwrap();
        let tmp = realpath("/tmp").unwrap();
        assert_eq!(resolved, format!("{tmp}/switchboard-missing-xyz/q"));
    }
}
