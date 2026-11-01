//! The per-chat helper: one process per visible remote chat.
//!
//! It registers as a genuine native Claude Code peer (its own PID, socket,
//! session UUID, and auth token), speaks the native peer socket protocol to
//! local chats, and talks to the bridge over line-delimited JSON on
//! stdin/stdout. Product versions are descriptive registry metadata, not
//! acceptance gates; native protocol and process identity checks still apply.
//!
//! The bridge alone supplies trusted IPC commands; local senders must prove
//! their kernel PID and match an explicitly enrolled live native registry
//! identity. No shell commands, settings changes, or native management
//! controls are exposed.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::discovery::{
    auth_key, discover, discover_one, same_identity, session_record, socket_path, Record, Selection,
};
use crate::error::{Error, Kind, Result};
use crate::fsutil;
use crate::identity::{self, monotonic};
use crate::net::{self, poll_fds, pollfd, Stream};
use crate::obj;
use crate::protocol::{
    self, address, clean_name, envelope, notice_text, parse_envelope, path_from_address,
    receipt_marker, receipt_state, unjoined_text, valid_hop_chain, valid_uuid, Attrs,
    ACTIVITY_STATES, DROP_REASONS, FRAME_LIMIT, MODES, NOTICE_STATES, TEXT_LIMIT,
};
use crate::text::take_chars;
use crate::wirejson::{self, as_int, int_cmp, truthy, value_eq};

/// Seconds a native receipt correlation is kept.
pub const RECEIPT_WINDOW: f64 = 86400.0;

pub type Event = Map<String, Value>;
pub type Emit = Arc<dyn Fn(Event) -> Result<()> + Send + Sync>;
pub type FrameSender =
    Arc<dyn Fn(&str, &Selection, &Record, &Map<String, Value>) -> Result<()> + Send + Sync>;

/// A message handed toward a local chat, awaiting a native receipt.
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: String,
    pub record: Record,
    pub at: f64,
    pub state: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Note {
    pub text: String,
    pub mode: Option<String>,
}

/// Correlation state, guarded by one lock.
#[derive(Default)]
pub struct State {
    pub pending: IndexMap<String, Pending>,
    pub seen: IndexMap<String, f64>,
    pub outbound: IndexMap<String, Record>,
    pub unjoined_notices: IndexMap<String, f64>,
    pub outbound_notes: IndexMap<String, Note>,
    pub handed: IndexMap<String, ()>,
    pub noticed: IndexMap<String, ()>,
}

/// The configured view: selection, revision, name, and the owned registry record.
#[derive(Clone)]
pub struct View {
    pub selected: Selection,
    pub revision: String,
    pub name: String,
    pub registry_record: Map<String, Value>,
}

struct Channels {
    listener: Option<OwnedFd>,
    wake_read: OwnedFd,
    wake_write: OwnedFd,
    main_read: OwnedFd,
    main_write: OwnedFd,
}

pub struct Peer {
    pub route: String,
    pub pid: i64,
    pub sid: String,
    pub start: String,
    token: String,
    pub path: String,
    pub config_dir: String,
    pub registry: String,
    pub registry_path: String,
    pub state_dir: String,
    pub socket_dir: String,
    pub domain: String,
    emit: Emit,
    send: FrameSender,
    auth_lock: Mutex<()>,
    view: Mutex<View>,
    pub state: Mutex<State>,
    owned: Mutex<Vec<(String, u64)>>,
    stop: AtomicBool,
    serve_lock: Mutex<bool>,
    channels: RwLock<Option<Channels>>,
    /// Listener loop passes; lets tests confirm the idle wait never spins.
    pub serve_passes: AtomicUsize,
}

fn bounded_insert<V>(store: &mut IndexMap<String, V>, key: String, value: V, limit: usize) {
    if store.len() >= limit {
        store.shift_remove_index(0);
    }
    store.insert(key, value);
}

fn socketpair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds is a valid two-element array.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(Error::errno(Error::last_os()));
    }
    // SAFETY: both descriptors are fresh and owned here.
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [pair.0.as_raw_fd(), pair.1.as_raw_fd()] {
        net::set_nonblocking(fd, true);
        // SAFETY: fcntl on a valid descriptor.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(pair)
}

fn wake_fd(fd: RawFd) {
    let byte = 0u8;
    // SAFETY: one readable byte; errors (full or closed channel) are ignored
    // because a full channel already carries a wakeup.
    unsafe {
        libc::send(
            fd,
            &byte as *const u8 as *const libc::c_void,
            1,
            net_send_flags(),
        )
    };
}

fn net_send_flags() -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        libc::MSG_NOSIGNAL
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Read and discard every pending wakeup byte.
pub fn drain(fd: RawFd) {
    let mut buffer = [0u8; 4096];
    loop {
        // SAFETY: buffer is valid and writable.
        let count = unsafe {
            libc::recv(
                fd,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                0,
            )
        };
        if count <= 0 {
            if count < 0 && Error::last_os() == libc::EINTR {
                continue;
            }
            return;
        }
    }
}

fn bind_listener(path: &str) -> Result<OwnedFd> {
    let fd = net::unix_socket()?;
    let (addr, len) = net::sockaddr(path)?;
    // SAFETY: addr/len describe a valid sockaddr_un.
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            len,
        )
    } != 0
    {
        return Err(Error::errno(Error::last_os()));
    }
    Ok(fd)
}

fn listen(fd: &OwnedFd, backlog: i32) -> Result<()> {
    // SAFETY: listen on a bound socket.
    if unsafe { libc::listen(fd.as_raw_fd(), backlog) } != 0 {
        return Err(Error::errno(Error::last_os()));
    }
    net::set_nonblocking(fd.as_raw_fd(), true);
    Ok(())
}

fn str_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

fn mode_of(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|m| MODES.contains(m))
        .map(str::to_string)
}

