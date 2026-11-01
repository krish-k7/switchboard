//! Installing, moving an old service, upgrading, and uninstalling, against
//! the built binary with a private HOME, stateful fake `launchctl` and
//! `systemctl` first on PATH (they record calls and remember which jobs are
//! loaded in files), and a fake GitHub Releases server on 127.0.0.1. Nothing
//! touches the real installation, service managers, or network.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use switchboard::cli::service::{LABEL, LEGACY_LABEL, LEGACY_UNIT, UNIT};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_switchboard")
}

const LINUX: bool = cfg!(target_os = "linux");

struct Env {
    root: PathBuf,
    port: Option<u16>,
    vars: Vec<(String, String)>,
}

impl Drop for Env {
    fn drop(&mut self) {
        clear_flags(&self.root);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Clear file flags under `path` (a test makes a service file immutable on
/// macOS, and the installer's backup copy keeps the flag), so the whole
/// tree can be removed.
#[cfg(target_os = "macos")]
fn clear_flags(path: &std::path::Path) {
    use std::os::macos::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.st_flags() != 0 {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: lchflags(2) on a NUL-terminated path.
        unsafe { libc::lchflags(c.as_ptr(), 0) };
    }
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            clear_flags(&entry.path());
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn clear_flags(_path: &std::path::Path) {}

/// The fake service managers: one file per loaded job under `state/`.
const LAUNCHCTL: &str = r#"#!/bin/sh
echo "launchctl $*" >> "$SB_TEST/calls"
case "$1" in
print) [ -e "$SB_TEST/state/${2##*/}" ] ;;
bootout)
    if [ -e "$SB_TEST/fail-stop" ]; then echo 'Operation not permitted' >&2; exit 5; fi
    [ -e "$SB_TEST/state/${2##*/}" ] || exit 3
    rm -f "$SB_TEST/state/${2##*/}" ;;
bootstrap)
    label=$(basename "$3" .plist); [ -e "$3" ] || exit 5
    if [ -e "$SB_TEST/state/disabled-$label" ]; then echo 'Bootstrap failed: 5: Input/output error' >&2; exit 5; fi
    : > "$SB_TEST/state/$label" ;;
kickstart) [ -e "$SB_TEST/state/${2##*/}" ] ;;
disable)
    if [ -e "$SB_TEST/fail-disable" ]; then echo 'Operation not permitted' >&2; exit 1; fi
    : > "$SB_TEST/state/disabled-${2##*/}" ;;
enable)
    if [ -e "$SB_TEST/fail-enable" ]; then echo 'Operation not permitted; domain locked' >&2; exit 1; fi
    rm -f "$SB_TEST/state/disabled-${2##*/}" ;;
print-disabled)
    echo "disabled services = {"
    for f in "$SB_TEST"/state/disabled-*; do [ -e "$f" ] && printf '\t"%s" => disabled\n' "${f##*/disabled-}"; done
    echo "}" ;;
*) exit 0 ;;
esac
"#;

const SYSTEMCTL: &str = r#"#!/bin/sh
echo "systemctl $*" >> "$SB_TEST/calls"
[ "$1" = --user ] && shift
command=$1; shift
[ "$1" = --quiet ] && shift
case "$command" in
is-active) [ -e "$SB_TEST/state/$1" ] ;;
stop)
    if [ -e "$SB_TEST/fail-stop" ]; then echo 'Operation not permitted' >&2; exit 5; fi
    rm -f "$SB_TEST/state/$1" ;;
start) : > "$SB_TEST/state/$1" ;;
disable)
    if [ -e "$SB_TEST/fail-disable" ]; then echo 'Access denied' >&2; exit 1; fi
    if [ "$1" = --now ]; then shift; rm -f "$SB_TEST/state/$1"; fi
    rm -f "$XDG_CONFIG_HOME/systemd/user/default.target.wants/$1" ;;
enable)
    if [ -e "$SB_TEST/fail-enable" ]; then echo 'Access denied' >&2; exit 1; fi
    [ "$1" = --now ] && shift
    ln -s "$XDG_CONFIG_HOME/systemd/user/$1" "$XDG_CONFIG_HOME/systemd/user/default.target.wants/$1" ;;
*) exit 0 ;;
esac
"#;

impl Env {
    fn new() -> Env {
        let template = std::ffi::CString::new("/tmp/sblife-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        let made = unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) };
        assert!(!made.is_null());
        raw.pop();
        let root = std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap();
        for dir in ["home", "bin", "state", "tmp"] {
            std::fs::create_dir(root.join(dir)).unwrap();
        }
        let env = Env {
            root,
            port: None,
            vars: Vec::new(),
        };
        env.write_exec("bin/launchctl", LAUNCHCTL);
        env.write_exec("bin/systemctl", SYSTEMCTL);
        env
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn write_exec(&self, relative: &str, body: &str) {
        let path = self.root.join(relative);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_program(binary(), args)
    }

    fn run_program(&self, program: &str, args: &[&str]) -> Output {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("SB_TEST", &self.root)
            .env("TMPDIR", self.root.join("tmp"))
            .env("NO_COLOR", "1")
            .env("SWITCHBOARD_NO_UPDATE_CHECK", "1")
            .env("COLUMNS", "100")
            .current_dir(&self.root);
        if LINUX {
            command.env("XDG_CONFIG_HOME", self.home().join(".config"));
        }
        if let Some(port) = self.port {
            command
                .env("SWITCHBOARD_REPO", "owner/repo")
                .env("SWITCHBOARD_GITHUB", format!("http://127.0.0.1:{port}"))
                .env(
                    "SWITCHBOARD_GITHUB_API",
                    format!("http://127.0.0.1:{port}/api"),
                );
        }
        command.envs(self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        command.output().unwrap()
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.root.join("calls")).unwrap_or_default()
    }

    fn clear_calls(&self) {
        let _ = std::fs::remove_file(self.root.join("calls"));
    }

    /// Mark a job (launchd label or systemd unit) as loaded or not.
    fn set_loaded(&self, name: &str, loaded: bool) {
        let path = self.root.join("state").join(name);
        if loaded {
            std::fs::write(path, "").unwrap();
        } else {
            let _ = std::fs::remove_file(path);
        }
    }

    fn loaded(&self, name: &str) -> bool {
        self.root.join("state").join(name).exists()
    }

    fn service_file(&self, legacy: bool) -> PathBuf {
        let home = self.home();
        match (LINUX, legacy) {
            (true, false) => home.join(".config/systemd/user").join(UNIT),
            (true, true) => home.join(".config/systemd/user").join(LEGACY_UNIT),
            (false, false) => home.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            (false, true) => home.join(format!("Library/LaunchAgents/{LEGACY_LABEL}.plist")),
        }
    }
}

/// The current service name as the fake managers record it.
fn current() -> &'static str {
    if LINUX {
        UNIT
    } else {
        LABEL
    }
}

fn legacy() -> &'static str {
    if LINUX {
        LEGACY_UNIT
    } else {
        LEGACY_LABEL
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_out(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("stdout: {}\nstderr: {}", stdout(output), stderr(output)))
}

/// Write a service file an earlier installer wrote, under the old name.
fn write_legacy_service(env: &Env) -> PathBuf {
    let path = env.service_file(true);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let destination = env.home().join(".local/share/switchboard");
    let text = if LINUX {
        switchboard::cli::install::systemd_unit(
            &destination.join("switchboard").to_string_lossy(),
            &destination.to_string_lossy(),
            &env.home()
                .join(".config/switchboard/bridge.json")
                .to_string_lossy(),
        )
        .replace(
            switchboard::cli::install::MARKER,
            switchboard::cli::install::LEGACY_MARKER,
        )
    } else {
        switchboard::cli::install::launch_agent(
            &destination.join("switchboard").to_string_lossy(),
            &destination.to_string_lossy(),
            &env.home()
                .join(".config/switchboard/bridge.json")
                .to_string_lossy(),
        )
        .replace(LABEL, LEGACY_LABEL)
    };
    std::fs::write(&path, text).unwrap();
    if LINUX {
        let wants = path.parent().unwrap().join("default.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        std::os::unix::fs::symlink(&path, wants.join(LEGACY_UNIT)).unwrap();
    }
    path
}

/// An existing installation's data: a config and a bridge log.
fn write_user_data(env: &Env) -> (PathBuf, PathBuf) {
    let config_dir = env.home().join(".config/switchboard");
    std::fs::create_dir_all(config_dir.join("identity")).unwrap();
    let config = config_dir.join("bridge.json");
    std::fs::write(&config, "{\"enabled\": true}\n").unwrap();
    std::fs::write(config_dir.join("identity/key.pem"), "key\n").unwrap();
    let share = env.home().join(".local/share/switchboard");
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(share.join(".switchboard-install"), "1\n").unwrap();
    let log = share.join("daemon.log");
    std::fs::write(&log, "earlier log line\n").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600)).unwrap();
    (config, log)
}

