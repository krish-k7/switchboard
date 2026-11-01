//! The server's state: enrolled machines, presence, queued events, invite
//! redemption, rate limits, and the SQLite message store.
//!
//! One mutex and one condition variable guard everything; long polls and
//! pings wait on that condition until their deadlines.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use indexmap::IndexMap;
use rusqlite::types::{Value as Sql, ValueRef};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::jsonval::{self, dict, get, has_other_keys, set, Dict, JsonStr, Val};
use crate::fsutil;
use crate::identity::{monotonic, uuid4, uuid4_hex};

#[cfg(test)]
thread_local! {
    /// Seconds added to both clocks, so tests can step over timers.
    static SHIFT: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

fn shift() -> f64 {
    #[cfg(test)]
    return SHIFT.with(std::cell::Cell::get);
    #[cfg(not(test))]
    0.0
}

/// Monotonic seconds (shifted in tests).
fn mono() -> f64 {
    monotonic() + shift()
}

pub const MAX_BODY: usize = 65536;
const LEASE: f64 = 30.0;
const PING_DEADLINE: f64 = 5.0;
/// Optional bridge features; machines that do not advertise them keep
/// consume-on-poll delivery and never receive ping events.
const CAPABILITIES: [&str; 2] = ["ack", "ping"];
const UNKNOWN_BURST: f64 = 20.0;
const UNKNOWN_REFILL: f64 = 3.0;
const REGISTRY_LIMIT: u64 = 1_048_576;
const QUEUE_LIMIT: usize = 256;
/// Requests that wait (long polls and pings) may hold at most this many of
/// the 128 connection handlers; the rest stay free for short requests.
pub const WAITING_LIMIT: usize = 96;
/// Why a message that was in flight when the server started is unknown.
const RESTART_DETAIL: &str = "the server restarted before delivery was confirmed";

/// How a request fails, and so how it is answered.
#[derive(Debug)]
pub enum Fail {
    /// `Error(code, status)`: answered as `{"ok": false, "error": code}`.
    Api(&'static str, u16),
    /// Text that cannot be encoded as UTF-8 (a lone surrogate): `400 invalid_unicode`.
    Unicode,
    /// A database or file-system error: `503 unavailable`.
    Unavailable,
    /// Any other failure: the connection closes without a response.
    Abort(String),
}

pub type Out<T> = Result<T, Fail>;

fn err<T>(code: &'static str, status: u16) -> Out<T> {
    Err(Fail::Api(code, status))
}

fn db_fail(exc: rusqlite::Error) -> Fail {
    let _ = exc;
    Fail::Unavailable
}

/// Wall-clock seconds since the epoch (shifted in tests).
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        + shift()
}

/// Round to the nearest integer, ties to even.
pub fn round_even(value: f64) -> i64 {
    value.round_ties_even() as i64
}

/// Plain fsync(2).
fn fsync(file: &File) -> Out<()> {
    // SAFETY: file is an open descriptor.
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(Fail::Unavailable);
    }
    Ok(())
}

/// A canonical lowercase UUID string.
pub fn identifier(value: Option<&Val>) -> Out<String> {
    match value.and_then(Val::as_str) {
        Some(text) if canonical_uuid(text) => Ok(text.to_string()),
        _ => err("invalid_uuid", 400),
    }
}

fn canonical_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// A non-empty string of at most `maximum` code points
/// without C0 controls or DEL.
pub fn label(value: Option<&Val>, maximum: usize) -> Out<JsonStr> {
    match value.and_then(Val::as_json_str) {
        Some(text)
            if !text.is_empty()
                && text.char_len() <= maximum
                && !text.code_points().any(|c| c < 32 || c == 127) =>
        {
            Ok(text.clone())
        }
        _ => err("invalid_label", 400),
    }
}

fn is_host(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One enabled registry entry. An `admin` key older registries carry is
/// ignored: there are no administrator rights.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub machine: String,
    pub owner: JsonStr,
}

pub type Enrollments = Arc<IndexMap<String, Peer>>;

/// Enabled identities by fingerprint; `None` when the registry is malformed
/// (a wrong type, a missing field, or an invalid value).
pub fn parse_registry(raw: &Val) -> Option<IndexMap<String, Peer>> {
    let map = raw.as_dict()?;
    if map.len() > 1024 {
        return None;
    }
    let mut enabled = IndexMap::new();
    let mut names = std::collections::HashSet::new();
    for (fingerprint, peer) in map {
        let peer = peer.as_dict()?;
        if get(peer, "enabled") != Some(&Val::Bool(true)) {
            continue;
        }
        let machine = get(peer, "machine")?;
        let machine = machine.as_str().filter(|m| is_host(m))?;
        let fingerprint = fingerprint.as_str().filter(|fp| is_hex(fp, 64))?;
        if names.contains(machine) {
            return None;
        }
        let owner = label(Some(get(peer, "owner")?), 64).ok()?;
        names.insert(machine.to_string());
        enabled.insert(
            fingerprint.to_string(),
            Peer {
                machine: machine.to_string(),
                owner,
            },
        );
    }
    Some(enabled)
}

fn parse_route(route: Option<&Val>) -> Out<(String, String)> {
    let Some(route) = route.and_then(Val::as_json_str) else {
        return err("invalid_route", 400);
    };
    // '/' never occurs inside a multi-byte sequence, so splitting bytes is
    // splitting code points; a surrogate fails the host or UUID pattern.
    let bytes = route.as_bytes();
    if bytes.iter().filter(|b| **b == b'/').count() != 1 {
        return err("invalid_route", 400);
    }
    let slash = bytes.iter().position(|b| *b == b'/').expect("one slash");
    let (host, sid) = (&bytes[..slash], &bytes[slash + 1..]);
    let host = match std::str::from_utf8(host) {
        Ok(host) if is_host(host) => host,
        _ => return err("invalid_route", 400),
    };
    match std::str::from_utf8(sid) {
        Ok(sid) if canonical_uuid(sid) => Ok((host.to_string(), sid.to_string())),
        _ => err("invalid_uuid", 400),
    }
}

/// The UTF-8 length in bytes; text with a lone surrogate fails as `Unicode`.
fn encoded_len(text: &JsonStr) -> Out<usize> {
    text.as_str().map(str::len).ok_or(Fail::Unicode)
}

#[derive(Default)]
struct Machine {
    sessions: IndexMap<String, Dict>,
    seen: f64,
    queue: VecDeque<Dict>,
    inflight: IndexMap<String, Dict>,
    ack: bool,
    ping: bool,
    registration: Option<String>,
    /// Stored notices already in `queue` or `inflight`, by message id (the
    /// notices table's key; its rowids can be reused once a row is replaced
    /// or deleted, so they never identify a notice).
    notices: std::collections::HashSet<String>,
}

struct Limit {
    tokens: f64,
    time: f64,
    active: i64,
    polls: i64,
}

struct Ping {
    machine: String,
    result: Option<(bool, f64)>,
}

/// Server configuration the state needs.
pub struct Config {
    pub db: String,
    pub peers_file: String,
    /// The config's `registry_lock` as given (`None` when absent or falsy).
    pub registry_lock: Option<Val>,
}

pub struct State {
    config: Config,
    db: Connection,
    machines: IndexMap<String, Machine>,
    limits: HashMap<String, Limit>,
    enrollment_key: Option<(u64, u64, u64, i128, i128)>,
    enrollment_cache: Option<Enrollments>,
    roster_dirty: bool,
    roster: Vec<Dict>,
    revision: String,
    next_expiration: f64,
    next_retention: f64,
    pings: IndexMap<String, Ping>,
    unknown_limits: HashMap<String, (f64, f64)>,
    /// Requests holding a handler while they wait (polls and pings).
    waiting: usize,
    /// Machines that may have stored notices not yet in their queue.
    notice_hosts: std::collections::HashSet<String>,
    /// When each machine's last request was written to `last_seen`.
    seen_recorded: HashMap<String, f64>,
}

pub struct Hub {
    state: Mutex<State>,
    cv: Condvar,
}

type Guard<'a> = MutexGuard<'a, State>;

fn sql_text(value: &Sql) -> Out<&str> {
    match value {
        Sql::Text(text) => Ok(text),
        other => Err(Fail::Abort(format!(
            "expected text in the message store, found {other:?}"
        ))),
    }
}

fn sql_num(value: &Sql) -> Out<f64> {
    match value {
        Sql::Real(number) => Ok(*number),
        Sql::Integer(number) => Ok(*number as f64),
        other => Err(Fail::Abort(format!(
            "expected a number in the message store, found {other:?}"
        ))),
    }
}

fn sql_val(value: &Sql) -> Out<Val> {
    Ok(match value {
        Sql::Null => Val::Null,
        Sql::Integer(number) => Val::Int(number.to_string()),
        Sql::Real(number) => Val::Float(*number),
        Sql::Text(text) => Val::from(text.as_str()),
        Sql::Blob(_) => return Err(Fail::Abort("bytes are not JSON serializable".into())),
    })
}

/// Read a row's columns as owned values; text that is not UTF-8 is a
/// database error.
fn row_values(row: &rusqlite::Row<'_>, count: usize) -> rusqlite::Result<Vec<Sql>> {
    (0..count)
        .map(|index| {
            Ok(match row.get_ref(index)? {
                ValueRef::Null => Sql::Null,
                ValueRef::Integer(number) => Sql::Integer(number),
                ValueRef::Real(number) => Sql::Real(number),
                ValueRef::Text(bytes) => {
                    Sql::Text(String::from_utf8(bytes.to_vec()).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            index,
                            "text".into(),
                            rusqlite::types::Type::Text,
                        )
                    })?)
                }
                ValueRef::Blob(bytes) => Sql::Blob(bytes.to_vec()),
            })
        })
        .collect()
}