/// The config's `sessions` as a set, keeping only members that can equal a
/// UUID: a list contributes its strings (a nested list or object is a type
/// error, other values are skipped), a string its characters, an object its
/// keys; anything else is a type error, with the details clients already see.
fn selection_from(value: Option<&Value>) -> Result<Selection> {
    match value {
        None => Err(Error::value("'sessions'")),
        Some(Value::Array(items)) => {
            let mut out = Selection::new();
            for item in items {
                match item {
                    Value::String(sid) => {
                        out.insert(sid.clone());
                    }
                    Value::Array(_) | Value::Object(_) => {
                        return Err(Error::type_error(format!(
                            "unhashable type: '{}'",
                            crate::discovery::type_name(Some(item))
                        )))
                    }
                    _ => {}
                }
            }
            Ok(out)
        }
        Some(Value::String(text)) => Ok(text.chars().map(String::from).collect()),
        Some(Value::Object(map)) => Ok(map.keys().cloned().collect()),
        Some(other) => Err(Error::type_error(format!(
            "'{}' object is not iterable",
            crate::discovery::type_name(Some(other))
        ))),
    }
}

impl Peer {
    /// Register a native peer for one remote route.
    pub fn new(
        config: &Map<String, Value>,
        route: &str,
        name: &str,
        socket_dir: &str,
        emit: Emit,
        status: &str,
    ) -> Result<Arc<Self>> {
        if !ACTIVITY_STATES.contains(&status) {
            return Err(Error::value("invalid remote activity"));
        }
        let (machine, sid) = route
            .split_once('/')
            .ok_or_else(|| Error::value("not enough values to unpack (expected 2, got 1)"))?;
        protocol::valid_uuid_str(sid)?;
        let machine_ok = (1..=64).contains(&machine.len())
            && machine
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !machine_ok {
            return Err(Error::value("invalid remote machine"));
        }
        let name = clean_name(name);
        if name.is_empty() {
            return Err(Error::value("empty proxy name"));
        }
        let selected = selection_from(config.get("sessions"))?;
        let config_dir = fsutil::owned_config_dir(&crate::discovery::path_value(Some(
            config
                .get("config_dir")
                .ok_or_else(|| Error::value("'config_dir'"))?,
        ))?)?;
        let registry = fsutil::private_dir(&fsutil::join(&config_dir, "sessions"), false)?;
        let state_dir = fsutil::private_dir(
            &crate::discovery::path_value(Some(
                config
                    .get("state_dir")
                    .ok_or_else(|| Error::value("'state_dir'"))?,
            ))?,
            true,
        )?;
        let socket_dir = fsutil::private_dir(socket_dir, false)?;
        let records = discover(&config_dir, &selected)?;
        let mut domains: Vec<Value> = Vec::new();
        for record in records.values() {
            let domain = record.get("pidDomain").cloned().unwrap_or(Value::Null);
            if !domains.iter().any(|d| value_eq(d, &domain)) {
                domains.push(domain);
            }
        }
        let mut same_namespace = true;
        for record in records.values() {
            if fsutil::parent(&socket_path(record)?) != socket_dir {
                same_namespace = false;
            }
        }
        if domains.len() != 1 || !same_namespace {
            return Err(Error::value(
                "selected sessions need one live domain/socket namespace",
            ));
        }
        let domain = domains
            .pop()
            .and_then(|d| d.as_str().map(str::to_string))
            .unwrap_or_default();
        // Mixed releases are valid. Stable UUID ordering selects descriptive
        // metadata without comparing or interpreting product version values.
        let metadata_source = records
            .iter()
            .min_by(|a, b| a.0.cmp(b.0))
            .map(|(_, r)| r.clone())
            .expect("one record");
        let pid = std::process::id() as i64;
        let sid = identity::uuid4();
        let start = identity::process_start(pid)?;
        let token = identity::token_hex16();
        let path = fsutil::join(&socket_dir, &format!("{pid}.sock"));
        if path.len() > 103 {
            return Err(Error::value("native socket path too long"));
        }
        let (wake_read, wake_write) = socketpair()?;
        let (main_read, main_write) = socketpair()?;
        let registry_path = fsutil::join(&registry, &format!("{pid}.json"));
        let peer = Arc::new(Peer {
            route: route.to_string(),
            pid,
            sid: sid.clone(),
            start: start.clone(),
            token: token.clone(),
            path: path.clone(),
            config_dir,
            registry: registry.clone(),
            registry_path: registry_path.clone(),
            state_dir: state_dir.clone(),
            socket_dir,
            domain: domain.clone(),
            emit,
            send: Arc::new(net::send_frame),
            auth_lock: Mutex::new(()),
            view: Mutex::new(View {
                selected,
                revision: "0".into(),
                name: name.clone(),
                registry_record: Map::new(),
            }),
            state: Mutex::new(State::default()),
            owned: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            serve_lock: Mutex::new(false),
            channels: RwLock::new(Some(Channels {
                listener: None,
                wake_read,
                wake_write,
                main_read,
                main_write,
            })),
            serve_passes: AtomicUsize::new(0),
        });
        let result = (|| -> Result<()> {
            let listener = bind_listener(&path)?;
            peer.channels
                .write()
                .unwrap()
                .as_mut()
                .expect("open")
                .listener = Some(listener);
            peer.own(&path, fsutil::lstat(&path)?.ino());
            listen(
                peer.channels
                    .read()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .listener
                    .as_ref()
                    .unwrap(),
                16,
            )?;
            let now = identity::epoch_millis();
            let mut record = obj! {
                "pid" => pid,
                "sessionId" => sid.clone(),
                "procStart" => start.clone(),
                "pidDomain" => domain.clone(),
                "peerProtocol" => 1,
                "peerFeatures" => Value::Array(vec![]),
                "kind" => "interactive",
                "name" => name.clone(),
                "nameSource" => "user",
                "nameSince" => now,
                "startedAt" => identity::epoch_millis(),
                "cwd" => state_dir.clone(),
                "messagingSocketPath" => path.clone(),
                "peerBridgeProxy" => true,
            };
            if status != "unknown" {
                record.insert("status".into(), Value::from(status));
            }
            if let Some(version) = metadata_source.get("version") {
                record.insert("version".into(), version.clone());
            }
            let key = fsutil::join(
                &registry,
                &format!("{pid}.{}.key", crate::discovery::socket_digest(&path)),
            );
            let key_record = obj! {"peerToken" => token.clone(), "procStart" => start.clone(), "pidDomain" => domain.clone()};
            peer.own(&key, fsutil::write_new(&key, &key_record)?);
            peer.own(&registry_path, fsutil::write_new(&registry_path, &record)?);
            peer.view.lock().unwrap().registry_record = record;
            let ledger = fsutil::join(&state_dir, &format!("proxy-{pid}-{sid}.json"));
            let ledger_record = obj! {
                "route" => route,
                "registry" => registry_path.clone(),
                "socket" => path.clone(),
                "pid" => pid,
                "procStart" => start.clone(),
                "sessionId" => sid.clone(),
            };
            peer.own(&ledger, fsutil::write_new(&ledger, &ledger_record)?);
            Ok(())
        })();
        if let Err(exc) = result {
            let _ = peer.close();
            return Err(exc);
        }
        Ok(peer)
    }

