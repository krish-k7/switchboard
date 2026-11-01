//! What the server commands ask the system: the service's state from
//! systemd, the server's events in the journal, and the sockets it listens
//! on. These are Linux-with-systemd questions; elsewhere each says it is
//! unavailable and nothing is run. The parsing is kept apart from the
//! queries so it can be tested with captured output.
//!
//! Every program is run with a fixed `PATH` and the C locale, as the
//! installer runs its own, since these commands usually run as root.

use std::collections::HashMap;
use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::install::{PATH, UNIT_NAME};

/// Whether this machine runs systemd (the only service manager the server
/// is installed under).
pub fn has_systemd() -> bool {
    cfg!(target_os = "linux") && std::path::Path::new("/run/systemd/system").is_dir()
}

/// Run a system program; its exit status, standard output, and standard
/// error. `None` without systemd (nothing is run) or when it can't start.
fn output(program: &str, arguments: &[&str]) -> Option<(i32, String, String)> {
    if !has_systemd() {
        return None;
    }
    let output = Command::new(program)
        .args(arguments)
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .env("SYSTEMD_PAGER", "")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Seconds since the epoch, now.
pub fn now() -> f64 {
    crate::fmtutil::time_now()
}

/// Seconds on the clock systemd's monotonic timestamps use, now.
fn monotonic_now() -> f64 {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into a caller-owned timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut spec) };
    spec.tv_sec as f64 + spec.tv_nsec as f64 / 1e9
}

// The service ------------------------------------------------------------------

/// The properties `systemctl show` is asked for.
const PROPERTIES: [&str; 9] = [
    "LoadState",
    "ActiveState",
    "SubState",
    "UnitFileState",
    "MainPID",
    "ActiveEnterTimestampMonotonic",
    "InactiveEnterTimestampMonotonic",
    "Result",
    "ExecMainStatus",
];

/// The service as systemd reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Service {
    pub load: String,
    pub active: String,
    pub sub: String,
    pub unit_file: String,
    pub pid: Option<u32>,
    /// When it last became active (seconds since the epoch).
    pub active_since: Option<f64>,
    /// When it last stopped or failed.
    pub inactive_since: Option<f64>,
    pub result: String,
    pub exit_status: Option<i32>,
}

/// What the service is doing, in the words `server status` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    NotInstalled,
    Running,
    Starting,
    Restarting,
    Stopping,
    Stopped,
    Failed,
}

impl State {
    /// The stable name in `--json`.
    pub fn name(self) -> &'static str {
        match self {
            State::NotInstalled => "not_installed",
            State::Running => "running",
            State::Starting => "starting",
            State::Restarting => "restarting",
            State::Stopping => "stopping",
            State::Stopped => "stopped",
            State::Failed => "failed",
        }
    }
}

impl Service {
    pub fn state(&self) -> State {
        if self.load == "not-found" {
            return State::NotInstalled;
        }
        match (self.active.as_str(), self.sub.as_str()) {
            ("active", _) | ("reloading", _) => State::Running,
            ("activating", "auto-restart") => State::Restarting,
            ("activating", _) => State::Starting,
            ("deactivating", _) => State::Stopping,
            ("failed", _) => State::Failed,
            _ => State::Stopped,
        }
    }

    pub fn enabled(&self) -> bool {
        matches!(
            self.unit_file.as_str(),
            "enabled" | "enabled-runtime" | "alias" | "static" | "linked"
        )
    }

    /// When it entered its current state, if systemd recorded it.
    pub fn since(&self) -> Option<f64> {
        match self.state() {
            State::Running | State::Stopping => self.active_since,
            State::NotInstalled => None,
            _ => self.inactive_since,
        }
    }
}

