//! Fixtures shared by the helper tests. Every registry, key, and socket
//! belongs to the test: the test process itself is a genuine native session
//! in a private temporary config directory.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use switchboard::discovery::{auth_key, socket_digest};
use switchboard::fsutil::{read_json, write_new};
use switchboard::identity::{local_domain, process_start, token_hex16, uuid4};
use switchboard::obj;
use switchboard::protocol::{address, envelope, Attrs};

pub type Obj = Map<String, Value>;

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let template = std::ffi::CString::new(format!("/tmp/{prefix}XXXXXX")).unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        let made = unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) };
        assert!(!made.is_null(), "mkdtemp failed");
        raw.pop();
        let path = PathBuf::from(String::from_utf8(raw).unwrap());
        TempDir(std::fs::canonicalize(path).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn mkdir_private(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

pub struct Fixture {
    pub temp: TempDir,
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub sessions: PathBuf,
    pub state: PathBuf,
    pub sockets: PathBuf,
    pub path: PathBuf,
    pub listener: UnixListener,
    pub sid: String,
    pub token: String,
    pub record: Obj,
    pub record_path: PathBuf,
    pub config: Obj,
    pub config_path: PathBuf,
    pub route: String,
    pub child: Option<Child>,
    pub stdin: Option<ChildStdin>,
    pub events: Option<Receiver<Obj>>,
    pub proxy: Obj,
    pub proxy_token: String,
}

impl Fixture {
    pub fn new() -> Self {
        let temp = TempDir::new("sbn-");
        let root = temp.0.clone();
        let config_dir = root.join("config");
        let sessions = config_dir.join("sessions");
        mkdir_private(&sessions);
        std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = root.join("state");
        mkdir_private(&state);
        let sockets = root.join("cc-socks");
        mkdir_private(&sockets);
        let pid = std::process::id() as i64;
        let path = sockets.join(format!("{pid}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let sid = uuid4();
        let token = token_hex16();
        let domain = local_domain().unwrap();
        let record = obj! {
            "pid" => pid,
            "sessionId" => sid.clone(),
            "procStart" => process_start(pid).unwrap(),
            "pidDomain" => domain.clone(),
            "version" => "99.0.0",
            "peerProtocol" => 1,
            "messagingSocketPath" => s(&path),
            "name" => "fixture",
            "status" => "idle",
        };
        let record_path = sessions.join(format!("{pid}.json"));
        write_new(&s(&record_path), &record).unwrap();
        let key = sessions.join(format!("{pid}.{}.key", socket_digest(&s(&path))));
        write_new(
            &s(&key),
            &obj! {"peerToken" => token.clone(), "procStart" => record["procStart"].clone(), "pidDomain" => domain},
        )
        .unwrap();
        let config = obj! {
            "enabled" => true,
            "sessions" => Value::Array(vec![Value::from(sid.clone())]),
            "config_dir" => s(&config_dir),
            "state_dir" => s(&state),
        };
        let config_path = root.join("bridge.json");
        write_new(&s(&config_path), &config).unwrap();
        Fixture {
            temp,
            root,
            config_dir,
            sessions,
            state,
            sockets,
            path,
            listener,
            sid,
            token,
            record,
            record_path,
            config,
            config_path,
            route: format!("remote/{}", uuid4()),
            child: None,
            stdin: None,
            events: None,
            proxy: Map::new(),
            proxy_token: String::new(),
        }
    }

    pub fn rewrite_record(&self) {
        std::fs::write(
            &self.record_path,
            serde_json::to_string(&Value::Object(self.record.clone())).unwrap(),
        )
        .unwrap();
    }

    pub fn rewrite_config(&self) {
        std::fs::write(
            &self.config_path,
            serde_json::to_string(&Value::Object(self.config.clone())).unwrap(),
        )
        .unwrap();
    }

    pub fn spawn(&mut self, status: Option<&str>) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_switchboard"));
        command.args([
            "helper",
            "--config",
            &s(&self.config_path),
            "--route",
            &self.route,
        ]);
        command.args(["--name", "remote-peer", "--socket-dir", &s(&self.sockets)]);
        if let Some(status) = status {
            command.args(["--status", status]);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().unwrap()
    }

    /// Start the helper and check its native registration.
    pub fn launch(&mut self, status: Option<&str>) {
        let mut child = self.spawn(status);
        let stdout = child.stdout.take().unwrap();
        self.stdin = child.stdin.take();
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value: Value = serde_json::from_str(&line).unwrap();
                if sender.send(value.as_object().unwrap().clone()).is_err() {
                    break;
                }
            }
        });
        self.events = Some(receiver);
        self.child = Some(child);
        let ready = self.event(5);
        assert_eq!(ready["event"], "ready");
        let pid = self.child_pid();
        self.proxy =
            read_json(&s(&self.sessions.join(format!("{pid}.json"))), 65536, true).unwrap();
        self.proxy_token = auth_key(&s(&self.config_dir), &self.proxy).unwrap();
        assert_eq!(self.proxy["pid"], Value::from(pid));
        assert_eq!(self.proxy["peerBridgeProxy"], Value::Bool(true));
        assert_eq!(
            self.proxy["procStart"],
            Value::from(process_start(pid).unwrap())
        );
        let both = [
            self.sid.clone(),
            self.proxy["sessionId"].as_str().unwrap().to_string(),
        ]
        .into_iter()
        .collect();
        let found = switchboard::discovery::discover(&s(&self.config_dir), &both).unwrap();
        assert_eq!(
            found.keys().cloned().collect::<Vec<_>>(),
            vec![self.sid.clone()]
        );
    }

    pub fn child_pid(&self) -> i64 {
        self.child.as_ref().unwrap().id() as i64
    }

    pub fn event(&self, seconds: u64) -> Obj {
        self.events
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(seconds))
            .expect("helper event")
    }

    pub fn no_event(&self, millis: u64) -> bool {
        self.events
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_millis(millis))
            .is_err()
    }

    pub fn ipc(&mut self, command: Obj) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(
                serde_json::to_string(&Value::Object(command))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    pub fn proxy_socket(&self) -> String {
        self.proxy["messagingSocketPath"]
            .as_str()
            .unwrap()
            .to_string()
    }

    pub fn proxy_sid(&self) -> String {
        self.proxy["sessionId"].as_str().unwrap().to_string()
    }

    /// Write an authenticated native frame to the helper, as a local chat does.
    pub fn send(&self, frame: &Obj, token: Option<&str>) {
        let mut conn = UnixStream::connect(self.proxy_socket()).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let auth = obj! {"type" => "auth", "token" => token.unwrap_or(&self.proxy_token)};
        let mut data = serde_json::to_vec(&Value::Object(auth)).unwrap();
        data.push(b'\n');
        data.extend(serde_json::to_vec(&Value::Object(frame.clone())).unwrap());
        data.push(b'\n');
        conn.write_all(&data).unwrap();
        conn.shutdown(std::net::Shutdown::Write).unwrap();
        let mut one = [0u8; 1];
        let _ = conn.read(&mut one);
    }

    pub fn user(&self, source: Option<&str>) -> Obj {
        let mut attrs = Attrs::new();
        attrs.insert(
            "from".into(),
            source
                .map(str::to_string)
                .unwrap_or_else(|| address(&s(&self.path))),
        );
        attrs.insert("from-name".into(), "fixture".into());
        attrs.insert("from-mode".into(), "prompting".into());
        obj! {
            "msgV" => 1,
            "msg_id" => uuid4(),
            "type" => "user",
            "session_id" => self.proxy_sid(),
            "message" => Value::Object(obj! {"role" => "user", "content" => envelope(&attrs, "hello λ\nbody").unwrap()}),
            "from" => address(&s(&self.path)),
        }
    }

    /// Accept one connection the helper makes to this fixture session.
    pub fn accept(&self, seconds: f64) -> Option<UnixStream> {
        accept_timeout(&self.listener, seconds)
    }

    /// Accept a connection and read its auth line and frame.
    pub fn accept_frame(&self) -> (Obj, Obj, UnixStream) {
        let conn = self.accept(5.0).expect("helper connection");
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let (mut auth, mut frame) = (String::new(), String::new());
        reader.read_line(&mut auth).unwrap();
        reader.read_line(&mut frame).unwrap();
        let parse = |line: &str| {
            serde_json::from_str::<Value>(line)
                .unwrap()
                .as_object()
                .unwrap()
                .clone()
        };
        (parse(&auth), parse(&frame), conn)
    }

    pub fn terminate(&mut self) -> i32 {
        let child = self.child.as_mut().unwrap();
        // SAFETY: signal to our own child process.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        wait_timeout(child, 8.0).expect("helper exit")
    }

    pub fn proxy_files_gone(&self, pid: i64) -> bool {
        let registry = self.sessions.join(format!("{pid}.json"));
        let socket = self.sockets.join(format!("{pid}.sock"));
        let ledgers = std::fs::read_dir(&self.state)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("proxy-"));
        let keys = std::fs::read_dir(&self.sessions)
            .unwrap()
            .flatten()
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{pid}."))
                    && e.file_name().to_string_lossy().ends_with(".key")
            });
        !registry.exists() && !socket.exists() && !ledgers && !keys
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn accept_timeout(listener: &UnixListener, seconds: f64) -> Option<UnixStream> {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false).unwrap();
                conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                return Some(conn);
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
}