    fn own(&self, path: &str, inode: u64) {
        let mut owned = self.owned.lock().unwrap();
        match owned.iter_mut().find(|(p, _)| p == path) {
            Some(entry) => entry.1 = inode,
            None => owned.push((path.to_string(), inode)),
        }
    }

    fn owned_inode(&self, path: &str) -> Option<u64> {
        self.owned
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, i)| *i)
    }

    fn disown(&self, path: &str) {
        self.owned.lock().unwrap().retain(|(p, _)| p != path);
    }

    pub fn view(&self) -> View {
        self.view.lock().unwrap().clone()
    }

    pub fn name(&self) -> String {
        self.view.lock().unwrap().name.clone()
    }

    pub fn selected(&self) -> Selection {
        self.view.lock().unwrap().selected.clone()
    }

    pub fn address(&self) -> String {
        address(&self.path)
    }

    pub fn emit(&self, event: Event) -> Result<()> {
        (self.emit)(event)
    }

    pub fn status(&self, identifier: &str, state: &str, detail: Option<&str>) -> Result<()> {
        let mut event = obj! {"event" => "status", "id" => identifier, "state" => state};
        if let Some(detail) = detail.filter(|d| !d.is_empty()) {
            event.insert("detail".into(), Value::from(detail));
        }
        self.emit(event)
    }

    fn user_frame(&self, session: &str, attrs: &Attrs, text: &str) -> Result<Map<String, Value>> {
        let message = obj! {"role" => "user", "content" => envelope(attrs, text)?};
        Ok(obj! {
            "msgV" => 1,
            "msg_id" => identity::uuid4(),
            "type" => "user",
            "session_id" => session,
            "message" => Value::Object(message),
            "priority" => "next",
            "from" => self.address(),
        })
    }

    fn proxy_attrs(&self, name: &str, mode: &str) -> Attrs {
        let mut attrs = Attrs::new();
        attrs.insert("from".into(), self.address());
        attrs.insert("from-session".into(), self.sid.clone());
        attrs.insert("from-name".into(), name.to_string());
        attrs.insert("from-mode".into(), mode.to_string());
        attrs
    }

    /// Apply a trusted bridge `configure` command.
    pub fn configure(&self, command: &Map<String, Value>) -> Result<()> {
        if let Some(status) = command.get("status") {
            if !status
                .as_str()
                .is_some_and(|s| ACTIVITY_STATES.contains(&s))
            {
                return Err(Error::value("invalid remote activity"));
            }
        }
        let revision = match command.get("revision").and_then(as_int) {
            Some(revision) if int_cmp(&revision, "1") != std::cmp::Ordering::Less => revision,
            _ => return Err(Error::value("invalid configuration revision")),
        };
        let name = match command.get("name") {
            Some(Value::String(name)) if !name.is_empty() && clean_name(name) == *name => {
                name.clone()
            }
            _ => return Err(Error::value("invalid native name")),
        };
        let sessions = match command.get("sessions") {
            Some(Value::Array(items)) if items.len() <= 32 => items,
            _ => return Err(Error::value("invalid selection list")),
        };
        let mut selected = Selection::new();
        for sid in sessions {
            selected.insert(valid_uuid(Some(sid))?);
        }
        if selected.len() != sessions.len() {
            return Err(Error::value("duplicate selected UUID"));
        }
        let _auth = self.auth_lock.lock().unwrap();
        let current = self.view();
        if int_cmp(&revision, &current.revision) != std::cmp::Ordering::Greater {
            return Err(Error::value("stale configuration revision"));
        }
        let records = discover(&self.config_dir, &selected)?;
        let mut replace = records.is_empty();
        for record in records.values() {
            if replace {
                break;
            }
            replace = !wirejson::value_eq_opt(
                record.get("pidDomain"),
                Some(&Value::from(self.domain.clone())),
            ) || fsutil::parent(&socket_path(record)?) != self.socket_dir;
        }
        if replace {
            return Err(Error::value("selection requires helper replacement"));
        }
        let mut record = current.registry_record.clone();
        record.insert("name".into(), Value::from(name.clone()));
        if let Some(status) = command.get("status").and_then(Value::as_str) {
            // Native decodes unknown statuses as absent; never imply idle.
            if status == "unknown" {
                record.shift_remove("status");
            } else {
                record.insert("status".into(), Value::from(status));
            }
        }
        if name != current.name {
            record.insert("nameSince".into(), Value::from(identity::epoch_millis()));
        }
        // Never replace a registry file another process has taken over.
        // The private parent protects the check/rename from other users.
        fsutil::private_dir(&self.registry, false)?;
        let changed = || Error::value("proxy registry ownership changed");
        if Some(fsutil::lstat(&self.registry_path)?.ino()) != self.owned_inode(&self.registry_path)
        {
            return Err(changed());
        }
        let temporary = fsutil::join(
            &self.registry,
            &format!(".proxy-{}-{}.tmp", self.pid, identity::uuid4_hex()),
        );
        let inode = fsutil::write_new(&temporary, &record)?;
        self.own(&temporary, inode);
        let result = (|| -> Result<()> {
            if Some(fsutil::lstat(&self.registry_path)?.ino())
                != self.owned_inode(&self.registry_path)
            {
                return Err(changed());
            }
            fsutil::replace(&temporary, &self.registry_path)?;
            self.own(&self.registry_path, inode);
            self.disown(&temporary);
            Ok(())
        })();
        if self.owned_inode(&temporary).is_some() {
            let cleanup = (|| -> Result<()> {
                if fsutil::exists(&temporary) && fsutil::lstat(&temporary)?.ino() == inode {
                    fsutil::unlink(&temporary)?;
                }
                Ok(())
            })();
            self.disown(&temporary);
            cleanup?;
        }
        result?;
        {
            let mut view = self.view.lock().unwrap();
            view.registry_record = record;
            view.selected = selected;
            view.name = name;
            view.revision = revision.clone();
        }
        self.emit(obj! {"event" => "configured", "revision" => int_value(&revision)})
    }

    /// Hand a remote chat's message to a selected local chat.
    pub fn deliver(&self, command: &Map<String, Value>) -> Result<()> {
        let identifier = valid_uuid(command.get("id"))?;
        let target = valid_uuid(command.get("target_session"))?;
        let view = self.view();
        if !view.selected.contains(&target) {
            return Err(Error::value("target session not enrolled"));
        }
        let text = str_of(command, "text");
        let mode = mode_of(command.get("mode"));
        let (text, mode) = match (text, mode) {
            (Some(text), Some(mode)) if !text.is_empty() && text.len() <= TEXT_LIMIT => {
                (text.to_string(), mode)
            }
            _ => return Err(Error::value("invalid delivery text/mode")),
        };
        {
            let mut state = self.state.lock().unwrap();
            if state.seen.contains_key(&identifier) {
                return Err(Error::value("duplicate delivery; no replay"));
            }
            if state.pending.len() >= 256 {
                let evictable = state
                    .pending
                    .iter()
                    .position(|(_, value)| value.state.as_deref() != Some("held"));
                match evictable {
                    Some(index) => {
                        state.pending.shift_remove_index(index);
                    }
                    None => return Err(Error::value("helper held-message capacity reached")),
                }
            }
            bounded_insert(&mut state.seen, identifier.clone(), monotonic(), 10000);
        }
        self.wake_listener(); // Recalculate if capacity eviction removed a deadline.
        let record = discover_one(&self.config_dir, &view.selected, &target)?
            .ok_or_else(|| Error::value("target session no longer live"))?;
        if fsutil::parent(&socket_path(&record)?) != self.socket_dir {
            return Err(Error::value("target socket namespace changed"));
        }
        let mut attrs = self.proxy_attrs(&view.name, &mode);
        if truthy(command.get("hop_chain")) {
            attrs.insert(
                "hop-chain".into(),
                valid_hop_chain(command.get("hop_chain"))?,
            );
        }
        let native_id = identity::uuid4();
        let mut frame = self.user_frame(&target, &attrs, &text)?;
        frame.insert("msg_id".into(), Value::from(native_id.clone()));
        {
            let mut state = self.state.lock().unwrap();
            state.pending.insert(
                native_id.clone(),
                Pending {
                    id: identifier.clone(),
                    record: record.clone(),
                    at: monotonic(),
                    state: None,
                },
            );
        }
        self.wake_listener();
        match (self.send)(&self.config_dir, &view.selected, &record, &frame) {
            Ok(()) => {
                // A receipt can arrive before EOF. Never overwrite it with handoff.
                let mut state = self.state.lock().unwrap();
                if let Some(pending) = state.pending.get_mut(&native_id) {
                    if pending.state.is_none() {
                        pending.state = Some("handed_to_claude".into());
                        self.status(
                            &identifier,
                            "handed_to_claude",
                            Some("native transport written"),
                        )?;
                    }
                }
                Ok(())
            }
            Err(exc) if exc.kind == Kind::WriteUnknown => {
                let mut state = self.state.lock().unwrap();
                if let Some(pending) = state.pending.get_mut(&native_id) {
                    if pending.state.is_none() {
                        pending.state = Some("unknown".into());
                        self.status(
                            &identifier,
                            "unknown",
                            Some("native write outcome uncertain; no replay"),
                        )?;
                    }
                }
                Ok(())
            }
            Err(exc) => {
                self.state.lock().unwrap().pending.shift_remove(&native_id);
                self.wake_listener();
                Err(exc)
            }
        }
    }

    /// Relay the bridge's delivery outcome for a message a local chat sent.
    pub fn relay_receipt(&self, command: &Map<String, Value>) -> Result<()> {
        let identifier = valid_uuid(command.get("id"))?;
        let source = self
            .state
            .lock()
            .unwrap()
            .outbound
            .get(&identifier)
            .cloned();
        let source = source.ok_or_else(|| Error::value("unmatched outbound status"))?;
        let Some((status, reason)) = receipt_marker(command.get("detail")) else {
            // Transport failures and uncertain handoffs have no native
            // equivalent; the sender gets a plainly labeled notice instead.
            let state = command
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string);
            let note = {
                let mut store = self.state.lock().unwrap();
                if state.as_deref() == Some("handed_to_claude") {
                    bounded_insert(&mut store.handed, identifier, (), 10000);
                    return Ok(());
                }
                // A late "unknown" after a confirmed handoff (receipt
                // correlation expiry) says nothing new about delivery.
                let state_ok = state.as_deref().is_some_and(|s| NOTICE_STATES.contains(&s));
                if !state_ok
                    || store.noticed.contains_key(&identifier)
                    || store.handed.contains_key(&identifier)
                {
                    return Ok(());
                }
                bounded_insert(&mut store.noticed, identifier.clone(), (), 10000);
                let note = store
                    .outbound_notes
                    .get(&identifier)
                    .cloned()
                    .unwrap_or_default();
                if state.as_deref() != Some("unknown") {
                    store.outbound.shift_remove(&identifier);
                }
                note
            };
            return self.notify(
                &source,
                state.as_deref().expect("notice state"),
                command.get("detail"),
                &note,
            );
        };
        let expected = receipt_state(&status).expect("marker status");
        if command.get("state").and_then(Value::as_str) != Some(expected) {
            return Err(Error::value("native receipt state/detail mismatch"));
        }
        let mut frame = obj! {
            "msgV" => 1,
            "msg_id" => identity::uuid4(),
            "type" => "control",
            "action" => "peer_message_status",
            "status" => if status == "refused" { "expired" } else { status.as_str() },
            "orig_msg_id" => identifier.clone(),
            "session_id" => source.get("sessionId").cloned().unwrap_or(Value::Null),
            "reason" => "Switchboard remote receiver observation",
            "from" => self.address(),
        };
        if status == "refused" {
            frame.insert("status_detail".into(), Value::from("refused"));
        }
        if status == "dropped" {
            frame.insert(
                "dropped_msg_ids".into(),
                Value::Array(vec![Value::from(identifier.clone())]),
            );
            if let Some(reason) = reason {
                frame.insert("drop_reason".into(), Value::from(reason));
            }
        }
        (self.send)(&self.config_dir, &self.selected(), &source, &frame)?;
        if status != "held" {
            self.state
                .lock()
                .unwrap()
                .outbound
                .shift_remove(&identifier);
        }
        Ok(())
    }

    fn notify(
        &self,
        source: &Record,
        state: &str,
        detail: Option<&Value>,
        note: &Note,
    ) -> Result<()> {
        let mode = note
            .mode
            .clone()
            .filter(|m| MODES.contains(&m.as_str()))
            .unwrap_or_else(|| "prompting".into());
        let view = self.view();
        let attrs = self.proxy_attrs(&view.name, &mode);
        let text = notice_text(&view.name, state, detail, &note.text);
        let session = source
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let frame = self.user_frame(&session, &attrs, &text)?;
        // A notice follows the chat (its session UUID), not the process that
        // sent: a chat resumed since then gets it in its new process. Native
        // receipts stay with the exact process, whose state they refer to.
        let current = discover_one(&self.config_dir, &view.selected, &session)?;
        let target = match &current {
            Some(record) if !same_identity(record, source, &["pid", "procStart"]) => record,
            _ => source,
        };
        (self.send)(&self.config_dir, &view.selected, target, &frame)
    }

    /// Adopt an outbound message sent through an earlier helper for this route.
    pub fn track(&self, command: &Map<String, Value>) -> Result<()> {
        let identifier = valid_uuid(command.get("id"))?;
        let source = valid_uuid(command.get("source_session"))?;
        let selected = self.selected();
        let text = str_of(command, "text");
        let mode = mode_of(command.get("mode"));
        let (text, mode) = match (text, mode) {
            (Some(text), Some(mode)) if selected.contains(&source) => (text.to_string(), mode),
            _ => return Err(Error::value("invalid outbound tracking")),
        };
        let record = discover_one(&self.config_dir, &selected, &source)?
            .ok_or_else(|| Error::value("source session no longer live"))?;
        let mut state = self.state.lock().unwrap();
        bounded_insert(&mut state.outbound, identifier.clone(), record, 10000);
        remember_note(&mut state, identifier, &text, &mode);
        Ok(())
    }

    /// Correlate an authenticated native receipt with pending deliveries.
    pub fn receipt(&self, frame: &Map<String, Value>, record: &Record) -> Result<()> {
        let mut status = frame.get("status").cloned();
        if status.as_ref().and_then(Value::as_str) == Some("expired")
            && wirejson::value_eq_opt(frame.get("status_detail"), Some(&Value::from("refused")))
        {
            status = Some(Value::from("refused"));
        }
        let status = match status.as_ref().and_then(Value::as_str) {
            Some(status) if receipt_state(status).is_some() => status.to_string(),
            _ => return Err(Error::value("unsupported receipt status")),
        };
        if truthy(frame.get("from"))
            && path_from_address(frame.get("from"))? != socket_path(record)?
        {
            return Err(Error::value("receipt source mismatch"));
        }
        // Native ignores unrecognized ancillary reasons; the authenticated
        // receipt remains valid. Never carry arbitrary metadata into the marker.
        let reason = match frame.get("drop_reason").and_then(Value::as_str) {
            Some(reason) if status == "dropped" && DROP_REASONS.contains(&reason) => {
                Some(reason.to_string())
            }
            _ => None,
        };
        let mut identifiers = Vec::new();
        match frame.get("dropped_msg_ids") {
            None | Some(Value::Null) => {}
            Some(batch) => {
                let items = match batch {
                    Value::Array(items) if status == "dropped" && items.len() <= 256 => items,
                    _ => return Err(Error::value("invalid native dropped message batch")),
                };
                for value in items {
                    identifiers.push(valid_uuid(Some(value))?);
                }
            }
        }
        match frame.get("orig_msg_id") {
            None | Some(Value::Null) => {}
            Some(value) => identifiers.push(valid_uuid(Some(value))?),
        }
        let mut unique: Vec<String> = Vec::new();
        for identifier in identifiers {
            if !unique.contains(&identifier) {
                unique.push(identifier);
            }
        }
        let mut detail = if status == "delivered" {
            "delivered".to_string()
        } else {
            format!("native receipt: {status}")
        };
        if let Some(reason) = &reason {
            detail.push_str("; drop_reason=");
            detail.push_str(reason);
        }
        let mapped = receipt_state(&status).expect("checked");
        {
            let mut state = self.state.lock().unwrap();
            let matched: Vec<String> = unique
                .into_iter()
                .filter(|identifier| {
                    state.pending.get(identifier).is_some_and(|pending| {
                        same_identity(&pending.record, record, &["pid", "sessionId", "procStart"])
                    })
                })
                .collect();
            if matched.is_empty() {
                return Err(Error::value("unmatched receipt identity"));
            }
            for identifier in matched {
                let pending = state.pending.get_mut(&identifier).expect("matched");
                pending.state = Some(mapped.to_string());
                let hub_id = pending.id.clone();
                self.status(&hub_id, mapped, Some(&detail))?;
                if status != "held" {
                    state.pending.shift_remove(&identifier);
                }
            }
        }
        if status != "held" {
            self.wake_listener();
        }
        Ok(())
    }

    fn receive(self: &Arc<Self>, conn: &Stream, wake: RawFd) -> Result<()> {
        let mut reader = LineReader {
            conn,
            wake,
            peer: self,
            buffer: Vec::new(),
            eof: false,
        };
        let line = reader.readline(FRAME_LIMIT + 1)?;
        if line.is_empty() {
            return Ok(()); // Empty native liveness probe.
        }
        let pid = identity::peer_pid(conn.raw())?;
        let auth = decode(&line)?;
        let token_ok = match auth.get("token") {
            Some(Value::String(token)) if str_of(&auth, "type") == Some("auth") => {
                compare_digest(token, &self.token)?
            }
            _ => false,
        };
        if !token_ok {
            return Err(Error::value("peer auth required"));
        }
        let frame = decode(&reader.readline(FRAME_LIMIT + 1)?)?;
        if self.explain_unjoined(pid, &frame)? {
            return Ok(());
        }
        let _auth = self.auth_lock.lock().unwrap();
        let view = self.view();
        let pid_value = Value::from(pid);
        let record = session_record(&self.config_dir, &pid_value, &view.selected)?;
        let sid = record["sessionId"].as_str().expect("validated").to_string();
        let unique = discover_one(&self.config_dir, &view.selected, &sid)?;
        if !unique.is_some_and(|unique| wirejson::value_eq_opt(unique.get("pid"), Some(&pid_value)))
        {
            return Err(Error::value("ambiguous enrolled native identity"));
        }
        auth_key(&self.config_dir, &record)?;
        let expected = socket_path(&record)?;
        if fsutil::parent(&expected) != self.socket_dir {
            return Err(Error::value("sender outside native socket namespace"));
        }
        let target_ok = match frame.get("session_id") {
            None | Some(Value::Null) => true,
            Some(value) => wirejson::value_eq(value, &Value::from(self.sid.clone())),
        };
        if !wirejson::value_eq_opt(frame.get("msgV"), Some(&Value::from(1))) || !target_ok {
            return Err(Error::value("native protocol or target mismatch"));
        }
        if str_of(&frame, "type") == Some("control")
            && str_of(&frame, "action") == Some("peer_message_status")
        {
            return self.receipt(&frame, &record);
        }
        let message = match (str_of(&frame, "type"), frame.get("message")) {
            (Some("user"), Some(Value::Object(message))) => message,
            _ => return Err(Error::value("unsupported native frame")),
        };
        let (attrs, body) = parse_envelope(message.get("content"))?;
        if path_from_address(frame.get("from"))? != expected
            || path_from_address(attrs.get("from").map(|v| Value::from(v.clone())).as_ref())?
                != expected
        {
            return Err(Error::value("native sender address mismatch"));
        }
        let native_id = valid_uuid(frame.get("msg_id"))?;
        let mode = attrs.get("from-mode").cloned().expect("validated mode");
        let mut event = obj! {
            "event" => "outbound",
            "native_id" => native_id.clone(),
            "revision" => int_value(&view.revision),
            "source_session" => sid,
            "text" => body.clone(),
            "mode" => mode.clone(),
            "recipient" => self.route.clone(),
        };
        if let Some(chain) = attrs.get("hop-chain") {
            event.insert("hop_chain".into(), Value::from(chain.clone()));
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.outbound.contains_key(&native_id) {
                return Err(Error::value("duplicate native message ID"));
            }
            bounded_insert(&mut state.outbound, native_id.clone(), record, 10000);
            remember_note(&mut state, native_id, &body, &mode);
        }
        self.emit(event)
    }

    /// Tell a genuine unjoined local chat why its message went nowhere.
    ///
    /// Returns true when the frame came from such a chat. Native receipts
    /// carry only fixed reasons, so the explanation arrives as a message from
    /// this proxy, at most once a minute per chat, in the sender's own mode.
    fn explain_unjoined(self: &Arc<Self>, pid: i64, frame: &Map<String, Value>) -> Result<bool> {
        let file = fsutil::join(&self.registry, &format!("{pid}.json"));
        let sid = match fsutil::read_json(&file, crate::discovery::RECORD_LIMIT, false)
            .and_then(|record| valid_uuid(record.get("sessionId")))
        {
            Ok(sid) => sid,
            Err(exc) if exc.is_handled() => return Ok(false),
            Err(exc) => return Err(exc),
        };
        let is_user = str_of(frame, "type") == Some("user")
            && matches!(frame.get("message"), Some(Value::Object(_)));
        if self.selected().contains(&sid) || !is_user {
            return Ok(false);
        }
        let only: Selection = [sid.clone()].into_iter().collect();
        let record = session_record(&self.config_dir, &Value::from(pid), &only)?;
        let expected = socket_path(&record)?;
        if fsutil::parent(&expected) != self.socket_dir {
            return Err(Error::value("sender outside native socket namespace"));
        }
        let content = frame.get("message").and_then(|m| m.get("content"));
        let (attrs, _) = parse_envelope(content)?;
        if path_from_address(frame.get("from"))? != expected
            || path_from_address(attrs.get("from").map(|v| Value::from(v.clone())).as_ref())?
                != expected
        {
            return Err(Error::value("native sender address mismatch"));
        }
        let now = monotonic();
        let notify = {
            let mut state = self.state.lock().unwrap();
            let last = state
                .unjoined_notices
                .get(&sid)
                .copied()
                .unwrap_or(now - 60.0);
            let notify = now - last >= 60.0;
            if notify {
                bounded_insert(&mut state.unjoined_notices, sid, now, 256);
            }
            notify
        };
        if notify {
            let peer = Arc::clone(self);
            let mode = attrs.get("from-mode").cloned().expect("validated mode");
            std::thread::spawn(move || {
                let _ = peer.send_unjoined_notice(&record, &mode);
            });
        }
        self.emit(obj! {"event" => "rejected", "detail" => "sender has not joined Switchboard; told it how to join"})?;
        Ok(true)
    }

    fn send_unjoined_notice(&self, record: &Record, mode: &str) -> Result<()> {
        let name = self.name();
        let attrs = self.proxy_attrs(&name, mode);
        let session = record
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let result = self
            .user_frame(&session, &attrs, &unjoined_text(&name))
            .and_then(|frame| {
                let only: Selection = [session.clone()].into_iter().collect();
                (self.send)(&self.config_dir, &only, record, &frame)
            });
        match result {
            Err(exc) if exc.is_os_value_subprocess() => self.emit(obj! {
                "event" => "rejected",
                "detail" => format!("unjoined notice not written: {}", exc.truncated(200)),
            }),
            other => other,
        }
    }

    fn with_channels<T>(&self, use_: impl FnOnce(&Channels) -> T) -> Option<T> {
        self.channels.read().unwrap().as_ref().map(use_)
    }

    pub fn wake_listener(&self) {
        self.with_channels(|c| wake_fd(c.wake_write.as_raw_fd()));
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || crate::signals::signalled()
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.with_channels(|c| {
            wake_fd(c.wake_write.as_raw_fd());
            wake_fd(c.main_write.as_raw_fd());
        });
    }

    /// Descriptors for the signal handler: (main wake, listener wake), duplicated
    /// so they stay valid for the life of the process.
    pub fn signal_fds(&self) -> Option<(RawFd, RawFd)> {
        self.with_channels(|c| {
            // SAFETY: dup of valid descriptors; the copies are intentionally
            // never closed so a late signal can never write to a reused number.
            unsafe {
                (
                    libc::dup(c.main_write.as_raw_fd()),
                    libc::dup(c.wake_write.as_raw_fd()),
                )
            }
        })
    }

    pub fn main_read_fd(&self) -> Option<RawFd> {
        self.with_channels(|c| c.main_read.as_raw_fd())
    }

    pub fn expiry_delay(&self) -> Option<f64> {
        let state = self.state.lock().unwrap();
        let deadline = state
            .pending
            .values()
            .map(|p| p.at + RECEIPT_WINDOW)
            .reduce(f64::min)?;
        Some((deadline - monotonic()).max(0.0))
    }

    pub fn expire(&self) -> Result<()> {
        let now = monotonic();
        let expired: Vec<Pending> = {
            let mut state = self.state.lock().unwrap();
            let keys: Vec<String> = state
                .pending
                .iter()
                .filter(|(_, value)| now - value.at >= RECEIPT_WINDOW)
                .map(|(key, _)| key.clone())
                .collect();
            keys.iter()
                .filter_map(|key| state.pending.shift_remove(key))
                .collect()
        };
        for record in expired {
            self.status(
                &record.id,
                "unknown",
                Some("native receipt correlation expired after 24 hours"),
            )?;
        }
        Ok(())
    }

    /// The native listener loop; runs until stop. Holds the serve lock so
    /// close waits for an active (bounded) receive before releasing FDs.
    pub fn serve(self: &Arc<Self>) -> Result<()> {
        let closed = self.serve_lock.lock().unwrap();
        if *closed {
            return Ok(());
        }
        let (listener, wake) = {
            let channels = self.channels.read().unwrap();
            let channels = channels.as_ref().expect("open while serving");
            (
                channels
                    .listener
                    .as_ref()
                    .map(|l| l.as_raw_fd())
                    .unwrap_or(-1),
                channels.wake_read.as_raw_fd(),
            )
        };
        while !self.stopped() {
            self.serve_passes.fetch_add(1, Ordering::SeqCst);
            let delay = self.expiry_delay();
            if delay == Some(0.0) {
                self.expire()?;
                continue;
            }
            let mut fds = [pollfd(listener, libc::POLLIN), pollfd(wake, libc::POLLIN)];
            if !poll_fds(&mut fds, delay)? {
                continue;
            }
            if fds[1].revents != 0 {
                drain(wake);
            }
            if fds[0].revents != 0 && !self.stopped() {
                // SAFETY: accept on a valid nonblocking listener.
                let fd =
                    unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
                if fd < 0 {
                    let code = Error::last_os();
                    if code == libc::EAGAIN || code == libc::EWOULDBLOCK || code == libc::EINTR {
                        continue;
                    }
                    // Any other accept error ends the listener thread.
                    return Err(Error::errno(code));
                }
                // SAFETY: fd is a fresh accepted descriptor owned here.
                let conn = Stream::from_fd(unsafe { OwnedFd::from_raw_fd(fd) }, 2.0);
                // SAFETY: fcntl on a valid descriptor.
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                // Stop may race accept; never begin a receive after it.
                if !self.stopped() {
                    if let Err(exc) = self.receive(&conn, wake) {
                        if !exc.is_handled() {
                            return Err(exc);
                        }
                        if !self.stopped() {
                            self.emit(
                                obj! {"event" => "rejected", "detail" => exc.truncated(256)},
                            )?;
                        }
                    }
                }
            }
        }
        drop(closed);
        Ok(())
    }

    /// Stop, release descriptors, and remove every file this helper still owns.
    pub fn close(&self) -> Result<()> {
        self.request_stop();
        let mut closed = self.serve_lock.lock().unwrap();
        if *closed {
            return Ok(());
        }
        *closed = true;
        *self.channels.write().unwrap() = None;
        let owned: Vec<(String, u64)> = self.owned.lock().unwrap().clone();
        for (path, inode) in owned.iter().rev() {
            let file = std::path::Path::new(path);
            let result = std::fs::symlink_metadata(file).and_then(|info| {
                if info.ino() == *inode {
                    std::fs::remove_file(file)
                } else {
                    Ok(())
                }
            });
            match result {
                Ok(()) => {}
                Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {}
                Err(exc) => return Err(Error::from_io(&exc, Some(file))),
            }
        }
        self.owned.lock().unwrap().clear();
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.channels.read().unwrap().is_none()
    }
}

