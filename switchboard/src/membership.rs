//! Joined-chat names, join times, and inactivity expiry, shared by the CLI
//! and bridge.
//!
//! The bridge config keeps the selection (`sessions`) plus two optional maps
//! keyed by session UUID: `names` (the name peers see, when the user chose
//! one at join) and `joined_at` (epoch seconds of the latest join). The
//! bridge's private state keeps `activity.json`: the last time each joined
//! chat worked (native status busy or waiting) or sent or received a
//! Switchboard message. A chat expires when the later of its join and its
//! last activity is more than seven days old.
//!
//! `membership.json`, beside the config, lists the joined chats and the names
//! peers see; the Claude Code plugin watches it for its footer label.

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::fileops;
use crate::fmtutil::{dumps_indent, float_value, round_half_even};
use crate::fsutil;
use crate::protocol::{clean_name, valid_uuid_str, FRAME_LIMIT};
use crate::wirejson::{self, num_kind, value_eq, NumKind};

pub const EXPIRY_SECONDS: i64 = 7 * 86400;
pub const ACTIVITY_FILE: &str = "activity.json";
pub const MEMBERSHIP_FILE: &str = "membership.json";
pub const NAME_LIMIT: usize = 26;

/// A timestamp that stays an integer or a float as it was written; adding
/// seconds and picking the later one keep its type, so files and output
/// print it the same way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stamp {
    Int(i64),
    Float(f64),
}

impl Stamp {
    pub fn as_f64(self) -> f64 {
        match self {
            Stamp::Int(value) => value as f64,
            Stamp::Float(value) => value,
        }
    }

    pub fn plus(self, seconds: i64) -> Stamp {
        match self {
            Stamp::Int(value) => Stamp::Int(value.saturating_add(seconds)),
            Stamp::Float(value) => Stamp::Float(value + seconds as f64),
        }
    }

    pub fn to_value(self) -> Value {
        match self {
            Stamp::Int(value) => Value::from(value),
            Stamp::Float(value) => float_value(value),
        }
    }

    /// A decoded JSON number that is an int or float (not a bool).
    pub fn from_value(value: &Value) -> Option<Stamp> {
        match value {
            Value::Number(number) => Some(match num_kind(number) {
                NumKind::Int(digits) => match digits.parse::<i64>() {
                    Ok(value) => Stamp::Int(value),
                    Err(_) => Stamp::Float(digits.parse::<f64>().unwrap_or(f64::INFINITY)),
                },
                NumKind::Float(value) => Stamp::Float(value),
            }),
            _ => None,
        }
    }
}

/// The session-name part of a remote alias, exactly as peers build it.
pub fn peer_name(value: &str) -> String {
    let dashed = dash_unsafe(&clean_name(value));
    let trimmed = dashed.trim_matches('-');
    crate::text::take_chars(trimmed, NAME_LIMIT)
        .trim_matches('-')
        .to_string()
}

/// `re.sub(r"[^A-Za-z0-9_-]+", "-", value)`.
pub fn dash_unsafe(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out
}

fn uuid_map<'a>(
    config: &'a Map<String, Value>,
    key: &str,
    numeric: bool,
) -> Result<Option<&'a Map<String, Value>>> {
    let value = match config.get(key) {
        None => return Ok(None),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(Error::value(format!(
                "{key} must be an object keyed by session UUID"
            )))
        }
    };
    for (sid, item) in value {
        valid_uuid_str(sid)?;
        let ok = if numeric {
            matches!(item, Value::Number(_))
        } else {
            matches!(item, Value::String(_))
        };
        if !ok {
            return Err(Error::value(format!("invalid {key} entry")));
        }
    }
    Ok(Some(value))
}

/// `names(config)`: the chosen peer-visible names.
pub fn names(config: &Map<String, Value>) -> Result<IndexMap<String, String>> {
    Ok(uuid_map(config, "names", false)?
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default())
}

/// `joined_at(config)`.
pub fn joined_at(config: &Map<String, Value>) -> Result<IndexMap<String, Stamp>> {
    Ok(uuid_map(config, "joined_at", true)?
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Stamp::from_value(v).map(|s| (k.clone(), s)))
                .collect()
        })
        .unwrap_or_default())
}

