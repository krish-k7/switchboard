//! The bridge against a fake server (requests complete as they are
//! submitted), a replaceable discovery function and clock, and helpers
//! without processes.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use super::helper_process::{FakeLog, Helper};
use super::*;
use crate::identity::uuid4;

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// The fake server's behavior; tests change it between calls.
struct Hub {
    registration: Option<String>,
    extra: Map<String, Value>,
    fail: Option<HubError>,
    reply: Option<Map<String, Value>>,
}

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    config_dir: PathBuf,
    path: PathBuf,
    sid: String,
    config: Value,
    calls: Calls,
    hub: Arc<Mutex<Hub>>,
    events: Arc<Mutex<Vec<Map<String, Value>>>>,
    found: Arc<Mutex<IndexMap<String, Record>>>,
    discovered: Arc<Mutex<usize>>,
    clock: Arc<Mutex<f64>>,
    record: Record,
    bridge: Bridge,
}

fn temp_root() -> PathBuf {
    let template = std::ffi::CString::new("/tmp/sbb-XXXXXX").unwrap();
    let mut raw = template.into_bytes_with_nul();
    // SAFETY: raw is a writable NUL-terminated template.
    let made = unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) };
    assert!(!made.is_null());
    raw.pop();
    std::fs::canonicalize(PathBuf::from(String::from_utf8(raw).unwrap())).unwrap()
}

