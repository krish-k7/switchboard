//! One native helper child per remote chat: `switchboard helper` started
//! from this same binary, driven over stdin/stdout NDJSON.
//!
//! Commands are written by one writer thread per helper, in order, each
//! within three seconds, so a helper that stops reading its stdin never
//! blocks the bridge's main loop. A write that fails is reported back as
//! `Event::HelperWrite` with the command whose outcome is uncertain and the
//! ones never written. Closing never waits on the main loop either: helpers
//! get SIGTERM and a short grace, and stragglers finish in `Closing`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};

use super::queue::BoundedQueue;
use super::Event;
use crate::error::{Error, Kind, Result};
use crate::identity::{monotonic, uuid4_hex};
use crate::wirejson::dumps;

/// `(route, helper instance, bridge generation)`.
pub type Identity = (String, String, u64);

/// A helper write whose outcome is uncertain: the helper may or may not have
/// received the command (unlike a command never written).
pub fn write_unknown() -> Error {
    Error {
        kind: Kind::WriteUnknown,
        message: "helper IPC outcome uncertain".into(),
    }
}

/// Seconds a helper has to exit after SIGTERM, then after SIGKILL.
pub const TERM_WAIT: f64 = 8.0;
pub const KILL_WAIT: f64 = 3.0;
/// Seconds a close waits on the main loop before finishing in the background.
pub const CLOSE_GRACE: f64 = 0.2;
/// Commands (and their bytes) a helper may have waiting to be written.
const PIPE_COMMANDS: usize = 64;
const PIPE_BYTES: usize = 256 * 1024;

/// A command that did not certainly reach its helper: its `op` and `id`.
#[derive(Clone, Debug, PartialEq)]
pub struct Unsent {
    pub op: String,
    pub id: Option<String>,
}

impl Unsent {
    fn of(message: &Map<String, Value>) -> Unsent {
        let text = |key: &str| message.get(key).and_then(Value::as_str).map(str::to_string);
        Unsent {
            op: text("op").unwrap_or_default(),
            id: text("id"),
        }
    }
}

/// Commands waiting for the writer thread.
#[derive(Default)]
struct PipeState {
    queue: VecDeque<(Vec<u8>, Unsent)>,
    bytes: usize,
    closed: bool,
    failed: bool,
}

#[derive(Default)]
struct Pipe {
    state: Mutex<PipeState>,
    ready: Condvar,
}

impl Pipe {
    fn push(&self, payload: Vec<u8>, unsent: Unsent) -> Result<()> {
        let mut state = self.state.lock().expect("pipe");
        if state.failed || state.closed {
            return Err(Error::os("native helper IPC closed"));
        }
        if state.queue.len() >= PIPE_COMMANDS || state.bytes + payload.len() > PIPE_BYTES {
            // Nothing of this command was written: the helper is not reading.
            return Err(Error::os("native helper IPC backlog full"));
        }
        state.bytes += payload.len();
        state.queue.push_back((payload, unsent));
        self.ready.notify_one();
        Ok(())
    }

    fn close(&self) {
        self.state.lock().expect("pipe").closed = true;
        self.ready.notify_all();
    }
}

pub struct Helper {
    pub route: String,
    pub identity: Identity,
    pub name: String,
    pub remote_status: String,
    pub socket_dir: String,
    pub sessions: Vec<String>,
    pub revision: i64,
    pub configured_revision: i64,
    pub pid: u32,
    child: Option<Arc<Mutex<Child>>>,
    pipe: Option<Arc<Pipe>>,
    reader_done: Arc<(Mutex<bool>, Condvar)>,
    fake: Option<Arc<Mutex<FakeLog>>>,
}

/// What a test helper (no process) saw: every IPC command, closes, and a
/// failure to return from `deliver`.
#[derive(Default)]
pub struct FakeLog {
    pub delivered: Vec<Map<String, Value>>,
    pub closed: usize,
    pub exited: bool,
    pub fail: Option<Error>,
}