#[test]
fn install_moves_a_running_old_service_to_the_new_name() {
    let env = Env::new();
    let (config, log) = write_user_data(&env);
    let old = write_legacy_service(&env);
    env.set_loaded(legacy(), true);
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    assert_eq!(result["migrated_from"], json!(old));
    assert_eq!(result["started"], json!(true));
    assert!(result.get("restart").is_none());
    // The old service is stopped and gone (with a backup), the new one runs.
    assert!(!old.exists());
    assert!(!env.loaded(legacy()));
    assert!(env.loaded(current()));
    assert!(env.service_file(false).exists());
    let backups = env.home().join(".local/share/switchboard/backups");
    let backup = std::fs::read_dir(&backups)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert!(backup.path().join(old.file_name().unwrap()).exists());
    if LINUX {
        let wants = env.home().join(".config/systemd/user/default.target.wants");
        assert!(std::fs::symlink_metadata(wants.join(LEGACY_UNIT)).is_err());
        assert!(std::fs::symlink_metadata(wants.join(UNIT)).is_ok());
        let calls = env.calls();
        assert!(
            calls.contains(&format!("systemctl --user stop {LEGACY_UNIT}\n")),
            "{calls}"
        );
        assert!(
            calls.contains(&format!("systemctl --user start {UNIT}\n")),
            "{calls}"
        );
    } else {
        let calls = env.calls();
        assert!(calls.contains("launchctl bootout gui/"), "{calls}");
        assert!(calls.contains(&format!("/{LEGACY_LABEL}\n")), "{calls}");
        assert!(calls.contains(&format!("{LABEL}.plist\n")), "{calls}");
    }
    // Logs and configuration are untouched.
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "earlier log line\n");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "{\"enabled\": true}\n"
    );
    // A second run has nothing to move and starts nothing.
    env.clear_calls();
    let again = json_out(&env.run(&["install", "--json"]));
    assert!(again.get("migrated_from").is_none());
    assert_eq!(again["started"], json!(false));
    assert_eq!(env.calls(), "");
}

#[test]
fn install_removes_a_stopped_old_service_without_starting_anything() {
    let env = Env::new();
    write_user_data(&env);
    let old = write_legacy_service(&env);
    let output = env.run(&["install"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.starts_with("Installed switchboard "), "{text}");
    assert!(
        text.contains("Moved the bridge service from its old name (~/"),
        "{text}"
    );
    assert!(text.contains("(not started)"), "{text}");
    assert!(
        text.ends_with("Start the bridge with: switchboard up\n"),
        "{text}"
    );
    assert!(!old.exists());
    assert!(!env.loaded(current()));
    let calls = env.calls();
    assert!(
        !calls.contains("bootstrap") && !calls.contains(" start "),
        "{calls}"
    );
    assert!(
        !calls.contains("bootout") && !calls.contains(" stop "),
        "{calls}"
    );
}

#[test]
fn install_leaves_an_unmanaged_old_service_alone_and_keeps_one_it_cannot_stop() {
    let env = Env::new();
    let old = env.service_file(true);
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    let foreign = if LINUX {
        "[Service]\nExecStart=/bin/true\n"
    } else {
        "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>x</string></dict></plist>\n"
    };
    std::fs::write(&old, foreign).unwrap();
    env.set_loaded(legacy(), true);
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(json_out(&output).get("migrated_from").is_none());
    assert_eq!(std::fs::read_to_string(&old).unwrap(), foreign);
    assert!(env.loaded(legacy()));
    assert_eq!(env.calls(), "");

    // A managed old service that refuses to stop keeps its file, and the
    // new one is not installed beside it (both would load at login).
    let env = Env::new();
    let old = write_legacy_service(&env);
    env.set_loaded(legacy(), true);
    std::fs::write(env.root.join("fail-stop"), "").unwrap();
    let output = env.run(&["install"]);
    assert_eq!(output.status.code(), Some(1));
    let error = stderr(&output);
    assert!(
        error.starts_with(
            "switchboard install: could not stop the bridge service installed under its old name"
        ),
        "{error}"
    );
    assert!(
        error.contains("the old service file is still installed and the new one is not"),
        "{error}"
    );
    let stop = if LINUX {
        format!("systemctl --user stop {LEGACY_UNIT}")
    } else {
        "launchctl bootout gui/".to_string()
    };
    assert!(error.contains(&stop), "{error}");
    assert!(old.exists());
    assert!(!env.service_file(false).exists());
    assert!(env.loaded(legacy()));
    assert!(!env.loaded(current()));
    if LINUX {
        let wants = env.home().join(".config/systemd/user/default.target.wants");
        assert!(std::fs::symlink_metadata(wants.join(UNIT)).is_err());
    }
    // Once it stops, install moves it.
    std::fs::remove_file(env.root.join("fail-stop")).unwrap();
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!old.exists());
    assert!(env.service_file(false).exists());
    assert!(env.loaded(current()));
}

#[test]
fn a_failed_move_removes_a_new_service_file_left_beside_the_old_one() {
    // An earlier installer left both files; the old service will not stop.
    let env = Env::new();
    write_user_data(&env);
    let old = write_legacy_service(&env);
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let new = env.service_file(false);
    let text = std::fs::read_to_string(&new).unwrap();
    write_legacy_service(&env);
    env.set_loaded(legacy(), true);
    std::fs::write(env.root.join("fail-stop"), "").unwrap();
    let output = env.run(&["install"]);
    assert_eq!(output.status.code(), Some(1));
    let error = stderr(&output);
    assert!(
        error.contains("A new service file an earlier run left beside it"),
        "{error}"
    );
    assert!(error.contains("was removed (a copy is in "), "{error}");
    assert!(old.exists());
    assert!(!new.exists());
    if LINUX {
        let wants = env.home().join(".config/systemd/user/default.target.wants");
        assert!(std::fs::symlink_metadata(wants.join(UNIT)).is_err());
    }
    // The copy is kept with the installer's backups.
    let backups = env.home().join(".local/share/switchboard/backups");
    let kept = std::fs::read_dir(&backups)
        .unwrap()
        .filter_map(|entry| {
            std::fs::read_to_string(entry.unwrap().path().join(new.file_name().unwrap())).ok()
        })
        .any(|copy| copy == text);
    assert!(kept);
}

/// Make the old service file impossible to remove while the new one can
/// still be written: on Linux its enabling link's directory is read-only,
/// on macOS the file is user-immutable. Returns a function that undoes it.
#[cfg(target_os = "linux")]
fn block_legacy_removal(env: &Env, _old: &std::path::Path) -> Box<dyn Fn()> {
    let wants = env.home().join(".config/systemd/user/default.target.wants");
    std::fs::set_permissions(&wants, std::fs::Permissions::from_mode(0o500)).unwrap();
    Box::new(move || {
        std::fs::set_permissions(&wants, std::fs::Permissions::from_mode(0o700)).unwrap();
    })
}

#[cfg(not(target_os = "linux"))]
fn block_legacy_removal(_env: &Env, old: &std::path::Path) -> Box<dyn Fn()> {
    let path = std::ffi::CString::new(old.to_str().unwrap()).unwrap();
    // SAFETY: chflags(2) on a NUL-terminated path.
    assert_eq!(
        unsafe { libc::chflags(path.as_ptr(), libc::UF_IMMUTABLE as _) },
        0
    );
    Box::new(move || {
        // SAFETY: as above.
        unsafe { libc::chflags(path.as_ptr(), 0) };
    })
}