fn s(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn mkdir(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn write_private(path: &std::path::Path, value: &Value) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, serde_json::to_string(value).unwrap()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let root = temp_root();
        let config_dir = root.join("claude");
        mkdir(&config_dir, 0o700);
        let state = root.join("state");
        mkdir(&state, 0o700);
        let key = root.join("key.pem");
        write_private(&key, &json!("fixture"));
        let sid = uuid4();
        let config = json!({"enabled": true, "hub_url": "https://localhost:8743", "ca": s(&root.join("ca.pem")),
                            "cert": s(&root.join("cert.pem")), "key": s(&key), "config_dir": s(&config_dir),
                            "state_dir": s(&state), "sessions": [sid]});
        let path = root.join("bridge.json");
        write_private(&path, &config);
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let hub = Arc::new(Mutex::new(Hub {
            registration: Some(uuid4()),
            extra: Map::new(),
            fail: None,
            reply: None,
        }));
        let events: Arc<Mutex<Vec<Map<String, Value>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut record = Map::new();
        record.insert("sessionId".into(), json!(sid));
        record.insert("name".into(), json!("fixture"));
        record.insert("status".into(), json!("idle"));
        record.insert(
            "messagingSocketPath".into(),
            json!(s(&root.join("cc-socks/123.sock"))),
        );
        let bridge = Self::make(&path, &calls, &hub, &events);
        let mut fixture = Fixture {
            root,
            state,
            config_dir,
            path,
            sid,
            config,
            calls,
            hub,
            events,
            found: Arc::new(Mutex::new(IndexMap::new())),
            discovered: Arc::new(Mutex::new(0)),
            clock: Arc::new(Mutex::new(1000.0)),
            record,
            bridge,
        };
        fixture.attach();
        fixture.bridge.runtime().unwrap();
        fixture
    }

    fn make(
        path: &std::path::Path,
        calls: &Calls,
        hub: &Arc<Mutex<Hub>>,
        events: &Arc<Mutex<Vec<Map<String, Value>>>>,
    ) -> Bridge {
        let (calls, hub, log) = (calls.clone(), hub.clone(), events.clone());
        let post: FakePost = Arc::new(move |path: &str, body: &Value| {
            calls.lock().unwrap().push((path.to_string(), body.clone()));
            let hub = hub.lock().unwrap();
            if let Some(error) = &hub.fail {
                return Err(error.clone());
            }
            if let Some(reply) = &hub.reply {
                return Ok(reply.clone());
            }
            let mut reply = obj! {"ok" => true, "machine" => "local",
            "registration" => hub.registration.clone().map(Value::from).unwrap_or(Value::Null)};
            reply.extend(hub.extra.clone());
            Ok(reply)
        });
        let emit: Emit = Arc::new(move |value: &Map<String, Value>| {
            log.lock().unwrap().push(value.clone());
            true
        });
        Bridge::with_fake_server(&s(path), emit, post).unwrap()
    }

    /// Install the replaceable discovery and clock on the current bridge.
    fn attach(&mut self) {
        let (found, count) = (self.found.clone(), self.discovered.clone());
        self.bridge.discover = Box::new(move |_: &str, selection: &Selection| {
            *count.lock().unwrap() += 1;
            Ok(found
                .lock()
                .unwrap()
                .iter()
                .filter(|(sid, _)| selection.contains(*sid))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        });
        self.bridge.clock = Some(self.clock.clone());
    }

    fn discover_returns(&self, records: &[&Record]) {
        let mut found = self.found.lock().unwrap();
        found.clear();
        for record in records {
            found.insert(
                record["sessionId"].as_str().unwrap().to_string(),
                (*record).clone(),
            );
        }
    }

    fn register(&mut self) {
        self.bridge.register().unwrap();
        self.bridge.drain_completions().unwrap();
    }

    /// Register again once the reconnect backoff has passed on the clock.
    fn retry(&mut self) {
        let mut clock = self.clock.lock().unwrap();
        *clock = clock.max(self.bridge.reconnect_at);
        drop(clock);
        self.register();
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<Map<String, Value>> {
        self.events.lock().unwrap().clone()
    }

    fn write_config(&self) {
        write_private(&self.path, &self.config);
    }

    fn socket_dir(&self) -> String {
        crate::fsutil::resolve(&s(&self.root.join("cc-socks"))).unwrap()
    }

    fn remote_helper(&self, remote_id: &str) -> (Helper, Arc<Mutex<FakeLog>>) {
        let route = format!("remote/{remote_id}");
        Helper::fake(
            (route, uuid4_hex(), self.bridge.generation),
            &format!("switchboard/remote/fixture-{}", &remote_id[..8]),
            &self.socket_dir(),
            std::slice::from_ref(&self.sid),
            "idle",
        )
    }

    /// `connected(remote_id)`: online as `local`, the fixture chat live, one helper.
    fn connected(&mut self, remote_id: &str) -> (String, Identity, Arc<Mutex<FakeLog>>) {
        self.bridge.machine = Some("local".into());
        self.bridge.online = true;
        self.bridge.records = IndexMap::from([(self.sid.clone(), self.record.clone())]);
        let (helper, log) = self.remote_helper(remote_id);
        let (route, identity) = (helper.route.clone(), helper.identity.clone());
        self.bridge.helpers.insert(route.clone(), helper);
        (route, identity, log)
    }

    /// Count helpers created by `update_peers`, recording their names.
    fn count_spawns(&mut self) -> Arc<Mutex<Vec<String>>> {
        let names = Arc::new(Mutex::new(Vec::new()));
        let seen = names.clone();
        self.bridge.spawn_helper = Some(Box::new(move |args: &SpawnArgs| {
            seen.lock().unwrap().push(args.name.clone());
            Ok(Helper::fake(
                (args.route.clone(), uuid4_hex(), args.generation),
                &args.name,
                &args.socket_dir,
                &args.sessions,
                &args.remote_status,
            )
            .0)
        }));
        names
    }

    fn poll(&mut self, generation: u64, reply: Value) -> (Result<()>, Vec<String>) {
        let ticket = Arc::new(Ticket::default());
        let result = self.bridge.handle(Event::Poll(
            generation,
            reply.as_object().unwrap().clone(),
            ticket.clone(),
        ));
        assert!(ticket.done.is_set());
        let acks = ticket.acks.lock().unwrap().clone();
        (result, acks)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.bridge.remove_runtime();
        self.bridge.close_observability();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ops(log: &Arc<Mutex<FakeLog>>) -> Vec<Map<String, Value>> {
    log.lock().unwrap().delivered.clone()
}

/// Record `identifier` as sent by the fixture chat through `helper`.
fn sent(f: &mut Fixture, identifier: &str, helper: &Identity) {
    f.bridge.outbound.insert(
        identifier.to_string(),
        Outbound {
            route: helper.0.clone(),
            helper: helper.clone(),
            source_session: f.sid.clone(),
            excerpt: "hello there".into(),
            mode: json!("bypass"),
            notified: false,
        },
    );
}

#[test]
fn metadata_negotiation_is_legacy_first_and_cached_when_idle() {
    let mut f = Fixture::new();
    f.hub.lock().unwrap().extra.insert(
        "session_metadata_fields".into(),
        json!(["project", "repository", "branch"]),
    );
    f.record.insert("cwd".into(), json!(s(&f.root)));
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    f.register();
    let keys: Vec<String> = f.calls()[0].1["sessions"][0]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["id", "name", "status"]);
    f.register();
    assert_eq!(
        f.calls()[1].1["sessions"][0]["project"],
        json!(crate::fileops::name(&s(&f.root)))
    );
    for _ in 0..30 {
        f.register();
    }
    assert_eq!(f.calls().len(), 2);
}

#[test]
fn old_hub_never_receives_metadata() {
    let mut f = Fixture::new();
    f.record.insert("cwd".into(), json!(s(&f.root)));
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    for _ in 0..30 {
        f.register();
    }
    assert_eq!(f.calls().len(), 1);
    assert_eq!(f.calls()[0].1["sessions"][0].as_object().unwrap().len(), 3);
}

#[test]
fn remote_status_change_preserves_helper_and_revision_fences() {
    let mut f = Fixture::new();
    let remote_id = uuid4();
    let mut peer =
        json!({"machine": "remote", "id": remote_id, "name": "fixture", "status": "idle"});
    f.bridge.records = IndexMap::from([(f.sid.clone(), f.record.clone())]);
    let (helper, log) = f.remote_helper(&remote_id);
    let route = helper.route.clone();
    let identity = helper.identity.clone();
    f.bridge.helpers.insert(route.clone(), helper);
    let spawned = f.count_spawns();
    f.bridge.update_peers(&json!([peer.clone()])).unwrap();
    assert!(ops(&log).is_empty());
    peer["status"] = json!("busy");
    f.bridge.update_peers(&json!([peer.clone()])).unwrap();
    assert!(spawned.lock().unwrap().is_empty());
    let helper = &f.bridge.helpers[&route];
    assert_eq!(helper.identity, identity);
    assert_eq!((helper.revision, helper.configured_revision), (1, 0));
    assert_eq!(ops(&log).last().unwrap()["status"], "busy");
    peer["status"] = json!("bogus");
    f.bridge.update_peers(&json!([peer])).unwrap();
    assert_eq!(ops(&log).last().unwrap()["status"], "unknown");
}

#[test]
fn default_off_has_no_runtime_side_effect() {
    let f = Fixture::new();
    let path = f.root.join("disabled.json");
    write_private(&path, &json!({}));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let mut bridge = Bridge::new(
        &s(&path),
        Arc::new(move |value: &Map<String, Value>| {
            log.lock().unwrap().push(value.clone());
            true
        }),
    )
    .unwrap();
    bridge.run().unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![obj! {"event" => "disabled"}]);
    assert!(bridge.client.is_none());
}

#[test]
fn hot_enrollment_withdraws_helpers_and_omits_tls_runtime() {
    let mut f = Fixture::new();
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    f.register();
    let (helper, log) = f.remote_helper(&uuid4());
    f.bridge.helpers.insert(helper.route.clone(), helper);
    let runtime_path = f.bridge.runtime_config.clone().unwrap();
    let runtime: Value =
        serde_json::from_str(&std::fs::read_to_string(&runtime_path).unwrap()).unwrap();
    assert!(runtime.get("key").is_none() && runtime.get("cert").is_none());
    f.config["sessions"] = json!([]);
    f.write_config();
    f.register();
    assert_eq!(log.lock().unwrap().closed, 1);
    assert!(f.bridge.helpers.is_empty());
    assert_eq!(
        f.calls().last().unwrap().clone(),
        (
            "/v1/register".to_string(),
            json!({"sessions": [], "capabilities": ["ack", "ping"]})
        )
    );
    let runtime_path = f.bridge.runtime_config.clone().unwrap();
    let runtime: Value =
        serde_json::from_str(&std::fs::read_to_string(&runtime_path).unwrap()).unwrap();
    assert_eq!(runtime["sessions"], json!([]));
}

#[test]
fn helper_count_limit_fails_before_process_creation() {
    let mut f = Fixture::new();
    f.bridge.machine = Some("local".into());
    f.bridge.records = IndexMap::from([(f.sid.clone(), f.record.clone())]);
    let spawned = f.count_spawns();
    let peers: Vec<Value> = (0..129)
        .map(|_| json!({"machine": "remote", "id": uuid4(), "name": "peer"}))
        .collect();
    let error = f.bridge.update_peers(&Value::Array(peers)).unwrap_err();
    assert!(error.message.contains("128"));
    assert!(spawned.lock().unwrap().is_empty());
}

#[test]
fn proxy_names_are_native_safe_and_unique() {
    let mut f = Fixture::new();
    f.bridge.machine = Some("local".into());
    f.bridge.records = IndexMap::from([(f.sid.clone(), f.record.clone())]);
    let spawned = f.count_spawns();
    let peers: Vec<Value> = (0..2)
        .map(|_| json!({"machine": "remote", "id": format!("aaaaaaaa-{}", &uuid4()[9..]), "name": "fixture@odd name"}))
        .collect();
    f.bridge.update_peers(&Value::Array(peers)).unwrap();
    let names = spawned.lock().unwrap().clone();
    assert_eq!(names.iter().collect::<HashSet<_>>().len(), 2);
    for name in &names {
        assert!(name.len() <= 64 && !name.contains('@'));
        assert!(
            name.starts_with("switchboard/remote/fixture-odd-name-aaaaaaaa"),
            "{name}"
        );
        assert!(name[12..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/".contains(c)));
    }
}

#[test]
fn prefixed_proxy_name_preserves_suffix_with_maximum_input() {
    let mut f = Fixture::new();
    f.bridge.machine = Some("local".into());
    f.bridge.records = IndexMap::from([(f.sid.clone(), f.record.clone())]);
    let spawned = f.count_spawns();
    let peers: Vec<Value> = ["c", "d"]
        .iter()
        .map(|tail| {
            json!({"machine": "machine-with-a-very-long-name",
                   "id": format!("12345678-1234-4234-9234-123456789ab{tail}"), "name": "A".repeat(64)})
        })
        .collect();
    f.bridge.update_peers(&Value::Array(peers)).unwrap();
    let names = spawned.lock().unwrap().clone();
    assert_eq!(
        names[0],
        format!("switchboard/machine-with-a-v/{}-12345678", "A".repeat(26))
    );
    assert_eq!(
        names[1],
        format!("switchboard/machine-with-a-v/{}-12345678-2", "A".repeat(24))
    );
    assert!(names.iter().all(|n| n.len() == 64));
}

#[test]
fn partial_ipc_is_unknown_not_failed_or_replayed() {
    let mut f = Fixture::new();
    let (route, _, log) = f.connected(&uuid4());
    log.lock().unwrap().fail = Some(super::helper_process::write_unknown());
    let event = json!({"type": "message", "id": uuid4(), "to": format!("local/{}", f.sid), "text": "hello",
                       "mode": "prompting", "sender_name": "peer", "from": route});
    f.bridge.message(&event).unwrap();
    f.bridge.drain_completions().unwrap();
    assert_eq!(f.calls().last().unwrap().1["state"], "unknown");
    assert_eq!(log.lock().unwrap().closed, 1);
    let id = event["id"].as_str().unwrap();
    assert!(f.bridge.deliveries.contains_key(id));
    let (helper, _) = f.remote_helper(&route[7..]);
    f.bridge.helpers.insert(route.clone(), helper);
    f.bridge.message(&event).unwrap();
    assert_eq!(ops(&log).len(), 1);
}

#[test]
fn stale_poll_cannot_recreate_withdrawn_helpers() {
    let mut f = Fixture::new();
    f.bridge.online = true;
    f.bridge.records = IndexMap::from([(f.sid.clone(), f.record.clone())]);
    let generation = f.bridge.generation;
    f.bridge.withdraw().unwrap();
    let spawned = f.count_spawns();
    let peers = json!([{"machine": "remote", "id": uuid4(), "name": "x"}]);
    let (result, _) = f.poll(
        generation,
        json!({"machine": "local", "peers": peers, "events": []}),
    );
    result.unwrap();
    assert!(spawned.lock().unwrap().is_empty());
}

#[test]
fn remote_status_routes_only_to_correlated_helper() {
    let mut f = Fixture::new();
    let (helper, log) = f.remote_helper(&uuid4());
    let (route, identity) = (helper.route.clone(), helper.identity.clone());
    f.bridge.helpers.insert(route.clone(), helper);
    let identifier = uuid4();
    sent(&mut f, &identifier, &identity);
    f.bridge
        .message(&json!({"type": "status", "id": identifier, "state": "held", "detail": "held"}))
        .unwrap();
    assert_eq!(
        ops(&log),
        vec![obj! {"op" => "receipt", "id" => identifier, "state" => "held", "detail" => "held"}]
    );
}

#[test]
fn existing_readable_config_root_accepted_without_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    std::fs::set_permissions(&f.config_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        load_config(&s(&f.path)).unwrap().config_dir,
        s(&f.config_dir)
    );
    std::fs::set_permissions(&f.config_dir, std::fs::Permissions::from_mode(0o775)).unwrap();
    assert!(load_config(&s(&f.path))
        .unwrap_err()
        .message
        .contains("shared-writable"));
    std::fs::set_permissions(&f.config_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let alias = f.root.join("alias");
    std::os::unix::fs::symlink(&f.config_dir, &alias).unwrap();
    f.config["config_dir"] = json!(s(&alias));
    f.write_config();
    assert!(load_config(&s(&f.path))
        .unwrap_err()
        .message
        .contains("symlinked"));
}

#[test]
fn legacy_viewer_port_is_ignored() {
    // Configs written for the removed loopback viewer keep loading, whatever
    // the old key holds.
    let mut f = Fixture::new();
    for port in [json!(0), json!(8765), json!(-1), json!("x")] {
        f.config["viewer_port"] = port;
        f.write_config();
        assert!(load_config(&s(&f.path)).unwrap().enabled);
    }
}

#[test]
fn plain_http_and_unsafe_config_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    f.config["hub_url"] = json!("http://localhost:8743");
    f.write_config();
    assert_eq!(load_config(&s(&f.path)).unwrap_err().kind, Kind::Value);
    f.config["hub_url"] = json!("https://localhost:8743");
    f.write_config();
    std::fs::set_permissions(&f.path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(load_config(&s(&f.path)).unwrap_err().kind, Kind::Value);
}

#[test]
fn runtime_status_identity_transitions_and_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    f.bridge.start_observability().unwrap();
    let path = f.state.join("runtime.json");
    let read = |path: &PathBuf| -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    let before = read(&path);
    assert_eq!(before["pid"], json!(std::process::id()));
    assert_eq!(
        before["procStart"],
        json!(process_start(std::process::id() as i64).unwrap())
    );
    assert_eq!(
        (before["connected"].clone(), before["machine"].clone()),
        (json!(false), Value::Null)
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let keys: Vec<&str> = before
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "pid",
            "procStart",
            "platform",
            "machine",
            "connected",
            "discovery_mode",
            "discovery_detail",
            "published",
            "remote",
            "updated_at"
        ]
    );
    f.discover_returns(&[]);
    f.register();
    let connected = read(&path);
    assert_eq!(
        (connected["connected"].clone(), connected["machine"].clone()),
        (json!(true), json!("local"))
    );
    let generation = f.bridge.generation;
    f.bridge.handle(Event::Offline(generation, None)).unwrap();
    assert_eq!(read(&path)["connected"], json!(false));
    let leftovers = std::fs::read_dir(&f.state)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".runtime-")
        })
        .count();
    assert_eq!(leftovers, 0);
    f.bridge.close_observability();
    assert!(!path.exists());
}

