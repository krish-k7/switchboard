//! `switchboard server status`: whether this machine's server is healthy,
//! on one screen. It only reads: the service's state from systemd, the
//! installed program's version, the configuration, the certificate on disk
//! and the one the server presents, the sockets it listens on, how many
//! machines and invites there are (counts, never the lists: `server
//! machines` and `server invites` are the lists), the database's size, and
//! the errors in the journal over the last day.
//!
//! What needs attention comes first, each problem with its fix, then the
//! facts as aligned `label  value` rows, then the recent errors. It exits
//! with status 1 when anything needs attention, so a monitor can run it.
//! It needs root or the server's service account, like the other local
//! commands; the journal is read only when this account may read it.

use std::os::unix::fs::{FileTypeExt, MetadataExt};

use serde_json::{Map, Value};

use super::certinfo::{self, Described};
use super::enroll_local::read_file;
use super::install::{self, Probe, SUDO};
use super::invites::{self, Local};
use super::jsonval::{self, get, Val};
use super::system::{self, Event, Level, Listener, Service, State};
use crate::cli::ui::{self, Mark, Theme, Tone};
use crate::fsutil;

const USAGE: &str = "Usage: switchboard server status [--config FILE] [--json]\n";
const HELP: &str = "
Whether this machine's server is healthy: its service, version, mode and
address, certificate, listening addresses, how many machines and invites it
has, its database's size, and its recent errors. Problems come first, each
with its fix. Exits 1 when anything needs attention. Run it on the server as
root or as the server's service account (the errors in the system journal
need root).

Options:
  --config FILE  the server config (default: /etc/switchboard/server.json)
  --json         print the result as JSON
  -h, --help     show this help
";

/// A machine counts as recently seen within this many seconds.
const RECENT: f64 = 600.0;
/// How far back the journal is read for errors.
const ERROR_WINDOW: &str = "-24h";
/// At most this many journal lines are read for errors.
const ERROR_LINES: usize = 2000;
/// At most this many recent errors are listed.
const ERRORS_SHOWN: usize = 3;

/// The listener the configuration selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Private,
    Acme,
    Caddy,
    /// Mutual TLS with a client CA (tests only).
    Direct,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Private => "private",
            Mode::Acme => "acme",
            Mode::Caddy => "caddy",
            Mode::Direct => "direct",
        }
    }
}

/// What the configuration says, with paths resolved as the server does.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    pub hostname: Option<String>,
    pub names: Vec<String>,
    /// The TCP port (not in Caddy mode).
    pub port: Option<u16>,
    /// The configured listen address, when it isn't the default.
    pub listen: Option<String>,
    pub socket: Option<String>,
    pub db: String,
    pub peers: String,
    pub lock: String,
}

impl Settings {
    fn state(&self) -> String {
        fsutil::parent(&self.db)
    }

    /// The certificate file the server serves, for the built-in modes.
    fn certificate_path(&self) -> Option<String> {
        match self.mode {
            Mode::Private => Some(format!("{}/tls/private-cert.pem", self.state())),
            Mode::Acme => Some(format!(
                "{}/tls/acme-{}-cert.pem",
                self.state(),
                self.hostname.clone().unwrap_or_default()
            )),
            _ => None,
        }
    }

    /// The address machines use, when the configuration says it.
    fn address(&self) -> Option<String> {
        let host = match self.mode {
            Mode::Acme => self.hostname.clone()?,
            Mode::Private => self
                .names
                .first()
                .cloned()
                .unwrap_or_else(|| "localhost".into()),
            _ => return None,
        };
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        Some(match self.port {
            Some(443) | None => format!("https://{host}"),
            Some(port) => format!("https://{host}:{port}"),
        })
    }

    /// The command that installs this server again with its options.
    fn install_command(&self) -> String {
        match self.mode {
            Mode::Acme => format!(
                "{SUDO} server install --tls acme --domain {}",
                self.hostname.clone().unwrap_or_default()
            ),
            Mode::Caddy => format!("{SUDO} server install --tls caddy"),
            _ => format!("{SUDO} server install --tls private"),
        }
    }

    /// Where a local check connects: the configured address, or loopback
    /// when the server listens on every address.
    fn probe_address(&self) -> Option<std::net::SocketAddr> {
        let port = self.port?;
        let host = self
            .listen
            .as_deref()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .filter(|ip| !ip.is_unspecified())
            .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
        Some(std::net::SocketAddr::new(host, port))
    }
}

/// Read a server configuration (`path` is where it was read from, for
/// relative paths).
pub fn parse_settings(data: &[u8], path: &str) -> Result<Settings, String> {
    let Some(Val::Dict(config)) = jsonval::loads(data) else {
        return Err("the server config is not a JSON object".into());
    };
    let directory = fsutil::parent(&fsutil::absolute(path).map_err(|e| e.message)?);
    let text = |name: &str| get(&config, name).and_then(Val::as_str).map(str::to_string);
    let resolve = |value: &str| {
        if fsutil::is_absolute(value) {
            fsutil::normalize(value)
        } else {
            fsutil::join(&directory, value)
        }
    };
    let db = resolve(&text("db").ok_or("the server config has no db")?);
    let peers = resolve(&text("peers_file").ok_or("the server config has no peers_file")?);
    let lock = match text("registry_lock") {
        Some(lock) if !lock.is_empty() => fsutil::normalize(&lock),
        _ => format!("{peers}.lock"),
    };
    let socket = text("unix_socket").map(|socket| resolve(&socket));
    let mode = if socket.is_some() {
        Mode::Caddy
    } else {
        match text("tls").as_deref() {
            Some("acme") => Mode::Acme,
            Some("private") => Mode::Private,
            Some(_) => return Err("the server config's tls is not acme or private".into()),
            None => Mode::Direct,
        }
    };
    let port = match get(&config, "port") {
        None => match mode {
            Mode::Acme => Some(443),
            Mode::Caddy => None,
            _ => Some(8743),
        },
        Some(Val::Int(digits)) => Some(
            digits
                .parse::<u16>()
                .map_err(|_| "the server config's port is invalid")?,
        ),
        Some(_) => return Err("the server config's port is invalid".into()),
    };
    let names = match get(&config, "names") {
        Some(Val::List(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ if mode == Mode::Private => {
            vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()]
        }
        _ => Vec::new(),
    };
    Ok(Settings {
        mode,
        hostname: text("hostname").map(|h| h.to_ascii_lowercase()),
        names,
        port: if mode == Mode::Caddy { None } else { port },
        listen: text("listen"),
        socket,
        db,
        peers,
        lock,
    })
}

/// The certificate on disk.
#[derive(Debug, Clone, PartialEq)]
pub enum Certificate {
    /// Caddy (or a test listener) holds the certificate.
    Elsewhere,
    Missing,
    Unreadable(String),
    Read(Described),
}

/// What the local connection to the server found.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    NotChecked,
    No,
    /// A TLS alert: an ACME server without a certificate yet.
    Alert,
    /// It answered; the certificate it presented (TLS modes).
    Yes(Option<String>),
}