/// Parse `systemctl show -p ...` output; monotonic timestamps become epoch
/// seconds through the two clocks' readings now.
pub fn parse_show(text: &str, now: f64, monotonic: f64) -> Service {
    let fields: HashMap<&str, &str> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let text = |key: &str| fields.get(key).copied().unwrap_or("").to_string();
    let stamp = |key: &str| {
        fields
            .get(key)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|micros| *micros > 0)
            .map(|micros| now - (monotonic - micros as f64 / 1e6))
    };
    Service {
        load: text("LoadState"),
        active: text("ActiveState"),
        sub: text("SubState"),
        unit_file: text("UnitFileState"),
        pid: fields
            .get("MainPID")
            .and_then(|value| value.parse().ok())
            .filter(|pid| *pid > 0),
        active_since: stamp("ActiveEnterTimestampMonotonic"),
        inactive_since: stamp("InactiveEnterTimestampMonotonic"),
        result: text("Result"),
        exit_status: fields.get("ExecMainStatus").and_then(|v| v.parse().ok()),
    }
}

/// The server's service, or `None` without systemd.
pub fn service() -> Option<Service> {
    let mut arguments = vec!["show", UNIT_NAME];
    for property in PROPERTIES {
        arguments.extend(["-p", property]);
    }
    let (code, stdout, _) = output("systemctl", &arguments)?;
    (code == 0).then(|| parse_show(&stdout, now(), monotonic_now()))
}

// The journal ------------------------------------------------------------------

/// How much an event matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

/// One line from the server's journal, read for people.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Seconds since the epoch.
    pub time: f64,
    /// `server` (the program's own output) or `systemd` (about the service).
    pub source: &'static str,
    pub event: String,
    pub detail: String,
    pub level: Level,
    /// The line as it was logged.
    pub message: String,
}