/// SIGTERM the child unless it has already been reaped.
fn terminate(child: &Mutex<Child>) {
    let mut child = child.lock().expect("helper child");
    if matches!(child.try_wait(), Ok(None)) {
        // SAFETY: the child is not reaped, so its PID cannot be reused.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    }
}

impl Helper {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        program: &str,
        runtime_config: &str,
        sessions: &[String],
        route: &str,
        name: &str,
        socket_dir: &str,
        events: Arc<BoundedQueue<Event>>,
        generation: u64,
        remote_status: &str,
    ) -> Result<Helper> {
        let identity: Identity = (route.to_string(), uuid4_hex(), generation);
        let mut command = Command::new(program);
        command
            .args([
                "helper",
                "--config",
                runtime_config,
                "--route",
                route,
                "--name",
                name,
            ])
            .args(["--socket-dir", socket_dir, "--status", remote_status])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear()
            .envs(std::env::vars_os().filter(|(key, _)| {
                let key = key.to_string_lossy();
                !(key.starts_with("CLAUDE_")
                    || key.starts_with("ANTHROPIC_")
                    || key.starts_with("OPENAI_"))
            }));
        let mut helper = Helper::start(command, identity, events)?;
        helper.name = name.to_string();
        helper.remote_status = remote_status.to_string();
        helper.socket_dir = socket_dir.to_string();
        let mut sorted = sessions.to_vec();
        sorted.sort();
        helper.sessions = sorted;
        Ok(helper)
    }

    /// Start `command` as a helper process with its reader and writer threads.
    pub fn start(
        mut command: Command,
        identity: Identity,
        events: Arc<BoundedQueue<Event>>,
    ) -> Result<Helper> {
        let program = command.get_program().to_owned();
        let mut child = command
            .spawn()
            .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&program))))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));
        let reader_done = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_child = child.clone();
        let reader_identity = identity.clone();
        let reader_events = events.clone();
        let done = reader_done.clone();
        let spawned = std::thread::Builder::new()
            .name("switchboard-helper-reader".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                read_events(stdout, &reader_child, &reader_identity, &reader_events, pid);
                *done.0.lock().expect("reader") = true;
                done.1.notify_all();
            });
        if let Err(exc) = spawned {
            terminate(&child);
            return Err(Error::from_io(&exc, None));
        }
        let pipe = match stdin {
            Some(stdin) => {
                let pipe = Arc::new(Pipe::default());
                let (writer_pipe, writer_child, writer_identity) =
                    (pipe.clone(), child.clone(), identity.clone());
                let spawned = std::thread::Builder::new()
                    .name("switchboard-helper-writer".into())
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        write_commands(stdin, &writer_pipe, &writer_child, writer_identity, &events)
                    });
                if let Err(exc) = spawned {
                    terminate(&child);
                    return Err(Error::from_io(&exc, None));
                }
                Some(pipe)
            }
            None => None,
        };
        Ok(Helper {
            route: identity.0.clone(),
            identity,
            name: String::new(),
            remote_status: String::new(),
            socket_dir: String::new(),
            sessions: Vec::new(),
            revision: 0,
            configured_revision: 0,
            pid,
            child: Some(child),
            pipe,
            reader_done,
            fake: None,
        })
    }

    /// A helper with no process, for tests; its log records what it was sent.
    pub fn fake(
        identity: Identity,
        name: &str,
        socket_dir: &str,
        sessions: &[String],
        remote_status: &str,
    ) -> (Helper, Arc<Mutex<FakeLog>>) {
        let log = Arc::new(Mutex::new(FakeLog::default()));
        let mut sorted = sessions.to_vec();
        sorted.sort();
        let helper = Helper {
            route: identity.0.clone(),
            identity,
            name: name.to_string(),
            remote_status: remote_status.to_string(),
            socket_dir: socket_dir.to_string(),
            sessions: sorted,
            revision: 0,
            configured_revision: 0,
            pid: 0,
            child: None,
            pipe: None,
            reader_done: Arc::new((Mutex::new(true), Condvar::new())),
            fake: Some(log.clone()),
        };
        (helper, log)
    }

    /// Whether the helper process has not exited yet.
    pub fn running(&self) -> bool {
        match (&self.child, &self.fake) {
            (Some(child), _) => matches!(child.lock().expect("helper child").try_wait(), Ok(None)),
            (None, Some(log)) => !log.lock().expect("fake").exited,
            (None, None) => false,
        }
    }

    pub fn terminate(&self) {
        if let Some(child) = &self.child {
            terminate(child);
        }
    }

    /// Queue one IPC command for the writer thread, which writes it within
    /// three seconds after the commands before it. Errors, none of which
    /// wrote anything: `Os` when the helper had exited, its writer failed,
    /// or its backlog is full; `Value` when too large. A later write
    /// failure arrives as `Event::HelperWrite`. Test helpers record the
    /// command and return their configured result at once.
    pub fn deliver(&mut self, message: &Map<String, Value>) -> Result<()> {
        if !self.running() {
            return Err(Error::os("native helper exited"));
        }
        if let Some(log) = &self.fake {
            let mut log = log.lock().expect("fake");
            log.delivered.push(message.clone());
            return match &log.fail {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            };
        }
        let mut payload = dumps(&Value::Object(message.clone())).into_bytes();
        payload.push(b'\n');
        if payload.len() > 65536 {
            return Err(Error::value("helper IPC too large"));
        }
        match &self.pipe {
            Some(pipe) => pipe.push(payload, Unsent::of(message)),
            None => Err(write_unknown()),
        }
    }

    /// Stop writing and send SIGTERM. Commands not yet written are dropped:
    /// the helper is going away.
    fn begin_close(&self) {
        if let Some(pipe) = &self.pipe {
            pipe.close();
        }
        self.terminate();
    }

    fn kill(&self) {
        if let Some(child) = &self.child {
            let mut child = child.lock().expect("helper child");
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
        }
    }

    fn reader_finished(&self, timeout: f64) -> bool {
        let (lock, changed) = &*self.reader_done;
        let guard = lock.lock().expect("reader");
        let timeout = Duration::from_secs_f64(timeout.max(0.0));
        !changed
            .wait_timeout_while(guard, timeout, |done| !*done)
            .expect("reader")
            .1
            .timed_out()
    }

    /// Close this helper: SIGTERM, then (on the caller's thread) at most
    /// `CLOSE_GRACE` seconds for it to exit. A helper still running is
    /// returned to finish in the background (`Closing::reap`).
    pub fn close(self) -> Option<Closing> {
        if let Some(log) = &self.fake {
            log.lock().expect("fake").closed += 1;
            return None;
        }
        self.begin_close();
        let terminated = monotonic();
        if wait_all(&[&self], terminated + CLOSE_GRACE) {
            self.reader_finished(0.05);
            return None;
        }
        Some(Closing {
            helper: self,
            terminated,
            killed: false,
        })
    }
}