/// Everything `server status` found.
#[derive(Debug, Clone)]
pub struct Report {
    pub settings: Settings,
    /// `None` without systemd.
    pub service: Option<Service>,
    pub version: Option<String>,
    pub certificate: Certificate,
    pub answer: Answer,
    /// The account that owns the registry: the one the server runs as.
    pub service_uid: Option<u32>,
    /// TCP listeners on the port, or `None` where they can't be read.
    pub listeners: Option<Vec<Listener>>,
    /// Whether the port takes IPv4 connections.
    pub ipv4_connects: bool,
    /// Caddy mode: whether the socket exists.
    pub socket_present: bool,
    /// Enrolled machines, and how many were seen recently.
    pub machines: Result<(usize, usize), String>,
    pub invites: Result<usize, String>,
    /// The database and its write-ahead files, in bytes.
    pub database: Option<u64>,
    /// Whether the journal could be read.
    pub journal: bool,
    /// The errors in the journal over the last day, oldest first.
    pub errors: Vec<Event>,
    pub now: f64,
}

impl Report {
    fn state(&self) -> Option<State> {
        self.service.as_ref().map(Service::state)
    }

    /// The listeners the server's account opened.
    fn own_listeners(&self) -> Vec<&Listener> {
        self.listeners
            .iter()
            .flatten()
            .filter(|l| self.service_uid.is_none_or(|uid| l.uid == uid))
            .collect()
    }

    /// The listeners another account holds on the server's port.
    fn foreign_listeners(&self) -> Vec<&Listener> {
        self.listeners
            .iter()
            .flatten()
            .filter(|l| self.service_uid.is_some_and(|uid| l.uid != uid))
            .collect()
    }

    /// Whether the server takes IPv4 and IPv6 connections.
    fn families(&self) -> (bool, bool) {
        let own = self.own_listeners();
        let v6 = own.iter().any(|l| l.address.is_ipv6());
        let v4 = own.iter().any(|l| l.address.is_ipv4())
            || (own
                .iter()
                .any(|l| l.address.is_ipv6() && l.address.is_unspecified())
                && self.ipv4_connects);
        (v4, v6)
    }

    /// Errors logged since the service last started.
    fn errors_since_start(&self) -> usize {
        match (
            self.state(),
            self.service.as_ref().and_then(|s| s.active_since),
        ) {
            (Some(State::Running), Some(since)) => {
                self.errors.iter().filter(|e| e.time >= since).count()
            }
            _ => 0,
        }
    }

    /// Whole days until the certificate expires (negative once expired).
    fn days_left(&self) -> Option<i64> {
        match &self.certificate {
            Certificate::Read(cert) => Some(days_until(cert.not_after, self.now)),
            _ => None,
        }
    }
}

fn epoch(time: std::time::SystemTime) -> f64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    }
}

/// Whole days from `now` until `time`, rounded down.
fn days_until(time: std::time::SystemTime, now: f64) -> i64 {
    ((epoch(time) - now) / 86400.0).floor() as i64
}

/// The days left at which a certificate needs attention: the server renews
/// an ACME certificate when a third of its lifetime is left, so by 7/30 of
/// it (21 days of a 90-day certificate) renewal has failed for over a week.
/// A private certificate is never renewed; it gets three weeks' notice.
fn warning_days(mode: Mode, cert: &Described) -> f64 {
    let lifetime = (epoch(cert.not_after) - epoch(cert.not_before)) / 86400.0;
    match mode {
        Mode::Acme => lifetime * 7.0 / 30.0,
        _ => 21.0,
    }
}

// Gathering --------------------------------------------------------------------

/// The installed program's version, asked only of a root-owned program in a
/// root-owned directory that no one else can write.
fn installed_version() -> Option<String> {
    let safe = |path: &str, directory: bool| {
        std::fs::symlink_metadata(path).is_ok_and(|info| {
            (if directory {
                info.is_dir()
            } else {
                info.file_type().is_file()
            }) && info.uid() == 0
                && info.mode() & 0o022 == 0
        })
    };
    if !safe(install::PROGRAM_DIR, true) || !safe(install::PROGRAM, false) {
        return None;
    }
    let output = std::process::Command::new(install::PROGRAM)
        .arg("--version")
        .env_clear()
        .env("PATH", install::PATH)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.trim().strip_prefix("switchboard ")?;
    (output.status.success() && !version.is_empty()).then(|| version.to_string())
}

fn read_certificate(settings: &Settings) -> Certificate {
    let Some(path) = settings.certificate_path() else {
        return Certificate::Elsewhere;
    };
    match read_file(&path) {
        Ok((pem, _)) => match certinfo::describe_pem(&pem) {
            Some(described) => Certificate::Read(described),
            None => Certificate::Unreadable(format!("{path} holds no certificate")),
        },
        Err(_) if std::fs::symlink_metadata(&path).is_err() => Certificate::Missing,
        Err(super::enroll_local::Failure::Reported(message))
        | Err(super::enroll_local::Failure::Other(message)) => Certificate::Unreadable(message),
    }
}

fn ask(settings: &Settings) -> Answer {
    match settings.mode {
        Mode::Caddy => match &settings.socket {
            Some(socket) if install::answers_unix(socket) => Answer::Yes(None),
            _ => Answer::No,
        },
        Mode::Direct => Answer::NotChecked,
        mode => {
            let Some(address) = settings.probe_address() else {
                return Answer::NotChecked;
            };
            let name = match mode {
                Mode::Acme => settings.hostname.clone().unwrap_or_default(),
                _ => "localhost".to_string(),
            };
            match install::probe_tls(address, &name) {
                Probe::Unreachable => Answer::No,
                Probe::Alert => Answer::Alert,
                Probe::Answered { fingerprint, http } if http => Answer::Yes(Some(fingerprint)),
                Probe::Answered { .. } => Answer::No,
            }
        }
    }
}

/// Machines and how many were seen in the last ten minutes.
fn machine_counts(local: &Local, now: f64) -> Result<(usize, usize), String> {
    let rows = invites::machines(local).map_err(failure_text)?;
    let seen = rows
        .iter()
        .filter(|row| row.last_seen.is_some_and(|at| now - at <= RECENT))
        .count();
    Ok((rows.len(), seen))
}

fn failure_text(failure: super::enroll_local::Failure) -> String {
    match failure {
        super::enroll_local::Failure::Reported(message)
        | super::enroll_local::Failure::Other(message) => message,
    }
}