impl State {
    fn query(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
        columns: usize,
    ) -> Out<Vec<Vec<Sql>>> {
        let mut statement = self.db.prepare_cached(sql).map_err(db_fail)?;
        let rows = statement
            .query_map(params, |row| row_values(row, columns))
            .map_err(db_fail)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_fail)
    }

    /// A data-changing statement inside an implicit transaction: one is opened
    /// before the first change and the next commit ends it.
    fn change(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Out<usize> {
        if self.db.is_autocommit() {
            self.db.execute_batch("BEGIN").map_err(db_fail)?;
        }
        let mut statement = self.db.prepare_cached(sql).map_err(db_fail)?;
        statement.execute(params).map_err(db_fail)
    }

    fn commit(&self) -> Out<()> {
        if !self.db.is_autocommit() {
            self.db.execute_batch("COMMIT").map_err(db_fail)?;
        }
        Ok(())
    }

    fn file_key(info: &std::fs::Metadata) -> (u64, u64, u64, i128, i128) {
        (
            info.dev(),
            info.ino(),
            info.size(),
            info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128,
            info.ctime() as i128 * 1_000_000_000 + info.ctime_nsec() as i128,
        )
    }

    fn read_enrollments(&mut self) -> Option<Enrollments> {
        let path = self.config.peers_file.clone();
        let before = std::fs::symlink_metadata(&path).ok()?;
        let key = Self::file_key(&before);
        if !before.file_type().is_file() || before.size() > REGISTRY_LIMIT {
            return None;
        }
        if Some(key) == self.enrollment_key {
            return self.enrollment_cache.clone();
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .ok()?;
        if Self::file_key(&file.metadata().ok()?) != key {
            return None;
        }
        let mut data = Vec::new();
        (&file)
            .take(REGISTRY_LIMIT + 1)
            .read_to_end(&mut data)
            .ok()?;
        let after_read = Self::file_key(&file.metadata().ok()?);
        drop(file);
        if data.len() as u64 > REGISTRY_LIMIT
            || after_read != key
            || Self::file_key(&std::fs::symlink_metadata(&path).ok()?) != key
        {
            return None;
        }
        let cache = Arc::new(parse_registry(&jsonval::loads(&data)?)?);
        if self.enrollment_cache.as_deref() != Some(&*cache) {
            self.roster_dirty = true;
        }
        self.enrollment_cache = Some(cache.clone());
        self.enrollment_key = Some(key);
        Some(cache)
    }

    pub fn enrollments(&mut self) -> Out<Enrollments> {
        match self.read_enrollments() {
            Some(cache) => Ok(cache),
            None => {
                self.enrollment_key = None;
                self.enrollment_cache = None;
                self.roster_dirty = true;
                err("enrollment_unavailable", 503)
            }
        }
    }

    fn auth(&mut self, fingerprint: &str) -> Out<Peer> {
        match self.enrollments()?.get(fingerprint) {
            Some(peer) => Ok(peer.clone()),
            None => err("not_enrolled", 403),
        }
    }

    fn enrolled(&mut self, fingerprint: &str) -> Out<bool> {
        Ok(self.enrollments()?.contains_key(fingerprint))
    }

    fn enabled_machines(&mut self) -> Out<std::collections::HashSet<String>> {
        Ok(self
            .enrollments()?
            .values()
            .map(|p| p.machine.clone())
            .collect())
    }

    fn registry_lock(&self) -> Out<String> {
        match &self.config.registry_lock {
            None => Ok(format!("{}.lock", self.config.peers_file)),
            Some(Val::Str(text)) => match text.as_str() {
                Some(path) => Ok(fsutil::normalize(path)),
                None => Err(Fail::Unicode),
            },
            Some(other) => Err(Fail::Abort(format!(
                "expected str, bytes or os.PathLike object, not {}",
                jsonval::dumps(other, jsonval::DEFAULT)
            ))),
        }
    }

    /// Take the registry lock the root helper and the local commands also
    /// take; it is held until the returned file is dropped.
    fn lock_registry(&self) -> Out<File> {
        let lock_path = self.registry_lock()?;
        let os = |_exc: std::io::Error| Fail::Unavailable;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(os)?;
        let info = lock.metadata().map_err(os)?;
        // SAFETY: geteuid cannot fail.
        let euid = unsafe { libc::geteuid() };
        if !info.file_type().is_file()
            || info.nlink() != 1
            || info.uid() != euid
            || info.mode() & 0o7777 != 0o600
        {
            return err("enrollment_unavailable", 503);
        }
        // SAFETY: lock is an open descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Fail::Unavailable);
        }
        Ok(lock)
    }

    /// Apply `mutate` to the registry under the registry lock, keeping the
    /// file's owner and group, and replace it atomically.
    #[cfg(test)]
    fn update_registry(&mut self, mutate: impl FnOnce(&mut Dict)) -> Out<()> {
        let lock = self.lock_registry()?;
        self.rewrite_registry(mutate)?;
        drop(lock);
        Ok(())
    }

    /// The registry rewrite itself; the caller holds the registry lock.
    fn rewrite_registry(&mut self, mutate: impl FnOnce(&mut Dict)) -> Out<()> {
        let peer_file = self.config.peers_file.clone();
        let os = |_exc: std::io::Error| Fail::Unavailable;
        let (mut registry, current) = (|| {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&peer_file)
                .ok()?;
            let current = file.metadata().ok()?;
            if !current.file_type().is_file()
                || current.nlink() != 1
                || current.size() > REGISTRY_LIMIT
            {
                return None;
            }
            let mut data = Vec::new();
            (&file)
                .take(REGISTRY_LIMIT + 1)
                .read_to_end(&mut data)
                .ok()?;
            let registry = jsonval::loads(&data)?;
            parse_registry(&registry)?;
            match registry {
                Val::Dict(map) => Some((map, current)),
                _ => None,
            }
        })()
        .ok_or(Fail::Api("enrollment_unavailable", 503))?;
        mutate(&mut registry);
        let registry = Val::Dict(registry);
        if parse_registry(&registry).is_none() {
            return err("invalid_registry_change", 409);
        }
        let name = peer_file.rsplit('/').next().unwrap_or(&peer_file);
        let directory = fsutil::parent(&peer_file);
        let temporary = fsutil::join(&directory, &format!(".{name}.{}", uuid4_hex()));
        let result = (|| -> Out<()> {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)
                .map_err(os)?;
            let created = out.metadata().map_err(os)?;
            if (created.uid(), created.gid()) != (current.uid(), current.gid()) {
                // The service's group differs from the registry's; keep the
                // original owner so the root helper and installer still accept
                // the file. Without the group, write anyway rather than fail.
                // SAFETY: out is an open descriptor.
                let changed =
                    unsafe { libc::fchown(out.as_raw_fd(), current.uid(), current.gid()) };
                if changed != 0 {
                    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    if code != libc::EPERM && code != libc::EACCES {
                        return Err(Fail::Unavailable);
                    }
                    let line = format!(
                        "{{\"event\": \"registry_group_not_kept\", \"gid\": {}}}\n",
                        current.gid()
                    );
                    let _ = std::io::stderr().write_all(line.as_bytes());
                }
            }
            let mut text = jsonval::dumps(&registry, jsonval::INDENT_SORTED);
            text.push('\n');
            out.write_all(text.as_bytes()).map_err(os)?;
            out.flush().map_err(os)?;
            fsync(&out)?;
            drop(out);
            std::fs::rename(&temporary, &peer_file).map_err(os)?;
            let dir = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(&directory)
                .map_err(os)?;
            fsync(&dir)?;
            Ok(())
        })();
        if std::fs::symlink_metadata(&temporary).is_ok() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        self.enrollments()?;
        Ok(())
    }

    /// Enroll `fingerprint` as `machine` with an invite's secret. Under the
    /// registry lock (which the local commands take too) the invite is
    /// checked and one use taken from it before the registry gains the key,
    /// so concurrent redemptions of a single-use invite enroll one machine.
    /// An unknown secret, or a machine name other than the one the invite
    /// fixes, gets `invalid_invite`; a known one that was used up or expired
    /// (remembered for a week) gets `invite_used` or `invite_expired`.
    fn redeem(
        &mut self,
        cv: &Condvar,
        fingerprint: &str,
        machine: &str,
        owner: JsonStr,
        secret: &str,
    ) -> Out<Val> {
        if !crate::invite::is_secret(secret) {
            return err("invalid_invite", 403);
        }
        let lock = self.lock_registry()?;
        let path = super::invites::store_path(&self.config.db);
        // SAFETY: geteuid cannot fail.
        let euid = unsafe { libc::geteuid() };
        let entries = match super::invites::read_store(&path, euid) {
            Ok(entries) => entries,
            Err(message) => {
                let line = format!(
                    "{{\"event\": \"invite_store_unavailable\", \"detail\": {}}}\n",
                    jsonval::dumps(&Val::from(message), jsonval::DEFAULT)
                );
                let _ = std::io::stderr().write_all(line.as_bytes());
                return err("enrollment_unavailable", 503);
            }
        };
        let hash = crate::invite::secret_hash(secret);
        let clock = super::invites::now();
        let found = entries.iter().position(|entry| {
            entry.hash.len() == hash.len()
                && entry
                    .hash
                    .bytes()
                    .zip(hash.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        });
        let Some(index) = found else {
            return err("invalid_invite", 403);
        };
        if entries[index]
            .machine
            .as_deref()
            .is_some_and(|fixed| fixed != machine)
        {
            return err("invalid_invite", 403);
        }
        // The holder of the secret may learn what became of it, for as long
        // as the store remembers it.
        if !super::invites::remembered(&entries[index], clock) {
            return err("invalid_invite", 403);
        }
        if entries[index].uses <= 0 {
            return err("invite_used", 403);
        }
        if !entries[index].open(clock) {
            return err("invite_expired", 403);
        }
        if self.enabled_machines()?.contains(machine) {
            return err("machine_taken", 409);
        }
        let id = entries[index].id.clone();
        let mut remaining = entries.clone();
        remaining[index].uses -= 1;
        super::invites::prune(&mut remaining, clock);
        let store_owner = std::fs::symlink_metadata(&path)
            .map(|info| (info.uid(), info.gid()))
            // SAFETY: getegid cannot fail.
            .unwrap_or((euid, unsafe { libc::getegid() }));
        let write = |entries: &[super::invites::Entry]| {
            super::invites::write_store(&path, entries, store_owner.0, store_owner.1)
                .map_err(|_| Fail::Unavailable)
        };
        write(&remaining)?;
        let (key, name, invite_id) = (fingerprint.to_string(), machine.to_string(), id.clone());
        let enrolled = self.rewrite_registry(move |registry| {
            registry.insert(
                JsonStr::new(&key),
                Val::Dict(dict([
                    ("machine", name.as_str().into()),
                    ("owner", owner.into()),
                    ("enabled", true.into()),
                    ("invite", invite_id.as_str().into()),
                    ("enrolled_at", clock.into()),
                ])),
            );
        });
        if let Err(fail) = enrolled {
            // Give the use back: the machine did not join.
            let _ = write(&entries);
            return Err(fail);
        }
        drop(lock);
        self.record_seen(machine);
        let line = format!(
            "{{\"event\": \"invite_redeemed\", \"invite\": \"{id}\", \"machine\": {}}}\n",
            jsonval::dumps(&Val::from(machine.to_string()), jsonval::DEFAULT)
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        cv.notify_all();
        Ok(Val::Dict(dict([
            ("ok", true.into()),
            ("state", "approved".into()),
            ("machine", machine.into()),
        ])))
    }

    /// Note that `machine` made a request, in the `last_seen` table that
    /// `switchboard server machines` reads; at most once a minute per
    /// machine, and never inside another request's transaction.
    fn record_seen(&mut self, machine: &str) {
        let at = now();
        if self
            .seen_recorded
            .get(machine)
            .is_some_and(|recorded| (0.0..60.0).contains(&(at - recorded)))
            || !self.db.is_autocommit()
        {
            return;
        }
        let written = self.db.execute(
            "INSERT OR REPLACE INTO last_seen (machine, at) VALUES (?, ?)",
            rusqlite::params![machine, at],
        );
        if written.is_ok() {
            if self.seen_recorded.len() > 2048 {
                self.seen_recorded.clear();
            }
            self.seen_recorded.insert(machine.to_string(), at);
        }
    }

    fn unknown_allowed(&mut self, address: &str) -> Out<()> {
        let now = mono();
        if self.unknown_limits.len() > 1024 {
            self.unknown_limits
                .retain(|_, (_, time)| now - *time <= 600.0);
            if self.unknown_limits.len() > 1024 {
                return err("rate_limited", 429);
            }
        }
        let limit = self
            .unknown_limits
            .entry(address.to_string())
            .or_insert((UNKNOWN_BURST, now));
        limit.0 = UNKNOWN_BURST.min(limit.0 + (now - limit.1) / UNKNOWN_REFILL);
        limit.1 = now;
        if limit.0 < 1.0 {
            return err("rate_limited", 429);
        }
        limit.0 -= 1.0;
        Ok(())
    }

    pub fn cleanup(&mut self, cv: &Condvar) -> Out<()> {
        let now = now();
        let active = self.enabled_machines()?;
        let gone: Vec<String> = self
            .machines
            .iter()
            .filter(|(name, state)| !(active.contains(*name) && now - state.seen <= LEASE))
            .map(|(name, _)| name.clone())
            .collect();
        let mut changed = false;
        for name in gone {
            if let Some(state) = self.machines.shift_remove(&name) {
                // Status observations it had not acknowledged wait for its return.
                changed |= self.keep_statuses(state)?;
            }
            self.roster_dirty = true;
        }
        let clock = mono();
        if clock >= self.next_expiration {
            let rows = self.query(
                "SELECT id,source FROM messages WHERE state='routed' AND created<?",
                &[&(now - LEASE)],
                2,
            )?;
            self.expire(
                cv,
                &rows,
                "delivery not confirmed within 30 seconds",
                "routed",
            )?;
            // Acknowledged by the receiving bridge, but no handoff was ever reported.
            let rows = self.query(
                "SELECT id,source FROM messages WHERE state='received' AND updated<?",
                &[&(now - LEASE)],
                2,
            )?;
            self.expire(
                cv,
                &rows,
                "received by the recipient machine but handoff not confirmed within 30 seconds",
                "received",
            )?;
            self.next_expiration = clock + 1.0;
            changed = true;
        }
        if clock >= self.next_retention {
            self.change(
                "DELETE FROM messages WHERE created<?",
                &[&(now - 7.0 * 86400.0)],
            )?;
            self.trim_messages()?;
            // A stored notice lives no longer than its message.
            self.change(
                "DELETE FROM notices WHERE created<? OR id NOT IN (SELECT id FROM messages)",
                &[&(now - 7.0 * 86400.0)],
            )?;
            self.next_retention = clock + 60.0;
            changed = true;
        }
        if changed {
            self.commit()?;
        }
        Ok(())
    }

    /// Mark unconfirmed messages unknown and tell their senders, best effort.
    fn expire(&mut self, cv: &Condvar, rows: &[Vec<Sql>], detail: &str, state: &str) -> Out<()> {
        let now = now();
        for row in rows {
            let (mid, source) = (&row[0], &row[1]);
            let updated = self.change(
                "UPDATE messages SET state='unknown',updated=?,detail=? WHERE id=? AND state=?",
                &[&now, &detail, mid, &state],
            )?;
            let host = sql_text(source)?
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            if updated == 0 {
                continue;
            }
            match self.machines.get_mut(&host) {
                Some(machine) if machine.queue.len() < QUEUE_LIMIT => {
                    machine.queue.push_back(dict([
                        ("type", "status".into()),
                        ("id", sql_val(mid)?),
                        ("state", "unknown".into()),
                        ("detail", detail.into()),
                        ("to", sql_val(source)?),
                        ("queued", now.into()),
                    ]));
                }
                // Offline or full: kept for the sender's machine instead of dropped.
                _ => {
                    self.store_notice(sql_text(mid)?, sql_text(source)?, "unknown", Some(detail))?
                }
            }
        }
        if !rows.is_empty() {
            cv.notify_all();
        }
        Ok(())
    }

    /// Keep a status observation for the sender's machine until it can be
    /// queued there. One notice per message: a newer observation replaces
    /// an older one not yet queued. The caller commits.
    fn store_notice(
        &mut self,
        id: &str,
        source: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Out<()> {
        let host = source.split('/').next().unwrap_or("").to_string();
        self.change(
            "INSERT OR REPLACE INTO notices (id,machine,source,state,detail,created) VALUES (?,?,?,?,?,?)",
            &[&id, &host, &source, &state, &detail, &now()],
        )?;
        self.notice_hosts.insert(host);
        Ok(())
    }

    /// Store the status events a dropped machine had not acknowledged; true
    /// when any was stored. Stored notices it held stay stored.
    fn keep_statuses(&mut self, state: Machine) -> Out<bool> {
        let mut kept = false;
        let events = state
            .inflight
            .into_values()
            .chain(state.queue)
            .filter(|e| event_str(e, "type") == "status" && get(e, "notice").is_none());
        for event in events {
            let detail = get(&event, "detail").and_then(Val::as_str);
            self.store_notice(
                event_str(&event, "id"),
                event_str(&event, "to"),
                event_str(&event, "state"),
                detail,
            )?;
            kept = true;
        }
        Ok(kept)
    }

    /// Queue a machine's stored notices while its queue has room. A notice
    /// whose message has since changed state (or is gone) is deleted.
    fn fill_notices(&mut self, cv: &Condvar, machine: &str) -> Out<()> {
        let Some(current) = self.machines.get(machine) else {
            return Ok(());
        };
        let room = QUEUE_LIMIT.saturating_sub(current.queue.len());
        let queued = current.notices.clone();
        let rows = self.query(
            "SELECT n.id,n.source,n.state,n.detail,m.state FROM notices n LEFT JOIN messages m ON m.id=n.id WHERE n.machine=? ORDER BY n.rowid",
            &[&machine],
            5,
        )?;
        let (mut events, mut stale, mut left) = (Vec::new(), Vec::new(), false);
        for row in &rows {
            let Sql::Text(id) = &row[0] else {
                continue;
            };
            if queued.contains(id) {
                continue;
            }
            if row[4] != row[2] {
                stale.push(id.clone());
                continue;
            }
            if events.len() >= room {
                left = true;
                continue;
            }
            let mut event = dict([
                ("type", "status".into()),
                ("id", id.as_str().into()),
                ("state", sql_val(&row[2])?),
            ]);
            if let Sql::Text(detail) = &row[3] {
                set(&mut event, "detail", detail.as_str());
            }
            set(&mut event, "to", sql_val(&row[1])?);
            set(&mut event, "queued", now());
            set(&mut event, "notice", true);
            events.push((id.clone(), event));
        }
        for id in &stale {
            self.change("DELETE FROM notices WHERE id=?", &[id])?;
        }
        self.commit()?;
        if left {
            self.notice_hosts.insert(machine.to_string());
        } else {
            self.notice_hosts.remove(machine);
        }
        if events.is_empty() {
            return Ok(());
        }
        let current = self.machines.get_mut(machine).expect("checked above");
        for (id, event) in events {
            current.notices.insert(id);
            current.queue.push_back(event);
        }
        cv.notify_all();
        Ok(())
    }

    /// A stored notice reached its machine: delete it, unless a newer
    /// observation of the same message replaced it meanwhile, which then
    /// waits to be queued in turn.
    fn notice_done(&mut self, machine: &str, event: &Dict) -> Out<()> {
        if !is_notice(event) {
            return Ok(());
        }
        let id = event_str(event, "id").to_string();
        if let Some(current) = self.machines.get_mut(machine) {
            current.notices.remove(&id);
        }
        let deleted = self.change(
            "DELETE FROM notices WHERE id=? AND state=? AND detail IS ?",
            &[
                &id,
                &event_str(event, "state"),
                &get(event, "detail").and_then(Val::as_str),
            ],
        )?;
        if deleted == 0 {
            self.notice_hosts.insert(machine.to_string());
        }
        Ok(())
    }

    fn trim_messages(&self) -> Out<()> {
        self.change(
            "DELETE FROM messages WHERE id IN (SELECT id FROM messages ORDER BY created DESC LIMIT -1 OFFSET 10000)",
            &[],
        )?;
        Ok(())
    }

    fn presence_snapshot(&mut self, enrollments: &Enrollments) -> (Vec<Val>, String) {
        if self.roster_dirty {
            let live: HashMap<&str, &Peer> = enrollments
                .values()
                .map(|p| (p.machine.as_str(), p))
                .collect();
            let mut roster: Vec<Dict> = Vec::new();
            for (machine, state) in &self.machines {
                let Some(peer) = live.get(machine.as_str()) else {
                    continue;
                };
                for session in state.sessions.values() {
                    let mut entry = session.clone();
                    set(&mut entry, "machine", machine.as_str());
                    set(&mut entry, "owner", peer.owner.clone());
                    roster.push(entry);
                }
            }
            roster.sort_by(|a, b| {
                let key = |d: &Dict| {
                    (
                        get(d, "machine").and_then(Val::as_json_str).cloned(),
                        get(d, "id").and_then(Val::as_json_str).cloned(),
                    )
                };
                key(a).cmp(&key(b))
            });
            let listing = Val::List(roster.iter().cloned().map(Val::Dict).collect());
            let digest =
                Sha256::digest(jsonval::dumps(&listing, jsonval::COMPACT_SORTED).as_bytes());
            self.revision = digest.iter().map(|b| format!("{b:02x}")).collect();
            self.roster = roster;
            self.roster_dirty = false;
        }
        let peers = self
            .roster
            .iter()
            .map(|peer| {
                let mut entry = peer.clone();
                let machine = get(peer, "machine").and_then(Val::as_str).unwrap_or("");
                let seen = self.machines.get(machine).map_or(0.0, |m| m.seen);
                set(&mut entry, "last_seen", seen);
                Val::Dict(entry)
            })
            .collect();
        (peers, self.revision.clone())
    }

    fn enter(&mut self, fingerprint: &str, polling: bool, waiting: bool) -> Out<()> {
        let now = mono();
        self.limits
            .retain(|_, value| value.active != 0 || now - value.time <= 60.0);
        let limit = self.limits.entry(fingerprint.to_string()).or_insert(Limit {
            tokens: 120.0,
            time: now,
            active: 0,
            polls: 0,
        });
        limit.tokens = 120f64.min(limit.tokens + (now - limit.time) * 2.0);
        limit.time = now;
        if limit.tokens < 1.0
            || limit.active >= 16
            || (polling && limit.polls >= 2)
            || (waiting && self.waiting >= WAITING_LIMIT)
        {
            return err("rate_limited", 429);
        }
        limit.tokens -= 1.0;
        limit.active += 1;
        limit.polls += i64::from(polling);
        self.waiting += usize::from(waiting);
        Ok(())
    }

    fn leave(&mut self, fingerprint: &str, polling: bool, waiting: bool) {
        if let Some(limit) = self.limits.get_mut(fingerprint) {
            limit.active -= 1;
            limit.polls -= i64::from(polling);
        }
        self.waiting = self.waiting.saturating_sub(usize::from(waiting));
    }
}

/// `{key: value for key, value in event.items() if key not in hidden}`.
fn public_event(event: &Dict, acknowledged: bool) -> Val {
    let mut copy = event.clone();
    copy.shift_remove(&JsonStr::new("queued"));
    copy.shift_remove(&JsonStr::new("notice"));
    if !acknowledged {
        copy.shift_remove(&JsonStr::new("ack_id"));
    }
    Val::Dict(copy)
}

/// Whether an event was queued from a stored notice.
fn is_notice(event: &Dict) -> bool {
    get(event, "notice").is_some()
}

fn event_str<'a>(event: &'a Dict, key: &str) -> &'a str {
    get(event, key).and_then(Val::as_str).unwrap_or("")
}

