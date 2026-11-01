//! The bridge role: the per-machine daemon.
//!
//! Only UUIDs listed in the private config are exported. Helpers are genuine
//! child processes (`switchboard helper` from this binary), one per live
//! remote route. Server failures withdraw every local proxy. With a server
//! that supports acknowledgments, received events are acknowledged on the
//! next poll and redelivered if lost; message IDs already handed to a helper
//! are persisted, so a redelivery or restart never hands a message to Claude
//! twice. Sends whose outcome is uncertain are resent once the connection
//! returns (the server treats an identical resend as the same message), and
//! the sending chat is told when delivery still cannot be confirmed.
//!
//! Threads: the main loop owns all bridge state; one long-poll thread, three
//! request workers, the transport's reaper, the discovery watcher, and one
//! reader and one writer per helper only queue events for it. Helper
//! commands are written by the helper's writer thread, and closing a helper
//! waits at most a short grace, so no helper can stall the main loop.

#[cfg(test)]
mod fake_server_tests;
pub mod helper_process;
pub mod queue;
pub mod requests;
pub mod watch;

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::discovery::{discover, socket_path, Record, Selection};
use crate::error::{Error, Kind, Result};
use crate::fmtutil::{float_value, time_now, utc_stamp};
use crate::fsutil;
use crate::hub_transport::{ClientConfig, HubClient, HubError};
use crate::identity::{monotonic, process_start, uuid4_hex};
use crate::membership;
use crate::obj;
use crate::protocol::{clean_name, receipt_marker, valid_uuid, NOTICE_STATES};
use crate::session_metadata::MetadataCache;
use crate::text;
use crate::wirejson::{dumps, truthy, value_eq, value_eq_opt};

use helper_process::{Closing, Helper, Identity, Unsent};
use queue::{BoundedQueue, Flag};
use requests::{HubRequests, Tag};

pub const MAX_HELPERS: usize = 128;
pub const BRIDGE_CAPABILITIES: [&str; 2] = ["ack", "ping"];
/// Uncertain sends are resent for this long; after it the sender is told.
pub const RESEND_WINDOW: f64 = 25.0;
pub const RESEND_NOTICE_WINDOW: f64 = 600.0;
const ACTIVITY_STATES: [&str; 4] = ["idle", "busy", "waiting", "shell"];

/// The HTTP status each server rejection is answered with before the
/// requested operation is accepted (`REJECTED_HTTP`).
pub fn rejected_status(code: &str) -> Option<u16> {
    Some(match code {
        "invalid_uuid"
        | "invalid_label"
        | "invalid_body"
        | "invalid_sessions"
        | "invalid_session"
        | "invalid_peers_revision"
        | "invalid_message"
        | "invalid_route"
        | "invalid_hop_chain"
        | "invalid_status"
        | "invalid_detail"
        | "invalid_length"
        | "expected_json"
        | "incomplete_body"
        | "invalid_json" => 400,
        "source_forbidden" | "status_forbidden" | "not_enrolled" => 403,
        "invalid_proxy_identity" => 401,
        "id_conflict" => 409,
        "target_offline" | "message_not_found" | "not_found" => 404,
        "queue_full" => 503,
        "rate_limited" => 429,
        "body_too_large" => 413,
        _ => return None,
    })
}

const GLOBAL_HTTP: [&str; 3] = [
    "not_enrolled",
    "invalid_proxy_identity",
    "enrollment_unavailable",
];

fn send_reason_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "target_offline" => "the recipient chat is offline or no longer joined",
        "queue_full" => "the recipient machine's queue is full",
        "rate_limited" => "Switchboard's rate limit was reached",
        "source_forbidden" => "this chat is not joined to Switchboard",
        "id_conflict" => "a different message already used this message ID",
        _ => return None,
    })
}

/// The plain reason a sender is told for an explicitly rejected send.
pub fn send_reason(error: &HubError) -> String {
    if let Some(reason) = error.code().and_then(send_reason_text) {
        return reason.to_string();
    }
    if matches!(error, HubError::Cancelled) {
        return "the send was cancelled before it reached the server".into();
    }
    format!("the server refused the message ({})", error.truncated(128))
}

/// The server's code when it refused this machine itself (HTTP 401 or 403
/// with `not_enrolled` or `invalid_proxy_identity`), which no retry fixes.
pub fn machine_refusal(error: &HubError) -> Option<String> {
    match error {
        HubError::Http {
            status: 401 | 403,
            code: Some(code),
        } if GLOBAL_HTTP.contains(&code.as_str()) => Some(code.clone()),
        _ => None,
    }
}

/// Whether a failed request means the connection itself is gone.
pub fn global_request_error(error: &HubError) -> bool {
    match error {
        HubError::Cancelled => false,
        HubError::Http {
            status,
            code: Some(code),
        } => GLOBAL_HTTP.contains(&code.as_str()) || rejected_status(code) != Some(*status),
        HubError::Http { code: None, .. } => true,
        _ => true,
    }
}

fn explicit_rejection(error: &HubError) -> bool {
    match error {
        HubError::Cancelled => true,
        HubError::Http {
            status,
            code: Some(code),
        } => rejected_status(code) == Some(*status),
        _ => false,
    }
}

/// An error the main loop does not handle: it ends the bridge.
fn uncaught(message: impl Into<String>) -> Error {
    Error {
        kind: Kind::Output,
        message: message.into(),
    }
}

fn handled(error: &Error) -> bool {
    matches!(
        error.kind,
        Kind::Value | Kind::NotJoined | Kind::Os | Kind::WriteUnknown | Kind::Type
    )
}

/// One main-loop event.
pub enum Event {
    Requests,
    Discovery,
    Stop,
    PollError(u64, String),
    /// The poller lost the server; the server's refusal of this machine,
    /// if that was why (see `machine_refusal`).
    Offline(u64, Option<String>),
    Poll(u64, Map<String, Value>, Arc<Ticket>),
    Helper(Identity, Map<String, Value>),
    HelperExit(Identity, u32),
    /// A helper's writer failed: the command whose outcome is uncertain,
    /// then every command queued behind it, never written.
    HelperWrite(Identity, Unsent, Vec<Unsent>),
}

/// A message a local chat sent: the route its delivery outcome goes to, and
/// what a replacement helper for that route needs to adopt it (`track`), so
/// the chat still hears about it after its helper was replaced.
#[derive(Clone, Debug)]
struct Outbound {
    route: String,
    helper: Identity,
    source_session: String,
    excerpt: String,
    mode: Value,
    /// A helper already sent the chat a Switchboard notice for it.
    notified: bool,
}

/// A poll reply's acceptance: the poller waits for `done`, then sends `acks`.
#[derive(Default)]
pub struct Ticket {
    pub done: Flag,
    pub acks: Mutex<Vec<String>>,
}

/// `(generation, registration, peers revision)`: the poll cursor.
type PollState = (u64, Option<String>, Option<String>);

type CompletionItem = (Tag, Option<Map<String, Value>>, Option<HubError>);

/// Message IDs already handed to native helpers, persisted across restarts.
pub struct DeliveredIds {
    path: String,
    ids: IndexMap<String, f64>,
}

impl DeliveredIds {
    pub const LIMIT: usize = 2048;
    pub const AGE: f64 = 3600.0;

    pub fn load(state_dir: &str) -> Self {
        let path = fsutil::join(state_dir, "delivered-ids.json");
        let mut ids = IndexMap::new();
        if let Ok(value) = fsutil::read_json(&path, 1 << 20, true) {
            match value.get("ids") {
                None => {}
                Some(Value::Object(map)) => {
                    for (key, at) in map {
                        let stamp = match at {
                            Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
                            other => membership::Stamp::from_value(other).map(|s| s.as_f64()),
                        };
                        if let Some(stamp) = stamp {
                            ids.insert(key.clone(), stamp);
                        }
                    }
                }
                Some(_) => {}
            }
        }
        DeliveredIds { path, ids }
    }

    pub fn contains(&self, identifier: &str) -> bool {
        self.ids.contains_key(identifier)
    }

    pub fn add(&mut self, identifier: &str) -> Result<()> {
        let now = time_now();
        self.ids.retain(|_, at| now - *at < Self::AGE);
        self.ids.insert(identifier.to_string(), now);
        while self.ids.len() > Self::LIMIT {
            self.ids.shift_remove_index(0);
        }
        let mut map = Map::new();
        for (key, at) in &self.ids {
            map.insert(key.clone(), float_value(*at));
        }
        let text = dumps(&Value::Object(obj! {"ids" => Value::Object(map)}));
        crate::fileops::atomic_write(&self.path, ".delivered-", ".json", text.as_bytes())
    }
}

/// `log_line`: one JSON object per stdout line, `at` (UTC) first.
pub fn log_line(value: &Map<String, Value>) -> bool {
    let text = format_log_line(value);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(text.as_bytes())
        .and_then(|_| lock.flush())
        .is_ok()
}

/// One log line: `{"at": "<UTC>", ...}` and a newline.
pub fn format_log_line(value: &Map<String, Value>) -> String {
    let mut line = Map::new();
    line.insert("at".into(), Value::String(utc_stamp()));
    for (key, item) in value {
        if key != "at" {
            line.insert(key.clone(), item.clone());
        }
    }
    let mut text = dumps(&Value::Object(line));
    text.push('\n');
    text
}

pub type Emit = Arc<dyn Fn(&Map<String, Value>) -> bool + Send + Sync>;

/// The validated bridge config (`load_config`).
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    pub enabled: bool,
    pub sessions: Vec<String>,
    pub cert: String,
    pub key: String,
    pub ca: Option<String>,
    pub config_dir: String,
    pub state_dir: String,
    pub hub_url: String,
}

fn disabled_config() -> BridgeConfig {
    BridgeConfig {
        enabled: false,
        sessions: Vec::new(),
        cert: String::new(),
        key: String::new(),
        ca: None,
        config_dir: String::new(),
        state_dir: String::new(),
        hub_url: String::new(),
    }
}

/// Validate a selection list as `load_config` and `register` do.
fn session_list(value: Option<&Value>, message: &str) -> Result<Vec<String>> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) if items.len() <= 32 => items,
        Some(_) => return Err(Error::value(message)),
    };
    let mut seen = HashSet::new();
    for item in items {
        if matches!(item, Value::Array(_) | Value::Object(_)) {
            return Err(Error::type_error(format!(
                "unhashable type: '{}'",
                crate::discovery::type_name(Some(item))
            )));
        }
        seen.insert(dumps(item));
    }
    if seen.len() != items.len() {
        return Err(Error::value(message));
    }
    items.iter().map(|item| valid_uuid(Some(item))).collect()
}

