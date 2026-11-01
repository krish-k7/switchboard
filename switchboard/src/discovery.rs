//! Native Claude session discovery: registry records, process identity, and
//! the per-session auth keys native peers publish.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

use indexmap::IndexMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::fsutil;
use crate::identity;
use crate::protocol::valid_uuid;
use crate::wirejson::{str_value, truthy, value_eq, value_eq_opt};

pub type Record = Map<String, Value>;
pub type Selection = HashSet<String>;

pub const RECORD_LIMIT: usize = 262144;

/// The type name of a decoded value in type-error details (`NoneType`,
/// `bool`, `int`, `float`, `str`, `list`, or `dict`), as clients already see
/// them.
pub fn type_name(value: Option<&Value>) -> &'static str {
    match value {
        None | Some(Value::Null) => "NoneType",
        Some(Value::Bool(_)) => "bool",
        Some(Value::Number(number)) => match crate::wirejson::num_kind(number) {
            crate::wirejson::NumKind::Int(_) => "int",
            crate::wirejson::NumKind::Float(_) => "float",
        },
        Some(Value::String(_)) => "str",
        Some(Value::Array(_)) => "list",
        Some(Value::Object(_)) => "dict",
    }
}

/// A decoded value as a normalized path; anything but a string is a type
/// error.
pub fn path_value(value: Option<&Value>) -> Result<String> {
    match value {
        Some(Value::String(value)) => Ok(fsutil::normalize(value)),
        other => Err(Error::type_error(format!(
            "argument should be a str or an os.PathLike object where __fspath__ returns a str, not {}",
            crate::text::repr_str(type_name(other))
        ))),
    }
}

/// The record's socket path, resolved ([`fsutil::resolve`]).
pub fn socket_path(record: &Record) -> Result<String> {
    fsutil::resolve(&path_value(record.get("messagingSocketPath"))?)
}

#[cfg(test)]
thread_local! {
    /// Full record checks run on this thread (tests count them).
    static CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read and verify one genuine, selected, live native session record.
pub fn session_record(config_dir: &str, pid: &Value, selected: &Selection) -> Result<Record> {
    #[cfg(test)]
    CHECKS.with(|count| count.set(count.get() + 1));
    let file = fsutil::join(config_dir, &format!("sessions/{}.json", str_value(pid)));
    let record = fsutil::read_json(&file, RECORD_LIMIT, false)?;
    if truthy(record.get("peerBridgeProxy"))
        || truthy(record.get("bridgeProxy"))
        || !value_eq_opt(record.get("pid"), Some(pid))
    {
        return Err(Error::value("not a real registered session"));
    }
    let sid = valid_uuid(record.get("sessionId"))?;
    if !selected.contains(&sid) {
        return Err(Error::not_joined("session not enrolled"));
    }
    if !value_eq_opt(record.get("peerProtocol"), Some(&Value::from(1))) {
        return Err(Error::value("unsupported native protocol"));
    }
    if !identity::domain_is_local(record.get("pidDomain"))?
        || !value_eq_opt(
            record.get("procStart"),
            Some(&Value::String(identity::process_start_value(Some(pid))?)),
        )
    {
        return Err(Error::value("native process identity mismatch"));
    }
    let default = Value::String(String::new());
    let path = path_value(Some(record.get("messagingSocketPath").unwrap_or(&default)))?;
    if !fsutil::is_absolute(&path) || fsutil::is_symlink(&path) {
        return Err(Error::value("unsafe native socket path"));
    }
    fsutil::private_dir(&fsutil::parent(&path), false)?;
    let info = fsutil::stat(&path)?;
    if !fsutil::is_type(&info, fsutil::S_IFSOCK) || info.uid() != fsutil::getuid() {
        return Err(Error::value("unsafe native socket"));
    }
    Ok(record)
}

fn pid_value(digits: &str) -> Value {
    serde_json::from_str(digits).expect("decimal digits")
}

/// A record file's identity and change markers: any write or replacement
/// changes at least one of them.
type FileKey = (u64, u64, u64, i64, i64, i64, i64, u32);

/// Record path -> (file key when read, the session UUID it claimed).
type Claims = HashMap<String, (FileKey, Option<String>)>;

fn file_key(info: &std::fs::Metadata) -> FileKey {
    (
        info.dev(),
        info.ino(),
        info.size(),
        info.mtime(),
        info.mtime_nsec(),
        info.ctime(),
        info.ctime_nsec(),
        info.mode(),
    )
}

/// What each record file's content claims, by path: the session UUID of a
/// record that is not a proxy and names its own PID, else `None`. This only
/// decides which files cannot contribute to a lookup; every file that can is
/// still fully checked (`session_record`: kernel PID and start time, socket
/// owner and type, selection) each time.
static CLAIMS: Mutex<Option<Claims>> = Mutex::new(None);
const CLAIMS_LIMIT: usize = 4096;

/// The session UUID a record file's content claims, read only when the file
/// changed. `Err(())` when reading it failed in a way `discover` does not
/// skip, so the caller runs the full check and reports that error as before.
fn claim(path: &str, pid: &Value) -> std::result::Result<Option<String>, ()> {
    let Ok(info) = std::fs::symlink_metadata(path) else {
        return Ok(None); // Gone: the full check would fail with an OS error.
    };
    let key = file_key(&info);
    let mut cache = CLAIMS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((cached, sid)) = cache.as_ref().and_then(|c| c.get(path)) {
        if *cached == key {
            return Ok(sid.clone());
        }
    }
    let sid = match fsutil::read_json(path, RECORD_LIMIT, false) {
        Ok(record) => {
            let genuine = !truthy(record.get("peerBridgeProxy"))
                && !truthy(record.get("bridgeProxy"))
                && value_eq_opt(record.get("pid"), Some(pid));
            genuine
                .then(|| valid_uuid(record.get("sessionId")).ok())
                .flatten()
        }
        Err(exc) if exc.is_os_value_subprocess() => None,
        Err(_) => return Err(()),
    };
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() >= CLAIMS_LIMIT {
        cache.clear();
    }
    cache.insert(path.to_string(), (key, sid.clone()));
    Ok(sid)
}

/// The registry's record files, as `(pid, path)`; the path is the one
/// `session_record` reads for that PID.
fn record_files(config_dir: &str) -> Vec<(Value, String)> {
    let sessions = fsutil::join(config_dir, "sessions");
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let mut bytes = stem.bytes();
        if !matches!(bytes.next(), Some(b'1'..=b'9')) || !bytes.all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid = pid_value(stem);
        let path = fsutil::join(config_dir, &format!("sessions/{}.json", str_value(&pid)));
        files.push((pid, path));
    }
    files
}

