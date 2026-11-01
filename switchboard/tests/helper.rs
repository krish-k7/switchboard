//! Helper tests, run against the `switchboard helper` binary and the library.

mod common;

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Map, Value};
use switchboard::discovery::{auth_key, discover, session_record, Selection};
use switchboard::error::{Error, Kind};
use switchboard::fsutil::{read_json, write_new};
use switchboard::helper::{Emit, FrameSender, Peer, Pending};
use switchboard::identity::{monotonic, process_start, uuid4};
use switchboard::obj;
use switchboard::protocol::{
    address, parse_envelope, valid_hop_chain, DROP_REASONS, RECEIPT_STATUSES,
};

/// Entry point for the second-session fixture process (see `SessionProcess`).
#[test]
fn session_process() {
    session_process_main();
}

fn selection(ids: &[&str]) -> Selection {
    ids.iter().map(|s| s.to_string()).collect()
}

fn ids(values: &[&str]) -> Value {
    Value::Array(values.iter().map(|v| Value::from(*v)).collect())
}

#[test]
fn configuration_preserves_identity_receipts_and_revokes_partial_frames() {
    let mut f = Fixture::new();
    let second_sid = uuid4();
    let mut second = f.record.clone();
    second.insert("sessionId".into(), Value::from(second_sid.clone()));
    let fixture = SessionProcess::start("session_process", &f.root, &second, false);
    f.launch(None);
    let before = f.proxy.clone();
    let registry = f.sessions.join(format!("{}.json", f.child_pid()));
    let old_inode = std::fs::metadata(&registry).unwrap().ino();
    // Both outbound and inbound receipt maps exist before reconfiguration.
    let outgoing = f.user(None);
    f.send(&outgoing, None);
    assert_eq!(f.event(3)["revision"], json!(0));
    let identifier = uuid4();
    f.ipc(obj! {"op" => "deliver", "id" => identifier.clone(), "target_session" => f.sid.clone(), "text" => "pending", "mode" => "prompting"});
    let (_, frame, conn) = f.accept_frame();
    let held = obj! {
        "msgV" => 1, "type" => "control", "action" => "peer_message_status", "status" => "held",
        "orig_msg_id" => frame["msg_id"].clone(), "from" => address(&s(&f.path)),
    };
    f.send(&held, None);
    assert_eq!(f.event(3)["state"], "held");
    drop(conn);
    f.ipc(obj! {"op" => "configure", "revision" => 1, "name" => "renamed-peer", "sessions" => ids(&[&f.sid, &second_sid]), "status" => "busy"});
    assert_eq!(
        Value::Object(f.event(3)),
        json!({"event": "configured", "revision": 1})
    );
    let after = read_json(&s(&registry), 65536, true).unwrap();
    for key in ["pid", "procStart", "sessionId", "messagingSocketPath"] {
        assert_eq!(after[key], before[key]);
    }
    assert_ne!(std::fs::metadata(&registry).unwrap().ino(), old_inode);
    assert_eq!(after["name"], "renamed-peer");
    assert_eq!(after["status"], "busy");
    assert_eq!(auth_key(&s(&f.config_dir), &after).unwrap(), f.proxy_token);
    let mut delivered = held.clone();
    delivered.insert("status".into(), Value::from("delivered"));
    f.send(&delivered, None);
    assert_eq!(f.event(3)["detail"], "delivered");
    f.ipc(obj! {"op" => "receipt", "id" => outgoing["msg_id"].clone(), "state" => "handed_to_claude", "detail" => "delivered"});
    {
        let (_, receipt, mut conn) = f.accept_frame();
        assert_eq!(receipt["orig_msg_id"], outgoing["msg_id"]);
        assert_eq!(receipt["status"], "delivered");
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }
    // A frame begins while enrolled but completes after the revocation ack.
    {
        let mut partial =
            UnixStream::connect(after["messagingSocketPath"].as_str().unwrap()).unwrap();
        partial
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut raw = serde_json::to_vec(&Value::Object(f.user(None))).unwrap();
        raw.push(b'\n');
        let auth = format!("{}\n", json!({"type": "auth", "token": f.proxy_token}));
        partial.write_all(auth.as_bytes()).unwrap();
        partial.write_all(&raw[..10]).unwrap();
        f.ipc(obj! {"op" => "configure", "revision" => 2, "name" => "renamed-peer", "sessions" => ids(&[&second_sid])});
        assert_eq!(
            Value::Object(f.event(3)),
            json!({"event": "configured", "revision": 2})
        );
        f.ipc(obj! {"op" => "configure", "revision" => 1, "name" => "stale", "sessions" => ids(&[&f.sid, &second_sid])});
        let rejected = f.event(3);
        assert_eq!(rejected["event"], "configure_rejected");
        assert_eq!(rejected["detail"], "stale configuration revision");
        partial.write_all(&raw[10..]).unwrap();
        partial.shutdown(std::net::Shutdown::Write).unwrap();
        let _ = partial.read(&mut [0u8; 1]);
        assert_eq!(f.event(3)["event"], "rejected");
    }
    f.send(&f.user(None), None);
    assert_eq!(f.event(3)["event"], "rejected");
    f.ipc(obj! {"op" => "configure", "revision" => 3, "name" => "renamed-peer", "sessions" => ids(&[&f.sid, &second_sid])});
    assert_eq!(f.event(3)["revision"], json!(3));
    assert_eq!(
        read_json(&s(&registry), 65536, true).unwrap()["status"],
        "busy"
    );
    f.send(&f.user(None), None);
    let event = f.event(3);
    assert_eq!(
        (event["event"].clone(), event["revision"].clone()),
        (json!("outbound"), json!(3))
    );
    assert_eq!(f.terminate(), 0);
    assert!(!registry.exists()); // The updated inode stays in the cleanup ledger.
    assert_eq!(fixture.finish(), 0);
}