/// A helper sent SIGTERM that had not exited within the close grace.
pub struct Closing {
    helper: Helper,
    terminated: f64,
    killed: bool,
}

impl Closing {
    /// Advance without blocking: SIGKILL after `TERM_WAIT`, and give up
    /// (leaving the reader to end on its own) after `KILL_WAIT` more. True
    /// once nothing is left to do.
    pub fn reap(&mut self, now: f64) -> bool {
        if !self.helper.running() {
            return true;
        }
        if !self.killed && now >= self.terminated + TERM_WAIT {
            self.helper.kill();
            self.killed = true;
        }
        now >= self.terminated + TERM_WAIT + KILL_WAIT
    }

    /// When `reap` next has something to do.
    pub fn next_deadline(&self) -> f64 {
        if self.killed {
            self.terminated + TERM_WAIT + KILL_WAIT
        } else {
            self.terminated + TERM_WAIT
        }
    }
}

/// Close several helpers at once: SIGTERM to all, then one `CLOSE_GRACE`
/// for all of them; those still running are returned.
pub fn close_many(helpers: Vec<Helper>) -> Vec<Closing> {
    let mut real = Vec::new();
    for helper in helpers {
        if let Some(log) = &helper.fake {
            log.lock().expect("fake").closed += 1;
            continue;
        }
        helper.begin_close();
        real.push(helper);
    }
    let terminated = monotonic();
    let refs: Vec<&Helper> = real.iter().collect();
    wait_all(&refs, terminated + CLOSE_GRACE);
    real.into_iter()
        .filter(|helper| helper.running())
        .map(|helper| Closing {
            helper,
            terminated,
            killed: false,
        })
        .collect()
}

/// Close every helper with one deadline for the whole group: SIGTERM to
/// all, `TERM_WAIT` seconds for all of them (counted from the earliest
/// SIGTERM), SIGKILL to the rest, `KILL_WAIT` seconds more, and one second
/// in total for their readers. Stuck helpers never add up.
pub fn close_group(helpers: Vec<Helper>, closing: Vec<Closing>) {
    close_group_within(helpers, closing, TERM_WAIT, KILL_WAIT);
}