pub fn wait_timeout(child: &mut Child, seconds: f64) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status.code().unwrap_or(-1));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A second genuine native session: this test executable re-run as a
/// separate process that owns its own PID, socket, registry record, and
/// (optionally) key, until its stdin closes.
pub struct SessionProcess {
    pub child: Child,
    pub pid: i64,
}

pub const SESSION_ENV: &str = "SWITCHBOARD_TEST_SESSION";

impl SessionProcess {
    pub fn start(test_name: &str, root: &Path, record: &Obj, with_key: bool) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
            .env(
                SESSION_ENV,
                serde_json::to_string(&Value::Object(record.clone())).unwrap(),
            )
            .env("SWITCHBOARD_TEST_ROOT", root)
            .env("SWITCHBOARD_TEST_KEY", if with_key { "1" } else { "" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                reader.read_line(&mut line).unwrap() > 0,
                "session fixture ended"
            );
            if line.trim_end().ends_with("ready") {
                break;
            }
        }
        let pid = child.id() as i64;
        SessionProcess { child, pid }
    }

    pub fn finish(mut self) -> i32 {
        drop(self.child.stdin.take());
        wait_timeout(&mut self.child, 5.0).expect("session fixture exit")
    }
}

/// Body of the session process; a no-op unless this run is one.
pub fn session_process_main() {
    let Ok(record) = std::env::var(SESSION_ENV) else {
        return;
    };
    let root = PathBuf::from(std::env::var("SWITCHBOARD_TEST_ROOT").unwrap());
    let with_key = std::env::var("SWITCHBOARD_TEST_KEY").unwrap() == "1";
    let mut record: Obj = serde_json::from_str::<Value>(&record)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let pid = std::process::id() as i64;
    let path = root.join("cc-socks").join(format!("{pid}.sock"));
    let listener = UnixListener::bind(&path).unwrap();
    record.insert("pid".into(), Value::from(pid));
    record.insert("procStart".into(), Value::from(process_start(pid).unwrap()));
    record.insert("messagingSocketPath".into(), Value::from(s(&path)));
    let registry = root
        .join("config")
        .join("sessions")
        .join(format!("{pid}.json"));
    let key = root
        .join("config")
        .join("sessions")
        .join(format!("{pid}.{}.key", socket_digest(&s(&path))));
    write_new(&s(&registry), &record).unwrap();
    if with_key {
        let content = obj! {
            "peerToken" => token_hex16(),
            "procStart" => record["procStart"].clone(),
            "pidDomain" => record["pidDomain"].clone(),
        };
        write_new(&s(&key), &content).unwrap();
    }
    println!("ready");
    std::io::stdout().flush().unwrap();
    let mut rest = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut rest);
    drop(listener);
    let _ = std::fs::remove_file(&registry);
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&path);
    std::process::exit(0);
}