#[test]
fn activity_transitions_preserve_identity_and_receipt_correlation() {
    let mut f = Fixture::new();
    f.launch(Some("idle"));
    let registry = f.sessions.join(format!("{}.json", f.child_pid()));
    assert_eq!(f.proxy["status"], "idle");
    let outgoing = f.user(None);
    f.send(&outgoing, None);
    assert_eq!(f.event(3)["event"], "outbound");
    let without_status = |record: &Map<String, Value>| {
        let mut record = record.clone();
        record.shift_remove("status");
        record
    };
    let before = without_status(&f.proxy);
    for (revision, status) in ["busy", "waiting", "shell", "unknown"].iter().enumerate() {
        let revision = revision as i64 + 1;
        f.ipc(obj! {"op" => "configure", "revision" => revision, "name" => "remote-peer", "sessions" => ids(&[&f.sid]), "status" => *status});
        assert_eq!(
            Value::Object(f.event(3)),
            json!({"event": "configured", "revision": revision})
        );
        let record = read_json(&s(&registry), 65536, true).unwrap();
        let expected = if *status == "unknown" {
            None
        } else {
            Some(&Value::from(*status))
        };
        assert_eq!(record.get("status"), expected);
        assert_eq!(without_status(&record), before);
        assert_eq!(auth_key(&s(&f.config_dir), &record).unwrap(), f.proxy_token);
    }
    let original = std::fs::read(&registry).unwrap();
    for status in [
        json!("invalid"),
        json!(""),
        Value::Null,
        json!(true),
        json!([]),
        json!({}),
    ] {
        f.ipc(obj! {"op" => "configure", "revision" => 5, "name" => "not-applied", "sessions" => ids(&[&f.sid]), "status" => status});
        assert_eq!(f.event(3)["detail"], "invalid remote activity");
        assert_eq!(std::fs::read(&registry).unwrap(), original);
    }
    // Invalid metadata neither consumes the revision nor loses pending replies.
    f.ipc(obj! {"op" => "configure", "revision" => 5, "name" => "remote-peer", "sessions" => ids(&[&f.sid])});
    assert_eq!(
        Value::Object(f.event(3)),
        json!({"event": "configured", "revision": 5})
    );
    assert!(!read_json(&s(&registry), 65536, true)
        .unwrap()
        .contains_key("status"));
    f.ipc(obj! {"op" => "receipt", "id" => outgoing["msg_id"].clone(), "state" => "handed_to_claude", "detail" => "delivered"});
    let (_, receipt, mut conn) = f.accept_frame();
    assert_eq!(receipt["orig_msg_id"], outgoing["msg_id"]);
    assert_eq!(receipt["status"], "delivered");
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
}

#[test]
fn unknown_initial_activity_omits_native_status() {
    let mut f = Fixture::new();
    f.launch(Some("unknown"));
    assert!(!f.proxy.contains_key("status"));
}

#[test]
fn busy_initial_activity_is_native_status() {
    let mut f = Fixture::new();
    f.launch(Some("busy"));
    assert_eq!(f.proxy["status"], "busy");
}

#[test]
fn invalid_configuration_and_foreign_registry_are_not_applied() {
    let mut f = Fixture::new();
    f.launch(None);
    let registry = f.sessions.join(format!("{}.json", f.child_pid()));
    let original = std::fs::read(&registry).unwrap();
    let base = obj! {"op" => "configure", "revision" => 1, "name" => "valid-name", "sessions" => ids(&[&f.sid])};
    let many: Vec<String> = (0..33).map(|_| uuid4()).collect();
    let changes: Vec<(&str, Value)> = vec![
        ("revision", json!(true)),
        ("revision", json!(0)),
        ("name", json!("bad<>")),
        ("name", json!("")),
        ("sessions", json!([])),
        ("sessions", json!([f.sid, f.sid])),
        ("sessions", json!(["invalid"])),
        ("sessions", json!(many)),
    ];
    for (key, value) in changes {
        let mut command = base.clone();
        command.insert(key.into(), value);
        f.ipc(command);
        assert_eq!(f.event(3)["event"], "configure_rejected");
        assert_eq!(std::fs::read(&registry).unwrap(), original);
    }
    let replacement = f.sessions.join("replacement.json");
    let mut foreign = f.proxy.clone();
    foreign.insert("name".into(), Value::from("foreign-owner"));
    write_new(&s(&replacement), &foreign).unwrap();
    std::fs::rename(&replacement, &registry).unwrap();
    f.ipc(base);
    assert_eq!(f.event(3)["detail"], "proxy registry ownership changed");
    assert_eq!(f.terminate(), 0);
    assert_eq!(
        read_json(&s(&registry), 65536, true).unwrap()["name"],
        "foreign-owner"
    );
}

/// A failure after registration (the bridge's pipe already closed when the
/// helper reports ready) exits nonzero and leaves no owned file behind.
#[test]
fn startup_failure_removes_owned_files() {
    let mut f = Fixture::new();
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds is a valid two-element array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: both descriptors are fresh and owned here.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    drop(read_end);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_switchboard"))
        .args([
            "helper",
            "--config",
            &s(&f.config_path),
            "--route",
            &f.route,
            "--name",
            "startup-peer",
        ])
        .args(["--socket-dir", &s(&f.sockets)])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(write_end))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let code = wait_timeout(&mut child, 5.0).expect("helper exit");
    let pid = child.id() as i64;
    assert_eq!(code, 1);
    assert!(f.proxy_files_gone(pid));
    assert!(f.record_path.exists());
    f.child = None;
}

