//! Invites and the machine commands run on the server itself.
//!
//! A machine joins by redeeming an invite: `switchboard server invite` on the
//! server prints a token (`crate::invite`) holding the server's address, its
//! certificate's SHA-256 when the certificate is private, and a random
//! secret; `switchboard init` on the new machine sends the secret
//! with its enrollment request, and the server enrolls the key at once.
//!
//! The server keeps only each secret's SHA-256, in `invites.json` beside the
//! database, with the invite's expiry, the uses it has left, and the machine
//! name it fixes, if any. Every change to that file, by the server or by these
//! commands, happens under the registry lock (`<peers_file>.lock`), so an
//! invite redeemed twice at once is used once. The file is owned like the
//! registry, mode 0600, and is never read or written through a link.
//!
//! These commands trust the machine they run on, like `enroll-local`: they
//! read the server config and its state directly, so they need root or the
//! server's service account.
//!
//! - `switchboard server invite [--expires DURATION] [--uses N] [--name MACHINE]`
//! - `switchboard server invite revoke ID`
//! - `switchboard server invites`
//! - `switchboard server machines`
//! - `switchboard server revoke NAME`

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

use serde_json::{Map, Value};
use sha2::Digest;

use super::enroll_local::{atomic, open_lock, os_error, read_file, refuse, Failure, Result};
use super::jsonval::{self, get, JsonStr, Val};
use crate::fsutil;
use crate::invite::{self, Invite};

/// The store's name in the server's state directory.
pub const STORE: &str = "invites.json";
/// How long an invite from `server invite` lasts by default (seconds).
pub const DEFAULT_EXPIRY: i64 = 3600;
/// How long the invite `server install` prints lasts (seconds).
pub const INSTALL_EXPIRY: i64 = 3600;
const MAX_EXPIRY: i64 = 30 * 86400;
const MAX_USES: i64 = 1000;
/// Open invites at most; older ones must be used, expire, or be revoked.
const MAX_OPEN: usize = 64;
const STORE_LIMIT: u64 = 262_144;
const DEFAULT_CONFIG: &str = super::install::CONFIG;

/// `invites.json` beside the database.
pub fn store_path(db: &str) -> String {
    fsutil::join(&fsutil::parent(db), STORE)
}

/// One invite as stored: the secret's SHA-256, never the secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The first 12 hex digits of `hash`; names the invite in listings.
    pub id: String,
    pub hash: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// Uses left.
    pub uses: i64,
    pub machine: Option<String>,
    /// `install` for the invite `server install` printed, else `command`.
    pub source: String,
}

impl Entry {
    /// Usable now: unexpired with a use left.
    pub fn open(&self, now: i64) -> bool {
        self.uses > 0 && now < self.expires_at
    }
}

/// How long an invite that was used up or expired is remembered, so a
/// machine that tries it later is told which (not just that it failed).
const REMEMBER: i64 = 7 * 86400;
/// Used-up and expired invites remembered at most.
const MAX_CLOSED: usize = 128;

/// Whether the store still remembers `entry` (open, or closed within
/// `REMEMBER`).
pub fn remembered(entry: &Entry, now: i64) -> bool {
    entry.open(now) || now < entry.expires_at.saturating_add(REMEMBER)
}

/// Keep the open invites, and the most recent invites that were used up or
/// expired within `REMEMBER`; drop the rest. The order is kept.
pub fn prune(entries: &mut Vec<Entry>, now: i64) {
    let mut closed: Vec<(i64, String)> = entries
        .iter()
        .filter(|e| !e.open(now) && remembered(e, now))
        .map(|e| (e.created_at, e.id.clone()))
        .collect();
    closed.sort_by(|a, b| b.cmp(a));
    closed.truncate(MAX_CLOSED);
    entries.retain(|e| e.open(now) || closed.iter().any(|(_, id)| *id == e.id));
}

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse the store; `None` for anything malformed (the caller fails closed).
pub fn parse_store(data: &[u8]) -> Option<Vec<Entry>> {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(data) else {
        return None;
    };
    if map.len() > 4 * MAX_OPEN {
        return None;
    }
    let mut entries = Vec::new();
    for (id, value) in &map {
        let Value::Object(fields) = value else {
            return None;
        };
        let known = [
            "hash",
            "created_at",
            "expires_at",
            "uses",
            "machine",
            "source",
        ];
        if fields.len() != known.len() || !known.iter().all(|k| fields.contains_key(*k)) {
            return None;
        }
        let hash = fields["hash"].as_str()?;
        let int = |key: &str| fields[key].as_i64();
        let machine = match &fields["machine"] {
            Value::Null => None,
            Value::String(name) if invite::is_machine_name(name) => Some(name.clone()),
            _ => return None,
        };
        let entry = Entry {
            id: id.clone(),
            hash: hash.to_string(),
            created_at: int("created_at")?,
            expires_at: int("expires_at")?,
            uses: int("uses").filter(|n| (0..=MAX_USES).contains(n))?,
            machine,
            source: fields["source"].as_str()?.to_string(),
        };
        if !is_hex(&entry.hash, 64) || entry.id != entry.hash[..12] {
            return None;
        }
        entries.push(entry);
    }
    Some(entries)
}

/// The store's JSON, one invite per key in the order given.
pub fn render_store(entries: &[Entry]) -> String {
    let mut map = Map::new();
    for entry in entries {
        let mut fields = Map::new();
        fields.insert("hash".into(), Value::from(entry.hash.clone()));
        fields.insert("created_at".into(), Value::from(entry.created_at));
        fields.insert("expires_at".into(), Value::from(entry.expires_at));
        fields.insert("uses".into(), Value::from(entry.uses));
        fields.insert(
            "machine".into(),
            entry.machine.clone().map_or(Value::Null, Value::from),
        );
        fields.insert("source".into(), Value::from(entry.source.clone()));
        map.insert(entry.id.clone(), Value::Object(fields));
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(map)).expect("plain JSON");
    text.push('\n');
    text
}

/// Read the store at `path`, which `owner` must own with mode 0600 as a
/// singly linked regular file (never a link). A missing file is no invites.
pub fn read_store(path: &str, owner: u32) -> std::result::Result<Vec<Entry>, String> {
    let c = std::ffi::CString::new(path).map_err(|_| "embedded null byte".to_string())?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: open with a valid path; the descriptor is owned below.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(Vec::new()),
            _ => Err(format!("unsafe or unreadable invite store: {path}")),
        };
    }
    // SAFETY: fd is a new descriptor that nothing else owns.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let info = file.metadata().map_err(|exc| format!("{path}: {exc}"))?;
    if !info.file_type().is_file()
        || info.nlink() != 1
        || info.uid() != owner
        || info.mode() & 0o7777 != 0o600
        || info.size() > STORE_LIMIT
    {
        return Err(format!("unsafe invite store owner, mode, or type: {path}"));
    }
    let mut data = Vec::new();
    (&file)
        .take(STORE_LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|exc| format!("{path}: {exc}"))?;
    parse_store(&data).ok_or_else(|| format!("invalid invite store: {path}"))
}