fn remember_note(state: &mut State, identifier: String, text: &str, mode: &str) {
    let note = Note {
        text: take_chars(text, 240),
        mode: Some(mode.to_string()),
    };
    bounded_insert(&mut state.outbound_notes, identifier, note, 10000);
}

/// Constant-time comparison of two ASCII strings; non-ASCII input is a type
/// error.
fn compare_digest(left: &str, right: &str) -> Result<bool> {
    if !left.is_ascii() || !right.is_ascii() {
        return Err(Error::type_error(
            "comparing strings with non-ASCII characters is not supported",
        ));
    }
    let (a, b) = (left.as_bytes(), right.as_bytes());
    let mut diff = (a.len() != b.len()) as u8;
    for (index, byte) in a.iter().enumerate() {
        diff |= byte ^ b.get(index).copied().unwrap_or(0);
    }
    Ok(diff == 0)
}

/// Decode one newline-terminated native or IPC frame into an object.
pub fn decode(line: &[u8]) -> Result<Map<String, Value>> {
    if !line.ends_with(b"\n") || line.len() > FRAME_LIMIT {
        return Err(Error::value("native/IPC frame too large or unterminated"));
    }
    wirejson::object(wirejson::loads(line)?, "frame object required")
}

/// Buffered line reads from a native connection; every read times out after
/// the socket timeout, and a stop request interrupts a waiting read.
struct LineReader<'a> {
    conn: &'a Stream,
    wake: RawFd,
    peer: &'a Peer,
    buffer: Vec<u8>,
    eof: bool,
}