#[test]
fn runtime_lock_excludes_second_supervisor() {
    let mut f = Fixture::new();
    f.bridge.start_observability().unwrap();
    let mut other = Fixture::make(&f.path, &f.calls, &f.hub, &f.events);
    assert!(other.start_observability().is_err());
    other.close_observability();
    assert!(f.state.join("runtime.json").exists());
}

#[test]
fn runtime_file_removed_while_singleton_lock_still_held() {
    let mut f = Fixture::new();
    f.bridge.start_observability().unwrap();
    f.bridge.remove_observability();
    assert!(!f.state.join("runtime.json").exists());
    let mut other = Fixture::make(&f.path, &f.calls, &f.hub, &f.events);
    assert!(other.start_observability().is_err());
    other.close_observability();
    f.bridge.close_observability();
    other.start_observability().unwrap();
    other.close_observability();
}

/// A server that refuses this machine's key is recorded in the runtime file
/// (`server_refused`, for doctor) while the bridge is offline because of it,
/// and cleared once it is offline for another reason or connects again.
#[test]
fn runtime_records_the_servers_refusal_of_this_machine() {
    let mut f = Fixture::new();
    f.bridge.start_observability().unwrap();
    let path = f.state.join("runtime.json");
    let read = |path: &PathBuf| -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    };
    f.discover_returns(&[]);
    assert_eq!(read(&path).get("server_refused"), None);
    f.hub.lock().unwrap().fail = Some(HubError::Http {
        status: 403,
        code: Some("not_enrolled".into()),
    });
    f.register();
    let runtime = read(&path);
    assert_eq!(runtime["connected"], json!(false));
    assert_eq!(runtime["server_refused"], json!("not_enrolled"));
    // A failure that says nothing about this machine replaces it.
    f.hub.lock().unwrap().fail = Some(HubError::Os("lost".into()));
    f.retry();
    assert_eq!(read(&path).get("server_refused"), None);
    // The poller's refusal is recorded the same way.
    let generation = f.bridge.generation;
    f.bridge
        .handle(Event::Offline(generation, Some("not_enrolled".into())))
        .unwrap();
    assert_eq!(read(&path)["server_refused"], json!("not_enrolled"));
    f.hub.lock().unwrap().fail = None;
    f.retry();
    let runtime = read(&path);
    assert_eq!(runtime["connected"], json!(true));
    assert_eq!(runtime.get("server_refused"), None);
    assert_eq!(
        super::machine_refusal(&HubError::Http {
            status: 503,
            code: Some("enrollment_unavailable".into()),
        }),
        None
    );
    f.bridge.close_observability();
}

#[test]
fn failed_registration_never_reports_connected_and_cleans_state() {
    let mut f = Fixture::new();
    f.hub.lock().unwrap().fail = Some(HubError::Os("fixture registration failed".into()));
    let state = f.state.clone();
    let stop = f.bridge.stop_handle();
    let log = f.events.clone();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let seen = observed.clone();
    f.bridge.emit = Arc::new(move |value: &Map<String, Value>| {
        if let Ok(raw) = std::fs::read_to_string(state.join("runtime.json")) {
            seen.lock()
                .unwrap()
                .push(serde_json::from_str::<Value>(&raw).unwrap());
        }
        log.lock().unwrap().push(value.clone());
        if value["event"] == "request_failed" {
            stop();
        }
        true
    });
    f.bridge.clock = None;
    f.bridge.run().unwrap();
    assert!(!observed.lock().unwrap().is_empty());
    assert!(observed
        .lock()
        .unwrap()
        .iter()
        .all(|v| v["connected"] == json!(false)));
    assert!(!f.state.join("runtime.json").exists());
    assert!(f.events().iter().any(|e| e["event"] == "request_failed"));
}

#[test]
fn thirty_unchanged_scans_register_once_and_scan_every_time() {
    let mut f = Fixture::new();
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    for tick in (0..62).step_by(2) {
        *f.clock.lock().unwrap() = tick as f64;
        f.register();
    }
    assert_eq!(*f.discovered.lock().unwrap(), 31);
    assert_eq!(f.calls().len(), 1);
    assert_eq!(f.bridge.advertised.as_ref().unwrap()[0]["id"], json!(f.sid));
}