fn database_size(db: &str) -> Option<u64> {
    let main = std::fs::symlink_metadata(db).ok()?.len();
    let extra: u64 = ["-wal", "-shm"]
        .iter()
        .filter_map(|suffix| std::fs::symlink_metadata(format!("{db}{suffix}")).ok())
        .map(|info| info.len())
        .sum();
    Some(main + extra)
}

/// Look at everything.
pub fn gather(settings: Settings) -> Report {
    let now = system::now();
    let local = Local {
        db: settings.db.clone(),
        peers: settings.peers.clone(),
        lock: settings.lock.clone(),
        url: None,
        pin: None,
    };
    let service_uid = std::fs::symlink_metadata(&settings.peers)
        .ok()
        .map(|info| info.uid());
    let listeners = settings.port.and_then(system::listeners);
    let ipv4_connects = settings
        .port
        .is_some_and(|port| system::connects(std::net::SocketAddr::from(([127, 0, 0, 1], port))));
    let socket_present = settings.socket.as_deref().is_some_and(|socket| {
        std::fs::symlink_metadata(socket).is_ok_and(|info| info.file_type().is_socket())
    });
    let journal = system::has_systemd() && system::can_read_journal();
    let errors = if journal {
        system::read_journal(ERROR_LINES, Some(ERROR_WINDOW))
            .map(|events| {
                events
                    .into_iter()
                    .filter(|event| event.level == Level::Error)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Report {
        service: system::service(),
        version: installed_version(),
        certificate: read_certificate(&settings),
        answer: ask(&settings),
        service_uid,
        listeners,
        ipv4_connects,
        socket_present,
        machines: machine_counts(&local, now),
        invites: invites::list(&local)
            .map(|open| open.len())
            .map_err(failure_text),
        database: database_size(&settings.db),
        journal,
        errors,
        now,
        settings,
    }
}

// Decisions --------------------------------------------------------------------

/// Something that needs attention, with how to fix it.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// The part it concerns: `service`, `answer`, `certificate`, `machines`,
    /// `invites`, or `errors`.
    pub check: &'static str,
    /// `Error` (failing) or `Warning`.
    pub level: Level,
    pub summary: String,
    /// The fix, with commands in backticks.
    pub fix: Option<String>,
}

/// `sudo switchboard server logs`, as this machine runs it.
struct Logs;

impl std::fmt::Display for Logs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{SUDO} server logs")
    }
}

const LOGS: Logs = Logs;
const START: &str = "sudo systemctl start switchboard-server";
const RESTART: &str = "sudo systemctl restart switchboard-server";

/// What needs attention, failures first.
pub fn problems(report: &Report) -> Vec<Problem> {
    let mut found = Vec::new();
    let mut add = |check, level, summary: String, fix: Option<String>| {
        found.push(Problem {
            check,
            level,
            summary,
            fix,
        })
    };
    let settings = &report.settings;
    let port_text = settings
        .port
        .map(|port| format!("port {port}"))
        .unwrap_or_else(|| "its socket".into());
    let foreign = report.foreign_listeners();
    let blocked = |port: u16| {
        format!("find it with `sudo ss -ltnp 'sport = :{port}'` and stop it, then run `{RESTART}`.")
    };
    // The service.
    let state = report.state();
    match (&report.service, state) {
        (None, _) => add(
            "service",
            Level::Error,
            "This machine doesn't run systemd, so the server's service can't be checked.".into(),
            None,
        ),
        (Some(_), Some(State::NotInstalled)) => add(
            "service",
            Level::Error,
            "The server's service isn't installed.".into(),
            Some(format!(
                "install it again with `{}`.",
                settings.install_command()
            )),
        ),
        (Some(_), Some(State::Failed | State::Stopped)) => {
            if let (Some(port), false) = (settings.port, foreign.is_empty()) {
                add(
                    "service",
                    Level::Error,
                    format!(
                        "The server isn't running, and another program is listening on port {port}."
                    ),
                    Some(blocked(port)),
                );
            } else if state == Some(State::Failed) {
                add(
                    "service",
                    Level::Error,
                    "The server stopped after a failure.".into(),
                    Some(format!("see why with `{LOGS}`, then run `{START}`.")),
                );
            } else {
                add(
                    "service",
                    Level::Error,
                    "The server isn't running.".into(),
                    Some(format!("run `{START}`.")),
                );
            }
        }
        (Some(_), Some(State::Restarting)) => add(
            "service",
            Level::Error,
            "The server keeps failing and restarting.".into(),
            Some(format!("see why with `{LOGS}`.")),
        ),
        (Some(_), Some(State::Starting)) => add(
            "service",
            Level::Warning,
            "The server is still starting.".into(),
            None,
        ),
        (Some(_), Some(State::Stopping)) => add(
            "service",
            Level::Warning,
            "The server is stopping.".into(),
            None,
        ),
        (Some(service), _) => {
            if !service.enabled() {
                add(
                    "service",
                    Level::Warning,
                    "The server won't start when this machine boots.".into(),
                    Some("run `sudo systemctl enable switchboard-server`.".into()),
                );
            }
        }
    }
    // Whether it answers, once it should.
    let should_answer = matches!(state, None | Some(State::Running));
    let acme_without_certificate =
        settings.mode == Mode::Acme && report.certificate == Certificate::Missing;
    match &report.answer {
        Answer::No | Answer::Alert if should_answer && !acme_without_certificate => {
            if let (Some(port), false) = (settings.port, foreign.is_empty()) {
                add(
                    "answer",
                    Level::Error,
                    format!("Another program is listening on port {port}, not the server."),
                    Some(blocked(port)),
                );
            } else {
                add(
                    "answer",
                    Level::Error,
                    format!("The server doesn't answer on {port_text}."),
                    Some(format!("see why with `{LOGS}`.")),
                );
            }
        }
        _ => {}
    }
    // The certificate.
    let path = settings.certificate_path().unwrap_or_default();
    match &report.certificate {
        Certificate::Elsewhere => {}
        Certificate::Missing if settings.mode == Mode::Acme => add(
            "certificate",
            Level::Error,
            "The server has no certificate from Let's Encrypt yet, so machines can't connect."
                .into(),
            Some(format!(
                "look for certificate_error lines in `{LOGS}`; docs/troubleshooting.md explains each."
            )),
        ),
        Certificate::Missing => add(
            "certificate",
            Level::Error,
            format!("The server's certificate is missing ({path})."),
            Some(format!(
                "restore it from a backup, or run `{RESTART}` to make a new one and give every machine a new invite."
            )),
        ),
        Certificate::Unreadable(message) => add(
            "certificate",
            Level::Error,
            format!("The server's certificate can't be read: {message}."),
            None,
        ),
        Certificate::Read(cert) => {
            let days = days_until(cert.not_after, report.now);
            let renewal = settings.mode == Mode::Acme;
            if days < 0 {
                add(
                    "certificate",
                    Level::Error,
                    "The server's certificate has expired, so machines can't connect.".into(),
                    Some(if renewal {
                        format!("look for certificate_error lines in `{LOGS}`.")
                    } else {
                        format!("run `{RESTART}` after moving {path} aside to make a new one, then give every machine a new invite.")
                    }),
                );
            } else if (epoch(cert.not_after) - report.now) / 86400.0 < warning_days(settings.mode, cert) {
                let summary = if renewal {
                    format!(
                        "The certificate expires in {} and should have been renewed by now.",
                        days_text(days)
                    )
                } else {
                    format!("The certificate expires in {}.", days_text(days))
                };
                add(
                    "certificate",
                    Level::Warning,
                    summary,
                    renewal.then(|| format!("look for certificate_error lines in `{LOGS}`.")),
                );
            }
            if let Answer::Yes(Some(served)) = &report.answer {
                if *served != cert.sha256 {
                    add(
                        "certificate",
                        Level::Warning,
                        format!("The server presents a different certificate than {path}."),
                        Some(format!("run `{RESTART}`.")),
                    );
                }
            }
        }
    }
    if let Err(message) = &report.machines {
        add(
            "machines",
            Level::Error,
            format!("The registry can't be read: {message}."),
            None,
        );
    }
    if let Err(message) = &report.invites {
        add(
            "invites",
            Level::Warning,
            format!("The invites can't be read: {message}."),
            None,
        );
    }
    let since_start = report.errors_since_start();
    if since_start > 0 {
        add(
            "errors",
            Level::Warning,
            format!(
                "The server has logged {} since it started.",
                ui::count(since_start, "error", "errors")
            ),
            Some(format!("see them in context with `{LOGS}`.")),
        );
    }
    found.sort_by_key(|problem| std::cmp::Reverse(problem.level));
    found
}