/// The state as a new holder of the lock finds it, with no transaction left
/// open. Every handler commits what it changed before it waits or returns,
/// so an open transaction here is one a handler left behind when it failed
/// or panicked (which also poisons the lock): its changes are rolled back
/// rather than committed by whatever request commits next.
fn consistent(guard: Guard<'_>) -> Guard<'_> {
    if !guard.db.is_autocommit() {
        if let Err(exc) = guard.db.execute_batch("ROLLBACK") {
            let _ = writeln!(
                std::io::stderr(),
                "switchboard server: rolling back an interrupted transaction failed: {exc}"
            );
        }
    }
    guard
}

impl Hub {
    pub fn open(config: Config) -> Result<Hub, String> {
        let db = Connection::open(&config.db)
            .map_err(|exc| format!("sqlite3.OperationalError: {exc}"))?;
        let setup = || -> rusqlite::Result<()> {
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, source TEXT, target TEXT, payload TEXT, state TEXT, created REAL, updated REAL, detail TEXT);
                 CREATE INDEX IF NOT EXISTS messages_created ON messages(created);
                 CREATE INDEX IF NOT EXISTS messages_state_created ON messages(state,created);
                 CREATE TABLE IF NOT EXISTS notices (id TEXT PRIMARY KEY, machine TEXT, source TEXT, state TEXT, detail TEXT, created REAL);
                 CREATE INDEX IF NOT EXISTS notices_machine ON notices(machine);
                 CREATE TABLE IF NOT EXISTS last_seen (machine TEXT PRIMARY KEY, at REAL);",
            )?;
            // A restart never replays pending deliveries, but each sender's
            // machine is told on its return that delivery was not confirmed.
            db.execute_batch("BEGIN")?;
            db.execute(
                "INSERT OR REPLACE INTO notices (id,machine,source,state,detail,created) SELECT id,substr(source,1,instr(source,'/')-1),source,'unknown',?,? FROM messages WHERE state IN ('routed','held','received') AND instr(source,'/')>1",
                rusqlite::params![RESTART_DETAIL, now()],
            )?;
            db.execute(
                "UPDATE messages SET state='unknown', updated=? WHERE state IN ('routed','held','received')",
                [now()],
            )?;
            db.execute_batch("COMMIT")
        };
        setup().map_err(|exc| format!("sqlite3.Error: {exc}"))?;
        let mut state = State {
            config,
            db,
            machines: IndexMap::new(),
            limits: HashMap::new(),
            enrollment_key: None,
            enrollment_cache: None,
            roster_dirty: true,
            roster: Vec::new(),
            revision: String::new(),
            next_expiration: 0.0,
            next_retention: 0.0,
            pings: IndexMap::new(),
            unknown_limits: HashMap::new(),
            waiting: 0,
            notice_hosts: std::collections::HashSet::new(),
            seen_recorded: HashMap::new(),
        };
        let cv = Condvar::new();
        state.cleanup(&cv).map_err(|fail| match fail {
            Fail::Api(code, _) => format!("startup cleanup failed: {code}"),
            other => format!("{other:?}"),
        })?;
        Ok(Hub {
            state: Mutex::new(state),
            cv,
        })
    }

    fn lock(&self) -> Guard<'_> {
        let guard = self.state.lock().unwrap_or_else(|poisoned| {
            self.state.clear_poison();
            poisoned.into_inner()
        });
        consistent(guard)
    }

    fn wait<'a>(&'a self, guard: Guard<'a>, seconds: f64) -> Guard<'a> {
        let duration = Duration::from_secs_f64(seconds.max(0.0));
        let guard = self
            .cv
            .wait_timeout(guard, duration)
            .unwrap_or_else(|poisoned| {
                self.state.clear_poison();
                poisoned.into_inner()
            })
            .0;
        consistent(guard)
    }

    /// A request path that panics in the middle of a database transaction,
    /// holding the lock (fault-injection builds only).
    #[cfg(feature = "fault-injection")]
    pub fn panic_in_transaction(&self) {
        let g = self.lock();
        let _ = g.change(
            "INSERT INTO messages VALUES ('fault-injection','a/b','c/d','{}','routed',0,0,NULL)",
            &[],
        );
        panic!("forced panic in a database transaction (fault-injection build)");
    }

    /// The registry rewrite, for tests that race it against the root helper.
    #[cfg(test)]
    pub fn update_registry_for_test(&self, mutate: impl FnOnce(&mut Dict)) -> Out<()> {
        self.lock().update_registry(mutate)
    }

    pub fn auth(&self, fingerprint: &str) -> Out<Peer> {
        self.lock().auth(fingerprint)
    }

    pub fn enrolled(&self, fingerprint: &str) -> Out<bool> {
        self.lock().enrolled(fingerprint)
    }

    pub fn enter(&self, fingerprint: &str, polling: bool, waiting: bool) -> Out<()> {
        self.lock().enter(fingerprint, polling, waiting)
    }

    pub fn leave(&self, fingerprint: &str, polling: bool, waiting: bool) {
        self.lock().leave(fingerprint, polling, waiting)
    }

    /// An unenrolled key asks to join with an invite's secret; the answer is
    /// `approved` with its machine name, or why not.
    pub fn enroll_unknown(&self, fingerprint: &str, address: &str, body: &Val) -> Out<Val> {
        let mut g = self.lock();
        g.unknown_allowed(address)?;
        if g.enrolled(fingerprint)? {
            drop(g);
            return self.request(fingerprint, "/v1/enroll", Some(body));
        }
        let body = match body {
            Val::Dict(map) if !has_other_keys(map, &["machine", "owner", "invite"]) => map,
            _ => return err("invalid_enrollment", 400),
        };
        let owner = label(get(body, "owner"), 64)?;
        let machine = match get(body, "machine").and_then(Val::as_str) {
            Some(name) if crate::invite::is_machine_name(name) => name.to_string(),
            _ => return err("invalid_machine", 400),
        };
        match get(body, "invite") {
            None => err("invite_required", 403),
            Some(secret) => match secret.as_str() {
                Some(secret) => g.redeem(&self.cv, fingerprint, &machine, owner, secret),
                None => err("invalid_enrollment", 400),
            },
        }
    }

    pub fn request(&self, fp: &str, path: &str, body: Option<&Val>) -> Out<Val> {
        let mut g = self.lock();
        let peer = g.auth(fp)?;
        g.record_seen(&peer.machine);
        g.cleanup(&self.cv)?;
        let machine = peer.machine.clone();
        match path {
            "/health" => {
                return Ok(Val::Dict(dict([
                    ("ok", true.into()),
                    ("machine", machine.into()),
                    ("owner", peer.owner.clone().into()),
                ])))
            }
            "/v1/enroll" => {
                return Ok(Val::Dict(dict([
                    ("ok", true.into()),
                    ("state", "approved".into()),
                    ("machine", machine.into()),
                ])))
            }
            "/v1/peers" => {
                let enrollments = g.enrollments()?;
                let (peers, revision) = g.presence_snapshot(&enrollments);
                return Ok(Val::Dict(dict([
                    ("ok", true.into()),
                    ("machine", machine.into()),
                    ("peers", Val::List(peers)),
                    ("peers_revision", revision.into()),
                ])));
            }
            _ => {}
        }
        let Some(Val::Dict(body)) = body else {
            return err("invalid_body", 400);
        };
        match path {
            "/v1/register" => register(&mut g, &self.cv, &machine, body),
            "/v1/poll" => self.poll(g, fp, &machine, body),
            "/v1/send" => send(&mut g, &self.cv, &machine, body),
            "/v1/status" => status(&mut g, &self.cv, &machine, body),
            "/v1/ping" => self.ping(g, body),
            "/v1/pong" => pong(&mut g, &self.cv, &machine, body),
            _ => err("not_found", 404),
        }
    }

    fn poll<'a>(&'a self, mut g: Guard<'a>, fp: &str, machine: &str, body: &Dict) -> Out<Val> {
        let has = |key: &str| get(body, key).is_some();
        let modern = has("registration") || has("peers_revision");
        let body_registration = get(body, "registration").filter(|v| **v != Val::Null);
        if body_registration.is_some() {
            identifier(body_registration)?;
        }
        let body_registration = body_registration.and_then(Val::as_str).map(str::to_string);
        let body_revision = get(body, "peers_revision").filter(|v| **v != Val::Null);
        if let Some(value) = body_revision {
            match value.as_json_str() {
                Some(text) if text.char_len() <= 256 => {}
                _ => return err("invalid_peers_revision", 400),
            }
        }
        let acks: Vec<String> = match get(body, "acks") {
            None => Vec::new(),
            Some(Val::List(items)) if items.len() <= 512 => items
                .iter()
                .map(|item| item.as_str().filter(|a| is_hex(a, 32)).map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or(Fail::Api("invalid_acks", 400))?,
            Some(_) => return err("invalid_acks", 400),
        };
        // The machine's registration matches the request's (both may be absent).
        let same_registration = |current: &Machine| current.registration == body_registration;
        if !g.machines.contains_key(machine) && !modern {
            // Legacy polling creates presence, but not a registered roster.
            g.machines.insert(
                machine.to_string(),
                Machine {
                    seen: now(),
                    ..Machine::default()
                },
            );
        }
        if let Some(current) = g.machines.get_mut(machine) {
            if !modern || same_registration(current) {
                current.seen = now();
            }
        }
        let mut acknowledged = modern
            && g.machines
                .get(machine)
                .is_some_and(|current| current.ack && same_registration(current));
        if acknowledged {
            let mut received = Vec::new();
            let mut noticed = Vec::new();
            {
                let current = g.machines.get_mut(machine).expect("current");
                for ack in &acks {
                    if let Some(event) = current.inflight.shift_remove(ack) {
                        if get(&event, "type").is_some_and(|t| t.is_str("message")) {
                            received.push(event_str(&event, "id").to_string());
                        } else if is_notice(&event) {
                            noticed.push(event);
                        }
                    }
                }
            }
            for event in &noticed {
                g.notice_done(machine, event)?;
            }
            for id in received {
                g.change(
                    "UPDATE messages SET state='received',updated=? WHERE id=? AND state='routed'",
                    &[&now(), &id],
                )?;
            }
            g.commit()?;
            // The bridge acknowledges each response on its next poll, so anything
            // still unacknowledged was lost in transit: deliver it again.
            if let Some(current) = g.machines.get_mut(machine) {
                let inflight: Vec<Dict> = current.inflight.drain(..).map(|(_, e)| e).collect();
                for event in inflight.into_iter().rev() {
                    current.queue.push_front(event);
                }
            }
        }
        let enrollments = g.enrollments()?;
        let (_, baseline) = g.presence_snapshot(&enrollments);
        let deadline = mono() + 10.0;
        let (peers, revision, registration, enrollments) = loop {
            g.cleanup(&self.cv)?;
            if g.notice_hosts.contains(machine) {
                g.fill_notices(&self.cv, machine)?;
            }
            let enrollments = g.enrollments()?;
            if enrollments.get(fp).is_none_or(|p| p.machine != machine) {
                return err("not_enrolled", 403);
            }
            // Re-fetch after every wait: expired or replaced objects own no deliveries.
            let registration = g.machines.get(machine).and_then(|m| m.registration.clone());
            let queued = g.machines.get(machine).is_some_and(|m| !m.queue.is_empty());
            let (peers, revision) = g.presence_snapshot(&enrollments);
            if queued || revision != baseline {
                break (peers, revision, registration, enrollments);
            }
            let revision_matches = body_revision.is_some_and(|v| v.is_str(&revision));
            if modern
                && (registration.is_none()
                    || body_registration != registration
                    || !revision_matches)
            {
                break (peers, revision, registration, enrollments);
            }
            let remaining = deadline - mono();
            if remaining <= 0.0 {
                break (peers, revision, registration, enrollments);
            }
            g = self.wait(g, remaining.min(1.0));
        };
        let mut events = Vec::new();
        let owns_registration =
            !modern || (registration.is_some() && body_registration == registration);
        let exists = g.machines.contains_key(machine);
        acknowledged = acknowledged && owns_registration && exists && g.machines[machine].ack;
        let active: std::collections::HashSet<&str> =
            enrollments.values().map(|p| p.machine.as_str()).collect();
        while owns_registration && g.machines.get(machine).is_some_and(|m| !m.queue.is_empty()) {
            let mut event = g
                .machines
                .get_mut(machine)
                .expect("current")
                .queue
                .pop_front()
                .expect("event");
            let kind = event_str(&event, "type").to_string();
            if kind == "message" {
                let id = event_str(&event, "id").to_string();
                let rows = g.query(
                    "SELECT state,created,source FROM messages WHERE id=?",
                    &[&id],
                    3,
                )?;
                let row = rows.first();
                if let Some(row) = row {
                    if matches!(&row[0], Sql::Text(s) if s == "routed")
                        && now() - sql_num(&row[1])? > LEASE
                    {
                        g.expire(
                            &self.cv,
                            &[vec![Sql::Text(id.clone()), row[2].clone()]],
                            "delivery not confirmed within 30 seconds",
                            "routed",
                        )?;
                        g.commit()?;
                        continue;
                    }
                }
                let routed = row.is_some_and(|r| matches!(&r[0], Sql::Text(s) if s == "routed"));
                let sid = event_str(&event, "to")
                    .split('/')
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let source = event_str(&event, "from")
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let current = &g.machines[machine];
                if !routed
                    || !current.sessions.contains_key(&sid)
                    || !active.contains(source.as_str())
                {
                    continue;
                }
            } else if kind == "ping" {
                let id = event_str(&event, "id");
                if !g.pings.contains_key(id) || !g.machines[machine].ping {
                    continue;
                }
            } else if acknowledged && !is_notice(&event) {
                // Stored notices are checked against their message when queued.
                let queued = match get(&event, "queued") {
                    Some(Val::Float(time)) => *time,
                    _ => now(),
                };
                if now() - queued > LEASE {
                    continue; // Stale redelivered status observations are dropped.
                }
            }
            if acknowledged {
                if get(&event, "ack_id").is_none() {
                    set(&mut event, "ack_id", uuid4_hex());
                }
                let ack_id = event_str(&event, "ack_id").to_string();
                events.push(public_event(&event, true));
                g.machines
                    .get_mut(machine)
                    .expect("current")
                    .inflight
                    .insert(ack_id, event);
            } else {
                events.push(public_event(&event, false));
                // Consumed on poll: a stored notice is delivered.
                g.notice_done(machine, &event)?;
            }
        }
        g.commit()?;
        let mut result = dict([
            ("ok", true.into()),
            ("events", Val::List(events)),
            ("machine", machine.into()),
            ("registration", registration.into()),
            ("peers_revision", revision.as_str().into()),
        ]);
        let revision_matches =
            matches!(get(body, "peers_revision"), Some(v) if v.is_str(&revision));
        if !revision_matches {
            set(&mut result, "peers", Val::List(peers));
        }
        Ok(Val::Dict(result))
    }

    fn ping<'a>(&'a self, mut g: Guard<'a>, body: &Dict) -> Out<Val> {
        // Answered by the remote bridge, never by a Claude session; not a message.
        if has_other_keys(body, &["to"]) {
            return err("invalid_ping", 400);
        }
        let (target, target_id) = parse_route(get(body, "to"))?;
        let enabled = g.enabled_machines()?;
        let Some(state) = g
            .machines
            .get_mut(&target)
            .filter(|s| enabled.contains(&target) && s.sessions.contains_key(&target_id))
        else {
            return err("target_offline", 404);
        };
        if !state.ping {
            return err("ping_unsupported", 409);
        }
        if state.queue.len() >= QUEUE_LIMIT {
            return err("queue_full", 503);
        }
        if g.pings.len() >= 256 {
            return err("rate_limited", 429);
        }
        let ping_id = uuid4();
        let started = mono();
        g.pings.insert(
            ping_id.clone(),
            Ping {
                machine: target.clone(),
                result: None,
            },
        );
        let to = get(body, "to").cloned().unwrap_or(Val::Null);
        g.machines
            .get_mut(&target)
            .expect("target")
            .queue
            .push_back(dict([
                ("type", "ping".into()),
                ("id", ping_id.as_str().into()),
                ("to", to),
                ("queued", now().into()),
            ]));
        self.cv.notify_all();
        let result = loop {
            if let Some(result) = g.pings.get(&ping_id).and_then(|p| p.result) {
                break Ok(result);
            }
            let remaining = started + PING_DEADLINE - mono();
            if remaining <= 0.0 {
                break err("ping_timeout", 504);
            }
            g = self.wait(g, remaining);
        };
        g.pings.shift_remove(&ping_id);
        let (live, at) = result?;
        Ok(Val::Dict(dict([
            ("ok", true.into()),
            ("rtt_ms", round_even((at - started) * 1000.0).max(0).into()),
            ("live", live.into()),
        ])))
    }
}