#[test]
fn advertised_changes_and_order_are_canonical() {
    let mut f = Fixture::new();
    let other_id = uuid4();
    let mut other = f.record.clone();
    other.insert("sessionId".into(), json!(other_id));
    other.insert("name".into(), json!("second"));
    f.config["sessions"]
        .as_array_mut()
        .unwrap()
        .push(json!(other_id));
    f.write_config();
    let record = f.record.clone();
    f.discover_returns(&[&record, &other]);
    f.register();
    let generation = f.bridge.generation;
    f.config["sessions"].as_array_mut().unwrap().reverse();
    f.write_config();
    f.discover_returns(&[&other, &record]);
    f.register();
    assert_eq!(f.calls().len(), 1);
    assert_eq!(f.bridge.generation, generation);
    let mut record = record;
    record.insert("status".into(), json!("busy"));
    f.discover_returns(&[&record, &other]);
    f.register();
    record.insert("name".into(), json!("renamed"));
    f.discover_returns(&[&record, &other]);
    f.register();
    f.discover_returns(&[&other]);
    f.register();
    f.discover_returns(&[]);
    f.register();
    f.register();
    let calls = f.calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls.last().unwrap().1,
        json!({"sessions": [], "capabilities": ["ack", "ping"]})
    );
    for (_, body) in calls {
        let ids: Vec<String> = body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }
}

#[test]
fn legacy_periodic_refresh_and_modern_recovery() {
    let mut f = Fixture::new();
    f.hub.lock().unwrap().registration = None;
    f.discover_returns(&[]);
    for tick in [0.0, 2.0, 18.0, 20.0, 22.0, 40.0] {
        *f.clock.lock().unwrap() = tick;
        f.register();
    }
    assert_eq!(f.calls().len(), 3);
    let generation = f.bridge.generation;
    f.bridge.handle(Event::Offline(generation, None)).unwrap();
    let registration = uuid4();
    f.hub.lock().unwrap().registration = Some(registration.clone());
    f.retry();
    assert_eq!(f.calls().len(), 4);
    assert_eq!(f.bridge.registration, Some(registration));
}

#[test]
fn lost_registration_discards_events_and_reregisters() {
    let mut f = Fixture::new();
    f.discover_returns(&[]);
    for lost in [Value::Null, json!(uuid4())] {
        f.register();
        let generation = f.bridge.generation;
        let before = f.calls().len();
        let (result, acks) = f.poll(generation, json!({"machine": "local", "registration": lost, "peers": [],
                                                       "events": [{"type": "ping", "id": uuid4(), "to": "local/x", "ack_id": "c".repeat(32)}]}));
        result.unwrap();
        assert!(acks.is_empty());
        assert_eq!(f.calls().len(), before); // The ping was never answered.
        assert!(!f.bridge.online);
        assert!(f.bridge.shared.poll_state.lock().unwrap().is_none());
        f.register();
        assert!(f.bridge.online);
        assert!(f.bridge.generation > generation);
    }
    assert_eq!(f.calls().len(), 3);
}

#[test]
fn failed_registration_does_not_commit_advertised_change() {
    let mut f = Fixture::new();
    let mut record = f.record.clone();
    f.discover_returns(&[&record]);
    f.register();
    record.insert("name".into(), json!("changed"));
    f.discover_returns(&[&record]);
    f.hub.lock().unwrap().fail = Some(HubError::Os("fixture failed".into()));
    f.register();
    assert!(!f.bridge.online);
    f.hub.lock().unwrap().fail = None;
    assert!(f.bridge.advertised.is_none());
    f.retry();
    assert_eq!(
        f.bridge.advertised.as_ref().unwrap()[0]["name"],
        json!("changed")
    );
    assert_eq!(f.calls().len(), 3);
}

#[test]
fn revision_cache_recovers_helpers_and_resets_on_generation() {
    let mut f = Fixture::new();
    let remote_id = uuid4();
    let peer = json!({"machine": "remote", "id": remote_id, "name": "remote"});
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    f.register();
    let spawned = f.count_spawns();
    let generation = f.bridge.generation;
    let registration = f.hub.lock().unwrap().registration.clone().unwrap();
    let reply = json!({"machine": "local", "registration": registration, "peers": [peer], "peers_revision": "a".repeat(64), "events": []});
    f.poll(generation, reply.clone()).0.unwrap();
    assert_eq!(spawned.lock().unwrap().len(), 1);
    // A helper lost while the roster is unchanged comes back from the cache.
    let route = format!("remote/{remote_id}");
    f.bridge.close_helper(&route);
    f.register();
    assert_eq!(spawned.lock().unwrap().len(), 2);
    f.poll(generation, json!({"machine": "local", "registration": registration, "peers_revision": "a".repeat(64), "events": []})).0.unwrap();
    assert_eq!(
        f.bridge
            .shared
            .poll_state
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .2,
        Some("a".repeat(64))
    );
    let (result, _) = f.poll(generation, json!({"machine": "local", "registration": registration, "peers_revision": "b".repeat(64), "events": []}));
    assert!(result.unwrap_err().message.contains("missing peer roster"));
    f.bridge.withdraw().unwrap();
    assert!(f.bridge.peers.is_none() && f.bridge.peers_revision.is_none());
    f.register();
    let count = spawned.lock().unwrap().len();
    f.poll(generation, reply).0.unwrap();
    assert_eq!(spawned.lock().unwrap().len(), count);
    assert_eq!(
        f.bridge
            .shared
            .poll_state
            .lock()
            .unwrap()
            .clone()
            .unwrap()
            .2,
        None
    );
}

#[test]
fn stale_helper_events_cannot_act_on_replacement_or_new_generation() {
    let mut f = Fixture::new();
    let (route, identity, log) = f.connected(&uuid4());
    let message_id = uuid4();
    f.bridge
        .deliveries
        .insert(message_id.clone(), route.clone());
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    let events = [
        json!({"event": "outbound", "source_session": f.sid, "recipient": route, "native_id": message_id,
               "text": "test", "mode": "prompting", "revision": 0}),
        json!({"event": "status", "id": message_id, "state": "delivered"}),
        json!({"event": "rejected", "detail": "test"}),
    ];
    let pid = f.bridge.helpers[&route].pid;
    for old in [
        (route.clone(), "old-instance".to_string(), identity.2),
        (
            route.clone(),
            identity.1.clone(),
            identity.2.wrapping_sub(1),
        ),
    ] {
        for event in &events {
            f.bridge
                .handle(Event::Helper(
                    old.clone(),
                    event.as_object().unwrap().clone(),
                ))
                .unwrap();
        }
        f.bridge
            .handle(Event::HelperExit(old.clone(), pid))
            .unwrap();
    }
    assert_eq!(*f.discovered.lock().unwrap(), 0);
    assert_eq!(log.lock().unwrap().closed, 0);
    assert!(f.calls().is_empty() && f.events().is_empty());
    f.bridge
        .handle(Event::Helper(
            identity.clone(),
            events[0].as_object().unwrap().clone(),
        ))
        .unwrap();
    assert_eq!(*f.discovered.lock().unwrap(), 1);
    assert_eq!(f.calls().last().unwrap().0, "/v1/send");
    f.bridge
        .handle(Event::Helper(
            identity.clone(),
            events[1].as_object().unwrap().clone(),
        ))
        .unwrap();
    assert_eq!(f.calls().last().unwrap().0, "/v1/status");
    f.bridge
        .handle(Event::Helper(
            identity.clone(),
            events[2].as_object().unwrap().clone(),
        ))
        .unwrap();
    assert_eq!(f.events().last().unwrap()["event"], "native_rejected");
    f.bridge.next_scan = 30.0;
    f.bridge.handle(Event::HelperExit(identity, pid)).unwrap();
    assert_eq!(log.lock().unwrap().closed, 1);
    assert!(!f.bridge.helpers.contains_key(&route));
    assert_eq!(f.bridge.next_scan, 0.0);
}