#[test]
fn an_old_service_that_cannot_be_removed_keeps_the_new_one_out() {
    let env = Env::new();
    write_user_data(&env);
    let old = write_legacy_service(&env);
    env.set_loaded(legacy(), true);
    let undo = block_legacy_removal(&env, &old);
    let output = env.run(&["install"]);
    undo();
    assert_eq!(output.status.code(), Some(1));
    let error = stderr(&output);
    assert!(
        error.starts_with(
            "switchboard install: could not remove the bridge service installed under its old name"
        ),
        "{error}"
    );
    assert!(
        error.contains("it was stopped, so the bridge is not running now"),
        "{error}"
    );
    assert!(
        error.contains("the old service file is still installed and the new one is not"),
        "{error}"
    );
    assert!(old.exists());
    assert!(!env.service_file(false).exists());
    assert!(!env.loaded(current()));
    // With the obstacle gone, install finishes the move.
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!old.exists());
    assert!(env.service_file(false).exists());
    // The backup of the old file kept its flag; teardown still removes
    // everything.
    let root = env.root.clone();
    drop(env);
    assert!(!root.exists(), "{} was left behind", root.display());
}

// Releases ------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    host: String,
    authorization: Option<String>,
}

type Releases = Arc<Mutex<HashMap<String, Vec<(String, Vec<u8>)>>>>;

struct ReleaseServer {
    port: u16,
    releases: Releases,
    latest: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

impl ReleaseServer {
    fn start() -> ReleaseServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = ReleaseServer {
            port,
            releases: Arc::new(Mutex::new(HashMap::new())),
            latest: Arc::new(Mutex::new(String::new())),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let (releases, latest, seen) = (
            server.releases.clone(),
            server.latest.clone(),
            server.seen.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut headers = HashMap::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let authorization = headers.get("authorization").cloned();
                seen.lock().unwrap().push(Seen {
                    path: path.clone(),
                    host: headers.get("host").cloned().unwrap_or_default(),
                    authorization: authorization.clone(),
                });
                let base = format!("http://127.0.0.1:{port}");
                let storage = format!("http://localhost:{port}");
                let file = |version: &str, name: &str| -> Option<Vec<u8>> {
                    releases.lock().unwrap().get(version).and_then(|files| {
                        files
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, d)| d.clone())
                    })
                };
                let authorized = authorization.as_deref() == Some("Bearer test-token");
                let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                match parts.as_slice() {
                    ["owner", "repo", "releases", "latest"] => {
                        let tag = format!("v{}", latest.lock().unwrap());
                        respond(
                            &mut stream,
                            "302 Found",
                            &[("Location", format!("{base}/owner/repo/releases/tag/{tag}"))],
                            b"",
                        );
                    }
                    ["owner", "repo", "releases", "download", tag, name] => {
                        let version = tag.trim_start_matches('v');
                        if file(version, name).is_some() {
                            respond(
                                &mut stream,
                                "302 Found",
                                &[("Location", format!("{storage}/storage/{version}/{name}"))],
                                b"",
                            );
                        } else {
                            respond(&mut stream, "404 Not Found", &[], b"Not Found");
                        }
                    }
                    ["storage", version, name] => match file(version, name) {
                        Some(data) => respond(&mut stream, "200 OK", &[], &data),
                        None => respond(&mut stream, "404 Not Found", &[], b""),
                    },
                    ["api", "repos", "owner", "repo", "releases", rest @ ..] if !authorized => {
                        let _ = rest;
                        respond(
                            &mut stream,
                            "404 Not Found",
                            &[],
                            b"{\"message\": \"Not Found\"}",
                        );
                    }
                    ["api", "repos", "owner", "repo", "releases", "assets", id] => {
                        let (version, name) = id.split_once('~').unwrap_or(("", ""));
                        if headers.get("accept").map(String::as_str)
                            != Some("application/octet-stream")
                        {
                            respond(&mut stream, "415 Unsupported Media Type", &[], b"");
                        } else if file(version, name).is_some() {
                            respond(
                                &mut stream,
                                "302 Found",
                                &[("Location", format!("{storage}/storage/{version}/{name}"))],
                                b"",
                            );
                        } else {
                            respond(&mut stream, "404 Not Found", &[], b"");
                        }
                    }
                    ["api", "repos", "owner", "repo", "releases", which @ ..] => {
                        let version = match which {
                            ["latest"] => Some(latest.lock().unwrap().clone()),
                            ["tags", tag] => Some(tag.trim_start_matches('v').to_string()),
                            _ => None,
                        };
                        let files = version
                            .as_ref()
                            .and_then(|v| releases.lock().unwrap().get(v).cloned());
                        match (version, files) {
                            (Some(version), Some(files)) => {
                                let assets: Vec<Value> = files.iter().map(|(name, _)| json!({
                                    "name": name,
                                    "url": format!("{base}/api/repos/owner/repo/releases/assets/{version}~{name}"),
                                })).collect();
                                let body =
                                    json!({"tag_name": format!("v{version}"), "assets": assets});
                                respond(
                                    &mut stream,
                                    "200 OK",
                                    &[("Content-Type", "application/json".into())],
                                    body.to_string().as_bytes(),
                                );
                            }
                            _ => respond(&mut stream, "404 Not Found", &[], b"{}"),
                        }
                    }
                    _ => respond(&mut stream, "404 Not Found", &[], b""),
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        server
    }

    /// Publish release `version`: a stand-in binary that reports `version`
    /// and installs the binary under test, with its checksum.
    fn publish(&self, version: &str, real: &str) {
        self.publish_script(
            version,
            &format!("if [ \"$1\" = install ]; then shift; exec '{real}' install \"$@\"; fi"),
        );
    }

    /// Publish a stand-in binary that reports `version` and otherwise runs
    /// `body`.
    fn publish_script(&self, version: &str, body: &str) {
        let asset = switchboard::cli::release::asset_name().unwrap();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"switchboard {version}\"; exit 0; fi\n{body}\nexit 9\n"
        );
        let hash: String = Sha256::digest(script.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!("{}  switchboard-other\n{hash}  {asset}\n", "0".repeat(64));
        self.releases.lock().unwrap().insert(
            version.to_string(),
            vec![
                (asset, script.into_bytes()),
                ("SHA256SUMS".into(), sums.into_bytes()),
            ],
        );
    }

    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

fn current_version() -> String {
    let output = Command::new(binary()).arg("--version").output().unwrap();
    stdout(&output)
        .trim()
        .strip_prefix("switchboard ")
        .unwrap()
        .to_string()
}

#[test]
fn upgrade_installs_the_latest_release_and_restarts_a_running_bridge() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    // A machine whose bridge runs is set up.
    let config = env.home().join(".config/switchboard");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("bridge.json"), "{}").unwrap();
    env.set_loaded(current(), true);
    server.publish("99.0.0", binary());
    *server.latest.lock().unwrap() = "99.0.0".into();
    env.clear_calls();
    let output = env.run(&["cli", "upgrade", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        json_out(&output),
        json!({"ok": true, "action": "upgrade", "previous": current_version(), "version": "99.0.0",
               "changed": true, "restarted": true})
    );
    // The bridge was stopped and started again through the new program.
    let calls = env.calls();
    let (stop, start) = if LINUX {
        (
            format!("systemctl --user stop {UNIT}"),
            format!("systemctl --user start {UNIT}"),
        )
    } else {
        (
            "launchctl bootout gui/".to_string(),
            "launchctl bootstrap gui/".to_string(),
        )
    };
    let stop_at = calls.find(&stop).expect(&calls);
    assert!(calls[stop_at..].contains(&start), "{calls}");
    assert!(env.loaded(current()));
    // Downloads followed the redirect to the storage host.
    let seen = server.seen();
    let asset = switchboard::cli::release::asset_name().unwrap();
    assert!(seen.iter().any(|s| s.path == "/owner/repo/releases/latest"));
    assert!(seen
        .iter()
        .any(|s| s.path == format!("/storage/99.0.0/{asset}") && s.host.starts_with("localhost")));
    assert!(seen.iter().all(|s| s.authorization.is_none()));
    // Nothing temporary is left behind.
    assert_eq!(std::fs::read_dir(env.root.join("tmp")).unwrap().count(), 0);

    // Human output, with the bridge stopped: it stays stopped.
    env.set_loaded(current(), false);
    server.publish("99.1.0", binary());
    *server.latest.lock().unwrap() = "99.1.0".into();
    let output = env.run(&["cli", "upgrade"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "  Downloading Switchboard 99.1.0 ({asset})...\n✓ Installed Switchboard 99.1.0 (was {})\n\
             ! The bridge isn't running. Start it with\n      switchboard up\n",
            current_version()
        )
    );
    assert!(!env.loaded(current()));
}

#[test]
fn upgrade_reports_current_versions_and_refuses_downgrades_without_force() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let version = current_version();
    server.publish(&version, binary());
    *server.latest.lock().unwrap() = version.clone();
    let output = env.run(&["cli", "upgrade"]);
    assert_eq!(
        (output.status.code(), stdout(&output)),
        (
            Some(0),
            format!("✓ Switchboard {version} is the latest release\n")
        )
    );
    // Only the version was looked up; nothing was downloaded.
    assert!(server.seen().iter().all(|s| !s.path.contains("/download/")));
    let output = env.run(&["cli", "upgrade", &version, "--json"]);
    assert_eq!(json_out(&output)["changed"], json!(false));