#[cfg(target_os = "linux")]
#[test]
fn worker_directed_signal_wakes_blocked_main_wait() {
    let mut f = Fixture::new();
    f.launch(None);
    let pid = f.child_pid();
    let tasks = std::path::PathBuf::from(format!("/proc/{pid}/task"));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut listener_tid = None;
    while Instant::now() < deadline {
        let waits: Vec<(i64, String)> = std::fs::read_dir(&tasks)
            .unwrap()
            .flatten()
            .filter_map(|task| {
                let tid: i64 = task.file_name().to_string_lossy().parse().ok()?;
                let wait = std::fs::read_to_string(task.path().join("wchan")).unwrap_or_default();
                Some((tid, wait))
            })
            .collect();
        let main_waiting = waits
            .iter()
            .any(|(tid, wait)| *tid == pid && wait.contains("poll"));
        let worker = waits
            .iter()
            .find(|(tid, wait)| *tid != pid && wait.contains("poll"));
        if main_waiting {
            if let Some((tid, _)) = worker {
                listener_tid = Some(*tid);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let tid = listener_tid.expect("helper did not reach both blocking waits");
    // SAFETY: tgkill to a thread of our own child.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            pid as libc::c_long,
            tid as libc::c_long,
            libc::SIGTERM as libc::c_long,
        )
    };
    assert_eq!(sent, 0);
    // No stdin, second signal, or periodic timeout wakes the main thread.
    let code = wait_timeout(f.child.as_mut().unwrap(), 2.0).expect("helper exit");
    assert_eq!(code, 0);
    assert!(f.proxy_files_gone(pid));
}

#[test]
fn idle_subprocess_signal_and_eof_cleanup() {
    for method in ["signal", "eof"] {
        let mut f = Fixture::new();
        f.launch(None);
        std::thread::sleep(Duration::from_millis(100)); // Both waits have no traffic.
        let started = Instant::now();
        let pid = f.child_pid();
        if method == "signal" {
            // SAFETY: signal to our own child.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        } else {
            drop(f.stdin.take());
        }
        let code = wait_timeout(f.child.as_mut().unwrap(), 3.0).expect("helper exit");
        assert_eq!(code, 0, "{method}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(f.proxy_files_gone(pid), "{method}");
        assert!(f.record_path.exists());
    }
}

#[test]
fn shutdown_interrupts_dripping_auth_and_frame() {
    for phase in ["auth", "frame"] {
        for method in ["signal", "eof"] {
            let mut f = Fixture::new();
            f.launch(None);
            let mut conn = UnixStream::connect(f.proxy_socket()).unwrap();
            if phase == "frame" {
                conn.write_all(
                    format!("{}\n", json!({"type": "auth", "token": f.proxy_token})).as_bytes(),
                )
                .unwrap();
            }
            let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let drip = {
                let stopped = Arc::clone(&stopped);
                let mut conn = conn.try_clone().unwrap();
                std::thread::spawn(move || {
                    while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(20));
                        if conn.write_all(b" ").is_err() {
                            break;
                        }
                    }
                })
            };
            std::thread::sleep(Duration::from_millis(150));
            let started = Instant::now();
            let pid = f.child_pid();
            if method == "signal" {
                // SAFETY: signal to our own child.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            } else {
                drop(f.stdin.take());
            }
            let code = wait_timeout(f.child.as_mut().unwrap(), 2.0).expect("helper exit");
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_eq!(code, 0, "{phase} {method}");
            assert!(f.proxy_files_gone(pid), "{phase} {method}");
            stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = conn.shutdown(std::net::Shutdown::Both);
            drip.join().unwrap();
        }
    }
}

fn channel_emit() -> (Emit, Receiver<Map<String, Value>>) {
    let (sender, receiver) = channel();
    let sender = Mutex::new(sender);
    let emit: Emit = Arc::new(move |event| {
        let _ = sender.lock().unwrap().send(event);
        Ok(())
    });
    (emit, receiver)
}

#[test]
fn idle_wait_deadline_wakeup_and_idempotent_close() {
    let f = Fixture::new();
    let (emit, events) = channel_emit();
    let idle = f.sockets.join("idle.sock");
    let peer = Peer::unregistered(
        &s(&f.config_dir),
        Selection::new(),
        &s(&f.path),
        "proxy",
        emit,
        None,
        Some(&s(&idle)),
    );
    let worker = {
        let peer = Arc::clone(&peer);
        std::thread::spawn(move || peer.serve())
    };
    let passes = || peer.serve_passes.load(std::sync::atomic::Ordering::SeqCst);
    let pend = |id: &str, at: f64| Pending {
        id: id.into(),
        record: Map::new(),
        at,
        state: None,
    };
    std::thread::sleep(Duration::from_millis(650));
    assert_eq!(passes(), 1);
    peer.state
        .lock()
        .unwrap()
        .pending
        .insert("future".into(), pend("future", monotonic()));
    assert!(peer.expiry_delay().unwrap() > 86390.0);
    peer.state
        .lock()
        .unwrap()
        .pending
        .insert("due".into(), pend("due", monotonic() - 86400.0));
    peer.wake_listener();
    let event = events.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        (event["id"].clone(), event["state"].clone()),
        (json!("due"), json!("unknown"))
    );
    assert_eq!(
        peer.state
            .lock()
            .unwrap()
            .pending
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["future".to_string()]
    );
    peer.state
        .lock()
        .unwrap()
        .pending
        .insert("timer".into(), pend("timer", monotonic() - 86400.0 + 0.1));
    peer.wake_listener();
    let event = events.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(event["id"], "timer");
    peer.state.lock().unwrap().pending.clear();
    peer.wake_listener();
    std::thread::sleep(Duration::from_millis(100));
    let count = passes();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(passes(), count);
    assert!(peer.expiry_delay().is_none());
    peer.close().unwrap();
    peer.close().unwrap();
    worker.join().unwrap().unwrap();
    assert!(peer.is_closed());
}

#[test]
fn readable_config_root_with_private_sessions() {
    let mut f = Fixture::new();
    std::fs::set_permissions(&f.config_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.launch(None);
    assert_eq!(
        std::fs::metadata(&f.config_dir).unwrap().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&f.sessions).unwrap().mode() & 0o777,
        0o700
    );
    std::fs::set_permissions(&f.config_dir, std::fs::Permissions::from_mode(0o775)).unwrap();
    let error = switchboard::fsutil::owned_config_dir(&s(&f.config_dir)).unwrap_err();
    assert!(error.message.contains("shared-writable"));
}