#[test]
fn explicit_message_errors_are_local_transport_and_enrollment_fail_closed() {
    let mut f = Fixture::new();
    for (status, code) in [
        (404, "target_offline"),
        (503, "queue_full"),
        (403, "source_forbidden"),
        (403, "status_forbidden"),
        (400, "invalid_message"),
        (409, "id_conflict"),
        (429, "rate_limited"),
    ] {
        for operation in ["send", "status"] {
            f.bridge.online = true;
            let (helper, log) = f.remote_helper(&uuid4());
            let route = helper.route.clone();
            f.bridge.helpers.insert(route.clone(), helper);
            let identifier = uuid4();
            f.hub.lock().unwrap().fail = Some(HubError::Http {
                status,
                code: Some(code.into()),
            });
            let kind: &'static str = if operation == "send" {
                "send"
            } else {
                "status"
            };
            let meta = Meta {
                id: Some(identifier.clone()),
                route: Some(route.clone()),
                ..Meta::default()
            };
            f.bridge
                .submit_request(
                    kind,
                    &format!("/v1/{operation}"),
                    obj! {"id" => identifier},
                    meta,
                )
                .unwrap();
            f.bridge.drain_completions().unwrap();
            assert!(f.bridge.online, "{code}");
            assert_eq!(log.lock().unwrap().closed, 0);
            f.bridge.helpers.clear();
        }
    }
    let failures = [
        HubError::Http {
            status: 403,
            code: Some("not_enrolled".into()),
        },
        HubError::Http {
            status: 503,
            code: Some("enrollment_unavailable".into()),
        },
        HubError::Http {
            status: 502,
            code: None,
        },
        HubError::Http {
            status: 503,
            code: Some("unavailable".into()),
        },
        HubError::Os("lost".into()),
    ];
    for error in failures {
        f.bridge.online = true;
        let (helper, log) = f.remote_helper(&uuid4());
        let route = helper.route.clone();
        f.bridge.helpers.insert(route.clone(), helper);
        let identifier = uuid4();
        f.hub.lock().unwrap().fail = Some(error.clone());
        let meta = Meta {
            id: Some(identifier.clone()),
            route: Some(route),
            ..Meta::default()
        };
        f.bridge
            .submit_request("send", "/v1/send", obj! {"id" => identifier.clone()}, meta)
            .unwrap();
        f.bridge.drain_completions().unwrap();
        assert!(!f.bridge.online);
        assert_eq!(log.lock().unwrap().closed, 1);
        let states: Vec<Value> = f
            .events()
            .iter()
            .filter(|e| e.get("id") == Some(&json!(identifier)))
            .map(|e| e["state"].clone())
            .collect();
        let expected = if explicit_rejection(&error) {
            "failed"
        } else {
            "unknown"
        };
        assert_eq!(states.last().unwrap(), &json!(expected));
    }
}

#[test]
fn enrollment_unavailable_after_possible_commit_is_unknown_and_global() {
    let mut f = Fixture::new();
    f.bridge.online = true;
    let identifier = uuid4();
    f.hub.lock().unwrap().fail = Some(HubError::Http {
        status: 503,
        code: Some("enrollment_unavailable".into()),
    });
    let meta = Meta {
        id: Some(identifier.clone()),
        route: Some("remote/test".into()),
        ..Meta::default()
    };
    f.bridge
        .submit_request("send", "/v1/send", obj! {"id" => identifier.clone()}, meta)
        .unwrap();
    f.bridge.drain_completions().unwrap();
    assert!(!f.bridge.online);
    let last = f
        .events()
        .into_iter()
        .rfind(|e| e.get("id") == Some(&json!(identifier)))
        .unwrap();
    assert_eq!(last["state"], "unknown");
    assert!(f.bridge.pending.is_empty());
}