/// Replace the store atomically: a new file beside it (created exclusively,
/// never through a link), owned `uid:gid` with mode 0600, synced, then
/// renamed into place. A group the caller cannot give is left as created.
pub fn write_store(
    path: &str,
    entries: &[Entry],
    uid: u32,
    gid: u32,
) -> std::result::Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = fsutil::parent(path);
    let temporary = fsutil::join(
        &directory,
        &format!(".{STORE}.{}", crate::identity::uuid4_hex()),
    );
    let fail = |exc: std::io::Error| format!("{path}: {exc}");
    let result = (|| -> std::result::Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(fail)?;
        let created = file.metadata().map_err(fail)?;
        if (created.uid(), created.gid()) != (uid, gid) {
            // SAFETY: file is an open descriptor.
            if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
                let error = std::io::Error::last_os_error();
                let only_group = created.uid() == uid
                    && matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES));
                if !only_group {
                    return Err(fail(error));
                }
            }
        }
        // SAFETY: file is an open descriptor.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(fail(std::io::Error::last_os_error()));
        }
        file.write_all(render_store(entries).as_bytes())
            .map_err(fail)?;
        file.sync_all().map_err(fail)?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(fail)?;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(&directory)
            .map_err(fail)?;
        dir.sync_all().map_err(fail)
    })();
    if std::fs::symlink_metadata(&temporary).is_ok() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Seconds since the epoch.
pub fn now() -> i64 {
    super::hub::round_even(super::hub::now())
}

// The local commands ---------------------------------------------------------

/// The server's files, from its config.
pub struct Local {
    pub db: String,
    pub peers: String,
    pub lock: String,
    /// The address clients use, when the config says it.
    pub url: Option<String>,
    /// The private certificate's SHA-256, for a private-mode server.
    pub pin: Option<String>,
}

impl Local {
    pub fn store(&self) -> String {
        store_path(&self.db)
    }
}

fn host_url(host: &str, port: u16, default: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if port == default {
        format!("https://{host}")
    } else {
        format!("https://{host}:{port}")
    }
}

/// Read the server config at `path` the way the server does (paths relative
/// to its directory) and find what an invite needs.
pub fn load(path: &str) -> Result<Local> {
    let (data, _) = read_file(path).map_err(|_| {
        Failure::Reported(format!(
            "cannot read the server config {path}; run this on the server as root or the server's account, or pass --config"
        ))
    })?;
    let Some(Val::Dict(config)) = jsonval::loads(&data) else {
        return refuse("the server config is not a JSON object");
    };
    let directory =
        fsutil::parent(&fsutil::absolute(path).map_err(|e| Failure::Reported(e.message))?);
    let text = |name: &str| -> Result<String> {
        match get(&config, name).and_then(Val::as_str) {
            Some(value) => Ok(value.to_string()),
            None => refuse(&format!("the server config has no {name}")),
        }
    };
    let resolve = |value: &str| {
        if fsutil::is_absolute(value) {
            fsutil::normalize(value)
        } else {
            fsutil::join(&directory, value)
        }
    };
    let db = resolve(&text("db")?);
    let peers = resolve(&text("peers_file")?);
    let lock = match get(&config, "registry_lock").and_then(Val::as_str) {
        Some(lock) if !lock.is_empty() => fsutil::normalize(lock),
        _ => format!("{peers}.lock"),
    };
    let port = |default: u16| -> Result<u16> {
        match get(&config, "port") {
            None => Ok(default),
            Some(Val::Int(digits)) => digits
                .parse()
                .map_err(|_| Failure::Reported("the server config's port is invalid".into())),
            Some(_) => refuse("the server config's port is invalid"),
        }
    };
    let (url, pin) = match get(&config, "tls").and_then(Val::as_str) {
        Some("acme") => (Some(host_url(&text("hostname")?, port(443)?, 443)), None),
        Some("private") => {
            let first = match get(&config, "names") {
                Some(Val::List(names)) => names.first().and_then(Val::as_str).map(str::to_string),
                _ => None,
            }
            .unwrap_or_else(|| "localhost".into());
            let certificate = format!("{}/tls/private-cert.pem", fsutil::parent(&db));
            let (pem, _) = read_file(&certificate)?;
            use rustls::pki_types::pem::PemObject;
            let der = rustls::pki_types::CertificateDer::pem_slice_iter(&pem)
                .next()
                .and_then(|item| item.ok())
                .ok_or_else(|| Failure::Reported(format!("no certificate in {certificate}")))?;
            let pin = super::tls::hex(&sha2::Sha256::digest(der.as_ref()));
            (Some(host_url(&first, port(8743)?, 443)), Some(pin))
        }
        _ => (None, None),
    };
    Ok(Local {
        db,
        peers,
        lock,
        url,
        pin,
    })
}

/// The registry's metadata, after checking this process may act for the
/// server: root, or the account that owns the registry.
fn registry_owner(local: &Local) -> Result<std::fs::Metadata> {
    let info =
        std::fs::symlink_metadata(&local.peers).map_err(|exc| os_error(exc, &local.peers))?;
    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 && euid != info.uid() {
        return refuse("run this as root or as the server's service account");
    }
    if !info.file_type().is_file() || info.nlink() != 1 || info.mode() & 0o7777 != 0o600 {
        return refuse("unsafe registry owner, mode, or type");
    }
    Ok(info)
}

/// Run `body` holding the registry lock, with the registry's metadata.
fn locked<T>(local: &Local, body: impl FnOnce(&std::fs::Metadata) -> Result<T>) -> Result<T> {
    let owner = registry_owner(local)?;
    let lock = open_lock(&local.lock, &owner)?;
    // SAFETY: lock stays open until the end of this function.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(os_error(std::io::Error::last_os_error(), &local.lock));
    }
    let result = body(&owner);
    drop(lock);
    result
}

fn store_failure(message: String) -> Failure {
    Failure::Reported(message)
}

/// A new invite and its token.
pub struct Created {
    pub entry: Entry,
    pub token: String,
    pub url: String,
    pub pin: Option<String>,
}

