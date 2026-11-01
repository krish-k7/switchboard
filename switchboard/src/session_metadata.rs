//! Bounded display metadata from a verified native session's local cwd:
//! project, repository, and branch. Never invokes Git, reads configuration
//! or remotes, or exports absolute paths.

use std::os::unix::fs::MetadataExt;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::fileops::{abspath, name};
use crate::fsutil;
use crate::text;

const MAX_CACHE: usize = 128;
const MAX_DEPTH: usize = 64;
const MAX_FILE: usize = 4096;

type Fingerprint = Option<(u64, u64, u32, u64, i128, i128)>;

/// Ordered optional `project`, `repository`, `branch` strings.
pub type Metadata = IndexMap<String, String>;

/// The per-process cache of recent lookups, keyed by the raw cwd string.
#[derive(Default)]
pub struct MetadataCache {
    entries: IndexMap<String, (Vec<(String, Fingerprint)>, Metadata)>,
}

fn fingerprint(path: &str) -> Fingerprint {
    let info = fsutil::lstat(path).ok()?;
    Some((
        info.dev(),
        info.ino(),
        info.mode(),
        info.size(),
        info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128,
        info.ctime() as i128 * 1_000_000_000 + info.ctime_nsec() as i128,
    ))
}

fn is_dir(print: &Fingerprint) -> bool {
    print.is_some_and(|p| p.2 & fsutil::S_IFMT == fsutil::S_IFDIR)
}

fn is_reg(print: &Fingerprint) -> bool {
    print.is_some_and(|p| p.2 & fsutil::S_IFMT == fsutil::S_IFREG)
}

fn label(value: &str, maximum: usize) -> bool {
    let length = text::char_len(value);
    length > 0 && length <= maximum && !value.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
}

/// `(path, *path.parents)` for an absolute normalized path.
fn ancestry(path: &str) -> Vec<String> {
    let mut out = vec![path.to_string()];
    let mut current = path.to_string();
    loop {
        let up = fsutil::parent(&current);
        if up == current {
            break;
        }
        out.push(up.clone());
        current = up;
    }
    out
}

fn parts_count(path: &str) -> usize {
    let normal = fsutil::normalize(path);
    let root = usize::from(normal.starts_with('/'));
    root + normal.split('/').filter(|p| !p.is_empty()).count()
}

struct Tracker {
    tracked: Vec<(String, Fingerprint)>,
}

impl Tracker {
    fn track(&mut self, path: &str) -> Fingerprint {
        let print = fingerprint(path);
        if let Some(entry) = self.tracked.iter_mut().find(|(p, _)| p == path) {
            entry.1 = print;
        } else {
            self.tracked.push((path.to_string(), print));
        }
        print
    }

    fn directory(&mut self, path: &str) -> bool {
        if parts_count(path) > MAX_DEPTH {
            return false;
        }
        for component in ancestry(path) {
            let print = self.track(&component);
            if !is_dir(&print) {
                return false;
            }
        }
        true
    }

    /// `_read`: a small regular file, refusing links, devices, and races.
    fn read(&mut self, path: &str) -> Result<String, ()> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let expected = self.track(path);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| ())?;
        let info = file.metadata().map_err(|_| ())?;
        let actual = Some((
            info.dev(),
            info.ino(),
            info.mode(),
            info.size(),
            info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128,
            info.ctime() as i128 * 1_000_000_000 + info.ctime_nsec() as i128,
        ));
        if actual != expected || !is_reg(&actual) || info.size() as usize > MAX_FILE {
            return Err(());
        }
        let mut data = Vec::new();
        (&file)
            .take(MAX_FILE as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|_| ())?;
        if data.len() > MAX_FILE {
            return Err(());
        }
        let text = String::from_utf8(data).map_err(|_| ())?;
        Ok(text.strip_suffix('\n').unwrap_or(&text).to_string())
    }
}