fn register(g: &mut State, cv: &Condvar, machine: &str, body: &Dict) -> Out<Val> {
    let sessions = match get(body, "sessions") {
        Some(Val::List(items)) if items.len() <= 32 => items,
        _ => return err("invalid_sessions", 400),
    };
    let offered: Vec<&str> = match get(body, "capabilities") {
        None => Vec::new(),
        Some(Val::List(items))
            if items.len() <= 16
                && items
                    .iter()
                    .all(|c| c.as_json_str().is_some_and(|t| t.char_len() <= 32)) =>
        {
            items.iter().filter_map(Val::as_str).collect()
        }
        Some(_) => return err("invalid_capabilities", 400),
    };
    let (ack, ping) = (offered.contains(&"ack"), offered.contains(&"ping"));
    let mut registered: IndexMap<String, Dict> = IndexMap::new();
    for session in sessions {
        let session = match session {
            Val::Dict(map)
                if !has_other_keys(
                    map,
                    &["id", "name", "status", "project", "repository", "branch"],
                ) =>
            {
                map
            }
            _ => return err("invalid_session", 400),
        };
        let sid = identifier(get(session, "id"))?;
        let status = get(session, "status");
        let known = ["idle", "busy", "waiting", "shell", "unknown"];
        if registered.contains_key(&sid)
            || !status.is_some_and(|s| known.iter().any(|k| s.is_str(k)))
        {
            return err("invalid_session", 400);
        }
        let mut entry = dict([
            ("id", sid.as_str().into()),
            ("name", label(get(session, "name"), 64)?.into()),
            ("status", status.cloned().unwrap_or(Val::Null)),
        ]);
        for (field, maximum) in [("project", 64), ("repository", 256), ("branch", 256)] {
            if let Some(value) = get(session, field) {
                set(&mut entry, field, label(Some(value), maximum)?);
            }
        }
        registered.insert(sid, entry);
    }
    if !g.machines.contains_key(machine) {
        g.machines.insert(
            machine.to_string(),
            Machine {
                seen: now(),
                ..Machine::default()
            },
        );
    }
    let removed: Vec<String> = g.machines[machine]
        .sessions
        .keys()
        .filter(|sid| !registered.contains_key(*sid))
        .cloned()
        .collect();
    for sid in removed {
        let target = format!("{machine}/{sid}");
        let rows = g.query(
            "SELECT id,source FROM messages WHERE target=? AND state='routed'",
            &[&target],
            2,
        )?;
        g.expire(
            cv,
            &rows,
            "recipient session left before confirming delivery",
            "routed",
        )?;
    }
    g.commit()?;
    let Some(current) = g.machines.get_mut(machine) else {
        // Inserted above under the same lock; never missing.
        return Err(Fail::Abort("the registering machine disappeared".into()));
    };
    if !ack && !current.inflight.is_empty() {
        // A downgraded bridge receives unacknowledged events the original way.
        let inflight: Vec<Dict> = current.inflight.drain(..).map(|(_, e)| e).collect();
        for event in inflight.into_iter().rev() {
            current.queue.push_front(event);
        }
    }
    current.ack = ack;
    current.ping = ping;
    let changed = current.sessions != registered || current.registration.is_none();
    if current.registration.is_none() {
        current.registration = Some(uuid4());
    }
    let roster_changed = current.sessions != registered;
    current.sessions = registered;
    current.seen = now();
    let registration = current.registration.clone();
    if roster_changed {
        g.roster_dirty = true;
    }
    if changed {
        cv.notify_all();
    }
    // Notices stored while this machine was away or its queue was full.
    g.fill_notices(cv, machine)?;
    Ok(Val::Dict(dict([
        ("ok", true.into()),
        ("machine", machine.into()),
        ("registration", registration.into()),
        (
            "session_metadata_fields",
            Val::List(vec!["project".into(), "repository".into(), "branch".into()]),
        ),
        (
            "capabilities",
            Val::List(CAPABILITIES.iter().map(|c| (*c).into()).collect()),
        ),
    ])))
}