pub fn load_config(path: &str) -> Result<BridgeConfig> {
    let path = fsutil::resolve(path)?;
    let config = fsutil::read_json(&path, crate::protocol::FRAME_LIMIT, true)?;
    if config.get("enabled") != Some(&Value::Bool(true)) {
        return Ok(disabled_config());
    }
    let sessions = session_list(
        config.get("sessions"),
        "sessions must be a unique list of at most 32 UUIDs",
    )?;
    let base = fsutil::parent(&path);
    let mut paths = Vec::new();
    for key in ["cert", "key", "config_dir", "state_dir"] {
        match config.get(key) {
            Some(Value::String(value)) if !value.is_empty() => {
                paths.push(fsutil::absolute(&crate::fileops::path_join(&base, value))?)
            }
            _ => return Err(Error::value(format!("missing bridge path: {key}"))),
        }
    }
    let ca = match config.get("ca") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.is_empty() => {
            Some(fsutil::absolute(&crate::fileops::path_join(&base, value))?)
        }
        Some(_) => {
            return Err(Error::value(
                "ca must be a certificate path or null for system roots",
            ))
        }
    };
    let hub_url = match crate::config::server_url(&config) {
        None => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(other) => {
            return Err(Error::type_error(format!(
                "Cannot mix str and non-str arguments ({})",
                crate::discovery::type_name(Some(other))
            )))
        }
    };
    let parts = crate::urlparse::urlsplit(&hub_url)?;
    if parts.scheme != "https"
        || parts.hostname().is_none()
        || parts.username().is_some_and(|u| !u.is_empty())
        || parts.password().is_some_and(|p| !p.is_empty())
        || !parts.query.is_empty()
        || !parts.fragment.is_empty()
        || !(parts.path.is_empty() || parts.path == "/")
    {
        return Err(Error::value("server_url must be an HTTPS origin"));
    }
    let config_dir = fsutil::owned_config_dir(&paths[2])?;
    fsutil::private_dir(&paths[3], true)?;
    let key_stat = fsutil::stat(&paths[1])?;
    if key_stat.uid() != fsutil::getuid() || key_stat.mode() & 0o077 != 0 {
        return Err(Error::value("TLS key must be owned and mode 0600"));
    }
    Ok(BridgeConfig {
        enabled: true,
        sessions,
        cert: paths[0].clone(),
        key: paths[1].clone(),
        ca,
        config_dir,
        state_dir: paths[3].clone(),
        hub_url,
    })
}

/// State shared with the poll thread, workers, watcher, and signal thread.
struct Shared {
    stop: Flag,
    events: Arc<BoundedQueue<Event>>,
    completions: BoundedQueue<CompletionItem>,
    discovery_pending: Mutex<bool>,
    poll_state: Mutex<Option<PollState>>,
    ack: AtomicBool,
    emit_failed: AtomicBool,
}

impl Shared {
    fn request_stop(&self) {
        self.stop.set();
        let _ = self.events.put_nowait(Event::Stop);
    }

    fn discovery_hint(&self) {
        let mut pending = self.discovery_pending.lock().expect("discovery");
        if *pending || self.stop.is_set() {
            return;
        }
        *pending = true;
        if self.events.put_nowait(Event::Discovery).is_err() {
            *pending = false;
        }
    }
}

/// Request metadata, kept until the request's completion.
#[derive(Clone, Default, Debug)]
struct Meta {
    id: Option<String>,
    route: Option<String>,
    source_session: Option<String>,
    helper_identity: Option<Identity>,
    body: Option<Map<String, Value>>,
    resend_deadline: Option<f64>,
    sessions: Option<Vec<Value>>,
    revision: u64,
    submitted: f64,
}

pub struct Bridge {
    config_path: String,
    config: BridgeConfig,
    emit: Emit,
    enabled: bool,
    client: Option<Arc<HubClient>>,
    program: String,
    machine: Option<String>,
    helpers: IndexMap<String, Helper>,
    shared: Arc<Shared>,
    records: IndexMap<String, Record>,
    generation: u64,
    online: bool,
    /// Why the bridge is offline, when the server refused this machine:
    /// published in the runtime file as `server_refused` for doctor.
    refused: Option<String>,
    registration: Option<String>,
    advertised: Option<Vec<Value>>,
    session_metadata_fields: Vec<String>,
    last_register: f64,
    peers: Option<Vec<Value>>,
    peers_revision: Option<String>,
    runtime_config: Option<String>,
    runtime_inode: Option<u64>,
    poller: Option<std::thread::JoinHandle<()>>,
    state_path: Option<String>,
    state_inode: Option<u64>,
    state_lock_fd: Option<OwnedFd>,
    state_start: Option<String>,
    deliveries: IndexMap<String, String>,
    outbound: IndexMap<String, Outbound>,
    closing: Vec<Closing>,
    watcher: Option<watch::DiscoveryWatcher>,
    discovery_mode: Option<String>,
    discovery_detail: Option<String>,
    next_scan: f64,
    retry_at: f64,
    pending: IndexMap<Tag, Meta>,
    registration_inflight: Option<Tag>,
    desired_sessions: Option<Vec<Value>>,
    roster_revision: u64,
    registration_retry: bool,
    helper_retry_at: Option<f64>,
    activity: Option<membership::Activity>,
    membership_key: Option<MembershipKey>,
    published_remote: Option<Value>,
    hub_capabilities: Vec<String>,
    delivered: Option<DeliveredIds>,
    status_seen: IndexMap<(String, String, String), bool>,
    resend: IndexMap<String, Meta>,
    metadata: MetadataCache,
    requests: Option<Runner>,
    discover: DiscoverFn,
    clock: Option<Arc<Mutex<f64>>>,
    wall: Option<Arc<Mutex<f64>>>,
    spawn_helper: Option<SpawnFn>,
}

/// Local discovery (`discover`), replaceable in tests.
type DiscoverFn = Box<dyn Fn(&str, &Selection) -> Result<IndexMap<String, Record>> + Send>;

/// What `update_peers` asks for when it starts a helper.
pub struct SpawnArgs {
    pub route: String,
    pub name: String,
    pub socket_dir: String,
    pub remote_status: String,
    pub generation: u64,
    pub sessions: Vec<String>,
}

/// Helper creation, replaceable in tests.
type SpawnFn = Box<dyn FnMut(&SpawnArgs) -> Result<Helper> + Send>;

/// A fake server call for tests: `(path, body) -> reply`.
pub type FakePost =
    Arc<dyn Fn(&str, &Value) -> std::result::Result<Map<String, Value>, HubError> + Send + Sync>;

/// The request runner: the three real workers, or (in tests) an immediate
/// fake that completes each request as it is submitted.
pub enum Runner {
    Workers(HubRequests),
    Immediate(FakePost),
}

type MembershipKey = (Vec<String>, Vec<(String, String)>, Vec<(String, String)>);

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    }
}

/// The lookup key for an event value: its JSON text. Arrays and objects can't
/// be keys and fail with `unhashable type`.
fn hashable(value: Option<&Value>) -> Result<String> {
    match value {
        Some(item @ (Value::Array(_) | Value::Object(_))) => Err(Error::type_error(format!(
            "unhashable type: '{}'",
            crate::discovery::type_name(Some(item))
        ))),
        Some(item) => Ok(dumps(item)),
        None => Ok("null".into()),
    }
}

/// The key to look up in a string-keyed map: only a string can match, and an
/// array or object fails as in `hashable`.
fn str_key(value: Option<&Value>) -> Result<Option<String>> {
    hashable(value)?;
    Ok(value.and_then(Value::as_str).map(str::to_string))
}

/// The event value split at its first `/` (absent reads as empty text); any
/// other non-string value fails.
fn split_target(value: Option<&Value>) -> Result<Vec<String>> {
    match value {
        None => Ok(vec![String::new()]),
        Some(Value::String(text)) => Ok(match text.split_once('/') {
            Some((a, b)) => vec![a.to_string(), b.to_string()],
            None => vec![text.clone()],
        }),
        Some(other) => Err(uncaught(format!(
            "AttributeError: '{}' object has no attribute 'split'",
            crate::discovery::type_name(Some(other))
        ))),
    }
}

impl Bridge {
    pub fn new(config_path: &str, emit: Emit) -> Result<Self> {
        let config_path = fsutil::resolve(config_path)?;
        let config = load_config(&config_path)?;
        let enabled = config.enabled;
        let events = Arc::new(BoundedQueue::new(512));
        let shared = Arc::new(Shared {
            stop: Flag::new(),
            events,
            completions: BoundedQueue::new(68),
            discovery_pending: Mutex::new(false),
            poll_state: Mutex::new(None),
            ack: AtomicBool::new(false),
            emit_failed: AtomicBool::new(false),
        });
        let client = if enabled {
            let client_config = ClientConfig {
                hub_url: config.hub_url.clone(),
                ca: config.ca.clone(),
                cert: config.cert.clone(),
                key: config.key.clone(),
            };
            Some(Arc::new(
                HubClient::new(&client_config).map_err(hub_to_error)?,
            ))
        } else {
            None
        };
        let requests = match &client {
            Some(client) => {
                let done = shared.clone();
                Some(Runner::Workers(HubRequests::new(
                    {
                        let client = client.clone();
                        Arc::new(move |path: &str, body: &Value| client.post(path, body))
                    },
                    Box::new(move |tag, result, error| {
                        // Exactly one completion per accepted submission; <=68 outstanding.
                        let _ = done.completions.put_nowait((tag, result, error));
                        let _ = done.events.put_nowait(Event::Requests);
                    }),
                )?))
            }
            None => None,
        };
        Self::assemble(config_path, config, emit, shared, client, requests)
    }

    /// A bridge whose server requests complete immediately through `post`
    /// (tests only; production always uses the three workers).
    pub fn with_fake_server(config_path: &str, emit: Emit, post: FakePost) -> Result<Self> {
        let config_path = fsutil::resolve(config_path)?;
        let config = load_config(&config_path)?;
        let shared = Arc::new(Shared {
            stop: Flag::new(),
            events: Arc::new(BoundedQueue::new(512)),
            completions: BoundedQueue::new(68),
            discovery_pending: Mutex::new(false),
            poll_state: Mutex::new(None),
            ack: AtomicBool::new(false),
            emit_failed: AtomicBool::new(false),
        });
        let requests = config.enabled.then(|| Runner::Immediate(post));
        Self::assemble(config_path, config, emit, shared, None, requests)
    }