/// What a new invite allows.
pub struct Request<'a> {
    pub url: &'a str,
    pub pin: Option<&'a str>,
    pub expires: i64,
    pub uses: i64,
    pub machine: Option<&'a str>,
    pub source: &'a str,
    /// Cancel open invites from the same source first (`server install`
    /// replaces the invite an earlier run printed).
    pub replace: bool,
}

/// Make an invite: store its secret's hash under the registry lock. An
/// invite that fixes the machine name has one use: a second machine under
/// that name would be refused as already on the server.
pub fn create(local: &Local, request: &Request<'_>) -> Result<Created> {
    if request.machine.is_some() && request.uses > 1 {
        return refuse("an invite for a fixed machine name enrolls only that machine, so it has one use; drop the name, or make it single use");
    }
    let secret = invite::new_secret();
    let hash = invite::secret_hash(&secret);
    let created_at = now();
    let entry = Entry {
        id: hash[..12].to_string(),
        hash,
        created_at,
        expires_at: created_at + request.expires,
        uses: request.uses,
        machine: request.machine.map(str::to_string),
        source: request.source.to_string(),
    };
    let store = local.store();
    locked(local, |owner| {
        let mut entries = read_store(&store, owner.uid()).map_err(store_failure)?;
        // A replaced invite is forgotten, as a revoked one is.
        entries.retain(|e| !(request.replace && e.source == request.source && e.open(created_at)));
        prune(&mut entries, created_at);
        if entries.iter().filter(|e| e.open(created_at)).count() >= MAX_OPEN {
            return refuse(&format!(
                "{MAX_OPEN} invites are open; revoke some (switchboard server invites lists them)"
            ));
        }
        entries.push(entry.clone());
        write_store(&store, &entries, owner.uid(), owner.gid()).map_err(store_failure)
    })?;
    let token = Invite {
        url: request.url.to_string(),
        pin: request.pin.map(str::to_string),
        secret,
        machine: request.machine.map(str::to_string),
    }
    .encode();
    Ok(Created {
        entry,
        token,
        url: request.url.to_string(),
        pin: request.pin.map(str::to_string),
    })
}

/// The open invites, oldest first.
pub fn list(local: &Local) -> Result<Vec<Entry>> {
    let owner = registry_owner(local)?;
    let now = now();
    let mut entries = read_store(&local.store(), owner.uid()).map_err(store_failure)?;
    entries.retain(|e| e.open(now));
    entries.sort_by_key(|e| e.created_at);
    Ok(entries)
}

/// Cancel an open invite by its ID.
pub fn cancel(local: &Local, id: &str) -> Result<Entry> {
    let store = local.store();
    locked(local, |owner| {
        let now = now();
        let mut entries = read_store(&store, owner.uid()).map_err(store_failure)?;
        let Some(index) = entries.iter().position(|e| e.id == id && e.open(now)) else {
            return refuse("no open invite has that ID (switchboard server invites lists them)");
        };
        let removed = entries.remove(index);
        prune(&mut entries, now);
        write_store(&store, &entries, owner.uid(), owner.gid()).map_err(store_failure)?;
        Ok(removed)
    })
}

/// An enrolled machine and when the server last heard from it.
pub struct MachineRow {
    pub machine: String,
    pub owner: JsonStr,
    pub last_seen: Option<f64>,
}

/// When each machine last made a request, as the server records it in its
/// database (at most once a minute per machine); empty when unavailable.
pub(super) fn last_seen(db: &str) -> HashMap<String, f64> {
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(connection) = rusqlite::Connection::open_with_flags(db, flags) else {
        return HashMap::new();
    };
    let _ = connection.busy_timeout(std::time::Duration::from_secs(2));
    let Ok(mut statement) = connection.prepare("SELECT machine, at FROM last_seen") else {
        return HashMap::new();
    };
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    });
    match rows {
        Ok(rows) => rows.filter_map(|row| row.ok()).collect(),
        Err(_) => HashMap::new(),
    }
}

/// The enabled machines, by name.
pub fn machines(local: &Local) -> Result<Vec<MachineRow>> {
    registry_owner(local)?;
    let (data, _) = read_file(&local.peers)?;
    let registry = jsonval::loads(&data)
        .and_then(|raw| super::hub::parse_registry(&raw))
        .ok_or_else(|| Failure::Reported("the registry is invalid".into()))?;
    let seen = last_seen(&local.db);
    let mut rows: Vec<MachineRow> = registry
        .values()
        .map(|peer| MachineRow {
            machine: peer.machine.clone(),
            owner: peer.owner.clone(),
            last_seen: seen.get(&peer.machine).copied(),
        })
        .collect();
    rows.sort_by(|a, b| a.machine.cmp(&b.machine));
    Ok(rows)
}

/// The account running this command, for the registry's `revoked_by`.
fn account() -> String {
    // SAFETY: getpwuid returns a static record or NULL, read at once.
    unsafe {
        let entry = libc::getpwuid(libc::geteuid());
        if !entry.is_null() && !(*entry).pw_name.is_null() {
            return std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned();
        }
    }
    "unknown".into()
}

/// Disable an enrolled machine's key; it stops working on its next request.
pub fn revoke(local: &Local, name: &str) -> Result<()> {
    locked(local, |_| {
        let (data, info) = read_file(&local.peers)?;
        let Some(Val::Dict(mut registry)) = jsonval::loads(&data) else {
            return refuse("the registry is invalid");
        };
        if super::hub::parse_registry(&Val::Dict(registry.clone())).is_none() {
            return refuse("the registry is invalid");
        }
        let revoked_at = now();
        let by = format!("{} on the server", account());
        let mut found = false;
        for value in registry.values_mut() {
            if let Val::Dict(entry) = value {
                if get(entry, "enabled") == Some(&Val::Bool(true))
                    && get(entry, "machine").is_some_and(|m| m.is_str(name))
                {
                    jsonval::set(entry, "enabled", false);
                    jsonval::set(entry, "revoked_by", by.as_str());
                    jsonval::set(entry, "revoked_at", revoked_at);
                    found = true;
                }
            }
        }
        if !found {
            return refuse(
                "no machine on this server has that name (switchboard server machines lists them)",
            );
        }
        let mut text = jsonval::dumps(&Val::Dict(registry), jsonval::INDENT_SORTED);
        text.push('\n');
        atomic(&local.peers, text.as_bytes(), &info)
    })
}

// Command lines --------------------------------------------------------------

