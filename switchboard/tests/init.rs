//! `switchboard init` without a terminal: every answer from the options,
//! failures that leave nothing behind, reruns that pick up where a run
//! stopped, `--replace`, the plugin, and the server path through the
//! stand-in server (`SWITCHBOARD_INIT_STUB_SERVER`), against real private
//! servers on 127.0.0.1. Each test has a private HOME and fake launchctl,
//! systemctl, and claude first on PATH, so nothing reaches the real
//! installation, service managers, or Claude Code settings, and nothing
//! runs as root.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_switchboard")
}

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let template = std::ffi::CString::new("/tmp/sbinit-XXXXXX").unwrap();
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

fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_private(path: &Path, data: &str) {
    std::fs::write(path, data).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A private-mode server on 127.0.0.1.
struct Server {
    dir: PathBuf,
    child: Child,
}

impl Server {
    fn start(root: &Path, name: &str) -> Server {
        let dir = root.join(name);
        private_dir(&dir.join("state"));
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        write_private(
            &dir.join("server.json"),
            &format!(
                r#"{{"tls": "private", "listen": "127.0.0.1", "port": {port}, "names": ["localhost", "127.0.0.1"], "db": "state/messages.sqlite3", "peers_file": "peers.json"}}"#
            ),
        );
        write_private(&dir.join("peers.json"), "{}\n");
        let child = Command::new(binary())
            .args([
                "server",
                "--config",
                dir.join("server.json").to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let began = Instant::now();
        while !dir.join("state/tls/private-cert.pem").exists()
            || std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
        {
            assert!(
                began.elapsed() < Duration::from_secs(15),
                "the server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Server { dir, child }
    }

    fn invite(&self, extra: &[&str]) -> String {
        let mut args = vec![
            "server",
            "invite",
            "--config",
            self.dir
                .join("server.json")
                .to_str()
                .unwrap()
                .to_string()
                .leak(),
            "--json",
        ];
        args.extend_from_slice(extra);
        let output = Command::new(binary()).args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["invite"].as_str().unwrap().to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A machine: a HOME and fake service managers and claude.
struct Machine {
    root: PathBuf,
}

impl Machine {
    fn new(temp: &Temp, name: &str) -> Machine {
        let root = temp.0.join(name);
        private_dir(&root.join("home/.claude/sessions"));
        private_dir(&root.join("bin"));
        let machine = Machine { root };
        machine.fake("launchctl", "exit 113");
        machine.fake("systemctl", "exit 3");
        let calls = machine.root.join("claude-calls");
        machine.fake(
            "claude",
            &format!(
                "echo \"$*\" >> '{}'\ncase \"$1 $2\" in\n  'plugin list') echo '[]' ;;\n  'plugin install') echo '{{}}' ;;\n  *) exit 1 ;;\nesac",
                calls.display()
            ),
        );
        machine
    }

    fn fake(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn config(&self) -> PathBuf {
        self.home().join(".config/switchboard/bridge.json")
    }

    fn run(&self, args: &[&str], vars: &[(&str, &str)], input: &str) -> Output {
        let mut command = Command::new(binary());
        command
            .arg("init")
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("USER", "tester")
            .env("NO_COLOR", "1")
            .env("SWITCHBOARD_NO_UPDATE_CHECK", "1")
            .env("COLUMNS", "100")
            .env("LANG", "en_US.UTF-8")
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in vars {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
        child.wait_with_output().unwrap()
    }

    /// The command line itself (not init), with the same environment.
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(binary())
            .args(args)
            .env_clear()
            .env("HOME", self.home())
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("USER", "tester")
            .env("NO_COLOR", "1")
            .env("SWITCHBOARD_NO_UPDATE_CHECK", "1")
            .env("COLUMNS", "100")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str], vars: &[(&str, &str)], input: &str) -> (i32, Value) {
        let mut all = args.to_vec();
        all.push("--json");
        let output = self.run(&all, vars, input);
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "not JSON: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(output.stderr, b"", "--json printed on stderr");
        (output.status.code().unwrap_or(-1), value)
    }

    fn plugin_calls(&self) -> String {
        std::fs::read_to_string(self.root.join("claude-calls")).unwrap_or_default()
    }

    /// Nothing of a setup is left: no config, key, or certificate.
    fn assert_clean(&self) {
        let root = self.home().join(".config/switchboard");
        for name in ["bridge.json", "identity", "server-cert.pem"] {
            assert!(!root.join(name).exists(), "{name} was left behind");
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn without_a_terminal_a_missing_answer_names_its_option() {
    let temp = Temp::new();
    let machine = Machine::new(&temp, "a");
    let output = machine.run(&[], &[], "");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output).contains("Nothing to set up: no invite and no server options."),
        "{}",
        text(&output)
    );
    let output = machine.run(&["--join"], &[], "");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output).contains("Missing --invite"),
        "{}",
        text(&output)
    );
    let (code, value) = machine.json(&["--join"], &[], "");
    assert_eq!(code, 2);
    assert_eq!(value["ok"], false);
    assert!(value["missing"].as_str().unwrap().starts_with("--invite"));
    // --invite reads standard input when it is not a terminal.
    let output = machine.run(&["--invite"], &[], "");
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("No invite on standard input."));
    let output = machine.run(&["--invite"], &[], "not-an-invite");
    assert!(
        text(&output).contains("That isn't a Switchboard invite."),
        "{}",
        text(&output)
    );
    // An invite on the command line is checked like a pasted one, before
    // anything is made.
    for args in [
        &["--invite", "sbi1.secret"][..],
        &["--invite=sbi1.secret"][..],
        &["--invite", "not-an-invite"][..],
    ] {
        let output = machine.run(args, &[], "");
        assert_ne!(output.status.code(), Some(0), "{}", text(&output));
        let said = text(&output);
        assert!(
            said.contains("damaged") || said.contains("isn't a Switchboard invite"),
            "{said}"
        );
        machine.assert_clean();
    }
    let output = machine.run(&["--bogus"], &[], "");
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output).contains(
        "✗ Unknown option --bogus.\n  To fix, see switchboard init --help for the options."
    ));
    let output = machine.run(&["--nmae", "x"], &[], "");
    assert!(
        text(&output).contains("To fix, use --name, or see switchboard init --help"),
        "{}",
        text(&output)
    );
    machine.assert_clean();
}

#[test]
fn joins_from_standard_input_and_reruns_show_the_state() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    let machine = Machine::new(&temp, "a");
    // Line breaks a chat app added are fine.
    let invite = server.invite(&[]);
    let pasted = format!("  {}\n{}\n", &invite[..30], &invite[30..]);
    let output = machine.run(
        &[
            "--invite",
            "-",
            "--name",
            "laptop",
            "--owner",
            "ada",
            "--no-start",
        ],
        &[],
        &pasted,
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let shown = text(&output);
    for line in [
        "✓ Server        localhost:",
        "certificate matches the invite",
        "✓ Machine name  laptop",
        "✓ Owner         ada",
        "✓ Joined the server as laptop",
        "Done. laptop joined localhost:",
        "switchboard up",
        "~/.local/bin isn't on your PATH",
    ] {
        assert!(shown.contains(line), "{line}\n{shown}");
    }
    assert!(!shown.contains(&invite), "the invite was printed");
    // Without a terminal the plugin is installed only when asked.
    assert_eq!(machine.plugin_calls(), "plugin list --json\n");
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(machine.config()).unwrap()).unwrap();
    assert!(config["server_url"]
        .as_str()
        .unwrap()
        .starts_with("https://localhost:"));
    assert!(config["ca"].as_str().unwrap().ends_with("server-cert.pem"));
    // A rerun shows where the machine stands and changes nothing.
    let (code, value) = machine.json(&[], &[], "");
    assert_eq!(code, 0);
    assert_eq!(value["role"], "configured");
    assert_eq!(value["machine"], "laptop");
    assert_eq!(value["enrolled"], true);
    assert_eq!(value["running"], false);
    // --plugin on a set-up machine adds the plugin.
    let (code, value) = machine.json(&["--plugin"], &[], "");
    assert_eq!(code, 0);
    assert_eq!(value["plugin"], "installed");
    assert!(machine
        .plugin_calls()
        .contains("plugin install switchboard --marketplace krish-k7/switchboard --json"));
}

/// The invite can be given on the command line, after a space or an equals
/// sign, and is cleaned as a pasted one is (line breaks, spaces, quotes).
#[test]
fn joins_with_the_invite_on_the_command_line() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    for (name, form) in [("first", "space"), ("second", "equals")] {
        let machine = Machine::new(&temp, name);
        let invite = server.invite(&[]);
        let pasted = format!("  '{}\n{}'  ", &invite[..30], &invite[30..]);
        let joined = format!("--invite={pasted}");
        let args: Vec<&str> = if form == "space" {
            vec![
                "--invite",
                &pasted,
                "--name",
                name,
                "--no-start",
                "--no-plugin",
            ]
        } else {
            vec![&joined, "--name", name, "--no-start", "--no-plugin"]
        };
        let (code, value) = machine.json(&args, &[], "");
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["machine"], name, "{value}");
        assert!(machine.config().exists());
        assert!(
            !value.to_string().contains(&invite),
            "the invite was printed"
        );
    }
}

