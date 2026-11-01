//! `switchboard init` on a real pseudo-terminal: however the program ends or
//! pauses (a panic, SIGQUIT, SIGTSTP and SIGCONT), the terminal is put back
//! (echo, line editing, cursor, paste mode), and a paste too large to take
//! is dropped whole instead of spilling into the keys after it. Also the
//! choices only a terminal offers: leaving a server removes the bridge's
//! background service.
//!
//! Each test runs init in a private HOME with fake launchctl, systemctl,
//! sudo, and claude first on PATH, on a pseudo-terminal of its own: a small
//! session leader (forked before exec) stands in for a job-control shell,
//! so init runs in a foreground process group of its own and can really be
//! stopped and continued. The panic test needs the `fault-injection`
//! feature (a key that panics):
//!
//! ```sh
//! cargo test --release --features fault-injection --test terminal
//! ```

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let template = std::ffi::CString::new("/tmp/sbterm-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
        raw.pop();
        Temp(std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private HOME, and fake launchctl, systemctl, sudo, and claude that
/// record their arguments in `calls`.
struct Sandbox {
    temp: Temp,
    home: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let temp = Temp::new();
        let home = temp.0.join("home");
        let bin = temp.0.join("bin");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let sandbox = Sandbox { temp, home, bin };
        for (name, status) in [
            ("launchctl", 113),
            ("systemctl", 3),
            ("sudo", 1),
            ("claude", 1),
        ] {
            sandbox.fake(name, status);
        }
        sandbox
    }

    fn fake(&self, name: &str, status: i32) {
        let path = self.bin.join(name);
        let calls = self.temp.0.join("calls");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"{name} $*\" >> '{}'\nexit {status}\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.temp.0.join("calls")).unwrap_or_default()
    }

    /// The binary in this sandbox's environment, without a terminal.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_switchboard"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("USER", "tester")
            .env("LANG", "en_US.UTF-8")
            .current_dir(&self.temp.0)
            .stdin(Stdio::null());
        command
    }
}

