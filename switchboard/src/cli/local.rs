//! Local reads and writes the commands share: the bridge config, the
//! bridge's runtime file, native sessions, joining and leaving, and the
//! bounded safe-file checks `doctor` and the machine commands use.

use std::os::unix::fs::MetadataExt;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::discovery::{auth_key, path_value, session_record, Record};
use crate::error::{Error, Result};
use crate::fsutil;
use crate::membership;
use crate::protocol::{unquote, valid_uuid_str};
use crate::text::repr_str;
use crate::wirejson::{str_value, truthy, value_eq};

/// The home directory: `$HOME`, else the password database, with symbolic links
/// resolved. Every path the commands derive from it (the configuration, the
/// identity, the installation) is checked for links in its directories, so a
/// home directory reached through a link (such as one under `/tmp` on macOS,
/// where `/tmp` links to `/private/tmp`) is used by its real path throughout.
/// A home directory that does not exist (or is not absolute) is kept as given.
pub fn home_dir() -> String {
    let home = given_home();
    if !home.starts_with('/') {
        return home;
    }
    match std::fs::canonicalize(&home) {
        Ok(real) => real.into_os_string().into_string().unwrap_or(home),
        Err(_) => home,
    }
}

fn given_home() -> String {
    if let Ok(home) = std::env::var("HOME") {
        return fsutil::normalize(&home);
    }
    // SAFETY: getpwuid returns a static record or NULL.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_dir.is_null() {
            return std::ffi::CStr::from_ptr((*entry).pw_dir)
                .to_string_lossy()
                .into_owned();
        }
    }
    "/".into()
}

/// The bridge config as an object.
pub fn private_json(path: &str) -> Result<Map<String, Value>> {
    fsutil::read_json(path, crate::protocol::FRAME_LIMIT, true)
}

/// The config's `state_dir` as a path, relative to the config's resolved parent.
fn state_path(config: &Map<String, Value>, config_path: &str) -> Result<String> {
    let value = match config.get("state_dir") {
        None => return Err(Error::value("'state_dir'")),
        Some(value) => path_value(Some(value))?,
    };
    if fsutil::is_absolute(&value) {
        Ok(value)
    } else {
        Ok(fsutil::join(
            &fsutil::parent(&fsutil::resolve(config_path)?),
            &value,
        ))
    }
}

/// Whether a process exists: kill(2) with signal 0.
pub fn kill0(pid: i64) -> Result<()> {
    let Ok(pid) = i32::try_from(pid) else {
        return Err(Error::value("signed integer is greater than maximum"));
    };
    // SAFETY: signal 0 only checks for the process.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(());
    }
    Err(Error::errno(Error::last_os()))
}

/// The JSON value of `pid` when it is an integer (not a float or boolean);
/// integers too large for an i64 read as `i64::MAX`.
pub fn int_pid(value: Option<&Value>) -> Option<i64> {
    crate::wirejson::as_int(value?).and_then(|digits| digits.parse::<i64>().ok().or(Some(i64::MAX)))
}

/// The live bridge's runtime record.
pub fn runtime_status(
    config: &Map<String, Value>,
    config_path: &str,
) -> Result<Map<String, Value>> {
    let runtime_path = fsutil::join(&state_path(config, config_path)?, "runtime.json");
    if fsutil::lstat(&runtime_path)?.mode() & 0o7777 != 0o600 {
        return Err(Error::value("runtime file must have mode 0600"));
    }
    let runtime = private_json(&runtime_path)?;
    let pid = match int_pid(runtime.get("pid")) {
        Some(pid) if pid > 1 => pid,
        _ => return Err(Error::value("the runtime file has an invalid bridge PID")),
    };
    kill0(pid)?;
    if !truthy(runtime.get("procStart"))
        || !value_eq(
            &Value::String(crate::identity::process_start(pid)?),
            runtime.get("procStart").unwrap_or(&Value::Null),
        )
    {
        return Err(Error::value(
            "the runtime file is out of date (its bridge process has exited)",
        ));
    }
    Ok(runtime)
}