/// The PATH offer is about ~/.local/bin, where the command is, and only
/// when it isn't on PATH already (whatever else is).
#[test]
fn the_path_offer_appears_only_when_local_bin_is_missing() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    let machine = Machine::new(&temp, "a");
    let local_bin = machine.home().join(".local/bin");
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        machine.root.join("bin").display(),
        local_bin.display()
    );
    let output = machine.run(
        &[
            "--invite",
            "-",
            "--name",
            "laptop",
            "--no-start",
            "--no-plugin",
        ],
        &[("PATH", &path)],
        &server.invite(&[]),
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(!text(&output).contains("on your PATH"), "{}", text(&output));
    // ~/bin on PATH doesn't count: the command isn't there.
    let other = Machine::new(&temp, "b");
    let path = format!(
        "{}:{}:/usr/bin:/bin",
        other.root.join("bin").display(),
        other.home().join("bin").display()
    );
    let output = other.run(
        &[
            "--invite",
            "-",
            "--name",
            "desk",
            "--no-start",
            "--no-plugin",
        ],
        &[("PATH", &path)],
        &server.invite(&[]),
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(
        text(&output).contains("~/.local/bin isn't on your PATH"),
        "{}",
        text(&output)
    );
    assert!(
        text(&output).contains("export PATH=\"$HOME/.local/bin:$PATH\""),
        "{}",
        text(&output)
    );
}

#[test]
fn a_home_reached_through_a_link_works() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    let machine = Machine::new(&temp, "a");
    let link = temp.0.join("linked-home");
    std::os::unix::fs::symlink(machine.home(), &link).unwrap();
    let home = link.to_str().unwrap();
    let (code, value) = machine.json(
        &["--invite", "--name", "linked", "--no-start", "--no-plugin"],
        &[("HOME", home)],
        &server.invite(&[]),
    );
    assert_eq!(code, 0, "{value}");
    assert!(machine.config().exists());
    let (code, value) = machine.json(&[], &[("HOME", home)], "");
    assert_eq!(
        (code, &value["role"], &value["enrolled"]),
        (0, &Value::from("configured"), &Value::Bool(true))
    );
}