/// init on a pseudo-terminal.
struct Session {
    sandbox: Sandbox,
    master: OwnedFd,
    /// Kept open so the terminal outlives init and its settings stay
    /// readable after it exits.
    _slave: OwnedFd,
    /// The stand-in shell (the session leader).
    leader: Child,
    /// init itself.
    pid: libc::pid_t,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Session {
    fn start(args: &[&str], vars: &[(&str, &str)]) -> Session {
        Session::start_in(Sandbox::new(), args, vars)
    }

    fn start_in(sandbox: Sandbox, args: &[&str], vars: &[(&str, &str)]) -> Session {
        let (master, slave) = open_pty();
        let mut pid_pipe = [0 as RawFd; 2];
        // SAFETY: pipe fills two descriptors.
        assert_eq!(unsafe { libc::pipe(pid_pipe.as_mut_ptr()) }, 0);
        let (pid_read, pid_write) = (pid_pipe[0], pid_pipe[1]);
        let stdio = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        let mut command = sandbox.command(&["init"]);
        command
            .args(args)
            .env("TERM", "xterm")
            .env("NO_COLOR", "1")
            .stdin(stdio(&slave))
            .stdout(stdio(&slave))
            .stderr(stdio(&slave));
        for (key, value) in vars {
            command.env(key, value);
        }
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let child = libc::fork();
                if child < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Both sides make init's own group the foreground one, as a
                // shell does (SIGTTOU ignored meanwhile).
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                if child == 0 {
                    libc::setpgid(0, 0);
                    libc::tcsetpgrp(0, libc::getpid());
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                    libc::close(pid_read);
                    libc::close(pid_write);
                    return Ok(());
                }
                libc::setpgid(child, child);
                libc::tcsetpgrp(0, child);
                libc::write(
                    pid_write,
                    &child as *const libc::pid_t as *const libc::c_void,
                    std::mem::size_of::<libc::pid_t>(),
                );
                // The leader never execs: close what it inherited (spawn's
                // error pipe among them) so the parent isn't kept waiting.
                for fd in 3..1024 {
                    libc::close(fd);
                }
                loop {
                    let mut status = 0;
                    let reaped = libc::waitpid(child, &mut status, 0);
                    if reaped == child {
                        if libc::WIFEXITED(status) {
                            libc::_exit(libc::WEXITSTATUS(status));
                        }
                        if libc::WIFSIGNALED(status) {
                            libc::_exit(128 + libc::WTERMSIG(status));
                        }
                    } else if reaped < 0 && *errno() != libc::EINTR {
                        libc::_exit(127);
                    }
                }
            });
        }
        let leader = command.spawn().unwrap();
        // SAFETY: closing our copy of the write end; reading the read end.
        unsafe { libc::close(pid_write) };
        let mut pid_file = unsafe { std::fs::File::from_raw_fd(pid_read) };
        let mut bytes = [0u8; std::mem::size_of::<libc::pid_t>()];
        pid_file.read_exact(&mut bytes).unwrap();
        let pid = libc::pid_t::from_ne_bytes(bytes);
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buffer[..count]);
            }
        });
        Session {
            sandbox,
            master,
            _slave: slave,
            leader,
            pid,
            output,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// The terminal's settings, read through the master side (another
    /// session's controlling terminal refuses them on macOS).
    fn settings(&self) -> libc::termios {
        // SAFETY: tcgetattr fills a zeroed termios.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            let code = libc::tcgetattr(self.master.as_raw_fd(), &mut settings);
            assert_eq!(code, 0, "{}", std::io::Error::last_os_error());
            settings
        }
    }

    fn echo(&self) -> bool {
        let settings = self.settings();
        settings.c_lflag & libc::ECHO != 0 && settings.c_lflag & libc::ICANON != 0
    }

    /// Wait until `ready` holds, or fail with what init printed.
    fn wait_for(&self, what: &str, ready: impl Fn(&Session) -> bool) {
        let began = Instant::now();
        while !ready(self) {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "timed out waiting for {what}; init printed:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the invite prompt reads keys in raw mode.
    fn wait_for_prompt(&self) {
        self.wait_for("the invite prompt", |s| {
            !s.echo() && s.text().contains("Paste the invite")
        });
        // Let it reach the read.
        std::thread::sleep(Duration::from_millis(200));
    }

    fn type_bytes(&self, bytes: &[u8]) {
        let mut written = 0;
        while written < bytes.len() {
            // SAFETY: writes from a live buffer.
            let count = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                )
            };
            if count < 0 {
                let error = std::io::Error::last_os_error();
                assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
                continue;
            }
            written += count as usize;
        }
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: kill on init's pid.
        assert_eq!(unsafe { libc::kill(self.pid, signal) }, 0);
    }

    fn stopped(&self) -> bool {
        Command::new("ps")
            .args(["-o", "stat=", "-p", &self.pid.to_string()])
            .output()
            .is_ok_and(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim_start()
                    .starts_with('T')
            })
    }

    /// init's exit status (as the stand-in shell reports it: 128 + signal).
    fn wait(&mut self) -> i32 {
        let began = Instant::now();
        loop {
            if let Some(status) = self.leader.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "init did not exit; it printed:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: kill on init's pid and the leader's.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::kill(self.pid, libc::SIGCONT);
        }
        let _ = self.leader.kill();
        let _ = self.leader.wait();
    }
}

#[cfg(target_os = "macos")]
fn errno() -> *mut libc::c_int {
    // SAFETY: the thread's errno location.
    unsafe { libc::__error() }
}

#[cfg(not(target_os = "macos"))]
fn errno() -> *mut libc::c_int {
    // SAFETY: the thread's errno location.
    unsafe { libc::__errno_location() }
}