/// `10m`, `2h`, `1d`, `90s`, or seconds; between a minute and 30 days.
pub fn parse_duration(text: &str) -> Option<i64> {
    let (digits, unit) = match text.char_indices().last()? {
        (index, c) if c.is_ascii_alphabetic() => (&text[..index], c),
        _ => (text, 's'),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 9 {
        return None;
    }
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    let seconds = digits.parse::<i64>().ok()? * scale;
    (60..=MAX_EXPIRY).contains(&seconds).then_some(seconds)
}

/// "10 minutes", "2 hours", "3 days".
pub fn span(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (count, unit) = if (seconds + 59) / 60 < 60 {
        ((seconds + 59) / 60, "minute")
    } else if seconds < 2 * 86400 {
        ((seconds + 1799) / 3600, "hour")
    } else {
        ((seconds + 43199) / 86400, "day")
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

/// The common options and the rest of the arguments.
pub(super) struct Common {
    pub config: String,
    pub json: bool,
    pub rest: Vec<(String, Option<String>)>,
    pub positional: Vec<String>,
}

/// Parse `--config`, `--json`, and the command's own valued options
/// (`valued`), `--name=value` or `--name value`; `Ok(None)` for `--help`.
pub(super) fn parse_common(
    argv: &[String],
    valued: &[&str],
    command: &str,
    usage: &str,
) -> std::result::Result<Option<Common>, String> {
    let mut common = Common {
        config: DEFAULT_CONFIG.into(),
        json: false,
        rest: Vec::new(),
        positional: Vec::new(),
    };
    let mut index = 0;
    let error = |message: String| format!("switchboard server {command}: {message}\n{usage}");
    while index < argv.len() {
        let argument = argv[index].as_str();
        if argument == "-h" || argument == "--help" {
            return Ok(None);
        }
        if !argument.starts_with("--") {
            common.positional.push(argument.to_string());
            index += 1;
            continue;
        }
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };
        if name == "--json" && inline.is_none() {
            common.json = true;
            index += 1;
            continue;
        }
        if name != "--config" && !valued.contains(&name) {
            return Err(error(format!("unknown option {name}")));
        }
        let value = match inline {
            Some(value) => value,
            None => {
                index += 1;
                match argv.get(index) {
                    Some(value) if !value.starts_with("--") => value.clone(),
                    _ => return Err(error(format!("{name} needs a value"))),
                }
            }
        };
        if name == "--config" {
            common.config = value;
        } else {
            common.rest.push((name.to_string(), Some(value)));
        }
        index += 1;
    }
    Ok(Some(common))
}

/// The usage error for leftover arguments.
pub(super) fn unexpected(extra: &[String]) -> String {
    match extra {
        [one] => format!("unexpected argument {one}"),
        _ => format!("unexpected arguments {}", extra.join(" ")),
    }
}

/// A finished action's line, for people.
fn human(value: &str) -> String {
    let theme = crate::cli::ui::Theme::for_fd(1);
    let mut text =
        crate::cli::ui::done(&theme, value, crate::cli::format::output_width()).join("\n");
    text.push('\n');
    text
}

/// A list for people: a bold title with its count, aligned rows (the first
/// cell bold), and what to do next.
fn listing(title: &str, count: &str, rows: &[Vec<String>], next: &str) -> String {
    use crate::cli::format::display_width;
    use crate::cli::ui::{self, Tone};
    let theme = ui::Theme::for_fd(1);
    let width = crate::cli::format::output_width();
    let indent = " ".repeat(ui::INDENT);
    let mut lines = vec![format!("{indent}{}  {count}", theme.strong(title))];
    if !rows.is_empty() {
        lines.push(String::new());
        let columns = rows[0].len();
        let widths: Vec<usize> = (0..columns)
            .map(|i| rows.iter().map(|r| display_width(&r[i])).max().unwrap_or(0))
            .collect();
        for row in rows {
            let mut line = indent.clone();
            for (i, cell) in row.iter().enumerate() {
                line.push_str(&if i == 0 {
                    theme.strong(cell)
                } else {
                    cell.clone()
                });
                if i + 1 < columns {
                    line.push_str(&" ".repeat(widths[i] - display_width(cell) + 2));
                }
            }
            lines.push(line.trim_end().to_string());
        }
    }
    if !next.is_empty() {
        lines.push(String::new());
        lines.extend(ui::para(&theme, next, Tone::Normal, width));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// Print a result (JSON, or the human text) and return the exit status.
fn finish(json: bool, outcome: Result<(Map<String, Value>, String)>, command: &str) -> i32 {
    match outcome {
        Ok((value, text)) => {
            if json {
                let mut all = Map::new();
                all.insert("ok".into(), Value::Bool(true));
                all.extend(value);
                println!("{}", crate::wirejson::dumps(&Value::Object(all)));
            } else {
                print!("{text}");
            }
            0
        }
        Err(Failure::Reported(message) | Failure::Other(message)) => {
            if json {
                let mut value = Map::new();
                value.insert("ok".into(), Value::Bool(false));
                value.insert("error".into(), Value::from(message));
                println!("{}", crate::wirejson::dumps(&Value::Object(value)));
            } else if let Some(rest) = message.strip_prefix("cannot read the server config ") {
                let path = rest.split(';').next().unwrap_or(rest);
                crate::cli::print_problem(
                    &crate::cli::ui::prompt::Problem::new(
                        "This isn't the server, or you can't read its configuration.",
                    )
                    .detail(format!("`switchboard server {command}` reads {path}."))
                    .fix(format!(
                        "run it on the server with sudo: `sudo switchboard server {command}`"
                    )),
                );
            } else {
                crate::cli::print_failure(&message);
            }
            1
        }
    }
}

const INVITE_USAGE: &str =
    "Usage: switchboard server invite [--expires DURATION] [--uses N] [--name MACHINE]
                                 [--url URL] [--config FILE] [--json]
       switchboard server invite revoke ID [--config FILE] [--json]
";
const INVITE_HELP: &str = "
Make an invite for a new machine: on that machine, switchboard init asks for
it (scripts pass it on standard input with --invite -, or in
SWITCHBOARD_INVITE). Run it on the server as root or as the server's service
account. invite revoke ID cancels an invite that has not been used up; the ID
is in the --json output and in switchboard server invites.

Options:
  --expires DURATION how long the invite lasts: 30m, 2h, 7d, or seconds
                     (default: 1h; at least a minute, at most 30 days)
  --uses N           how many machines may join with it (default: 1)
  --name MACHINE     the name the machine joins under (default: the one it
                     asks for); a named invite has one use
  --url URL          the address machines use to reach the server (default:
                     from the server config; needed when Caddy terminates TLS)
  --config FILE      the server config (default: /etc/switchboard/server.json)
  --json             print the result as JSON
  -h, --help         show this help
";

/// `switchboard server invite ...` and `switchboard server invite revoke ID`.
pub fn invite_main(argv: &[String]) -> i32 {
    if argv.first().map(String::as_str) == Some("revoke") {
        return cancel_main(&argv[1..]);
    }
    let parsed = parse_common(
        argv,
        &["--expires", "--uses", "--name", "--url"],
        "invite",
        INVITE_USAGE,
    );
    let common = match parsed {
        Ok(Some(common)) => common,
        Ok(None) => {
            crate::cli::print_help(&format!("{INVITE_USAGE}{INVITE_HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server invite --help");
            return 2;
        }
    };
    let usage = |message: &str| {
        crate::cli::print_usage_text(
            &format!("switchboard server invite: {message}"),
            "switchboard server invite --help",
        );
        2
    };
    if !common.positional.is_empty() {
        return usage(&unexpected(&common.positional));
    }
    let (mut expires, mut uses, mut machine, mut url) = (DEFAULT_EXPIRY, 1, None, None);
    for (name, value) in &common.rest {
        let value = value.clone().unwrap_or_default();
        match name.as_str() {
            "--expires" => match parse_duration(&value) {
                Some(seconds) => expires = seconds,
                None => {
                    return usage(&format!(
                        "--expires must be a duration like 30m, 2h, or 7d (at least a minute, at most 30 days), not '{value}'"
                    ))
                }
            },
            "--uses" => match value.parse::<i64>().ok().filter(|n| (1..=MAX_USES).contains(n)) {
                Some(count) => uses = count,
                None => {
                    return usage(&format!(
                        "--uses must be a count from 1 to {MAX_USES}, not '{value}'"
                    ))
                }
            },
            "--name" => {
                if !invite::is_machine_name(&value) {
                    return usage(&format!(
                        "--name must be a machine name (lowercase letters, digits, and hyphens, starting with a letter, at most 48), not '{value}'"
                    ));
                }
                machine = Some(value);
            }
            _ => {
                // The check `setup` applies to the invite's address, so no
                // invite is made that no machine could use.
                let trimmed = value.trim_end_matches('/');
                if !invite::is_url(trimmed) {
                    return usage(&format!(
                        "--url must be https://HOST or https://HOST:PORT (no path, query, or user), not {}",
                        crate::text::repr_str(&value)
                    ));
                }
                url = Some(trimmed.to_string());
            }
        }
    }
    if machine.is_some() && uses > 1 {
        // The second machine would join under the same name, which the
        // server refuses as already on it, so the other uses could never work.
        crate::cli::print_problem(
            &crate::cli::ui::prompt::Problem::new(format!(
                "An invite with --name enrolls only that one machine, so it can't have {uses} uses."
            ))
            .detail("A second machine joining under the same name is refused as already on this server.")
            .fix(format!(
                "drop --name to let {uses} machines join under their own names, or use --uses 1."
            )),
        );
        return 2;
    }
    // SAFETY: umask cannot fail.
    unsafe { libc::umask(0o077) };
    let outcome = (|| {
        let local = load(&common.config)?;
        let Some(url) = url.or(local.url.clone()) else {
            return refuse(
                "the server config does not say the address machines use (Caddy terminates TLS); pass --url https://NAME",
            );
        };
        if !invite::is_url(&url) {
            return refuse(&format!(
                "the server config gives {url} as the address machines use, which is not https://HOST[:PORT]; pass --url https://NAME"
            ));
        }
        let created = create(
            &local,
            &Request {
                url: &url,
                pin: local.pin.as_deref(),
                expires,
                uses,
                machine: machine.as_deref(),
                source: "command",
                replace: false,
            },
        )?;
        Ok(describe(&created))
    })();
    finish(common.json, outcome, "invite")
}

/// The JSON fields and the text for a new invite.
pub fn describe(created: &Created) -> (Map<String, Value>, String) {
    describe_for(created, false)
}

/// The JSON fields and the text for a new invite; `first`: the first
/// machine's, from an installation (the text then says how to make more).
pub fn describe_for(created: &Created, first: bool) -> (Map<String, Value>, String) {
    let entry = &created.entry;
    let mut value = Map::new();
    value.insert("id".into(), Value::from(entry.id.clone()));
    value.insert("invite".into(), Value::from(created.token.clone()));
    value.insert("url".into(), Value::from(created.url.clone()));
    value.insert(
        "server_pin".into(),
        created.pin.clone().map_or(Value::Null, Value::from),
    );
    value.insert("expires_at".into(), Value::from(entry.expires_at));
    value.insert("uses".into(), Value::from(entry.uses));
    value.insert(
        "machine".into(),
        entry.machine.clone().map_or(Value::Null, Value::from),
    );
    let which = match (&entry.machine, entry.uses, first) {
        (Some(machine), _, _) => format!("the machine {machine}"),
        (None, _, true) => "the first machine".to_string(),
        (None, 1, false) => "a machine".to_string(),
        (None, uses, false) => format!("{uses} machines"),
    };
    let terminal = crate::cli::ui::term::is_tty(1);
    let lines = crate::cli::init::invite_lines(
        &crate::cli::ui::Theme::for_fd(1),
        &crate::cli::init::InviteText {
            invite: &created.token,
            which,
            uses: if entry.uses == 1 {
                "single use".to_string()
            } else {
                format!("{} uses", entry.uses)
            },
            expires_at: Some(entry.expires_at),
            more: first.then_some("sudo switchboard server invite"),
            wrap: terminal,
        },
        crate::cli::ui::term::columns(),
    );
    let mut text = lines.join("\n");
    text.push('\n');
    (value, text)
}

fn cancel_main(argv: &[String]) -> i32 {
    let common = match parse_common(argv, &[], "invite revoke", INVITE_USAGE) {
        Ok(Some(common)) => common,
        Ok(None) => {
            crate::cli::print_help(&format!("{INVITE_USAGE}{INVITE_HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server invite --help");
            return 2;
        }
    };
    let [id] = common.positional.as_slice() else {
        crate::cli::print_usage_text(
            "switchboard server invite revoke: name one invite ID",
            "switchboard server invite --help",
        );
        return 2;
    };
    let outcome = (|| {
        let local = load(&common.config)?;
        let entry = cancel(&local, id)?;
        let mut value = Map::new();
        value.insert("id".into(), Value::from(entry.id.clone()));
        value.insert("state".into(), Value::from("revoked"));
        Ok((
            value,
            human(&format!("Revoked invite {}; it no longer works.", entry.id)),
        ))
    })();
    finish(common.json, outcome, "invite revoke")
}

const LIST_USAGE: &str = "Usage: switchboard server invites [--config FILE] [--json]\n";

/// `switchboard server invites`: the open invites, without secrets.
pub fn invites_main(argv: &[String]) -> i32 {
    let common = match parse_common(argv, &[], "invites", LIST_USAGE) {
        Ok(Some(common)) if common.positional.is_empty() => common,
        Ok(Some(common)) => {
            crate::cli::print_usage_text(
                &format!(
                    "switchboard server invites: {}",
                    unexpected(&common.positional)
                ),
                "switchboard server invites --help",
            );
            return 2;
        }
        Ok(None) => {
            crate::cli::print_help(&format!("{LIST_USAGE}\nList the open invites (never their secrets). Run it on the server.\n"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server invites --help");
            return 2;
        }
    };
    let outcome = (|| {
        let local = load(&common.config)?;
        let entries = list(&local)?;
        let now = now();
        let rows: Vec<Value> = entries
            .iter()
            .map(|e| {
                let mut row = Map::new();
                row.insert("id".into(), Value::from(e.id.clone()));
                row.insert("created_at".into(), Value::from(e.created_at));
                row.insert("expires_at".into(), Value::from(e.expires_at));
                row.insert("uses".into(), Value::from(e.uses));
                row.insert(
                    "machine".into(),
                    e.machine.clone().map_or(Value::Null, Value::from),
                );
                Value::Object(row)
            })
            .collect();
        let text = if entries.is_empty() {
            listing(
                "Open invites",
                "none",
                &[],
                "Make one with\n`sudo switchboard server invite`",
            )
        } else {
            let cells: Vec<Vec<String>> = entries
                .iter()
                .map(|e| {
                    vec![
                        e.id.clone(),
                        e.machine.clone().unwrap_or_else(|| "any name".into()),
                        if e.uses == 1 {
                            "1 use left".to_string()
                        } else {
                            format!("{} uses left", e.uses)
                        },
                        format!("expires in {}", span(e.expires_at - now)),
                    ]
                })
                .collect();
            listing(
                "Open invites",
                &entries.len().to_string(),
                &cells,
                "To revoke one, run\n`switchboard server invite revoke ID`",
            )
        };
        let mut value = Map::new();
        value.insert("invites".into(), Value::Array(rows));
        Ok((value, text))
    })();
    finish(common.json, outcome, "invites")
}

const MACHINES_USAGE: &str = "Usage: switchboard server machines [--config FILE] [--json]\n";

/// `switchboard server machines`: enrolled machines with when each was last seen.
pub fn machines_main(argv: &[String]) -> i32 {
    let common = match parse_common(argv, &[], "machines", MACHINES_USAGE) {
        Ok(Some(common)) if common.positional.is_empty() => common,
        Ok(Some(common)) => {
            crate::cli::print_usage_text(
                &format!(
                    "switchboard server machines: {}",
                    unexpected(&common.positional)
                ),
                "switchboard server machines --help",
            );
            return 2;
        }
        Ok(None) => {
            crate::cli::print_help(&format!(
                "{MACHINES_USAGE}\nList the machines on this server. Run it on the server.\n"
            ));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server machines --help");
            return 2;
        }
    };
    let outcome = (|| {
        let local = load(&common.config)?;
        let rows = machines(&local)?;
        let now = super::hub::now();
        let owner_text = |owner: &JsonStr| String::from_utf8_lossy(owner.as_bytes()).into_owned();
        let json: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut row = Map::new();
                row.insert("machine".into(), Value::from(r.machine.clone()));
                row.insert("owner".into(), Value::from(owner_text(&r.owner)));
                row.insert(
                    "last_seen".into(),
                    r.last_seen.map_or(Value::Null, crate::fmtutil::float_value),
                );
                Value::Object(row)
            })
            .collect();
        let text = if rows.is_empty() {
            listing(
                "Machines",
                "none joined yet",
                &[],
                "Make an invite with\n`sudo switchboard server invite`",
            )
        } else {
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    vec![
                        r.machine.clone(),
                        crate::cli::format::text(&owner_text(&r.owner)),
                        match r.last_seen {
                            Some(at) => crate::cli::format::ago(Some(now - at)),
                            None => "never seen".to_string(),
                        },
                    ]
                })
                .collect();
            listing("Machines", &rows.len().to_string(), &cells, "")
        };
        let mut value = Map::new();
        value.insert("machines".into(), Value::Array(json));
        Ok((value, text))
    })();
    finish(common.json, outcome, "machines")
}

const REVOKE_USAGE: &str = "Usage: switchboard server revoke MACHINE [--config FILE] [--json]\n";

/// `switchboard server revoke NAME`: disable a machine's key.
pub fn revoke_main(argv: &[String]) -> i32 {
    let common = match parse_common(argv, &[], "revoke", REVOKE_USAGE) {
        Ok(Some(common)) => common,
        Ok(None) => {
            crate::cli::print_help(&format!("{REVOKE_USAGE}\nRemove a machine from this server: its key stops working on its next request. Run it on the server.\n"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server revoke --help");
            return 2;
        }
    };
    let [name] = common.positional.as_slice() else {
        crate::cli::print_usage_text(
            "switchboard server revoke: name one machine",
            "switchboard server revoke --help",
        );
        return 2;
    };
    let outcome = (|| {
        let local = load(&common.config)?;
        revoke(&local, name)?;
        let mut value = Map::new();
        value.insert("machine".into(), Value::from(name.clone()));
        value.insert("state".into(), Value::from("revoked"));
        Ok((
            value,
            human(&format!(
                "Revoked {name}. Its key stops working on its next request."
            )),
        ))
    })();
    finish(common.json, outcome, "revoke")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_round_to_the_unit_people_use() {
        assert_eq!(span(3599), "1 hour");
        assert_eq!(span(3600), "1 hour");
        assert_eq!(span(600), "10 minutes");
        assert_eq!(span(59), "1 minute");
        assert_eq!(span(7 * 86400), "7 days");
    }

    #[test]
    fn used_and_expired_invites_are_remembered_for_a_week_at_most() {
        let entry = |id: &str, created_at: i64, expires_at: i64, uses: i64| Entry {
            id: id.into(),
            hash: format!("{id:0<64}"),
            created_at,
            expires_at,
            uses,
            machine: None,
            source: "command".into(),
        };
        let now = 10 * 86400;
        let mut entries = vec![
            entry("open", now - 10, now + 600, 1),
            entry("used", now - 20, now + 600, 0),
            entry("expired", now - 5000, now - 1000, 1),
            entry("old", 0, 100, 1),
        ];
        prune(&mut entries, now);
        let kept: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(kept, ["open", "used", "expired"]);
        let mut many: Vec<Entry> = (0..200)
            .map(|n| entry(&format!("c{n:03}"), now - 1000 + n, now - 1, 0))
            .collect();
        prune(&mut many, now);
        assert_eq!(many.len(), MAX_CLOSED);
        assert_eq!(many.first().unwrap().id, "c072");
    }
    use std::os::unix::fs::PermissionsExt;

    /// A server config in a private directory: private mode, with a
    /// certificate file for the pin, a registry with one machine, and a
    /// server state over the same files for redeeming.
    struct Server {
        root: String,
        config: String,
    }

    impl Server {
        fn new(tls: &str) -> Server {
            let template = std::ffi::CString::new("/tmp/sbi-XXXXXX").unwrap();
            let mut raw = template.into_bytes_with_nul();
            // SAFETY: raw is a writable NUL-terminated template.
            assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
            raw.pop();
            let root = std::fs::canonicalize(String::from_utf8(raw).unwrap())
                .unwrap()
                .to_string_lossy()
                .into_owned();
            std::fs::create_dir_all(format!("{root}/state/tls")).unwrap();
            let identity = super::super::tls::private_identity(
                &format!("{root}/state/tls"),
                &["switchboard.lan".to_string()],
            )
            .unwrap();
            drop(identity);
            crate::server::write_private(
                &format!("{root}/peers.json"),
                format!(
                    r#"{{"{}": {{"machine": "old-laptop", "owner": "krish", "enabled": true, "admin": true}}}}"#,
                    "a".repeat(64)
                )
                .as_bytes(),
            )
            .unwrap();
            let config = format!("{root}/server.json");
            let body = match tls {
                "private" => {
                    r#"{"tls": "private", "names": ["switchboard.lan", "10.0.0.5"], "db": "state/messages.db", "peers_file": "peers.json"}"#
                }
                "acme" => {
                    r#"{"tls": "acme", "hostname": "sb.example.com", "port": 8443, "db": "state/messages.db", "peers_file": "peers.json"}"#
                }
                _ => {
                    r#"{"unix_socket": "/run/x.sock", "db": "state/messages.db", "peers_file": "peers.json"}"#
                }
            };
            crate::server::write_private(&config, body.as_bytes()).unwrap();
            Server { root, config }
        }

        fn local(&self) -> Local {
            load(&self.config).unwrap()
        }

        fn hub(&self) -> super::super::hub::Hub {
            super::super::hub::Hub::open(super::super::hub::Config {
                db: format!("{}/state/messages.db", self.root),
                peers_file: format!("{}/peers.json", self.root),
                registry_lock: None,
            })
            .unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn request<'a>(local: &'a Local, uses: i64, machine: Option<&'a str>) -> Request<'a> {
        Request {
            url: local.url.as_deref().unwrap_or("https://x"),
            pin: local.pin.as_deref(),
            expires: DEFAULT_EXPIRY,
            uses,
            machine,
            source: "command",
            replace: false,
        }
    }

    fn redeem(hub: &super::super::hub::Hub, fp: char, machine: &str, token: &str) -> bool {
        let invite = Invite::parse(token).unwrap();
        let body = jsonval::loads(
            format!(
                r#"{{"machine": "{machine}", "owner": "sam", "invite": "{}"}}"#,
                invite.secret
            )
            .as_bytes(),
        )
        .unwrap();
        hub.enroll_unknown(&fp.to_string().repeat(64), "192.0.2.1", &body)
            .is_ok()
    }

    #[test]
    fn the_config_gives_the_address_and_pin() {
        let private = Server::new("private");
        let local = private.local();
        assert_eq!(local.url.as_deref(), Some("https://switchboard.lan:8743"));
        let pem = std::fs::read(format!("{}/state/tls/private-cert.pem", private.root)).unwrap();
        use rustls::pki_types::pem::PemObject;
        let der = rustls::pki_types::CertificateDer::from_pem_slice(&pem).unwrap();
        assert_eq!(
            local.pin.as_deref(),
            Some(super::super::tls::hex(&sha2::Sha256::digest(der.as_ref())).as_str())
        );
        assert_eq!(local.db, format!("{}/state/messages.db", private.root));
        assert_eq!(local.lock, format!("{}/peers.json.lock", private.root));
        let acme = Server::new("acme").local();
        assert_eq!(
            (acme.url.as_deref(), acme.pin),
            (Some("https://sb.example.com:8443"), None)
        );
        let caddy = Server::new("caddy").local();
        assert_eq!((caddy.url, caddy.pin), (None, None));
        assert!(load("/nonexistent/server.json").is_err());
        assert_eq!(host_url("::1", 8743, 443), "https://[::1]:8743");
        assert_eq!(host_url("a.b", 443, 443), "https://a.b");
    }

    #[test]
    fn a_made_invite_is_a_token_the_server_redeems_once() {
        let server = Server::new("private");
        let local = server.local();
        let created = create(&local, &request(&local, 1, None)).unwrap();
        let invite = Invite::parse(&created.token).unwrap();
        assert_eq!(invite.url, "https://switchboard.lan:8743");
        assert_eq!(invite.pin, local.pin);
        assert_eq!(invite.machine, None);
        assert_eq!(created.entry.id, &invite::secret_hash(&invite.secret)[..12]);
        // Only the hash is stored, private.
        let store = std::fs::read_to_string(local.store()).unwrap();
        assert!(!store.contains(&invite.secret));
        assert!(store.contains(&created.entry.hash));
        let info = std::fs::symlink_metadata(local.store()).unwrap();
        assert_eq!(info.mode() & 0o7777, 0o600);
        assert_eq!(list(&local).unwrap(), vec![created.entry.clone()]);
        let hub = server.hub();
        assert!(redeem(&hub, 'c', "new-box", &created.token));
        assert!(!redeem(&hub, 'd', "other-box", &created.token));
        assert!(list(&local).unwrap().is_empty());
        let rows = machines(&local).unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.machine.as_str()).collect();
        assert_eq!(names, ["new-box", "old-laptop"]);
    }

    #[test]
    fn invites_are_listed_cancelled_and_bounded() {
        let server = Server::new("acme");
        let local = server.local();
        // A fixed name enrolls one machine, so it can't have more uses.
        assert!(create(&local, &request(&local, 2, Some("laptop"))).is_err());
        assert!(list(&local).unwrap().is_empty());
        let fixed = create(&local, &request(&local, 1, Some("laptop"))).unwrap();
        let other = create(&local, &request(&local, 2, None)).unwrap();
        let listed = list(&local).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].machine.as_deref(), Some("laptop"));
        assert_eq!(listed[0].uses, 1);
        assert_eq!(listed[1].uses, 2);
        assert!(Invite::parse(&fixed.token).unwrap().pin.is_none());
        assert!(cancel(&local, "000000000000").is_err());
        assert_eq!(cancel(&local, &other.entry.id).unwrap().id, other.entry.id);
        assert!(cancel(&local, &other.entry.id).is_err());
        let hub = server.hub();
        assert!(!redeem(&hub, 'c', "new-box", &other.token));
        // The name an invite fixes is the only one it enrolls.
        assert!(!redeem(&hub, 'c', "new-box", &fixed.token));
        assert!(redeem(&hub, 'c', "laptop", &fixed.token));
        // Both are used up or cancelled, so MAX_OPEN more fill the store.
        for _ in 0..MAX_OPEN {
            create(&local, &request(&local, 1, None)).unwrap();
        }
        assert!(create(&local, &request(&local, 1, None)).is_err());
        // The installer's invite replaces the one an earlier run printed.
        let install = |local: &Local| {
            create(
                local,
                &Request {
                    source: "install",
                    replace: true,
                    ..request(local, 1, None)
                },
            )
        };
        cancel(&local, &list(&local).unwrap()[0].id).unwrap();
        let first = install(&local).unwrap();
        cancel(&local, &list(&local).unwrap()[0].id).unwrap();
        let second = install(&local).unwrap();
        let ids: Vec<String> = list(&local).unwrap().into_iter().map(|e| e.id).collect();
        assert!(!ids.contains(&first.entry.id) && ids.contains(&second.entry.id));
    }

    #[test]
    fn revoking_a_machine_disables_its_key_at_once() {
        let server = Server::new("private");
        let local = server.local();
        let hub = server.hub();
        assert_eq!(hub.auth(&"a".repeat(64)).unwrap().machine, "old-laptop");
        assert!(revoke(&local, "nobody").is_err());
        revoke(&local, "old-laptop").unwrap();
        assert!(hub.auth(&"a".repeat(64)).is_err());
        let registry = jsonval::loads(&std::fs::read(&local.peers).unwrap()).unwrap();
        let entry = get(registry.as_dict().unwrap(), &"a".repeat(64))
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        assert_eq!(get(&entry, "enabled"), Some(&Val::Bool(false)));
        assert!(get(&entry, "revoked_at").is_some());
        assert!(get(&entry, "revoked_by")
            .and_then(Val::as_str)
            .is_some_and(|by| by.ends_with(" on the server")));
        let info = std::fs::symlink_metadata(&local.peers).unwrap();
        assert_eq!(info.mode() & 0o7777, 0o600);
        assert!(revoke(&local, "old-laptop").is_err());
        // The name is free for a new key.
        let created = create(&local, &request(&local, 1, None)).unwrap();
        assert!(redeem(&hub, 'c', "old-laptop", &created.token));
        assert!(machines(&local)
            .unwrap()
            .iter()
            .any(|r| r.machine == "old-laptop"));
    }

    #[test]
    fn machines_show_when_the_server_last_saw_them() {
        let server = Server::new("private");
        let local = server.local();
        assert_eq!(machines(&local).unwrap()[0].last_seen, None);
        let hub = server.hub();
        hub.request(&"a".repeat(64), "/health", None).unwrap();
        let seen = machines(&local).unwrap()[0].last_seen.unwrap();
        assert!((super::super::hub::now() - seen).abs() < 5.0);
    }

    #[test]
    fn unsafe_stores_and_registries_are_refused() {
        let server = Server::new("private");
        let local = server.local();
        create(&local, &request(&local, 1, None)).unwrap();
        std::fs::set_permissions(local.store(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(list(&local).is_err());
        assert!(create(&local, &request(&local, 1, None)).is_err());
        std::fs::remove_file(local.store()).unwrap();
        std::os::unix::fs::symlink(&local.peers, local.store()).unwrap();
        assert!(create(&local, &request(&local, 1, None)).is_err());
        std::fs::remove_file(local.store()).unwrap();
        std::fs::set_permissions(&local.peers, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(create(&local, &request(&local, 1, None)).is_err());
        assert!(revoke(&local, "old-laptop").is_err());
        std::fs::set_permissions(&local.peers, std::fs::Permissions::from_mode(0o600)).unwrap();
        create(&local, &request(&local, 1, None)).unwrap();
        let names: Vec<String> = std::fs::read_dir(format!("{}/state", server.root))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
    }

    #[test]
    fn stores_round_trip_and_fail_closed() {
        let entry = Entry {
            id: "0123456789ab".into(),
            hash: format!("0123456789ab{}", "c".repeat(52)),
            created_at: 100,
            expires_at: 700,
            uses: 2,
            machine: Some("laptop".into()),
            source: "command".into(),
        };
        let text = render_store(std::slice::from_ref(&entry));
        assert_eq!(parse_store(text.as_bytes()).unwrap(), vec![entry.clone()]);
        assert!(entry.open(699) && !entry.open(700));
        assert!(!Entry {
            uses: 0,
            ..entry.clone()
        }
        .open(100));
        let mangled = [
            text.replace("0123456789ab\"", "0123456789ac\""),
            text.replace("\"uses\": 2", "\"uses\": -1"),
            text.replace("\"uses\": 2", "\"uses\": \"2\""),
            text.replace("\"laptop\"", "\"Laptop\""),
            text.replace(
                "\"source\": \"command\"",
                "\"source\": \"command\", \"x\": 1",
            ),
            "[]".into(),
        ];
        for bad in &mangled {
            assert!(parse_store(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("30d"), Some(30 * 86400));
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("90s"), Some(90));
        for bad in [
            "",
            "m",
            "59",
            "31d",
            "1w",
            "-5m",
            "1.5h",
            "10 m",
            "9999999999",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(span(600), "10 minutes");
        assert_eq!(span(60), "1 minute");
        assert_eq!(span(3600), "1 hour");
        assert_eq!(span(7 * 86400), "7 days");
    }
}