/// `state_dir(config, config_path)`: the state directory, relative to the
/// config's resolved parent when not absolute.
pub fn state_dir(config: &Map<String, Value>, config_path: &str) -> Result<String> {
    let value = match config.get("state_dir") {
        Some(Value::String(value)) => fsutil::normalize(value),
        None => return Err(Error::type_error("'NoneType' object is not subscriptable")),
        Some(_) => {
            return Err(Error::type_error(
                "expected str, bytes or os.PathLike object",
            ))
        }
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

/// Activity stamps, always floats once loaded.
pub type Activity = IndexMap<String, f64>;

/// `load_activity(directory)`: unreadable or malformed files read as empty.
pub fn load_activity(directory: &str) -> Activity {
    let path = fsutil::join(directory, ACTIVITY_FILE);
    let value = match fsutil::read_json(&path, 65536, true) {
        Ok(value) => value,
        Err(_) => return Activity::new(),
    };
    let mut result = Activity::new();
    for (sid, stamp) in value {
        if valid_uuid_str(&sid).is_err() {
            continue;
        }
        if let Some(stamp) = Stamp::from_value(&stamp) {
            result.insert(sid, stamp.as_f64());
        }
    }
    result
}

/// `save_activity(directory, activity)`: sorted, rounded to whole seconds.
pub fn save_activity(directory: &str, activity: &Activity) -> Result<()> {
    let mut entries: Vec<(&String, &f64)> = activity.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut map = Map::new();
    for (sid, stamp) in entries {
        map.insert(sid.clone(), Value::from(round_half_even(*stamp)));
    }
    let mut text = wirejson::dumps(&Value::Object(map));
    text.push('\n');
    fileops::atomic_write(
        &fsutil::join(directory, ACTIVITY_FILE),
        ".activity-",
        ".json",
        text.as_bytes(),
    )
}

/// `last_active`: the later of activity and join (the activity stamp wins a tie).
pub fn last_active(
    config: &Map<String, Value>,
    activity: &Activity,
    sid: &str,
) -> Result<Option<Stamp>> {
    let joined = joined_at(config)?;
    let stamps: Vec<Stamp> = [
        activity.get(sid).map(|v| Stamp::Float(*v)),
        joined.get(sid).copied(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut best: Option<Stamp> = None;
    for stamp in stamps {
        if best.is_none_or(|b| stamp.as_f64() > b.as_f64()) {
            best = Some(stamp);
        }
    }
    Ok(best)
}

pub fn expires_at(
    config: &Map<String, Value>,
    activity: &Activity,
    sid: &str,
) -> Result<Option<Stamp>> {
    Ok(last_active(config, activity, sid)?.map(|stamp| stamp.plus(EXPIRY_SECONDS)))
}

/// The config's `sessions` list as strings (non-strings are skipped; callers
/// only see validated lists).
pub fn sessions(config: &Map<String, Value>) -> Vec<String> {
    match config.get("sessions") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Joined chats past expiry. A chat with no record yet is not expired.
pub fn expired(config: &Map<String, Value>, activity: &Activity, now: f64) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for sid in sessions(config) {
        if let Some(deadline) = expires_at(config, activity, &sid)? {
            if now >= deadline.as_f64() {
                result.push(sid);
            }
        }
    }
    Ok(result)
}

/// Remove chats from the selection and their per-chat entries, in place.
pub fn drop_sessions(config: &mut Map<String, Value>, sids: &[String]) {
    let kept: Vec<Value> = match config.get("sessions") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|v| !v.as_str().is_some_and(|s| sids.iter().any(|x| x == s)))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    config.insert("sessions".into(), Value::Array(kept));
    for key in ["names", "joined_at"] {
        if let Some(Value::Object(map)) = config.get(key) {
            let filtered: Map<String, Value> = map
                .iter()
                .filter(|(sid, _)| !sids.iter().any(|x| x == *sid))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if filtered.is_empty() {
                config.shift_remove(key);
            } else {
                config.insert(key.into(), Value::Object(filtered));
            }
        }
    }
}

pub fn membership_path(config_path: &str) -> String {
    fsutil::join(&fsutil::parent(config_path), MEMBERSHIP_FILE)
}

/// Joined chats and their peer-visible names: chosen, else live, else last known.
pub fn membership_value(
    config: &Map<String, Value>,
    live_names: &IndexMap<String, String>,
    previous: Option<&Map<String, Value>>,
) -> Result<Value> {
    let chosen = names(config)?;
    let mut entries = Map::new();
    for sid in sessions(config) {
        let last = previous
            .and_then(|known| known.get(&sid))
            .and_then(Value::as_object)
            .and_then(|entry| entry.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let live = live_names.get(&sid).map(String::as_str).unwrap_or("");
        let mut name = chosen.get(&sid).cloned().unwrap_or_default();
        if name.is_empty() {
            name = peer_name(live);
        }
        if name.is_empty() {
            name = last.to_string();
        }
        if name.is_empty() {
            name = "Claude".into();
        }
        let mut entry = Map::new();
        entry.insert("name".into(), Value::String(name));
        entries.insert(sid, Value::Object(entry));
    }
    let mut value = Map::new();
    value.insert("version".into(), Value::from(1));
    value.insert("sessions".into(), Value::Object(entries));
    Ok(Value::Object(value))
}

/// Rewrite membership.json when its content changes; returns the value.
pub fn write_membership(
    config_path: &str,
    config: &Map<String, Value>,
    live_names: &IndexMap<String, String>,
) -> Result<Value> {
    let path = membership_path(config_path);
    let current = fsutil::read_json(&path, 65536, true).ok();
    let previous = current
        .as_ref()
        .and_then(|c| c.get("sessions"))
        .and_then(Value::as_object);
    let value = membership_value(config, live_names, previous)?;
    if let Some(current) = &current {
        if value_eq(&value, &Value::Object(current.clone())) {
            return Ok(value);
        }
    }
    let mut text = dumps_indent(&value, 2, true);
    text.push('\n');
    fileops::atomic_write(&path, ".membership-", ".json", text.as_bytes())?;
    Ok(value)
}

/// The config lock: `<config>.lock`, created 0600, held exclusively.
pub fn config_lock(path: &str) -> Result<fileops::FileLock> {
    let normal = fsutil::normalize(path);
    if fileops::name(&normal).is_empty() {
        // `Path.with_suffix` refuses a path without a name, such as '.'.
        return Err(Error::value(format!(
            "PosixPath({}) has an empty name",
            crate::text::repr_str(&normal)
        )));
    }
    fileops::lock_exclusive(&format!("{normal}.lock"))
}

/// Read the config as `native_peer.read_json(path)` does.
pub fn read_config(path: &str) -> Result<Map<String, Value>> {
    fsutil::read_json(path, FRAME_LIMIT, true)
}

/// Rewrite membership.json from the config as it is now, under the config lock.
pub fn sync_membership(config_path: &str, live_names: &IndexMap<String, String>) -> Result<Value> {
    let _lock = config_lock(config_path)?;
    let config = read_config(config_path)?;
    write_membership(config_path, &config, live_names)
}

/// Apply `mutate(config, live_names)` under the config lock and write the
/// config atomically (indent 2, newline). membership.json follows in the
/// same critical section; its failure leaves the config change in place.
pub fn update_config<T>(
    path: &str,
    live_names: &mut IndexMap<String, String>,
    mutate: impl FnOnce(&mut Map<String, Value>, &mut IndexMap<String, String>) -> Result<T>,
) -> Result<T> {
    let _lock = config_lock(path)?;
    let mut config = read_config(path)?;
    let result = mutate(&mut config, live_names)?;
    let mut text = dumps_indent(&Value::Object(config.clone()), 2, false);
    text.push('\n');
    fileops::atomic_write(&fsutil::normalize(path), ".bridge-", "", text.as_bytes())?;
    let _ = write_membership(path, &config, live_names);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn peer_names_are_cleaned() {
        assert_eq!(peer_name("Héllo World!!"), "H-llo-World");
        assert_eq!(peer_name("!!!"), "");
        assert_eq!(peer_name("Release Planner"), "Release-Planner");
        assert_eq!(peer_name(&"a".repeat(30)), "a".repeat(26));
        assert_eq!(
            peer_name("abcdefghijklmnopqrstuvwxy-z"),
            "abcdefghijklmnopqrstuvwxy"
        );
    }

    #[test]
    fn expiry_keeps_integer_or_float_stamps() {
        let sid = "11111111-1111-4111-8111-111111111111";
        let config = json!({"sessions": [sid], "joined_at": {sid: 100}});
        let config = config.as_object().unwrap();
        let mut activity = Activity::new();
        assert_eq!(
            expires_at(config, &activity, sid).unwrap(),
            Some(Stamp::Int(100 + EXPIRY_SECONDS))
        );
        activity.insert(sid.into(), 150.0);
        assert_eq!(
            expires_at(config, &activity, sid).unwrap(),
            Some(Stamp::Float(150.0 + EXPIRY_SECONDS as f64))
        );
        activity.insert(sid.into(), 100.0);
        // A tie keeps the first (activity) stamp, a float.
        assert!(matches!(
            expires_at(config, &activity, sid).unwrap(),
            Some(Stamp::Float(_))
        ));
        assert_eq!(
            expired(config, &activity, 1e12).unwrap(),
            vec![sid.to_string()]
        );
    }

    #[test]
    fn drop_keeps_key_order() {
        let sid = "11111111-1111-4111-8111-111111111111";
        let mut config = json!({"a": 1, "names": {sid: "x"}, "sessions": [sid], "z": 2})
            .as_object()
            .unwrap()
            .clone();
        drop_sessions(&mut config, &[sid.to_string()]);
        assert_eq!(config.keys().collect::<Vec<_>>(), ["a", "sessions", "z"]);
    }

    #[test]
    fn invalid_maps_are_refused() {
        let config = json!({"names": []});
        assert_eq!(
            names(config.as_object().unwrap()).unwrap_err().message,
            "names must be an object keyed by session UUID"
        );
        let config = json!({"names": {"nope": "x"}});
        assert_eq!(
            names(config.as_object().unwrap()).unwrap_err().message,
            "badly formed hexadecimal UUID string"
        );
        let config = json!({"joined_at": {"11111111-1111-4111-8111-111111111111": true}});
        assert_eq!(
            joined_at(config.as_object().unwrap()).unwrap_err().message,
            "invalid joined_at entry"
        );
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn activity_round_trip_private_and_tolerant() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("sbm-{}", crate::identity::uuid4_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let directory = dir.to_string_lossy().into_owned();
        let sid = "11111111-1111-4111-8111-111111111111".to_string();
        let mut activity = Activity::new();
        activity.insert(sid.clone(), 12.6);
        save_activity(&directory, &activity).unwrap();
        let path = dir.join(ACTIVITY_FILE);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{{\"{sid}\": 13}}\n")
        );
        assert_eq!(
            load_activity(&directory),
            Activity::from([(sid.clone(), 13.0)])
        );
        std::fs::write(&path, format!("{{\"bad\": 1, \"{sid}\": \"x\"}}")).unwrap();
        assert!(load_activity(&directory).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn membership_sync_keeps_last_name_and_skips_unchanged_writes() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("sbm-{}", crate::identity::uuid4_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge.json");
        let (sid, other) = (
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        );
        std::fs::write(&path, format!(r#"{{"enabled": true, "sessions": ["{sid}", "{other}"], "names": {{"{other}": "chosen"}}, "state_dir": "state"}}"#)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = path.to_string_lossy().into_owned();
        sync_membership(
            &config,
            &IndexMap::from([(sid.to_string(), "Live name".to_string())]),
        )
        .unwrap();
        let file = dir.join(MEMBERSHIP_FILE);
        let raw = std::fs::read_to_string(&file).unwrap();
        assert_eq!(raw, format!("{{\n  \"sessions\": {{\n    \"{sid}\": {{\n      \"name\": \"Live-name\"\n    }},\n    \"{other}\": {{\n      \"name\": \"chosen\"\n    }}\n  }},\n  \"version\": 1\n}}\n"));
        let inode = std::fs::metadata(&file).unwrap().ino();
        sync_membership(&config, &IndexMap::new()).unwrap();
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("Live-name"));
        assert_eq!(std::fs::metadata(&file).unwrap().ino(), inode);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