/// A JSON field's text: the server writes strings, numbers, and booleans.
fn field_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// The detail people read for one of the server's own events.
fn event_detail(event: &str, fields: &serde_json::Map<String, Value>) -> String {
    let get = |key: &str| fields.get(key).map(field_text).unwrap_or_default();
    match event {
        "private_certificate" => {
            // The start of the pin, like an invite's ID; `server status`
            // and the logged line have all of it.
            let full = get("sha256");
            let pin = match full.get(..12) {
                Some(start) if full.len() > 12 => format!("{start}…"),
                _ => full,
            };
            if fields.get("created") == Some(&Value::Bool(true)) {
                format!("made a new certificate; pin {pin}")
            } else {
                format!("pin {pin}")
            }
        }
        "invite_redeemed" => format!("{} joined with invite {}", get("machine"), get("invite")),
        "certificate_obtained" => format!("for {}", get("detail")),
        "registry_group_not_kept" => format!("group {}", get("gid")),
        _ => {
            if let (Some(detail), 2) = (fields.get("detail"), fields.len()) {
                return field_text(detail);
            }
            fields
                .iter()
                .filter(|(key, _)| key.as_str() != "event")
                .map(|(key, value)| format!("{key}={}", field_text(value)))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// The event, detail, and level of one line the server program wrote.
fn classify_server(message: &str) -> (String, String, Level) {
    if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(message) {
        if let Some(Value::String(event)) = fields.get("event") {
            let level = if event.ends_with("_error") || event.ends_with("_unavailable") {
                Level::Error
            } else if event.ends_with("_not_kept") {
                Level::Warning
            } else {
                Level::Info
            };
            return (event.clone(), event_detail(event, &fields), level);
        }
    }
    let prefixed = [
        ("switchboard server: ", "server_error"),
        (
            "Exception occurred during processing of request: ",
            "request_error",
        ),
    ];
    for (prefix, event) in prefixed {
        if let Some(rest) = message.strip_prefix(prefix) {
            return (event.into(), rest.into(), Level::Error);
        }
    }
    if message.starts_with("thread '") && message.contains("panicked at") {
        return ("panic".into(), message.into(), Level::Error);
    }
    ("output".into(), message.into(), Level::Info)
}

/// The event, detail, and level of one of systemd's lines about the unit.
fn classify_systemd(message: &str, priority: Option<u8>) -> (String, String, Level) {
    let text = message
        .strip_prefix(&format!("{UNIT_NAME}: "))
        .unwrap_or(message);
    let with = |event: &str, detail: &str, level| (event.to_string(), detail.to_string(), level);
    if text.starts_with("Started ") {
        return with("started", "", Level::Info);
    }
    if text.starts_with("Starting ") {
        return with("starting", "", Level::Info);
    }
    if text.starts_with("Stopping ") {
        return with("stopping", "", Level::Info);
    }
    if text.starts_with("Stopped ") {
        return with("stopped", "", Level::Info);
    }
    if text.starts_with("Deactivated successfully") {
        return with("exited", "cleanly", Level::Info);
    }
    if let Some(rest) = text.strip_prefix("Main process exited, ") {
        let clean = rest.contains("status=0/SUCCESS") || rest.contains("status=15/TERM");
        let level = if clean { Level::Info } else { Level::Error };
        return with("exited", rest, level);
    }
    if let Some(rest) = text.strip_prefix("Failed with result ") {
        let result = rest.trim_end_matches('.').trim_matches('\'');
        return with("failed", &format!("result {result}"), Level::Error);
    }
    if text.starts_with("Start request repeated too quickly") {
        return with(
            "failed",
            "restarted too often; systemd gave up",
            Level::Error,
        );
    }
    if let Some(rest) = text.strip_prefix("Scheduled restart job, restart counter is at ") {
        let count = rest.trim_end_matches('.');
        return with("restarting", &format!("attempt {count}"), Level::Warning);
    }
    if let Some(rest) = text.strip_prefix("Consumed ") {
        return with("resources", &format!("consumed {rest}"), Level::Info);
    }
    let level = match priority {
        Some(0..=3) => Level::Error,
        Some(4) => Level::Warning,
        _ => Level::Info,
    };
    with("service", text, level)
}

/// One `journalctl --output=json` line as an event; `None` for a line that
/// isn't a journal entry.
pub fn parse_entry(line: &str) -> Option<Event> {
    let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("");
    let time = text("__REALTIME_TIMESTAMP").parse::<f64>().ok()? / 1e6;
    // The journal stores a message that isn't valid UTF-8 as its bytes.
    let message = match entry.get("MESSAGE") {
        Some(Value::String(message)) => message.clone(),
        Some(Value::Array(bytes)) => String::from_utf8_lossy(
            &bytes
                .iter()
                .filter_map(|b| b.as_u64().map(|b| b as u8))
                .collect::<Vec<_>>(),
        )
        .into_owned(),
        _ => String::new(),
    };
    let message = message.trim_end().to_string();
    let systemd = text("_PID") == "1" || text("SYSLOG_IDENTIFIER") == "systemd";
    let priority = text("PRIORITY").parse::<u8>().ok();
    let (event, detail, level) = if systemd {
        classify_systemd(&message, priority)
    } else {
        classify_server(&message)
    };
    Some(Event {
        time,
        source: if systemd { "systemd" } else { "server" },
        event,
        detail,
        level,
        message,
    })
}

/// Whether this process can read the system journal: root, or a member of a
/// group journald lets read it.
pub fn can_read_journal() -> bool {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    let mut groups = vec![0 as libc::gid_t; 256];
    // SAFETY: getgroups writes at most groups.len() entries.
    let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    groups.truncate(count.max(0) as usize);
    // SAFETY: getegid cannot fail.
    groups.push(unsafe { libc::getegid() });
    ["systemd-journal", "adm", "wheel"]
        .iter()
        .filter_map(|name| super::install::group(name))
        .any(|gid| groups.contains(&gid))
}

/// The `journalctl` arguments for the server's unit.
pub fn journal_arguments(lines: usize, since: Option<&str>, follow: bool) -> Vec<String> {
    let mut arguments = vec![
        format!("--unit={UNIT_NAME}"),
        "--output=json".to_string(),
        "--no-pager".to_string(),
        format!("--lines={lines}"),
    ];
    if let Some(since) = since {
        arguments.push(format!("--since={since}"));
    }
    if follow {
        arguments.push("--follow".to_string());
    }
    arguments
}

/// The server's events, oldest first: at most `lines`, from `since`.
pub fn read_journal(lines: usize, since: Option<&str>) -> Result<Vec<Event>, String> {
    let arguments = journal_arguments(lines, since, false);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let (code, stdout, stderr) =
        output("journalctl", &arguments).ok_or("journalctl isn't available on this machine")?;
    if code != 0 {
        let first = stderr.lines().next().unwrap_or("").trim();
        return Err(format!("journalctl failed: {first}"));
    }
    Ok(stdout.lines().filter_map(parse_entry).collect())
}

/// Print the server's events as they are logged (after the last `lines`,
/// from `since`) until journalctl ends or the reader is interrupted.
pub fn follow_journal(
    lines: usize,
    since: Option<&str>,
    each: impl FnMut(Event) -> bool,
) -> Result<(), String> {
    if !has_systemd() {
        return Err("journalctl isn't available on this machine".into());
    }
    let mut command = Command::new("journalctl");
    command
        .args(journal_arguments(lines, since, true))
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .env("SYSTEMD_PAGER", "");
    // journalctl follows forever: it ends with this process, however this
    // process ends (Ctrl-C reaches both; `timeout` or `kill` only this one).
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe; runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    follow_command(command, 1, each)
}

/// Run `command`, which prints journal entries until it is stopped, and
/// pass each to `each`. It is stopped when `each` returns false (its output
/// couldn't be written) or when the reader of descriptor `watch` is gone,
/// checked even while nothing new is logged; either way that is a normal
/// end, not an error.
pub fn follow_command(
    mut command: Command,
    watch: std::os::fd::RawFd,
    mut each: impl FnMut(Event) -> bool,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .map_err(|exc| format!("journalctl: {exc}"))?;
    let stdout = child.stdout.take().ok_or("journalctl: no output")?;
    let child = Arc::new(Mutex::new(child));
    let finished = Arc::new(AtomicBool::new(false));
    let gone = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (child, finished, gone) = (child.clone(), finished.clone(), gone.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if crate::cli::reader_gone(watch) {
                    gone.store(true, Ordering::SeqCst);
                    // Killing a child already waited for does nothing.
                    let _ = child.lock().map(|mut child| child.kill());
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        })
    };
    let mut stopped = false;
    for line in std::io::BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Some(event) = parse_entry(&line) {
            if !each(event) {
                stopped = true;
                break;
            }
        }
    }
    let status = {
        let mut child = child.lock().map_err(|_| "journalctl: lost".to_string())?;
        if stopped {
            let _ = child.kill();
        }
        let status = child.wait();
        finished.store(true, Ordering::SeqCst);
        status
    };
    let _ = watcher.join();
    let status = status.map_err(|exc| format!("journalctl: {exc}"))?;
    if stopped || gone.load(Ordering::SeqCst) || status.success() || status.code().is_none() {
        Ok(())
    } else {
        Err(format!("journalctl ended with status {status}"))
    }
}

// Sockets ----------------------------------------------------------------------

/// A listening TCP socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub address: IpAddr,
    pub port: u16,
    /// The account that opened it.
    pub uid: u32,
}