#[test]
fn unjoined_sender_is_told_how_to_join() {
    let mut f = Fixture::new();
    let selected_sid = uuid4();
    let mut selected = f.record.clone();
    selected.insert("sessionId".into(), Value::from(selected_sid.clone()));
    let fixture = SessionProcess::start("session_process", &f.root, &selected, false);
    std::fs::remove_file(&f.config_path).unwrap();
    let mut config = f.config.clone();
    config.insert("sessions".into(), ids(&[&selected_sid]));
    write_new(&s(&f.config_path), &config).unwrap();
    f.launch(None);
    f.send(&f.user(None), None);
    let event = f.event(3);
    assert_eq!(event["event"], "rejected");
    assert!(event["detail"]
        .as_str()
        .unwrap()
        .contains("told it how to join"));
    let (auth, frame, _conn) = f.accept_frame();
    assert_eq!(auth["token"], Value::from(f.token.clone()));
    assert_eq!(
        (frame["type"].clone(), frame["session_id"].clone()),
        (json!("user"), Value::from(f.sid.clone()))
    );
    let (attrs, body) = parse_envelope(frame["message"].get("content")).unwrap();
    assert_eq!(
        (attrs["from-name"].as_str(), attrs["from-mode"].as_str()),
        ("remote-peer", "prompting")
    );
    assert_eq!(
        body,
        "Switchboard on this machine wrote this, not remote-peer; don't reply to it.\n\n\
         Your message to remote-peer was not delivered: this chat hasn't joined Switchboard.\n\n\
         Ask the user to run /switchboard join (or !switchboard join), then resend it."
    );
    // A retry within a minute is still refused, without a second notice.
    f.send(&f.user(None), None);
    assert_eq!(f.event(3)["event"], "rejected");
    assert!(f.accept(1.0).is_none());
    assert_eq!(fixture.finish(), 0);
}

#[test]
fn kernel_enrolled_sender_and_rejection() {
    let mut f = Fixture::new();
    f.launch(None);
    // Native ListAgents opens and immediately closes empty liveness probes.
    drop(UnixStream::connect(f.proxy_socket()).unwrap());
    f.send(&f.user(None), None);
    let event = f.event(3);
    assert_eq!(event["event"], "outbound");
    assert_eq!(event["source_session"], Value::from(f.sid.clone()));
    assert_eq!(event["text"], "hello λ\nbody");
    assert_eq!(event["recipient"], Value::from(f.route.clone()));
    f.send(&f.user(Some("uds:/tmp/forged.sock")), None);
    assert_eq!(f.event(3)["event"], "rejected");
    f.send(&f.user(None), Some("wrong"));
    assert_eq!(f.event(3)["event"], "rejected");
    f.record.insert("procStart".into(), Value::from("stale"));
    f.rewrite_record();
    f.send(&f.user(None), None);
    assert_eq!(f.event(3)["event"], "rejected");
    let pid = f.child_pid();
    assert_eq!(f.terminate(), 0);
    assert!(f.proxy_files_gone(pid));
    assert!(f.record_path.exists());
}

#[test]
fn future_metadata_message_reply_and_receipt_release() {
    let mut f = Fixture::new();
    f.launch(None);
    assert_eq!(f.proxy["version"], "99.0.0");
    let identifier = uuid4();
    f.ipc(obj! {"op" => "deliver", "id" => identifier.clone(), "target_session" => f.sid.clone(), "text" => "inbound", "mode" => "bypass", "sender_name" => "remote"});
    let (auth, frame, conn) = f.accept_frame();
    assert_eq!(
        switchboard::identity::peer_pid(conn.as_raw_fd()).unwrap(),
        f.child_pid()
    );
    assert_eq!(auth["token"], Value::from(f.token.clone()));
    let (attrs, body) = parse_envelope(frame["message"].get("content")).unwrap();
    assert_eq!(body, "inbound");
    assert_eq!(attrs["from-mode"], "bypass");
    let receipt = obj! {
        "msgV" => 1, "type" => "control", "action" => "peer_message_status", "status" => "held",
        "orig_msg_id" => frame["msg_id"].clone(), "from" => address(&s(&f.path)),
    };
    f.send(&receipt, None);
    let event = f.event(3);
    assert_eq!(
        (event["id"].clone(), event["state"].clone()),
        (Value::from(identifier), json!("held"))
    );
    drop(conn);
    let mut release = receipt.clone();
    release.insert("status".into(), Value::from("delivered"));
    f.send(&release, None);
    let event = f.event(3);
    assert_eq!(
        (event["state"].clone(), event["detail"].clone()),
        (json!("handed_to_claude"), json!("delivered"))
    );
    assert!(f.no_event(200));
    f.send(&f.user(None), None);
    let reply = f.event(3);
    assert_eq!(reply["event"], "outbound");
    assert_eq!(reply["source_session"], Value::from(f.sid.clone()));
    assert_eq!(reply["text"], "hello λ\nbody");
}

type Sent = Arc<Mutex<Vec<Map<String, Value>>>>;

fn recording_sender(result: Option<Error>) -> (FrameSender, Sent) {
    let sent: Sent = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&sent);
    let sender: FrameSender = Arc::new(move |_, _, _, frame| {
        log.lock().unwrap().push(frame.clone());
        match &result {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    });
    (sender, sent)
}

#[test]
fn uncertain_write_retains_correlation() {
    let f = Fixture::new();
    let (emit, events) = channel_emit();
    let (sender, _) = recording_sender(Some(Error::write_unknown()));
    let peer = Peer::unregistered(
        &s(&f.config_dir),
        selection(&[&f.sid]),
        &s(&f.path),
        "proxy",
        emit,
        Some(sender),
        None,
    );
    let identifier = uuid4();
    peer.deliver(&obj! {"id" => identifier, "target_session" => f.sid.clone(), "text" => "body", "mode" => "prompting"}).unwrap();
    assert_eq!(events.try_recv().unwrap()["state"], "unknown");
    assert_eq!(peer.state.lock().unwrap().pending.len(), 1);
}