fn send(g: &mut State, cv: &Condvar, machine: &str, body: &Dict) -> Out<Val> {
    let mid = identifier(get(body, "id"))?;
    if has_other_keys(body, &["id", "from", "to", "text", "mode", "hop_chain"]) {
        return err("invalid_message", 400);
    }
    let (src, source_id) = parse_route(get(body, "from"))?;
    let (target, target_id) = parse_route(get(body, "to"))?;
    let source_session = g
        .machines
        .get(machine)
        .and_then(|m| m.sessions.get(&source_id));
    if src != machine || source_session.is_none() {
        return err("source_forbidden", 403);
    }
    let text = get(body, "text").and_then(Val::as_json_str);
    let Some(text) = text else {
        return err("invalid_message", 400);
    };
    let mode_ok = get(body, "mode").is_some_and(|m| m.is_str("prompting") || m.is_str("bypass"));
    if encoded_len(text)? > 8192 || !mode_ok {
        return err("invalid_message", 400);
    }
    if let Some(hop_chain) = get(body, "hop_chain") {
        match hop_chain.as_json_str() {
            Some(chain) if encoded_len(chain)? <= 8192 => {}
            _ => return err("invalid_hop_chain", 400),
        }
    }
    let payload = jsonval::dumps(&Val::Dict(body.clone()), jsonval::SORTED);
    let previous = g.query("SELECT payload,state FROM messages WHERE id=?", &[&mid], 2)?;
    if let Some(row) = previous.first() {
        if !matches!(&row[0], Sql::Text(stored) if *stored == payload) {
            return err("id_conflict", 409);
        }
        return Ok(Val::Dict(dict([
            ("ok", true.into()),
            ("id", mid.into()),
            ("state", sql_val(&row[1])?),
        ])));
    }
    let active = g.enabled_machines()?;
    let online = g
        .machines
        .get(&target)
        .is_some_and(|m| m.sessions.contains_key(&target_id));
    if !active.contains(&target) || !online {
        return err("target_offline", 404);
    }
    if g.machines[&target].queue.len() >= QUEUE_LIMIT {
        return err("queue_full", 503);
    }
    let now = now();
    let from = get(body, "from")
        .and_then(Val::as_str)
        .unwrap_or("")
        .to_string();
    let to = get(body, "to")
        .and_then(Val::as_str)
        .unwrap_or("")
        .to_string();
    // Read before the commit: nothing after it may fail or panic, or the
    // stored message would never reach the recipient's queue.
    let sender_name = g
        .machines
        .get(machine)
        .and_then(|m| m.sessions.get(&source_id))
        .and_then(|session| get(session, "name"))
        .cloned()
        .unwrap_or(Val::Null);
    g.change(
        "INSERT INTO messages VALUES (?,?,?,?,?,?,?,?)",
        &[
            &mid,
            &from,
            &to,
            &payload,
            &"routed",
            &now,
            &now,
            &rusqlite::types::Null,
        ],
    )?;
    g.trim_messages()?;
    g.commit()?;
    let mut event = body.clone();
    set(&mut event, "type", "message");
    set(&mut event, "sender_name", sender_name);
    // The target was checked above under the same lock, so it is still here.
    if let Some(recipient) = g.machines.get_mut(&target) {
        recipient.queue.push_back(event);
    }
    g.cleanup(cv)?;
    cv.notify_all();
    Ok(Val::Dict(dict([
        ("ok", true.into()),
        ("id", mid.into()),
        ("state", "routed".into()),
    ])))
}

fn status(g: &mut State, cv: &Condvar, machine: &str, body: &Dict) -> Out<Val> {
    let mid = identifier(get(body, "id"))?;
    let states = ["handed_to_claude", "held", "refused", "failed", "unknown"];
    let state = match get(body, "state").and_then(Val::as_str) {
        Some(state) if states.contains(&state) => state.to_string(),
        _ => return err("invalid_status", 400),
    };
    let detail = get(body, "detail").filter(|d| **d != Val::Null);
    let detail = match detail {
        None => None,
        Some(Val::Str(text)) if encoded_len(text)? <= 2048 => {
            Some(text.as_str().expect("encodable").to_string())
        }
        Some(_) => return err("invalid_detail", 400),
    };
    let rows = g.query(
        "SELECT source,target,state FROM messages WHERE id=?",
        &[&mid],
        3,
    )?;
    let Some(row) = rows.first() else {
        return err("message_not_found", 404);
    };
    if sql_text(&row[1])?.split('/').next() != Some(machine) {
        return err("status_forbidden", 403);
    }
    let recorded = match &row[2] {
        Sql::Text(text) => text.as_str(),
        _ => "",
    };
    let terminal = recorded == "refused" || recorded == "failed";
    if (terminal && state != "refused" && state != "failed")
        || (recorded == "held"
            && state == "handed_to_claude"
            && detail.as_deref() != Some("delivered"))
    {
        return Ok(Val::Dict(dict([("ok", true.into())])));
    }
    let source = sql_text(&row[0])?.to_string();
    let host = source.split('/').next().unwrap_or("").to_string();
    g.change(
        "UPDATE messages SET state=?,updated=?,detail=? WHERE id=?",
        &[&state, &now(), &detail, &mid],
    )?;
    // The observation is authoritative and always recorded. A sender's
    // machine that is away or whose queue is full gets it later instead.
    let direct = g
        .machines
        .get(&host)
        .is_some_and(|m| m.queue.len() < QUEUE_LIMIT);
    if !direct {
        g.store_notice(&mid, &source, &state, detail.as_deref())?;
    }
    g.commit()?;
    if let Some(sender) = g.machines.get_mut(&host).filter(|_| direct) {
        let mut event = dict([
            ("type", "status".into()),
            ("id", mid.as_str().into()),
            ("state", state.as_str().into()),
            ("to", source.as_str().into()),
            ("queued", now().into()),
        ]);
        if let Some(detail) = &detail {
            set(&mut event, "detail", detail.as_str());
        }
        sender.queue.push_back(event);
    }
    cv.notify_all();
    Ok(Val::Dict(dict([("ok", true.into())])))
}