#[test]
fn acknowledged_events_are_deduplicated_across_restart() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let remote_id = uuid4();
    let (route, _, log) = f.connected(&remote_id);
    let message = json!({"type": "message", "id": uuid4(), "to": format!("local/{}", f.sid), "text": "hello",
                         "mode": "prompting", "sender_name": "peer", "ack_id": "a".repeat(32), "from": route});
    let status =
        json!({"type": "status", "id": uuid4(), "state": "unknown", "ack_id": "b".repeat(32)});
    let response = json!({"machine": "local", "peers": [{"machine": "remote", "id": remote_id, "name": "fixture", "status": "idle"}],
                          "events": [message, status]});
    let generation = f.bridge.generation;
    let (result, acks) = f.poll(generation, response.clone());
    result.unwrap();
    assert_eq!(acks, ["a".repeat(32), "b".repeat(32)]);
    assert_eq!(
        ops(&log)
            .iter()
            .map(|o| o["op"].clone())
            .collect::<Vec<_>>(),
        [json!("deliver")]
    );
    let (_, acks) = f.poll(generation, response);
    assert_eq!(acks, ["a".repeat(32), "b".repeat(32)]);
    assert_eq!(ops(&log).len(), 1);
    let mut restarted = Fixture::make(&f.path, &f.calls, &f.hub, &f.events);
    restarted.machine = Some("local".into());
    restarted.online = true;
    restarted.records = IndexMap::from([(f.sid.clone(), f.record.clone())]);
    let (replacement, replaced) = f.remote_helper(&remote_id);
    restarted
        .helpers
        .insert(replacement.route.clone(), replacement);
    let calls = f.calls().len();
    restarted.message(&message).unwrap();
    assert!(ops(&replaced).is_empty());
    assert_eq!(f.calls().len(), calls);
    let mode = std::fs::metadata(f.state.join("delivered-ids.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn events_rejected_after_lost_registration_are_not_acknowledged() {
    let mut f = Fixture::new();
    f.connected(&uuid4());
    f.bridge.registration = Some(uuid4());
    let generation = f.bridge.generation;
    let (result, acks) = f.poll(generation, json!({"machine": "local", "registration": uuid4(),
                                                   "events": [{"type": "status", "id": uuid4(), "state": "held", "ack_id": "c".repeat(32)}]}));
    result.unwrap();
    assert!(acks.is_empty());
}

#[test]
fn redelivered_status_does_not_repeat_native_receipt() {
    let mut f = Fixture::new();
    let (helper, log) = f.remote_helper(&uuid4());
    let (route, identity) = (helper.route.clone(), helper.identity.clone());
    f.bridge.helpers.insert(route.clone(), helper);
    let identifier = uuid4();
    sent(&mut f, &identifier, &identity);
    for _ in 0..2 {
        f.bridge
            .message(&json!({"type": "status", "id": identifier, "state": "held", "detail": "native receipt: held", "ack_id": "e".repeat(32)}))
            .unwrap();
    }
    assert_eq!(ops(&log).len(), 1);
}

#[test]
fn ping_is_answered_with_local_liveness_only() {
    let mut f = Fixture::new();
    f.connected(&uuid4());
    for (target, live) in [
        (format!("local/{}", f.sid), true),
        (format!("local/{}", uuid4()), false),
        (format!("other/{}", f.sid), false),
    ] {
        let identifier = uuid4();
        f.bridge
            .message(&json!({"type": "ping", "id": identifier, "to": target}))
            .unwrap();
        f.bridge.drain_completions().unwrap();
        assert_eq!(
            f.calls().last().unwrap().clone(),
            (
                "/v1/pong".to_string(),
                json!({"id": identifier, "live": live})
            )
        );
    }
    assert!(f.bridge.deliveries.is_empty());
}

fn send_uncertain(
    f: &mut Fixture,
    route: &str,
    identity: &Identity,
    error: HubError,
) -> (String, Map<String, Value>) {
    let identifier = uuid4();
    let body = obj! {"id" => identifier.clone(), "from" => format!("local/{}", f.sid), "to" => route,
    "text" => "hello there", "mode" => "bypass"};
    f.hub.lock().unwrap().fail = Some(error);
    let meta = Meta {
        id: Some(identifier.clone()),
        route: Some(route.to_string()),
        source_session: Some(f.sid.clone()),
        helper_identity: Some(identity.clone()),
        body: Some(body.clone()),
        ..Meta::default()
    };
    f.bridge
        .submit_request("send", "/v1/send", body.clone(), meta)
        .unwrap();
    f.bridge.drain_completions().unwrap();
    f.hub.lock().unwrap().fail = None;
    (identifier, body)
}

#[test]
fn uncertain_send_is_resent_identically_after_reconnect() {
    let mut f = Fixture::new();
    let remote_id = uuid4();
    let (route, identity, _) = f.connected(&remote_id);
    let (identifier, body) = send_uncertain(&mut f, &route, &identity, HubError::Os("lost".into()));
    assert!(!f.bridge.online);
    assert!(f.bridge.resend.contains_key(&identifier));
    assert!(!f
        .events()
        .iter()
        .any(|e| e.get("id") == Some(&json!(identifier))));
    let (_, _, replacement) = f.connected(&remote_id);
    f.bridge.flush_resends().unwrap();
    assert_eq!(
        ops(&replacement).last().unwrap().clone(),
        obj! {"op" => "track", "id" => identifier, "source_session" => f.sid.clone(), "text" => "hello there", "mode" => "bypass"}
    );
    assert_eq!(
        f.calls().last().unwrap().clone(),
        ("/v1/send".to_string(), Value::Object(body))
    );
    assert!(f.bridge.resend.is_empty());
}

#[test]
fn resend_window_expiry_tells_the_sender() {
    let mut f = Fixture::new();
    let remote_id = uuid4();
    let (route, identity, _) = f.connected(&remote_id);
    let (identifier, _) = send_uncertain(&mut f, &route, &identity, HubError::Os("lost".into()));
    let entry = f.bridge.resend.get_mut(&identifier).unwrap();
    entry.resend_deadline = Some(entry.resend_deadline.unwrap() - (RESEND_WINDOW + 1.0));
    let (_, _, replacement) = f.connected(&remote_id);
    let calls = f.calls().len();
    f.bridge.flush_resends().unwrap();
    let seen = ops(&replacement);
    assert_eq!(
        seen.iter().map(|o| o["op"].clone()).collect::<Vec<_>>(),
        [json!("track"), json!("receipt")]
    );
    assert_eq!(seen[1]["state"], "unknown");
    assert_eq!(f.calls().len(), calls);
    assert!(f.bridge.resend.is_empty());
}

#[test]
fn resend_reports_recorded_outcome_and_drops_departed_sender() {
    let mut f = Fixture::new();
    let remote_id = uuid4();
    let (route, identity, _) = f.connected(&remote_id);
    let (identifier, _) = send_uncertain(&mut f, &route, &identity, HubError::Os("lost".into()));
    let (_, _, replacement) = f.connected(&remote_id);
    f.hub.lock().unwrap().reply =
        Some(obj! {"ok" => true, "id" => identifier, "state" => "unknown"});
    f.bridge.flush_resends().unwrap();
    f.bridge.drain_completions().unwrap();
    f.hub.lock().unwrap().reply = None;
    assert_eq!(ops(&replacement).last().unwrap()["state"], "unknown");
    let (route, identity, _) = f.connected(&remote_id);
    let (departed, _) = send_uncertain(&mut f, &route, &identity, HubError::Os("lost".into()));
    f.bridge.config.sessions.clear();
    f.bridge.flush_resends().unwrap();
    assert!(!f.bridge.resend.contains_key(&departed));
}

#[test]
fn explicit_send_failure_gives_plain_reason() {
    let mut f = Fixture::new();
    let (route, identity, log) = f.connected(&uuid4());
    send_uncertain(
        &mut f,
        &route,
        &identity,
        HubError::Http {
            status: 404,
            code: Some("target_offline".into()),
        },
    );
    assert!(f.bridge.online);
    assert_eq!(
        ops(&log).last().unwrap()["detail"],
        "the recipient chat is offline or no longer joined"
    );
    assert!(f.bridge.resend.is_empty());
}

#[test]
fn inactive_chats_expire_and_activity_is_recorded() {
    let mut f = Fixture::new();
    let now = crate::fmtutil::time_now().round() as i64;
    let other = uuid4();
    f.config["sessions"] = json!([f.sid, other]);
    f.config["joined_at"] = json!({f.sid.clone(): now - 8 * 86400, other.clone(): now});
    f.config["names"] = json!({f.sid.clone(): "old-name"});
    f.write_config();
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    f.register();
    let expired: Vec<Map<String, Value>> = f
        .events()
        .into_iter()
        .filter(|e| e["event"] == "session_expired")
        .collect();
    assert_eq!(
        expired,
        vec![
            obj! {"event" => "session_expired", "session" => f.sid.clone(), "detail" => "no activity for 7 days"}
        ]
    );
    let config: Value = serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
    assert_eq!(config["sessions"], json!([other]));
    assert!(config.get("names").is_none());
    let membership: Value =
        serde_json::from_str(&std::fs::read_to_string(f.root.join("membership.json")).unwrap())
            .unwrap();
    assert_eq!(
        membership,
        json!({"version": 1, "sessions": {other.clone(): {"name": "Claude"}}})
    );
    assert_eq!(f.bridge.config.sessions, vec![other]);
}

// Membership as the bridge applies it: expiry, renewal, chosen names, and syncing.

const DAY: f64 = 86400.0;

/// The expiry fixture: two joined chats with no join record, a patched wall clock.
fn expiry_fixture() -> (Fixture, String, Arc<Mutex<f64>>) {
    let mut f = Fixture::new();
    let other = uuid4();
    f.config["sessions"] = json!([f.sid, other]);
    f.write_config();
    let wall = Arc::new(Mutex::new(0.0));
    f.bridge.wall = Some(wall.clone());
    let mut second = f.record.clone();
    second.insert("sessionId".into(), json!(other));
    second.insert("name".into(), json!(format!("Chat {}", &other[..4])));
    let first = f.record.clone();
    f.discover_returns(&[&first, &second]);
    (f, other, wall)
}

fn expiry_register(f: &mut Fixture, wall: &Arc<Mutex<f64>>, now: f64) {
    *wall.lock().unwrap() = now;
    f.register();
}

fn config_sessions(f: &Fixture) -> Value {
    serde_json::from_str::<Value>(&std::fs::read_to_string(&f.path).unwrap()).unwrap()["sessions"]
        .clone()
}

fn set_status(f: &Fixture, sid: &str, status: &str) {
    f.found
        .lock()
        .unwrap()
        .get_mut(sid)
        .unwrap()
        .insert("status".into(), json!(status));
}

#[test]
fn legacy_chats_start_clock_and_idle_ones_expire() {
    let (mut f, other, wall) = expiry_fixture();
    let start = 1000.0 * DAY;
    expiry_register(&mut f, &wall, start);
    let activity = membership::load_activity(&s(&f.state));
    assert_eq!(
        activity.keys().cloned().collect::<HashSet<_>>(),
        HashSet::from([f.sid.clone(), other.clone()])
    );
    let sid = f.sid.clone();
    set_status(&f, &sid, "busy");
    expiry_register(&mut f, &wall, start + 3.0 * DAY);
    set_status(&f, &sid, "idle");
    expiry_register(&mut f, &wall, start + 7.0 * DAY + 1.0);
    assert_eq!(config_sessions(&f), json!([sid]));
    let last = f.events().last().unwrap().clone();
    assert_eq!(
        (last["event"].clone(), last["session"].clone()),
        (json!("session_expired"), json!(other))
    );
    assert_eq!(
        membership::load_activity(&s(&f.state))
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        std::slice::from_ref(&sid)
    );
    let body = f.calls().last().unwrap().1.clone();
    assert_eq!(
        body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].clone())
            .collect::<Vec<_>>(),
        [json!(sid)]
    );
    expiry_register(&mut f, &wall, start + 10.0 * DAY + 1.0);
    assert_eq!(config_sessions(&f), json!([]));
}

#[test]
fn resume_does_not_renew_but_messages_do() {
    let (mut f, _, wall) = expiry_fixture();
    let start = 1000.0 * DAY;
    expiry_register(&mut f, &wall, start);
    expiry_register(&mut f, &wall, start + 6.0 * DAY);
    let sid = f.sid.clone();
    f.bridge.note_activity(&sid, None, true);
    expiry_register(&mut f, &wall, start + 8.0 * DAY);
    assert_eq!(config_sessions(&f), json!([sid]));
}