#[test]
fn refusals_before_enrolling_leave_nothing_behind() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    // A used invite.
    let invite = server.invite(&[]);
    let first = Machine::new(&temp, "first");
    let (code, value) = first.json(
        &["--invite", "--name", "first", "--no-start", "--no-plugin"],
        &[],
        &invite,
    );
    assert_eq!((code, &value["machine"]), (0, &Value::from("first")));
    let second = Machine::new(&temp, "second");
    let output = second.run(
        &["--invite", "--name", "second", "--no-start"],
        &[],
        &invite,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output).contains("✗ This invite was already used."),
        "{}",
        text(&output)
    );
    assert!(text(&output).contains("Nothing was left on this machine."));
    second.assert_clean();
    // A taken name.
    let output = second.run(
        &["--invite", "--name", "first", "--no-start"],
        &[],
        &server.invite(&[]),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output).contains("A machine named first is already on this server."));
    second.assert_clean();
    // The same machine name is free for the next try, with the same invite
    // still unused (a taken name does not use it up).
    let fresh = server.invite(&[]);
    let output = second.run(&["--invite", "--name", "first", "--no-start"], &[], &fresh);
    assert_eq!(output.status.code(), Some(1));
    let (code, _) = second.json(
        &["--invite", "--name", "second", "--no-start", "--no-plugin"],
        &[],
        &fresh,
    );
    assert_eq!(code, 0);
}