// Text -------------------------------------------------------------------------

fn days_text(days: i64) -> String {
    ui::count(days.max(0) as usize, "day", "days")
}

/// A size in bytes for people: `48 KB`, `1.2 MB`.
pub fn size_text(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes < 1000 {
        ui::count(bytes as usize, "byte", "bytes")
    } else if value < 1e6 {
        format!("{:.0} KB", value / 1e3)
    } else if value < 1e9 {
        format!("{:.1} MB", value / 1e6)
    } else {
        format!("{:.1} GB", value / 1e9)
    }
}

/// `Jan 1, 2125`.
fn date_text(time: std::time::SystemTime) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let tm = crate::fmtutil::localtime(epoch(time).floor() as i64);
    format!(
        "{} {}, {}",
        MONTHS[(tm.month as usize).clamp(1, 12) - 1],
        tm.day,
        tm.year
    )
}

/// `a`, `a and b`, `a, b, and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one} and {two}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn local_time(at: f64) -> String {
    crate::cli::init::system::local_time(at.floor() as i64)
}

fn service_text(report: &Report) -> String {
    let Some(service) = &report.service else {
        return "unknown (this machine doesn't run systemd)".into();
    };
    let since = service.since().map(local_time);
    let at = |word: &str| match &since {
        Some(time) => format!("{word} {time}"),
        None => String::new(),
    };
    match service.state() {
        State::NotInstalled => "not installed".into(),
        State::Running => format!("running {}", at("since")).trim_end().into(),
        State::Starting => "starting".into(),
        State::Restarting => "restarting after a failure".into(),
        State::Stopping => "stopping".into(),
        State::Stopped => format!("stopped {}", at("since")).trim_end().into(),
        State::Failed => {
            let mut text = format!("failed {}", at("at")).trim_end().to_string();
            if let Some(code) = service.exit_status.filter(|code| *code != 0) {
                text.push_str(&format!(", exit status {code}"));
            }
            text
        }
    }
}

fn mode_text(mode: Mode) -> &'static str {
    match mode {
        Mode::Private => "private network",
        Mode::Acme => "public domain, certificate from Let's Encrypt",
        Mode::Caddy => "behind Caddy",
        Mode::Direct => "direct mutual TLS (for tests)",
    }
}

fn certificate_text(report: &Report) -> String {
    match &report.certificate {
        Certificate::Elsewhere if report.settings.mode == Mode::Caddy => "held by Caddy".into(),
        Certificate::Elsewhere => "from the configuration".into(),
        Certificate::Missing if report.settings.mode == Mode::Acme => "none yet".into(),
        Certificate::Missing => "missing".into(),
        Certificate::Unreadable(_) => "can't be read".into(),
        Certificate::Read(cert) => {
            let days = days_until(cert.not_after, report.now);
            let expiry = if days < 0 {
                format!("expired {}", date_text(cert.not_after))
            } else if days < 365 {
                format!(
                    "expires in {} ({})",
                    days_text(days),
                    date_text(cert.not_after)
                )
            } else {
                format!("valid until {}", date_text(cert.not_after))
            };
            let names = if cert.names.is_empty() {
                cert.subject.clone()
            } else {
                and_list(&cert.names)
            };
            if cert.self_signed {
                format!("self-signed for {names}, {expiry}")
            } else {
                format!("{names}, from {}, {expiry}", cert.issuer)
            }
        }
    }
}

fn listening_text(report: &Report) -> Option<String> {
    if report.settings.mode == Mode::Caddy {
        let socket = report.settings.socket.clone().unwrap_or_default();
        return Some(if report.socket_present {
            socket
        } else {
            format!("nothing at {socket}")
        });
    }
    let port = report.settings.port?;
    report.listeners.as_ref()?;
    let own = report.own_listeners();
    if own.is_empty() {
        let foreign = report.foreign_listeners();
        return Some(if foreign.is_empty() {
            format!("nothing on port {port}")
        } else {
            format!("port {port} is held by another program")
        });
    }
    let addresses: Vec<String> = own.iter().map(|l| l.text()).collect();
    let families = match report.families() {
        (true, true) => "IPv6 and IPv4",
        (true, false) => "IPv4 only",
        (false, true) => "IPv6 only",
        (false, false) => "",
    };
    Some(if families.is_empty() {
        addresses.join(", ")
    } else {
        format!("{}, {families}", addresses.join(", "))
    })
}

/// `2 machines, both seen in the last 10 minutes`.
pub fn machines_text(enrolled: usize, seen: usize) -> String {
    let count = ui::count(enrolled, "machine", "machines");
    let window = "in the last 10 minutes";
    match (enrolled, seen) {
        (0, _) => "none joined yet".into(),
        (1, 1) => format!("{count}, seen {window}"),
        (1, _) => format!("{count}, not seen {window}"),
        (2, 2) => format!("{count}, both seen {window}"),
        (n, s) if n == s => format!("{count}, all seen {window}"),
        (_, 0) => format!("{count}, none seen {window}"),
        (_, s) => format!("{count}, {s} seen {window}"),
    }
}