fn pong(g: &mut State, cv: &Condvar, machine: &str, body: &Dict) -> Out<Val> {
    let ping_id = identifier(get(body, "id"))?;
    let live = match get(body, "live") {
        Some(Val::Bool(live)) if !has_other_keys(body, &["id", "live"]) => *live,
        _ => return err("invalid_pong", 400),
    };
    let Some(ping) = g.pings.get_mut(&ping_id) else {
        return Ok(Val::Dict(dict([("ok", true.into())]))); // Late answers after the deadline are ignored.
    };
    if ping.machine != machine {
        return err("pong_forbidden", 403);
    }
    if ping.result.is_none() {
        ping.result = Some((live, mono()));
        cv.notify_all();
    }
    Ok(Val::Dict(dict([("ok", true.into())])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_rounding() {
        assert!(identifier(Some(&Val::from("0a1b2c3d-0000-4000-8000-00000000000f"))).is_ok());
        assert!(identifier(Some(&Val::from("0A1B2C3D-0000-4000-8000-00000000000F"))).is_err());
        assert!(identifier(Some(&Val::from("{0a1b2c3d-0000-4000-8000-00000000000f}"))).is_err());
        assert!(identifier(Some(&Val::Int("5".into()))).is_err());
        assert_eq!(round_even(2.5), 2);
        assert_eq!(round_even(3.5), 4);
        assert_eq!(round_even(-0.4), 0);
    }

    #[test]
    fn labels_count_code_points_and_refuse_controls() {
        let ok = |text: &str, max| label(Some(&Val::from(text)), max).is_ok();
        assert!(ok(&"\u{e9}".repeat(64), 64));
        assert!(!ok(&"\u{e9}".repeat(65), 64));
        assert!(!ok("", 64));
        assert!(!ok("a\nb", 64));
        assert!(!ok("del\u{7f}", 64));
        assert!(ok("tab\u{85}", 64));
        let surrogate = jsonval::loads(br#""\ud800""#).unwrap();
        assert!(label(Some(&surrogate), 64).is_ok());
        assert!(label(Some(&Val::Int("5".into())), 64).is_err());
    }

    #[test]
    fn registry_parsing_fails_closed() {
        let fp = "a".repeat(64);
        let parse = |text: &str| parse_registry(&jsonval::loads(text.as_bytes()).unwrap());
        // The removed viewer's `view` and the removed administrator flag
        // are ignored.
        let good = format!(
            r#"{{"{fp}": {{"machine": "m", "owner": "o", "enabled": true, "view": 1, "admin": true}}, "x": {{"enabled": false}}}}"#
        );
        let parsed = parse(&good).unwrap();
        assert_eq!(
            parsed[&fp],
            Peer {
                machine: "m".into(),
                owner: "o".into(),
            }
        );
        assert!(parse(&format!(
            r#"{{"{fp}": {{"machine": "m", "enabled": true}}}}"#
        ))
        .is_none());
        assert!(parse(&format!(
            r#"{{"{fp}": {{"machine": "m", "owner": "o", "enabled": 1}}}}"#
        ))
        .unwrap()
        .is_empty());
        assert!(parse(&format!(
            r#"{{"{}": {{"machine": "m", "owner": "o", "enabled": true}}}}"#,
            "A".repeat(64)
        ))
        .is_none());
        let dup = format!(
            r#"{{"{fp}": {{"machine": "m", "owner": "o", "enabled": true}}, "{}": {{"machine": "m", "owner": "o", "enabled": true}}}}"#,
            "b".repeat(64)
        );
        assert!(parse(&dup).is_none());
        assert!(parse("[]").is_none());
        assert!(parse(r#"{"x": 5}"#).is_none());
    }

    use std::os::unix::fs::PermissionsExt;

    /// A private directory with a registry (machines `a` and `b`) and a
    /// server state over it. The entries still carry the `view` key of the
    /// removed browser viewer and `a` the removed `admin` flag, as older
    /// registries do.
    struct Fixture {
        root: String,
        peers: String,
        hub: Hub,
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    impl Fixture {
        fn new() -> Fixture {
            Fixture::new_in("")
        }

        /// The registry (and its lock) in `subdirectory` of the root, apart
        /// from the database and the invite store when it is not empty.
        fn new_in(subdirectory: &str) -> Fixture {
            let template = std::ffi::CString::new("/tmp/sbs-XXXXXX").unwrap();
            let mut raw = template.into_bytes_with_nul();
            // SAFETY: raw is a writable NUL-terminated template.
            assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
            raw.pop();
            let root = std::fs::canonicalize(String::from_utf8(raw).unwrap())
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let directory = if subdirectory.is_empty() {
                root.clone()
            } else {
                let directory = format!("{root}/{subdirectory}");
                std::fs::create_dir(&directory).unwrap();
                directory
            };
            let peers = format!("{directory}/peers.json");
            let registry = format!(
                r#"{{"{A}": {{"machine": "a", "owner": "test", "enabled": true, "view": true, "admin": true}}, "{B}": {{"machine": "b", "owner": "test", "enabled": true, "view": false}}}}"#
            );
            crate::server::write_private(&peers, registry.as_bytes()).unwrap();
            let hub = Hub::open(Config {
                db: format!("{root}/messages.db"),
                peers_file: peers.clone(),
                registry_lock: None,
            })
            .unwrap();
            Fixture { root, peers, hub }
        }

        fn store(&self) -> String {
            super::super::invites::store_path(&format!("{}/messages.db", self.root))
        }

        /// Store an invite with `uses` uses for `expires` seconds (fixing
        /// `machine` when given) and return its secret.
        fn invite(&self, uses: i64, expires: i64, machine: Option<&str>) -> String {
            use super::super::invites::{now, read_store, write_store, Entry};
            let secret = crate::invite::new_secret();
            let hash = crate::invite::secret_hash(&secret);
            // SAFETY: geteuid and getegid cannot fail.
            let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
            let mut entries = read_store(&self.store(), uid).unwrap();
            entries.push(Entry {
                id: hash[..12].to_string(),
                hash,
                created_at: now(),
                expires_at: now() + expires,
                uses,
                machine: machine.map(str::to_string),
                source: "command".into(),
            });
            write_store(&self.store(), &entries, uid, gid).unwrap();
            secret
        }

        fn stored(&self) -> Vec<super::super::invites::Entry> {
            // SAFETY: geteuid cannot fail.
            super::super::invites::read_store(&self.store(), unsafe { libc::geteuid() }).unwrap()
        }

        /// An unknown key asks to join as `machine` with `secret`.
        fn redeem(&self, fp: &str, machine: &str, secret: &str) -> Out<Val> {
            let body = jsonval::loads(
                format!(r#"{{"machine": "{machine}", "owner": "sam", "invite": "{secret}"}}"#)
                    .as_bytes(),
            )
            .unwrap();
            self.hub.enroll_unknown(fp, "192.0.2.9", &body)
        }

        fn call(&self, fp: &str, path: &str, body: &str) -> Out<Val> {
            let body = jsonval::loads(body.as_bytes()).unwrap();
            self.hub.request(fp, path, Some(&body))
        }

        fn get(&self, fp: &str, path: &str) -> Out<Val> {
            self.hub.request(fp, path, None)
        }

        fn register(&self, fp: &str, sid: &str) -> String {
            let body =
                format!(r#"{{"sessions": [{{"id": "{sid}", "name": "chat", "status": "idle"}}]}}"#);
            let value = self.call(fp, "/v1/register", &body).unwrap();
            value
                .as_dict()
                .and_then(|d| get(d, "registration"))
                .and_then(Val::as_str)
                .unwrap()
                .to_string()
        }

        fn state_of(&self, id: &str) -> String {
            let g = self.hub.lock();
            g.db.query_row("SELECT state FROM messages WHERE id=?", [id], |row| {
                row.get(0)
            })
            .unwrap()
        }

        fn insert(&self, id: &str, state: &str, created: f64) {
            let g = self.hub.lock();
            g.db.execute(
                "INSERT INTO messages VALUES (?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    id,
                    "a/x",
                    "b/x",
                    "{}",
                    state,
                    created,
                    created,
                    rusqlite::types::Null
                ],
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.root);
            SHIFT.with(|shift| shift.set(0.0));
        }
    }

    fn step(seconds: f64) {
        SHIFT.with(|shift| shift.set(shift.get() + seconds));
    }

    fn code(result: Out<Val>) -> &'static str {
        match result {
            Err(Fail::Api(code, _)) => code,
            Err(other) => panic!("unexpected failure {other:?}"),
            Ok(value) => panic!("unexpected success {value:?}"),
        }
    }

    fn field<'a>(value: &'a Val, key: &str) -> &'a Val {
        get(value.as_dict().unwrap(), key).unwrap()
    }

    #[test]
    fn maintenance_runs_on_its_cadence_and_keeps_held_messages() {
        let f = Fixture::new();
        let start = now();
        f.insert("routed", "routed", start - 31.0);
        f.insert("held", "held", start - 31.0);
        f.insert("old", "handed_to_claude", start - 8.0 * 86400.0);
        let cv = Condvar::new();
        {
            let mut g = f.hub.lock();
            g.next_expiration = mono() + 1.0;
            g.next_retention = mono() + 60.0;
            g.cleanup(&cv).unwrap();
        }
        assert_eq!(f.state_of("routed"), "routed");
        step(1.0);
        f.hub.lock().cleanup(&cv).unwrap();
        assert_eq!(f.state_of("routed"), "unknown");
        assert_eq!(f.state_of("held"), "held");
        assert_eq!(f.state_of("old"), "handed_to_claude"); // Retention waits for its minute.
        step(59.0);
        f.hub.lock().cleanup(&cv).unwrap();
        let g = f.hub.lock();
        let remaining: i64 =
            g.db.query_row("SELECT count(*) FROM messages WHERE id='old'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
        assert!(g.next_retention > mono() + 59.0);
    }

    #[test]
    fn expired_message_is_not_delivered_between_maintenance_ticks() {
        let f = Fixture::new();
        let (sa, sb) = (uuid4(), uuid4());
        f.register(A, &sa);
        let registration = f.register(B, &sb);
        let id = uuid4();
        let body = format!(
            r#"{{"id": "{id}", "from": "a/{sa}", "to": "b/{sb}", "text": "expired boundary", "mode": "prompting"}}"#
        );
        f.call(A, "/v1/send", &body).unwrap();
        {
            let mut g = f.hub.lock();
            g.db.execute(
                "UPDATE messages SET created=? WHERE id=?",
                rusqlite::params![now() - 30.1, id],
            )
            .unwrap();
            g.next_expiration = mono() + 1.0;
        }
        let poll = f
            .call(
                B,
                "/v1/poll",
                &format!(r#"{{"registration": "{registration}", "peers_revision": null}}"#),
            )
            .unwrap();
        assert_eq!(field(&poll, "events"), &Val::List(vec![]));
        assert_eq!(f.state_of(&id), "unknown");
    }

    #[test]
    fn health_reports_no_administrator_flag_and_approval_paths_are_gone() {
        let f = Fixture::new();
        let health = f.get(A, "/health").unwrap();
        let keys: Vec<String> = health
            .as_dict()
            .unwrap()
            .keys()
            .map(|key| key.as_str().unwrap().to_string())
            .collect();
        assert_eq!(keys, ["ok", "machine", "owner"]);
        for path in ["/v1/approve", "/v1/refuse", "/v1/revoke", "/v1/machines"] {
            assert_eq!(code(f.call(A, path, r#"{"machine": "b"}"#)), "not_found");
        }
    }

    #[test]
    fn requests_record_when_each_machine_was_last_seen() {
        let f = Fixture::new();
        let seen = |f: &Fixture, machine: &str| -> Option<f64> {
            f.hub
                .lock()
                .db
                .query_row(
                    "SELECT at FROM last_seen WHERE machine=?",
                    [machine],
                    |row| row.get(0),
                )
                .ok()
        };
        assert_eq!(seen(&f, "a"), None);
        f.get(A, "/health").unwrap();
        let first = seen(&f, "a").unwrap();
        assert!((now() - first).abs() < 5.0);
        assert_eq!(seen(&f, "b"), None);
        // At most once a minute per machine.
        step(30.0);
        f.get(A, "/v1/peers").unwrap();
        assert_eq!(seen(&f, "a"), Some(first));
        step(31.0);
        f.get(A, "/v1/peers").unwrap();
        assert!(seen(&f, "a").unwrap() >= first + 60.0);
    }

    #[test]
    fn unknown_keys_refill_one_request_every_three_seconds() {
        let f = Fixture::new();
        let stranger = "c".repeat(64);
        let body = jsonval::loads(br#"{"machine": "new-box", "owner": "sam"}"#).unwrap();
        for _ in 0..20 {
            assert_eq!(
                code(f.hub.enroll_unknown(&stranger, "192.0.2.50", &body)),
                "invite_required"
            );
        }
        assert_eq!(
            code(f.hub.enroll_unknown(&stranger, "192.0.2.50", &body)),
            "rate_limited"
        );
        step(3.0);
        assert_eq!(
            code(f.hub.enroll_unknown(&stranger, "192.0.2.50", &body)),
            "invite_required"
        );
        assert_eq!(
            code(f.hub.enroll_unknown(&stranger, "192.0.2.50", &body)),
            "rate_limited"
        );
        // Redeeming counts against the same limit.
        let secret = f.invite(1, 600, None);
        assert_eq!(
            code(
                f.hub.enroll_unknown(
                    &stranger,
                    "192.0.2.50",
                    &jsonval::loads(
                        format!(
                            r#"{{"machine": "new-box", "owner": "sam", "invite": "{secret}"}}"#
                        )
                        .as_bytes()
                    )
                    .unwrap()
                )
            ),
            "rate_limited"
        );
        assert_eq!(f.stored()[0].uses, 1);
    }

    #[test]
    fn machine_requests_refill_two_per_second() {
        let f = Fixture::new();
        for _ in 0..120 {
            f.hub.enter(B, false, false).unwrap();
            f.hub.leave(B, false, false);
        }
        assert_eq!(
            code(f.hub.enter(B, false, false).map(|()| Val::Null)),
            "rate_limited"
        );
        step(1.0);
        for _ in 0..2 {
            f.hub.enter(B, false, false).unwrap();
            f.hub.leave(B, false, false);
        }
        assert_eq!(
            code(f.hub.enter(B, false, false).map(|()| Val::Null)),
            "rate_limited"
        );
    }

    #[test]
    fn silent_machines_lose_their_lease_after_thirty_seconds() {
        let f = Fixture::new();
        let registration = f.register(B, &uuid4());
        step(29.0);
        assert!(
            matches!(field(&f.get(A, "/v1/peers").unwrap(), "peers"), Val::List(items) if items.len() == 1)
        );
        step(2.0);
        assert!(
            matches!(field(&f.get(A, "/v1/peers").unwrap(), "peers"), Val::List(items) if items.is_empty())
        );
        assert_ne!(f.register(B, &uuid4()), registration);
    }

    #[test]
    fn registry_replaced_by_a_symlink_fails_closed() {
        let f = Fixture::new();
        f.hub.auth(A).unwrap();
        let saved = format!("{}/saved.json", f.root);
        std::fs::rename(&f.peers, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &f.peers).unwrap();
        assert_eq!(
            code(f.hub.auth(A).map(|_| Val::Null)),
            "enrollment_unavailable"
        );
        assert!(f.hub.lock().enrollment_cache.is_none());
        std::fs::remove_file(&f.peers).unwrap();
        std::fs::rename(&saved, &f.peers).unwrap();
        assert_eq!(f.hub.auth(A).unwrap().machine, "a");
    }

    #[test]
    fn a_failed_registry_write_gives_the_invite_back() {
        let f = Fixture::new_in("registry");
        let stranger = "c".repeat(64);
        let secret = f.invite(1, 600, None);
        let directory = format!("{}/registry", f.root);
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(format!("{}.lock", f.peers))
            .unwrap();
        let before = std::fs::read(&f.peers).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = f.redeem(&stranger, "new-box", &secret);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(failed, Err(Fail::Unavailable)), "{failed:?}");
        assert_eq!(std::fs::read(&f.peers).unwrap(), before);
        assert_eq!(f.stored()[0].uses, 1);
        for dir in [&f.root, &directory] {
            let names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                names
                    .iter()
                    .all(|n| !n.starts_with(".peers.json.") && !n.starts_with(".invites.json.")),
                "{names:?}"
            );
        }
        // The invite still works once the registry is writable.
        let approved = f.redeem(&stranger, "new-box", &secret).unwrap();
        assert!(field(&approved, "state").is_str("approved"));
        assert_eq!(f.hub.auth(&stranger).unwrap().machine, "new-box");
        // Used up, and remembered as used.
        assert!(f.stored().iter().all(|e| e.uses == 0));
    }

    #[test]
    fn a_failed_invite_store_write_changes_nothing() {
        let f = Fixture::new();
        let stranger = "c".repeat(64);
        let secret = f.invite(1, 600, None);
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(format!("{}.lock", f.peers))
            .unwrap();
        let before = std::fs::read(&f.peers).unwrap();
        std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = f.redeem(&stranger, "new-box", &secret);
        std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(failed, Err(Fail::Unavailable)), "{failed:?}");
        assert_eq!(std::fs::read(&f.peers).unwrap(), before);
        assert_eq!(f.stored()[0].uses, 1);
        assert_eq!(
            code(f.hub.auth(&stranger).map(|_| Val::Null)),
            "not_enrolled"
        );
    }

    #[test]
    fn registry_rewrite_keeps_the_registry_group() {
        let f = Fixture::new();
        // SAFETY: getgroups with a sized buffer; getegid cannot fail.
        let (primary, groups) = unsafe {
            let mut groups = vec![0 as libc::gid_t; 64];
            let count = libc::getgroups(64, groups.as_mut_ptr());
            groups.truncate(count.max(0) as usize);
            (libc::getegid(), groups)
        };
        let Some(other) = groups.into_iter().find(|g| *g != primary) else {
            return;
        };
        // New files take the directory's group on BSD and the process's on
        // Linux; both differ from the registry's, so the server must restore it.
        let chown = |path: &str, gid: libc::gid_t| {
            let path = std::ffi::CString::new(path).unwrap();
            // SAFETY: path is NUL-terminated; -1 keeps the owner.
            assert_eq!(unsafe { libc::chown(path.as_ptr(), u32::MAX, gid) }, 0);
        };
        chown(&f.root, primary);
        chown(&f.peers, other);
        let secret = f.invite(1, 600, None);
        chown(&f.store(), other);
        f.redeem(&"c".repeat(64), "new-box", &secret).unwrap();
        for path in [&f.peers, &f.store()] {
            let info = std::fs::metadata(path).unwrap();
            assert_eq!((info.gid(), info.mode() & 0o777), (other, 0o600), "{path}");
        }
        let text = std::fs::read_to_string(&f.peers).unwrap();
        assert!(
            text.starts_with("{\n  \"aaaa") && text.ends_with("}\n"),
            "{text}"
        );
    }

    #[test]
    fn a_transaction_left_open_is_rolled_back_by_the_next_holder() {
        let f = Fixture::new();
        {
            // A handler that changed the store and then failed before committing.
            let g = f.hub.lock();
            g.change(
                "INSERT INTO messages VALUES ('left-open','a/b','c/d','{}','routed',0,0,NULL)",
                &[],
            )
            .unwrap();
            assert!(!g.db.is_autocommit());
        }
        let g = f.hub.lock();
        assert!(g.db.is_autocommit());
        g.commit().unwrap();
        let rows = g
            .query("SELECT id FROM messages WHERE id='left-open'", &[], 1)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn an_invite_enrolls_a_key_once() {
        let f = Fixture::new();
        let (first, second) = ("c".repeat(64), "d".repeat(64));
        let secret = f.invite(1, 600, None);
        let approved = f.redeem(&first, "new-box", &secret).unwrap();
        assert_eq!(
            approved,
            Val::Dict(dict([
                ("ok", true.into()),
                ("state", "approved".into()),
                ("machine", "new-box".into()),
            ]))
        );
        assert_eq!(f.hub.auth(&first).unwrap().machine, "new-box");
        let health = f.get(&first, "/health").unwrap();
        assert_eq!(field(&health, "owner"), &Val::from("sam"));
        let registry = jsonval::loads(&std::fs::read(&f.peers).unwrap()).unwrap();
        let entry = get(registry.as_dict().unwrap(), &first)
            .unwrap()
            .as_dict()
            .unwrap();
        let keys: Vec<&str> = entry.keys().map(|k| k.as_str().unwrap()).collect();
        assert_eq!(
            keys,
            ["enabled", "enrolled_at", "invite", "machine", "owner"]
        );
        assert_eq!(
            get(entry, "invite"),
            Some(&Val::from(
                crate::invite::secret_hash(&secret)[..12].to_string()
            ))
        );
        assert_eq!(get(entry, "admin"), None);
        // Used up: the store remembers it as used, and a second key is told so.
        assert!(f.stored().iter().all(|e| e.uses == 0));
        assert_eq!(code(f.redeem(&second, "other-box", &secret)), "invite_used");
        // The enrolled key asking again is told it is enrolled, invite or not.
        assert_eq!(f.redeem(&first, "whatever", "x").unwrap(), approved);
    }

    #[test]
    fn wrong_expired_and_mismatched_invites_are_refused() {
        let f = Fixture::new();
        let stranger = "c".repeat(64);
        let secret = f.invite(1, 600, None);
        let other = crate::invite::new_secret();
        assert_eq!(
            code(f.redeem(&stranger, "new-box", &other)),
            "invalid_invite"
        );
        assert_eq!(
            code(f.redeem(&stranger, "new-box", "short")),
            "invalid_invite"
        );
        assert_eq!(
            code(f.redeem(&stranger, "new-box", &secret[..secret.len() - 1])),
            "invalid_invite"
        );
        // A name the invite fixes must be the one asked for.
        let fixed = f.invite(1, 600, Some("laptop"));
        assert_eq!(
            code(f.redeem(&stranger, "new-box", &fixed)),
            "invalid_invite"
        );
        // A taken name is refused without using the invite.
        assert_eq!(code(f.redeem(&stranger, "a", &secret)), "machine_taken");
        assert_eq!(f.stored().iter().map(|e| e.uses).sum::<i64>(), 2);
        // Expiry, on the wall clock.
        step(595.0);
        f.redeem(&stranger, "laptop", &fixed).unwrap();
        step(10.0);
        assert_eq!(
            code(f.redeem(&"d".repeat(64), "new-box", &secret)),
            "invite_expired"
        );
        // A week after it expired, it is forgotten.
        step(7.0 * 86400.0);
        f.invite(1, 600, None);
        assert_eq!(
            code(f.redeem(&"d".repeat(64), "new-box", &secret)),
            "invalid_invite"
        );
    }

    #[test]
    fn requests_without_a_usable_invite_are_refused() {
        let f = Fixture::new();
        let stranger = "c".repeat(64);
        let ask = |body: &str| {
            f.hub.enroll_unknown(
                &stranger,
                "192.0.2.8",
                &jsonval::loads(body.as_bytes()).unwrap(),
            )
        };
        assert_eq!(
            code(ask(r#"{"machine": "new-box", "owner": "sam"}"#)),
            "invite_required"
        );
        assert_eq!(
            code(ask(
                r#"{"machine": "new-box", "owner": "sam", "invite": 5}"#
            )),
            "invalid_enrollment"
        );
        // The first-administrator token of earlier servers is not accepted.
        assert_eq!(
            code(ask(
                r#"{"machine": "new-box", "owner": "sam", "bootstrap_token": "x"}"#
            )),
            "invalid_enrollment"
        );
        assert_eq!(code(ask(r#"["new-box"]"#)), "invalid_enrollment");
        assert_eq!(
            code(ask(r#"{"machine": "New", "owner": "sam", "invite": "x"}"#)),
            "invalid_machine"
        );
        assert_eq!(
            code(ask(r#"{"machine": "new-box", "owner": "", "invite": "x"}"#)),
            "invalid_label"
        );
        assert_eq!(
            code(f.hub.auth(&stranger).map(|_| Val::Null)),
            "not_enrolled"
        );
    }

    #[test]
    fn use_counts_allow_that_many_machines() {
        let f = Fixture::new();
        let secret = f.invite(3, 600, None);
        for (index, fp) in ["c", "d", "e"].iter().enumerate() {
            f.redeem(&fp.repeat(64), &format!("box-{index}"), &secret)
                .unwrap();
            assert_eq!(f.stored().first().map_or(0, |e| e.uses), 2 - index as i64);
        }
        assert_eq!(
            code(f.redeem(&"f".repeat(64), "box-9", &secret)),
            "invite_used"
        );
        // Other invites are untouched; spent and expired ones are remembered.
        let keep = f.invite(1, 600, None);
        f.invite(1, 60, None);
        step(61.0);
        let spare = f.invite(2, 600, None);
        f.redeem(&"0".repeat(64), "box-x", &spare).unwrap();
        let left: Vec<i64> = f.stored().iter().map(|e| e.uses).collect();
        assert_eq!(left, [0, 1, 1, 1]);
        f.redeem(&"1".repeat(64), "box-y", &keep).unwrap();
    }

    #[test]
    fn concurrent_redemptions_of_one_invite_enroll_one_machine() {
        let f = Fixture::new();
        // A second server state over the same files stands for another
        // process: only the registry lock orders it against the first.
        let other = Hub::open(Config {
            db: format!("{}/messages.db", f.root),
            peers_file: f.peers.clone(),
            registry_lock: None,
        })
        .unwrap();
        for uses in [1, 3] {
            let secret = f.invite(uses, 600, None);
            let tag = format!("{uses}");
            let outcomes: Vec<Out<Val>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..8)
                    .map(|index| {
                        let (secret, hub, tag) = (&secret, if index % 2 == 0 { &f.hub } else { &other }, &tag);
                        scope.spawn(move || {
                            let fp = format!("{tag}{index}").repeat(32);
                            let body = jsonval::loads(
                                format!(
                                    r#"{{"machine": "box-{tag}-{index}", "owner": "sam", "invite": "{secret}"}}"#
                                )
                                .as_bytes(),
                            )
                            .unwrap();
                            hub.enroll_unknown(&fp, &format!("198.51.100.{index}"), &body)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let won = outcomes.iter().filter(|o| o.is_ok()).count();
            assert_eq!(won as i64, uses, "{outcomes:?}");
            assert!(outcomes
                .iter()
                .filter_map(|o| o.as_ref().err())
                .all(|fail| matches!(fail, Fail::Api("invite_used", 403))));
            let registry = jsonval::loads(&std::fs::read(&f.peers).unwrap()).unwrap();
            let enrolled = parse_registry(&registry)
                .unwrap()
                .values()
                .filter(|p| p.machine.starts_with(&format!("box-{tag}-")))
                .count();
            assert_eq!(enrolled as i64, uses);
            assert!(f.stored().iter().all(|e| e.uses == 0));
        }
    }

    #[test]
    fn an_unsafe_or_damaged_invite_store_fails_closed() {
        let f = Fixture::new();
        let stranger = "c".repeat(64);
        let secret = f.invite(1, 600, None);
        let store = f.store();
        let saved = format!("{}/saved.json", f.root);
        let unavailable = |f: &Fixture| code(f.redeem(&stranger, "new-box", &secret));
        // A link, even to the real store.
        std::fs::rename(&store, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &store).unwrap();
        assert_eq!(unavailable(&f), "enrollment_unavailable");
        std::fs::remove_file(&store).unwrap();
        // A second hard link.
        std::fs::hard_link(&saved, &store).unwrap();
        assert_eq!(unavailable(&f), "enrollment_unavailable");
        std::fs::remove_file(&saved).unwrap();
        // A mode other than 0600.
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(unavailable(&f), "enrollment_unavailable");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Damaged contents.
        let good = std::fs::read(&store).unwrap();
        for bad in [&b"{not json"[..], b"[]", br#"{"abc": {"hash": "x"}}"#] {
            std::fs::write(&store, bad).unwrap();
            assert_eq!(unavailable(&f), "enrollment_unavailable");
        }
        std::fs::write(&store, &good).unwrap();
        // A linked registry lock.
        let lock = format!("{}.lock", f.peers);
        let _ = std::fs::remove_file(&lock);
        std::os::unix::fs::symlink(format!("{}/elsewhere", f.root), &lock).unwrap();
        let linked = f.redeem(&stranger, "new-box", &secret);
        assert!(matches!(linked, Err(Fail::Unavailable)), "{linked:?}");
        assert!(std::fs::symlink_metadata(format!("{}/elsewhere", f.root)).is_err());
        std::fs::remove_file(&lock).unwrap();
        assert_eq!(
            code(f.hub.auth(&stranger).map(|_| Val::Null)),
            "not_enrolled"
        );
        // Restored, the invite works, and the store is written back private.
        f.invite(1, 600, None);
        f.redeem(&stranger, "new-box", &secret).unwrap();
        let info = std::fs::symlink_metadata(&store).unwrap();
        assert!(info.file_type().is_file());
        assert_eq!((info.mode() & 0o7777, info.nlink()), (0o600, 1));
        assert_eq!(f.stored().iter().filter(|e| e.uses > 0).count(), 1);
    }

    #[test]
    fn no_invite_store_means_no_invites() {
        let f = Fixture::new();
        assert!(std::fs::symlink_metadata(f.store()).is_err());
        assert_eq!(
            code(f.redeem(&"c".repeat(64), "new-box", &crate::invite::new_secret())),
            "invalid_invite"
        );
        assert!(std::fs::symlink_metadata(f.store()).is_err());
    }

    impl Fixture {
        /// Stop and start the server over the same database and registry.
        fn restart(&mut self) {
            self.hub = Hub::open(Config {
                db: format!("{}/messages.db", self.root),
                peers_file: self.peers.clone(),
                registry_lock: None,
            })
            .unwrap();
        }

        fn register_ack(&self, fp: &str, sid: &str) -> String {
            let body = format!(
                r#"{{"sessions": [{{"id": "{sid}", "name": "chat", "status": "idle"}}], "capabilities": ["ack"]}}"#
            );
            let value = self.call(fp, "/v1/register", &body).unwrap();
            field(&value, "registration").as_str().unwrap().to_string()
        }

        fn send(&self, fp: &str, from: &str, to: &str) -> String {
            let id = uuid4();
            let body = format!(
                r#"{{"id": "{id}", "from": "{from}", "to": "{to}", "text": "t", "mode": "prompting"}}"#
            );
            self.call(fp, "/v1/send", &body).unwrap();
            id
        }

        fn status(&self, fp: &str, id: &str, state: &str) -> Out<Val> {
            self.call(
                fp,
                "/v1/status",
                &format!(r#"{{"id": "{id}", "state": "{state}"}}"#),
            )
        }

        /// One poll's events (registration and revision as given).
        fn poll(&self, fp: &str, registration: &str, acks: &[String]) -> Vec<Dict> {
            let acks = acks
                .iter()
                .map(|a| format!(r#""{a}""#))
                .collect::<Vec<_>>()
                .join(", ");
            let body = format!(
                r#"{{"registration": "{registration}", "peers_revision": null, "acks": [{acks}]}}"#
            );
            let Val::List(events) =
                field(&self.call(fp, "/v1/poll", &body).unwrap(), "events").clone()
            else {
                panic!("events")
            };
            events
                .into_iter()
                .map(|e| e.as_dict().unwrap().clone())
                .collect()
        }

        fn notices(&self) -> i64 {
            let g = self.hub.lock();
            g.db.query_row("SELECT count(*) FROM notices", [], |row| row.get(0))
                .unwrap()
        }
    }

    fn statuses(events: &[Dict]) -> Vec<(String, String, String)> {
        let mut found: Vec<_> = events
            .iter()
            .filter(|e| event_str(e, "type") == "status")
            .map(|e| {
                (
                    event_str(e, "id").to_string(),
                    event_str(e, "state").to_string(),
                    event_str(e, "detail").to_string(),
                )
            })
            .collect();
        found.sort();
        found
    }

    fn ack_ids(events: &[Dict]) -> Vec<String> {
        events
            .iter()
            .map(|e| event_str(e, "ack_id").to_string())
            .collect()
    }

    #[test]
    fn a_restart_tells_each_sender_on_its_return() {
        let mut f = Fixture::new();
        let (sa, sb) = (uuid4(), uuid4());
        f.register_ack(A, &sa);
        let b = f.register(B, &sb);
        let (from, to) = (format!("a/{sa}"), format!("b/{sb}"));
        let routed = f.send(A, &from, &to);
        let held = f.send(A, &from, &to);
        let handed = f.send(A, &from, &to);
        let later = f.send(A, &from, &to);
        f.poll(B, &b, &[]);
        f.status(B, &held, "held").unwrap();
        f.status(B, &handed, "handed_to_claude").unwrap();
        f.restart();
        assert_eq!(f.notices(), 3);
        // The recipient reports while the sender is away: that newer
        // observation replaces the restart notice for its message.
        f.register(B, &sb);
        f.status(B, &later, "handed_to_claude").unwrap();
        // Registering twice before polling queues each notice once.
        let a = f.register_ack(A, &sa);
        f.register_ack(A, &sa);
        let events = f.poll(A, &a, &[]);
        let restart = RESTART_DETAIL.to_string();
        let mut expected = vec![
            (routed.clone(), "unknown".into(), restart.clone()),
            (held.clone(), "unknown".into(), restart.clone()),
            (later.clone(), "handed_to_claude".into(), String::new()),
        ];
        expected.sort();
        assert_eq!(statuses(&events), expected);
        assert!(events
            .iter()
            .all(|e| get(e, "notice").is_none() && get(e, "queued").is_none()));
        assert_eq!(f.state_of(&handed), "handed_to_claude");
        // Unacknowledged notices survive another restart; acknowledged ones are gone.
        f.restart();
        let a = f.register_ack(A, &sa);
        let events = f.poll(A, &a, &[]);
        assert_eq!(statuses(&events), expected);
        assert!(f.poll(A, &a, &ack_ids(&events)).is_empty());
        assert_eq!(f.notices(), 0);
        f.restart();
        let a = f.register_ack(A, &sa);
        assert!(f.poll(A, &a, &[]).is_empty());
    }

    #[test]
    fn a_newer_observation_outlives_the_acknowledged_notice_it_replaced() {
        let f = Fixture::new();
        let (sa, sb) = (uuid4(), uuid4());
        let a = f.register_ack(A, &sa);
        let b = f.register(B, &sb);
        let (to_a, to_b) = (format!("a/{sa}"), format!("b/{sb}"));
        let message = f.send(A, &to_a, &to_b);
        f.poll(B, &b, &[]);
        let fill = || {
            for _ in 0..QUEUE_LIMIT {
                f.send(B, &to_b, &to_a);
            }
        };
        // The sender's queue is full, so the observation is stored.
        fill();
        f.status(B, &message, "held").unwrap();
        assert_eq!(f.notices(), 1);
        let first = f.poll(A, &a, &[]);
        assert!(statuses(&first).is_empty());
        // The stored notice goes out and is in flight, unacknowledged.
        let second = f.poll(A, &a, &ack_ids(&first));
        assert_eq!(
            statuses(&second),
            vec![(message.clone(), "held".into(), String::new())]
        );
        // A newer observation replaces the stored row while the queue is
        // full again; the replacement can take the same rowid.
        fill();
        f.status(B, &message, "refused").unwrap();
        assert_eq!(f.notices(), 1);
        // Acknowledging the old notice must not delete the newer one.
        let third = f.poll(A, &a, &ack_ids(&second));
        assert_eq!(f.notices(), 1);
        let fourth = f.poll(A, &a, &ack_ids(&third));
        let mut seen = statuses(&third);
        seen.extend(statuses(&fourth));
        assert_eq!(
            seen,
            vec![(message.clone(), "refused".into(), String::new())]
        );
        let last = f.poll(A, &a, &ack_ids(&fourth));
        assert!(statuses(&last).is_empty());
        assert_eq!(f.notices(), 0);
    }

    #[test]
    fn a_reused_rowid_does_not_make_an_acknowledgment_delete_another_notice() {
        let f = Fixture::new();
        let (sa, sb) = (uuid4(), uuid4());
        let a = f.register_ack(A, &sa);
        let b = f.register(B, &sb);
        let (to_a, to_b) = (format!("a/{sa}"), format!("b/{sb}"));
        let fill = || {
            for _ in 0..QUEUE_LIMIT {
                f.send(B, &to_b, &to_a);
            }
        };
        let old = f.send(A, &to_a, &to_b);
        f.poll(B, &b, &[]);
        fill();
        f.status(B, &old, "held").unwrap();
        let first = f.poll(A, &a, &[]);
        let second = f.poll(A, &a, &ack_ids(&first));
        assert_eq!(
            statuses(&second),
            vec![(old.clone(), "held".into(), String::new())]
        );
        // While that notice is in flight, retention removes its message and
        // with it the stored row, the table's newest.
        {
            let g = f.hub.lock();
            g.db.execute("DELETE FROM messages WHERE id=?", [&old])
                .unwrap();
            g.db.execute(
                "DELETE FROM notices WHERE id NOT IN (SELECT id FROM messages)",
                [],
            )
            .unwrap();
        }
        // A notice for another message is stored and takes the free rowid.
        let new = f.send(A, &to_a, &to_b);
        f.poll(B, &b, &[]);
        fill();
        f.status(B, &new, "refused").unwrap();
        assert_eq!(f.notices(), 1);
        // Acknowledging the first notice leaves the second stored, and it
        // is delivered.
        let third = f.poll(A, &a, &ack_ids(&second));
        assert_eq!(f.notices(), 1);
        let fourth = f.poll(A, &a, &ack_ids(&third));
        let mut seen = statuses(&third);
        seen.extend(statuses(&fourth));
        assert_eq!(seen, vec![(new.clone(), "refused".into(), String::new())]);
        f.poll(A, &a, &ack_ids(&fourth));
        assert_eq!(f.notices(), 0);
    }

    #[test]
    fn a_status_for_a_full_sender_queue_is_kept_not_refused() {
        let f = Fixture::new();
        let (sa, sb) = (uuid4(), uuid4());
        let a = f.register(A, &sa);
        let b = f.register(B, &sb);
        let message = f.send(A, &format!("a/{sa}"), &format!("b/{sb}"));
        f.poll(B, &b, &[]);
        for _ in 0..QUEUE_LIMIT {
            f.send(B, &format!("b/{sb}"), &format!("a/{sa}"));
        }
        // Before: 503 queue_full, and the observation was never recorded.
        f.status(B, &message, "refused").unwrap();
        assert_eq!(f.state_of(&message), "refused");
        let first = f.poll(A, &a, &[]);
        assert_eq!(first.len(), QUEUE_LIMIT);
        assert!(statuses(&first).is_empty());
        let second = f.poll(A, &a, &[]);
        assert_eq!(
            statuses(&second),
            vec![(message, "refused".into(), String::new())]
        );
        // Consumed on poll by a machine without acknowledgments.
        assert_eq!(f.notices(), 0);
    }

    #[test]
    fn statuses_wait_for_a_sender_that_was_away() {
        let f = Fixture::new();
        let (sa, sb) = (uuid4(), uuid4());
        let a = f.register_ack(A, &sa);
        let b = f.register(B, &sb);
        let (from, to) = (format!("a/{sa}"), format!("b/{sb}"));
        let refused = f.send(A, &from, &to);
        let expired = f.send(A, &from, &to);
        f.poll(B, &b, &[]);
        f.status(B, &refused, "refused").unwrap();
        // Handed out but never acknowledged before the sender's lease lapses.
        let events = f.poll(A, &a, &[]);
        assert_eq!(statuses(&events).len(), 1);
        step(31.0);
        f.register(B, &sb);
        assert_eq!(f.state_of(&expired), "unknown");
        let a = f.register_ack(A, &sa);
        let events = f.poll(A, &a, &[]);
        let mut expected = vec![
            (refused, "refused".into(), String::new()),
            (
                expired,
                "unknown".into(),
                "delivery not confirmed within 30 seconds".into(),
            ),
        ];
        expected.sort();
        assert_eq!(statuses(&events), expected);
        f.poll(A, &a, &ack_ids(&events));
        assert_eq!(f.notices(), 0);
    }

    #[test]
    fn waiting_requests_leave_handlers_for_short_requests() {
        let f = Fixture::new();
        let fps: Vec<String> = (0..WAITING_LIMIT)
            .map(|i| format!("{:064x}", i + 1))
            .collect();
        for (index, fp) in fps.iter().enumerate() {
            f.hub.enter(fp, index % 2 == 0, true).unwrap();
        }
        assert_eq!(
            code(f.hub.enter(A, true, true).map(|()| Val::Null)),
            "rate_limited"
        );
        assert_eq!(
            code(f.hub.enter(A, false, true).map(|()| Val::Null)),
            "rate_limited"
        );
        f.hub.enter(A, false, false).unwrap();
        f.hub.leave(A, false, false);
        f.hub.leave(&fps[0], true, true);
        f.hub.enter(A, true, true).unwrap();
    }
}