impl Listener {
    /// `[::]:8743` or `0.0.0.0:8743`.
    pub fn text(&self) -> String {
        match self.address {
            IpAddr::V4(v4) => format!("{v4}:{}", self.port),
            IpAddr::V6(v6) => format!("[{v6}]:{}", self.port),
        }
    }
}

/// The listening sockets in `/proc/net/tcp` (`v6`: `/proc/net/tcp6`)
/// text: each word of an address is in host byte order.
pub fn parse_proc_net(text: &str, v6: bool) -> Vec<Listener> {
    let mut found = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st tx:rx tr:when retrnsmt uid ...
        if fields.len() < 8 || fields[3] != "0A" {
            continue;
        }
        let Some((address, port)) = fields[1].split_once(':') else {
            continue;
        };
        let (Ok(port), Ok(uid)) = (u16::from_str_radix(port, 16), fields[7].parse::<u32>()) else {
            continue;
        };
        let words: Option<Vec<u32>> = (0..address.len() / 8)
            .map(|i| u32::from_str_radix(&address[i * 8..i * 8 + 8], 16).ok())
            .collect();
        let Some(words) = words else { continue };
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let address = match (v6, bytes.len()) {
            (false, 4) => IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])),
            (true, 16) => {
                let octets: [u8; 16] = bytes.try_into().unwrap_or_default();
                IpAddr::V6(Ipv6Addr::from(octets))
            }
            _ => continue,
        };
        found.push(Listener { address, port, uid });
    }
    found
}