    server.publish("0.0.1", binary());
    let output = env.run(&["cli", "upgrade", "0.0.1"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!("✗ 0.0.1 is older than the installed {version}.\n  To fix, pass --force to downgrade.\n")
    );
    let output = env.run(&["cli", "upgrade", "0.0.1", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        json_out(&output),
        json!({"ok": false, "error": format!("0.0.1 is older than the installed {version}; pass --force to downgrade")})
    );
    // With --force the running service is stopped and removed before the
    // older installer runs, then started again.
    let config = env.home().join(".config/switchboard");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("bridge.json"), "{}").unwrap();
    env.set_loaded(current(), true);
    let output = env.run(&["cli", "upgrade", "v0.0.1", "--force", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    assert_eq!(
        (result["version"].as_str(), result["restarted"].as_bool()),
        (Some("0.0.1"), Some(true))
    );
    assert!(env.loaded(current()));

    // An older installer that fails leaves this version installed and running.
    server.publish_script(
        "0.0.2",
        "if [ \"$1\" = install ]; then echo '{\"ok\": false, \"error\": \"disk full\"}'; exit 1; fi",
    );
    let output = env.run(&["cli", "upgrade", "0.0.2", "--force"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "✗ The 0.0.2 installer failed (disk full); this version is still installed.\n"
    );
    assert!(env.service_file(false).exists());
    assert!(env.loaded(current()));

    let output = env.run(&["cli", "upgrade", "nonsense"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("There's no release version called nonsense"),
        "{}",
        stderr(&output)
    );
    // Only semver characters get into a request: nothing is fetched for a
    // version carrying a line break, a path, or a query.
    server.seen();
    for bad in [
        "9.9.9-x\r\nX-Injected: 1",
        "9.9.9+../../etc",
        "1.0.0-../../../evil?x=1",
    ] {
        let output = env.run(&["cli", "upgrade", bad, "--force", "--json"]);
        assert_eq!(output.status.code(), Some(1), "{bad:?}");
        let error = json_out(&output)["error"].as_str().unwrap().to_string();
        assert!(
            error.starts_with("there's no release version called"),
            "{error}"
        );
    }
    assert!(server.seen().is_empty());
    // Nor for such a tag named as the latest release.
    *server.latest.lock().unwrap() = "9.9.9+../../etc".into();
    let output = env.run(&["cli", "upgrade", "--force"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Cannot compare versions"),
        "{}",
        stderr(&output)
    );
    assert!(server
        .seen()
        .iter()
        .all(|s| !s.path.contains("/download/") && !s.path.contains("/tags/")));
    let output = env.run(&["cli", "status", "--force"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).starts_with("✗ --force only work"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn upgrade_checks_downloads_and_keeps_tokens_off_other_hosts() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let installed = env.home().join(".local/bin/switchboard");
    let before = std::fs::read(&installed).unwrap();

    // A missing release.
    let output = env.run(&["cli", "upgrade", "98.0.0"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Could not download"),
        "{}",
        stderr(&output)
    );

    // A checksum mismatch installs nothing.
    server.publish("99.0.0", binary());
    {
        let mut releases = server.releases.lock().unwrap();
        let files = releases.get_mut("99.0.0").unwrap();
        files[1].1 = format!("{}  {}\n", "1".repeat(64), files[0].0).into_bytes();
    }
    let output = env.run(&["cli", "upgrade", "99.0.0"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Checksum mismatch"),
        "{}",
        stderr(&output)
    );
    // Read across the lines it wraps onto.
    assert!(
        stderr(&output)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("nothing was installed"),
        "{}",
        stderr(&output)
    );
    assert_eq!(std::fs::read(&installed).unwrap(), before);

    // A private repository: through the API with the token, which is sent to
    // the API host only, never to the storage host it redirects to.
    server.publish("99.0.0", binary());
    server.seen();
    env.vars.push(("GITHUB_TOKEN".into(), "test-token".into()));
    let output = env.run(&["cli", "upgrade", "99.0.0", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(json_out(&output)["version"], json!("99.0.0"));
    let seen = server.seen();
    assert!(seen
        .iter()
        .any(|s| s.path.starts_with("/api/")
            && s.authorization.as_deref() == Some("Bearer test-token")));
    let storage: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.path.starts_with("/storage/"))
        .collect();
    assert_eq!(storage.len(), 2, "{seen:?}");
    assert!(storage.iter().all(|s| s.authorization.is_none()));
    // A token that cannot read the repository, and one that is malformed.
    env.vars = vec![("GITHUB_TOKEN".into(), "wrong-token".into())];
    let output = env.run(&["cli", "upgrade"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "✗ No release (latest) in owner/repo, or GITHUB_TOKEN cannot read it.\n"
    );
    env.vars = vec![("GITHUB_TOKEN".into(), "bad\"token".into())];
    let output = env.run(&["cli", "upgrade"]);
    assert!(stderr(&output).contains("GITHUB_TOKEN has characters"));
    assert!(!stderr(&output).contains("bad\"token"));
}

#[test]
fn uninstall_keeps_user_data_unless_purged() {
    let env = Env::new();
    let (config, log) = write_user_data(&env);
    // The machine's name on the server, from its identity.
    let identity = config.parent().unwrap().join("identity");
    std::fs::write(
        &config,
        format!(
            "{{\"enabled\": true, \"cert\": \"{}\"}}\n",
            identity.join("cert.pem").display()
        ),
    )
    .unwrap();
    std::fs::write(
        identity.join("identity.json"),
        "{\"machine\": \"desk\", \"owner\": \"k\"}\n",
    )
    .unwrap();
    for (path, mode) in [
        (config.parent().unwrap().to_path_buf(), 0o700),
        (identity.clone(), 0o700),
        (config.clone(), 0o600),
        (identity.join("identity.json"), 0o600),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let old = write_legacy_service(&env);
    env.set_loaded(current(), true);
    let home = env.home();
    let output = env.run(&["cli", "uninstall"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.starts_with("✓ Stopped the bridge\n✓ Removed Switchboard from this machine:\n"),
        "{text}"
    );
    assert!(text.contains("    ~/.local/bin/switchboard\n"), "{text}");
    assert!(text.contains(
        "  Kept this machine's configuration, key, chat membership, and logs:\n    ~/.config/switchboard\n    ~/.local/share/switchboard\n"
    ), "{text}");
    assert!(
        text.contains(
            "\n\n  To remove those too, run\n      rm -rf ~/.config/switchboard ~/.local/share/switchboard\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "  The server still lists this machine as desk. To remove it there, run this on\n  the server:\n      sudo switchboard server revoke desk\n"
        ),
        "{text}"
    );
    assert!(!env.loaded(current()));
    for gone in [
        env.service_file(false),
        old,
        home.join("bin/switchboard"),
        home.join(".local/bin/switchboard"),
        home.join(".local/share/switchboard/switchboard"),
        home.join(".local/share/switchboard/backups"),
    ] {
        assert!(
            std::fs::symlink_metadata(&gone).is_err(),
            "{}",
            gone.display()
        );
    }
    if LINUX {
        let wants = home.join(".config/systemd/user/default.target.wants");
        assert!(std::fs::symlink_metadata(wants.join(UNIT)).is_err());
        assert!(env.calls().contains("systemctl --user daemon-reload"));
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "earlier log line\n");
    assert!(config.exists());
    assert!(home
        .join(".local/share/switchboard/.switchboard-install")
        .exists());
    // Nothing left to remove.
    let output = env.run(&["cli", "uninstall", "--json"]);
    let result = json_out(&output);
    assert_eq!(result["removed"], json!([]));
    assert_eq!(result["stopped"], json!(false));

    // --purge removes the data too; an unmanaged command is left alone.
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(home.join("bin/switchboard"), "#!/bin/sh\necho mine\n").unwrap();
    let output = env.run(&["cli", "uninstall", "--purge", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    assert_eq!(result["kept"], json!([]));
    assert!(!home.join(".config/switchboard").exists());
    assert!(!home.join(".local/share/switchboard").exists());
    assert_eq!(
        std::fs::read_to_string(home.join("bin/switchboard")).unwrap(),
        "#!/bin/sh\necho mine\n"
    );
}

#[test]
fn a_bridge_that_will_not_stop_blocks_uninstall() {
    let env = Env::new();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    env.set_loaded(current(), true);
    std::fs::write(env.root.join("fail-stop"), "").unwrap();
    let output = env.run(&["cli", "uninstall"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "✗ Could not stop the bridge (Operation not permitted); nothing was removed.\n"
    );
    assert!(env.service_file(false).exists());
    assert!(env.home().join(".local/bin/switchboard").exists());
}

#[test]
fn uninstalling_an_unused_installation_leaves_nothing_behind() {
    let env = Env::new();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let output = env.run(&["cli", "uninstall", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(json_out(&output)["kept"], json!([]));
    assert!(!env.home().join(".local/share/switchboard").exists());
    assert!(!env.home().join(".config/switchboard").exists());
    assert!(!env.home().join(".local/bin/switchboard").exists());
}

/// A configuration `status` and `doctor` can read: private, with a state
/// directory and an empty Claude Code directory.
fn write_full_config(env: &Env) -> PathBuf {
    let config_dir = env.home().join(".config/switchboard");
    let state = config_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let claude = env.home().join(".claude/sessions");
    std::fs::create_dir_all(&claude).unwrap();
    let config = config_dir.join("bridge.json");
    std::fs::write(
        &config,
        json!({"enabled": true, "server_url": "https://sb.example.com", "ca": null,
               "cert": config_dir.join("identity/cert.pem"), "key": config_dir.join("identity/key.pem"),
               "config_dir": env.home().join(".claude"), "state_dir": state, "sessions": []})
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    config
}

/// `down` keeps the machine off Switchboard across logins and upgrades
/// until `up`; status and doctor say so without calling it a failure.
#[test]
fn down_stays_down_through_installs_and_upgrades_until_up() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let config = write_full_config(&env);
    let marker = config.parent().unwrap().join("down");
    let wants = env
        .home()
        .join(".config/systemd/user/default.target.wants")
        .join(UNIT);
    if LINUX {
        assert!(wants.exists());
    }
    env.set_loaded(current(), true);
    env.clear_calls();
    let output = env.run(&["cli", "down", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        json_out(&output),
        json!({"ok": true, "action": "down", "down": true, "stopped": true})
    );
    assert!(marker.exists());
    assert!(!env.loaded(current()));
    let calls = env.calls();
    if LINUX {
        assert!(
            calls.contains(&format!("systemctl --user disable --now {UNIT}")),
            "{calls}"
        );
        assert!(std::fs::symlink_metadata(&wants).is_err());
    } else {
        assert!(calls.contains("launchctl disable gui/"), "{calls}");
        assert!(env
            .root
            .join("state")
            .join(format!("disabled-{LABEL}"))
            .exists());
    }

    // Status and doctor report the state, not a failure.
    let status = env.run(&["cli", "status"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    assert!(
        stdout(&status).starts_with("  Down. Run switchboard up to reconnect.\n"),
        "{}",
        stdout(&status)
    );
    assert_eq!(
        json_out(&env.run(&["cli", "status", "--json"]))["down"],
        json!(true)
    );
    let doctor = json_out(&env.run(&["cli", "doctor", "--json"]));
    let bridge = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "bridge")
        .unwrap()
        .clone();
    assert_eq!(
        bridge,
        json!({"name": "bridge", "status": "pass", "summary": "Down. Run switchboard up to reconnect.", "down": true})
    );

    // Installing again (as install.sh does) and upgrading leave it down.
    let output = env.run(&["install", "--start", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let installed = json_out(&output);
    assert_eq!(
        (installed["started"].clone(), installed["down"].clone()),
        (json!(false), json!(true))
    );
    assert!(!env.loaded(current()));
    if LINUX {
        assert!(std::fs::symlink_metadata(&wants).is_err());
    }
    server.publish("99.0.0", binary());
    *server.latest.lock().unwrap() = "99.0.0".into();
    let output = env.run(&["cli", "upgrade", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let upgraded = json_out(&output);
    assert_eq!(
        (upgraded["restarted"].clone(), upgraded["down"].clone()),
        (json!(false), json!(true))
    );
    assert!(!env.loaded(current()) && marker.exists());

    // Up enables and starts it again.
    env.clear_calls();
    let output = env.run(&["cli", "up", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let up = json_out(&output);
    assert_eq!(
        (up["action"].clone(), up["connected"].clone()),
        (json!("up"), json!(false))
    );
    assert!(env.loaded(current()));
    assert!(!marker.exists());
    if LINUX {
        assert!(std::fs::symlink_metadata(&wants).is_ok());
    } else {
        assert!(!env
            .root
            .join("state")
            .join(format!("disabled-{LABEL}"))
            .exists());
    }
}

/// `status` says when a newer release exists, asking GitHub at most once a
/// day and never in CI or when turned off; `--json` only adds a field.
#[test]
fn status_names_a_newer_release_once_a_day() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    let config = write_full_config(&env);
    let cache = config.parent().unwrap().join("state/update-check.json");
    *server.latest.lock().unwrap() = "99.0.0".into();
    // Turned on for this test (Env turns it off for every other one).
    env.vars
        .push(("SWITCHBOARD_NO_UPDATE_CHECK".into(), String::new()));
    let output = env.run(&["cli", "status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("\n\n  Switchboard 99.0.0 is available. Run switchboard upgrade.\n"),
        "{text}"
    );
    // How many times GitHub was asked so far (`seen` hands each request out
    // once, after it is answered).
    let total = std::cell::Cell::new(0);
    let asked = |server: &ReleaseServer| {
        let began = std::time::Instant::now();
        loop {
            total.set(
                total.get()
                    + server
                        .seen()
                        .iter()
                        .filter(|s| s.path.ends_with("/releases/latest"))
                        .count(),
            );
            if total.get() > 0 || began.elapsed() > std::time::Duration::from_secs(2) {
                return total.get();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    assert_eq!(asked(&server), 1);
    assert!(cache.exists());
    // The answer is kept for a day: no second question.
    let value = json_out(&env.run(&["cli", "status", "--json"]));
    assert_eq!(value["update_available"], json!("99.0.0"));
    assert_eq!(asked(&server), 1);
    // Doctor never shows it.
    assert!(!stdout(&env.run(&["cli", "doctor"])).contains("is available"));
    // Turned off, or in CI: no line, no field, no question.
    std::fs::remove_file(&cache).unwrap();
    for (name, value) in [("SWITCHBOARD_NO_UPDATE_CHECK", "1"), ("CI", "true")] {
        env.vars = vec![
            ("SWITCHBOARD_NO_UPDATE_CHECK".into(), String::new()),
            (name.into(), value.into()),
        ];
        assert!(!stdout(&env.run(&["cli", "status"])).contains("is available"));
        assert!(json_out(&env.run(&["cli", "status", "--json"]))
            .get("update_available")
            .is_none());
    }
    assert_eq!(asked(&server), 1);
    // A failed question (a private repository, no network) is quiet, and
    // remembered too.
    env.vars = vec![
        ("SWITCHBOARD_NO_UPDATE_CHECK".into(), String::new()),
        ("SWITCHBOARD_REPO".into(), "owner/missing".into()),
    ];
    let output = env.run(&["cli", "status"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!stdout(&output).contains("is available") && stderr(&output).is_empty());
    assert!(std::fs::read_to_string(&cache)
        .unwrap()
        .contains("\"latest\":null"));
}

/// The `down` file and the service's enabled state never disagree: `down`
/// writes the file only once the service is disabled, `up` removes it only
/// once the service is enabled again, and each failure says what to do.
#[test]
fn down_and_up_keep_the_marker_and_the_service_in_step() {
    let env = Env::new();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let config = write_full_config(&env);
    let marker = config.parent().unwrap().join("down");
    let wants = env
        .home()
        .join(".config/systemd/user/default.target.wants")
        .join(UNIT);
    let disabled = env.root.join("state").join(format!("disabled-{LABEL}"));
    env.set_loaded(current(), true);

    // Disabling refused: not marked down, and still running.
    std::fs::write(env.root.join("fail-disable"), "").unwrap();
    let output = env.run(&["cli", "down", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let error = json_out(&output)["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("couldn't be kept from starting at login")
            && error.contains("so this machine wasn't taken down"),
        "{error}"
    );
    assert!(
        !marker.exists(),
        "marked down though the service wasn't disabled"
    );
    assert!(env.loaded(current()));
    std::fs::remove_file(env.root.join("fail-disable")).unwrap();

    // Down, then enabling refused: still marked down, with the fix.
    let output = env.run(&["cli", "down", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(marker.exists());
    std::fs::write(env.root.join("fail-enable"), "").unwrap();
    if LINUX {
        // systemctl refuses, and the link can't be made by hand either.
        std::fs::create_dir_all(wants.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            wants.parent().unwrap(),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
    }
    let output = env.run(&["cli", "up"]);
    if LINUX {
        std::fs::set_permissions(
            wants.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    assert_eq!(output.status.code(), Some(1));
    // As one line, however the terminal width wrapped it.
    let said = stderr(&output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        said.starts_with("✗ The bridge's service couldn't be allowed to start again"),
        "{said}"
    );
    assert!(said.contains("so this machine is still down"), "{said}");
    let fix = if LINUX {
        format!("1 Run systemctl --user enable {UNIT}. 2 Run switchboard up")
    } else {
        format!(
            "1 Run launchctl enable gui/{}/{LABEL}. 2 Run switchboard up",
            switchboard::fsutil::getuid()
        )
    };
    assert!(said.contains(&fix), "{said}");
    assert!(
        marker.exists(),
        "the down marker went before the service was enabled"
    );
    assert!(!env.loaded(current()));
    std::fs::remove_file(env.root.join("fail-enable")).unwrap();
    let output = env.run(&["cli", "up", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!marker.exists() && env.loaded(current()));
    if LINUX {
        assert!(std::fs::symlink_metadata(&wants).is_ok());
    } else {
        assert!(!disabled.exists());
    }
}

/// A job launchd keeps disabled without the `down` file (disabled by hand,
/// or by an earlier version) is explained, with `switchboard up` as the fix,
/// instead of launchd's bare refusal. (A disabled systemd unit still starts.)
#[cfg(target_os = "macos")]
#[test]
fn starting_a_job_disabled_without_the_marker_says_to_run_up() {
    let env = Env::new();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    write_full_config(&env);
    std::fs::write(env.root.join("state").join(format!("disabled-{LABEL}")), "").unwrap();
    let output = env.run(&["install", "--start"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "switchboard install: installed, but the bridge did not start: the bridge's service is \
         turned off in launchd (launchctl disable), though this machine wasn't taken down with \
         switchboard down; run switchboard up to turn it back on and start the bridge\n"
    );
    // `up` turns it back on and starts it.
    let output = env.run(&["cli", "up", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(env.loaded(current()));
}

/// `uninstall --purge` on a machine that was down lets the service start
/// again, so a later setup doesn't find it disabled.
#[cfg(target_os = "macos")]
#[test]
fn uninstall_purge_forgets_that_the_machine_was_down() {
    let env = Env::new();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    write_full_config(&env);
    env.set_loaded(current(), true);
    assert_eq!(env.run(&["cli", "down"]).status.code(), Some(0));
    let disabled = env.root.join("state").join(format!("disabled-{LABEL}"));
    assert!(disabled.exists());
    let output = env.run(&["cli", "uninstall", "--purge", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!disabled.exists());
}

/// A failed or unanswered update question is kept for an hour, so a network
/// that hangs costs one two-second wait, not one per `status`.
#[test]
fn a_hanging_update_check_is_remembered() {
    // A server that accepts connections and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    {
        let accepted = accepted.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                accepted.lock().unwrap().push(stream);
            }
        });
    }
    let mut env = Env::new();
    env.port = Some(port);
    let config = write_full_config(&env);
    let cache = config.parent().unwrap().join("state/update-check.json");
    env.vars
        .push(("SWITCHBOARD_NO_UPDATE_CHECK".into(), String::new()));
    let began = std::time::Instant::now();
    let output = env.run(&["cli", "status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(began.elapsed() >= std::time::Duration::from_millis(1500));
    assert!(!stdout(&output).contains("is available"));
    let kept = std::fs::read_to_string(&cache).expect("the unanswered question was not kept");
    assert!(
        kept.contains("\"failed\":true") && kept.contains("\"latest\":null"),
        "{kept}"
    );
    let asked = accepted.lock().unwrap().len();
    assert!(asked >= 1);
    // The next status doesn't wait, or ask.
    let began = std::time::Instant::now();
    let output = env.run(&["cli", "status"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        began.elapsed() < std::time::Duration::from_millis(1500),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(accepted.lock().unwrap().len(), asked);
    // An hour on, it asks again.
    let mut value: Value = serde_json::from_str(&kept).unwrap();
    value["checked_at"] = json!(value["checked_at"].as_f64().unwrap() - 3700.0);
    std::fs::write(&cache, value.to_string()).unwrap();
    env.run(&["cli", "status"]);
    assert!(accepted.lock().unwrap().len() > asked);
}

// Moving an installation from before 0.3.1 ------------------------------------

/// An installation as 0.3.0 left it, after Python releases before it: the
/// program in ~/.local/share/switchboard, the ~/bin/switchboard wrapper,
/// ~/.local/bin/switchboard linked to it, the service running the old copy,
/// the Python modules and their bytecode, a file of the user's, and more
/// backups than are kept. Returns the old program's path.
fn write_030_install(env: &Env) -> PathBuf {
    let home = env.home();
    let share = home.join(".local/share/switchboard");
    std::fs::create_dir_all(share.join("__pycache__")).unwrap();
    std::fs::write(share.join(".switchboard-install"), "1\n").unwrap();
    let old = share.join("switchboard");
    let mut program = std::fs::read(binary()).unwrap();
    program.extend_from_slice(b"\n0.3.0 copy\n");
    std::fs::write(&old, program).unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in [
        "bridge.py",
        "native_peer.py",
        "switchboard.py",
        "machines_client.py",
    ] {
        std::fs::write(share.join(name), "# python\n").unwrap();
    }
    std::fs::write(share.join("__pycache__/bridge.cpython-312.pyc"), "pyc").unwrap();
    std::fs::write(
        share.join("__pycache__/native_peer.cpython-313.opt-1.pyc"),
        "pyc",
    )
    .unwrap();
    std::fs::write(share.join("notes.txt"), "mine\n").unwrap();
    for index in 0..8 {
        let folder = share.join(format!(
            "backups/20261001T00000{index}-17592{index}0000000000000"
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("switchboard"), "old").unwrap();
    }
    std::fs::create_dir_all(share.join("backups/kept-by-hand")).unwrap();
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(
        home.join("bin/switchboard"),
        format!(
            "#!/bin/sh\n# Managed by switchboard install\nexec {} cli \"$@\"\n",
            old.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        home.join("bin/switchboard"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".local/bin")).unwrap();
    std::os::unix::fs::symlink(
        home.join("bin/switchboard"),
        home.join(".local/bin/switchboard"),
    )
    .unwrap();
    let service = env.service_file(false);
    std::fs::create_dir_all(service.parent().unwrap()).unwrap();
    let config = home.join(".config/switchboard/bridge.json");
    let text = if LINUX {
        switchboard::cli::install::systemd_unit(
            &old.to_string_lossy(),
            &share.to_string_lossy(),
            &config.to_string_lossy(),
        )
    } else {
        switchboard::cli::install::launch_agent(
            &old.to_string_lossy(),
            &share.to_string_lossy(),
            &config.to_string_lossy(),
        )
    };
    std::fs::write(&service, text).unwrap();
    if LINUX {
        let wants = service.parent().unwrap().join("default.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        std::os::unix::fs::symlink(&service, wants.join(UNIT)).unwrap();
    }
    old
}

fn gone(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_err()
}

/// A fake `claude` whose plugin list has the Switchboard plugin at
/// `version` (None: no plugin; "fail": the command fails).
fn fake_claude(env: &Env, version: Option<&str>) {
    let body = match version {
        Some("fail") => "echo 'not logged in' >&2; exit 1".to_string(),
        Some(version) => format!(
            "echo '[{{\"id\": \"switchboard@switchboard\", \"version\": \"{version}\", \"scope\": \"user\"}}]'"
        ),
        None => "echo '[]'".to_string(),
    };
    env.write_exec(
        "bin/claude",
        &format!("#!/bin/sh\necho \"claude $*\" >> \"$SB_TEST/calls\"\n{body}\n"),
    );
}

#[test]
fn install_moves_a_030_installation_and_removes_only_what_installers_wrote() {
    let env = Env::new();
    let old = write_030_install(&env);
    let home = env.home();
    let share = home.join(".local/share/switchboard");
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    let program = home.join(".local/bin/switchboard");
    // The old link at the program's path became the program.
    let info = std::fs::symlink_metadata(&program).unwrap();
    assert!(info.file_type().is_file());
    assert_eq!(
        std::fs::read(&program).unwrap(),
        std::fs::read(binary()).unwrap()
    );
    // The service runs it from there now.
    let service = std::fs::read_to_string(env.service_file(false)).unwrap();
    assert!(
        service.contains(&program.to_string_lossy().to_string()),
        "{service}"
    );
    assert!(
        !service.contains(&old.to_string_lossy().to_string()),
        "{service}"
    );
    // The wrapper, the old copy, the Python modules, and their bytecode are
    // gone, by name; the user's files stay.
    let mut removed: Vec<String> = result["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    removed.sort();
    let mut expected: Vec<String> = [
        home.join("bin/switchboard"),
        old.clone(),
        share.join("bridge.py"),
        share.join("native_peer.py"),
        share.join("switchboard.py"),
        share.join("machines_client.py"),
        share.join("__pycache__/bridge.cpython-312.pyc"),
        share.join("__pycache__/native_peer.cpython-313.opt-1.pyc"),
        share.join("__pycache__"),
    ]
    .iter()
    .map(|p| p.to_string_lossy().into_owned())
    .collect();
    expected.sort();
    assert_eq!(removed, expected);
    for path in &expected {
        assert!(gone(std::path::Path::new(path)), "{path}");
    }
    assert_eq!(
        std::fs::read_to_string(share.join("notes.txt")).unwrap(),
        "mine\n"
    );
    // Backups: the newest five of the installer's (this run's among them),
    // and a folder it didn't name.
    assert_eq!(result["pruned_backups"], json!(4));
    let mut backups: Vec<String> = std::fs::read_dir(share.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    backups.sort();
    assert_eq!(backups.len(), 6, "{backups:?}");
    assert!(backups.contains(&"kept-by-hand".to_string()));
    assert!(!backups
        .iter()
        .any(|b| b.starts_with("20261001T000000-") || b.starts_with("20261001T000003-")));
    assert!(backups.iter().any(|b| b.starts_with("20261001T000007-")));
    // The service manager was only asked whether the bridge runs.
    let calls = env.calls();
    assert!(
        !calls.contains(" start") && !calls.contains("bootstrap"),
        "{calls}"
    );

    // Again: nothing more to move, and nothing of the user's goes.
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let again = json_out(&output);
    assert!(again.get("removed").is_none(), "{again}");
    assert_eq!(again["pruned_backups"], json!(1));
    assert_eq!(
        std::fs::read_to_string(share.join("notes.txt")).unwrap(),
        "mine\n"
    );
    assert!(share.join("backups/kept-by-hand").exists());

    // The human summary says what it removed.
    std::fs::write(share.join("doctor.py"), "# python\n").unwrap();
    let text = stdout(&env.run(&["install"]));
    assert!(
        text.starts_with(&format!(
            "Installed switchboard {} as ~/.local/bin/switchboard.\n",
            current_version()
        )),
        "{text}"
    );
    assert!(
        text.contains(
            "Removed what earlier versions left:\n    ~/.local/share/switchboard/doctor.py\n"
        ),
        "{text}"
    );
}

#[test]
fn install_leaves_a_foreign_bin_command_alone() {
    let env = Env::new();
    write_030_install(&env);
    let home = env.home();
    let mine = "#!/bin/sh\necho mine\n";
    std::fs::write(home.join("bin/switchboard"), mine).unwrap();
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    assert_eq!(result["left"], json!(home.join("bin/switchboard")));
    assert_eq!(
        std::fs::read_to_string(home.join("bin/switchboard")).unwrap(),
        mine
    );
    // A link of the user's there is left too, and so is one elsewhere that
    // points at the program.
    std::fs::remove_file(home.join("bin/switchboard")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", home.join("bin/switchboard")).unwrap();
    let result = json_out(&env.run(&["install", "--json"]));
    assert_eq!(result["left"], json!(home.join("bin/switchboard")));
    assert_eq!(
        std::fs::read_link(home.join("bin/switchboard")).unwrap(),
        PathBuf::from("/usr/bin/true")
    );
    let text = stdout(&env.run(&["install"]));
    assert!(text.contains("Left ~/bin/switchboard alone"), "{text}");
    // A script that only mentions the marker (not as a line of its own) is
    // the user's too.
    std::fs::remove_file(home.join("bin/switchboard")).unwrap();
    let mentions = "#!/bin/sh\n# copied from: # Managed by switchboard install\necho mine\n";
    std::fs::write(home.join("bin/switchboard"), mentions).unwrap();
    let result = json_out(&env.run(&["install", "--json"]));
    assert_eq!(result["left"], json!(home.join("bin/switchboard")));
    assert_eq!(
        std::fs::read_to_string(home.join("bin/switchboard")).unwrap(),
        mentions
    );
    // At the program's path, such a script is refused, not replaced, and
    // uninstall leaves it.
    let program = home.join(".local/bin/switchboard");
    std::fs::remove_file(&program).unwrap();
    std::fs::write(&program, mentions).unwrap();
    let refused = env.run(&["install"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("unmanaged command: "),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("move it aside"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(std::fs::read_to_string(&program).unwrap(), mentions);
    let output = env.run(&["cli", "uninstall", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(&program).unwrap(), mentions);
    std::fs::remove_file(&program).unwrap();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    // Every Switchboard program carries the text that marks it.
    assert!(switchboard::cli::install::is_our_program(binary()));
    assert!(switchboard::cli::install::is_our_program(
        program.to_str().unwrap()
    ));
    // Uninstalling leaves it as well.
    let output = env.run(&["cli", "uninstall", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(std::fs::symlink_metadata(home.join("bin/switchboard")).is_ok());
    assert!(gone(&home.join(".local/bin/switchboard")));
}

#[test]
fn an_older_plugin_keeps_bin_switchboard_as_a_link_until_it_is_updated() {
    let env = Env::new();
    write_030_install(&env);
    let home = env.home();
    let old_command = home.join("bin/switchboard");
    let program = home.join(".local/bin/switchboard");
    fake_claude(&env, Some("0.3.0"));
    let result = json_out(&env.run(&["install", "--json"]));
    assert_eq!(result["plugin_link"], json!(old_command));
    assert!(result.get("plugin_unknown").is_none(), "{result}");
    assert_eq!(std::fs::read_link(&old_command).unwrap(), program);
    // The plugin's path runs the program.
    let output = env.run_program(old_command.to_str().unwrap(), &["--version"]);
    assert_eq!(
        stdout(&output).trim(),
        format!("switchboard {}", current_version())
    );
    // Claude Code that can't answer may still have the old plugin: kept.
    fake_claude(&env, Some("fail"));
    let result = json_out(&env.run(&["install", "--json"]));
    assert_eq!(result["plugin_link"], json!(old_command));
    assert_eq!(result["plugin_unknown"], json!(true));
    assert!(std::fs::read_link(&old_command).is_ok());
    let text = stdout(&env.run(&["install"]));
    assert!(
        text.contains("Claude Code couldn't say which Switchboard plugin it has"),
        "{text}"
    );
    // Once the plugin finds the command itself, the link goes.
    fake_claude(&env, Some("0.3.1"));
    let result = json_out(&env.run(&["install", "--json"]));
    assert!(result.get("plugin_link").is_none(), "{result}");
    assert_eq!(result["removed"], json!([old_command]));
    assert!(gone(&old_command));
    assert!(program.is_file());
    // Without Claude Code on PATH there is no plugin to serve.
    let env = Env::new();
    write_030_install(&env);
    let result = json_out(&env.run(&["install", "--json"]));
    assert!(result.get("plugin_link").is_none(), "{result}");
    assert!(gone(&env.home().join("bin/switchboard")));
    // Uninstalling removes the link with the program.
    let env = Env::new();
    write_030_install(&env);
    fake_claude(&env, Some("0.2.1"));
    json_out(&env.run(&["install", "--json"]));
    let output = env.run(&["cli", "uninstall", "--json"]);
    let removed = json_out(&output)["removed"].clone();
    assert!(removed
        .as_array()
        .unwrap()
        .contains(&json!(env.home().join("bin/switchboard"))));
    assert!(gone(&env.home().join("bin/switchboard")));
    assert!(gone(&env.home().join(".local/bin/switchboard")));
}

#[test]
fn a_running_bridge_keeps_the_old_copy_until_it_starts_from_the_new_place() {
    let env = Env::new();
    let old = write_030_install(&env);
    let config = write_full_config(&env);
    env.set_loaded(current(), true);
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    assert_eq!(result["old_program_kept"], json!(old));
    assert!(old.exists());
    let text = stdout(&env.run(&["install"]));
    assert!(
        text.contains("The running bridge still uses ~/.local/share/switchboard/switchboard"),
        "{text}"
    );
    // A bridge started from anywhere else leaves it.
    let invalid = env.root.join("invalid.json");
    std::fs::write(&invalid, "{}").unwrap();
    std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o600)).unwrap();
    env.run(&["bridge", "--config", invalid.to_str().unwrap()]);
    assert!(old.exists());
    // The service starting it from the program's place removes it, even
    // when the bridge then stops for another reason.
    let program = env.home().join(".local/bin/switchboard");
    env.run_program(
        program.to_str().unwrap(),
        &["bridge", "--config", invalid.to_str().unwrap()],
    );
    assert!(gone(&old));
    // A later install has nothing left to keep.
    let result = json_out(&env.run(&["install", "--json"]));
    assert!(result.get("old_program_kept").is_none(), "{result}");
    assert!(config.exists());
}

#[test]
fn a_machine_that_is_down_is_moved_and_stays_down() {
    let env = Env::new();
    let old = write_030_install(&env);
    let config = write_full_config(&env);
    let marker = config.parent().unwrap().join("down");
    std::fs::write(&marker, "down\n").unwrap();
    let wants = env
        .home()
        .join(".config/systemd/user/default.target.wants")
        .join(UNIT);
    if LINUX {
        // `down` removed the enabling link.
        std::fs::remove_file(&wants).unwrap();
    }
    env.clear_calls();
    let output = env.run(&["install", "--start", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result = json_out(&output);
    assert_eq!(
        (result["down"].clone(), result["started"].clone()),
        (json!(true), json!(false))
    );
    assert!(gone(&old));
    assert!(gone(&env.home().join("bin/switchboard")));
    let service = std::fs::read_to_string(env.service_file(false)).unwrap();
    assert!(service.contains(
        &env.home()
            .join(".local/bin/switchboard")
            .to_string_lossy()
            .to_string()
    ));
    assert!(marker.exists());
    assert!(!env.loaded(current()));
    if LINUX {
        assert!(gone(&wants));
    }
    let calls = env.calls();
    assert!(
        !calls.contains("bootstrap")
            && !calls.contains(&format!("start {UNIT}"))
            && !calls.contains(" enable"),
        "{calls}"
    );
}

/// The upgrade writes the new program beside the old and renames it into
/// place: a bridge running the old one keeps its file until it restarts.
#[test]
fn upgrade_replaces_the_running_program_in_one_step() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let program = env.home().join(".local/bin/switchboard");
    // What the running bridge has open: a program that isn't this build.
    let mut running = std::fs::read(binary()).unwrap();
    running.extend_from_slice(b"\nthe running version\n");
    std::fs::write(&program, &running).unwrap();
    let held = std::fs::File::open(&program).unwrap();
    let inode = std::fs::metadata(&program).unwrap().ino();
    let config = env.home().join(".config/switchboard");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("bridge.json"), "{}").unwrap();
    env.set_loaded(current(), true);
    server.publish("99.0.0", binary());
    *server.latest.lock().unwrap() = "99.0.0".into();
    let output = env.run(&["cli", "upgrade", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(json_out(&output)["restarted"], json!(true));
    // A new file at the path; the old one is intact for whoever holds it.
    assert_ne!(std::fs::metadata(&program).unwrap().ino(), inode);
    assert_eq!(
        std::fs::read(&program).unwrap(),
        std::fs::read(binary()).unwrap()
    );
    let mut kept = Vec::new();
    std::io::Read::read_to_end(&mut &held, &mut kept).unwrap();
    assert_eq!(kept, running);
    // No temporary file is left beside it.
    let beside: Vec<String> = std::fs::read_dir(program.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(beside, ["switchboard"]);
}

/// Moving back to a release before 0.3.1: its installer finds the way clear
/// (it refuses a program or link at the paths it writes).
#[test]
fn a_downgrade_before_031_clears_the_new_layout_for_the_older_installer() {
    let server = ReleaseServer::start();
    let mut env = Env::new();
    env.port = Some(server.port);
    write_030_install(&env);
    fake_claude(&env, Some("0.3.0"));
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    let home = env.home();
    assert!(std::fs::read_link(home.join("bin/switchboard")).is_ok());
    // An older stand-in refuses anything in its way, as 0.3.0 does.
    server.publish_script(
        "0.2.9",
        "if [ \"$1\" = install ]; then\n\
         if [ -e \"$HOME/.local/bin/switchboard\" ] || [ -L \"$HOME/bin/switchboard\" ]; then echo '{\"ok\": false, \"error\": \"unmanaged command\"}'; exit 1; fi\n\
         printf '#!/bin/sh\\n# Managed by switchboard install\\n' > \"$HOME/bin/switchboard\"; ln -s \"$HOME/bin/switchboard\" \"$HOME/.local/bin/switchboard\"; echo '{}'; exit 0; fi",
    );
    let output = env.run(&["cli", "upgrade", "0.2.9", "--force", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(std::fs::read_link(home.join(".local/bin/switchboard")).is_ok());
    // A failing older installer puts this version back.
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    server.publish_script(
        "0.2.8",
        "if [ \"$1\" = install ]; then echo '{\"ok\": false, \"error\": \"disk full\"}'; exit 1; fi",
    );
    let output = env.run(&["cli", "upgrade", "0.2.8", "--force"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("this version is still installed"),
        "{}",
        stderr(&output)
    );
    assert!(home.join(".local/bin/switchboard").is_file());
    assert!(env.service_file(false).exists());
}

/// `status` on a connected machine with no chats joined says how to join
/// one: `/switchboard join` where Claude Code has the plugin, the `!`
/// command where it doesn't; the plugin's own display always has the
/// plugin, and plain JSON asks nothing.
#[test]
fn status_suggests_the_join_command_that_works_here() {
    let mut env = Env::new();
    let config = write_full_config(&env);
    let state = config.parent().unwrap().join("state");
    let pid = std::process::id();
    let runtime = state.join("runtime.json");
    std::fs::write(
        &runtime,
        json!({"pid": pid, "procStart": switchboard::identity::process_start(pid as i64).unwrap(),
               "connected": true, "machine": "desk"})
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o600)).unwrap();
    let hint = |env: &Env| {
        let text = stdout(&env.run(&["cli", "status"]));
        assert!(
            text.starts_with("✓ Connected to sb.example.com as desk"),
            "{text}"
        );
        text.rsplit("To connect a chat, run this in it:\n")
            .next()
            .unwrap()
            .trim()
            .to_string()
    };
    // No Claude Code on PATH: no plugin, and switchboard isn't on PATH.
    assert_eq!(hint(&env), "!~/.local/bin/switchboard join");
    let local_bin = env.home().join(".local/bin");
    env.vars.push((
        "PATH".into(),
        format!(
            "{}:{}:/usr/bin:/bin",
            env.root.join("bin").display(),
            local_bin.display()
        ),
    ));
    assert_eq!(hint(&env), "!switchboard join");
    fake_claude(&env, None);
    assert_eq!(hint(&env), "!switchboard join");
    fake_claude(&env, Some("fail"));
    assert_eq!(hint(&env), "!switchboard join");
    fake_claude(&env, Some("0.3.1"));
    assert_eq!(hint(&env), "/switchboard join");
    // The plugin's display and plain JSON don't ask Claude Code.
    fake_claude(&env, None);
    env.clear_calls();
    let display = json_out(&env.run(&["cli", "status", "--json", "--display"]));
    assert!(
        display["display"]
            .to_string()
            .contains("\"/switchboard join\""),
        "{display}"
    );
    json_out(&env.run(&["cli", "status", "--json"]));
    assert!(!env.calls().contains("claude"), "{}", env.calls());
}