fn invites_text(open: usize) -> String {
    match open {
        0 => "none open".into(),
        1 => "1 open invite".into(),
        n => format!("{n} open invites"),
    }
}

/// The facts, as `label  value` rows.
fn facts(report: &Report) -> Vec<(&'static str, String)> {
    let mut rows = vec![("Service", service_text(report))];
    let version = match &report.version {
        Some(version) if version == crate::VERSION => version.clone(),
        Some(version) => format!("{version} (this command is {})", crate::VERSION),
        None => format!("unknown (this command is {})", crate::VERSION),
    };
    rows.push(("Version", version));
    rows.push(("Mode", mode_text(report.settings.mode).to_string()));
    if let Some(address) = report.settings.address() {
        rows.push(("Address", address));
    }
    rows.push(("Certificate", certificate_text(report)));
    if let (Mode::Private, Certificate::Read(cert)) = (report.settings.mode, &report.certificate) {
        rows.push(("Pin", cert.sha256.clone()));
    }
    if let Some(listening) = listening_text(report) {
        rows.push(("Listening", listening));
    }
    rows.push((
        "Machines",
        match &report.machines {
            Ok((enrolled, seen)) => machines_text(*enrolled, *seen),
            Err(_) => "can't be read".into(),
        },
    ));
    rows.push((
        "Invites",
        match &report.invites {
            Ok(open) => invites_text(*open),
            Err(_) => "can't be read".into(),
        },
    ));
    rows.push((
        "Database",
        report
            .database
            .map(size_text)
            .unwrap_or_else(|| "not created yet".into()),
    ));
    rows
}