fn close_group_within(helpers: Vec<Helper>, closing: Vec<Closing>, term_wait: f64, kill_wait: f64) {
    let now = monotonic();
    let mut started = now;
    let mut all: Vec<Helper> = Vec::new();
    for helper in helpers {
        if let Some(log) = &helper.fake {
            log.lock().expect("fake").closed += 1;
            continue;
        }
        helper.begin_close();
        all.push(helper);
    }
    for entry in closing {
        started = started.min(entry.terminated);
        all.push(entry.helper);
    }
    if all.is_empty() {
        return;
    }
    let refs: Vec<&Helper> = all.iter().collect();
    if !wait_all(&refs, started + term_wait) {
        for helper in &all {
            helper.kill();
        }
        wait_all(&refs, monotonic() + kill_wait);
    }
    let readers = monotonic() + 1.0;
    for helper in &all {
        helper.reader_finished(readers - monotonic());
    }
}

/// Wait until none of `helpers` is running, or `deadline`; true if none is.
fn wait_all(helpers: &[&Helper], deadline: f64) -> bool {
    let mut delay = 0.0005;
    loop {
        if helpers.iter().all(|helper| !helper.running()) {
            return true;
        }
        let left = deadline - monotonic();
        if left <= 0.0 {
            return false;
        }
        delay = (delay * 2.0_f64).min(left).min(0.05);
        std::thread::sleep(Duration::from_secs_f64(delay));
    }
}

/// Write queued commands in order, each within three seconds. On the first
/// failure, terminate the helper and report the command whose outcome is
/// uncertain and every command never written.
fn write_commands(
    mut stdin: ChildStdin,
    pipe: &Pipe,
    child: &Mutex<Child>,
    identity: Identity,
    events: &BoundedQueue<Event>,
) {
    let fd = stdin.as_raw_fd();
    crate::net::set_nonblocking(fd, true);
    loop {
        let (payload, unsent) = {
            let mut state = pipe.state.lock().expect("pipe");
            loop {
                if let Some(item) = state.queue.pop_front() {
                    state.bytes -= item.0.len();
                    break item;
                }
                if state.closed {
                    return;
                }
                state = pipe.ready.wait(state).expect("pipe");
            }
        };
        if write_within(&mut stdin, fd, &payload, 3.0) {
            continue;
        }
        let unwritten: Vec<Unsent> = {
            let mut state = pipe.state.lock().expect("pipe");
            state.failed = true;
            state.bytes = 0;
            let closed = state.closed;
            let rest = state.queue.drain(..).map(|(_, unsent)| unsent).collect();
            if closed {
                return; // Closing anyway: nothing to report.
            }
            rest
        };
        terminate(child);
        let _ = events.put_timeout(
            Event::HelperWrite(identity, unsent, unwritten),
            Duration::from_secs(1),
        );
        return;
    }
}