#[test]
fn many_handoffs_do_not_exhaust_pending_capacity() {
    let f = Fixture::new();
    let emit: Emit = Arc::new(|_| Ok(()));
    let (sender, _) = recording_sender(None);
    let peer = Peer::unregistered(
        &s(&f.config_dir),
        selection(&[&f.sid]),
        &s(&f.path),
        "proxy",
        emit,
        Some(sender),
        None,
    );
    for _ in 0..300 {
        peer.deliver(&obj! {"id" => uuid4(), "target_session" => f.sid.clone(), "text" => "body", "mode" => "prompting"}).unwrap();
    }
    let state = peer.state.lock().unwrap();
    assert_eq!(state.pending.len(), 256);
    assert_eq!(state.seen.len(), 300);
}

const PEER_NAME: &str = "switchboard/workstation/research-12345678";

struct ReceiptPeer {
    peer: Arc<Peer>,
    events: Receiver<Map<String, Value>>,
    sent: Sent,
}

fn receipt_peer(f: &Fixture) -> ReceiptPeer {
    let (emit, events) = channel_emit();
    let (sender, sent) = recording_sender(None);
    let peer = Peer::unregistered(
        &s(&f.config_dir),
        selection(&[&f.sid]),
        &s(&f.path),
        PEER_NAME,
        emit,
        Some(sender),
        None,
    );
    ReceiptPeer { peer, events, sent }
}

impl ReceiptPeer {
    fn pend(&self, wire: &str, hub: &str, record: &Map<String, Value>) {
        let pending = Pending {
            id: hub.into(),
            record: record.clone(),
            at: monotonic(),
            state: None,
        };
        self.peer
            .state
            .lock()
            .unwrap()
            .pending
            .insert(wire.into(), pending);
    }

    fn outbound(&self, id: &str, record: &Map<String, Value>) {
        self.peer
            .state
            .lock()
            .unwrap()
            .outbound
            .insert(id.into(), record.clone());
    }

    fn last_sent(&self) -> Option<Map<String, Value>> {
        self.sent.lock().unwrap().last().cloned()
    }

    fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

fn receipt_state(status: &str) -> &'static str {
    switchboard::protocol::receipt_state(status).unwrap()
}

#[test]
fn native_receipt_semantics_roundtrip_and_generic_no_emit() {
    let f = Fixture::new();
    for status in RECEIPT_STATUSES {
        let mapped = receipt_state(status);
        let p = receipt_peer(&f);
        let (wire_id, hub_id) = (uuid4(), uuid4());
        p.pend(&wire_id, &hub_id, &f.record);
        p.outbound(&hub_id, &f.record);
        p.peer
            .receipt(
                &obj! {"status" => status, "orig_msg_id" => wire_id},
                &f.record,
            )
            .unwrap();
        let event = p.events.try_recv().unwrap();
        assert_eq!(event["state"], mapped);
        p.peer.relay_receipt(&event).unwrap();
        let frame = p.last_sent().unwrap();
        assert_eq!(
            frame["status"],
            if status == "refused" {
                "expired"
            } else {
                status
            }
        );
        assert_eq!(frame["orig_msg_id"], Value::from(hub_id.clone()));
        if status == "refused" {
            assert_eq!(frame["status_detail"], "refused");
        }
        let state = p.peer.state.lock().unwrap();
        assert_eq!(!state.pending.is_empty(), status == "held");
        assert_eq!(!state.outbound.is_empty(), status == "held");
        drop(state);
        for wrong in ["held", "refused", "failed", "handed_to_claude"]
            .into_iter()
            .filter(|w| *w != mapped)
        {
            p.outbound(&hub_id, &f.record);
            let before = p.sent_count();
            let mut changed = event.clone();
            changed.insert("state".into(), Value::from(wrong));
            let error = p.peer.relay_receipt(&changed).unwrap_err();
            assert!(error.message.contains("state/detail mismatch"));
            assert_eq!(p.sent_count(), before);
        }
    }
    // Generic failures have no native receipt; the sender gets exactly one
    // Switchboard-authored notice and never a fabricated native receipt.
    let p = receipt_peer(&f);
    let identifier = uuid4();
    p.outbound(&identifier, &f.record);
    p.peer.state.lock().unwrap().outbound_notes.insert(
        identifier.clone(),
        switchboard::helper::Note {
            text: "please rerun the mesh study".into(),
            mode: Some("bypass".into()),
        },
    );
    p.peer.relay_receipt(&obj! {"id" => identifier.clone(), "state" => "failed", "detail" => "recipient offline"}).unwrap();
    let frame = p.last_sent().unwrap();
    assert_eq!(frame["type"], "user");
    assert_eq!(frame["session_id"], f.record["sessionId"]);
    let (attrs, body) = parse_envelope(frame["message"].get("content")).unwrap();
    assert_eq!(attrs["from-mode"], "bypass");
    assert_eq!(
        body,
        format!(
            "Switchboard on this machine wrote this, not {PEER_NAME}; don't reply to it.\n\n\
             Your message to {PEER_NAME} was not delivered (recipient offline). \
             Your message began: \"please rerun the mesh study\"\n\n\
             Resend it later or tell the user."
        )
    );
    assert!(!p
        .peer
        .state
        .lock()
        .unwrap()
        .outbound
        .contains_key(&identifier));
    p.outbound(&identifier, &f.record);
    for (state, detail) in [
        ("unknown", Value::Null),
        ("failed", json!("recipient offline")),
        ("handed_to_claude", json!("native transport written")),
        ("held", json!("native receipt: held; drop_reason=duplicate")),
    ] {
        let before = p.sent_count();
        p.peer
            .relay_receipt(&obj! {"id" => identifier.clone(), "state" => state, "detail" => detail})
            .unwrap();
        assert_eq!(p.sent_count(), before);
    }
}

