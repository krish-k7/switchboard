//! `up` and `down` (and the older `start` and `stop`): the user service that
//! runs the bridge, a launchd job on macOS and a systemd user unit on Linux,
//! with the same settle rules.
//!
//! `down` takes this machine off Switchboard until `up`: it disables the
//! service (`launchctl disable`, `systemctl --user disable`), so it doesn't
//! start again at login or after a restart, stops it, and writes the file
//! `down` beside the configuration, which says the machine is down on
//! purpose. `up` removes that file, enables the service, and starts it.
//! Installs, upgrades, and `init` leave a machine that is down as it is.
//! Earlier versions installed the service under other names (`LEGACY_LABEL`,
//! `LEGACY_UNIT`); `switchboard install` moves it (see `install.rs`).

use std::time::Duration;

use super::local::{home_dir, private_json, runtime_status};
use super::machines::run_capture;
use crate::error::{Error, Result};
use crate::identity::monotonic;
use crate::wirejson::value_eq;

/// The bridge's launchd job label (macOS).
pub const LABEL: &str = "dev.switchboard.bridge";
/// The bridge's systemd user unit (Linux).
pub const UNIT: &str = "switchboard-bridge.service";
/// The names early releases installed the bridge service under (the launchd
/// label and the systemd unit), moved to `LABEL` and `UNIT` on install.
pub const LEGACY_LABEL: &str = "com.vorelios.switchboard";
pub const LEGACY_UNIT: &str = "switchboard.service";

/// The file beside the configuration that says this machine was taken
/// down with `switchboard down`.
pub const DOWN_FILE: &str = "down";

const DOWN_TEXT: &str = "This machine was taken off Switchboard with `switchboard down`.\n`switchboard up` brings it back and removes this file.\n";

/// The `down` file for the configuration at `config_path`.
pub fn down_marker(config_path: &str) -> String {
    crate::fsutil::join(&crate::fsutil::parent(config_path), DOWN_FILE)
}

/// Whether this machine was taken down with `switchboard down` (and not
/// brought back up since).
pub fn is_down(config_path: &str) -> bool {
    crate::fsutil::lstat(&down_marker(config_path)).is_ok()
}

/// Whether the default configuration's machine is down: what installs and
/// upgrades, which run the service for that configuration, respect.
pub fn default_is_down() -> bool {
    is_down(&super::install::paths(&home_dir()).config)
}

fn write_marker(config_path: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = down_marker(config_path);
    if crate::fsutil::lstat(&path).is_ok() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&path))))?;
    file.write_all(DOWN_TEXT.as_bytes())
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&path))))
}

fn remove_marker(config_path: &str) -> Result<()> {
    let path = down_marker(config_path);
    match crate::fsutil::lstat(&path) {
        Ok(_) => crate::fsutil::unlink(&path),
        Err(_) => Ok(()),
    }
}

/// `switchboard down`: keep the service from starting at login, record that
/// this machine is down, and stop the bridge. The configuration, key, and
/// joined chats stay. Returns whether the bridge was running.
///
/// The `down` file is written only once the service is disabled, so a
/// refusal leaves the machine as it was rather than marked down with a
/// service that still starts at login.
pub fn take_down(config_path: &str) -> Result<bool> {
    if !supported() {
        return Err(Error::value(
            "taking this machine down needs launchd (macOS) or systemd (Linux)",
        ));
    }
    let manager = Manager::new("stop", 30.0);
    let identity = private_json(config_path)
        .and_then(|config| runtime_status(&config, config_path))
        .ok();
    let was_running = manager.present().unwrap_or(false) || identity.is_some();
    if cfg!(target_os = "macos") {
        let (code, stderr) = manager.launchctl(&["disable", &manager.target()])?;
        if code != 0 {
            return Err(Error::value(format!(
                "the bridge's service couldn't be kept from starting at login ({}), so this machine wasn't taken down",
                failure_text(&stderr, "launchctl disable failed")
            )));
        }
    } else {
        let (code, stderr) = manager.systemctl(&["disable", "--now", manager.unit])?;
        if code != 0 && installed() {
            return Err(Error::value(format!(
                "the bridge's service couldn't be kept from starting at login ({}), so this machine wasn't taken down",
                failure_text(&stderr, "systemctl disable failed")
            )));
        }
        // The installer enabled the unit with this link; disabling removes
        // it, and so does this, should the service manager not have.
        let p = super::install::paths(&home_dir());
        if let Some(wants) = &p.wants {
            if super::install::links_to(wants, &p.service) {
                crate::fsutil::unlink(wants)?;
            }
        }
    }
    if let Err(error) = write_marker(config_path) {
        // Not marked down: let the service start at login again, so the
        // machine isn't left disabled without the file that says why.
        let undone = enable(&manager).is_ok();
        return Err(Error::value(format!(
            "this machine couldn't be marked down ({}), so it wasn't taken down{}",
            one_clause(&error.message),
            if undone {
                ""
            } else {
                "; run switchboard up to let the bridge start at login again"
            }
        )));
    }
    manager.stop_and_wait(identity.as_ref())?;
    Ok(was_running)
}