/// Write all of `payload` before `timeout` seconds pass.
fn write_within(stdin: &mut ChildStdin, fd: i32, payload: &[u8], timeout: f64) -> bool {
    let deadline = monotonic() + timeout;
    let mut rest = payload;
    while !rest.is_empty() {
        let remaining = deadline - monotonic();
        let mut poll = [crate::net::pollfd(fd, libc::POLLOUT)];
        match crate::net::poll_fds(&mut poll, Some(remaining.max(0.0))) {
            Ok(true) if remaining > 0.0 => {}
            _ => return false,
        }
        match stdin.write(rest) {
            Ok(count) => rest = &rest[count..],
            Err(exc) if exc.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(exc) if exc.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

fn read_events(
    stdout: std::process::ChildStdout,
    child: &Mutex<Child>,
    identity: &Identity,
    events: &BoundedQueue<Event>,
    pid: u32,
) {
    let mut reader = BufReader::with_capacity(16 * 1024, stdout);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(65537).read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => {
                terminate(child);
                break;
            }
        }
        let value = match crate::helper::decode(&line) {
            Ok(value) => value,
            Err(_) => {
                terminate(child);
                break;
            }
        };
        let item = Event::Helper(identity.clone(), value);
        if events.put_timeout(item, Duration::from_secs(1)).is_err() {
            terminate(child);
            break;
        }
    }
    let _ = events.put_timeout(
        Event::HelperExit(identity.clone(), pid),
        Duration::from_secs(1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A helper process that never reads its stdin and ignores SIGTERM.
    fn stuck(events: &Arc<BoundedQueue<Event>>) -> Helper {
        let mark = format!("/tmp/sbh-{}", uuid4_hex());
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; : > \"$0\"; exec sleep 60", &mark])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let identity = ("remote/x".to_string(), uuid4_hex(), 1);
        let helper = Helper::start(command, identity, events.clone()).unwrap();
        // Signal it only once its trap is installed.
        let deadline = monotonic() + 10.0;
        while std::fs::remove_file(&mark).is_err() {
            assert!(monotonic() < deadline, "stuck helper did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        helper
    }

    fn deliver(id: &str) -> Map<String, Value> {
        let mut command = Map::new();
        command.insert("op".into(), Value::from("deliver"));
        command.insert("id".into(), Value::from(id));
        command.insert("text".into(), Value::from("x".repeat(3000)));
        command
    }

    #[test]
    fn a_helper_that_stops_reading_never_blocks_the_caller() {
        let events = Arc::new(BoundedQueue::new(16));
        let mut helper = stuck(&events);
        let started = monotonic();
        // Far more than a pipe holds: before, the caller blocked three
        // seconds on the first command that did not fit.
        for index in 0..60 {
            helper.deliver(&deliver(&index.to_string())).unwrap();
        }
        assert!(monotonic() - started < 0.5, "{}", monotonic() - started);
        let failed = loop {
            match events.get_timeout(Duration::from_secs(6)) {
                Some(Event::HelperWrite(identity, uncertain, unwritten)) => {
                    break (identity, uncertain, unwritten)
                }
                Some(_) => continue,
                None => panic!("no write failure reported"),
            }
        };
        assert_eq!(failed.0, helper.identity);
        assert_eq!(failed.1.op, "deliver");
        let first: usize = failed.1.id.as_deref().unwrap().parse().unwrap();
        // Every later command is reported as never written, in order.
        let rest: Vec<String> = failed.2.iter().map(|u| u.id.clone().unwrap()).collect();
        let expected: Vec<String> = (first + 1..60).map(|i| i.to_string()).collect();
        assert_eq!(rest, expected);
        assert!(helper.deliver(&deliver("late")).unwrap_err().is_os());
        close_group_within(vec![helper], Vec::new(), 0.2, 2.0);
    }

    #[test]
    fn a_full_backlog_is_refused_without_writing() {
        let events = Arc::new(BoundedQueue::new(16));
        let mut helper = stuck(&events);
        let mut refused = None;
        for index in 0..(PIPE_COMMANDS * 4) {
            if let Err(error) = helper.deliver(&deliver(&index.to_string())) {
                refused = Some(error);
                break;
            }
        }
        assert!(refused.is_some_and(|e| e.is_os()));
        close_group_within(vec![helper], Vec::new(), 0.2, 2.0);
    }

    #[test]
    fn stuck_helpers_share_one_shutdown_deadline() {
        let events = Arc::new(BoundedQueue::new(64));
        let helpers: Vec<Helper> = (0..4).map(|_| stuck(&events)).collect();
        let started = monotonic();
        let closing = close_many(helpers);
        assert_eq!(closing.len(), 4);
        // Before, each helper waited out its own deadline in turn.
        close_group_within(Vec::new(), closing, 1.0, 2.0);
        let took = monotonic() - started;
        assert!(took < 2.5, "group close took {took}");
    }

    #[test]
    fn a_closing_helper_is_killed_after_its_deadline_without_blocking() {
        let events = Arc::new(BoundedQueue::new(16));
        let helper = stuck(&events);
        let started = monotonic();
        let mut closing = helper.close().expect("still running after the grace");
        assert!(monotonic() - started < CLOSE_GRACE + 0.2);
        assert!(!closing.reap(monotonic()));
        assert!(closing.next_deadline() > monotonic() + TERM_WAIT - 1.0);
        // At its deadline the next reap kills it, and it is soon gone.
        assert!(!closing.reap(closing.next_deadline()) || !closing.helper.running());
        assert!(wait_all(&[&closing.helper], monotonic() + 2.0));
        assert!(closing.reap(monotonic()));
    }
}
