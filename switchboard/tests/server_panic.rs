//! A panic in one request handler ends only that handler: its connection is
//! closed, its connection slot and its machine's request counters are
//! released, any database transaction it left open is rolled back, and the
//! server keeps answering. Needs a build with the `fault-injection` feature,
//! which adds request paths that panic; run it in the release profile to
//! check the shipped panic strategy:
//!
//! ```sh
//! cargo test --release --features fault-injection --test server_panic
//! ```
#![cfg(feature = "fault-injection")]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FINGERPRINT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let template = std::ffi::CString::new("/tmp/sbp-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
        raw.pop();
        TempDir(std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_private(path: &Path, data: &[u8]) {
    std::fs::write(path, data).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

struct Server {
    child: Child,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &TempDir) -> (Server, PathBuf) {
    let run = dir.path("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o750)).unwrap();
    // The socket directory must carry the server's own group (on macOS a new
    // directory takes its parent's group instead).
    // SAFETY: getegid cannot fail.
    std::os::unix::fs::chown(&run, None, Some(unsafe { libc::getegid() })).unwrap();
    let socket = run.join("server.sock");
    write_private(
        &dir.path("peers.json"),
        format!(
            r#"{{"{FINGERPRINT}": {{"machine": "member", "owner": "test", "enabled": true, "admin": false}}}}"#
        )
        .as_bytes(),
    );
    write_private(
        &dir.path("server.json"),
        format!(
            r#"{{"unix_socket": "{}", "db": "state/messages.db", "peers_file": "peers.json"}}"#,
            socket.display()
        )
        .as_bytes(),
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_switchboard"));
    command
        .args(["server", "--config"])
        .arg(dir.path("server.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: signal(2) is async-signal-safe; runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            sink.lock().unwrap().push(line.unwrap_or_default());
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            Instant::now() < deadline,
            "server did not listen: {:?}",
            lines.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (
        Server {
            child,
            stderr: lines,
        },
        socket,
    )
}

/// One request; the whole reply (empty when the connection closed without one).
fn request(socket: &Path, path: &str) -> String {
    exchange(socket, "GET", path, None)
}

fn exchange(socket: &Path, method: &str, path: &str, body: Option<&str>) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let body_headers = match body {
        Some(body) => format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ),
        None => String::new(),
    };
    let text = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nX-Switchboard-Client-Fingerprint: {FINGERPRINT}\r\nX-Switchboard-Client-Address: 127.0.0.1\r\n{body_headers}\r\n{}",
        body.unwrap_or("")
    );
    stream.write_all(text.as_bytes()).unwrap();
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    String::from_utf8_lossy(&reply).into_owned()
}

fn status(reply: &str) -> Option<u16> {
    reply.split(' ').nth(1).and_then(|s| s.parse().ok())
}

#[test]
fn a_panicking_handler_leaves_the_server_running() {
    let dir = TempDir::new();
    let (mut server, socket) = start(&dir);
    assert_eq!(status(&request(&socket, "/health")), Some(200));

    // More panics than connection slots (128): a slot that a panic leaked
    // would leave none, and new connections would be closed unanswered.
    for _ in 0..140 {
        assert_eq!(request(&socket, "/__panic"), "");
    }
    // More panics than one machine's concurrent request limit (16): a leaked
    // count would leave the machine rate limited (429).
    for _ in 0..20 {
        assert_eq!(request(&socket, "/__panic_entered"), "");
    }
    let reply = request(&socket, "/health");
    assert_eq!(status(&reply), Some(200), "{reply}");
    assert!(reply.contains("\"machine\": \"member\""), "{reply}");
    assert!(server.child.try_wait().unwrap().is_none());

    // Every panic was reported on stderr (the reader may lag a little).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stderr = server.stderr.lock().unwrap().join("\n");
        let before = stderr.matches("forced panic before authentication").count();
        let counted = stderr.matches("forced panic in a counted request").count();
        if (before, counted) == (140, 20) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "saw {before} and {counted} panics"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_panic_inside_a_transaction_leaves_the_database_consistent() {
    let dir = TempDir::new();
    let (server, socket) = start(&dir);
    // The handler inserts a row inside a transaction, then panics holding the
    // server's lock.
    assert_eq!(request(&socket, "/__panic_in_transaction"), "");
    // The next request that commits (a registration expires and commits)
    // must not commit the interrupted transaction's row.
    let reply = exchange(&socket, "POST", "/v1/register", Some(r#"{"sessions": []}"#));
    assert_eq!(status(&reply), Some(200), "{reply}");
    let db = rusqlite::Connection::open(dir.path("state/messages.db")).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM messages WHERE id='fault-injection'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "the interrupted transaction was committed");
    // And the server holds no write lock: another connection can write.
    db.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .expect("the server still holds a write transaction");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server
        .stderr
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("forced panic in a database transaction"))
    {
        assert!(Instant::now() < deadline, "the panic was not reported");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(status(&request(&socket, "/health")), Some(200));
}