/// `switchboard up`: let the service start at login again, forget that this
/// machine was down, and start the bridge.
pub fn bring_up(config_path: &str) -> Result<()> {
    if !supported() {
        return Err(Error::value(
            "starting the bridge needs launchd (macOS) or systemd (Linux)",
        ));
    }
    allow_up(config_path)?;
    service("start", Some(config_path))
}

/// Undo `down` without starting anything: let the service start at login
/// again, then remove the `down` file. If the service can't be enabled, the
/// file stays, so the machine is still recorded as down. On a machine that
/// isn't down, enabling is only a precaution.
pub fn allow_up(config_path: &str) -> Result<()> {
    let manager = Manager::new("start", 30.0);
    if !is_down(config_path) {
        let _ = enable(&manager);
        return Ok(());
    }
    enable(&manager).map_err(|error| {
        Error::value(format!(
            "the bridge's service couldn't be allowed to start again ({}), so this machine is still down; {}",
            one_clause(&error.message),
            enable_fix()
        ))
    })?;
    remove_marker(config_path)
}

/// Let the service start at login: `launchctl enable` (harmless when it was
/// never disabled), or the unit's enabling link.
fn enable(manager: &Manager) -> Result<()> {
    if cfg!(target_os = "macos") {
        let (code, stderr) = manager.launchctl(&["enable", &manager.target()])?;
        if code != 0 {
            return Err(Error::value(failure_text(
                &stderr,
                "launchctl enable failed",
            )));
        }
        return Ok(());
    }
    let p = super::install::paths(&home_dir());
    if !crate::fsutil::exists(&p.service) {
        // No unit to enable; installing it later enables it.
        return Ok(());
    }
    let refusal = match manager.systemctl(&["enable", manager.unit]) {
        Ok((0, _)) => None,
        Ok((_, stderr)) => Some(failure_text(&stderr, "systemctl enable failed")),
        Err(error) => Some(error.message),
    };
    // The link systemctl makes, made here should it not have.
    if let Some(wants) = &p.wants {
        if crate::fsutil::lstat(wants).is_err() {
            crate::fsutil::mkdir_parents(&crate::fsutil::parent(wants), 0o700)
                .and_then(|()| {
                    std::os::unix::fs::symlink(&p.service, wants)
                        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(wants))))
                })
                .map_err(|error| match &refusal {
                    Some(refusal) => Error::value(format!("{refusal}, and {}", error.message)),
                    None => error,
                })?;
        }
    }
    Ok(())
}

/// A reason set inside a message whose fix follows its first "; ": any
/// "; " of its own would be read as that boundary.
fn one_clause(reason: &str) -> String {
    reason.replace("; ", ", ")
}

/// How to enable the service by hand.
fn enable_fix() -> String {
    if cfg!(target_os = "macos") {
        format!(
            "run launchctl enable gui/{}/{LABEL}, then switchboard up",
            uid()
        )
    } else {
        format!("run systemctl --user enable {UNIT}, then switchboard up")
    }
}