#[test]
fn rejoin_under_lock_restarts_clock() {
    let (mut f, other, wall) = expiry_fixture();
    let start = 1000.0 * DAY;
    expiry_register(&mut f, &wall, start);
    let mut config: Value =
        serde_json::from_str(&std::fs::read_to_string(&f.path).unwrap()).unwrap();
    config["joined_at"] = json!({other.clone(): start + 7.0 * DAY});
    write_private(&f.path, &config);
    expiry_register(&mut f, &wall, start + 7.0 * DAY + 5.0);
    assert_eq!(config_sessions(&f), json!([other]));
}

#[test]
fn chosen_name_is_published_and_runtime_confirms() {
    let (mut f, other, _) = expiry_fixture();
    f.bridge.wall = None;
    f.config["names"] = json!({f.sid.clone(): "release-planner"});
    f.write_config();
    f.bridge.start_observability().unwrap();
    f.bridge.peers = Some(vec![
        json!({"machine": "remote", "id": uuid4()}),
        json!({"machine": "remote", "id": uuid4()}),
        json!({"machine": "other", "id": uuid4()}),
        json!({"machine": "local", "id": f.sid}),
    ]);
    f.count_spawns();
    f.register();
    let body = f.calls().last().unwrap().1.clone();
    let names: IndexMap<String, Value> = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap().to_string(), s["name"].clone()))
        .collect();
    assert_eq!(names[&f.sid], json!("release-planner"));
    assert_eq!(names[&other], json!(format!("Chat {}", &other[..4])));
    let runtime: Value =
        serde_json::from_str(&std::fs::read_to_string(f.state.join("runtime.json")).unwrap())
            .unwrap();
    let published: IndexMap<String, Value> = runtime["published"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap().to_string(), p["name"].clone()))
        .collect();
    assert_eq!(published, names);
    assert_eq!(runtime["remote"], json!({"sessions": 3, "machines": 2}));
}

#[test]
fn offline_runtime_publishes_nothing() {
    let mut f = Fixture::new();
    f.bridge.start_observability().unwrap();
    let runtime: Value =
        serde_json::from_str(&std::fs::read_to_string(f.state.join("runtime.json")).unwrap())
            .unwrap();
    assert_eq!(runtime["published"], json!([]));
}

#[test]
fn bridge_syncs_membership_only_when_its_view_changes() {
    use std::os::unix::fs::MetadataExt;
    let mut f = Fixture::new();
    let sid = f.sid.clone();
    let mut record = f.record.clone();
    record.insert("name".into(), json!("Chat one"));
    f.bridge.records = IndexMap::from([(sid.clone(), record.clone())]);
    let names = IndexMap::new();
    f.bridge.sync_membership(std::slice::from_ref(&sid), &names);
    let path = f.root.join("membership.json");
    let inode = std::fs::metadata(&path).unwrap().ino();
    // An unchanged view does not even reread the config: the file stays put
    // even after it is replaced underneath.
    std::fs::remove_file(&path).unwrap();
    f.bridge.sync_membership(std::slice::from_ref(&sid), &names);
    assert!(!path.exists());
    record.insert("name".into(), json!("Renamed"));
    f.bridge.records = IndexMap::from([(sid.clone(), record)]);
    f.bridge.sync_membership(std::slice::from_ref(&sid), &names);
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["sessions"], json!({sid: {"name": "Renamed"}}));
    let _ = inode;
    assert!(f.events().is_empty());
}

#[test]
fn daemon_log_lines_start_with_utc_timestamp() {
    let line = format_log_line(&obj! {"event" => "ready", "machine" => "laptop"});
    let (stamp, rest) = line.split_at(29);
    assert!(
        stamp.starts_with("{\"at\": \"") && stamp.ends_with("Z\""),
        "{line}"
    );
    let digits: String = stamp[8..28].chars().collect();
    assert!(
        digits.chars().enumerate().all(|(i, c)| match i {
            4 | 7 => c == '-',
            10 => c == 'T',
            19 => c == 'Z',
            13 | 16 => c == ':',
            _ => c.is_ascii_digit(),
        }),
        "{digits}"
    );
    assert_eq!(rest, ", \"event\": \"ready\", \"machine\": \"laptop\"}\n");
}

#[test]
fn a_replacement_helper_adopts_a_message_before_its_outcome() {
    let mut f = Fixture::new();
    let remote = uuid4();
    let (route, old, _) = f.connected(&remote);
    let identifier = uuid4();
    sent(&mut f, &identifier, &old);
    // The connection was lost and the route's helper replaced; the server
    // then reports that delivery could not be confirmed.
    f.bridge.close_helper(&route);
    let (helper, log) = f.remote_helper(&remote);
    let new = helper.identity.clone();
    f.bridge.helpers.insert(route.clone(), helper);
    let detail = "the server restarted before delivery was confirmed";
    f.bridge
        .message(&json!({"type": "status", "id": identifier, "state": "unknown", "detail": detail}))
        .unwrap();
    assert_eq!(
        ops(&log),
        vec![
            obj! {"op" => "track", "id" => identifier.clone(), "source_session" => f.sid.clone(),
            "text" => "hello there", "mode" => "bypass"},
            obj! {"op" => "receipt", "id" => identifier.clone(), "state" => "unknown", "detail" => detail},
        ]
    );
    assert_eq!(f.bridge.outbound[&identifier].helper, new);
    // That helper sent the notice; another replacement is not told again.
    f.bridge.close_helper(&route);
    let (helper, log) = f.remote_helper(&remote);
    f.bridge.helpers.insert(route.clone(), helper);
    f.bridge
        .message(&json!({"type": "status", "id": identifier, "state": "failed", "detail": "late"}))
        .unwrap();
    assert!(ops(&log).is_empty());
}

#[test]
fn a_failed_send_reaches_the_sender_through_a_replacement_helper() {
    let mut f = Fixture::new();
    let remote = uuid4();
    let (route, old, old_log) = f.connected(&remote);
    let identifier = uuid4();
    sent(&mut f, &identifier, &old);
    f.bridge.close_helper(&route);
    let (helper, log) = f.remote_helper(&remote);
    f.bridge.helpers.insert(route.clone(), helper);
    f.bridge
        .outbound_failure(&identifier, &route, "unknown", "lost", Some(&old));
    assert!(ops(&old_log).is_empty());
    let sent_ops: Vec<String> = ops(&log)
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sent_ops, ["track", "receipt"]);
}