#[test]
fn uncertain_outcome_notice_and_handoff_suppression() {
    let f = Fixture::new();
    let p = receipt_peer(&f);
    let (uncertain, handed) = (uuid4(), uuid4());
    p.outbound(&uncertain, &f.record);
    p.outbound(&handed, &f.record);
    p.peer
        .relay_receipt(&obj! {"id" => uncertain.clone(), "state" => "unknown", "detail" => "delivery not confirmed within 30 seconds"})
        .unwrap();
    let (_, body) = parse_envelope(p.last_sent().unwrap()["message"].get("content")).unwrap();
    assert!(body.starts_with(&format!(
        "Switchboard on this machine wrote this, not {PEER_NAME}; don't reply to it.\n\n"
    )));
    assert!(body.contains(&format!(
        "Switchboard couldn't confirm that your message to {PEER_NAME} arrived \
         (delivery not confirmed within 30 seconds); it may or may not have reached that chat."
    )));
    assert!(body.ends_with("\n\nIf it matters, resend it once or tell the user."));
    assert!(p
        .peer
        .state
        .lock()
        .unwrap()
        .outbound
        .contains_key(&uncertain)); // A later receipt can still be relayed.
    let before = p.sent_count();
    // A confirmed handoff followed by the 24-hour correlation expiry is not news.
    p.peer.relay_receipt(&obj! {"id" => handed.clone(), "state" => "handed_to_claude", "detail" => "native transport written"}).unwrap();
    p.peer
        .relay_receipt(&obj! {"id" => handed, "state" => "unknown", "detail" => "native receipt correlation expired after 24 hours"})
        .unwrap();
    assert_eq!(p.sent_count(), before);
}

#[test]
fn track_adopts_outbound_for_replacement_helper() {
    let f = Fixture::new();
    let p = receipt_peer(&f);
    let identifier = uuid4();
    // The fixture session is genuinely live, so discovery finds it.
    p.peer.track(&obj! {"id" => identifier.clone(), "source_session" => f.sid.clone(), "text" => "hello", "mode" => "prompting"}).unwrap();
    let state = p.peer.state.lock().unwrap();
    assert_eq!(state.outbound[&identifier], f.record);
    let note = &state.outbound_notes[&identifier];
    assert_eq!(
        (note.text.as_str(), note.mode.as_deref()),
        ("hello", Some("prompting"))
    );
    drop(state);
    assert!(p.peer.track(&obj! {"id" => identifier, "source_session" => uuid4(), "text" => "hello", "mode" => "prompting"}).is_err());
}

#[test]
fn a_notice_follows_its_chat_into_a_resumed_process() {
    let f = Fixture::new();
    // The chat's earlier process: the same session UUID under another PID.
    let parked = f.root.join("parked.json");
    std::fs::rename(&f.record_path, &parked).unwrap();
    let earlier = SessionProcess::start("session_process", &f.root, &f.record, false);
    let old = discover(&s(&f.config_dir), &selection(&[&f.sid]))
        .unwrap()
        .shift_remove(&f.sid)
        .unwrap();
    assert_eq!(old["pid"], json!(earlier.pid));
    earlier.finish();
    // Resumed in this process.
    std::fs::rename(&parked, &f.record_path).unwrap();
    let (emit, _events) = channel_emit();
    let targets = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&targets);
    let sender: FrameSender = Arc::new(move |_, _, record, frame| {
        seen.lock()
            .unwrap()
            .push((record["pid"].clone(), frame["type"].clone()));
        Ok(())
    });
    let peer = Peer::unregistered(
        &s(&f.config_dir),
        selection(&[&f.sid]),
        &s(&f.path),
        PEER_NAME,
        emit,
        Some(sender),
        None,
    );
    let (notice, receipt) = (uuid4(), uuid4());
    for id in [&notice, &receipt] {
        peer.state
            .lock()
            .unwrap()
            .outbound
            .insert(id.clone(), old.clone());
    }
    // A Switchboard notice reaches the chat in its current process ...
    peer.relay_receipt(&obj! {"id" => notice, "state" => "unknown", "detail" => "lost"})
        .unwrap();
    // ... while a native receipt stays with the process whose state it names.
    peer.relay_receipt(
        &obj! {"id" => receipt, "state" => "held", "detail" => "native receipt: held"},
    )
    .unwrap();
    assert_eq!(
        *targets.lock().unwrap(),
        vec![
            (f.record["pid"].clone(), json!("user")),
            (old["pid"].clone(), json!("control")),
        ]
    );
}