/// Let the bridge's service start at login again after the setup that was
/// down is gone (`uninstall --purge`, or leaving the server), so a later
/// setup doesn't find it disabled. On Linux a unit that is still installed
/// gets its enabling link back; a removed one needs nothing.
pub fn forget_down() -> Result<()> {
    if !supported() {
        return Ok(());
    }
    enable(&Manager::new("start", 10.0)).map_err(|error| {
        Error::value(format!(
            "the bridge's service is still kept from starting at login ({}); {}",
            one_clause(&error.message),
            enable_fix().replace(", then switchboard up", "")
        ))
    })
}

/// Whether launchd keeps the bridge's job from loading (`launchctl
/// disable`). False when it can't be told, and always on Linux, where a
/// disabled unit still starts.
fn launchd_disabled(manager: &Manager) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let Ok((code, stdout)) =
        manager.launchctl_output(&["print-disabled", &format!("gui/{}", uid())])
    else {
        return false;
    };
    let quoted = format!("\"{}\"", manager.label);
    code == 0
        && stdout.lines().any(|line| {
            let line = line.trim();
            line.starts_with(&quoted)
                && matches!(line[quoted.len()..].trim(), "=> disabled" | "=> true")
        })
}

/// Why the service can't start when launchd keeps it disabled: the machine
/// is down, or something disabled it without `switchboard down`.
fn disabled_start_error(config_path: Option<&str>) -> String {
    let down = config_path.map_or_else(default_is_down, is_down);
    if down {
        "this machine is down (taken offline with switchboard down), so the bridge can't start; run switchboard up to bring it back online".into()
    } else {
        "the bridge's service is turned off in launchd (launchctl disable), though this machine wasn't taken down with switchboard down; run switchboard up to turn it back on and start the bridge".into()
    }
}

/// Whether this platform has service management.
pub fn supported() -> bool {
    cfg!(target_os = "macos") || cfg!(target_os = "linux")
}

fn uid() -> u32 {
    crate::fsutil::getuid()
}

/// One bounded command against the shared 30-second deadline.
fn run(program: &str, arguments: &[&str], deadline: f64, action: &str) -> Result<(i32, String)> {
    let remaining = deadline - monotonic();
    if remaining <= 0.0 {
        return Err(Error::os(settle_failure(action)));
    }
    let mut argv = vec![program.to_string()];
    argv.extend(arguments.iter().map(|a| a.to_string()));
    let (code, _, stderr) = run_capture(&argv, remaining, true)?;
    Ok((code, stderr))
}

fn settle_failure(action: &str) -> String {
    match action {
        "start" => "the bridge did not start within 30 seconds".into(),
        "stop" => "the bridge did not stop within 30 seconds".into(),
        other => format!("the service manager did not answer within 30 seconds ({other})"),
    }
}

struct Manager {
    action: String,
    deadline: f64,
    label: &'static str,
    unit: &'static str,
}

impl Manager {
    fn new(action: &str, seconds: f64) -> Self {
        Manager {
            action: action.to_string(),
            deadline: monotonic() + seconds,
            label: LABEL,
            unit: UNIT,
        }
    }

    /// The service as earlier versions installed it.
    fn legacy(action: &str, seconds: f64) -> Self {
        Manager {
            label: LEGACY_LABEL,
            unit: LEGACY_UNIT,
            ..Manager::new(action, seconds)
        }
    }

    fn launchctl(&self, arguments: &[&str]) -> Result<(i32, String)> {
        run("launchctl", arguments, self.deadline, &self.action)
    }

    /// `launchctl` with its standard output rather than its errors.
    fn launchctl_output(&self, arguments: &[&str]) -> Result<(i32, String)> {
        let remaining = self.deadline - monotonic();
        if remaining <= 0.0 {
            return Err(Error::os(settle_failure(&self.action)));
        }
        let mut argv = vec!["launchctl".to_string()];
        argv.extend(arguments.iter().map(|a| a.to_string()));
        let (code, stdout, _) = run_capture(&argv, remaining, true)?;
        Ok((code, stdout))
    }

    fn systemctl(&self, arguments: &[&str]) -> Result<(i32, String)> {
        let mut all = vec!["--user"];
        all.extend_from_slice(arguments);
        run("systemctl", &all, self.deadline, &self.action)
    }