/// The sockets listening on `port`, or `None` where `/proc/net` isn't
/// there to say.
pub fn listeners(port: u16) -> Option<Vec<Listener>> {
    let v4 = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let v6 = std::fs::read_to_string("/proc/net/tcp6").unwrap_or_default();
    let mut all = parse_proc_net(&v4, false);
    all.extend(parse_proc_net(&v6, true));
    all.retain(|listener| listener.port == port);
    all.dedup();
    Some(all)
}

/// Whether a TCP connection to `address` opens within a second.
pub fn connects(address: std::net::SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(1)).is_ok()
}

// Times ------------------------------------------------------------------------

/// A time as `Oct 31 05:20:14`, in local time.
pub fn clock(at: f64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let tm = crate::fmtutil::localtime(at.floor() as i64);
    format!(
        "{} {:2} {:02}:{:02}:{:02}",
        MONTHS[(tm.month as usize).clamp(1, 12) - 1],
        tm.day,
        tm.hour,
        tm.minute,
        tm.second
    )
}

/// The local time zone's abbreviation now (`PDT`).
pub fn zone() -> String {
    // SAFETY: localtime_r fills a zeroed tm; strftime writes at most the
    // buffer's length.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let now = libc::time(std::ptr::null_mut());
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "UTC".into();
        }
        let mut buffer = [0u8; 32];
        let written = libc::strftime(
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
            c"%Z".as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buffer[..written]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_properties_parse_into_states_and_times() {
        let running = parse_show(
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nMainPID=812\nActiveEnterTimestampMonotonic=5000000\nInactiveEnterTimestampMonotonic=0\nResult=success\nExecMainStatus=0\n",
            1_000_000.0,
            105.0,
        );
        assert_eq!(running.state(), State::Running);
        assert!(running.enabled());
        assert_eq!(running.pid, Some(812));
        // Active since monotonic 5 s, 100 s ago.
        assert_eq!(running.since(), Some(999_900.0));
        assert_eq!(running.inactive_since, None);
        let failed = parse_show(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=disabled\nMainPID=0\nActiveEnterTimestampMonotonic=5000000\nInactiveEnterTimestampMonotonic=95000000\nResult=exit-code\nExecMainStatus=1\n",
            1_000_000.0,
            105.0,
        );
        assert_eq!(failed.state(), State::Failed);
        assert!(!failed.enabled());
        assert_eq!(failed.pid, None);
        assert_eq!(failed.since(), Some(999_990.0));
        assert_eq!(failed.exit_status, Some(1));
        let missing = parse_show(
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
            0.0,
            0.0,
        );
        assert_eq!(missing.state(), State::NotInstalled);
        assert_eq!(missing.since(), None);
        let restarting = parse_show(
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\n",
            0.0,
            0.0,
        );
        assert_eq!(restarting.state(), State::Restarting);
        assert_eq!(restarting.state().name(), "restarting");
    }

    fn entry(message: &str, pid: &str, identifier: &str) -> String {
        serde_json::json!({
            "__REALTIME_TIMESTAMP": "1793318400250000",
            "MESSAGE": message,
            "_PID": pid,
            "SYSLOG_IDENTIFIER": identifier,
            "PRIORITY": "6",
        })
        .to_string()
    }

    #[test]
    fn the_servers_own_lines_become_events() {
        let event = parse_entry(&entry(
            r#"{"event": "invite_redeemed", "invite": "09f019947dc9", "machine": "laptop"}"#,
            "812",
            "switchboard",
        ))
        .unwrap();
        assert_eq!(event.time, 1_793_318_400.25);
        assert_eq!(event.source, "server");
        assert_eq!(event.event, "invite_redeemed");
        assert_eq!(event.detail, "laptop joined with invite 09f019947dc9");
        assert_eq!(event.level, Level::Info);
        let cases = [
            (
                r#"{"event": "certificate_error", "detail": "order Invalid: unauthorized"}"#,
                "certificate_error",
                "order Invalid: unauthorized",
                Level::Error,
            ),
            (
                r#"{"event": "certificate_obtained", "detail": "sb.example.com"}"#,
                "certificate_obtained",
                "for sb.example.com",
                Level::Info,
            ),
            (
                r#"{"event": "private_certificate", "sha256": "9fe3cade5e4a90c1549e93d87a8b8abcc16ac3a0ef553826c55614869b2533ce", "certificate": "/x", "created": true}"#,
                "private_certificate",
                "made a new certificate; pin 9fe3cade5e4a…",
                Level::Info,
            ),
            (
                r#"{"event": "invite_store_unavailable", "detail": "unsafe"}"#,
                "invite_store_unavailable",
                "unsafe",
                Level::Error,
            ),
            (
                r#"{"event": "registry_group_not_kept", "gid": 997}"#,
                "registry_group_not_kept",
                "group 997",
                Level::Warning,
            ),
            (
                r#"{"event": "something_new", "a": 1, "b": "two"}"#,
                "something_new",
                "a=1, b=two",
                Level::Info,
            ),
            (
                "switchboard server: [::]:8743: Address already in use (os error 98)",
                "server_error",
                "[::]:8743: Address already in use (os error 98)",
                Level::Error,
            ),
            (
                "Exception occurred during processing of request: boom",
                "request_error",
                "boom",
                Level::Error,
            ),
            (
                "thread '<unnamed>' panicked at src/server/http.rs:10:5:",
                "panic",
                "thread '<unnamed>' panicked at src/server/http.rs:10:5:",
                Level::Error,
            ),
            ("plain words", "output", "plain words", Level::Info),
        ];
        for (message, name, detail, level) in cases {
            let event = parse_entry(&entry(message, "812", "switchboard")).unwrap();
            assert_eq!(
                (event.event.as_str(), event.detail.as_str(), event.level),
                (name, detail, level),
                "{message}"
            );
            assert_eq!(event.message, message);
        }
    }

    #[test]
    fn systemds_lines_about_the_service_become_events() {
        let cases = [
            (
                "Started switchboard-server.service - Switchboard server.",
                "started",
                "",
                Level::Info,
            ),
            (
                "Stopping switchboard-server.service - Switchboard server...",
                "stopping",
                "",
                Level::Info,
            ),
            (
                "switchboard-server.service: Main process exited, code=exited, status=1/FAILURE",
                "exited",
                "code=exited, status=1/FAILURE",
                Level::Error,
            ),
            (
                "switchboard-server.service: Main process exited, code=killed, status=15/TERM",
                "exited",
                "code=killed, status=15/TERM",
                Level::Info,
            ),
            (
                "switchboard-server.service: Failed with result 'exit-code'.",
                "failed",
                "result exit-code",
                Level::Error,
            ),
            (
                "switchboard-server.service: Scheduled restart job, restart counter is at 3.",
                "restarting",
                "attempt 3",
                Level::Warning,
            ),
            (
                "switchboard-server.service: Deactivated successfully.",
                "exited",
                "cleanly",
                Level::Info,
            ),
            (
                "switchboard-server.service: Start request repeated too quickly.",
                "failed",
                "restarted too often; systemd gave up",
                Level::Error,
            ),
            (
                "switchboard-server.service: Something unusual.",
                "service",
                "Something unusual.",
                Level::Info,
            ),
        ];
        for (message, name, detail, level) in cases {
            let event = parse_entry(&entry(message, "1", "systemd")).unwrap();
            assert_eq!(event.source, "systemd");
            assert_eq!(
                (event.event.as_str(), event.detail.as_str(), event.level),
                (name, detail, level),
                "{message}"
            );
        }
        // Bytes that aren't UTF-8 arrive as an array; a line that isn't an
        // entry is skipped.
        let bytes = serde_json::json!({"__REALTIME_TIMESTAMP": "1000000", "MESSAGE": [104, 105, 255], "_PID": "5"});
        assert_eq!(
            parse_entry(&bytes.to_string()).unwrap().message,
            "hi\u{fffd}"
        );
        assert!(parse_entry("-- No entries --").is_none());
        assert!(parse_entry(r#"{"MESSAGE": "no time"}"#).is_none());
    }

    #[test]
    fn following_ends_when_nobody_reads_the_output() {
        let entry = r#"{"__REALTIME_TIMESTAMP": "1793318400000000", "MESSAGE": "{\"event\": \"started\"}", "_PID": "42"}"#;
        // One entry, then a journal that stays quiet for a minute.
        let quiet = || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("printf '%s\\n' '{entry}'; exec sleep 60")]);
            command
        };
        let mut fds = [0; 2];
        // SAFETY: pipe fills two descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let began = std::time::Instant::now();
        // Output that can't be written: the follow ends at once.
        let mut seen = 0;
        let result = follow_command(quiet(), fds[1], |_| {
            seen += 1;
            false
        });
        assert_eq!((result, seen), (Ok(()), 1));
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        // A reader that closes its end while nothing new is logged: the
        // follow notices without writing, and ends the journal reader.
        // SAFETY: closing the read end this test opened.
        unsafe { libc::close(fds[0]) };
        let began = std::time::Instant::now();
        let result = follow_command(quiet(), fds[1], |_| true);
        assert_eq!(result, Ok(()));
        assert!(
            began.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        // SAFETY: closing the write end this test opened.
        unsafe { libc::close(fds[1]) };
    }

    #[test]
    fn journal_arguments_name_the_unit_and_options() {
        assert_eq!(
            journal_arguments(50, None, false),
            [
                "--unit=switchboard-server.service",
                "--output=json",
                "--no-pager",
                "--lines=50"
            ]
        );
        assert_eq!(
            journal_arguments(10, Some("-1h"), true)[4..],
            ["--since=-1h", "--follow"]
        );
    }

    #[test]
    fn listening_sockets_are_read_from_proc() {
        let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let v4 = format!(
            "{header}   0: 00000000:2227 00000000:0000 0A 00000000:00000000 00:00000000 00000000   997        0 1 1\n   1: 0100007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1\n   2: 0100007F:2227 0200007F:9999 01 00000000:00000000 00:00000000 00000000   997        0 3 1\n"
        );
        let found = parse_proc_net(&v4, false);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].text(), "0.0.0.0:8743");
        assert_eq!(found[0].uid, 997);
        if cfg!(target_endian = "little") {
            assert_eq!(found[1].text(), "127.0.0.1:53");
            let v6 = format!(
                "{header}   0: 00000000000000000000000000000000:01BB 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   997        0 4 1\n   1: 00000000000000000000000001000000:2227 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 5 1\n"
            );
            let found = parse_proc_net(&v6, true);
            assert_eq!(found[0].text(), "[::]:443");
            assert_eq!(found[1].text(), "[::1]:8743");
        }
    }

    #[test]
    fn times_read_as_month_day_and_clock() {
        let text = clock(1_793_318_400.0);
        assert_eq!(text.len(), "Oct 31 05:20:14".len(), "{text}");
        assert!(text.chars().nth(9) == Some(':'), "{text}");
    }
}
