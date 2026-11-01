//! Command-line tests: the chat list, the installer and service control,
//! doctor, setup, and invites. Library tests call the formatting functions;
//! process tests run the built binary with a private HOME and fake service
//! managers first on PATH, so nothing touches the real installation or
//! launchd.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

use serde_json::{json, Map, Value};
use switchboard::cli::format::display_width;
use switchboard::cli::service::LEGACY_LABEL;
use switchboard::cli::view;

static ENV: Mutex<()> = Mutex::new(());

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let template = std::ffi::CString::new("/tmp/sbcli-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        let made = unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) };
        assert!(!made.is_null());
        raw.pop();
        let path = std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn obj(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn strip_ansi(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// Chat list formatting ---------------------------------------------------

fn chats_view(details: bool) -> view::ChatsView<'static> {
    view::ChatsView {
        scope: view::Scope::All,
        current: None,
        search: "",
        details,
        filtered: false,
        hint: Some("switchboard chats --details".into()),
        machine: Some("laptop".into()),
    }
}

fn shown(doc: &view::Doc, width: usize, color: bool) -> String {
    let theme = switchboard::cli::ui::Theme {
        color,
        unicode: true,
    };
    view::render(doc, &theme, width).join("\n")
}

#[test]
fn chat_tables_keep_names_ids_alignment_and_colors() {
    let now = 1_800_000_000.0;
    // Joined chats first, then by name; activity and membership in words.
    let result = obj(json!({"enabled": true, "sessions": [
        {"id": "11111111-1111-4111-8111-111111111111", "name": "Geometry generation", "status": "busy", "enrolled": false},
        {"id": "22222222-2222-4222-8222-222222222222", "name": "模型 architecture", "status": "idle", "enrolled": true,
         "joined_name": "architecture", "expires_at": null}]}));
    let doc = view::chats(&result, &chats_view(false), now);
    let output = shown(&doc, 80, false);
    assert!(
        output.find("模型 architecture").unwrap() < output.find("Geometry generation").unwrap(),
        "{output}"
    );
    assert!(
        output.contains("  模型 architecture    Idle      Joined as architecture\n"),
        "{output}"
    );
    assert!(
        output.contains("  Geometry generation  Busy      Not joined\n"),
        "{output}"
    );
    // IDs are for --details, whole.
    assert!(!output.contains("11111111-1111"), "{output}");
    let details = shown(&view::chats(&result, &chats_view(true), now), 36, false);
    assert!(
        details
            .lines()
            .any(|l| l.trim() == "11111111-1111-4111-8111-111111111111"),
        "{details}"
    );
    // Control characters never reach the terminal.
    let unsafe_rows = obj(json!({"enabled": true, "sessions": [
        {"id": "11111111-1111-4111-8111-111111111111", "name": "unsafe\u{1b}[2J\nname\r\t", "status": "busy\nstatus", "enrolled": false}]}));
    let output = shown(
        &view::chats(&unsafe_rows, &chats_view(true), now),
        80,
        false,
    );
    assert!(!output.contains('\x1b') && !output.contains('\r') && !output.contains('\t'));
    assert!(output.contains("unsafe [2J name"), "{output}");
    // Every width: nothing past the edge, names bold, nothing else colored.
    let names = [
        "模型 café",
        "Geometry generation",
        "A session with a longer descriptive name",
    ];
    let rows: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({"id": format!("{i:08}-1111-4111-8111-111111111111"), "name": n,
                   "status": if i == 0 {"busy"} else {"idle"}, "enrolled": i == 0})
        })
        .collect();
    let result = obj(json!({"enabled": true, "sessions": rows}));
    for width in [32, 45, 80, 100, 120] {
        for details in [false, true] {
            let doc = view::chats(&result, &chats_view(details), now);
            let (plain, colored) = (shown(&doc, width, false), shown(&doc, width, true));
            assert_eq!(plain, strip_ansi(&colored));
            for line in plain.lines() {
                assert!(display_width(line) < width.max(40), "{width}: {line}");
            }
            assert!(colored.contains("\x1b[1m模型 café"), "{colored}");
            assert!(!colored.contains("\x1b[2m"));
        }
    }
    // Metadata belongs to --details, without a project that repeats the
    // repository.
    let id = "11111111-1111-4111-8111-111111111111";
    let meta = obj(
        json!({"enabled": true, "sessions": [{"name": "A", "id": id, "status": "idle", "enrolled": false,
        "project": "example", "repository": "example", "branch": "main"}]}),
    );
    let output = shown(&view::chats(&meta, &chats_view(true), now), 120, false);
    assert!(
        output.contains("  Repository  example\n  Branch      main\n"),
        "{output}"
    );
    assert!(!output.contains("Project"), "{output}");
}

#[test]
fn remote_chats_group_by_machine_and_say_why_they_are_missing() {
    let result = obj(
        json!({"enabled": true, "sessions": [], "remote_available": true, "machine": "a",
        "remote_sessions": [
            {"id": "22222222-2222-4222-8222-222222222222", "machine": "desk", "owner": "d", "name": "beta", "status": "busy", "route": "x"},
            {"id": "11111111-1111-4111-8111-111111111111", "machine": "desk", "owner": "d", "name": "Alpha", "status": "idle", "route": "y"}]}),
    );
    let output = shown(&view::chats(&result, &chats_view(false), 0.0), 100, false);
    assert!(output.contains("\n  desk\n  Chat   Activity\n"), "{output}");
    assert!(output.find("Alpha").unwrap() < output.find("beta").unwrap());
    let failed = obj(
        json!({"enabled": true, "sessions": [], "remote_available": false, "remote_sessions": [],
        "remote_error": "Chats on other machines are unavailable: the server doesn't accept this machine's key."}),
    );
    let remote = view::ChatsView {
        scope: view::Scope::Remote,
        ..chats_view(false)
    };
    let output = shown(&view::chats(&failed, &remote, 0.0), 100, false);
    assert_eq!(
        output,
        "  Chats on other machines\n\n! Chats on other machines are unavailable: the server doesn't accept this\n  machine's key."
    );
}

#[test]
fn doctor_and_status_wrap_within_their_columns() {
    let result = obj(json!({"ok": true, "checks": [
        {"name": "configuration", "status": "pass", "summary": "Private configuration is readable."},
        {"name": "discovery", "status": "warn", "summary": "Using periodic polling instead of filesystem events.",
         "fix": "Run switchboard doctor after checking the service installation."}]}));
    let status = obj(
        json!({"enabled": true, "connected": false, "machine": "模型 café long machine name", "pid": 123,
        "sessions": [], "detail": "The local bridge has not connected to the server."}),
    );
    for width in [45, 80, 100, 120] {
        let doctor = view::doctor(&result);
        let status_doc = view::status(&status, None, None, false, 0.0);
        let (plain, colored, status_text) = (
            shown(&doctor, width, false),
            shown(&doctor, width, true),
            shown(&status_doc, width, false),
        );
        assert_eq!(plain, strip_ansi(&colored));
        for line in plain.lines().chain(status_text.lines()) {
            assert!(display_width(line) <= width, "{line}");
            // Every line starts with a mark or the two blanks of the text column.
            assert!(
                line.is_empty() || line.chars().nth(1) == Some(' '),
                "{line}"
            );
        }
        assert!(plain.ends_with("\n\n  1 warning."), "{plain}");
        // What needs attention comes first; then the passing checks.
        assert!(plain.starts_with("! Using periodic polling"), "{plain}");
        assert!(
            plain.contains("\n✓ Private configuration is readable."),
            "{plain}"
        );
        assert!(!status_text.contains("123"));
        if width >= 100 {
            let words = status_text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(words.starts_with(
                "✗ Not connected to the server: the local bridge has not connected to the server. Chats here"
            ), "{status_text}");
            assert!(
                status_text.ends_with("\n\n  To see why, run\n      switchboard doctor"),
                "{status_text}"
            );
            assert!(
                plain.contains("\n  To fix, run switchboard doctor after checking"),
                "{plain}"
            );
        }
    }
}

#[test]
fn status_names_the_server_and_counts_the_joined_chats() {
    let row = |id: &str, name: &str, joined: Option<&str>| {
        let mut row =
            json!({"id": id, "name": name, "status": "idle", "enrolled": joined.is_some()});
        if let Some(chosen) = joined {
            row["joined_name"] = json!(chosen);
        }
        row
    };
    let connected = obj(
        json!({"enabled": true, "connected": true, "machine": "desk", "pid": 77,
        "server_url": "https://sb.example.com", "sessions": [
            row("a", "Research", Some("research")), row("b", "Solver", None), row("c", "Runs", Some(""))]}),
    );
    assert_eq!(
        shown(&view::status(&connected, None, None, false, 0.0), 80, false),
        "✓ Connected to sb.example.com as desk\n  Chats here  2 of 3 joined"
    );
    assert_eq!(
        shown(&view::status(&connected, Some("c"), None, false, 0.0), 80, false),
        "✓ Connected to sb.example.com as desk\n  This chat   Joined as Runs\n  Chats here  2 of 3 joined"
    );
    assert_eq!(
        shown(&view::status(&connected, Some("b"), None, false, 0.0), 80, false),
        "✓ Connected to sb.example.com as desk\n  This chat   Not joined. Run /switchboard join.\n  Chats here  2 of 3 joined"
    );
    let none = obj(
        json!({"enabled": true, "connected": true, "machine": "desk",
        "server_url": "https://100.64.0.5:8743", "sessions": []}),
    );
    assert_eq!(
        shown(&view::status(&none, None, None, false, 0.0), 80, false),
        "✓ Connected to 100.64.0.5:8743 as desk\n  Chats here  0 of 0 joined\n\n  To connect a chat, run this in it:\n      /switchboard join"
    );
    let stopped = obj(
        json!({"enabled": true, "connected": false, "detail": "the bridge is not running",
        "sessions": []}),
    );
    assert_eq!(
        shown(&view::status(&stopped, None, None, true, 0.0), 80, false),
        "✗ Not connected: the bridge isn't running.\n  Chats here  0 of 0 joined\n\n  To install the bridge's service and start it, run\n      switchboard install --start"
    );
    assert_eq!(
        shown(&view::status(&stopped, None, None, false, 0.0), 80, false),
        "✗ Not connected: the bridge isn't running.\n  Chats here  0 of 0 joined\n\n  To reconnect, run\n      switchboard up"
    );
}