/// A pseudo-terminal pair, 80 columns by 24 rows.
fn open_pty() -> (OwnedFd, OwnedFd) {
    // SAFETY: posix_openpt, grantpt, unlockpt, and ptsname on a fresh master;
    // ptsname's buffer is copied at once (tests open one at a time under the
    // lock).
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(master >= 0);
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_owned();
        let slave = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        assert!(slave >= 0);
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &size);
        (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
    }
}

#[test]
fn suspending_restores_the_terminal_and_continuing_takes_it_back() {
    let mut session = Session::start(&["--invite"], &[]);
    session.wait_for_prompt();
    let paste_on = |s: &Session| s.text().matches("\x1b[?2004h").count();
    let before = paste_on(&session);
    let shown = session.text().len();
    session.signal(libc::SIGTSTP);
    session.wait_for("init to stop", Session::stopped);
    assert!(session.echo(), "the terminal was left raw while stopped");
    assert!(
        session.text()[shown..].contains("\x1b[?2004l\x1b[?25h"),
        "paste mode and the cursor were not put back"
    );
    session.signal(libc::SIGCONT);
    session.wait_for("raw input again", |s| !s.echo());
    session.wait_for("paste mode again", |s| paste_on(s) > before);
    // The prompt still reads keys: Ctrl-C cancels it.
    session.type_bytes(b"\x03");
    assert_eq!(session.wait(), 130, "{}", session.text());
    assert!(session.echo());
}

#[test]
fn quit_restores_the_terminal() {
    let mut session = Session::start(&["--invite"], &[]);
    session.wait_for_prompt();
    session.signal(libc::SIGQUIT);
    assert_eq!(session.wait(), 128 + libc::SIGQUIT, "{}", session.text());
    assert!(session.echo(), "SIGQUIT left the terminal raw");
    let text = session.text();
    let restored = text
        .rfind("\x1b[?2004l\x1b[?25h")
        .expect("paste mode and cursor put back");
    assert!(restored > text.rfind("Paste the invite").unwrap());
}