/// Genuine live native sessions with valid keys.
pub fn real_sessions(config: &Map<String, Value>) -> Result<Vec<Record>> {
    let config_dir = match config.get("config_dir") {
        None => format!("{}/.claude", home_dir()),
        Some(value) => path_value(Some(value))?,
    };
    let directory = fsutil::join(&config_dir, "sessions");
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(result);
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = fsutil::join(&directory, name);
        let Ok(info) = fsutil::lstat(&path) else {
            continue;
        };
        if !fsutil::is_type(&info, fsutil::S_IFREG) || info.uid() != fsutil::getuid() {
            continue;
        }
        let Ok(raw) = std::fs::read(&path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(raw) else {
            continue;
        };
        let Ok(Value::Object(record)) = crate::wirejson::loads(text.as_bytes()) else {
            continue;
        };
        if truthy(record.get("peerBridgeProxy")) {
            continue;
        }
        let Some(sid) = record.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let digits = stem.trim_start_matches('0');
        let pid: Value = match serde_json::from_str(if digits.is_empty() { "0" } else { digits }) {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        let selected = std::iter::once(sid.to_string()).collect();
        let Ok(record) = session_record(&fsutil::parent(&directory), &pid, &selected) else {
            continue;
        };
        if auth_key(&fsutil::parent(&directory), &record).is_err() {
            continue;
        }
        result.push(record);
    }
    Ok(result)
}

/// The config's `sessions` as a list to test membership in (absent is empty;
/// a string gives its characters and an object its keys; anything else fails).
fn selected_set(config: &Map<String, Value>) -> Result<Vec<Value>> {
    match config.get("sessions") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::String(value)) => {
            Ok(value.chars().map(|c| Value::from(c.to_string())).collect())
        }
        Some(Value::Object(map)) => Ok(map.keys().map(|k| Value::from(k.clone())).collect()),
        Some(other) => Err(Error::type_error(format!(
            "{} object is not iterable",
            repr_str(crate::discovery::type_name(Some(other)))
        ))),
    }
}

fn contains(set: &[Value], value: &Value) -> bool {
    set.iter().any(|item| value_eq(item, value))
}