#[test]
fn join_and_leave_confirm_in_one_line() {
    let joined = obj(
        json!({"ok": true, "action": "join", "session": "s", "name": "Model-work",
                            "confirmed": true, "remote_sessions": 1, "remote_machines": 1}),
    );
    assert_eq!(
        shown(&view::selection(&joined), 80, false),
        "✓ Joined as Model-work.\n  Reachable by 1 chat on 1 other machine."
    );
    let left = obj(
        json!({"ok": true, "action": "leave", "session": "s", "name": "s", "confirmed": false,
                          "detail": "the bridge is not running (switchboard up)"}),
    );
    assert_eq!(
        shown(&view::selection(&left), 80, false),
        "! This chat left, but the bridge hasn't confirmed it yet.\n  The bridge is not running (switchboard up).\n\n  If it stays this way, run\n      switchboard doctor"
    );
    let unchanged =
        obj(json!({"ok": true, "action": "leave", "session": "s", "name": "s", "changed": false}));
    assert_eq!(
        shown(&view::selection(&unchanged), 80, false),
        "  This chat wasn't joined; nothing changed."
    );
}

/// A rename (`join second` on a chat joined as `first`) is confirmed only
/// once the bridge publishes the new name; until then the result says what
/// other machines still see, and never reports the old name as the new one.
#[test]
fn join_confirms_a_rename_only_when_the_new_name_is_published() {
    let _guard = ENV.lock().unwrap();
    let temp = Temp::new();
    let state = temp.0.join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = temp.0.join("bridge.json");
    let config = obj(json!({"enabled": true, "state_dir": state}));
    let sid = "11111111-1111-4111-8111-111111111111";
    // The test process stands in for the running bridge.
    let pid = std::process::id() as i64;
    let publish = |name: &str| {
        let path = state.join("runtime.json");
        let _ = std::fs::remove_file(&path);
        let runtime = json!({"pid": pid,
            "procStart": switchboard::identity::process_start(pid).unwrap(),
            "connected": true, "published": [{"id": sid, "name": name}],
            "remote": {"sessions": 2, "machines": 1}});
        std::fs::write(&path, format!("{runtime}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    let wait = |name: Option<&str>| {
        switchboard::cli::local::await_bridge(
            &config,
            config_path.to_str().unwrap(),
            sid,
            true,
            name,
            0.6,
        )
        .unwrap()
    };
    publish("first");
    let stale = wait(Some("second"));
    assert_eq!(
        Value::Object(stale.clone()),
        json!({"confirmed": false, "published_name": "first",
            "detail": "the bridge has not published the new name within 10 seconds"})
    );
    let mut shown = stale;
    shown.insert("ok".into(), json!(true));
    shown.insert("action".into(), json!("join"));
    shown.insert("name".into(), json!("second"));
    assert!(view::render(&view::selection(&shown), &switchboard::cli::ui::Theme::plain(), 80)
        .join("\n")
        .starts_with("! Joined as second, but other machines still see it as first.\n  The bridge has not published the new name within 10 seconds."));
    publish("second");
    assert_eq!(
        Value::Object(wait(Some("second"))),
        json!({"confirmed": true, "remote_sessions": 2, "remote_machines": 1, "name": "second"})
    );
    // A join without a name expects the chat's own name, cleaned as the bridge
    // cleans it.
    publish("Model work");
    assert_eq!(wait(Some("Model work"))["name"], "Model-work");
    assert_eq!(wait(Some("Model work"))["confirmed"], true);
}

// Process tests -----------------------------------------------------------

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_switchboard")
}

struct Env {
    temp: Temp,
}

impl Env {
    fn new() -> Self {
        let temp = Temp::new();
        for dir in ["home", "bin"] {
            std::fs::create_dir(temp.0.join(dir)).unwrap();
        }
        let env = Env { temp };
        env.fake("launchctl", "exit 113");
        env.fake("systemctl", "exit 3");
        env
    }

    fn home(&self) -> PathBuf {
        self.temp.0.join("home")
    }

    fn fake(&self, name: &str, body: &str) {
        let path = self.temp.0.join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn command(&self, program: &str, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.temp.0.join("bin").display()),
            )
            .env("COLUMNS", "100")
            .env("SWITCHBOARD_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .current_dir(&self.temp.0);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(binary(), args).output().unwrap()
    }

    /// Run with extra environment variables and `input` on standard input.
    fn run_with(&self, args: &[&str], vars: &[(&str, &str)], input: &str) -> Output {
        use std::io::Write;
        let mut command = self.command(binary(), args);
        command
            .envs(vars.iter().copied())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_entry_points_match_and_need_no_installation() {
    let env = Env::new();
    let plain = env.run(&["cli", "help"]);
    assert_eq!(plain.status.code(), Some(0));
    assert!(stdout(&plain).starts_with("Usage: switchboard COMMAND [options]\n"));
    assert!(stdout(&plain).contains("\nChats:\n  join [NAME]        Join this chat, so chats"));
    assert!(!stdout(&plain).contains("setup") && !stdout(&plain).contains("sessions"));
    for args in [
        &["cli", "-h"][..],
        &["cli", "--help"],
        &["help"],
        &["--help"],
    ] {
        let output = env.run(args);
        assert_eq!(
            (output.status.code(), stdout(&output)),
            (Some(0), stdout(&plain)),
            "{args:?}"
        );
    }
    // Alone, on a machine that isn't set up: the next step, then the help.
    for args in [&["cli"][..], &[]] {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            stdout(&output),
            format!(
                "  Switchboard isn't set up on this machine yet. To set it up, run\n      switchboard init\n\n{}",
                stdout(&plain)
            ),
            "{args:?}"
        );
    }
    // Each command has its own help.
    let uninstall = stdout(&env.run(&["cli", "uninstall", "--help"]));
    assert!(uninstall.starts_with("Usage: switchboard uninstall [--purge]\n"));
    assert!(uninstall.contains("sudo switchboard server revoke NAME"));
    let chats = stdout(&env.run(&["cli", "--config", "/x", "chats", "-h"]));
    assert!(chats.starts_with("Usage: switchboard chats"));
    assert_eq!(chats, stdout(&env.run(&["cli", "sessions", "--help"])));
    assert_eq!(uninstall, stdout(&env.run(&["cli", "help", "uninstall"])));
    assert!(stdout(&env.run(&["cli", "help", "init"])).starts_with("Usage: switchboard init"));
    assert_eq!(env.run(&["cli", "help", "bogus"]).status.code(), Some(2));
    let json_help = env.run(&["cli", "help", "--json"]);
    let value: Value = serde_json::from_slice(&json_help.stdout).unwrap();
    assert_eq!(value, json!({"ok": true, "help": stdout(&plain)}));
    // The browser viewer is gone: no command, no option, no mention.
    assert!(!stdout(&plain).contains("view"), "{}", stdout(&plain));
    assert!(!stdout(&plain).contains("--open"));
    for args in [
        &["cli", "view"][..],
        &["cli", "status", "--open"],
        &["cli", "-hx"],
    ] {
        assert_eq!(env.run(args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn usage_errors_follow_output_format() {
    let env = Env::new();
    let human = env.run(&["cli", "status", "--local"]);
    assert_eq!(human.status.code(), Some(2));
    assert_eq!(stdout(&human), "");
    assert_eq!(
        stderr(&human),
        "✗ --local, --remote, --name, and --project only work with chats.\n  \
         To fix, see switchboard status --help for the options.\n"
    );
    // With --json, errors are JSON on stdout like every other result.
    let json_error = env.run(&["cli", "status", "--local", "--json"]);
    assert_eq!(stderr(&json_error), "");
    assert_eq!(
        serde_json::from_slice::<Value>(&json_error.stdout).unwrap(),
        json!({"ok": false, "error": "--local, --remote, --name, and --project only work with chats"})
    );
    let unknown = env.run(&["cli", "statsu"]);
    assert_eq!(
        (unknown.status.code(), stderr(&unknown)),
        (
            Some(2),
            "✗ There's no command called statsu.\n  To fix, run switchboard status, or switchboard help to see the commands.\n".into()
        )
    );
    // test_cli_json_failures_go_to_stdout_for_the_mod
    // The approval commands and options are gone.
    for args in [
        &["cli", "approve", "ABCD-EFGH"][..],
        &["cli", "refuse", "ABCD-EFGH"],
        &["cli", "revoke", "x"],
        &["cli", "machines"],
        &["cli", "join", "--owner", "x"],
        &["cli", "ping", "x", "--invite", "y"],
        &["cli", "setup", "--wait", "0"],
        &["cli", "setup", "--yes"],
        &["cli", "setup", "--token", "x"],
    ] {
        assert_eq!(env.run(args).status.code(), Some(2), "{args:?}");
    }
    let missing = env.temp.0.join("missing.json");
    let output = env
        .command(binary(), &["cli", "setup", "--json"])
        .env("SWITCHBOARD_CONFIG", &missing)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]
            .as_str()
            .unwrap()
            .starts_with("setup needs an invite")
    );
    // Status answers its own question; it still exits 1.
    let status = env.run(&["cli", "--config", missing.to_str().unwrap(), "status"]);
    assert_eq!(status.status.code(), Some(1));
    assert_eq!(
        (stdout(&status), stderr(&status)),
        (
            "  Not set up. Run switchboard init.\n".into(),
            String::new()
        )
    );
    // `up` and `down` (and `start` and `stop`) check before they call the
    // service manager.
    for command in ["up", "start", "down", "stop"] {
        let start = env.run(&["cli", "--config", missing.to_str().unwrap(), command]);
        assert_eq!(
            (start.status.code(), stderr(&start)),
            (
                Some(1),
                "✗ This machine isn't set up yet.\n  To fix, run\n      switchboard init\n".into()
            ),
            "{command}"
        );
    }
    assert!(!env.temp.0.join("launchctl.log").exists());
}

#[test]
fn doctor_with_missing_config_keeps_independent_checks_and_writes_nothing() {
    let env = Env::new();
    let missing = env.temp.0.join("absent.json");
    let output = env.run(&[
        "cli",
        "doctor",
        "--json",
        "--config",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["program", "configuration"]);
    assert_eq!(
        value["checks"][1],
        json!({"name": "configuration", "status": "fail", "summary": "This machine isn't set up yet.",
        "fix": "Run switchboard init."})
    );
    assert!(!missing.exists());
    let human = env.run(&["cli", "doctor", "--config", missing.to_str().unwrap()]);
    let text = stdout(&human);
    assert!(
        text.contains("✗ This machine isn't set up yet.\n  To fix, run\n      switchboard init\n"),
        "{text}"
    );
    assert!(
        text.ends_with("\n  1 problem to fix, and 1 warning.\n"),
        "{text}"
    );
}

// Installer ----------------------------------------------------------------

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o777
}

#[test]
fn install_is_standalone_preserves_logs_and_refuses_unmanaged_files() {
    let env = Env::new();
    let home = env.home();
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let source = home.join(".local/share/switchboard");
    let service = if cfg!(target_os = "linux") {
        home.join(".config/systemd/user/switchboard-bridge.service")
    } else {
        home.join("Library/LaunchAgents/dev.switchboard.bridge.plist")
    };
    let service_key = if cfg!(target_os = "linux") {
        "user_service"
    } else {
        "launch_agent"
    };
    let installed = home.join(".local/bin/switchboard");
    assert_eq!(
        result,
        json!({"command": installed, "source": home.join(".local/bin"), "data": source,
        "config": home.join(".config/switchboard/bridge.json"), service_key: service, "started": false})
    );
    assert!(!home.join(".config/switchboard/bridge.json").exists());
    // The command is the program itself: no wrapper, no second copy.
    assert!(std::fs::symlink_metadata(&installed)
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(mode(&installed), 0o700);
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(binary()).unwrap()
    );
    assert!(!source.join("switchboard").exists());
    assert!(std::fs::symlink_metadata(home.join("bin")).is_err());
    assert_eq!(
        std::fs::read_to_string(source.join(".switchboard-install")).unwrap(),
        "1\n"
    );
    assert_eq!(mode(&service), 0o600);
    let service_text = std::fs::read_to_string(&service).unwrap();
    if cfg!(target_os = "linux") {
        assert!(service_text.contains("ExecStart=\""));
        assert!(home
            .join(".config/systemd/user/default.target.wants/switchboard-bridge.service")
            .exists());
    } else {
        assert!(service_text.contains(&format!("\t<array>\n\t\t<string>{}</string>\n\t\t<string>bridge</string>\n\t\t<string>--config</string>", installed.display())));
        assert!(service_text.contains("\t<key>SwitchboardManaged</key>\n\t<true/>\n"));
        assert!(service_text
            .contains("\t<key>Label</key>\n\t<string>dev.switchboard.bridge</string>\n"));
        assert!(service_text.contains("\t<key>Umask</key>\n\t<integer>63</integer>\n"));
    }
    // The installed command works without the build tree, and doctor writes nothing.
    let help = env
        .command(installed.to_str().unwrap(), &["--help"])
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let missing = home.join("missing-bridge.json");
    let doctor = env
        .command(
            installed.to_str().unwrap(),
            &["doctor", "--json", "--config", missing.to_str().unwrap()],
        )
        .output()
        .unwrap();
    assert_eq!(doctor.status.code(), Some(1));
    let diagnostics: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(diagnostics["checks"][0]["name"], "program");
    assert_eq!(diagnostics["checks"][0]["status"], "pass");
    assert!(diagnostics["checks"][0]["summary"]
        .as_str()
        .unwrap()
        .ends_with(", installed in ~/.local/bin."));
    assert!(!missing.exists());
    // Existing logs keep their contents and inode; a widened mode is backed up and reset.
    let log = source.join("daemon.log");
    assert_eq!(mode(&log), 0o600);
    std::fs::write(&log, "preserve existing log\n").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
    let inode = std::fs::metadata(&log).unwrap().ino();
    assert_eq!(env.run(&["install"]).status.code(), Some(0));
    assert_eq!(mode(&log), 0o600);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "preserve existing log\n"
    );
    assert_eq!(std::fs::metadata(&log).unwrap().ino(), inode);
    assert!(std::fs::read_dir(source.join("backups"))
        .unwrap()
        .next()
        .is_some());
    // An unmanaged command in the way stops the installer without changing
    // it: a file of its own, or a link elsewhere.
    std::fs::remove_file(&installed).unwrap();
    std::fs::write(&installed, "unmanaged").unwrap();
    let refused = env.run(&["install"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("unmanaged command: "));
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), "unmanaged");
    std::fs::remove_file(&installed).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", &installed).unwrap();
    let refused = env.run(&["install"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("unmanaged command: "));
    assert_eq!(
        std::fs::read_link(&installed).unwrap(),
        Path::new("/usr/bin/true")
    );
}

#[test]
fn install_upgrades_an_early_release_installation_in_place() {
    let env = Env::new();
    let home = env.home();
    let source = home.join(".local/share/switchboard");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(home.join("bin")).unwrap();
    std::fs::write(source.join(".switchboard-install"), "1\n").unwrap();
    std::fs::write(source.join("old-runtime"), "# old runtime\n").unwrap();
    std::fs::write(
        home.join("bin/switchboard"),
        "#!/bin/sh\n# Managed by Switchboard install_local.py\nexec /x/old-switchboard \"$@\"\n",
    )
    .unwrap();
    if cfg!(target_os = "macos") {
        let agents = home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        let old_plist = switchboard::cli::install::launch_agent("/x/old-switchboard", "/x", "/y")
            .replace("<string>bridge</string>", "<string>/x/old-runtime</string>");
        std::fs::write(agents.join(format!("{LEGACY_LABEL}.plist")), old_plist).unwrap();
    }
    let output = env.run(&["install", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The early release's command is gone; the program is the command now.
    assert!(std::fs::symlink_metadata(home.join("bin/switchboard")).is_err());
    assert!(home.join(".local/bin/switchboard").is_file());
    // A file whose name no installer wrote stays.
    assert_eq!(
        std::fs::read_to_string(source.join("old-runtime")).unwrap(),
        "# old runtime\n"
    );
    let backups: Vec<_> = std::fs::read_dir(source.join("backups")).unwrap().collect();
    assert_eq!(backups.len(), 1);
    if cfg!(target_os = "macos") {
        // The early release's launch agent moved to the new label.
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        let old = home.join(format!("Library/LaunchAgents/{LEGACY_LABEL}.plist"));
        assert_eq!(result["migrated_from"], json!(old));
        assert!(!old.exists());
        assert!(backups[0]
            .as_ref()
            .unwrap()
            .path()
            .join(format!("{LEGACY_LABEL}.plist"))
            .exists());
    }
    // An unmanaged launch agent or source directory is refused.
    if cfg!(target_os = "macos") {
        let plist = home.join("Library/LaunchAgents/dev.switchboard.bridge.plist");
        let text = std::fs::read_to_string(&plist).unwrap().replace(
            "<key>SwitchboardManaged</key>\n\t<true/>",
            "<key>SwitchboardManaged</key>\n\t<false/>",
        );
        std::fs::write(&plist, text).unwrap();
        let refused = env.run(&["install"]);
        assert_eq!(refused.status.code(), Some(1));
        assert!(stderr(&refused).contains("unmanaged launch agent: "));
    }
}

// Start and stop ------------------------------------------------------------

#[cfg(target_os = "macos")]
fn scripted_launchctl(env: &Env, prints: &[i32]) -> PathBuf {
    let log = env.temp.0.join("launchctl.log");
    let sequence = env.temp.0.join("prints");
    std::fs::write(
        &sequence,
        prints.iter().map(|c| format!("{c}\n")).collect::<String>(),
    )
    .unwrap();
    env.fake(
        "launchctl",
        &format!(
            "echo \"$1\" >> '{log}'\nif [ \"$1\" = print ]; then\n  code=$(head -n 1 '{seq}')\n  tail -n +2 '{seq}' > '{seq}.next'; mv '{seq}.next' '{seq}'\n  exit \"${{code:-1}}\"\nfi\nexit 0",
            log = log.display(),
            seq = sequence.display()
        ),
    );
    log
}

#[cfg(target_os = "macos")]
#[test]
fn start_rebootstraps_when_a_kickstarted_job_disappears() {
    let env = Env::new();
    let log = scripted_launchctl(&env, &[0, 1, 0, 0, 0]);
    let config = env.temp.0.join("bridge.json");
    std::fs::write(&config, "{}").unwrap();
    // Without the service file, start says so and runs nothing.
    let output = env.run(&[
        "cli",
        "--config",
        config.to_str().unwrap(),
        "start",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"ok": false, "error": "the bridge's background service isn't installed; run switchboard install --start to install it and start the bridge"})
    );
    assert!(!log.exists());
    let human = env.run(&["cli", "--config", config.to_str().unwrap(), "start"]);
    assert_eq!(
        stderr(&human),
        "✗ The bridge's background service isn't installed.\n  To fix, run switchboard install --start to install it and start the bridge.\n"
    );
    let agents = env.temp.0.join("home/Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("dev.switchboard.bridge.plist"), "plist").unwrap();
    let output = env.run(&[
        "cli",
        "--config",
        config.to_str().unwrap(),
        "start",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"ok": true, "action": "start", "connected": false})
    );
    // `start` is `up`: it lets the service start at login again first.
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "enable\nprint\nkickstart\nprint\nbootstrap\nprint\nprint\nprint\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn start_stops_at_a_refusal_that_repeats_and_says_it_plainly() {
    let env = Env::new();
    env.fake(
        "launchctl",
        "if [ \"$1\" = bootstrap ]; then echo 'Bootstrap failed: 5: Input/output error' >&2; echo 'Try re-running the command as root for richer errors.' >&2; exit 5; fi\nexit 113",
    );
    let config = env.temp.0.join("bridge.json");
    std::fs::write(&config, "{}").unwrap();
    let agents = env.temp.0.join("home/Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("dev.switchboard.bridge.plist"), "plist").unwrap();
    let began = std::time::Instant::now();
    let output = env.run(&["cli", "--config", config.to_str().unwrap(), "start"]);
    assert!(began.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.starts_with(
            "✗ The bridge's service couldn't be started (launchd: Bootstrap failed: 5:"
        ),
        "{text}"
    );
    assert!(!text.contains("root"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn stop_waits_for_the_job_and_allows_a_missing_runtime() {
    let env = Env::new();
    let log = scripted_launchctl(&env, &[0, 0, 0, 1]);
    let config = env.temp.0.join("bridge.json");
    std::fs::write(&config, "{}").unwrap();
    let marker = env.temp.0.join("down");
    let output = env.run(&["cli", "--config", config.to_str().unwrap(), "stop"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "✓ Down. This machine's chats are offline until you run switchboard up.\n  It stays down after a restart. Its setup and joined chats are kept.\n"
    );
    // `stop` is `down`: the service is disabled, then stopped, and the
    // machine is marked down beside its configuration.
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "print\ndisable\nbootout\nprint\nprint\nprint\n"
    );
    assert!(marker.exists());
    // A failed bootout of a job that stays loaded is reported.
    env.fake(
        "launchctl",
        "if [ \"$1\" = bootout ]; then echo 'not permitted' >&2; exit 5; fi\nexit 0",
    );
    let output = env.run(&[
        "cli",
        "--config",
        config.to_str().unwrap(),
        "down",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"ok": false, "error": "not permitted"})
    );
}

// Setup's local identity --------------------------------------------------------

/// An invite token for `url`, as `switchboard server invite` prints one.
fn invite_for(url: &str, pin: Option<&str>, machine: Option<&str>) -> String {
    switchboard::invite::Invite {
        url: url.into(),
        pin: pin.map(str::to_string),
        secret: switchboard::invite::new_secret(),
        machine: machine.map(str::to_string),
    }
    .encode()
}

#[test]
fn setup_makes_a_private_long_lived_identity_and_reuses_it() {
    let env = Env::new();
    let root = env.temp.0.join("cfg");
    let config = root.join("bridge.json");
    let token = invite_for("https://127.0.0.1:1", None, None);
    let args = [
        "cli",
        "--config",
        config.to_str().unwrap(),
        "setup",
        "--machine",
        "new-box",
        "--owner",
        "bob",
        "--invite",
        "-",
        "--no-start",
        "--json",
    ];
    let output = env.run_with(&args, &[], &token);
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("could not reach the server: "),
        "{error}"
    );
    assert_eq!(mode(&root), 0o700);
    for name in [
        "bridge.json",
        "identity/key.pem",
        "identity/cert.pem",
        "identity/identity.json",
    ] {
        assert_eq!(mode(&root.join(name)), 0o600, "{name}");
    }
    assert_eq!(mode(&root.join("identity")), 0o700);
    assert_eq!(mode(&root.join("state")), 0o700);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        written,
        json!({"enabled": true, "server_url": "https://127.0.0.1:1", "ca": null,
        "cert": root.join("identity/cert.pem"), "key": root.join("identity/key.pem"),
        "config_dir": env.home().join(".claude"), "state_dir": root.join("state"), "sessions": []})
    );
    assert_eq!(
        std::fs::read_to_string(root.join("identity/identity.json")).unwrap(),
        "{\"machine\": \"new-box\", \"owner\": \"bob\", \"server_url\": \"https://127.0.0.1:1\", \"joined_by\": \"invite\"}"
    );
    let text = Command::new("openssl")
        .args(["x509", "-noout", "-text", "-in"])
        .arg(root.join("identity/cert.pem"))
        .output()
        .unwrap();
    // LibreSSL (macOS) prints "CN=new-box", OpenSSL 3 "CN = new-box".
    let text = stdout(&text).replace(" = ", "=");
    for needle in [
        "Subject: CN=new-box",
        "Issuer: CN=new-box",
        "CA:FALSE",
        "Digital Signature",
        "TLS Web Client Authentication",
        "Subject Key Identifier",
        "Authority Key Identifier",
        "ecdsa-with-SHA256",
        "prime256v1",
    ] {
        assert!(text.contains(needle), "{needle}\n{text}");
    }
    let key_pub = Command::new("openssl")
        .args(["pkey", "-pubout", "-in"])
        .arg(root.join("identity/key.pem"))
        .output()
        .unwrap();
    let cert_pub = Command::new("openssl")
        .args(["x509", "-pubkey", "-noout", "-in"])
        .arg(root.join("identity/cert.pem"))
        .output()
        .unwrap();
    assert_eq!(key_pub.stdout, cert_pub.stdout);
    // A rerun, with or without the invite, reuses the key and config and
    // never overwrites them.
    let key = std::fs::read(root.join("identity/key.pem")).unwrap();
    for rerun in [&args[..], &args[..4]] {
        let mut rerun = rerun.to_vec();
        rerun.extend(["--no-start", "--json"]);
        let output = env.run_with(&rerun, &[], &token);
        assert_eq!(output.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .starts_with("could not reach the server: "),
            "{error}"
        );
        assert_eq!(std::fs::read(root.join("identity/key.pem")).unwrap(), key);
    }
    // An invite for another server is refused on a machine set up for this one.
    let elsewhere = invite_for("https://127.0.0.1:2", None, None);
    let output = env.run_with(
        &[
            "cli",
            "--config",
            config.to_str().unwrap(),
            "setup",
            "--invite",
            "-",
            "--json",
        ],
        &[],
        &elsewhere,
    );
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .starts_with("this machine is already set up for the server at https://127.0.0.1:1"),
        "{error}"
    );
    // An identity without a config is refused rather than replaced.
    std::fs::remove_file(&config).unwrap();
    let refused = env.run_with(&args, &[], &token);
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.stdout).unwrap()["error"],
        "an identity exists without a bridge config; move ~/.config/switchboard/identity aside first"
    );
    // Invalid machine names and owners are refused before any key is made.
    let other = env.temp.0.join("other/bridge.json");
    let bad = env.run_with(
        &[
            "cli",
            "--config",
            other.to_str().unwrap(),
            "setup",
            "--machine",
            "Bad_Name",
            "--invite",
            "-",
            "--json",
        ],
        &[],
        &token,
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&bad.stdout).unwrap()["error"],
        "machine names use lowercase letters, digits, and hyphens, starting with a letter"
    );
    assert!(!env.temp.0.join("other/identity").exists());
}

#[test]
fn setup_refuses_a_missing_damaged_or_contradicted_invite_before_making_a_key() {
    let env = Env::new();
    let config = env.temp.0.join("cfg/bridge.json");
    let setup = |extra: &[&str], vars: &[(&str, &str)]| {
        let mut args = vec!["cli", "--config", config.to_str().unwrap(), "setup"];
        args.extend_from_slice(extra);
        args.push("--json");
        let output = env.run_with(&args, vars, "");
        assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["error"].as_str().unwrap().to_string()
    };
    assert!(setup(&[], &[])
        .starts_with("setup needs an invite: on the server run switchboard server invite"));
    // An invite on the command line is checked like any other.
    assert!(setup(&["--invite", "sbi1.secret"], &[]).starts_with("the invite is damaged"));
    assert!(setup(&["--invite", "nonsense"], &[]).starts_with("that is not a Switchboard invite"));
    assert!(setup(&[], &[("SWITCHBOARD_INVITE", "nonsense")])
        .starts_with("that is not a Switchboard invite"));
    assert!(setup(&[], &[("SWITCHBOARD_INVITE", "sbi1.AAAA")]).starts_with("the invite is damaged"));
    assert_eq!(
        setup(&["--invite", "-"], &[]),
        "--invite - read no invite from standard input"
    );
    let fixed = invite_for(
        "https://127.0.0.1:1",
        Some(&"ab".repeat(32)),
        Some("laptop"),
    );
    assert_eq!(
        setup(&["--machine", "desktop"], &[("SWITCHBOARD_INVITE", &fixed)]),
        "the invite is for the machine name laptop, not desktop"
    );
    assert_eq!(
        setup(
            &["--server-pin", &"cd".repeat(32)],
            &[("SWITCHBOARD_INVITE", &fixed)]
        ),
        "--server-pin differs from the server certificate the invite names"
    );
    assert!(setup(
        &["--server-url", "https://127.0.0.1:2"],
        &[("SWITCHBOARD_INVITE", &fixed)]
    )
    .starts_with("--server-url https://127.0.0.1:2 is not the server the invite is for"));
    // The invite given on the command line, as pasted (wrapped, indented,
    // quoted) or after an equals sign, is the one setup uses.
    let pasted = format!("  `{}\n{}`  ", &fixed[..30], &fixed[30..]);
    for invite in [fixed.as_str(), pasted.as_str()] {
        assert_eq!(
            setup(&["--invite", invite, "--machine", "desktop"], &[]),
            "the invite is for the machine name laptop, not desktop"
        );
    }
    assert_eq!(
        setup(
            &[&format!("--invite={pasted}"), "--machine", "desktop"],
            &[]
        ),
        "the invite is for the machine name laptop, not desktop"
    );
    assert!(!env.temp.0.join("cfg").exists());
    // Only init and setup take --invite.
    let output = env.run(&["cli", "status", "--invite", &fixed, "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"ok": false, "error": "--owner, --no-start, and --invite only work with init"})
    );
    let output = env.run(&["cli", "status", "--server-pin", "ab", "--json"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"ok": false, "error": "--server-url and --server-pin only work with setup"})
    );
}

#[test]
fn setup_reads_at_most_64_kib_of_invite_from_standard_input() {
    use std::io::Write;
    let env = Env::new();
    let config = env.temp.0.join("cfg/bridge.json");
    let mut child = env
        .command(
            binary(),
            &[
                "cli",
                "--config",
                config.to_str().unwrap(),
                "setup",
                "--invite",
                "-",
                "--json",
            ],
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Input without a line break that never ends: the writer keeps the pipe
    // open, so a reader waiting for a line break would wait forever.
    let mut stdin = child.stdin.take().unwrap();
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let chunk = vec![b'A'; 4096];
        for _ in 0..64 {
            if stdin.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = stopped.recv_timeout(std::time::Duration::from_secs(20));
        drop(stdin);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let _ = stop.send(());
    writer.join().unwrap();
    let status = status.expect("setup kept reading standard input past 64 KiB");
    assert_eq!(status.code(), Some(1));
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .starts_with("that is not a Switchboard invite"),
        "{value}"
    );
    assert!(!env.temp.0.join("cfg").exists());
}

// Invites and private servers --------------------------------------------------

/// A private-mode server on a free loopback port; its pin and port.
struct PrivateServer {
    child: std::process::Child,
    port: u16,
    pin: String,
    config: PathBuf,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl PrivateServer {
    fn start(dir: &Path) -> PrivateServer {
        PrivateServer::start_on(dir, free_port())
    }

    /// Start on `port`, in `dir` (reused when it exists).
    fn start_on(dir: &Path, port: u16) -> PrivateServer {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        if !dir.exists() {
            std::fs::create_dir(dir).unwrap();
            std::fs::create_dir(dir.join("state")).unwrap();
            std::fs::set_permissions(dir.join("state"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let private = |name: &str, text: &str| {
            std::fs::write(dir.join(name), text).unwrap();
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        };
        if !dir.join("peers.json").exists() {
            private("peers.json", "{}\n");
        }
        private(
            "server.json",
            &format!(
                r#"{{"tls": "private", "names": ["127.0.0.1"], "listen": "127.0.0.1", "port": {port}, "db": "state/messages.db", "peers_file": "peers.json"}}"#
            ),
        );
        let mut command = Command::new(binary());
        command
            .args(["server", "--config"])
            .arg(dir.join("server.json"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        // SAFETY: signal(2) is async-signal-safe; runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let mut lines = std::io::BufReader::new(child.stderr.take().unwrap()).lines();
        let event = lines.next().unwrap().unwrap();
        std::thread::spawn(move || for _ in lines {});
        let pin = event.split("\"sha256\": \"").nth(1).unwrap()[..64].to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "server did not listen"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        PrivateServer {
            child,
            port,
            pin,
            config: dir.join("server.json"),
        }
    }

    /// `switchboard server COMMAND ... --config CONFIG --json`, as this user
    /// (who owns the server's files, like its service account).
    fn local(&self, env: &Env, args: &[&str]) -> Value {
        let mut all = vec!["server"];
        all.extend_from_slice(args);
        all.extend(["--config", self.config.to_str().unwrap(), "--json"]);
        let output = env.run(&all);
        let value: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{all:?}: {}{}", stdout(&output), stderr(&output)));
        assert_eq!(
            output.status.code(),
            Some(if value["ok"] == true { 0 } else { 1 }),
            "{value}"
        );
        value
    }

    fn invite(&self, env: &Env, args: &[&str]) -> String {
        let mut all = vec!["invite"];
        all.extend_from_slice(args);
        let value = self.local(env, &all);
        assert_eq!(value["ok"], true, "{value}");
        value["invite"].as_str().unwrap().to_string()
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// On a machine the server removed, the bridge records the server's refusal
/// and doctor's bridge check names it, with the fix (a new invite), instead
/// of blaming the network.
#[test]
fn doctor_says_when_the_server_no_longer_accepts_this_machine() {
    let env = Env::new();
    let server = PrivateServer::start(&env.temp.0.join("server"));
    let config = env.temp.0.join("first/bridge.json");
    let token = server.invite(&env, &[]);
    let output = env.run_with(
        &[
            "cli",
            "--config",
            config.to_str().unwrap(),
            "setup",
            "--machine",
            "first",
            "--owner",
            "op",
            "--no-start",
            "--json",
            "--invite",
            "-",
        ],
        &[],
        &format!("{token}\n"),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    assert_eq!(server.local(&env, &["revoke", "first"])["ok"], true);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    let state = PathBuf::from(written["state_dir"].as_str().unwrap());
    // The test process stands in for the running bridge.
    let pid = std::process::id() as i64;
    let bridge_check = |refused: Option<&str>| -> Value {
        let mut runtime = json!({"pid": pid,
            "procStart": switchboard::identity::process_start(pid).unwrap(),
            "platform": "test", "machine": "first", "connected": false,
            "discovery_mode": "events", "discovery_detail": null, "published": [],
            "remote": {"sessions": 0, "machines": 0}, "updated_at": 1.0});
        if let Some(code) = refused {
            runtime["server_refused"] = json!(code);
        }
        let path = state.join("runtime.json");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, format!("{runtime}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let output = env.run(&["doctor", "--json", "--config", config.to_str().unwrap()]);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "bridge")
            .unwrap()
            .clone()
    };
    let check = bridge_check(Some("not_enrolled"));
    assert_eq!(check["status"], "fail");
    assert_eq!(
        check["summary"],
        "The bridge is running, but the server doesn't accept this machine's key: it was removed from the server, or never added."
    );
    assert_eq!(
        check["fix"],
        "Make a new invite on the server (sudo switchboard server invite), then run switchboard init here and choose to join again."
    );
    // Offline for any other reason: the network advice, as before.
    let check = bridge_check(None);
    assert_eq!(check["status"], "fail");
    assert_eq!(
        check["summary"],
        "The bridge is running, but not connected to the server."
    );
    assert!(check["fix"].as_str().unwrap().contains("network"));
    std::fs::remove_file(state.join("runtime.json")).unwrap();
}

/// A fixed name enrolls one machine (a second under the same name is
/// refused as already on the server), so an invite with --name and more
/// than one use is refused when it would be made, and nothing is stored.
#[test]
fn a_named_invite_with_more_than_one_use_is_refused() {
    let env = Env::new();
    let server = PrivateServer::start(&env.temp.0.join("server"));
    let config = server.config.to_str().unwrap();
    for json in [false, true] {
        let mut args = vec![
            "server", "invite", "--name", "laptop", "--uses", "3", "--config", config,
        ];
        if json {
            args.push("--json");
        }
        let output = env.run(&args);
        assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
        assert_eq!(stdout(&output), "");
        let text = stderr(&output);
        // The sentence as people read it, across the lines it wraps onto.
        let read = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            read.contains(
                "An invite with --name enrolls only that one machine, so it can't have 3 uses."
            ),
            "{text}"
        );
        assert!(
            read.contains(
                "drop --name to let 3 machines join under their own names, or use --uses 1."
            ),
            "{text}"
        );
    }
    let listed = server.local(&env, &["invites"]);
    assert_eq!(listed["invites"], json!([]), "{listed}");
    // Either alone is fine.
    server.invite(&env, &["--name", "laptop", "--uses", "1"]);
    server.invite(&env, &["--uses", "3"]);
}

/// `server status` against a running private server, as the account that
/// owns its files: counts (never lists), the certificate and pin it
/// presents, the database, and problems first. Without systemd (or with no
/// installed service, on a Linux test machine) the service is a problem,
/// so the exit status is 1 either way.
#[test]
fn server_status_reports_health_and_counts() {
    let env = Env::new();
    let server = PrivateServer::start(&env.temp.0.join("server"));
    let config = server.config.to_str().unwrap();
    let status = || {
        let output = env.run(&["server", "status", "--config", config, "--json"]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        let value: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{}{}", stdout(&output), stderr(&output)));
        value
    };
    let value = status();
    assert_eq!(value["ok"], false);
    assert_eq!(value["problems"][0]["check"], "service", "{value}");
    assert_eq!(value["problems"][0]["level"], "fail");
    assert_eq!(value["service"]["unit"], "switchboard-server.service");
    assert_eq!(value["mode"], "private");
    assert_eq!(
        value["address"],
        json!(format!("https://127.0.0.1:{}", server.port))
    );
    assert_eq!(value["port"], json!(server.port));
    assert_eq!(value["answering"], true);
    assert_eq!(value["pin"], json!(server.pin));
    assert_eq!(value["certificate"]["sha256"], json!(server.pin));
    assert_eq!(value["certificate"]["served"], true);
    assert_eq!(value["certificate"]["self_signed"], true);
    assert_eq!(value["certificate"]["names"], json!(["127.0.0.1"]));
    assert!(value["certificate"]["days_left"].as_i64().unwrap() > 30_000);
    assert_eq!(
        value["machines"],
        json!({"enrolled": 0, "seen_last_10_minutes": 0})
    );
    assert_eq!(value["invites"], json!({"open": 0}));
    assert!(value["database"]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(value["command_version"], json!(switchboard::VERSION));
    assert!(value.get("machine").is_none() && value["machines"].get("list").is_none());
    // One invite, used by one machine, and one left open.
    let token = server.invite(&env, &[]);
    server.invite(&env, &["--uses", "2"]);
    let joined = env.temp.0.join("first/bridge.json");
    let output = env.run_with(
        &[
            "cli",
            "--config",
            joined.to_str().unwrap(),
            "setup",
            "--machine",
            "first",
            "--owner",
            "op",
            "--no-start",
            "--json",
            "--invite",
            "-",
        ],
        &[],
        &format!("{token}\n"),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let value = status();
    assert_eq!(
        value["machines"],
        json!({"enrolled": 1, "seen_last_10_minutes": 1})
    );
    assert_eq!(value["invites"], json!({"open": 1}));
    // For people: problems first, then the facts as rows, never the lists.
    let output = env.run(&["server", "status", "--config", config]);
    assert_eq!(output.status.code(), Some(1));
    let text = stdout(&output);
    assert!(text.starts_with("✗ "), "{text}");
    assert!(!text.contains("The server is healthy"), "{text}");
    assert!(
        text.contains("\n  Mode         private network\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!("\n  Pin          {}\n", server.pin)),
        "{text}"
    );
    assert!(
        text.contains("\n  Machines     1 machine, seen in the last 10 minutes\n"),
        "{text}"
    );
    assert!(text.contains("\n  Invites      1 open invite\n"), "{text}");
    assert!(!text.contains("first") && !text.contains(" op "), "{text}");
    // Usage errors and a missing config.
    let output = env.run(&["server", "status", "extra"]);
    assert_eq!(output.status.code(), Some(2));
    let missing = env.temp.0.join("none/server.json");
    let output = env.run(&[
        "server",
        "status",
        "--config",
        missing.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["error"]
        .as_str()
        .unwrap()
        .starts_with("there is no server config at "));
    let output = env.run(&["server", "status", "--config", missing.to_str().unwrap()]);
    assert!(
        stderr(&output).contains("There's no Switchboard server on this machine."),
        "{}",
        stderr(&output)
    );
}

/// `server logs` and `server uninstall` refuse bad options, and uninstall
/// refuses to run without root (or off Linux) before touching anything.
#[test]
fn server_logs_and_uninstall_check_their_arguments() {
    let env = Env::new();
    for args in [
        &["server", "logs", "--lines", "0"][..],
        &["server", "logs", "--since", "5x"],
        &["server", "logs", "extra"],
        &["server", "uninstall", "--force"],
    ] {
        let output = env.run(args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    for command in ["status", "logs", "uninstall"] {
        let output = env.run(&["cli", "server", command, "--help"]);
        assert_eq!(output.status.code(), Some(0));
        assert!(
            stdout(&output).starts_with(&format!("Usage: switchboard server {command}")),
            "{}",
            stdout(&output)
        );
    }
    // This test never runs as root, so uninstall stops at once.
    let output = env.run(&["server", "uninstall", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    let error = value["error"].as_str().unwrap();
    assert!(
        error == "run as root" || error == "the server runs only on Linux with systemd",
        "{error}"
    );
}

#[test]
fn invites_enroll_machines_of_a_pinned_server() {
    let env = Env::new();
    let server = PrivateServer::start(&env.temp.0.join("server"));
    let url = format!("https://127.0.0.1:{}", server.port);
    let setup = |config: &Path, extra: &[&str], vars: &[(&str, &str)], input: &str| {
        let mut args = vec![
            "cli",
            "--config",
            config.to_str().unwrap(),
            "setup",
            "--owner",
            "op",
            "--no-start",
            "--json",
        ];
        args.extend_from_slice(extra);
        env.run_with(&args, vars, input)
    };
    let error = |output: &Output| -> String {
        assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["error"].as_str().unwrap().to_string()
    };
    let made = server.local(&env, &["invite"]);
    assert_eq!(made["url"], json!(url));
    assert_eq!(made["server_pin"], json!(server.pin));
    assert_eq!(made["uses"], 1);
    assert_eq!(made["machine"], Value::Null);
    let token = made["invite"].as_str().unwrap().to_string();
    let parsed = switchboard::invite::Invite::parse(&token).unwrap();
    assert_eq!(parsed.pin.as_deref(), Some(server.pin.as_str()));
    // An invite naming another certificate is refused before any key is made.
    let first = env.temp.0.join("first/bridge.json");
    let forged = switchboard::invite::Invite {
        pin: Some("ab".repeat(32)),
        ..parsed.clone()
    }
    .encode();
    let message = error(&setup(
        &first,
        &["--machine", "first", "--invite", "-"],
        &[],
        &forged,
    ));
    assert!(
        message.contains("does not match the pinned SHA-256"),
        "{message}"
    );
    assert!(message.contains(&server.pin), "{message}");
    assert!(!env.temp.0.join("first").exists() || !env.temp.0.join("first/identity").exists());
    // The invite on standard input pins the server and enrolls the machine.
    let output = setup(
        &first,
        &["--machine", "first", "--invite", "-"],
        &[],
        &format!("{token}\n"),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result,
        json!({"ok": true, "action": "setup", "machine": "first", "owner": "op", "enrolled": true, "started": false})
    );
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&first).unwrap()).unwrap();
    let ca = env.temp.0.join("first/server-cert.pem");
    assert_eq!(written["ca"], json!(ca));
    assert_eq!(written["server_url"], json!(url));
    assert_eq!(mode(&ca), 0o600);
    // The invite works once.
    let second = env.temp.0.join("second/bridge.json");
    let message = error(&setup(
        &second,
        &["--machine", "second", "--invite", "-"],
        &[],
        &token,
    ));
    assert!(
        message.starts_with("this invite was already used"),
        "{message}"
    );
    // A new one, from the environment, with the pin also given (any case, colons).
    let colons = server
        .pin
        .to_uppercase()
        .as_bytes()
        .chunks(2)
        .map(|pair| std::str::from_utf8(pair).unwrap())
        .collect::<Vec<_>>()
        .join(":");
    let fresh = server.invite(&env, &["--uses", "2", "--expires", "1h"]);
    let output = setup(
        &second,
        &["--machine", "second", "--server-pin", &colons],
        &[("SWITCHBOARD_INVITE", &fresh)],
        "",
    );
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    // A fixed name: refused under another, used when none is given.
    let fixed = server.invite(&env, &["--name", "laptop"]);
    let third = env.temp.0.join("third/bridge.json");
    let message = error(&setup(
        &third,
        &["--machine", "desk", "--invite", "-"],
        &[],
        &fixed,
    ));
    assert_eq!(
        message,
        "the invite is for the machine name laptop, not desk"
    );
    let output = setup(&third, &["--invite", "-"], &[], &fixed);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["machine"], "laptop");
    // The server lists its open invites without secrets, and cancels one.
    let listed = server.local(&env, &["invites"]);
    let rows = listed["invites"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["uses"], 1);
    assert!(!listed
        .to_string()
        .contains(&switchboard::invite::Invite::parse(&fresh).unwrap().secret));
    let id = rows[0]["id"].as_str().unwrap().to_string();
    let revoked = server.local(&env, &["invite", "revoke", &id]);
    assert_eq!(revoked, json!({"ok": true, "id": id, "state": "revoked"}));
    assert!(server.local(&env, &["invites"])["invites"]
        .as_array()
        .unwrap()
        .is_empty());
    let fourth = env.temp.0.join("fourth/bridge.json");
    let message = error(&setup(
        &fourth,
        &["--machine", "fourth", "--invite", "-"],
        &[],
        &fresh,
    ));
    assert!(
        message.starts_with("the server didn't accept the invite"),
        "{message}"
    );
    // The machines, as the server lists them.
    let listing = server.local(&env, &["machines"]);
    let names: Vec<&str> = listing["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["machine"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["first", "laptop", "second"]);
    assert!(listing["machines"][0]["last_seen"].is_number(), "{listing}");
    // An enrolled machine reruns setup without an invite.
    let output = setup(&first, &[], &[], "");
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    // A rerun with a different pin is refused rather than retrusting.
    let message = error(&setup(&first, &["--server-pin", &"cd".repeat(32)], &[], ""));
    assert!(
        message.contains("trusts a different server certificate"),
        "{message}"
    );
    // Revoked on the server, the key is refused at once.
    let revoked = server.local(&env, &["revoke", "second"]);
    assert_eq!(
        revoked,
        json!({"ok": true, "machine": "second", "state": "revoked"})
    );
    let message = error(&setup(&second, &[], &[], ""));
    assert!(
        message.starts_with("this machine isn't on the server; ask for an invite"),
        "{message}"
    );
    let missing = server.local(&env, &["revoke", "second"]);
    assert_eq!(missing["ok"], false);
    // Human output of the local commands.
    let output = env.run(&[
        "server",
        "invite",
        "--config",
        server.config.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        text.starts_with("  Invite\n  Single use, expires in 1 hour, at "),
        "{text}"
    );
    // Not a terminal: the invite stays on one line, for scripts.
    assert!(
        text.lines().any(|l| l.starts_with("      sbi1.")
            && switchboard::invite::Invite::parse(l.trim()).is_ok()),
        "{text}"
    );
    assert!(text.contains("/install.sh"), "{text}");
    assert!(!text.contains("setup"), "{text}");
    assert!(!text.contains("Make more invites"), "{text}");
    let output = env.run(&[
        "server",
        "invite",
        "--expires",
        "1y",
        "--config",
        server.config.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn setup_works_with_a_home_reached_through_a_link_and_explains_other_failures() {
    let env = Env::new();
    let server = PrivateServer::start(&env.temp.0.join("server"));
    // HOME through a symbolic link, as a home under /tmp is on macOS.
    let link = env.temp.0.join("link");
    std::os::unix::fs::symlink(&env.temp.0, &link).unwrap();
    let linked_home = link.join("home");
    let token = server.invite(&env, &[]);
    let output = env.run_with(
        &[
            "cli",
            "setup",
            "--machine",
            "first",
            "--owner",
            "op",
            "--no-start",
            "--json",
            "--invite",
            "-",
        ],
        &[("HOME", linked_home.to_str().unwrap())],
        &format!("{token}\n"),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    // Everything is written under, and refers to, the real path.
    let config = env.home().join(".config/switchboard/bridge.json");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        written["cert"],
        json!(env.home().join(".config/switchboard/identity/cert.pem"))
    );
    let output = env.run_with(
        &["cli", "doctor", "--json"],
        &[("HOME", linked_home.to_str().unwrap())],
        "",
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let configuration = value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "configuration")
        .unwrap();
    assert_eq!(configuration["status"], "pass", "{value}");

    // A config named explicitly through a link is still refused (its
    // directories must be link-free), and setup says why instead of
    // claiming the machine is not set up.
    let token = server.invite(&env, &[]);
    let config = link.join("second/bridge.json");
    let output = env.run_with(
        &[
            "cli",
            "--config",
            config.to_str().unwrap(),
            "setup",
            "--machine",
            "second",
            "--owner",
            "op",
            "--no-start",
            "--json",
            "--invite",
            "-",
        ],
        &[],
        &format!("{token}\n"),
    );
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let message = value["error"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "the bridge config {} cannot be used: a directory above it",
            config.display()
        )),
        "{message}"
    );
    assert!(!message.contains("not set up"), "{message}");
}

#[test]
fn server_invite_refuses_an_address_no_machine_could_use() {
    let env = Env::new();
    let server = PrivateServer::start(&env.temp.0.join("server"));
    let config = server.config.to_str().unwrap().to_string();
    for bad in [
        "http://plain",
        "https://",
        "https://host/path",
        "https://host?x=1",
        "https://host#frag",
        "https://user@host",
        "https://host:0",
        "https://host:65536",
        "https://host:x",
        "https://bad_host",
        "https://[::1",
        "https://host\r\nInjected: 1",
    ] {
        let output = env.run(&[
            "server", "invite", "--url", bad, "--config", &config, "--json",
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{bad:?}: {}",
            stdout(&output)
        );
        assert!(
            stderr(&output).starts_with("✗ --url must be https://HOST"),
            "{bad:?}: {}",
            stderr(&output)
        );
    }
    // None of them made an invite or took an open-invite slot.
    assert_eq!(server.local(&env, &["invites"])["invites"], json!([]));
    // A trailing slash is still accepted and dropped.
    let made = server.local(&env, &["invite", "--url", "https://sb.example.com:8443/"]);
    assert_eq!(made["url"], json!("https://sb.example.com:8443"));
    let parsed = switchboard::invite::Invite::parse(made["invite"].as_str().unwrap()).unwrap();
    assert_eq!(parsed.url, "https://sb.example.com:8443");
}

#[test]
fn setup_without_a_pin_stops_before_writing_and_a_pinned_rerun_recovers() {
    let env = Env::new();
    let port = free_port();
    let url = format!("https://127.0.0.1:{port}");
    // An invite without a pin, as for a server with a public certificate.
    let pinless = invite_for(&url, None, None);
    let setup = |config: &Path, extra: &[&str], vars: &[(&str, &str)], input: &str| {
        let mut args = vec![
            "cli",
            "--config",
            config.to_str().unwrap(),
            "setup",
            "--machine",
            "first",
            "--owner",
            "op",
            "--no-start",
            "--json",
        ];
        args.extend_from_slice(extra);
        env.run_with(&args, vars, input)
    };
    let error = |output: &Output| -> String {
        assert_eq!(output.status.code(), Some(1), "{}", stdout(output));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["error"].as_str().unwrap().to_string()
    };
    // An earlier run while the server was down left a config that pins no
    // certificate.
    let first = env.temp.0.join("first/bridge.json");
    let output = setup(&first, &["--invite", "-"], &[], &pinless);
    assert!(error(&output).starts_with("could not reach the server: "));
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&first).unwrap()).unwrap();
    assert_eq!(written["ca"], Value::Null);

    let server_dir = env.temp.0.join("server");
    let server = PrivateServer::start_on(&server_dir, port);
    // A new machine with a pinless invite: refused before any key or config
    // is written, with the fingerprint to compare.
    let fresh = env.temp.0.join("fresh/bridge.json");
    let message = error(&setup(&fresh, &["--invite", "-"], &[], &pinless));
    assert!(message.contains("rerun with --server-pin"), "{message}");
    assert!(message.contains(&server.pin), "{message}");
    assert!(!env.temp.0.join("fresh/identity").exists());
    assert!(!fresh.exists());
    // The same for the existing config that pins nothing.
    let message = error(&setup(&first, &[], &[], ""));
    assert!(message.contains("rerun with --server-pin"), "{message}");
    // Rerunning with --server-pin pins it in the existing config, and an
    // invite read from standard input enrolls the machine.
    let token = server.invite(&env, &[]);
    let output = setup(
        &first,
        &["--server-pin", &server.pin, "--invite", "-"],
        &[],
        &format!("{token}\n"),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let ca = env.temp.0.join("first/server-cert.pem");
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&first).unwrap()).unwrap();
    assert_eq!(written["ca"], json!(ca));
    assert_eq!(mode(&first), 0o600);
    assert_eq!(mode(&ca), 0o600);

    // The server's certificate is issued again (new names): the old pin no
    // longer connects, a pin the server does not present is refused, and the
    // new pin is trusted in place of the old one.
    drop(server);
    for name in ["private-cert.pem", "private-key.pem"] {
        std::fs::remove_file(server_dir.join("state/tls").join(name)).unwrap();
    }
    let server = PrivateServer::start_on(&server_dir, port);
    let message = error(&setup(&first, &[], &[], ""));
    assert!(
        message.starts_with("could not reach the server: "),
        "{message}"
    );
    let wrong = "cd".repeat(32);
    let message = error(&setup(&first, &["--server-pin", &wrong], &[], ""));
    assert!(
        message.contains("trusts a different server certificate"),
        "{message}"
    );
    let output = setup(&first, &["--server-pin", &server.pin], &[], "");
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let output = setup(&first, &[], &[], "");
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
}

#[test]
fn version_is_reported() {
    let env = Env::new();
    let output = env.run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        format!("switchboard {}\n", switchboard::VERSION)
    );
}

/// The installed command is a wrapper that runs `switchboard cli "$@"`, so
/// what users are told to run (`switchboard server ...`, `switchboard install
/// --start`, `switchboard bridge` under their own supervisor, `switchboard
/// --version`) has to work after `cli` too.
#[test]
fn roles_work_through_the_installed_wrapper() {
    let env = Env::new();
    let output = env.run(&["cli", "server", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("Usage: switchboard server --config FILE\n"),
        "{}",
        stdout(&output)
    );
    let output = env.run(&["cli", "server", "invite", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("Usage: switchboard server invite "));
    let output = env.run(&["cli", "install", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("Usage: switchboard install"));
    let output = env.run(&["cli", "bridge", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("Usage: switchboard bridge --config FILE\n"));
    let output = env.run(&["cli", "--version"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("switchboard {}\n", switchboard::VERSION)
    );
    // The command line's own commands are unchanged.
    let output = env.run(&["cli", "help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).starts_with("Usage: switchboard COMMAND [options]\n"));
}

#[test]
fn enroll_local_has_help_and_still_refuses_anything_else() {
    let env = Env::new();
    for flag in ["--help", "-h"] {
        let output = env.run(&["server", "enroll-local", flag]);
        assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
        let text = stdout(&output);
        assert!(
            text.starts_with("Usage: switchboard server enroll-local\n"),
            "{text}"
        );
        assert!(text.contains("standard input"), "{text}");
    }
    // Not root (the tests never run as root), or any other argument: the
    // same one-line JSON refusal as before.
    for args in [
        &["server", "enroll-local"][..],
        &["server", "enroll-local", "x"],
    ] {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stdout(&output),
            "{\"ok\": false, \"error\": \"run the installed helper as root with no arguments\"}\n"
        );
    }
}

#[test]
fn logs_show_the_bridge_activity_readably_and_raw_with_json() {
    let env = Env::new();
    let share = env.home().join(".local/share/switchboard");
    // No log yet.
    let output = env.run(&["cli", "logs"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout(&output).starts_with("  The bridge hasn't logged anything yet"),
        "{}",
        stdout(&output)
    );
    std::fs::create_dir_all(&share).unwrap();
    let lines = [
        r#"{"at": "2026-10-31T21:02:11Z", "event": "ready", "machine": "laptop"}"#,
        r#"{"at": "2026-10-31T21:05:40Z", "event": "offline", "detail": "server unavailable; native proxies withdrawn"}"#,
        r#"{"at": "2026-10-31T21:06:00Z", "event": "status", "id": "0123456789abcdef", "state": "refused", "detail": "the chat is busy"}"#,
    ];
    std::fs::write(share.join("daemon.log"), lines.join("\n") + "\n").unwrap();
    let output = env
        .command(binary(), &["cli", "logs", "--lines", "2"])
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert_eq!(
        stdout(&output),
        "  Oct 31 21:05:40  offline         server unavailable; native proxies withdrawn\n  Oct 31 21:06:00  delivery        message 01234567 refused: the chat is busy\n"
    );
    let raw = env.run(&["cli", "logs", "--json", "--lines", "1"]);
    assert_eq!(stdout(&raw), format!("{}\n", lines[2]));
    // What the bridge printed on standard error follows.
    std::fs::write(
        share.join("daemon-error.log"),
        "switchboard bridge: something broke\n",
    )
    .unwrap();
    let output = env.run(&["cli", "logs"]);
    assert!(
        stdout(&output).ends_with(
            "\n\n  The bridge's error output, latest last\n  switchboard bridge: something broke\n"
        ),
        "{}",
        stdout(&output)
    );
    for bad in ["0", "x", "10001"] {
        let output = env.run(&["cli", "logs", "--lines", bad]);
        assert_eq!(output.status.code(), Some(2), "{bad}");
        assert!(
            stderr(&output).contains("--lines needs a number from 1 to 10000"),
            "{}",
            stderr(&output)
        );
    }
    let output = env.run(&["cli", "status", "--follow"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--follow and --lines only work with logs"));
}

/// `logs --json` prints a line that isn't JSON with its control characters
/// escaped, never as raw escape sequences.
#[test]
fn logs_json_escapes_lines_that_are_not_json() {
    let env = Env::new();
    let share = env.home().join(".local/share/switchboard");
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(
        share.join("daemon.log"),
        "{\"at\": \"2026-10-31T21:02:11Z\", \"event\": \"ready\"}\nboom \u{1b}]0;owned\u{7}\u{1b}[2J\r\n",
    )
    .unwrap();
    let raw = env.run(&["cli", "logs", "--json"]);
    assert_eq!(raw.status.code(), Some(0));
    assert_eq!(
        stdout(&raw),
        "{\"at\": \"2026-10-31T21:02:11Z\", \"event\": \"ready\"}\nboom \\u001b]0;owned\\u0007\\u001b[2J\\u000d\n"
    );
}

/// `logs --follow` ends, cleanly and soon, once its reader closes the pipe:
/// at the next line it can't write, and while nothing new is logged too.
#[test]
fn logs_follow_ends_when_its_pipe_is_closed() {
    use std::io::{BufRead, Write};
    let env = Env::new();
    let share = env.home().join(".local/share/switchboard");
    std::fs::create_dir_all(&share).unwrap();
    let log = share.join("daemon.log");
    std::fs::write(
        &log,
        "{\"at\": \"2026-10-31T21:02:11Z\", \"event\": \"ready\"}\n",
    )
    .unwrap();
    for (json, more) in [(false, true), (true, true), (false, false)] {
        let mut args = vec!["cli", "logs", "--follow"];
        if json {
            args.push("--json");
        }
        let mut child = env
            .command(binary(), &args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Read the first line, then close the pipe, as `| head -n 1` does.
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert!(
            first.contains("ready") || first.contains("connected"),
            "{first}"
        );
        drop(reader);
        if more {
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            for n in 0..3 {
                writeln!(
                    file,
                    "{{\"at\": \"2026-10-31T21:03:0{n}Z\", \"event\": \"offline\"}}"
                )
                .unwrap();
            }
        }
        let began = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if began.elapsed() > std::time::Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "logs --follow (json {json}, more {more}) kept running after its pipe closed"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(status.code(), Some(0), "json {json}, more {more}");
        let mut said = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut said).unwrap();
        assert_eq!(said, "", "json {json}, more {more}");
    }
}

#[test]
fn completion_prints_a_script_per_shell_and_completes_chats_from_local_files() {
    let env = Env::new();
    for shell in ["bash", "zsh", "fish"] {
        let output = env.run(&["cli", "completion", shell]);
        assert_eq!(output.status.code(), Some(0), "{shell}");
        let text = stdout(&output);
        assert!(
            text.contains("switchboard completion --words") || text.contains("completion --words"),
            "{text}"
        );
        assert!(text.contains("logs") && text.contains("details"), "{shell}");
    }
    let output = env.run(&["cli", "completion", "tcsh"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("✗ Completion works with bash, zsh, or fish, not tcsh."));
    assert_eq!(env.run(&["cli", "completion"]).status.code(), Some(2));
    // Chats on other machines, as their bridge proxies name them; nothing
    // from the server.
    let sessions = env.home().join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("4242.json"),
        r#"{"pid": 4242, "sessionId": "x", "name": "switchboard/workstation/research-837bd3b0", "peerBridgeProxy": true}"#,
    )
    .unwrap();
    std::fs::write(
        sessions.join("4243.json"),
        r#"{"pid": 4243, "sessionId": "y", "name": "local chat"}"#,
    )
    .unwrap();
    let output = env.run(&["cli", "completion", "--words", "chats"]);
    assert_eq!(stdout(&output), "workstation/research-837bd3b0\n");
    assert_eq!(
        stdout(&env.run(&["cli", "completion", "--words", "ids"])),
        ""
    );
}

/// The bash script completes commands, options, and chats (when bash is
/// here to run it).
#[test]
fn the_bash_completion_completes_commands_options_and_chats() {
    let env = Env::new();
    let Some(bash) = ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .find(|p| Path::new(p).exists())
    else {
        return;
    };
    let wrapper = env.temp.0.join("bin/switchboard");
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec {} cli \"$@\"\n", binary()),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sessions = env.home().join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("4242.json"),
        r#"{"pid": 4242, "sessionId": "x", "name": "switchboard/workstation/research-837bd3b0", "peerBridgeProxy": true}"#,
    )
    .unwrap();
    let complete = |words: &str| {
        let script = format!(
            "eval \"$(switchboard completion bash)\"\nCOMP_WORDS=({words})\nCOMP_CWORD=$(( ${{#COMP_WORDS[@]}} - 1 ))\n_switchboard\nprintf '%s\\n' \"${{COMPREPLY[@]}}\"\n"
        );
        stdout(
            &env.command(bash, &["--norc", "--noprofile", "-c", &script])
                .output()
                .unwrap(),
        )
    };
    assert!(
        complete("switchboard d").contains("doctor\ndown\n"),
        "{}",
        complete("switchboard d")
    );
    assert_eq!(complete("switchboard chats --de"), "--details\n");
    assert_eq!(
        complete("switchboard ping w"),
        "workstation/research-837bd3b0\n"
    );
    assert_eq!(complete("switchboard logs --f"), "--follow\n");
    assert_eq!(complete("switchboard completion z"), "zsh\n");
}