/// The status for people.
pub fn render(report: &Report, problems: &[Problem], theme: &Theme, columns: usize) -> String {
    let mut lines = Vec::new();
    for problem in problems {
        let (mark, tone) = match problem.level {
            Level::Error => (Mark::Failed, Tone::Strong),
            _ => (Mark::Warned, Tone::Normal),
        };
        lines.extend(ui::item(theme, mark, &problem.summary, tone, columns));
        if let Some(fix) = &problem.fix {
            let steps = ui::fix_steps(fix);
            let numbered = steps.len() > 1;
            lines.extend(ui::fix_lines(theme, &steps, columns));
            if numbered {
                lines.push(String::new());
            }
        }
    }
    if problems.is_empty() {
        lines.extend(ui::done(theme, "The server is healthy.", columns));
    }
    while lines.last().is_some_and(|line: &String| line.is_empty()) {
        lines.pop();
    }
    lines.push(String::new());
    let rows = facts(report);
    let pad = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, value) in &rows {
        // The pin is copied and compared whole, so it stays on one line
        // whenever the terminal is wide enough, even past the measure.
        let head = ui::INDENT + pad + 2;
        if *label == "Pin" && head + value.len() < columns {
            lines.push(format!(
                "{}{}  {value}",
                " ".repeat(ui::INDENT),
                crate::cli::format::padded(label, pad)
            ));
            continue;
        }
        lines.extend(ui::prompt::fact_lines(
            theme,
            Mark::Blank,
            label,
            value,
            pad,
            columns,
        ));
    }
    if !report.journal && report.service.is_some() {
        lines.push(String::new());
        lines.extend(ui::para(
            theme,
            &format!("The journal can't be read as this account, so recent errors aren't shown. To see them, run\n`{SUDO} server status`"),
            Tone::Normal,
            columns,
        ));
    } else if !report.errors.is_empty() {
        lines.push(String::new());
        let indent = " ".repeat(ui::INDENT);
        lines.push(format!(
            "{indent}{}  {} in the last day, times in {}",
            theme.strong("Recent errors"),
            report.errors.len(),
            system::zone()
        ));
        lines.push(String::new());
        let shown = &report.errors[report.errors.len().saturating_sub(ERRORS_SHOWN)..];
        lines.extend(super::logs::event_lines(theme, shown, None, columns));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The status for scripts; the field names are stable.
pub fn json(report: &Report, problems: &[Problem]) -> Map<String, Value> {
    let optional = |value: Option<String>| value.map_or(Value::Null, Value::from);
    let mut out = Map::new();
    out.insert("ok".into(), Value::Bool(problems.is_empty()));
    out.insert(
        "problems".into(),
        Value::Array(
            problems
                .iter()
                .map(|problem| {
                    let mut item = Map::new();
                    item.insert("check".into(), Value::from(problem.check));
                    item.insert(
                        "level".into(),
                        Value::from(match problem.level {
                            Level::Error => "fail",
                            _ => "warn",
                        }),
                    );
                    item.insert("summary".into(), Value::from(problem.summary.clone()));
                    item.insert(
                        "fix".into(),
                        optional(problem.fix.as_ref().map(|fix| fix.replace('`', ""))),
                    );
                    Value::Object(item)
                })
                .collect(),
        ),
    );
    let mut service = Map::new();
    service.insert("unit".into(), Value::from(install::UNIT_NAME));
    match &report.service {
        Some(info) => {
            service.insert("state".into(), Value::from(info.state().name()));
            service.insert(
                "since".into(),
                info.since()
                    .map_or(Value::Null, |at| Value::from(at.floor() as i64)),
            );
            service.insert("enabled".into(), Value::Bool(info.enabled()));
            service.insert("pid".into(), info.pid.map_or(Value::Null, Value::from));
        }
        None => {
            service.insert("state".into(), Value::from("unknown"));
            service.insert("since".into(), Value::Null);
            service.insert("enabled".into(), Value::Null);
            service.insert("pid".into(), Value::Null);
        }
    }
    out.insert("service".into(), Value::Object(service));
    out.insert("version".into(), optional(report.version.clone()));
    out.insert("command_version".into(), Value::from(crate::VERSION));
    out.insert("mode".into(), Value::from(report.settings.mode.name()));
    out.insert("address".into(), optional(report.settings.address()));
    out.insert("socket".into(), optional(report.settings.socket.clone()));
    out.insert(
        "port".into(),
        report.settings.port.map_or(Value::Null, Value::from),
    );
    out.insert(
        "answering".into(),
        match report.answer {
            Answer::NotChecked => Value::Null,
            Answer::Yes(_) => Value::Bool(true),
            Answer::No | Answer::Alert => Value::Bool(false),
        },
    );
    let certificate = match &report.certificate {
        Certificate::Read(cert) => {
            let mut item = Map::new();
            item.insert("path".into(), optional(report.settings.certificate_path()));
            item.insert("subject".into(), Value::from(cert.subject.clone()));
            item.insert(
                "names".into(),
                Value::Array(cert.names.iter().cloned().map(Value::from).collect()),
            );
            item.insert("issuer".into(), Value::from(cert.issuer.clone()));
            item.insert("self_signed".into(), Value::Bool(cert.self_signed));
            item.insert(
                "not_after".into(),
                Value::from(epoch(cert.not_after).floor() as i64),
            );
            item.insert(
                "days_left".into(),
                report.days_left().map_or(Value::Null, Value::from),
            );
            item.insert("sha256".into(), Value::from(cert.sha256.clone()));
            item.insert(
                "served".into(),
                match &report.answer {
                    Answer::Yes(Some(served)) => Value::Bool(*served == cert.sha256),
                    _ => Value::Null,
                },
            );
            Value::Object(item)
        }
        _ => Value::Null,
    };
    out.insert("certificate".into(), certificate);
    out.insert(
        "pin".into(),
        match (&report.certificate, report.settings.mode) {
            (Certificate::Read(cert), Mode::Private) => Value::from(cert.sha256.clone()),
            _ => Value::Null,
        },
    );
    let (v4, v6) = report.families();
    let tcp = report.settings.port.is_some() && report.listeners.is_some();
    out.insert(
        "listening".into(),
        if tcp {
            Value::Array(
                report
                    .own_listeners()
                    .iter()
                    .map(|l| Value::from(l.text()))
                    .collect(),
            )
        } else {
            Value::Null
        },
    );
    out.insert(
        "ipv4".into(),
        if tcp { Value::Bool(v4) } else { Value::Null },
    );
    out.insert(
        "ipv6".into(),
        if tcp { Value::Bool(v6) } else { Value::Null },
    );
    out.insert(
        "machines".into(),
        match &report.machines {
            Ok((enrolled, seen)) => {
                let mut item = Map::new();
                item.insert("enrolled".into(), Value::from(*enrolled));
                item.insert("seen_last_10_minutes".into(), Value::from(*seen));
                Value::Object(item)
            }
            Err(_) => Value::Null,
        },
    );
    out.insert(
        "invites".into(),
        match &report.invites {
            Ok(open) => {
                let mut item = Map::new();
                item.insert("open".into(), Value::from(*open));
                Value::Object(item)
            }
            Err(_) => Value::Null,
        },
    );
    out.insert(
        "database".into(),
        match report.database {
            Some(bytes) => {
                let mut item = Map::new();
                item.insert("path".into(), Value::from(report.settings.db.clone()));
                item.insert("bytes".into(), Value::from(bytes));
                Value::Object(item)
            }
            None => Value::Null,
        },
    );
    let mut errors = Map::new();
    errors.insert("readable".into(), Value::Bool(report.journal));
    errors.insert("last_day".into(), Value::from(report.errors.len()));
    errors.insert(
        "since_start".into(),
        Value::from(report.errors_since_start()),
    );
    errors.insert(
        "recent".into(),
        Value::Array(
            report.errors[report.errors.len().saturating_sub(ERRORS_SHOWN)..]
                .iter()
                .map(super::logs::event_json)
                .map(Value::Object)
                .collect(),
        ),
    );
    out.insert("errors".into(), Value::Object(errors));
    out
}

// The command ------------------------------------------------------------------

/// Why the status can't be shown at all.
fn unavailable(json: bool, problem: ui::prompt::Problem, message: &str) -> i32 {
    if json {
        let mut value = Map::new();
        value.insert("ok".into(), Value::Bool(false));
        value.insert("error".into(), Value::from(message));
        println!("{}", crate::wirejson::dumps(&Value::Object(value)));
    } else {
        crate::cli::print_problem(&problem);
    }
    1
}

/// `switchboard server status [--config FILE] [--json]`.
pub fn main(argv: &[String]) -> i32 {
    let common = match invites::parse_common(argv, &[], "status", USAGE) {
        Ok(Some(common)) if common.positional.is_empty() => common,
        Ok(Some(common)) => {
            crate::cli::print_usage_text(
                &format!(
                    "switchboard server status: {}",
                    invites::unexpected(&common.positional)
                ),
                "switchboard server status --help",
            );
            return 2;
        }
        Ok(None) => {
            crate::cli::print_help(&format!("{USAGE}{HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server status --help");
            return 2;
        }
    };
    let path = common.config.clone();
    let data = match read_file(&path) {
        Ok((data, _)) => data,
        Err(_) => {
            let missing = matches!(
                std::fs::symlink_metadata(&path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            );
            return if missing {
                unavailable(
                    common.json,
                    ui::prompt::Problem::new("There's no Switchboard server on this machine.")
                        .detail(format!("{path} doesn't exist."))
                        .fix("set one up with `switchboard init`."),
                    &format!("there is no server config at {path}"),
                )
            } else {
                unavailable(
                    common.json,
                    ui::prompt::Problem::new("You can't read the server's configuration as this user.")
                        .detail(format!("`switchboard server status` reads {path}, which only root and the server's account can read."))
                        .fix(format!("run it with sudo: `{SUDO} server status`")),
                    &format!("cannot read the server config {path}; run this as root or as the server's account"),
                )
            };
        }
    };
    let settings = match parse_settings(&data, &path) {
        Ok(settings) => settings,
        Err(message) => {
            return unavailable(
                common.json,
                ui::prompt::Problem::new(format!("The server config {path} is invalid."))
                    .detail(format!("{message}."))
                    .without_fix(),
                &message,
            )
        }
    };
    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    let owner = std::fs::symlink_metadata(&settings.peers).map(|info| info.uid());
    if euid != 0 && owner.as_ref().is_ok_and(|uid| *uid != euid) {
        return unavailable(
            common.json,
            ui::prompt::Problem::new("Only root and the server's account can check the server.")
                .fix(format!("run it with sudo: `{SUDO} server status`")),
            "run this as root or as the server's service account",
        );
    }
    let report = gather(settings);
    let found = problems(&report);
    if common.json {
        println!(
            "{}",
            crate::wirejson::dumps(&Value::Object(json(&report, &found)))
        );
    } else {
        let theme = Theme::for_fd(1);
        print!(
            "{}",
            render(&report, &found, &theme, crate::cli::format::output_width())
        );
    }
    if found.is_empty() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    const NOW: f64 = 1_793_318_400.0;

    fn settings(config: &str) -> Settings {
        parse_settings(config.as_bytes(), "/etc/switchboard/server.json").unwrap()
    }

    fn private_settings() -> Settings {
        settings(
            r#"{"tls": "private", "names": ["sb.lan", "10.0.0.5"], "db": "/var/lib/switchboard/messages.sqlite3", "peers_file": "/var/lib/switchboard/peers.json"}"#,
        )
    }

    fn acme_settings() -> Settings {
        settings(
            r#"{"tls": "acme", "hostname": "sb.example.com", "db": "/var/lib/switchboard/messages.sqlite3", "peers_file": "/var/lib/switchboard/peers.json"}"#,
        )
    }

    fn running() -> Service {
        Service {
            load: "loaded".into(),
            active: "active".into(),
            sub: "running".into(),
            unit_file: "enabled".into(),
            pid: Some(812),
            active_since: Some(NOW - 7200.0),
            inactive_since: None,
            result: "success".into(),
            exit_status: Some(0),
        }
    }

    fn cert(days_left: f64, lifetime_days: f64, self_signed: bool) -> Described {
        let after = NOW + days_left * 86400.0;
        Described {
            subject: "sb.example.com".into(),
            issuer: if self_signed {
                "Switchboard server".into()
            } else {
                "Let's Encrypt R11".into()
            },
            self_signed,
            names: vec!["sb.example.com".into()],
            not_before: UNIX_EPOCH + Duration::from_secs_f64(after - lifetime_days * 86400.0),
            not_after: UNIX_EPOCH + Duration::from_secs_f64(after),
            sha256: "ab".repeat(32),
        }
    }

    fn report(settings: Settings) -> Report {
        let certificate = Certificate::Read(cert(60.0, 90.0, settings.mode == Mode::Private));
        Report {
            service: Some(running()),
            version: Some(crate::VERSION.into()),
            answer: Answer::Yes(Some("ab".repeat(32))),
            certificate,
            service_uid: Some(997),
            listeners: Some(vec![Listener {
                address: "::".parse().unwrap(),
                port: settings.port.unwrap_or(0),
                uid: 997,
            }]),
            ipv4_connects: true,
            socket_present: false,
            machines: Ok((2, 2)),
            invites: Ok(1),
            database: Some(1_234_567),
            journal: true,
            errors: Vec::new(),
            now: NOW,
            settings,
        }
    }

    fn checks(report: &Report) -> Vec<(&'static str, Level, String)> {
        problems(report)
            .into_iter()
            .map(|p| (p.check, p.level, p.summary))
            .collect()
    }

    #[test]
    fn configs_give_the_mode_address_and_files() {
        let private = private_settings();
        assert_eq!(private.mode, Mode::Private);
        assert_eq!(private.port, Some(8743));
        assert_eq!(private.address().unwrap(), "https://sb.lan:8743");
        assert_eq!(
            private.certificate_path().unwrap(),
            "/var/lib/switchboard/tls/private-cert.pem"
        );
        assert_eq!(private.lock, "/var/lib/switchboard/peers.json.lock");
        let acme = acme_settings();
        assert_eq!(acme.address().unwrap(), "https://sb.example.com");
        assert_eq!(
            acme.certificate_path().unwrap(),
            "/var/lib/switchboard/tls/acme-sb.example.com-cert.pem"
        );
        assert_eq!(
            acme.install_command(),
            "sudo switchboard server install --tls acme --domain sb.example.com"
        );
        let caddy = settings(
            r#"{"unix_socket": "/run/switchboard/hub.sock", "db": "state/m.db", "peers_file": "peers.json"}"#,
        );
        assert_eq!(caddy.mode, Mode::Caddy);
        assert_eq!(caddy.port, None);
        assert_eq!(caddy.address(), None);
        assert_eq!(caddy.db, "/etc/switchboard/state/m.db");
        let custom = settings(
            r#"{"tls": "private", "names": ["::1"], "listen": "127.0.0.1", "port": 9000, "db": "m.db", "peers_file": "p.json"}"#,
        );
        assert_eq!(custom.address().unwrap(), "https://[::1]:9000");
        assert_eq!(
            custom.probe_address().unwrap(),
            "127.0.0.1:9000".parse().unwrap()
        );
        assert!(parse_settings(b"[]", "/x").is_err());
        assert!(parse_settings(br#"{"db": "a"}"#, "/x").is_err());
        assert!(
            parse_settings(br#"{"tls": "other", "db": "a", "peers_file": "b"}"#, "/x").is_err()
        );
    }

    #[test]
    fn a_healthy_server_has_no_problems() {
        for settings in [private_settings(), acme_settings()] {
            let report = report(settings);
            assert_eq!(checks(&report), [], "{:?}", report.settings.mode);
            let value = json(&report, &[]);
            assert_eq!(value["ok"], true);
            assert_eq!(value["service"]["state"], "running");
            assert_eq!(value["machines"]["enrolled"], 2);
            assert_eq!(value["machines"]["seen_last_10_minutes"], 2);
            assert_eq!(value["invites"]["open"], 1);
            assert_eq!(
                value["listening"],
                serde_json::json!([
                    "[::]:".to_string() + &report.settings.port.unwrap().to_string()
                ])
            );
            assert_eq!(
                (value["ipv4"].clone(), value["ipv6"].clone()),
                (Value::Bool(true), Value::Bool(true))
            );
            assert_eq!(value["certificate"]["days_left"], 60);
            assert_eq!(value["certificate"]["served"], true);
        }
    }

    #[test]
    fn the_service_state_decides_the_first_problem() {
        let mut report = report(private_settings());
        report.service = Some(Service {
            load: "not-found".into(),
            ..Default::default()
        });
        report.answer = Answer::No;
        let found = problems(&report);
        assert_eq!(found[0].summary, "The server's service isn't installed.");
        assert_eq!(
            found[0].fix.as_deref(),
            Some("install it again with `sudo switchboard server install --tls private`.")
        );
        // Not running: no separate complaint that it doesn't answer.
        assert_eq!(found.len(), 1);
        report.service = Some(Service {
            active: "failed".into(),
            sub: "failed".into(),
            load: "loaded".into(),
            inactive_since: Some(NOW - 60.0),
            exit_status: Some(1),
            ..Default::default()
        });
        assert_eq!(checks(&report)[0].2, "The server stopped after a failure.");
        assert_eq!(
            service_text(&report).split(',').nth(1),
            Some(" exit status 1")
        );
        // Another program holds the port.
        report.listeners = Some(vec![Listener {
            address: "0.0.0.0".parse().unwrap(),
            port: 8743,
            uid: 0,
        }]);
        assert_eq!(
            checks(&report)[0].2,
            "The server isn't running, and another program is listening on port 8743."
        );
        assert_eq!(
            listening_text(&report).unwrap(),
            "port 8743 is held by another program"
        );
        report.service = Some(Service {
            load: "loaded".into(),
            active: "activating".into(),
            sub: "auto-restart".into(),
            ..Default::default()
        });
        assert_eq!(
            checks(&report)[0].2,
            "The server keeps failing and restarting."
        );
        let mut disabled = running();
        disabled.unit_file = "disabled".into();
        let mut report = super::tests::report(private_settings());
        report.service = Some(disabled);
        assert_eq!(
            checks(&report),
            [(
                "service",
                Level::Warning,
                "The server won't start when this machine boots.".to_string()
            )]
        );
        report.service = None;
        assert_eq!(checks(&report)[0].0, "service");
        assert_eq!(checks(&report)[0].1, Level::Error);
    }

    #[test]
    fn certificates_warn_before_they_expire() {
        // A 90-day ACME certificate: renewed at 30 days left, a warning
        // under 21; a 45-day one scales down.
        for (days, lifetime, warned) in [
            (25.0, 90.0, false),
            (20.5, 90.0, true),
            (11.0, 45.0, false),
            (10.0, 45.0, true),
        ] {
            let mut report = report(acme_settings());
            report.certificate = Certificate::Read(cert(days, lifetime, false));
            let found = checks(&report);
            assert_eq!(!found.is_empty(), warned, "{days} of {lifetime}: {found:?}");
            if warned {
                assert_eq!(found[0].1, Level::Warning);
                assert!(found[0].2.contains("should have been renewed"), "{found:?}");
            }
        }
        let mut report = report(acme_settings());
        report.certificate = Certificate::Read(cert(-1.0, 90.0, false));
        assert_eq!(checks(&report)[0].1, Level::Error);
        assert!(certificate_text(&report)
            .starts_with("sb.example.com, from Let's Encrypt R11, expired "));
        // No certificate yet: the alert the server answers with is expected.
        report.certificate = Certificate::Missing;
        report.answer = Answer::Alert;
        let found = checks(&report);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].2.contains("no certificate from Let's Encrypt yet"));
        // A private certificate lasts a century; three weeks' notice.
        let mut report = super::tests::report(private_settings());
        report.certificate = Certificate::Read(cert(19_000.0, 20_000.0, true));
        assert_eq!(checks(&report), []);
        assert!(
            certificate_text(&report).starts_with("self-signed for sb.example.com, valid until ")
        );
        report.certificate = Certificate::Read(cert(20.0, 20_000.0, true));
        assert_eq!(checks(&report)[0].2, "The certificate expires in 20 days.");
        // The server serves another certificate than the one on disk.
        let mut report = super::tests::report(private_settings());
        report.answer = Answer::Yes(Some("cd".repeat(32)));
        assert_eq!(checks(&report)[0].2, "The server presents a different certificate than /var/lib/switchboard/tls/private-cert.pem.");
        assert_eq!(json(&report, &[])["certificate"]["served"], false);
    }

    #[test]
    fn errors_since_the_start_need_attention_and_older_ones_do_not() {
        let mut report = report(private_settings());
        let error = |time: f64| Event {
            time,
            source: "server",
            event: "request_error".into(),
            detail: "boom".into(),
            level: Level::Error,
            message: "Exception occurred during processing of request: boom".into(),
        };
        report.errors = vec![error(NOW - 10_000.0)];
        assert_eq!(checks(&report), []);
        report.errors.push(error(NOW - 60.0));
        assert_eq!(
            checks(&report),
            [(
                "errors",
                Level::Warning,
                "The server has logged 1 error since it started.".to_string()
            )]
        );
        let value = json(&report, &problems(&report));
        assert_eq!(value["ok"], false);
        assert_eq!(value["errors"]["last_day"], 2);
        assert_eq!(value["errors"]["since_start"], 1);
        assert_eq!(value["errors"]["recent"][1]["event"], "request_error");
        assert_eq!(
            value["problems"][0]["fix"],
            "see them in context with sudo switchboard server logs."
        );
    }

    #[test]
    fn a_server_that_does_not_answer_is_a_failure() {
        let mut report = report(private_settings());
        report.answer = Answer::No;
        assert_eq!(
            checks(&report),
            [(
                "answer",
                Level::Error,
                "The server doesn't answer on port 8743.".to_string()
            )]
        );
        report.machines = Err("unsafe registry owner, mode, or type".into());
        assert_eq!(
            checks(&report)[1].2,
            "The registry can't be read: unsafe registry owner, mode, or type."
        );
    }

    #[test]
    fn counts_read_naturally() {
        assert_eq!(machines_text(0, 0), "none joined yet");
        assert_eq!(
            machines_text(1, 1),
            "1 machine, seen in the last 10 minutes"
        );
        assert_eq!(
            machines_text(1, 0),
            "1 machine, not seen in the last 10 minutes"
        );
        assert_eq!(
            machines_text(2, 2),
            "2 machines, both seen in the last 10 minutes"
        );
        assert_eq!(
            machines_text(5, 5),
            "5 machines, all seen in the last 10 minutes"
        );
        assert_eq!(
            machines_text(5, 3),
            "5 machines, 3 seen in the last 10 minutes"
        );
        assert_eq!(
            machines_text(5, 0),
            "5 machines, none seen in the last 10 minutes"
        );
        assert_eq!(invites_text(1), "1 open invite");
        assert_eq!(invites_text(0), "none open");
        assert_eq!(size_text(512), "512 bytes");
        assert_eq!(size_text(48_200), "48 KB");
        assert_eq!(size_text(1_234_567), "1.2 MB");
        assert_eq!(
            and_list(&["a".into(), "b".into(), "c".into()]),
            "a, b, and c"
        );
    }

    #[test]
    fn the_screen_puts_problems_first_then_facts() {
        let plain = Theme::plain();
        let healthy = report(private_settings());
        let text = render(&healthy, &[], &plain, 100);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "✓ The server is healthy.");
        assert_eq!(lines[1], "");
        assert!(
            lines[2].starts_with("  Service      running since "),
            "{text}"
        );
        assert!(
            text.contains(&format!("\n  Version      {}\n", crate::VERSION)),
            "{text}"
        );
        assert!(
            text.contains("\n  Mode         private network\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  Address      https://sb.lan:8743\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!("\n  Pin          {}\n", "ab".repeat(32))),
            "{text}"
        );
        assert!(
            text.contains("\n  Listening    [::]:8743, IPv6 and IPv4\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  Machines     2 machines, both seen in the last 10 minutes\n"),
            "{text}"
        );
        assert!(text.contains("\n  Invites      1 open invite\n"), "{text}");
        assert!(text.ends_with("\n  Database     1.2 MB\n"), "{text}");
        // Never the lists themselves.
        assert!(!text.contains("laptop"));
        let mut broken = report(private_settings());
        broken.answer = Answer::No;
        broken.errors = vec![Event {
            time: NOW - 60.0,
            source: "server",
            event: "server_error".into(),
            detail: "port in use".into(),
            level: Level::Error,
            message: String::new(),
        }];
        let found = problems(&broken);
        let text = render(&broken, &found, &plain, 100);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "✗ The server doesn't answer on port 8743.");
        assert_eq!(lines[1], "  To fix, see why with");
        assert_eq!(lines[2], "      sudo switchboard server logs");
        assert_eq!(
            lines[3],
            "! The server has logged 1 error since it started."
        );
        assert!(
            text.contains("\n  Recent errors  1 in the last day, times in "),
            "{text}"
        );
        assert!(
            text.trim_end().ends_with("server_error  port in use"),
            "{text}"
        );
    }
}