    fn target(&self) -> String {
        format!("gui/{}/{}", uid(), self.label)
    }

    /// Whether the job is loaded (launchd) or active (systemd).
    fn present(&self) -> Result<bool> {
        if cfg!(target_os = "macos") {
            Ok(self.launchctl(&["print", &self.target()])?.0 == 0)
        } else {
            Ok(self.systemctl(&["is-active", "--quiet", self.unit])?.0 == 0)
        }
    }

    fn stop_job(&self) -> Result<(i32, String)> {
        if cfg!(target_os = "macos") {
            self.launchctl(&["bootout", &self.target()])
        } else {
            self.systemctl(&["stop", self.unit])
        }
    }

    fn start_job(&self, present: bool) -> Result<(i32, String)> {
        if cfg!(target_os = "macos") {
            if present {
                self.launchctl(&["kickstart", &self.target()])
            } else {
                let plist = format!("{}/Library/LaunchAgents/{}.plist", home_dir(), self.label);
                self.launchctl(&["bootstrap", &format!("gui/{}", uid()), &plist])
            }
        } else {
            let reload = self.systemctl(&["daemon-reload"])?;
            if reload.0 != 0 {
                return Ok(reload);
            }
            self.systemctl(&["start", self.unit])
        }
    }

    /// Stop the job and wait until it is gone (and, when `identity` names
    /// the bridge's runtime record, until that process has exited).
    fn stop_and_wait(
        &self,
        identity: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<()> {
        let result = self.stop_job()?;
        loop {
            let present = self.present()?;
            let mut alive = false;
            if let Some(identity) = identity {
                if let Some(pid) = super::local::int_pid(identity.get("pid")) {
                    if let Ok(start) = crate::identity::process_start(pid) {
                        alive = value_eq(
                            &serde_json::Value::String(start),
                            identity
                                .get("procStart")
                                .unwrap_or(&serde_json::Value::Null),
                        );
                    }
                }
            }
            if !present && !alive {
                return Ok(());
            }
            if result.0 != 0 && present {
                let fallback = if cfg!(target_os = "macos") {
                    "launchctl bootout failed"
                } else {
                    "systemctl stop failed"
                };
                return Err(Error::value(failure_text(&result.1, fallback)));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn failure_text(stderr: &str, fallback: &str) -> String {
    let trimmed = crate::text::strip(stderr);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// A service manager's refusal to start the bridge, without its advice to
/// rerun as root (which is wrong for a user service).
fn plain_start_error(stderr: &str) -> String {
    let text = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text
        .split(" Try re-running the command as root")
        .next()
        .unwrap_or(&text)
        .trim();
    if text.is_empty() {
        String::new()
    } else if text.starts_with("Bootstrap failed") {
        format!("the bridge's service couldn't be started (launchd: {text})")
    } else {
        text.to_string()
    }
}

/// Start or stop the bridge service (`action` is `start` or `stop`).
pub fn service(action: &str, config_path: Option<&str>) -> Result<()> {
    if !supported() {
        return Err(Error::value(
            "starting and stopping the bridge needs launchd (macOS) or systemd (Linux)",
        ));
    }
    let manager = Manager::new(action, 30.0);
    if action == "stop" {
        let identity = config_path.and_then(|path| {
            private_json(path)
                .and_then(|config| runtime_status(&config, path))
                .ok()
        });
        return manager.stop_and_wait(identity.as_ref());
    }
    // A successful start can race a pending stop. Require registration across
    // multiple observations, and start again if the job disappears.
    let mut stable = 0;
    let mut launched = false;
    let mut last_error = String::new();
    let mut failures = 0;
    loop {
        let present = manager.present()?;
        if present && launched {
            stable += 1;
            if stable >= 3 {
                return Ok(());
            }
        } else {
            stable = 0;
            let result = manager.start_job(present)?;
            launched = result.0 == 0;
            last_error = plain_start_error(&result.1);
            failures = if launched { 0 } else { failures + 1 };
            // launchd refuses a disabled job however often it is asked; say
            // why instead of passing on its bare error.
            if failures == 1 && launchd_disabled(&manager) {
                return Err(Error::os(disabled_start_error(config_path)));
            }
            // A refusal that repeats is not a race with a stop: say so now
            // rather than at the deadline.
            if failures >= 3 {
                return Err(Error::os(if last_error.is_empty() {
                    settle_failure("start")
                } else {
                    last_error
                }));
            }
        }
        if monotonic() + 0.5 >= manager.deadline {
            return Err(Error::os(if last_error.is_empty() {
                settle_failure("start")
            } else {
                last_error
            }));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Whether the bridge's service file is installed (the launchd plist or the
/// systemd user unit).
pub fn installed() -> bool {
    crate::fsutil::exists(&super::install::paths(&home_dir()).service)
}

/// Whether the bridge service is running now, under its current name or
/// the name earlier versions used.
pub fn running() -> bool {
    supported()
        && [Manager::new("check", 5.0), Manager::legacy("check", 5.0)]
            .iter()
            .any(|manager| manager.present().unwrap_or(false))
}

/// Stop the bridge service an earlier version installed, if it is running,
/// and wait until it is gone. Returns whether it was running.
pub fn stop_legacy() -> Result<bool> {
    let manager = Manager::legacy("stop", 30.0);
    // A service manager that cannot be run has nothing running.
    if !manager.present().unwrap_or(false) {
        return Ok(false);
    }
    manager.stop_and_wait(None)?;
    Ok(true)
}

/// Stop the bridge service under its current name, if it is running.
/// Returns whether it was running.
pub fn stop_current() -> Result<bool> {
    let manager = Manager::new("stop", 30.0);
    if !manager.present().unwrap_or(false) {
        return Ok(false);
    }
    manager.stop_and_wait(None)?;
    Ok(true)
}

/// Make systemd forget unit files that were removed (Linux; best effort).
pub fn reload_units() {
    if cfg!(target_os = "linux") {
        let _ = Manager::new("check", 5.0).systemctl(&["daemon-reload"]);
    }
}

/// `doctor`'s service check: (status, summary, fix).
pub fn doctor_check() -> (&'static str, &'static str, Option<&'static str>) {
    let deadline = monotonic() + 3.0;
    let legacy = super::install::legacy_paths(&home_dir()).service;
    let legacy_left = crate::fsutil::exists(&legacy);
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{LABEL}", uid());
        return match run("launchctl", &["print", &target], deadline, "check") {
            Ok((0, _)) => ("pass", "The bridge service is loaded.", None),
            Ok(_) if legacy_left => (
                "warn",
                "The bridge service is still installed under its old name.",
                Some("Run `switchboard install` to move it, then `switchboard up`."),
            ),
            Ok(_) => (
                "warn",
                "The bridge service is not loaded.",
                Some("Run `switchboard up`."),
            ),
            Err(_) => ("warn", "The bridge service's state is unavailable.", None),
        };
    }
    if cfg!(target_os = "linux") {
        return match run(
            "systemctl",
            &["--user", "is-active", "--quiet", UNIT],
            deadline,
            "check",
        ) {
            Ok((0, _)) => ("pass", "The bridge service is active.", None),
            Ok(_) if legacy_left => (
                "warn",
                "The bridge service is still installed under its old name.",
                Some("Run `switchboard install` to move it, then `switchboard up`."),
            ),
            Ok(_) => (
                "warn",
                "The bridge service is not active.",
                Some("Run `switchboard up`."),
            ),
            Err(_) => ("warn", "The bridge service's state is unavailable.", None),
        };
    }
    (
        "warn",
        "Service checks need launchd (macOS) or systemd (Linux); check your own supervisor.",
        None,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn launchd_refusals_read_plainly() {
        assert_eq!(
            super::plain_start_error(
                "Bootstrap failed: 5: Input/output error\nTry re-running the command as root for richer errors.\n"
            ),
            "the bridge's service couldn't be started (launchd: Bootstrap failed: 5: Input/output error)"
        );
        assert_eq!(super::plain_start_error("  "), "");
        assert_eq!(super::plain_start_error("unit not found"), "unit not found");
    }
}