fn valid_branch(branch: &str) -> bool {
    if !label(branch, 256) || branch.starts_with(['/', '-']) {
        return false;
    }
    let forbidden = branch
        .chars()
        .any(|c| text::is_space(c) || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'));
    if forbidden || branch.contains("..") || branch.contains("@{") || branch.contains("//") {
        return false;
    }
    if branch.ends_with(['/', '.']) {
        return false;
    }
    branch
        .split('/')
        .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

impl MetadataCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `metadata_for_session(record)`: optional project, repository, and
    /// branch; never absolute paths.
    pub fn for_session(&mut self, record: &Map<String, Value>) -> Metadata {
        let cwd = match record.get("cwd") {
            Some(Value::String(cwd)) => cwd.clone(),
            _ => return Metadata::new(),
        };
        if cwd.is_empty() || text::char_len(&cwd) > 4096 || cwd.contains('\0') {
            return Metadata::new();
        }
        let path = fsutil::normalize(&cwd);
        if !fsutil::is_absolute(&path) {
            return Metadata::new();
        }
        if let Some((tracked, result)) = self.entries.get(&cwd) {
            if tracked.iter().all(|(p, print)| fingerprint(p) == *print) {
                let result = result.clone();
                let entry = self.entries.shift_remove(&cwd).expect("cached");
                self.entries.insert(cwd, entry);
                return result;
            }
        }
        let mut tracker = Tracker {
            tracked: Vec::new(),
        };
        let mut result = Metadata::new();
        Self::inspect(&path, &mut tracker, &mut result);
        self.entries.shift_remove(&cwd);
        self.entries.insert(cwd, (tracker.tracked, result.clone()));
        while self.entries.len() > MAX_CACHE {
            self.entries.shift_remove_index(0);
        }
        result
    }

    fn inspect(path: &str, tracker: &mut Tracker, result: &mut Metadata) {
        if !tracker.directory(path) {
            return;
        }
        let project = name(path);
        if label(&project, 64) {
            result.insert("project".into(), project);
        }
        for (depth, root) in ancestry(path).into_iter().enumerate() {
            if depth >= MAX_DEPTH {
                break;
            }
            let marker = fsutil::join(&root, ".git");
            let print = tracker.track(&marker);
            if print.is_none() {
                continue;
            }
            let gitdir = if is_dir(&print) {
                marker
            } else if is_reg(&print) {
                let Ok(pointer) = tracker.read(&marker) else {
                    return;
                };
                if !pointer.starts_with("gitdir: ") || pointer.contains('\n') {
                    break;
                }
                let target = &pointer[8..];
                let joined = if target.starts_with('/') {
                    target.to_string()
                } else {
                    format!("{root}/{target}")
                };
                let Ok(gitdir) = abspath(&joined) else { return };
                if !tracker.directory(&gitdir) {
                    break;
                }
                gitdir
            } else {
                break;
            };
            let mut repository = name(&root);
            let common = fsutil::join(&gitdir, "commondir");
            if tracker.track(&common).is_some() {
                let Ok(pointer) = tracker.read(&common) else {
                    return;
                };
                let joined = if pointer.starts_with('/') {
                    pointer
                } else {
                    format!("{gitdir}/{pointer}")
                };
                let Ok(commonpath) = abspath(&joined) else {
                    return;
                };
                if !tracker.directory(&commonpath) {
                    break;
                }
                if name(&commonpath) == ".git" {
                    repository = name(&fsutil::parent(&commonpath));
                }
            }
            if label(&repository, 256) {
                result.insert("repository".into(), repository);
            }
            let Ok(head) = tracker.read(&fsutil::join(&gitdir, "HEAD")) else {
                return;
            };
            if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
                if valid_branch(branch) {
                    result.insert("branch".into(), branch.to_string());
                }
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_rules_match_git_ref_checks() {
        assert!(valid_branch("main"));
        assert!(valid_branch("feature/x-1"));
        for bad in [
            "-x", "/x", "a..b", "a b", "a@{b", "a//b", "x/", "x.", ".x", "a/.b", "x.lock", "a~b",
        ] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }

    #[test]
    fn reads_project_repository_and_branch() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!("sbm-{}", crate::identity::uuid4_hex()));
        let work = root.join("example/packages/solver");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(root.join("example/.git")).unwrap();
        std::fs::write(root.join("example/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let mut record = Map::new();
        record.insert(
            "cwd".into(),
            Value::from(work.to_string_lossy().into_owned()),
        );
        let mut cache = MetadataCache::new();
        let found = cache.for_session(&record);
        assert_eq!(
            found
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            [
                ("project", "solver"),
                ("repository", "example"),
                ("branch", "main")
            ]
        );
        assert_eq!(cache.for_session(&record), found);
        std::fs::write(root.join("example/.git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        assert_eq!(cache.for_session(&record)["branch"], "dev");
        let _ = std::fs::remove_dir_all(&root);
    }
}