impl LineReader<'_> {
    fn fill(&mut self) -> Result<()> {
        let deadline = monotonic() + self.conn.timeout;
        loop {
            if self.peer.stopped() {
                return Err(Error::os("helper stopping"));
            }
            let left = deadline - monotonic();
            let mut fds = [
                pollfd(self.conn.raw(), libc::POLLIN),
                pollfd(self.wake, libc::POLLIN),
            ];
            if left <= 0.0 || !poll_fds(&mut fds, Some(left))? {
                return Err(Error::timed_out());
            }
            if fds[1].revents != 0 {
                // The serve loop recalculates its deadline after every
                // receive, so this wakeup has nothing else to do.
                drain(self.wake);
                continue;
            }
            let mut chunk = [0u8; 8192];
            // SAFETY: chunk is a valid writable buffer.
            let count = unsafe {
                libc::recv(
                    self.conn.raw(),
                    chunk.as_mut_ptr() as *mut libc::c_void,
                    chunk.len(),
                    0,
                )
            };
            if count < 0 {
                let code = Error::last_os();
                if code == libc::EINTR || code == libc::EAGAIN || code == libc::EWOULDBLOCK {
                    continue;
                }
                return Err(Error::errno(code));
            }
            if count == 0 {
                self.eof = true;
            } else {
                self.buffer.extend_from_slice(&chunk[..count as usize]);
            }
            return Ok(());
        }
    }

    /// Read up to and including the next newline, or at most `limit` bytes, or
    /// what is left at end of stream.
    fn readline(&mut self, limit: usize) -> Result<Vec<u8>> {
        loop {
            let window = self.buffer.len().min(limit);
            if let Some(index) = self.buffer[..window].iter().position(|b| *b == b'\n') {
                return Ok(self.buffer.drain(..=index).collect());
            }
            if self.buffer.len() >= limit || self.eof {
                return Ok(self.buffer.drain(..window).collect());
            }
            self.fill()?;
        }
    }
}