#[test]
fn a_paste_over_the_limit_is_dropped_whole() {
    let mut session = Session::start(&["--invite"], &[]);
    session.wait_for_prompt();
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend(std::iter::repeat_n(b'x', 70 * 1024));
    paste.extend_from_slice(b"\x1b[201~ab");
    session.type_bytes(&paste);
    session.wait_for("the too-large notice", |s| {
        s.text().contains("too large (over 64 KiB)")
    });
    // Only the two keys typed after the paste reached the prompt.
    session.wait_for("the two typed characters", |s| {
        s.text().contains("hidden, 2 characters")
    });
    assert!(!session.text().contains("hidden, 3 characters"));
    session.type_bytes(b"\x03");
    assert_eq!(session.wait(), 130, "{}", session.text());
    assert!(session.echo());
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_panic_restores_the_terminal_before_its_message() {
    let mut session = Session::start(&["--invite"], &[("SWITCHBOARD_FAULT_PANIC_ON_KEY", "1")]);
    session.wait_for_prompt();
    session.type_bytes(b"a");
    assert_eq!(session.wait(), 128 + libc::SIGABRT, "{}", session.text());
    assert!(session.echo(), "the panic left the terminal raw");
    let text = session.text();
    let panicked = text.find("panicked at").expect("the panic message");
    let restored = text[..panicked]
        .rfind("\x1b[?2004l\x1b[?25h")
        .expect("the terminal was put back before the message");
    assert!(restored > text.find("Paste the invite").unwrap());
}

#[test]
fn leaving_removes_the_background_service() {
    leave(false);
}

/// Leaving from a machine that was down also undoes `down`'s hold on the
/// service (the setup with the `down` file is moved aside), so the next
/// setup doesn't find launchd keeping it disabled.
#[test]
fn leaving_a_machine_that_is_down_lets_the_service_start_again() {
    leave(true);
}

fn leave(down: bool) {
    let sandbox = Sandbox::new();
    // launchctl allows enabling; everything else reports nothing loaded.
    std::fs::write(
        sandbox.bin.join("launchctl"),
        format!(
            "#!/bin/sh\necho \"launchctl $*\" >> '{}'\n[ \"$1\" = enable ] && exit 0\nexit 113\n",
            sandbox.temp.0.join("calls").display()
        ),
    )
    .unwrap();
    let installed = sandbox.command(&["install", "--json"]).output().unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let home = sandbox.home.clone();
    let (service, wants) = if cfg!(target_os = "macos") {
        (
            home.join("Library/LaunchAgents/dev.switchboard.bridge.plist"),
            None,
        )
    } else {
        let units = home.join(".config/systemd/user");
        (
            units.join("switchboard-bridge.service"),
            Some(units.join("default.target.wants/switchboard-bridge.service")),
        )
    };
    assert!(service.exists());
    // A setup whose server can't be reached (so init offers to leave it).
    let setup = home.join(".config/switchboard");
    let private = |path: &std::path::Path, data: &str| {
        std::fs::write(path, data).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    std::fs::create_dir_all(setup.join("identity")).unwrap();
    std::fs::set_permissions(
        setup.join("identity"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    private(
        &setup.join("identity/identity.json"),
        r#"{"machine": "box", "owner": "tester"}"#,
    );
    let config = setup.join("bridge.json");
    private(
        &config,
        &format!(
            r#"{{"enabled": true, "server_url": "https://127.0.0.1:9", "cert": "{}"}}"#,
            setup.join("identity/cert.pem").display()
        ),
    );
    if down {
        private(&setup.join("down"), "down\n");
    }
    let mut session = Session::start_in(sandbox, &[], &[]);
    session.wait_for("the actions", |s| {
        !s.echo() && s.text().contains("What would you like to do?")
    });
    std::thread::sleep(Duration::from_millis(200));
    // "Leave this server" is just above the last action, "Keep it as it
    // is", which is the default for a machine that is down (otherwise the
    // first action is, and Up wraps around to the last).
    for _ in 0..if down { 1 } else { 2 } {
        session.type_bytes(b"\x1b[A");
        std::thread::sleep(Duration::from_millis(100));
    }
    session.type_bytes(b"\r");
    session.wait_for("the confirmation", |s| s.text().contains("Leave it"));
    std::thread::sleep(Duration::from_millis(200));
    session.type_bytes(b"1");
    std::thread::sleep(Duration::from_millis(100));
    session.type_bytes(b"\r");
    assert_eq!(session.wait(), 0, "{}", session.text());
    let text = session.text();
    assert!(
        text.contains("Removed the bridge's background service"),
        "{text}"
    );
    assert!(!service.exists(), "the service file was left installed");
    if let Some(wants) = wants {
        assert!(std::fs::symlink_metadata(wants).is_err());
        assert!(session
            .sandbox
            .calls()
            .contains("systemctl --user daemon-reload"));
    }
    assert!(!setup.exists());
    let enabled = session.sandbox.calls().contains("launchctl enable gui/");
    if cfg!(target_os = "macos") {
        assert_eq!(enabled, down, "{}", session.sandbox.calls());
    }
    assert!(!text.contains("still kept from starting"), "{text}");
    let aside: Vec<_> = std::fs::read_dir(home.join(".config"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("switchboard.left-")
        })
        .collect();
    assert_eq!(aside.len(), 1, "the setup was not moved aside");
    // What the service would have run says why it stops, and exits.
    let bridge = session
        .sandbox
        .command(&["bridge", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(bridge.status.code(), Some(1));
    let said = String::from_utf8_lossy(&bridge.stderr);
    assert!(said.contains("there is no configuration at"), "{said}");
    assert!(said.contains("switchboard init"), "{said}");
}