#[test]
fn a_helper_write_failure_settles_its_messages() {
    let mut f = Fixture::new();
    let (route, identity, log) = f.connected(&uuid4());
    let unsent = |op: &str, id: &str| super::helper_process::Unsent {
        op: op.into(),
        id: Some(id.into()),
    };
    let (uncertain, unwritten, receipt) = (uuid4(), uuid4(), uuid4());
    f.bridge
        .handle(Event::HelperWrite(
            identity.clone(),
            unsent("deliver", &uncertain),
            vec![unsent("receipt", &receipt), unsent("deliver", &unwritten)],
        ))
        .unwrap();
    f.bridge.drain_completions().unwrap();
    assert!(!f.bridge.helpers.contains_key(&route));
    assert_eq!(log.lock().unwrap().closed, 1);
    let statuses: Vec<(String, String)> = f
        .calls()
        .iter()
        .filter(|(path, _)| path == "/v1/status")
        .map(|(_, body)| {
            (
                body["id"].as_str().unwrap().to_string(),
                body["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [(uncertain, "unknown".into()), (unwritten, "failed".into())]
    );
    // A stale report from an earlier connection changes nothing.
    let before = f.calls().len();
    f.bridge
        .handle(Event::HelperWrite(
            (route, uuid4_hex(), identity.2.wrapping_sub(1)),
            unsent("deliver", &uuid4()),
            Vec::new(),
        ))
        .unwrap();
    assert_eq!(f.calls().len(), before);
}

/// Offline, the server is tried with exponential backoff (not every two
/// seconds), while local discovery still runs at least every 30 seconds; a
/// network change retries within a second, and an answered poll resets the
/// backoff. Ten simulated minutes, on the bridge's own scheduling.
#[test]
fn offline_retries_back_off_and_a_network_change_retries_soon() {
    let mut f = Fixture::new();
    let record = f.record.clone();
    f.discover_returns(&[&record]);
    f.hub.lock().unwrap().fail = Some(HubError::Os("server unreachable".into()));
    let mut now = 0.0;
    *f.clock.lock().unwrap() = now;
    let mut gaps = Vec::new();
    let mut last_attempt = 0.0;
    while now < 600.0 {
        let before = f.calls().len();
        f.bridge.reconcile().unwrap();
        f.bridge.drain_completions().unwrap();
        if f.calls().len() > before {
            gaps.push(now - last_attempt);
            last_attempt = now;
        }
        assert!(!f.bridge.online);
        now = f.bridge.next_scan.max(f.bridge.retry_at).max(now);
        *f.clock.lock().unwrap() = now;
    }
    f.bridge.close_watcher();
    let attempts = gaps.len();
    // Before: one attempt every 2 seconds (300). Now at most 24, at least 10.
    assert!(
        (10..=24).contains(&attempts),
        "{attempts} attempts: {gaps:?}"
    );
    assert!(
        gaps[1..].iter().all(|gap| *gap >= 1.0 && *gap <= 60.0),
        "{gaps:?}"
    );
    assert!(
        gaps[gaps.len() - 3..].iter().all(|gap| *gap >= 30.0),
        "{gaps:?}"
    );
    // Local discovery kept running at least every 30 seconds.
    let scans = *f.discovered.lock().unwrap();
    assert!(scans >= 20, "{scans} scans, attempts {gaps:?}");
    // One more failed attempt; then the network changes: the next attempt
    // is within a second instead of 30 to 60.
    f.retry();
    let now = *f.clock.lock().unwrap();
    assert!(f.bridge.reconnect_at - now >= 30.0);
    f.bridge.handle(Event::Network).unwrap();
    assert!(f.bridge.reconnect_at - now <= 1.0);
    assert!(f.bridge.next_scan - now <= 1.0);
    assert_eq!(f.bridge.reconnect.failures(), 0);
    // Back: registration, then an answered poll, resets the backoff.
    f.hub.lock().unwrap().fail = None;
    f.retry();
    assert!(f.bridge.online);
    f.bridge.reconnect.next_delay();
    let generation = f.bridge.generation;
    let registration = f.hub.lock().unwrap().registration.clone();
    f.poll(
        generation,
        json!({"machine": "local", "registration": registration, "peers": [], "events": []}),
    )
    .0
    .unwrap();
    assert_eq!(f.bridge.reconnect.failures(), 0);
    // Online, a network change changes nothing.
    let (at, scan) = (f.bridge.reconnect_at, f.bridge.next_scan);
    f.bridge.handle(Event::Network).unwrap();
    assert_eq!((f.bridge.reconnect_at, f.bridge.next_scan), (at, scan));
}

/// The runtime file is replaced (and synced to disk) only when what it says
/// changes, not on every registration; one removed behind the bridge's back
/// is written again.
#[test]
fn runtime_file_is_rewritten_only_when_it_changes() {
    let mut f = Fixture::new();
    f.bridge.start_observability().unwrap();
    let path = f.state.join("runtime.json");
    let inode =
        |path: &PathBuf| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).unwrap());
    f.discover_returns(&[]);
    f.register();
    let connected = inode(&path);
    for _ in 0..3 {
        f.bridge.publish_state().unwrap();
    }
    assert_eq!(inode(&path), connected);
    let generation = f.bridge.generation;
    f.bridge.handle(Event::Offline(generation, None)).unwrap();
    let offline = inode(&path);
    assert_ne!(offline, connected);
    f.bridge.withdraw().unwrap();
    assert_eq!(inode(&path), offline);
    std::fs::remove_file(&path).unwrap();
    f.bridge.publish_state().unwrap();
    assert!(path.exists());
    f.bridge.close_observability();
}

fn poll_thread(shared: &Arc<Shared>, post: FakePost) -> std::thread::JoinHandle<()> {
    let shared = shared.clone();
    std::thread::spawn(move || Bridge::poll_loop(shared, post))
}

fn counting_post(
    reply: std::result::Result<Map<String, Value>, HubError>,
) -> (FakePost, Arc<Mutex<usize>>) {
    let posts = Arc::new(Mutex::new(0));
    let count = posts.clone();
    let post: FakePost = Arc::new(move |_: &str, _: &Value| {
        *count.lock().unwrap() += 1;
        reply.clone()
    });
    (post, posts)
}

fn wait_for(what: &str, check: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Without a poll cursor (offline) the poll thread sleeps: it does not wake
/// at all until the main loop publishes one, then polls at once; a stop
/// ends it at once.
#[test]
fn the_poll_thread_sleeps_until_there_is_a_cursor() {
    let shared = Arc::new(Shared::new(Arc::new(BoundedQueue::new(8))));
    let (post, posts) = counting_post(Err(HubError::Http {
        status: 429,
        code: Some("rate_limited".into()),
    }));
    let thread = poll_thread(&shared, post);
    std::thread::sleep(Duration::from_millis(1500));
    // Before: four wakeups a second.
    assert_eq!(shared.poll_waits.load(Ordering::Relaxed), 0);
    assert_eq!(*posts.lock().unwrap(), 0);
    let started = std::time::Instant::now();
    shared.set_poll_state(Some((1, Some(uuid4()), None)));
    wait_for("the first poll", || *posts.lock().unwrap() == 1);
    assert!(started.elapsed() < Duration::from_millis(500));
    // A failure that leaves the cursor alone waits out a backoff (1 to 2 s).
    assert!(matches!(
        shared.events.get_timeout(Duration::from_secs(1)),
        Some(Event::PollError(1, _))
    ));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(*posts.lock().unwrap(), 1);
    // A new cursor ends the wait at once.
    shared.set_poll_state(Some((2, Some(uuid4()), None)));
    wait_for("the poll for the new cursor", || {
        *posts.lock().unwrap() == 2
    });
    let started = std::time::Instant::now();
    shared.request_stop();
    thread.join().unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
}

/// A stop releases a poll thread waiting for the main loop to accept a
/// reply it will never handle.
#[test]
fn a_stop_releases_a_poll_reply_nobody_accepts() {
    let shared = Arc::new(Shared::new(Arc::new(BoundedQueue::new(8))));
    let (post, posts) = counting_post(Ok(obj! {"ok" => true}));
    shared.set_poll_state(Some((1, Some(uuid4()), None)));
    let thread = poll_thread(&shared, post);
    wait_for("the poll reply", || {
        shared.poll_ticket.lock().unwrap().is_some()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(*posts.lock().unwrap(), 1);
    let started = std::time::Instant::now();
    shared.request_stop();
    thread.join().unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
}

/// An offline bridge's main loop sleeps between attempts: in three real
/// seconds against an unreachable server it tries at most twice and passes
/// through its loop only a handful of times.
#[test]
fn an_offline_bridge_sleeps_between_attempts() {
    let mut f = Fixture::new();
    f.hub.lock().unwrap().fail = Some(HubError::Os("server unreachable".into()));
    f.bridge.clock = None;
    let stop = f.bridge.stop_handle();
    let shared = f.bridge.shared.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        stop();
    });
    f.bridge.run().unwrap();
    stopper.join().unwrap();
    let registers = f
        .calls()
        .iter()
        .filter(|(path, _)| path == "/v1/register")
        .count();
    assert!((1..=3).contains(&registers), "{registers} registrations");
    assert!(f.calls().iter().all(|(path, _)| path != "/v1/poll"));
    let passes = shared.loop_passes.load(Ordering::Relaxed);
    assert!(passes <= 20, "{passes} main loop passes in 3 s");
    // The poll thread woke once: for the stop.
    assert!(shared.poll_waits.load(Ordering::Relaxed) <= 1);
}