#[test]
fn the_server_is_checked_before_any_key_exists() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    let machine = Machine::new(&temp, "a");
    let invite = switchboard::invite::Invite::parse(&server.invite(&[])).unwrap();
    // A pin that is not the server's certificate.
    let forged = switchboard::invite::Invite {
        pin: Some("ab".repeat(32)),
        ..invite.clone()
    }
    .encode();
    let output = machine.run(&["--invite", "--no-start"], &[], &forged);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output).contains("The server sent a different certificate than the invite names."),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("Machine name"),
        "asked for a name before checking the server"
    );
    machine.assert_clean();
    // A server that does not answer.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead = switchboard::invite::Invite {
        url: format!("https://127.0.0.1:{port}"),
        ..invite
    }
    .encode();
    let output = machine.run(
        &["--invite", "--no-start"],
        &[("SWITCHBOARD_INVITE", "")],
        &dead,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output).contains(&format!("Can't reach the server at 127.0.0.1:{port}.")),
        "{}",
        text(&output)
    );
    assert!(text(&output).contains("Connection refused."));
    machine.assert_clean();
}

#[test]
fn a_removed_machine_joins_again_with_its_key() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    let machine = Machine::new(&temp, "a");
    let invite = server.invite(&[]);
    let (code, _) = machine.json(
        &["--invite", "--name", "box", "--no-start", "--no-plugin"],
        &[],
        &invite,
    );
    assert_eq!(code, 0);
    let key = std::fs::read(machine.home().join(".config/switchboard/identity/key.pem")).unwrap();
    // The server forgets the machine (revoked); a rerun joins again with a
    // new invite and the same key.
    let revoke = Command::new(binary())
        .args([
            "server",
            "revoke",
            "box",
            "--config",
            server.dir.join("server.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        revoke.status.success(),
        "{}",
        String::from_utf8_lossy(&revoke.stderr)
    );
    let (code, value) = machine.json(&[], &[], "");
    assert_eq!((code, &value["enrolled"]), (0, &Value::Bool(false)));
    let (code, value) = machine.json(
        &["--invite", "--no-start", "--no-plugin"],
        &[],
        &server.invite(&[]),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["machine"], "box");
    assert_eq!(
        std::fs::read(machine.home().join(".config/switchboard/identity/key.pem")).unwrap(),
        key
    );
    let (_, value) = machine.json(&[], &[], "");
    assert_eq!(value["enrolled"], true);
}

#[test]
fn joining_another_server_needs_replace_and_moves_the_old_setup_aside() {
    let temp = Temp::new();
    let one = Server::start(&temp.0, "one");
    let two = Server::start(&temp.0, "two");
    let machine = Machine::new(&temp, "a");
    let (code, _) = machine.json(
        &["--invite", "--name", "box", "--no-start", "--no-plugin"],
        &[],
        &one.invite(&[]),
    );
    assert_eq!(code, 0);
    let other = two.invite(&[]);
    let output = machine.run(&["--invite", "--no-start", "--no-plugin"], &[], &other);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output).contains("pass --replace"),
        "{}",
        text(&output)
    );
    // A refused invite for the new server puts the old setup back.
    let (code, _) = machine.json(
        &["--invite", "--replace", "--no-start", "--no-plugin"],
        &[],
        &{
            let mut used = switchboard::invite::Invite::parse(&other).unwrap();
            used.secret = switchboard::invite::new_secret();
            used.encode()
        },
    );
    assert_eq!(code, 1);
    let config = std::fs::read_to_string(machine.config()).unwrap();
    assert!(
        config.contains(&one_url(&one)),
        "the old setup was not put back: {config}"
    );
    // With a good invite the old setup moves aside and the new one joins.
    let (code, value) = machine.json(
        &["--invite", "--replace", "--no-start", "--no-plugin"],
        &[],
        &other,
    );
    assert_eq!(code, 0, "{value}");
    let config = std::fs::read_to_string(machine.config()).unwrap();
    assert!(config.contains(&one_url(&two)));
    let backups: Vec<_> = std::fs::read_dir(machine.home().join(".config"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("switchboard.before-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert!(
        std::fs::read_to_string(backups[0].path().join("bridge.json"))
            .unwrap()
            .contains(&one_url(&one))
    );
}

fn one_url(server: &Server) -> String {
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(server.dir.join("server.json")).unwrap())
            .unwrap();
    format!("https://localhost:{}", config["port"])
}

/// Stops the stand-in server a test started, however the test ends.
struct StubGuard(PathBuf);