/// Every selected live session with exactly one record, keyed by session
/// UUID; `only` limits it to one UUID. Files whose content claims a UUID
/// that is not wanted are skipped unread while they are unchanged; they
/// could only have failed the full check, or counted for another UUID.
fn discover_wanted(
    config_dir: &str,
    selected: &Selection,
    only: Option<&str>,
) -> Result<IndexMap<String, Record>> {
    let mut found: IndexMap<String, Record> = IndexMap::new();
    let mut duplicates: HashSet<String> = HashSet::new();
    for (pid, path) in record_files(config_dir) {
        match claim(&path, &pid) {
            Ok(None) => continue,
            Ok(Some(sid)) if !selected.contains(&sid) || only.is_some_and(|o| o != sid) => continue,
            _ => {}
        }
        let record = match session_record(config_dir, &pid, selected) {
            Ok(record) => record,
            Err(exc) if exc.is_os_value_subprocess() => continue,
            Err(exc) => return Err(exc),
        };
        let sid = record["sessionId"].as_str().expect("validated").to_string();
        if only.is_some_and(|o| o != sid) {
            continue;
        }
        if found.contains_key(&sid) {
            duplicates.insert(sid.clone());
        }
        found.insert(sid, record);
    }
    found.retain(|sid, _| !duplicates.contains(sid));
    Ok(found)
}

/// Every selected live session with exactly one record, keyed by session UUID.
pub fn discover(config_dir: &str, selected: &Selection) -> Result<IndexMap<String, Record>> {
    discover_wanted(config_dir, selected, None)
}

/// `discover(config_dir, selected).shift_remove(sid)`, fully checking only
/// the records that claim `sid`.
pub fn discover_one(config_dir: &str, selected: &Selection, sid: &str) -> Result<Option<Record>> {
    Ok(discover_wanted(config_dir, selected, Some(sid))?.shift_remove(sid))
}