#[test]
fn native_drop_batch_scopes_identity_and_preserves_reasons() {
    let f = Fixture::new();
    let mut reasons = DROP_REASONS.to_vec();
    reasons.sort();
    for reason in reasons {
        let p = receipt_peer(&f);
        let wire: Vec<String> = (0..5).map(|_| uuid4()).collect();
        let hub: Vec<String> = (0..5).map(|_| uuid4()).collect();
        for (index, identifier) in wire.iter().enumerate() {
            let mut record = f.record.clone();
            match index {
                2 => {
                    record.insert(
                        "pid".into(),
                        Value::from(record["pid"].as_i64().unwrap() + 100000),
                    );
                }
                3 => {
                    record.insert("procStart".into(), Value::from("older"));
                }
                4 => {
                    record.insert("sessionId".into(), Value::from(uuid4()));
                }
                _ => {}
            }
            p.pend(identifier, &hub[index], &record);
        }
        let mut batch: Vec<Value> = wire.iter().map(|w| Value::from(w.clone())).collect();
        batch.push(Value::from(wire[0].clone()));
        p.peer
            .receipt(&obj! {"status" => "dropped", "orig_msg_id" => uuid4(), "dropped_msg_ids" => batch, "drop_reason" => reason}, &f.record)
            .unwrap();
        for expected in &hub[..2] {
            let event = p.events.try_recv().unwrap();
            assert_eq!(
                Value::Object(event.clone()),
                json!({"event": "status", "id": expected, "state": "failed", "detail": format!("native receipt: dropped; drop_reason={reason}")})
            );
            p.outbound(expected, &f.record);
            p.peer.relay_receipt(&event).unwrap();
            let frame = p.last_sent().unwrap();
            assert_eq!(frame["drop_reason"], reason);
            assert_eq!(frame["dropped_msg_ids"], json!([expected]));
        }
        assert!(p.events.try_recv().is_err());
        let left: Vec<String> = p
            .peer
            .state
            .lock()
            .unwrap()
            .pending
            .keys()
            .cloned()
            .collect();
        assert_eq!(left, wire[2..].to_vec());
    }
    let bad_batches = [
        obj! {"dropped_msg_ids" => json!([])},
        obj! {"dropped_msg_ids" => json!(["bad"])},
        obj! {"dropped_msg_ids" => Value::Array(vec![Value::from(uuid4()); 257])},
        obj! {"status" => "held"},
    ];
    for changes in bad_batches {
        let p = receipt_peer(&f);
        let identifier = uuid4();
        p.pend(&identifier, &uuid4(), &f.record);
        let mut frame = obj! {"status" => "dropped", "dropped_msg_ids" => json!([identifier])};
        for (key, value) in changes {
            frame.insert(key, value);
        }
        assert!(p.peer.receipt(&frame, &f.record).is_err());
        assert!(p
            .peer
            .state
            .lock()
            .unwrap()
            .pending
            .contains_key(&identifier));
    }
    let p = receipt_peer(&f);
    let identifier = uuid4();
    p.pend(&identifier, &uuid4(), &f.record);
    assert!(p.peer.receipt(&obj! {"status" => "dropped", "dropped_msg_ids" => json!([identifier, "malformed-tail"])}, &f.record).is_err());
    assert!(p
        .peer
        .state
        .lock()
        .unwrap()
        .pending
        .contains_key(&identifier));
    assert!(p.events.try_recv().is_err());
    let error = p
        .peer
        .receipt(
            &obj! {"status" => "dropped", "dropped_msg_ids" => json!([uuid4()])},
            &f.record,
        )
        .unwrap_err();
    assert_eq!(error.message, "unmatched receipt identity");
    assert!(p
        .peer
        .state
        .lock()
        .unwrap()
        .pending
        .contains_key(&identifier));
    p.peer
        .receipt(
            &obj! {"status" => "dropped", "dropped_msg_ids" => json!([identifier])},
            &f.record,
        )
        .unwrap();
    assert_eq!(
        p.events.try_recv().unwrap()["detail"],
        "native receipt: dropped"
    );
}

#[test]
fn native_immediate_drop_empty_batch_uses_original_id() {
    let f = Fixture::new();
    let mut reasons = DROP_REASONS.to_vec();
    reasons.sort();
    for reason in reasons {
        let p = receipt_peer(&f);
        let (identifier, hub_id) = (uuid4(), uuid4());
        p.pend(&identifier, &hub_id, &f.record);
        p.outbound(&hub_id, &f.record);
        p.peer
            .receipt(&obj! {"status" => "dropped", "orig_msg_id" => identifier, "dropped_msg_ids" => json!([]), "drop_reason" => reason}, &f.record)
            .unwrap();
        let event = p.events.try_recv().unwrap();
        assert_eq!(event["id"], Value::from(hub_id.clone()));
        assert_eq!(
            event["detail"],
            Value::from(format!("native receipt: dropped; drop_reason={reason}"))
        );
        assert!(p.peer.state.lock().unwrap().pending.is_empty());
        p.peer.relay_receipt(&event).unwrap();
        let frame = p.last_sent().unwrap();
        assert_eq!(frame["orig_msg_id"], Value::from(hub_id));
        assert_eq!(frame["drop_reason"], reason);
    }
    let p = receipt_peer(&f);
    let identifier = uuid4();
    p.pend(&identifier, &identifier, &f.record);
    let error = p
        .peer
        .receipt(
            &obj! {"status" => "dropped", "dropped_msg_ids" => json!([])},
            &f.record,
        )
        .unwrap_err();
    assert_eq!(error.message, "unmatched receipt identity");
    assert!(p
        .peer
        .state
        .lock()
        .unwrap()
        .pending
        .contains_key(&identifier));
    assert!(p.events.try_recv().is_err());
}

#[test]
fn native_unknown_drop_metadata_is_omitted() {
    let f = Fixture::new();
    for status in RECEIPT_STATUSES {
        let mapped = receipt_state(status);
        for reason in [
            json!("unknown"),
            json!([]),
            json!({"untrusted": true}),
            json!("queue-full"),
        ] {
            let p = receipt_peer(&f);
            let identifier = uuid4();
            p.pend(&identifier, &identifier, &f.record);
            p.outbound(&identifier, &f.record);
            p.peer
                .receipt(&obj! {"status" => status, "orig_msg_id" => identifier.clone(), "drop_reason" => reason.clone()}, &f.record)
                .unwrap();
            let event = p.events.try_recv().unwrap();
            let mut expected = if status == "delivered" {
                "delivered".to_string()
            } else {
                format!("native receipt: {status}")
            };
            let kept = status == "dropped" && reason == json!("queue-full");
            if kept {
                expected.push_str("; drop_reason=queue-full");
            }
            assert_eq!(
                (event["state"].clone(), event["detail"].clone()),
                (json!(mapped), json!(expected))
            );
            p.peer.relay_receipt(&event).unwrap();
            let frame = p.last_sent().unwrap();
            if kept {
                assert_eq!(frame["drop_reason"], "queue-full");
            } else {
                assert!(!frame.contains_key("drop_reason"));
            }
        }
    }
}

#[test]
fn native_drop_batch_accepts_256_correlations() {
    let f = Fixture::new();
    let p = receipt_peer(&f);
    let identifiers: Vec<String> = (0..256).map(|_| uuid4()).collect();
    for identifier in &identifiers {
        p.pend(identifier, identifier, &f.record);
    }
    p.peer
        .receipt(
            &obj! {"status" => "dropped", "dropped_msg_ids" => json!(identifiers)},
            &f.record,
        )
        .unwrap();
    assert!(p.peer.state.lock().unwrap().pending.is_empty());
    let seen: std::collections::HashSet<String> = p
        .events
        .try_iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(seen, identifiers.into_iter().collect());
}