/// A JSON integer from its decimal digits (any size).
fn int_value(digits: &str) -> Value {
    serde_json::from_str(digits).expect("integer digits")
}

impl Peer {
    /// A peer with its correlation logic but no registry files or ledger,
    /// for tests.
    /// `send` replaces the native frame writer when given.
    #[doc(hidden)]
    pub fn unregistered(
        config_dir: &str,
        selected: Selection,
        path: &str,
        name: &str,
        emit: Emit,
        send: Option<FrameSender>,
        listener_path: Option<&str>,
    ) -> Arc<Self> {
        let (wake_read, wake_write) = socketpair().unwrap();
        let (main_read, main_write) = socketpair().unwrap();
        let listener = listener_path.map(|p| {
            let fd = bind_listener(p).unwrap();
            listen(&fd, 1).unwrap();
            fd
        });
        Arc::new(Peer {
            route: format!("remote/{}", identity::uuid4()),
            pid: std::process::id() as i64,
            sid: identity::uuid4(),
            start: String::new(),
            token: identity::token_hex16(),
            path: path.to_string(),
            config_dir: config_dir.to_string(),
            registry: fsutil::join(config_dir, "sessions"),
            registry_path: String::new(),
            state_dir: String::new(),
            socket_dir: fsutil::parent(path),
            domain: identity::local_domain().unwrap(),
            emit,
            send: send.unwrap_or_else(|| Arc::new(net::send_frame)),
            auth_lock: Mutex::new(()),
            view: Mutex::new(View {
                selected,
                revision: "0".into(),
                name: name.into(),
                registry_record: Map::new(),
            }),
            state: Mutex::new(State::default()),
            owned: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            serve_lock: Mutex::new(false),
            channels: RwLock::new(Some(Channels {
                listener,
                wake_read,
                wake_write,
                main_read,
                main_write,
            })),
            serve_passes: AtomicUsize::new(0),
        })
    }
}