/// One row per native session, for the chat list.
pub fn session_rows(
    config: &Map<String, Value>,
    config_path: &str,
    records: &[Record],
) -> Result<Vec<Map<String, Value>>> {
    let selected = selected_set(config)?;
    let chosen = membership::names(config)?;
    let activity = match membership::state_dir(config, config_path) {
        Ok(dir) => membership::load_activity(&dir),
        Err(error) if error.kind == crate::error::Kind::Type => IndexMap::new(),
        Err(error) => return Err(error),
    };
    let mut rows = Vec::new();
    for record in records {
        let sid = record.get("sessionId").cloned().unwrap_or(Value::Null);
        let mut row = Map::new();
        row.insert("id".into(), sid.clone());
        row.insert(
            "name".into(),
            record
                .get("name")
                .cloned()
                .unwrap_or_else(|| Value::from("")),
        );
        row.insert(
            "status".into(),
            record
                .get("status")
                .cloned()
                .unwrap_or_else(|| Value::from("unknown")),
        );
        let enrolled = contains(&selected, &sid);
        row.insert("enrolled".into(), Value::Bool(enrolled));
        if enrolled {
            let id = str_value(&sid);
            let name = match row.get("name") {
                Some(Value::String(name)) => name.clone(),
                _ => return Err(Error::value("invalid name")),
            };
            let mut joined = chosen.get(&id).cloned().unwrap_or_default();
            if joined.is_empty() {
                joined = membership::peer_name(&name);
            }
            if joined.is_empty() {
                joined = "Claude".into();
            }
            row.insert("joined_name".into(), Value::from(joined));
            row.insert(
                "expires_at".into(),
                membership::expires_at(config, &activity, &id)?
                    .map(|s| s.to_value())
                    .unwrap_or(Value::Null),
            );
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Why join or leave found no chat to act on, run outside one.
pub const NOT_IN_CHAT: &str =
    "this isn't a Claude Code chat; run it inside the chat, or pass --chat with a chat's ID (from switchboard chats)";
/// The start of the error for a `--chat` ID that no open chat has.
pub const NO_SUCH_CHAT: &str = "no open chat has the ID";

pub fn current_session(config: &Map<String, Value>, explicit: Option<&str>) -> Result<Record> {
    let sessions = real_sessions(config)?;
    let explicit = explicit.filter(|e| !e.is_empty());
    let matches: Vec<Record> = match explicit {
        Some(explicit) => sessions
            .into_iter()
            .filter(|s| s.get("sessionId").and_then(Value::as_str) == Some(explicit))
            .collect(),
        None => {
            let raw = std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").unwrap_or_default();
            let target = match raw.strip_prefix("uds:") {
                Some(rest) => unquote(rest),
                None => raw,
            };
            if target.is_empty() {
                Vec::new()
            } else {
                let wanted = fsutil::realpath(&target).unwrap_or_default();
                sessions
                    .into_iter()
                    .filter(|s| {
                        s.get("messagingSocketPath")
                            .and_then(Value::as_str)
                            .and_then(|p| fsutil::realpath(p).ok())
                            .is_some_and(|p| p == wanted)
                    })
                    .collect()
            }
        }
    };
    if matches.len() != 1 {
        return Err(Error::value(match explicit {
            Some(id) => format!("{NO_SUCH_CHAT} {id}; run switchboard chats to find its full ID"),
            None => NOT_IN_CHAT.to_string(),
        }));
    }
    Ok(matches.into_iter().next().expect("one match"))
}

/// Accept any text that parses as a UUID (any case, braces, `urn:uuid:`, or
/// no hyphens); only malformed text fails.
fn parse_uuid(value: &str) -> Result<()> {
    match valid_uuid_str(value) {
        Ok(_) => Ok(()),
        Err(error) if error.message == "invalid UUID" => Ok(()),
        Err(error) => Err(error),
    }
}

/// Join or leave under the
/// config lock; returns the session and the config as written.
pub fn edit_config(
    path: &str,
    action: &str,
    explicit: Option<&str>,
    name: Option<String>,
) -> Result<(Record, Map<String, Value>)> {
    let mut live: IndexMap<String, String> = IndexMap::new();
    membership::update_config(path, &mut live, |config, live| {
        let session: Record = if action == "leave" && explicit.is_some_and(|e| !e.is_empty()) {
            // A chat that is no longer live can still be left by its UUID;
            // it then has no name to report.
            let explicit = explicit.expect("explicit");
            parse_uuid(explicit)?;
            match current_session(config, Some(explicit)) {
                Ok(session) => session,
                Err(_) => {
                    let mut session = Map::new();
                    session.insert("sessionId".into(), Value::from(explicit));
                    session
                }
            }
        } else {
            current_session(config, explicit)?
        };
        let sid = session.get("sessionId").map(str_value).unwrap_or_default();
        if action == "join" {
            if let Some(Value::String(name)) = session.get("name") {
                live.insert(sid.clone(), name.clone());
            } else if session.get("name").is_none() {
                live.insert(sid.clone(), String::new());
            }
            let activity = match membership::state_dir(config, path) {
                Ok(dir) => membership::load_activity(&dir),
                Err(error) if error.kind == crate::error::Kind::Type => IndexMap::new(),
                Err(error) => return Err(error),
            };
            let now = crate::fmtutil::time_now();
            let gone: Vec<String> = membership::expired(config, &activity, now)?
                .into_iter()
                .filter(|s| *s != sid)
                .collect();
            membership::drop_sessions(config, &gone);
            let mut selected = selected_set(config)?;
            if !contains(&selected, &Value::from(sid.clone())) {
                selected.push(Value::from(sid.clone()));
            }
            let mut unique: Vec<Value> = Vec::new();
            for item in selected {
                if !contains(&unique, &item) {
                    unique.push(item);
                }
            }
            if unique.len() > 32 {
                return Err(Error::value(
                    "at most 32 chats may be joined; leave another chat first",
                ));
            }
            let mut strings: Vec<String> = Vec::new();
            for item in &unique {
                match item {
                    Value::String(s) => strings.push(s.clone()),
                    _ => {
                        return Err(Error::type_error(
                            "'<' not supported between instances of 'str' and another type",
                        ))
                    }
                }
            }
            strings.sort();
            config.insert(
                "sessions".into(),
                Value::Array(strings.into_iter().map(Value::from).collect()),
            );
            membership::joined_at(config)?;
            let mut joined: Map<String, Value> = config
                .get("joined_at")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            joined.insert(
                sid.clone(),
                Value::from(crate::fmtutil::round_half_even(now)),
            );
            config.insert("joined_at".into(), Value::Object(joined));
            membership::names(config)?;
            let mut chosen: Map<String, Value> = config
                .get("names")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            chosen.shift_remove(&sid);
            if let Some(name) = name.clone().filter(|n| !n.is_empty()) {
                chosen.insert(sid.clone(), Value::from(name));
            }
            if chosen.is_empty() {
                config.shift_remove("names");
            } else {
                config.insert("names".into(), Value::Object(chosen));
            }
        } else {
            membership::drop_sessions(config, std::slice::from_ref(&sid));
        }
        Ok((session, config.clone()))
    })
}

/// Wait until the running
/// bridge has published (or withdrawn) this chat. For a join, `name` is the
/// name the chat should be published under (the chosen name, else the chat's
/// own): a publication under another name is the one from before a rename,
/// so it confirms nothing. If the new name is not published in time, the
/// result is unconfirmed and `published_name` says what other machines still
/// see.
pub fn await_bridge(
    config: &Map<String, Value>,
    config_path: &str,
    sid: &str,
    joined: bool,
    name: Option<&str>,
    timeout: f64,
) -> Result<Map<String, Value>> {
    let mut result = Map::new();
    let unconfirmed = |detail: &str| {
        let mut value = Map::new();
        value.insert("confirmed".into(), Value::Bool(false));
        value.insert("detail".into(), Value::from(detail));
        value
    };
    if config.get("enabled") != Some(&Value::Bool(true)) {
        return Ok(unconfirmed("the bridge is disabled in its configuration"));
    }
    // The bridge publishes the cleaned name, or "Claude" when nothing is left.
    let expected = name.map(|raw| {
        if crate::protocol::clean_name(raw).is_empty() {
            membership::peer_name("Claude")
        } else {
            membership::peer_name(raw)
        }
    });
    let mut stale_name: Option<String>;
    let start = crate::identity::monotonic();
    let deadline = start + timeout;
    let offline_deadline = start + timeout.min(3.0);
    let mut detail;
    loop {
        stale_name = None;
        let runtime = match runtime_status(config, config_path) {
            Ok(runtime) => Some(runtime),
            Err(error) if crate::cli::is_file_not_found(&error) => {
                return Ok(unconfirmed("the bridge is not running (switchboard up)"))
            }
            Err(error) if error.is_handled() => None,
            Err(error) => return Err(error),
        };
        detail = "the bridge is not running (switchboard up)";
        if let Some(runtime) = runtime {
            if !runtime.contains_key("published") {
                return Ok(unconfirmed(
                    "the running bridge is too old to confirm; restart it with switchboard down, then switchboard up",
                ));
            }
            let mut published: IndexMap<String, Map<String, Value>> = IndexMap::new();
            if let Some(Value::Array(items)) = runtime.get("published") {
                for item in items {
                    if let Value::Object(item) = item {
                        let key = item
                            .get("id")
                            .map(crate::wirejson::dumps)
                            .unwrap_or_else(|| "null".into());
                        published.insert(key, item.clone());
                    }
                }
            }
            let remote = runtime
                .get("remote")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let sid_key = crate::wirejson::dumps(&Value::from(sid));
            if runtime.get("connected") != Some(&Value::Bool(true)) {
                detail = "the bridge is offline, so other machines cannot see changes until it reconnects";
                if crate::identity::monotonic() >= offline_deadline {
                    return Ok(unconfirmed(detail));
                }
            } else if published.contains_key(&sid_key) == joined {
                let name = if joined {
                    match published[&sid_key].get("name") {
                        Some(value) if truthy(Some(value)) => match value {
                            Value::String(name) => membership::peer_name(name),
                            _ => return Err(Error::value("invalid name")),
                        },
                        _ => String::new(),
                    }
                } else {
                    String::new()
                };
                if joined && expected.as_ref().is_some_and(|expected| *expected != name) {
                    detail = "the bridge has not published the new name within 10 seconds";
                    stale_name = Some(name).filter(|name| !name.is_empty());
                } else {
                    result.insert("confirmed".into(), Value::Bool(true));
                    result.insert(
                        "remote_sessions".into(),
                        remote.get("sessions").cloned().unwrap_or(Value::from(0)),
                    );
                    result.insert(
                        "remote_machines".into(),
                        remote.get("machines").cloned().unwrap_or(Value::from(0)),
                    );
                    if joined && !name.is_empty() {
                        result.insert("name".into(), Value::from(name));
                    }
                    return Ok(result);
                }
            } else {
                detail = "the bridge has not published this change within 10 seconds";
            }
        }
        if crate::identity::monotonic() >= deadline {
            let mut value = unconfirmed(detail);
            if let Some(stale) = stale_name {
                value.insert("published_name".into(), Value::from(stale));
            }
            return Ok(value);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

// Safe file checks shared with doctor, the roster, and the machine commands.

/// Reject links and writable ancestors.
pub fn safe_path(path: &str, directory: bool, private: bool) -> Result<String> {
    let path = crate::fileops::abspath(path)?;
    let mut ancestors = Vec::new();
    let mut current = path.clone();
    loop {
        let up = fsutil::parent(&current);
        if up == current {
            break;
        }
        ancestors.push(up.clone());
        current = up;
    }
    for parent in ancestors.iter().rev() {
        let info = fsutil::lstat(parent)?;
        let mode = info.mode();
        let uid = info.uid();
        if !fsutil::is_type(&info, fsutil::S_IFDIR)
            || (uid != 0 && uid != fsutil::getuid())
            || (mode & 0o022 != 0 && !(uid == 0 && mode & fsutil::S_ISVTX != 0))
        {
            return Err(Error::value("unsafe ancestor"));
        }
    }
    let info = fsutil::lstat(&path)?;
    let expected = if directory {
        fsutil::S_IFDIR
    } else {
        fsutil::S_IFREG
    };
    let forbidden = if private { 0o077 } else { 0o022 };
    if !fsutil::is_type(&info, expected)
        || info.uid() != fsutil::getuid()
        || info.mode() & forbidden != 0
        || (!directory && info.size() > 1024 * 1024)
    {
        return Err(Error::value("unsafe path"));
    }
    Ok(path)
}

/// A private JSON object of at most `limit` bytes, opened without
/// following links or blocking on FIFOs.
pub fn read_json_bounded(path: &str, limit: usize) -> Result<Map<String, Value>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    let info = file.metadata().map_err(|exc| Error::from_io(&exc, None))?;
    if !fsutil::is_type(&info, fsutil::S_IFREG)
        || info.uid() != fsutil::getuid()
        || info.mode() & 0o077 != 0
    {
        return Err(Error::value("unsafe JSON file"));
    }
    let mut raw = Vec::new();
    (&file)
        .take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|exc| Error::from_io(&exc, None))?;
    if raw.len() > limit {
        return Err(Error::value("JSON file too large"));
    }
    match crate::wirejson::loads(&raw)? {
        Value::Object(map) => Ok(map),
        _ => Err(Error::value("JSON object required")),
    }
}

/// An owned, private, regular file (<= 4 MiB).
pub fn private_bytes(path: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    let info = file.metadata().map_err(|exc| Error::from_io(&exc, None))?;
    if !fsutil::is_type(&info, fsutil::S_IFREG)
        || info.uid() != fsutil::getuid()
        || info.mode() & 0o077 != 0
    {
        return Err(Error::value(
            "enrollment file must be owned, regular, and private",
        ));
    }
    let limit = 4 * 1024 * 1024;
    let mut raw = Vec::new();
    (&file)
        .take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|exc| Error::from_io(&exc, None))?;
    if raw.len() > limit {
        return Err(Error::value("enrollment file too large"));
    }
    Ok(raw)
}

/// The server address (`server_url`, or `hub_url` in older configs), cert,
/// key, and optional ca from a config file, every path checked
/// with `safe_path` (as roster, ping, and machine commands build it).
pub fn client_config(
    config_path: &str,
) -> Result<(Map<String, Value>, crate::hub_transport::ClientConfig)> {
    let path = safe_path(config_path, false, true)?;
    let source = read_json_bounded(&path, 65536)?;
    let hub_url = crate::config::server_url(&source)
        .cloned()
        .ok_or_else(|| Error::value("the configuration has no server_url"))?;
    let mut config = crate::hub_transport::ClientConfig {
        hub_url: match &hub_url {
            Value::String(value) => value.clone(),
            _ => String::new(),
        },
        ..Default::default()
    };
    for key in ["cert", "key", "ca"] {
        if key == "ca" && source.get(key).is_none_or(Value::is_null) {
            continue;
        }
        let value = source.get(key).ok_or_else(|| Error::value(repr_str(key)))?;
        let joined = match value {
            Value::String(value) if fsutil::is_absolute(&fsutil::normalize(value)) => {
                fsutil::normalize(value)
            }
            Value::String(value) => fsutil::join(&fsutil::parent(&path), value),
            other => {
                return Err(Error::type_error(format!(
                    "unsupported operand type(s) for /: 'PosixPath' and {}",
                    repr_str(crate::discovery::type_name(Some(other)))
                )))
            }
        };
        let checked = safe_path(&joined, false, true)?;
        match key {
            "cert" => config.cert = checked,
            "key" => config.key = checked,
            _ => config.ca = Some(checked),
        }
    }
    Ok((source, config))
}