#[test]
fn native_refused_normalization_and_held_release() {
    let f = Fixture::new();
    let p = receipt_peer(&f);
    let (wire_id, hub_id) = (uuid4(), uuid4());
    p.pend(&wire_id, &hub_id, &f.record);
    p.peer
        .receipt(
            &obj! {"status" => "held", "orig_msg_id" => wire_id.clone()},
            &f.record,
        )
        .unwrap();
    assert_eq!(
        p.events.try_recv().unwrap()["detail"],
        "native receipt: held"
    );
    p.peer
        .receipt(
            &obj! {"status" => "delivered", "orig_msg_id" => wire_id.clone()},
            &f.record,
        )
        .unwrap();
    assert_eq!(p.events.try_recv().unwrap()["detail"], "delivered");
    assert!(!p.peer.state.lock().unwrap().pending.contains_key(&wire_id));
    p.pend(&wire_id, &hub_id, &f.record);
    p.peer
        .receipt(
            &obj! {"status" => "expired", "status_detail" => "refused", "orig_msg_id" => wire_id},
            &f.record,
        )
        .unwrap();
    assert_eq!(
        p.events.try_recv().unwrap()["detail"],
        "native receipt: refused"
    );
}

#[test]
fn remote_receipt_returns_to_native_origin() {
    let mut f = Fixture::new();
    f.launch(None);
    let frame = f.user(None);
    f.send(&frame, None);
    assert_eq!(f.event(3)["event"], "outbound");
    f.ipc(obj! {"op" => "receipt", "id" => frame["msg_id"].clone(), "state" => "refused", "detail" => "native receipt: refused"});
    let (auth, receipt, _conn) = f.accept_frame();
    assert_eq!(auth["token"], Value::from(f.token.clone()));
    assert_eq!(receipt["orig_msg_id"], frame["msg_id"]);
    assert_eq!(receipt["session_id"], Value::from(f.sid.clone()));
    assert_eq!(
        (receipt["status"].clone(), receipt["status_detail"].clone()),
        (json!("expired"), json!("refused"))
    );
}

#[test]
fn product_version_metadata_does_not_gate_discovery() {
    let mut f = Fixture::new();
    let pid = Value::from(std::process::id() as i64);
    let selected = selection(&[&f.sid]);
    for version in [
        json!("99.0.0"),
        json!("nightly-unreleased"),
        json!(""),
        Value::Null,
    ] {
        f.record.insert("version".into(), version.clone());
        f.rewrite_record();
        assert_eq!(
            session_record(&s(&f.config_dir), &pid, &selected).unwrap()["version"],
            version
        );
        assert!(discover(&s(&f.config_dir), &selected)
            .unwrap()
            .contains_key(&f.sid));
    }
    f.record.shift_remove("version");
    f.rewrite_record();
    assert!(discover(&s(&f.config_dir), &selected)
        .unwrap()
        .contains_key(&f.sid));
}

#[test]
fn future_version_does_not_bypass_protocol_or_identity() {
    let mut f = Fixture::new();
    let pid = Value::from(std::process::id() as i64);
    let selected = selection(&[&f.sid]);
    for protocol in [Value::Null, json!("1"), json!(2)] {
        f.record.insert("peerProtocol".into(), protocol);
        f.rewrite_record();
        let error = session_record(&s(&f.config_dir), &pid, &selected).unwrap_err();
        assert!(error.message.contains("unsupported native protocol"));
    }
    f.record.insert("peerProtocol".into(), json!(1));
    f.record.insert("procStart".into(), json!("stale"));
    f.rewrite_record();
    let error = session_record(&s(&f.config_dir), &pid, &selected).unwrap_err();
    assert!(error.message.contains("native process identity mismatch"));
}

#[test]
fn mixed_release_proxy_metadata_uses_lowest_selected_uuid() {
    let mut f = Fixture::new();
    let second_sid = "00000000-0000-4000-8000-000000000001";
    // A genuine second fixture process owns its own socket, PID, and key.
    let mut second = f.record.clone();
    second.insert("sessionId".into(), Value::from(second_sid));
    second.insert("version".into(), Value::from("nightly-unreleased"));
    let fixture = SessionProcess::start("session_process", &f.root, &second, true);
    f.config
        .insert("sessions".into(), ids(&[&f.sid, second_sid]));
    f.rewrite_config();
    let records = discover(&s(&f.config_dir), &selection(&[&f.sid, second_sid])).unwrap();
    let mut keys: Vec<String> = records.keys().cloned().collect();
    keys.sort();
    let mut expected = vec![f.sid.clone(), second_sid.to_string()];
    expected.sort();
    assert_eq!(keys, expected);
    f.launch(None);
    let lowest = records.keys().min().unwrap();
    assert_eq!(f.proxy["version"], records[lowest]["version"]);
    assert_eq!(f.proxy["version"], "nightly-unreleased");
    f.send(&f.user(None), None);
    assert_eq!(f.event(3)["event"], "outbound");
    drop(f.stdin.take());
    assert_eq!(fixture.finish(), 0);
}

#[test]
fn malformed_envelope_hops_and_symlink_files() {
    let f = Fixture::new();
    assert!(valid_hop_chain(Some(&json!("not-a-chain"))).is_err());
    let chain = format!("{},{}", "a".repeat(24), "b".repeat(24));
    assert_eq!(valid_hop_chain(Some(&json!(chain))).unwrap(), chain);
    let swapped = "<cross-session-message from-mode=\"prompting\" from=\"uds:/x\">\nx\n</cross-session-message>";
    assert!(parse_envelope(Some(&json!(swapped))).is_err());
    let link = f.root.join("link.json");
    std::os::unix::fs::symlink(&f.record_path, &link).unwrap();
    let error = read_json(&s(&link), 65536, true).unwrap_err();
    assert_eq!(error.kind, Kind::Os);
}

/// The helper's own process identity matches what Claude reads.
#[test]
fn helper_registry_identity_is_genuine() {
    let mut f = Fixture::new();
    f.launch(None);
    let pid = f.child_pid();
    assert_eq!(
        f.proxy["procStart"],
        Value::from(process_start(pid).unwrap())
    );
    let mode = std::fs::metadata(f.sessions.join(format!("{pid}.json")))
        .unwrap()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}