    fn assemble(
        config_path: String,
        config: BridgeConfig,
        emit: Emit,
        shared: Arc<Shared>,
        client: Option<Arc<HubClient>>,
        requests: Option<Runner>,
    ) -> Result<Self> {
        let enabled = config.enabled;
        let delivered = enabled.then(|| DeliveredIds::load(&config.state_dir));
        let program = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|exc| Error::from_io(&exc, None))?;
        Ok(Bridge {
            config_path,
            config,
            emit,
            enabled,
            client,
            program,
            machine: None,
            helpers: IndexMap::new(),
            shared,
            records: IndexMap::new(),
            generation: 0,
            online: false,
            refused: None,
            registration: None,
            advertised: None,
            session_metadata_fields: Vec::new(),
            last_register: 0.0,
            peers: None,
            peers_revision: None,
            runtime_config: None,
            runtime_inode: None,
            poller: None,
            state_path: None,
            state_inode: None,
            state_lock_fd: None,
            state_start: None,
            deliveries: IndexMap::new(),
            outbound: IndexMap::new(),
            closing: Vec::new(),
            watcher: None,
            discovery_mode: None,
            discovery_detail: None,
            next_scan: 0.0,
            retry_at: 0.0,
            pending: IndexMap::new(),
            registration_inflight: None,
            desired_sessions: None,
            roster_revision: 0,
            registration_retry: false,
            helper_retry_at: None,
            activity: None,
            membership_key: None,
            published_remote: None,
            hub_capabilities: Vec::new(),
            delivered,
            status_seen: IndexMap::new(),
            resend: IndexMap::new(),
            metadata: MetadataCache::new(),
            requests,
            discover: Box::new(|dir: &str, selection: &Selection| discover(dir, selection)),
            clock: None,
            wall: None,
            spawn_helper: None,
        })
    }

    /// Wall-clock seconds since the epoch, or the test wall clock when one is set.
    fn wall_now(&self) -> f64 {
        match &self.wall {
            Some(clock) => *clock.lock().expect("clock"),
            None => time_now(),
        }
    }

    /// Monotonic seconds, or the test clock when one is set.
    fn now(&self) -> f64 {
        match &self.clock {
            Some(clock) => *clock.lock().expect("clock"),
            None => monotonic(),
        }
    }

    fn say(&self, value: Map<String, Value>) {
        if !(self.emit)(&value) {
            // The log line could not be written: the bridge stops with an
            // error.
            self.shared.emit_failed.store(true, Ordering::SeqCst);
            self.shared.request_stop();
        }
    }

    /// Ask the bridge to stop (signal-safe callers use `stop_handle`).
    pub fn request_stop(&self) {
        self.shared.request_stop();
    }

    /// A handle another thread can use to stop the bridge.
    pub fn stop_handle(&self) -> impl Fn() + Send + Sync + 'static {
        let shared = self.shared.clone();
        move || shared.request_stop()
    }

    fn submit_request(
        &mut self,
        kind: &'static str,
        path: &str,
        body: Map<String, Value>,
        metadata: Meta,
    ) -> Result<bool> {
        if self.pending.len() >= 68 {
            return Ok(false);
        }
        let tag: Tag = (self.generation, uuid4_hex(), kind);
        self.pending.insert(tag.clone(), metadata);
        if kind == "register" {
            self.registration_inflight = Some(tag.clone());
        }
        let key = body.get("id").and_then(Value::as_str).map(str::to_string);
        let body = Value::Object(body);
        let accepted = match self.requests.as_ref() {
            Some(Runner::Workers(requests)) => {
                requests.submit(kind, path, &body, tag.clone(), key.as_deref())
            }
            Some(Runner::Immediate(post)) => {
                let item = match post(path, &body) {
                    Ok(result) => (tag.clone(), Some(result), None),
                    Err(error) => (tag.clone(), None, Some(error)),
                };
                let _ = self.shared.completions.put_nowait(item);
                Ok(true)
            }
            None => Ok(false),
        };
        match accepted {
            Err(error) => {
                self.pending.shift_remove(&tag);
                if self.registration_inflight.as_ref() == Some(&tag) {
                    self.registration_inflight = None;
                }
                Err(error)
            }
            Ok(false) => {
                self.pending.shift_remove(&tag);
                if kind == "register" {
                    self.registration_inflight = None;
                }
                Ok(false)
            }
            Ok(true) => Ok(true),
        }
    }

    fn close_helper(&mut self, route: &str) {
        if let Some(helper) = self.helpers.shift_remove(route) {
            self.closing.extend(helper.close());
        }
    }

    /// Advance helpers still closing in the background; when to come back.
    fn reap_closing(&mut self) -> Option<f64> {
        let now = monotonic();
        self.closing.retain_mut(|closing| !closing.reap(now));
        self.closing
            .iter()
            .map(Closing::next_deadline)
            .min_by(f64::total_cmp)
    }

    /// Pass a delivery outcome for a message a local chat sent to the helper
    /// for its route. A replacement helper first adopts the message
    /// (`track`), so the outcome still reaches the chat; one whose earlier
    /// helper already sent the chat a notice is not told again.
    fn relay_outcome(&mut self, identifier: &str, state: &Value, detail: &Value) -> Result<()> {
        let Some(entry) = self.outbound.get(identifier).cloned() else {
            return Ok(());
        };
        let Some(helper) = self.helpers.get_mut(&entry.route) else {
            return Ok(());
        };
        if helper.identity != entry.helper {
            if entry.notified {
                return Ok(());
            }
            let track = obj! {"op" => "track", "id" => identifier,
            "source_session" => entry.source_session.clone(), "text" => entry.excerpt.clone(),
            "mode" => entry.mode.clone()};
            if let Err(error) = helper.deliver(&track) {
                if !error.is_os() {
                    return Err(error);
                }
                self.close_helper(&entry.route);
                return Ok(());
            }
            let identity = helper.identity.clone();
            if let Some(kept) = self.outbound.get_mut(identifier) {
                kept.helper = identity;
            }
        }
        let command = obj! {"op" => "receipt", "id" => identifier,
        "state" => state.clone(), "detail" => detail.clone()};
        let helper = self.helpers.get_mut(&entry.route).expect("helper");
        if let Err(error) = helper.deliver(&command) {
            if !error.is_os() {
                return Err(error);
            }
            self.close_helper(&entry.route);
            return Ok(());
        }
        let notice = state
            .as_str()
            .is_some_and(|state| NOTICE_STATES.contains(&state))
            && receipt_marker(Some(detail)).is_none();
        if notice {
            if let Some(kept) = self.outbound.get_mut(identifier) {
                kept.notified = true;
            }
        }
        Ok(())
    }

    fn outbound_failure(
        &mut self,
        identifier: &str,
        route: &str,
        state: &str,
        detail: &str,
        identity: Option<&Identity>,
    ) {
        let detail = text::take_chars(detail, 256);
        self.say(obj! {"event" => "status", "id" => identifier, "state" => state, "detail" => detail.clone()});
        if self.outbound.contains_key(identifier) {
            // Only errors that are not OS errors end the bridge, and a helper
            // write never returns one.
            let _ = self.relay_outcome(identifier, &Value::from(state), &Value::from(detail));
            return;
        }
        let matches = self
            .helpers
            .get(route)
            .is_some_and(|helper| identity.is_none_or(|identity| helper.identity == *identity));
        if matches {
            let command =
                obj! {"op" => "receipt", "id" => identifier, "state" => state, "detail" => detail};
            let failed = self
                .helpers
                .get_mut(route)
                .is_some_and(|helper| helper.deliver(&command).is_err_and(|e| e.is_os()));
            if failed {
                self.close_helper(route);
            }
        }
    }

    fn drain_completions(&mut self) -> Result<()> {
        while let Some((tag, result, error)) = self.shared.completions.get_nowait() {
            let Some(metadata) = self.pending.shift_remove(&tag) else {
                continue;
            };
            if self.registration_inflight.as_ref() == Some(&tag) {
                self.registration_inflight = None;
            }
            let stopping = self.shared.stop.is_set();
            if tag.0 != self.generation || stopping {
                if tag.2 == "send" && !stopping && !self.queue_resend(&metadata) {
                    let cancelled = matches!(error, Some(HubError::Cancelled));
                    let (state, detail) = if cancelled {
                        (
                            "failed",
                            "the send was cancelled before it reached the server",
                        )
                    } else {
                        (
                            "unknown",
                            "Switchboard lost its server connection during the send",
                        )
                    };
                    self.outbound_failure(
                        metadata.id.as_deref().unwrap_or(""),
                        metadata.route.as_deref().unwrap_or(""),
                        state,
                        detail,
                        metadata.helper_identity.as_ref(),
                    );
                }
                if tag.2 == "register" && !stopping {
                    self.next_scan = 0.0;
                }
                continue;
            }
            let kind = tag.2;
            if let Some(error) = error {
                let detail = error.truncated(256);
                if kind == "send" {
                    if explicit_rejection(&error) {
                        self.outbound_failure(
                            metadata.id.as_deref().unwrap_or(""),
                            metadata.route.as_deref().unwrap_or(""),
                            "failed",
                            &send_reason(&error),
                            metadata.helper_identity.as_ref(),
                        );
                    } else if !self.queue_resend(&metadata) {
                        self.outbound_failure(
                            metadata.id.as_deref().unwrap_or(""),
                            metadata.route.as_deref().unwrap_or(""),
                            "unknown",
                            "Switchboard could not tell whether the server accepted the send",
                            metadata.helper_identity.as_ref(),
                        );
                    }
                } else {
                    let mut line =
                        obj! {"event" => "request_failed", "operation" => kind, "detail" => detail};
                    if let Some(id) = &metadata.id {
                        line.insert("id".into(), Value::from(id.clone()));
                    }
                    self.say(line);
                }
                if global_request_error(&error) {
                    self.refused = machine_refusal(&error);
                    self.withdraw()?;
                    self.retry_at = self.now() + 2.0;
                    self.next_scan = self.retry_at;
                } else if kind == "register" {
                    self.retry_at = self.now() + 2.0;
                    self.next_scan = self.retry_at;
                }
                continue;
            }
            let result = result.unwrap_or_default();
            if kind == "register" {
                self.accept_registration(&result, &metadata)?;
            } else if kind == "send"
                && metadata
                    .source_session
                    .as_ref()
                    .is_some_and(|sid| !self.records.contains_key(sid))
            {
                self.outbound_failure(
                    metadata.id.as_deref().unwrap_or(""),
                    metadata.route.as_deref().unwrap_or(""),
                    "unknown",
                    "session selection changed during send; no replay",
                    metadata.helper_identity.as_ref(),
                );
            } else if kind == "send" {
                if let Some(state @ ("unknown" | "failed")) =
                    result.get("state").and_then(Value::as_str)
                {
                    let detail = if state == "unknown" {
                        "the server could not confirm delivery"
                    } else {
                        "the server recorded the delivery as failed"
                    };
                    let state = state.to_string();
                    self.outbound_failure(
                        metadata.id.as_deref().unwrap_or(""),
                        metadata.route.as_deref().unwrap_or(""),
                        &state,
                        detail,
                        metadata.helper_identity.as_ref(),
                    );
                }
            }
        }
        Ok(())
    }

    /// Keep an uncertain send for an identical resend after reconnecting.
    fn queue_resend(&mut self, metadata: &Meta) -> bool {
        let selected = metadata
            .source_session
            .as_ref()
            .is_some_and(|sid| self.config.sessions.contains(sid));
        if metadata.body.is_none() || !selected || self.resend.len() >= 256 {
            return false;
        }
        let deadline = metadata
            .resend_deadline
            .filter(|d| *d != 0.0)
            .unwrap_or_else(|| self.now() + RESEND_WINDOW);
        let mut kept = metadata.clone();
        kept.resend_deadline = Some(deadline);
        let id = metadata.id.clone().unwrap_or_default();
        self.resend.insert(id, kept);
        true
    }

    /// Resend or report uncertain sends once the route's helper exists again.
    fn flush_resends(&mut self) -> Result<()> {
        let now = self.now();
        let entries: Vec<(String, Meta)> = self
            .resend
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (identifier, metadata) in entries {
            let route = metadata.route.clone().unwrap_or_default();
            let source = metadata.source_session.clone().unwrap_or_default();
            let deadline = metadata.resend_deadline.unwrap_or(0.0);
            if !self.config.sessions.contains(&source) || now > deadline + RESEND_NOTICE_WINDOW {
                self.resend.shift_remove(&identifier);
                self.say(
                    obj! {"event" => "status", "id" => identifier.clone(), "state" => "unknown",
                    "detail" => "send outcome unknown; sender could not be told"},
                );
                continue;
            }
            if !self.helpers.contains_key(&route)
                || !self.online
                || !self.records.contains_key(&source)
            {
                continue;
            }
            let body = metadata.body.clone().unwrap_or_default();
            let command = obj! {"op" => "track", "id" => identifier.clone(), "source_session" => source.clone(),
            "text" => body.get("text").cloned().unwrap_or(Value::Null),
            "mode" => body.get("mode").cloned().unwrap_or(Value::Null)};
            let helper = self.helpers.get_mut(&route).expect("helper");
            if let Err(error) = helper.deliver(&command) {
                if handled(&error) {
                    self.close_helper(&route);
                    self.helper_retry_at = Some(self.now() + 2.0);
                    continue;
                }
                return Err(error);
            }
            let identity = helper.identity.clone();
            self.resend.shift_remove(&identifier);
            if let Some(kept) = self.outbound.get_mut(&identifier) {
                kept.helper = identity.clone();
            }
            if now > deadline {
                self.outbound_failure(
                    &identifier,
                    &route,
                    "unknown",
                    "Switchboard lost its server connection during the send and could not resend it in time",
                    Some(&identity),
                );
            } else {
                let mut meta = metadata.clone();
                meta.helper_identity = Some(identity);
                if !self.submit_request("send", "/v1/send", body, meta)? {
                    self.resend.insert(identifier, metadata);
                }
            }
        }
        Ok(())
    }

    fn close_watcher(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            watcher.close();
        }
    }

    fn reconcile(&mut self) -> Result<()> {
        if self.helper_retry_at.is_some_and(|at| self.now() >= at) {
            self.helper_retry_at = None;
        }
        if !self.resend.is_empty() {
            self.flush_resends()?;
        }
        let mut failure: Option<String> = None;
        if self.watcher.as_ref().is_some_and(|w| !w.healthy()) {
            self.close_watcher();
        }
        if self.watcher.is_none() {
            let hint = self.shared.clone();
            match watch::DiscoveryWatcher::new(
                &self.config_path,
                &self.config.config_dir,
                Arc::new(move || hint.discovery_hint()),
            ) {
                Ok(watcher) => self.watcher = Some(watcher),
                Err(error) => failure = Some(error.message),
            }
        }
        self.register()?;
        let healthy = self.watcher.as_ref().is_some_and(|w| w.healthy());
        let mode = if healthy { "events" } else { "polling" };
        let detail = if healthy {
            None
        } else {
            let reason = match &self.watcher {
                Some(watcher) => watcher.failure_reason(),
                None => failure,
            };
            let reason = reason
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "OS discovery notifications unavailable".into());
            let printable: String = reason
                .chars()
                .map(|c| if text::is_printable(c) { c } else { ' ' })
                .collect();
            Some(text::take_chars(
                &text::collapse_whitespace(&printable),
                256,
            ))
        };
        if (Some(mode.to_string()), detail.clone())
            != (self.discovery_mode.clone(), self.discovery_detail.clone())
        {
            self.discovery_mode = Some(mode.into());
            self.discovery_detail = detail;
            self.publish_state()?;
        }
        let mut interval: f64 = if healthy { 30.0 } else { 5.0 };
        if self.registration.is_none() {
            interval = interval.min(20.0);
        }
        if self.registration_retry
            || (self.registration_inflight.is_none()
                && !opt_list_eq(&self.desired_sessions, &self.advertised))
        {
            interval = interval.min(2.0);
        }
        self.next_scan = self.now() + interval;
        if let Some(at) = self.helper_retry_at {
            self.next_scan = self.next_scan.min(at);
        }
        self.retry_at = 0.0;
        Ok(())
    }

    fn event_loop(&mut self) -> Result<()> {
        while !self.shared.stop.is_set() {
            let step = (|| -> Result<()> {
                self.drain_completions()?;
                let due = self.next_scan.max(self.retry_at);
                if self.now() >= due {
                    self.reconcile()?;
                    self.drain_completions()?;
                }
                if self.shared.stop.is_set() {
                    return Ok(());
                }
                let mut timeout = (self.next_scan.max(self.retry_at) - self.now()).max(0.0);
                if let Some(at) = self.reap_closing() {
                    timeout = timeout.min((at - monotonic()).max(0.0));
                }
                if let Some(event) = self
                    .shared
                    .events
                    .get_timeout(Duration::from_secs_f64(timeout))
                {
                    self.handle(event)?;
                }
                Ok(())
            })();
            match step {
                Ok(()) => {}
                Err(error) if handled(&error) => {
                    self.withdraw()?;
                    self.say(obj! {"event" => "error", "detail" => error.truncated(256)});
                    self.retry_at = self.now() + 2.0;
                    self.next_scan = self.retry_at;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Acquire a private singleton lock and publish credential-free status.
    fn start_observability(&mut self) -> Result<()> {
        let root = fsutil::private_dir(&self.config.state_dir, false)?;
        let fd = crate::fileops::open_lock_file(&fsutil::join(&root, "runtime.lock"))?;
        let info = std::fs::File::from(fd.try_clone().map_err(|e| Error::from_io(&e, None))?)
            .metadata()
            .map_err(|e| Error::from_io(&e, None))?;
        if !fsutil::is_type(&info, fsutil::S_IFREG)
            || info.uid() != fsutil::getuid()
            || info.mode() & 0o077 != 0
        {
            return Err(Error::value("unsafe runtime lock file"));
        }
        // SAFETY: flock on an owned descriptor.
        if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::errno(Error::last_os()));
        }
        self.state_lock_fd = Some(fd);
        let path = fsutil::join(&root, "runtime.json");
        if fsutil::exists(&path) || fsutil::is_symlink(&path) {
            fsutil::read_json(&path, 4096, true)?;
        }
        self.state_start = Some(process_start(std::process::id() as i64)?);
        self.state_path = Some(path);
        self.publish_state()
    }

    fn publish_state(&mut self) -> Result<()> {
        let (Some(path), true) = (self.state_path.clone(), self.state_lock_fd.is_some()) else {
            return Ok(());
        };
        let published: Vec<Value> = if self.online {
            self.advertised
                .iter()
                .flatten()
                .map(|s| {
                    Value::Object(obj! {"id" => s.get("id").cloned().unwrap_or(Value::Null),
                    "name" => s.get("name").cloned().unwrap_or(Value::Null)})
                })
                .collect()
        } else {
            Vec::new()
        };
        let remote = self.remote_summary()?;
        let mut value = obj! {
            "pid" => std::process::id(),
            "procStart" => self.state_start.clone().map(Value::from).unwrap_or(Value::Null),
            "platform" => platform(),
            "machine" => self.machine.clone().map(Value::from).unwrap_or(Value::Null),
            "connected" => self.online,
            "discovery_mode" => self.discovery_mode.clone().map(Value::from).unwrap_or(Value::Null),
            "discovery_detail" => self.discovery_detail.clone().map(Value::from).unwrap_or(Value::Null),
            "published" => Value::Array(published),
            "remote" => remote.clone(),
            "updated_at" => float_value(time_now()),
        };
        if let (false, Some(code)) = (self.online, &self.refused) {
            value.insert("server_refused".into(), Value::from(code.clone()));
        }
        self.published_remote = Some(remote);
        let mut text = dumps(&Value::Object(value));
        text.push('\n');
        crate::fileops::atomic_write(&path, ".runtime-", ".json", text.as_bytes())?;
        self.state_inode = Some(fsutil::lstat(&path)?.ino());
        Ok(())
    }

    /// Withdraw status before potentially blocking shutdown; keep the lock.
    fn remove_observability(&mut self) {
        if let (Some(path), Some(inode)) = (&self.state_path, self.state_inode) {
            if fsutil::lstat(path).is_ok_and(|info| info.ino() == inode) {
                let _ = fsutil::unlink(path);
            }
        }
        self.state_path = None;
        self.state_inode = None;
    }

    fn close_observability(&mut self) {
        self.remove_observability();
        if let Some(fd) = self.state_lock_fd.take() {
            // SAFETY: unlock an owned descriptor before it closes.
            unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    /// Write the helpers' private runtime config (no TLS paths).
    fn runtime(&mut self) -> Result<()> {
        let value = obj! {
            "enabled" => true,
            "config_dir" => self.config.config_dir.clone(),
            "state_dir" => self.config.state_dir.clone(),
            "sessions" => Value::Array(self.config.sessions.iter().cloned().map(Value::from).collect()),
        };
        let path = fsutil::join(
            &self.config.state_dir,
            &format!("bridge-{}.json", std::process::id()),
        );
        if self.runtime_config.is_some() {
            self.remove_runtime();
        }
        self.runtime_inode = Some(fsutil::write_new(&path, &value)?);
        self.runtime_config = Some(path);
        Ok(())
    }

    fn remove_runtime(&mut self) {
        if let Some(path) = self.runtime_config.take() {
            if fsutil::lstat(&path).is_ok_and(|info| Some(info.ino()) == self.runtime_inode) {
                let _ = fsutil::unlink(&path);
            }
        }
    }

    fn withdraw(&mut self) -> Result<()> {
        let previous = self.generation;
        if let Some(Runner::Workers(requests)) = &self.requests {
            requests.cancel(|tag| tag.0 == previous);
        }
        self.generation += 1;
        self.online = false;
        self.registration = None;
        self.advertised = None;
        self.session_metadata_fields.clear();
        self.hub_capabilities.clear();
        self.shared.ack.store(false, Ordering::SeqCst);
        self.peers = None;
        self.peers_revision = None;
        *self.shared.poll_state.lock().expect("poll state") = None;
        let published = self.publish_state();
        let helpers: Vec<Helper> = self.helpers.drain(..).map(|(_, helper)| helper).collect();
        self.closing.extend(helper_process::close_many(helpers));
        published
    }

    fn register(&mut self) -> Result<()> {
        let fresh = fsutil::read_json(&self.config_path, crate::protocol::FRAME_LIMIT, true)?;
        if fresh.get("enabled") != Some(&Value::Bool(true)) {
            self.shared.stop.set();
            return self.withdraw();
        }
        let selected = match fresh.get("sessions") {
            None => Vec::new(),
            Some(Value::Array(items)) if items.len() <= 32 => {
                let mut list = Vec::new();
                for item in items {
                    list.push(valid_uuid(Some(item))?);
                }
                if list.iter().collect::<HashSet<_>>().len() != list.len() {
                    return Err(Error::value("duplicate enrollment"));
                }
                list
            }
            Some(_) => return Err(Error::value("invalid enrollment")),
        };
        let chosen = membership::names(&fresh)?;
        let selection: Selection = selected.iter().cloned().collect();
        let records = (self.discover)(&self.config.config_dir, &selection)?;
        let selected = self.expire_inactive(&fresh, selected, &records)?;
        let now_set: HashSet<&String> = selected.iter().collect();
        let old_set: HashSet<&String> = self.config.sessions.iter().collect();
        if now_set != old_set {
            let removed: HashSet<String> = self
                .config
                .sessions
                .iter()
                .filter(|sid| !selected.contains(sid))
                .cloned()
                .collect();
            let cancel: HashSet<Tag> = self
                .pending
                .iter()
                .filter(|(_, meta)| {
                    meta.source_session
                        .as_ref()
                        .is_some_and(|sid| removed.contains(sid))
                })
                .map(|(tag, _)| tag.clone())
                .collect();
            if let Some(Runner::Workers(requests)) = &self.requests {
                requests.cancel(|tag| cancel.contains(tag));
            }
            self.config.sessions = selected.clone();
            self.runtime()?;
        }
        self.records = records
            .into_iter()
            .filter(|(sid, _)| selected.contains(sid))
            .collect();
        self.sync_membership(&selected, &chosen);
        if let Some(watcher) = &self.watcher {
            let pairs = self
                .records
                .values()
                .map(|r| {
                    (
                        r.get("pid").cloned().unwrap_or(Value::Null),
                        r.get("procStart").cloned().unwrap_or(Value::Null),
                    )
                })
                .collect();
            watcher.update(pairs);
        }
        let mut ordered: Vec<(&String, &Record)> = self.records.iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(b.0));
        let mut sessions: Vec<Value> = Vec::new();
        for (sid, record) in ordered {
            let raw = match chosen.get(sid).filter(|n| !n.is_empty()) {
                Some(name) => Value::String(name.clone()),
                None => record
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| Value::from("Claude")),
            };
            let Value::String(raw) = raw else {
                return Err(Error::value("invalid name"));
            };
            let mut name = clean_name(&raw);
            if name.is_empty() {
                name = "Claude".into();
            }
            let status = match record.get("status").and_then(Value::as_str) {
                Some(status) if ACTIVITY_STATES.contains(&status) => status,
                _ => "unknown",
            };
            let mut session = obj! {"id" => sid.clone(), "name" => name, "status" => status};
            if !self.session_metadata_fields.is_empty() {
                for (key, value) in self.metadata.for_session(record) {
                    if self.session_metadata_fields.contains(&key) {
                        session.insert(key, Value::String(value));
                    }
                }
            }
            sessions.push(Value::Object(session));
        }
        if !opt_list_eq(&Some(sessions.clone()), &self.desired_sessions) {
            self.roster_revision += 1;
            self.desired_sessions = Some(sessions.clone());
        }
        if self.peers.is_some() || self.records.is_empty() {
            let peers = self.peers.clone().unwrap_or_default();
            self.update_peers(&Value::Array(peers))?;
        }
        let now = self.now();
        if self.registration_inflight.is_some() {
            return Ok(());
        }
        if self.online
            && opt_list_eq(&Some(sessions.clone()), &self.advertised)
            && (self.registration.is_some() || now - self.last_register < 20.0)
        {
            return Ok(());
        }
        let body = obj! {
            "sessions" => Value::Array(sessions.clone()),
            "capabilities" => Value::Array(BRIDGE_CAPABILITIES.iter().map(|c| Value::from(*c)).collect()),
        };
        let meta = Meta {
            sessions: Some(sessions),
            revision: self.roster_revision,
            submitted: now,
            ..Meta::default()
        };
        self.registration_retry = !self.submit_request("register", "/v1/register", body, meta)?;
        Ok(())
    }

    /// Keep membership.json (the plugin's footer label source) current,
    /// writing only on change.
    fn sync_membership(&mut self, selected: &[String], chosen: &IndexMap<String, String>) {
        let mut live = IndexMap::new();
        let mut invalid = false;
        for (sid, record) in &self.records {
            match record.get("name") {
                Some(Value::String(name)) => {
                    live.insert(sid.clone(), name.clone());
                }
                None => {
                    live.insert(sid.clone(), String::new());
                }
                Some(other) => {
                    let unchosen = chosen.get(sid).is_none_or(|n| n.is_empty());
                    if unchosen && truthy(Some(other)) && selected.contains(sid) {
                        invalid = true;
                    }
                    live.insert(sid.clone(), String::new());
                }
            }
        }
        let mut sorted_selected = selected.to_vec();
        sorted_selected.sort();
        let mut chosen_items: Vec<(String, String)> =
            chosen.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        chosen_items.sort();
        let mut live_items: Vec<(String, String)> =
            live.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        live_items.sort();
        let key = (sorted_selected, chosen_items, live_items);
        if self.membership_key.as_ref() == Some(&key) {
            return;
        }
        let result = if invalid {
            Err(Error::value("invalid name"))
        } else {
            membership::sync_membership(&self.config_path, &live)
        };
        match result {
            Ok(_) => self.membership_key = Some(key),
            Err(error) if error.is_os() || matches!(error.kind, Kind::Value | Kind::NotJoined) => {
                self.say(obj! {"event" => "error",
                "detail" => format!("membership file not written: {}", error.truncated(200))});
            }
            Err(_) => {}
        }
    }

    fn load_activity(&mut self) -> &mut membership::Activity {
        if self.activity.is_none() {
            self.activity = Some(membership::load_activity(&self.config.state_dir));
        }
        self.activity.as_mut().expect("activity")
    }

    fn save_activity(&mut self) {
        let activity = self.load_activity().clone();
        if let Err(error) = membership::save_activity(&self.config.state_dir, &activity) {
            if error.is_os() {
                self.say(obj! {"event" => "error",
                "detail" => format!("activity not saved: {}", error.truncated(200))});
            }
        }
    }

    /// Record that a joined chat worked or messaged; coarse to limit writes.
    fn note_activity(&mut self, sid: &str, now: Option<f64>, save: bool) -> bool {
        let now = now.unwrap_or_else(|| self.wall_now());
        let activity = self.load_activity();
        if now - activity.get(sid).copied().unwrap_or(0.0) < 300.0 {
            return false;
        }
        activity.insert(sid.to_string(), now);
        if save {
            self.save_activity();
        }
        true
    }

    /// Record working chats, then drop joined chats inactive for seven days.
    fn expire_inactive(
        &mut self,
        fresh: &Map<String, Value>,
        selected: Vec<String>,
        records: &IndexMap<String, Record>,
    ) -> Result<Vec<String>> {
        let now = self.wall_now();
        self.load_activity();
        let joined = membership::joined_at(fresh)?;
        let mut changed = false;
        for sid in &selected {
            let working = records
                .get(sid)
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| s == "busy" || s == "waiting");
            let unknown = !self.load_activity().contains_key(sid) && !joined.contains_key(sid);
            if working || unknown {
                changed = self.note_activity(sid, Some(now), false) || changed;
            }
        }
        let stale: Vec<String> = self
            .load_activity()
            .keys()
            .filter(|sid| !selected.contains(sid))
            .cloned()
            .collect();
        for sid in stale {
            self.load_activity().shift_remove(&sid);
            changed = true;
        }
        let mut selected = selected;
        let activity = self.load_activity().clone();
        if !membership::expired(fresh, &activity, now)?.is_empty() {
            let keep = selected.clone();
            let mut live = IndexMap::new();
            let gone = membership::update_config(&self.config_path, &mut live, |config, _| {
                // Recheck under the lock: a join since our read restarts the clock.
                let gone: Vec<String> = membership::expired(config, &activity, now)?
                    .into_iter()
                    .filter(|sid| keep.contains(sid))
                    .collect();
                membership::drop_sessions(config, &gone);
                Ok(gone)
            })?;
            for sid in &gone {
                self.load_activity().shift_remove(sid);
                self.say(
                    obj! {"event" => "session_expired", "session" => sid.clone(),
                    "detail" => "no activity for 7 days"},
                );
            }
            selected.retain(|sid| !gone.contains(sid));
            changed = changed || !gone.is_empty();
        }
        if changed {
            self.save_activity();
        }
        Ok(selected)
    }

    fn machine_value(&self) -> Value {
        self.machine.clone().map(Value::from).unwrap_or(Value::Null)
    }

    fn remote_summary(&self) -> Result<Value> {
        let machine = self.machine_value();
        let mut sessions = 0;
        let mut machines = HashSet::new();
        for peer in self.peers.iter().flatten() {
            let Value::Object(peer) = peer else { continue };
            if value_eq_opt(peer.get("machine"), Some(&machine)) {
                continue;
            }
            sessions += 1;
            machines.insert(hashable(peer.get("machine"))?);
        }
        Ok(Value::Object(
            obj! {"sessions" => sessions, "machines" => machines.len()},
        ))
    }

    fn accept_registration(
        &mut self,
        response: &Map<String, Value>,
        metadata: &Meta,
    ) -> Result<()> {
        let machine = match response.get("machine") {
            Some(Value::String(machine)) if !machine.is_empty() => machine.clone(),
            _ => {
                return Err(Error::value(
                    "the server did not return the machine identity",
                ))
            }
        };
        if self.machine.as_ref().is_some_and(|known| *known != machine) {
            return Err(Error::value(
                "the server reported a different machine identity",
            ));
        }
        let registration = match response.get("registration") {
            None | Some(Value::Null) => None,
            Some(value) => Some(valid_uuid(Some(value))?),
        };
        if self.online && registration != self.registration {
            self.withdraw()?;
        }
        let first = self.machine.is_none();
        self.machine = Some(machine.clone());
        let negotiated: Vec<String> = match response.get("session_metadata_fields") {
            None => Vec::new(),
            Some(Value::Array(fields)) => {
                let mut list: Vec<String> = fields
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|f| matches!(*f, "project" | "repository" | "branch"))
                    .map(str::to_string)
                    .collect();
                list.sort();
                list.dedup();
                list
            }
            Some(_) => Vec::new(),
        };
        let capability_changed = negotiated != self.session_metadata_fields;
        self.session_metadata_fields = negotiated;
        let mut offered: Vec<String> = match response.get("capabilities") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .filter(|c| BRIDGE_CAPABILITIES.contains(c))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        offered.sort();
        offered.dedup();
        self.shared
            .ack
            .store(offered.iter().any(|c| c == "ack"), Ordering::SeqCst);
        self.hub_capabilities = offered;
        self.registration = registration;
        self.advertised = metadata.sessions.clone();
        self.last_register = metadata.submitted;
        self.online = true;
        self.refused = None;
        *self.shared.poll_state.lock().expect("poll state") = Some((
            self.generation,
            self.registration.clone(),
            self.peers_revision.clone(),
        ));
        if let Some(peers) = self.peers.clone() {
            self.update_peers(&Value::Array(peers))?;
        }
        self.publish_state()?;
        if first {
            self.say(obj! {"event" => "ready", "machine" => machine});
        }
        if capability_changed || metadata.revision != self.roster_revision {
            self.next_scan = 0.0;
        } else {
            let interval: f64 = if self.watcher.as_ref().is_some_and(|w| w.healthy()) {
                30.0
            } else {
                5.0
            };
            let interval = if self.registration.is_none() {
                interval.min(20.0)
            } else {
                interval
            };
            self.next_scan = self.now() + interval;
            if let Some(at) = self.helper_retry_at {
                self.next_scan = self.next_scan.min(at);
            }
        }
        Ok(())
    }

    fn update_peers(&mut self, peers: &Value) -> Result<()> {
        let Value::Array(peers) = peers else {
            return Err(Error::value("invalid peer roster"));
        };
        let machine = self.machine_value();
        let mut desired: IndexMap<String, (String, String)> = IndexMap::new();
        let mut used_names: HashSet<String> = HashSet::new();
        if !self.records.is_empty() {
            for peer in peers {
                let Value::Object(peer) = peer else { continue };
                if value_eq_opt(peer.get("machine"), Some(&machine)) {
                    continue;
                }
                let id = valid_uuid(peer.get("id"))?;
                let Some(Value::String(remote_machine)) = peer.get("machine") else {
                    return Err(Error::value("invalid remote machine"));
                };
                let route = format!("{remote_machine}/{id}");
                let raw_name = match peer.get("name") {
                    None => "Claude".to_string(),
                    Some(Value::String(name)) => name.clone(),
                    Some(_) => return Err(Error::value("invalid name")),
                };
                let dashed = membership::dash_unsafe(&clean_name(&raw_name));
                let mut base = dashed.trim_matches('-').to_string();
                if base.is_empty() {
                    base = "Claude".into();
                }
                let host = membership::dash_unsafe(remote_machine);
                let host16 = text::take_chars(&host, 16);
                let short = text::take_chars(&id, 8);
                let mut candidate = format!(
                    "switchboard/{host16}/{}-{short}",
                    text::take_chars(&base, 26)
                );
                let mut suffix = 2;
                while used_names.contains(&candidate) {
                    let tag = format!("-{suffix}");
                    let keep = 26usize.saturating_sub(tag.len());
                    candidate = format!(
                        "switchboard/{host16}/{}-{short}{tag}",
                        text::take_chars(&base, keep)
                    );
                    suffix += 1;
                }
                used_names.insert(candidate.clone());
                let status = match peer.get("status").and_then(Value::as_str) {
                    Some(status) if ACTIVITY_STATES.contains(&status) => status.to_string(),
                    _ => "unknown".to_string(),
                };
                desired.insert(route, (candidate, status));
            }
        }
        if desired.len() > MAX_HELPERS {
            self.withdraw()?;
            return Err(Error::value("remote peer count exceeds 128 helper limit"));
        }
        let mut namespaces: Vec<String> = Vec::new();
        for record in self.records.values() {
            let namespace = fsutil::parent(&socket_path(record)?);
            if !namespaces.contains(&namespace) {
                namespaces.push(namespace);
            }
        }
        if !desired.is_empty() && namespaces.len() != 1 {
            self.withdraw()?;
            return Err(Error::value(
                "enrolled sessions span unsupported socket namespaces",
            ));
        }
        let routes: Vec<String> = self.helpers.keys().cloned().collect();
        for route in routes {
            let keep = desired.contains_key(&route)
                && namespaces.contains(&self.helpers[&route].socket_dir);
            if !keep {
                self.close_helper(&route);
            }
        }
        let mut sessions = self.config.sessions.clone();
        sessions.sort();
        for (route, (name, remote_status)) in desired {
            if !self.helpers.contains_key(&route) {
                let args = SpawnArgs {
                    route: route.clone(),
                    name: name.clone(),
                    socket_dir: namespaces[0].clone(),
                    remote_status: remote_status.clone(),
                    generation: self.generation,
                    sessions: self.config.sessions.clone(),
                };
                let helper = match self.spawn_helper.as_mut() {
                    Some(spawn) => spawn(&args)?,
                    None => Helper::spawn(
                        &self.program,
                        &self.runtime_config.clone().unwrap_or_default(),
                        &args.sessions,
                        &args.route,
                        &args.name,
                        &args.socket_dir,
                        self.shared.events.clone(),
                        args.generation,
                        &args.remote_status,
                    )?,
                };
                self.helpers.insert(route, helper);
                continue;
            }
            let helper = self.helpers.get_mut(&route).expect("helper");
            if helper.name != name
                || helper.sessions != sessions
                || helper.remote_status != remote_status
            {
                helper.revision += 1;
                helper.name = name.clone();
                helper.sessions = sessions.clone();
                helper.remote_status = remote_status.clone();
                let command = obj! {
                    "op" => "configure", "revision" => helper.revision, "name" => name,
                    "sessions" => Value::Array(sessions.iter().cloned().map(Value::from).collect()),
                    "status" => remote_status,
                };
                if let Err(error) = helper.deliver(&command) {
                    if !handled(&error) {
                        return Err(error);
                    }
                    self.close_helper(&route);
                    self.helper_retry_at = Some(self.now() + 2.0);
                }
            }
        }
        Ok(())
    }

    fn status(
        &mut self,
        identifier: &str,
        state: Option<&Value>,
        detail: Option<&Value>,
    ) -> Result<()> {
        let mut body = obj! {"id" => identifier, "state" => state.cloned().unwrap_or(Value::Null)};
        if truthy(detail) {
            body.insert("detail".into(), detail.cloned().unwrap_or(Value::Null));
        }
        let meta = Meta {
            id: Some(identifier.to_string()),
            ..Meta::default()
        };
        if !self.submit_request("status", "/v1/status", body, meta)? {
            self.say(
                obj! {"event" => "request_failed", "operation" => "status", "id" => identifier,
                "detail" => "local request queue full"},
            );
        }
        Ok(())
    }

    fn message(&mut self, event: &Value) -> Result<()> {
        let Value::Object(event) = event else {
            return Err(uncaught(format!(
                "AttributeError: '{}' object has no attribute 'get'",
                crate::discovery::type_name(Some(event))
            )));
        };
        let kind = event.get("type");
        if kind.and_then(Value::as_str) == Some("ping") {
            let identifier = valid_uuid(event.get("id"))?;
            let target = split_target(event.get("to"))?;
            let live = target.len() == 2
                && self.machine.as_deref() == Some(target[0].as_str())
                && self.records.contains_key(&target[1]);
            let meta = Meta {
                id: Some(identifier.clone()),
                ..Meta::default()
            };
            if !self.submit_request(
                "pong",
                "/v1/pong",
                obj! {"id" => identifier.clone(), "live" => live},
                meta,
            )? {
                self.say(
                    obj! {"event" => "request_failed", "operation" => "pong", "id" => identifier,
                    "detail" => "local request queue full"},
                );
            }
            return Ok(());
        }
        if kind.and_then(Value::as_str) == Some("status") {
            let identifier = valid_uuid(event.get("id"))?;
            if event.contains_key("ack_id") {
                // Redelivered observations must not repeat native notices.
                let key = (
                    identifier.clone(),
                    hashable(event.get("state"))?,
                    hashable(event.get("detail"))?,
                );
                if self.status_seen.contains_key(&key) {
                    return Ok(());
                }
                if self.status_seen.len() >= 4096 {
                    self.status_seen.shift_remove_index(0);
                }
                self.status_seen.insert(key, true);
            }
            self.say(obj! {"event" => "status", "id" => identifier.clone(),
            "state" => event.get("state").cloned().unwrap_or(Value::Null)});
            let state = event.get("state").cloned().unwrap_or(Value::Null);
            let detail = event.get("detail").cloned().unwrap_or(Value::Null);
            return self.relay_outcome(&identifier, &state, &detail);
        }
        if kind.and_then(Value::as_str) != Some("message") {
            return Err(Error::value("unsupported server event"));
        }
        let identifier = valid_uuid(event.get("id"))?;
        if self.deliveries.contains_key(&identifier)
            || self
                .delivered
                .as_ref()
                .is_some_and(|d| d.contains(&identifier))
        {
            return Ok(()); // Already handed to a helper: never hand it to Claude twice.
        }
        let target = split_target(event.get("to"))?;
        if target.len() != 2
            || self.machine.as_deref() != Some(target[0].as_str())
            || !self.records.contains_key(&target[1])
        {
            return self.status(
                &identifier,
                Some(&Value::from("failed")),
                Some(&Value::from("local session is no longer enrolled/live")),
            );
        }
        let sender = str_key(event.get("from"))?;
        let Some(route) = sender.filter(|route| self.helpers.contains_key(route)) else {
            return self.status(
                &identifier,
                Some(&Value::from("failed")),
                Some(&Value::from("remote sender proxy is unavailable")),
            );
        };
        if self.deliveries.len() >= 10000 {
            self.deliveries.shift_remove_index(0);
        }
        self.deliveries.insert(identifier.clone(), route.clone());
        self.note_activity(&target[1], None, true);
        if let Some(delivered) = self.delivered.as_mut() {
            if let Err(error) = delivered.add(&identifier) {
                if !error.is_os() {
                    return Err(error);
                }
                self.say(
                    obj! {"event" => "request_failed", "operation" => "record_delivery",
                    "id" => identifier.clone(), "detail" => error.truncated(256)},
                );
            }
        }
        let mut command = obj! {
            "op" => "deliver", "id" => identifier.clone(), "target_session" => target[1].clone(),
            "text" => event.get("text").cloned().unwrap_or(Value::Null),
            "mode" => event.get("mode").cloned().unwrap_or(Value::Null),
            "sender_name" => event.get("sender_name").cloned().unwrap_or(Value::Null),
        };
        if truthy(event.get("hop_chain")) {
            command.insert("hop_chain".into(), event["hop_chain"].clone());
        }
        let helper = self.helpers.get_mut(&route).expect("helper");
        match helper.deliver(&command) {
            Ok(()) => Ok(()),
            Err(error) if error.kind == Kind::WriteUnknown => {
                self.close_helper(&route);
                self.status(
                    &identifier,
                    Some(&Value::from("unknown")),
                    Some(&Value::from(
                        "native helper IPC outcome uncertain; no replay",
                    )),
                )
            }
            Err(error) if error.is_os() || error.kind == Kind::Value => self.status(
                &identifier,
                Some(&Value::from("failed")),
                Some(&Value::from("native helper IPC failed before write")),
            ),
            Err(error) => Err(error),
        }
    }

    fn handle(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Poll(generation, response, ticket) => {
                let result = self.handle_poll(generation, &response, &ticket);
                ticket.done.set();
                result
            }
            other => self.handle_event(other),
        }
    }

    fn handle_poll(
        &mut self,
        source: u64,
        event: &Map<String, Value>,
        ticket: &Ticket,
    ) -> Result<()> {
        if source != self.generation || !self.online {
            return Ok(());
        }
        if !value_eq_opt(event.get("machine"), Some(&self.machine_value())) {
            return Err(Error::value("poll identity mismatch"));
        }
        if let Some(registration) = &self.registration {
            if event.get("registration").and_then(Value::as_str) != Some(registration.as_str()) {
                self.withdraw()?;
                self.next_scan = 0.0;
                self.say(obj! {"event" => "offline", "detail" => "server registration lost; native proxies withdrawn"});
                return Ok(());
            }
        }
        let revision = match event.get("peers_revision") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if (1..=128).contains(&text::char_len(value)) => {
                Some(value.clone())
            }
            Some(_) => return Err(Error::value("invalid peers revision")),
        };
        let peers = if let Some(peers) = event.get("peers") {
            peers.clone()
        } else if self.registration.is_some()
            && self.peers.is_some()
            && revision.is_some()
            && revision == self.peers_revision
        {
            Value::Array(self.peers.clone().unwrap_or_default())
        } else {
            return Err(Error::value(
                "missing peer roster without matching revision",
            ));
        };
        self.update_peers(&peers)?;
        self.peers = match peers {
            Value::Array(items) => Some(items),
            _ => None,
        };
        self.peers_revision = revision.clone();
        if Some(self.remote_summary()?) != self.published_remote {
            self.publish_state()?;
        }
        *self.shared.poll_state.lock().expect("poll state") =
            Some((self.generation, self.registration.clone(), revision));
        let messages: Vec<Value> = match event.get("events") {
            None => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(Value::Object(map)) => map.keys().map(|k| Value::from(k.clone())).collect(),
            Some(Value::String(text)) => text.chars().map(|c| Value::from(c.to_string())).collect(),
            Some(other) => {
                return Err(Error::type_error(format!(
                    "'{}' object is not iterable",
                    crate::discovery::type_name(Some(other))
                )))
            }
        };
        for message in &messages {
            let result = self.message(message);
            // Processed (or rejected exactly as before): never redeliver it.
            if let Some(Value::String(ack)) = message.as_object().and_then(|m| m.get("ack_id")) {
                ticket.acks.lock().expect("acks").push(ack.clone());
            }
            result?;
        }
        if !self.resend.is_empty() {
            self.flush_resends()?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Requests => self.drain_completions(),
            Event::Discovery => {
                *self.shared.discovery_pending.lock().expect("discovery") = false;
                if self.watcher.as_ref().is_some_and(|w| w.healthy()) {
                    self.next_scan = 0.0;
                } else {
                    // A failed constructor also notifies; never turn its retry
                    // into a self-triggering scan loop on unsupported systems.
                    self.next_scan = self.next_scan.min(self.now() + 5.0);
                }
                Ok(())
            }
            Event::Stop => Ok(()),
            Event::PollError(source, detail) => {
                if source == self.generation {
                    self.say(obj! {"event" => "request_failed", "operation" => "poll", "detail" => detail});
                }
                Ok(())
            }
            Event::Offline(source, refused) => {
                if source == self.generation {
                    self.refused = refused;
                    self.withdraw()?;
                    self.retry_at = self.now() + 2.0;
                    self.next_scan = self.retry_at;
                    self.say(obj! {"event" => "offline", "detail" => "server unavailable; native proxies withdrawn"});
                }
                Ok(())
            }
            Event::Poll(..) => Ok(()),
            Event::Helper(identity, value) => self.helper_event(identity, value),
            Event::HelperWrite(identity, uncertain, unwritten) => {
                self.helper_write_failed(identity, uncertain, unwritten)
            }
            Event::HelperExit(identity, pid) => {
                let (route, _, generation) = identity.clone();
                let current = self
                    .helpers
                    .get(&route)
                    .is_some_and(|h| h.identity == identity);
                if generation != self.generation || !current {
                    return Ok(());
                }
                if self.helpers[&route].pid == pid {
                    self.close_helper(&route);
                    self.say(obj! {"event" => "helper_failed", "route" => route});
                    self.next_scan = 0.0; // Revalidate local records before cached-roster recovery.
                }
                Ok(())
            }
        }
    }

    /// A helper stopped taking commands: close it, and settle the messages
    /// it may not have been given (its writer already sent it SIGTERM).
    fn helper_write_failed(
        &mut self,
        identity: Identity,
        uncertain: Unsent,
        unwritten: Vec<Unsent>,
    ) -> Result<()> {
        let (route, _, generation) = identity.clone();
        if generation != self.generation || !self.online {
            return Ok(()); // The server's own timeout settles these.
        }
        if self
            .helpers
            .get(&route)
            .is_some_and(|helper| helper.identity == identity)
        {
            self.close_helper(&route);
            self.say(obj! {"event" => "helper_failed", "route" => route});
        }
        let settle = std::iter::once((
            uncertain,
            "unknown",
            "native helper IPC outcome uncertain; no replay",
        ))
        .chain(
            unwritten
                .into_iter()
                .map(|command| (command, "failed", "native helper IPC failed before write")),
        );
        for (command, state, detail) in settle {
            if let (Some(id), "deliver") = (command.id.as_deref(), command.op.as_str()) {
                self.status(id, Some(&Value::from(state)), Some(&Value::from(detail)))?;
            }
        }
        Ok(())
    }

    fn helper_event(&mut self, identity: Identity, event: Map<String, Value>) -> Result<()> {
        let (route, _, generation) = identity.clone();
        let current = self
            .helpers
            .get(&route)
            .is_some_and(|h| h.identity == identity);
        if generation != self.generation || !current || !self.online {
            return Ok(());
        }
        let source = route;
        let revision_value = Value::from(self.helpers[&source].revision);
        let kind = event
            .get("event")
            .and_then(Value::as_str)
            .map(str::to_string);
        match kind.as_deref() {
            Some("configured") => {
                let helper = self.helpers.get_mut(&source).expect("helper");
                if value_eq_opt(event.get("revision"), Some(&revision_value)) {
                    helper.configured_revision = helper.revision;
                } else {
                    let newer = match event.get("revision").and_then(crate::wirejson::as_int) {
                        Some(digits) => {
                            crate::wirejson::int_cmp(&digits, &helper.revision.to_string())
                                == std::cmp::Ordering::Greater
                        }
                        None => true,
                    };
                    if newer {
                        self.close_helper(&source);
                        let retry = self.now() + 2.0;
                        self.helper_retry_at = Some(retry);
                        self.next_scan = retry;
                    }
                }
            }
            Some("configure_rejected") => {
                if value_eq_opt(event.get("revision"), Some(&revision_value)) {
                    self.close_helper(&source);
                    self.next_scan = 0.0;
                }
            }
            Some("outbound") => {
                let helper = &self.helpers[&source];
                let stale = !value_eq_opt(event.get("revision"), Some(&revision_value))
                    || helper.configured_revision != helper.revision;
                if stale {
                    if str_key(event.get("source_session"))?
                        .is_some_and(|sid| self.records.contains_key(&sid))
                    {
                        let identifier = valid_uuid(event.get("native_id"))?;
                        self.outbound_failure(
                            &identifier,
                            &source,
                            "failed",
                            "helper configuration changed before message submission",
                            None,
                        );
                    }
                    return Ok(());
                }
                let sid = str_key(event.get("source_session"))?;
                // Refresh liveness before publishing, not just at registration ticks.
                let selection: Selection = self.config.sessions.iter().cloned().collect();
                let live = (self.discover)(&self.config.config_dir, &selection)?;
                let Some(sid) = sid.filter(|sid| live.contains_key(sid)) else {
                    return Ok(());
                };
                if event.get("recipient").and_then(Value::as_str) != Some(source.as_str()) {
                    return Err(Error::value("helper recipient mismatch"));
                }
                let identifier = valid_uuid(event.get("native_id"))?;
                let machine = self.machine.clone().unwrap_or_default();
                let mut payload = obj! {
                    "id" => identifier.clone(), "from" => format!("{machine}/{sid}"), "to" => source.clone(),
                    "text" => event.get("text").cloned().unwrap_or(Value::Null),
                    "mode" => event.get("mode").cloned().unwrap_or(Value::Null),
                };
                if truthy(event.get("hop_chain")) {
                    payload.insert("hop_chain".into(), event["hop_chain"].clone());
                }
                if self.outbound.len() >= 10000 {
                    self.outbound.shift_remove_index(0);
                }
                let excerpt = match event.get("text") {
                    Some(Value::String(text)) => text::take_chars(text, 240),
                    _ => String::new(),
                };
                self.outbound.insert(
                    identifier.clone(),
                    Outbound {
                        route: source.clone(),
                        helper: self.helpers[&source].identity.clone(),
                        source_session: sid.clone(),
                        excerpt,
                        mode: event.get("mode").cloned().unwrap_or(Value::Null),
                        notified: false,
                    },
                );
                self.note_activity(&sid, None, true);
                let meta = Meta {
                    id: Some(identifier.clone()),
                    route: Some(source.clone()),
                    source_session: Some(sid),
                    helper_identity: Some(self.helpers[&source].identity.clone()),
                    body: Some(payload.clone()),
                    ..Meta::default()
                };
                if !self.submit_request("send", "/v1/send", payload, meta)? {
                    self.outbound_failure(
                        &identifier,
                        &source,
                        "failed",
                        "local request queue full",
                        None,
                    );
                }
            }
            Some("status") => {
                let id = str_key(event.get("id"))?;
                if let Some(id) = id.filter(|id| self.deliveries.get(id) == Some(&source)) {
                    self.status(&id, event.get("state"), event.get("detail"))?;
                }
            }
            Some("rejected") => {
                let detail = event
                    .get("detail")
                    .cloned()
                    .unwrap_or_else(|| Value::from("native frame rejected"));
                self.say(
                    obj! {"event" => "native_rejected", "route" => source, "detail" => detail},
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn poll_loop(shared: Arc<Shared>, post: FakePost) {
        let mut acks: Vec<String> = Vec::new();
        while !shared.stop.is_set() {
            let snapshot = shared.poll_state.lock().expect("poll state").clone();
            let Some((generation, registration, revision)) = snapshot else {
                shared.stop.wait(Duration::from_millis(250));
                continue;
            };
            let mut body = Map::new();
            if let Some(registration) = &registration {
                body.insert("registration".into(), Value::from(registration.clone()));
                body.insert(
                    "peers_revision".into(),
                    revision.map(Value::from).unwrap_or(Value::Null),
                );
                if shared.ack.load(Ordering::SeqCst) {
                    let start = acks.len().saturating_sub(512);
                    body.insert(
                        "acks".into(),
                        Value::Array(acks[start..].iter().cloned().map(Value::from).collect()),
                    );
                }
            }
            let failure = match post("/v1/poll", &Value::Object(body)) {
                Ok(response) => {
                    acks.clear(); // The server has these; a lost reply only causes deduplicated redelivery.
                    let ticket = Arc::new(Ticket::default());
                    match shared.events.put_timeout(
                        Event::Poll(generation, response, ticket.clone()),
                        Duration::from_secs(1),
                    ) {
                        Ok(()) => {
                            // Main-loop acceptance publishes the next cursor before we poll.
                            while !shared.stop.is_set()
                                && !ticket.done.wait(Duration::from_millis(250))
                            {
                            }
                            acks = ticket.acks.lock().expect("acks").clone();
                            None
                        }
                        Err(_) => Some(Event::Offline(generation, None)),
                    }
                }
                Err(error) => Some(if global_request_error(&error) {
                    Event::Offline(generation, machine_refusal(&error))
                } else {
                    Event::PollError(generation, error.truncated(256))
                }),
            };
            if let Some(event) = failure {
                if shared
                    .events
                    .put_timeout(event, Duration::from_secs(1))
                    .is_err()
                {
                    shared.stop.set();
                }
                shared.stop.wait(Duration::from_secs(2));
            }
        }
    }

    /// Run until stopped; the bridge's exit status.
    pub fn run(&mut self) -> Result<()> {
        if !self.enabled {
            self.say(obj! {"event" => "disabled"});
            return Ok(());
        }
        let result = self.run_enabled();
        self.shared.request_stop();
        self.close_watcher();
        let withdrawn = self.withdraw();
        // One terminate and kill deadline for every helper still running.
        helper_process::close_group(Vec::new(), std::mem::take(&mut self.closing));
        self.finish_shutdown();
        result.and(withdrawn)?;
        if self.shared.emit_failed.load(Ordering::SeqCst) {
            return Err(uncaught("log output failed"));
        }
        Ok(())
    }

    fn run_enabled(&mut self) -> Result<()> {
        self.start_observability()?;
        self.runtime()?;
        let shared = self.shared.clone();
        let post: FakePost = match (&self.requests, &self.client) {
            (Some(Runner::Immediate(post)), _) => post.clone(),
            (_, Some(client)) => {
                let client = client.clone();
                Arc::new(move |path: &str, body: &Value| client.post(path, body))
            }
            _ => return Err(Error::value("bridge has no server transport")),
        };
        self.poller = Some(
            std::thread::Builder::new()
                .name("switchboard-poll".into())
                .stack_size(256 * 1024)
                .spawn(move || Bridge::poll_loop(shared, post))
                .map_err(|exc| Error::from_io(&exc, None))?,
        );
        self.event_loop()
    }

    fn finish_shutdown(&mut self) {
        // Remove all discoverable local runtime before waiting on network
        // threads; keep the singleton lock until local state is shut down.
        self.remove_runtime();
        self.remove_observability();
        if let Some(Runner::Workers(requests)) = &self.requests {
            requests.close();
        }
        // The poll thread only queues events; process exit ends its transport.
        self.poller.take();
        if let Some(client) = &self.client {
            client.close();
        }
        self.close_observability();
    }
}

fn opt_list_eq(left: &Option<Vec<Value>>, right: &Option<Vec<Value>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => value_eq(&Value::Array(a.clone()), &Value::Array(b.clone())),
        _ => false,
    }
}

fn hub_to_error(error: HubError) -> Error {
    match error {
        HubError::Value(message) => Error::value(message),
        other => Error::os(other.to_string()),
    }
}

const BRIDGE_USAGE: &str = "Usage: switchboard bridge --config FILE\n";
const BRIDGE_HELP: &str = "Usage: switchboard bridge --config FILE

Run the bridge: it connects this machine's joined Claude Code chats to the
server. The user service that switchboard install sets up runs it this way.

Options:
  --config FILE  The bridge configuration (~/.config/switchboard/bridge.json)
  -h, --help     Show this help
";

/// Parse `--config PATH` (or `--config=PATH`).
fn parse_args(argv: &[String]) -> std::result::Result<String, (i32, String)> {
    let mut config = None;
    let mut index = 0;
    let error = |message: &str| (2, format!("switchboard bridge: {message}\n{BRIDGE_USAGE}"));
    while index < argv.len() {
        let arg = &argv[index];
        if arg == "-h" || arg == "--help" {
            return Err((0, BRIDGE_HELP.to_string()));
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        if name == "--config" {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    match argv.get(index) {
                        Some(value) if !value.starts_with('-') || value == "-" => value.clone(),
                        _ => return Err(error("--config needs a value")),
                    }
                }
            };
            config = Some(value);
            index += 1;
            continue;
        }
        return Err(error(&if arg.starts_with('-') {
            format!("unknown option {arg}")
        } else {
            format!("unexpected argument {arg}")
        }));
    }
    config.ok_or_else(|| error("--config is required"))
}

/// The signal-wakeup pipe: SIGTERM and SIGINT request a clean stop.
fn install_signal_stop(stop: impl Fn() + Send + 'static) -> Result<()> {
    let mut pipe = [0 as RawFd; 2];
    // SAFETY: pipe(2) fills two descriptors.
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return Err(Error::errno(Error::last_os()));
    }
    for fd in pipe {
        // SAFETY: fcntl on owned descriptors.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    crate::net::set_nonblocking(pipe[1], true);
    // SAFETY: the read end is owned by the watcher thread from here on.
    let reader = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
    std::thread::Builder::new()
        .name("switchboard-signals".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                // SAFETY: blocking read of one byte from the owned pipe.
                let count = unsafe {
                    libc::read(
                        reader.as_raw_fd(),
                        byte.as_mut_ptr() as *mut libc::c_void,
                        1,
                    )
                };
                if count > 0 && crate::signals::signalled() {
                    stop();
                } else if count == 0 {
                    return;
                }
            }
        })
        .map_err(|exc| Error::from_io(&exc, None))?;
    crate::signals::install(pipe[1], -1);
    Ok(())
}