/// SHA-256 hex digest of the socket path, as native key file names use.
pub fn socket_digest(path: &str) -> String {
    Sha256::digest(path.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The native auth token a session published for its socket.
pub fn auth_key(config_dir: &str, record: &Record) -> Result<String> {
    let socket = record
        .get("messagingSocketPath")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::type_error("messagingSocketPath must be a str"))?;
    let pid = str_value(record.get("pid").unwrap_or(&Value::Null));
    let file = fsutil::join(
        config_dir,
        &format!("sessions/{pid}.{}.key", socket_digest(socket)),
    );
    let key = fsutil::read_json(&file, 4096, true)?;
    let token = key.get("peerToken");
    let token_ok = matches!(token, Some(Value::String(t))
        if t.len() == 32 && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    if !value_eq_opt(key.get("procStart"), record.get("procStart"))
        || !value_eq_opt(key.get("pidDomain"), record.get("pidDomain"))
        || !token_ok
    {
        return Err(Error::value("native auth key identity mismatch"));
    }
    Ok(token.and_then(Value::as_str).expect("checked").to_string())
}

/// Whether the records agree on every key ([`value_eq`]; absent equals null).
pub fn same_identity(a: &Record, b: &Record, keys: &[&str]) -> bool {
    keys.iter().all(|key| {
        value_eq(
            a.get(*key).unwrap_or(&Value::Null),
            b.get(*key).unwrap_or(&Value::Null),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn checks() -> usize {
        CHECKS.with(|count| count.get())
    }

    /// A registry with two live selected sessions (this process and a child),
    /// an unselected one, and a proxy record.
    struct Registry {
        root: String,
        config: String,
        child: std::process::Child,
        _sockets: Vec<UnixListener>,
    }

    impl Registry {
        fn new() -> Registry {
            let template = std::ffi::CString::new("/tmp/sbd-XXXXXX").unwrap();
            let mut raw = template.into_bytes_with_nul();
            // SAFETY: raw is a writable NUL-terminated template.
            assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
            raw.pop();
            let root = std::fs::canonicalize(String::from_utf8(raw).unwrap())
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let config = format!("{root}/config");
            for dir in [
                config.clone(),
                format!("{config}/sessions"),
                format!("{root}/socks"),
            ] {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let child = std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap();
            Registry {
                root,
                config,
                child,
                _sockets: Vec::new(),
            }
        }

        fn write(&mut self, pid: i64, sid: &str, extra: &[(&str, Value)]) {
            let socket = format!("{}/socks/{pid}.sock", self.root);
            if !std::path::Path::new(&socket).exists() {
                self._sockets.push(UnixListener::bind(&socket).unwrap());
            }
            let mut record = Map::new();
            record.insert("pid".into(), Value::from(pid));
            record.insert("sessionId".into(), Value::from(sid));
            record.insert(
                "procStart".into(),
                Value::from(identity::process_start(pid).unwrap_or_default()),
            );
            record.insert(
                "pidDomain".into(),
                Value::from(identity::local_domain().unwrap()),
            );
            record.insert("peerProtocol".into(), Value::from(1));
            record.insert("messagingSocketPath".into(), Value::from(socket));
            for (key, value) in extra {
                record.insert((*key).into(), value.clone());
            }
            let path = format!("{}/sessions/{pid}.json", self.config);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    impl Drop for Registry {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn one_session_lookup_checks_only_records_that_claim_it() {
        let mut r = Registry::new();
        let (own, other, unselected) = (
            crate::identity::uuid4(),
            crate::identity::uuid4(),
            crate::identity::uuid4(),
        );
        let me = std::process::id() as i64;
        let child = r.child.id() as i64;
        r.write(me, &own, &[]);
        r.write(child, &other, &[]);
        r.write(999_999_901, &unselected, &[]);
        r.write(
            999_999_902,
            &crate::identity::uuid4(),
            &[("peerBridgeProxy", Value::Bool(true))],
        );
        let selected: Selection = [own.clone(), other.clone()].into_iter().collect();
        let all = discover(&r.config, &selected).unwrap();
        assert_eq!(all.keys().cloned().collect::<HashSet<_>>(), selected);
        for sid in [&own, &other, &unselected] {
            let before = checks();
            let found = discover_one(&r.config, &selected, sid).unwrap();
            assert_eq!(found, all.get(sid.as_str()).cloned());
            // Only the record claiming it is checked, and checked in full.
            assert_eq!(checks() - before, usize::from(sid != &unselected));
        }
        // Another live record claiming the same session makes it ambiguous.
        r.write(child, &own, &[]);
        assert_eq!(discover_one(&r.config, &selected, &own).unwrap(), None);
        assert!(!discover(&r.config, &selected).unwrap().contains_key(&own));
        // Kernel identity is checked every time, never cached.
        r.write(child, &other, &[]);
        assert!(discover_one(&r.config, &selected, &other)
            .unwrap()
            .is_some());
        let _ = r.child.kill();
        let _ = r.child.wait();
        assert_eq!(discover_one(&r.config, &selected, &other).unwrap(), None);
        // So is the selection.
        let narrowed: Selection = [own.clone()].into_iter().collect();
        assert_eq!(discover_one(&r.config, &narrowed, &other).unwrap(), None);
        assert!(discover_one(&r.config, &narrowed, &own).unwrap().is_some());
    }

    #[test]
    fn digest_matches_known_vector() {
        assert_eq!(
            socket_digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn path_type_errors_name_the_value_type() {
        assert_eq!(
            path_value(Some(&Value::from(5))).unwrap_err().message,
            "argument should be a str or an os.PathLike object where __fspath__ returns a str, not 'int'"
        );
    }
}