impl Drop for StubGuard {
    fn drop(&mut self) {
        if let Ok(text) = std::fs::read_to_string(self.0.join("server.pid")) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                // SAFETY: signals the stand-in server this test started.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
    }
}

#[test]
fn the_server_path_runs_through_the_stand_in_and_this_machine_can_join_it() {
    let temp = Temp::new();
    let machine = Machine::new(&temp, "a");
    let stub = temp.0.join("stub");
    private_dir(&stub);
    let _guard = StubGuard(stub.clone());
    let vars = [("SWITCHBOARD_INIT_STUB_SERVER", stub.to_str().unwrap())];
    let (code, value) = machine.json(&["--server", "--tls", "private"], &vars, "");
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["role"], "server");
    let first = value["invite"].as_str().unwrap().to_string();
    assert!(value["server_url"]
        .as_str()
        .unwrap()
        .starts_with("https://localhost:"));
    assert_eq!(value["server_pin"].as_str().unwrap().len(), 64);
    // Both, from the options: the server is already running (the rerun
    // reuses it) and its new first invite joins this machine.
    let other = Machine::new(&temp, "b");
    let (code, value) = other.json(
        &[
            "--server",
            "--join",
            "--tls",
            "private",
            "--name",
            "both",
            "--no-start",
            "--no-plugin",
        ],
        &vars,
        "",
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        (value["role"].as_str(), value["machine"].as_str()),
        (Some("both"), Some("both"))
    );
    // Each run's first invite replaces the one an earlier run printed.
    let third = Machine::new(&temp, "c");
    let (code, value) = third.json(
        &["--invite", "--name", "third", "--no-start", "--no-plugin"],
        &[],
        &first,
    );
    assert_eq!(code, 1, "{value}");
    assert!(value["error"]
        .as_str()
        .unwrap()
        .contains("didn't accept this invite"));
    third.assert_clean();
}

#[test]
fn public_mode_checks_the_domain_before_installing() {
    let temp = Temp::new();
    let machine = Machine::new(&temp, "a");
    let stub = temp.0.join("stub");
    private_dir(&stub);
    let vars = [("SWITCHBOARD_INIT_STUB_SERVER", stub.to_str().unwrap())];
    let output = machine.run(
        &["--server", "--domain", "no-such-name.invalid", "--yes"],
        &vars,
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        text(&output).contains("no-such-name.invalid doesn't resolve to an address."),
        "{}",
        text(&output)
    );
    assert!(
        !stub.join("server.json").exists(),
        "installed before the domain check"
    );
    let output = machine.run(
        &["--server", "--domain", "https://x.example.com"],
        &vars,
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    // The address is cut down to the name.
    assert!(
        text(&output).contains("✓ Domain     x.example.com\n"),
        "{}",
        text(&output)
    );
}

/// The older `setup` command leaves a machine that is down as it is, as
/// installs and init do: it confirms the enrollment, but neither starts the
/// bridge nor lets its service start at login again.
#[test]
fn setup_leaves_a_machine_that_is_down_down() {
    let temp = Temp::new();
    let server = Server::start(&temp.0, "server");
    let machine = Machine::new(&temp, "a");
    let invite = server.invite(&[]);
    let output = machine.run(
        &[
            "--invite",
            "-",
            "--name",
            "laptop",
            "--owner",
            "ada",
            "--no-start",
        ],
        &[],
        &invite,
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let marker = machine.config().parent().unwrap().join("down");
    std::fs::write(&marker, "down\n").unwrap();
    let calls = machine.root.join("service-calls");
    for tool in ["launchctl", "systemctl"] {
        machine.fake(
            tool,
            &format!("echo \"{tool} $*\" >> '{}'\nexit 0", calls.display()),
        );
    }
    let output = machine.cli(&["setup", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["enrolled"], true);
    assert_eq!(value["started"], false);
    assert_eq!(value["down"], true);
    assert!(marker.exists(), "setup brought the machine back up");
    let called = std::fs::read_to_string(&calls).unwrap_or_default();
    for verb in ["enable", "bootstrap", "kickstart", " start "] {
        assert!(!called.contains(verb), "{verb}: {called}");
    }
    // In words, it says so and how to bring it back.
    let output = machine.cli(&["setup"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let said = text(&output);
    assert!(said.contains("This machine is down"), "{said}");
    assert!(said.contains("switchboard up"), "{said}");
    assert!(marker.exists());
}