/// `switchboard bridge --config PATH`.
pub fn main(argv: &[String]) -> i32 {
    let config = match parse_args(argv) {
        Ok(config) => config,
        Err((code, text)) => {
            if code == 0 {
                print!("{text}");
            } else {
                eprint!("{text}");
            }
            return code;
        }
    };
    // Without a configuration (this machine left its server, or it was
    // removed) say so plainly. The service starts the bridge only while
    // the configuration exists, so this is printed once, not every restart.
    if std::fs::symlink_metadata(&config).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        eprintln!("{}", missing_config_line(&config));
        return 1;
    }
    let mut bridge = match Bridge::new(&config, Arc::new(log_line)) {
        Ok(bridge) => bridge,
        Err(error) => {
            eprintln!("switchboard bridge: {}", error.message);
            return 1;
        }
    };
    if let Err(error) = install_signal_stop(bridge.stop_handle()) {
        eprintln!("switchboard bridge: {}", error.message);
        return 1;
    }
    match bridge.run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("switchboard bridge: {}", error.message);
            1
        }
    }
}

/// What the bridge prints when its configuration doesn't exist.
fn missing_config_line(config: &str) -> String {
    format!(
        "switchboard bridge: there is no configuration at {config}, so the bridge has nothing to run \
         (this machine left its server, or hasn't joined one). It stays stopped until \
         `switchboard init` joins a server."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejections_classify_as_global_or_explicit() {
        let http = |status, code: &str| HubError::Http {
            status,
            code: Some(code.to_string()),
        };
        assert!(!global_request_error(&http(404, "target_offline")));
        assert!(global_request_error(&http(403, "not_enrolled")));
        assert!(global_request_error(&http(500, "target_offline")));
        assert!(global_request_error(&HubError::Http {
            status: 502,
            code: None
        }));
        assert!(global_request_error(&HubError::Os("timed out".into())));
        assert!(!global_request_error(&HubError::Cancelled));
        assert!(explicit_rejection(&http(503, "queue_full")));
        assert!(!explicit_rejection(&http(500, "queue_full")));
        assert_eq!(
            send_reason(&http(404, "target_offline")),
            "the recipient chat is offline or no longer joined"
        );
        assert_eq!(
            send_reason(&HubError::Cancelled),
            "the send was cancelled before it reached the server"
        );
        assert_eq!(
            send_reason(&http(400, "invalid_route")),
            "the server refused the message (server HTTP 400 (invalid_route))"
        );
    }

    #[test]
    fn arguments_parse() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&args(&["--config", "x"])).unwrap(), "x");
        assert_eq!(parse_args(&args(&["--config=y"])).unwrap(), "y");
        assert_eq!(
            parse_args(&args(&["--conf=y"])).unwrap_err().1,
            "switchboard bridge: unknown option --conf=y\nUsage: switchboard bridge --config FILE\n"
        );
        assert_eq!(parse_args(&args(&[])).unwrap_err().0, 2);
        assert_eq!(parse_args(&args(&["--config"])).unwrap_err().0, 2);
        assert_eq!(parse_args(&args(&["-h"])).unwrap_err().0, 0);
    }

    #[test]
    fn delivered_ids_persist_and_prune() {
        let dir = std::env::temp_dir().join(format!("sbd-{}", uuid4_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.to_string_lossy().into_owned();
        let mut ids = DeliveredIds::load(&state);
        ids.add("a").unwrap();
        let again = DeliveredIds::load(&state);
        assert!(again.contains("a"));
        let raw = std::fs::read_to_string(dir.join("delivered-ids.json")).unwrap();
        assert!(raw.starts_with("{\"ids\": {\"a\": "));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
