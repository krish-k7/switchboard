//! `switchboard server uninstall [--purge]`: undo `server install`.
//!
//! It stops and disables the service, then removes the unit, the program,
//! and the `/usr/local/bin/switchboard` link. The configuration and the
//! state (registry, invites, messages, certificates) and the service account
//! stay, so installing again picks them up, unless `--purge` removes them
//! too. Like the installer it refuses, before changing anything, a unit or
//! directory the installer did not write; inside the directories it did
//! write it removes only the files the installer and the server make, by
//! name, and leaves anything else (and the directory holding it) in place.
//! Every removal in a directory goes through a descriptor opened without
//! following links, since the service account owns the state directory.
//! Running it again finds less to do and says what remains.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use serde_json::{Map, Value};

use super::install::{self, ACCOUNT, MARKER, UNIT_NAME};
use crate::cli::ui::{self, Mark, Theme, Tone};

const USAGE: &str = "Usage: switchboard server uninstall [--purge] [--json]\n";
const HELP: &str = "
Remove the Switchboard server from this machine (run as root): stop and
disable its service, and remove the unit, the program, and the
/usr/local/bin/switchboard link that switchboard server install wrote.

The configuration (/etc/switchboard), the registry, invites, messages, and
certificates (/var/lib/switchboard), and the switchboard account stay, so
installing again with the same options brings the server back as it was and
machines reconnect without joining again. Files the installer didn't write
are never removed. Running it again is safe.

Options:
  --purge     also delete the configuration, the state, and the account
  --json      print the result as JSON
  -h, --help  show this help
";

/// Where the installer puts things (tests use another root).
#[derive(Debug, Clone)]
pub struct Layout {
    pub unit: String,
    pub program_dir: String,
    pub program: String,
    pub link: String,
    pub config_dir: String,
    pub state: String,
}

impl Layout {
    pub fn installed() -> Layout {
        Layout {
            unit: install::UNIT.into(),
            program_dir: install::PROGRAM_DIR.into(),
            program: install::PROGRAM.into(),
            link: install::LINK.into(),
            config_dir: install::CONFIG_DIR.into(),
            state: install::STATE.into(),
        }
    }

    fn program_marker(&self) -> String {
        format!("{}/{MARKER_NAME}", self.program_dir)
    }

    fn config_marker(&self) -> String {
        format!("{}/{MARKER_NAME}", self.config_dir)
    }

    fn tls(&self) -> String {
        format!("{}/tls", self.state)
    }
}

/// The file beside the installer's files that says it wrote them.
const MARKER_NAME: &str = ".server-install";

/// What the uninstaller asks of the system (tests use a stand-in).
pub trait Host {
    fn systemd(&self) -> bool;
    /// Run `systemctl` with these arguments; an error when it fails.
    fn systemctl(&self, arguments: &[&str]) -> Result<(), String>;
    /// Whether the service is running (or starting).
    fn active(&self) -> bool;
    /// The service account's home directory, if the account exists.
    fn account_home(&self) -> Option<String>;
    /// Delete the service account and its group.
    fn remove_account(&self) -> Result<(), String>;
}

/// The real system.
struct System;

impl Host for System {
    fn systemd(&self) -> bool {
        super::system::has_systemd()
    }

    fn systemctl(&self, arguments: &[&str]) -> Result<(), String> {
        install::checked("systemctl", arguments)
    }

    fn active(&self) -> bool {
        install::run("systemctl", &["is-active", "--quiet", UNIT_NAME])
            .is_ok_and(|(code, _)| code == 0)
    }

    fn account_home(&self) -> Option<String> {
        install::user(ACCOUNT).map(|account| account.home)
    }

    fn remove_account(&self) -> Result<(), String> {
        install::checked("userdel", &[ACCOUNT])?;
        // userdel removes the account's own group only where the system
        // is configured to; the installer refuses a group without its
        // account, so it goes too.
        if install::group(ACCOUNT).is_some() {
            install::checked("groupdel", &[ACCOUNT])?;
        }
        Ok(())
    }
}

/// Something left in place, and what it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    /// `configuration`, `registry`, `invites`, `messages`, `certificates`,
    /// or `other` (a file the installer didn't write).
    pub kind: &'static str,
    pub path: String,
    /// For the registry, how many machines it holds.
    pub machines: Option<usize>,
}

/// What the uninstaller did and what remains.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// The service was running and was stopped.
    pub stopped: bool,
    /// The service was disabled (its unit was the installer's).
    pub disabled: bool,
    pub removed: Vec<String>,
    pub account_removed: bool,
    pub kept: Vec<Kept>,
    pub account_kept: bool,
    pub purge: bool,
}

impl Outcome {
    fn nothing_done(&self) -> bool {
        !self.stopped && !self.disabled && self.removed.is_empty() && !self.account_removed
    }
}

fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A directory itself, not a link to one.
fn real_directory(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|info| info.is_dir())
}

fn os_error(path: &str) -> String {
    format!("{path}: {}", std::io::Error::last_os_error())
}

/// The names in the directory open at `fd`.
fn names(fd: &OwnedFd, path: &str) -> Result<Vec<String>, String> {
    // SAFETY: dup returns a new descriptor that fdopendir takes over and
    // closedir closes; readdir entries are read before the next call.
    unsafe {
        let copy = libc::dup(fd.as_raw_fd());
        if copy < 0 {
            return Err(os_error(path));
        }
        let stream = libc::fdopendir(copy);
        if stream.is_null() {
            libc::close(copy);
            return Err(os_error(path));
        }
        libc::rewinddir(stream);
        let mut found = Vec::new();
        loop {
            let entry = libc::readdir(stream);
            if entry.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                .to_string_lossy()
                .into_owned();
            if name != "." && name != ".." {
                found.push(name);
            }
        }
        libc::closedir(stream);
        found.sort();
        Ok(found)
    }
}

/// Open `name` in the directory at `parent` as a directory, never through a
/// link; `None` when it isn't there.
fn open_inside(parent: &OwnedFd, name: &str, label: &str) -> Result<Option<OwnedFd>, String> {
    let c = CString::new(name).map_err(|_| format!("{label}: embedded null byte"))?;
    // SAFETY: openat with a valid name; the descriptor is owned below.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
                Err(format!("refusing non-directory: {label}"))
            }
            _ => Err(format!("{label}: {error}")),
        };
    }
    // SAFETY: fd is a new descriptor that nothing else owns.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Remove `name` from the directory at `fd` (a directory with
/// `directory`); `false` when it isn't a file of that kind or is gone.
fn unlink(fd: &OwnedFd, name: &str, directory: bool) -> bool {
    let Ok(c) = CString::new(name) else {
        return false;
    };
    let flags = if directory { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: unlinkat on a name in an open directory; it never follows a
    // link in the name.
    unsafe { libc::unlinkat(fd.as_raw_fd(), c.as_ptr(), flags) == 0 }
}

/// `.NAME.<32 hex>` or `NAME.<32 hex>`: a file being replaced atomically.
fn temporary_of(name: &str, base: impl Fn(&str) -> bool) -> bool {
    let Some((stem, suffix)) = name.rsplit_once('.') else {
        return false;
    };
    if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    base(stem.strip_prefix('.').unwrap_or(stem))
}

/// `.install-XXXXXX` or `.enroll-XXXXXX`: the installer's and
/// `enroll-local`'s temporary files.
fn mkstemp_temporary(name: &str) -> bool {
    [".install-", ".enroll-"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.len() == 6)
    })
}

fn program_file(name: &str) -> bool {
    name == "switchboard" || mkstemp_temporary(name)
}

fn config_file(name: &str) -> bool {
    name == "server.json" || mkstemp_temporary(name)
}

const STATE_FILES: [&str; 8] = [
    "peers.json",
    "peers.json.lock",
    "invites.json",
    "messages.sqlite3",
    "messages.sqlite3-wal",
    "messages.sqlite3-shm",
    "messages.sqlite3-journal",
    "bootstrap-token",
];

fn state_file(name: &str) -> bool {
    STATE_FILES.contains(&name)
        || mkstemp_temporary(name)
        || temporary_of(name, |base| STATE_FILES.contains(&base))
}

fn tls_base(name: &str) -> bool {
    name == "private-cert.pem"
        || name == "private-key.pem"
        || (name.starts_with("acme-account-") && name.ends_with(".json"))
        || (name.starts_with("acme-")
            && (name.ends_with("-cert.pem") || name.ends_with("-key.pem")))
}

fn tls_file(name: &str) -> bool {
    tls_base(name) || temporary_of(name, tls_base)
}

/// What sweeping a directory did.
#[derive(Debug, Default)]
struct Sweep {
    /// The directory itself was removed (it held nothing else).
    gone: bool,
    removed: Vec<String>,
    /// Entries the installer didn't write, left in place.
    left: Vec<String>,
}

/// Remove the entries `ours` accepts from the directory `path` (open at
/// `fd`), and the directory too when nothing else is in it. The marker, if
/// any, goes last and only with the directory, so a directory left holding
/// other files still reads as the installer's on the next run.
fn sweep(
    fd: OwnedFd,
    path: &str,
    ours: impl Fn(&str) -> bool,
    marker: bool,
    remove: impl FnOnce() -> bool,
) -> Result<Sweep, String> {
    let mut result = Sweep::default();
    for name in names(&fd, path)? {
        if marker && name == MARKER_NAME {
            continue;
        }
        let full = format!("{path}/{name}");
        if ours(&name) && unlink(&fd, &name, false) {
            result.removed.push(full);
        } else {
            result.left.push(full);
        }
    }
    if result.left.is_empty() {
        if marker && !unlink(&fd, MARKER_NAME, false) && exists(&format!("{path}/{MARKER_NAME}")) {
            return Err(os_error(&format!("{path}/{MARKER_NAME}")));
        }
        drop(fd);
        if !remove() {
            return Err(os_error(path));
        }
        result.gone = true;
    }
    Ok(result)
}

/// The machines in a registry file, when it can be read.
fn machine_count(path: &str) -> Option<usize> {
    let data = std::fs::read(path).ok()?;
    let registry = super::jsonval::loads(&data)?;
    Some(super::hub::parse_registry(&registry)?.len())
}

/// Refuse, before changing anything, what the installer didn't write.
fn refusals(layout: &Layout, purge: bool) -> Result<(), String> {
    if exists(&layout.unit)
        && !std::fs::read_to_string(&layout.unit).is_ok_and(|text| text.starts_with(MARKER))
    {
        return Err(format!("refusing unmanaged unit: {}", layout.unit));
    }
    if exists(&layout.program_dir)
        && (!real_directory(&layout.program_dir) || !exists(&layout.program_marker()))
    {
        return Err(format!(
            "refusing unmanaged directory: {}",
            layout.program_dir
        ));
    }
    if purge && exists(&layout.config_dir) {
        let managed = real_directory(&layout.config_dir) && exists(&layout.config_marker());
        if !managed && !install::empty_directory(&layout.config_dir) {
            return Err(format!(
                "refusing unmanaged directory: {}",
                layout.config_dir
            ));
        }
    }
    if purge {
        for path in [&layout.state, &layout.tls()] {
            if exists(path) && !real_directory(path) {
                return Err(format!("refusing non-directory: {path}"));
            }
        }
    }
    Ok(())
}

/// Remove the server; see the module documentation.
pub fn uninstall(layout: &Layout, purge: bool, host: &dyn Host) -> Result<Outcome, String> {
    refusals(layout, purge)?;
    let mut outcome = Outcome {
        purge,
        ..Default::default()
    };
    let home = host.account_home();
    // Whether the installer ran here: only then are the state's files its.
    let installed = exists(&layout.unit)
        || exists(&layout.program_marker())
        || exists(&layout.config_marker())
        || home.as_deref() == Some(layout.state.as_str());
    let mut left = Vec::new();

    // The service.
    if exists(&layout.unit) {
        if host.systemd() {
            // Disabling works on the unit file even when systemd hasn't
            // loaded it (an install with --no-start); stopping only when it
            // runs.
            host.systemctl(&["disable", "--quiet", UNIT_NAME])?;
            outcome.disabled = true;
            if host.active() {
                host.systemctl(&["stop", UNIT_NAME])?;
                outcome.stopped = true;
            }
        }
        std::fs::remove_file(&layout.unit).map_err(|exc| format!("{}: {exc}", layout.unit))?;
        outcome.removed.push(layout.unit.clone());
        if host.systemd() {
            host.systemctl(&["daemon-reload"])?;
            // Forget a failed state, so the unit doesn't linger in listings.
            let _ = host.systemctl(&["reset-failed", UNIT_NAME]);
        }
    }

    // The program.
    if exists(&layout.program_marker()) {
        let fd = install::open_directory(&layout.program_dir)?;
        let dir = layout.program_dir.clone();
        let swept = sweep(fd, &dir, program_file, true, || {
            std::fs::remove_dir(&dir).is_ok()
        })?;
        if swept.gone {
            outcome.removed.push(layout.program_dir.clone());
        } else {
            outcome.removed.extend(swept.removed);
            left.extend(swept.left);
        }
    }

    // The command's link, only when it points at the installer's program.
    if let Ok(target) = std::fs::read_link(&layout.link) {
        if target.to_string_lossy() == layout.program {
            std::fs::remove_file(&layout.link).map_err(|exc| format!("{}: {exc}", layout.link))?;
            outcome.removed.push(layout.link.clone());
        }
    }

    if purge {
        // The configuration.
        if real_directory(&layout.config_dir) && exists(&layout.config_marker()) {
            let fd = install::open_directory(&layout.config_dir)?;
            let dir = layout.config_dir.clone();
            let swept = sweep(fd, &dir, config_file, true, || {
                std::fs::remove_dir(&dir).is_ok()
            })?;
            if swept.gone {
                outcome.removed.push(layout.config_dir.clone());
            } else {
                outcome.removed.extend(swept.removed);
                left.extend(swept.left);
            }
        }
        // The state: certificates first, then the files beside them.
        if installed && real_directory(&layout.state) {
            let state = install::open_directory(&layout.state)?;
            if let Some(tls) = open_inside(&state, "tls", &layout.tls())? {
                let tls_path = layout.tls();
                let swept = sweep(tls, &tls_path, tls_file, false, || {
                    unlink(&state, "tls", true)
                })?;
                if !swept.gone {
                    outcome.removed.extend(swept.removed);
                    left.extend(swept.left);
                }
            }
            let dir = layout.state.clone();
            let swept = sweep(
                state,
                &dir,
                |name| name != "tls" && state_file(name),
                false,
                || std::fs::remove_dir(&dir).is_ok(),
            )?;
            if swept.gone {
                outcome.removed.push(layout.state.clone());
            } else {
                outcome.removed.extend(swept.removed);
                for path in swept.left {
                    // The certificate directory's own leftovers are listed.
                    if path != layout.tls() {
                        left.push(path);
                    }
                }
            }
        }
        // The account, once its home is gone.
        if home.as_deref() == Some(layout.state.as_str()) && !exists(&layout.state) {
            host.remove_account()?;
            outcome.account_removed = true;
        }
    }

    // What remains.
    let state = |name: &str| format!("{}/{name}", layout.state);
    let config = format!("{}/server.json", layout.config_dir);
    for (kind, path) in [
        ("configuration", config),
        ("registry", state("peers.json")),
        ("invites", state("invites.json")),
        ("messages", state("messages.sqlite3")),
        ("certificates", layout.tls()),
    ] {
        if exists(&path) && (installed || kind == "configuration") {
            let machines = (kind == "registry").then(|| machine_count(&path)).flatten();
            outcome.kept.push(Kept {
                kind,
                path,
                machines,
            });
        }
    }
    if exists(&layout.link) && !outcome.removed.contains(&layout.link) {
        left.push(layout.link.clone());
    }
    for path in left {
        if !outcome.kept.iter().any(|kept| kept.path == path) {
            outcome.kept.push(Kept {
                kind: "other",
                path,
                machines: None,
            });
        }
    }
    outcome.account_kept = !outcome.account_removed
        && home.as_deref() == Some(layout.state.as_str())
        && exists(&layout.state);
    Ok(outcome)
}

// Output -----------------------------------------------------------------------

fn label(kind: &str) -> &'static str {
    match kind {
        "configuration" => "Configuration",
        "registry" => "Registry",
        "invites" => "Invites",
        "messages" => "Messages",
        "certificates" => "Certificates",
        _ => "Other",
    }
}

/// What the uninstaller did and what remains, for people.
pub fn render(outcome: &Outcome, theme: &Theme, columns: usize) -> String {
    let mut lines = Vec::new();
    let path_line = |path: &str| format!("{}{path}", " ".repeat(ui::INDENT + 2));
    if outcome.nothing_done() {
        lines.extend(ui::done(
            theme,
            "The Switchboard server isn't installed here; there was nothing to remove.",
            columns,
        ));
    } else {
        if outcome.stopped {
            lines.extend(ui::done(
                theme,
                &format!("Stopped and disabled {UNIT_NAME}."),
                columns,
            ));
        } else if outcome.disabled {
            lines.extend(ui::done(
                theme,
                &format!("Disabled {UNIT_NAME} (it wasn't running)."),
                columns,
            ));
        }
        if !outcome.removed.is_empty() {
            lines.extend(ui::done(theme, "Removed:", columns));
            lines.extend(outcome.removed.iter().map(|path| path_line(path)));
        }
        if outcome.account_removed {
            lines.extend(ui::done(
                theme,
                &format!("Removed the {ACCOUNT} account."),
                columns,
            ));
        }
    }
    let data: Vec<&Kept> = outcome.kept.iter().filter(|k| k.kind != "other").collect();
    let other: Vec<&Kept> = outcome.kept.iter().filter(|k| k.kind == "other").collect();
    if !data.is_empty() || outcome.account_kept {
        lines.push(String::new());
        lines.extend(ui::heading(theme, "Kept", columns));
        let mut rows: Vec<(&str, String)> = data
            .iter()
            .map(|kept| {
                let value = match kept.machines {
                    Some(count) => {
                        format!("{}, {}", kept.path, ui::count(count, "machine", "machines"))
                    }
                    None => kept.path.clone(),
                };
                (label(kept.kind), value)
            })
            .collect();
        if outcome.account_kept {
            rows.push(("Account", format!("{ACCOUNT}, which owns the files above")));
        }
        let pad = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
        for (name, value) in rows {
            lines.extend(ui::prompt::fact_lines(
                theme,
                Mark::Blank,
                name,
                &value,
                pad,
                columns,
            ));
        }
    }
    if !other.is_empty() {
        lines.push(String::new());
        lines.extend(ui::para(
            theme,
            "Left in place, since Switchboard didn't write them:",
            Tone::Normal,
            columns,
        ));
        lines.extend(other.iter().map(|kept| path_line(&kept.path)));
    }
    lines.push(String::new());
    if !data.is_empty() || outcome.account_kept {
        lines.extend(ui::para(
            theme,
            "Installing the server again with the same options uses these, so machines reconnect without joining again. To delete them too, run\n`sudo switchboard server uninstall --purge`",
            Tone::Normal,
            columns,
        ));
    } else if outcome.purge && !outcome.nothing_done() {
        lines.extend(ui::para(
            theme,
            "Machines that joined this server can't connect any more. On each one, this removes Switchboard:\n`switchboard uninstall`",
            Tone::Normal,
            columns,
        ));
    } else {
        lines.pop();
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The result for scripts; the field names are stable.
pub fn json(outcome: &Outcome) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("ok".into(), Value::Bool(true));
    value.insert("stopped".into(), Value::Bool(outcome.stopped));
    value.insert("disabled".into(), Value::Bool(outcome.disabled));
    value.insert(
        "removed".into(),
        Value::Array(outcome.removed.iter().cloned().map(Value::from).collect()),
    );
    value.insert(
        "account_removed".into(),
        Value::Bool(outcome.account_removed),
    );
    value.insert(
        "kept".into(),
        Value::Array(
            outcome
                .kept
                .iter()
                .map(|kept| {
                    let mut item = Map::new();
                    item.insert("kind".into(), Value::from(kept.kind));
                    item.insert("path".into(), Value::from(kept.path.clone()));
                    if let Some(count) = kept.machines {
                        item.insert("machines".into(), Value::from(count));
                    }
                    Value::Object(item)
                })
                .collect(),
        ),
    );
    value.insert("account_kept".into(), Value::Bool(outcome.account_kept));
    value
}

/// Parse the arguments: `(purge, json)`; `Ok(None)` for `--help`.
pub fn parse(argv: &[String]) -> Result<Option<(bool, bool)>, String> {
    let (mut purge, mut json) = (false, false);
    for argument in argv {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--purge" => purge = true,
            "--json" => json = true,
            other if other.starts_with('-') => {
                return Err(format!(
                    "switchboard server uninstall: unknown option {other}\n{USAGE}"
                ))
            }
            other => {
                return Err(format!(
                    "switchboard server uninstall: unexpected argument {other}\n{USAGE}"
                ))
            }
        }
    }
    Ok(Some((purge, json)))
}

/// `switchboard server uninstall [--purge] [--json]`.
pub fn main(argv: &[String]) -> i32 {
    let (purge, json_output) = match parse(argv) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            crate::cli::print_help(&format!("{USAGE}{HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server uninstall --help");
            return 2;
        }
    };
    let fail = |problem: ui::prompt::Problem, message: &str| {
        if json_output {
            let mut value = Map::new();
            value.insert("ok".into(), Value::Bool(false));
            value.insert("error".into(), Value::from(message));
            println!("{}", crate::wirejson::dumps(&Value::Object(value)));
        } else {
            crate::cli::print_problem(&problem);
        }
        1
    };
    if !cfg!(target_os = "linux") {
        return fail(
            ui::prompt::Problem::new("The server runs only on Linux with systemd.").without_fix(),
            "the server runs only on Linux with systemd",
        );
    }
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return fail(
            ui::prompt::Problem::new("Removing the server needs root.")
                .fix("run it with sudo: `sudo switchboard server uninstall`"),
            "run as root",
        );
    }
    match uninstall(&Layout::installed(), purge, &System) {
        Ok(outcome) => {
            if json_output {
                println!("{}", crate::wirejson::dumps(&Value::Object(json(&outcome))));
            } else {
                print!(
                    "{}",
                    render(
                        &outcome,
                        &Theme::for_fd(1),
                        crate::cli::format::output_width()
                    )
                );
            }
            0
        }
        Err(message) => {
            let foreign = [
                "refusing unmanaged unit: ",
                "refusing unmanaged directory: ",
                "refusing non-directory: ",
            ]
            .iter()
            .find_map(|prefix| message.strip_prefix(prefix));
            let problem = match foreign {
                Some(path) => ui::prompt::Problem::new(format!(
                    "{path} wasn't written by the server installer, so nothing was removed."
                ))
                .fix(format!(
                    "move it aside if it isn't needed, then run `sudo switchboard server uninstall{}` again.",
                    if purge { " --purge" } else { "" }
                )),
                None => ui::prompt::Problem::new(format!("The server couldn't be removed: {message}."))
                    .fix("fix that, then run `sudo switchboard server uninstall` again; it carries on from where it stopped."),
            };
            fail(problem, &message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        systemd: bool,
        home: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
    }

    impl Host for Fake {
        fn systemd(&self) -> bool {
            self.systemd
        }
        fn systemctl(&self, arguments: &[&str]) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("systemctl {}", arguments.join(" ")));
            Ok(())
        }
        fn active(&self) -> bool {
            self.calls.borrow_mut().push("is-active".into());
            true
        }
        fn account_home(&self) -> Option<String> {
            self.home.borrow().clone()
        }
        fn remove_account(&self) -> Result<(), String> {
            self.calls.borrow_mut().push("userdel".into());
            *self.home.borrow_mut() = None;
            Ok(())
        }
    }

    struct Root(String);

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn root() -> Root {
        let template = CString::new("/tmp/sbuninst-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
        raw.pop();
        let path = std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap();
        Root(path.to_string_lossy().into_owned())
    }

    fn layout(root: &str) -> Layout {
        Layout {
            unit: format!("{root}/etc/systemd/system/switchboard-server.service"),
            program_dir: format!("{root}/usr/local/lib/switchboard"),
            program: format!("{root}/usr/local/lib/switchboard/switchboard"),
            link: format!("{root}/usr/local/bin/switchboard"),
            config_dir: format!("{root}/etc/switchboard"),
            state: format!("{root}/var/lib/switchboard"),
        }
    }

    fn write(path: &str, text: &str) {
        std::fs::create_dir_all(crate::fsutil::parent(path)).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// What `server install --tls private` leaves, with a joined machine.
    fn installed(layout: &Layout) {
        write(&layout.unit, &format!("{MARKER}\n[Unit]\n"));
        write(&layout.program, "binary");
        write(&layout.program_marker(), "1\n");
        std::fs::create_dir_all(crate::fsutil::parent(&layout.link)).unwrap();
        std::os::unix::fs::symlink(&layout.program, &layout.link).unwrap();
        write(&format!("{}/server.json", layout.config_dir), "{}\n");
        write(&layout.config_marker(), "1\n");
        let state = |name: &str| format!("{}/{name}", layout.state);
        write(
            &state("peers.json"),
            &format!(
                "{{\"{}\": {{\"machine\": \"laptop\", \"owner\": \"op\", \"enabled\": true}}}}\n",
                "a".repeat(64)
            ),
        );
        for name in [
            "peers.json.lock",
            "invites.json",
            "messages.sqlite3",
            "messages.sqlite3-wal",
        ] {
            write(&state(name), "");
        }
        write(&state("tls/private-cert.pem"), "cert");
        write(&state("tls/private-key.pem"), "key");
    }

    fn fake(layout: &Layout, systemd: bool) -> Fake {
        Fake {
            systemd,
            home: RefCell::new(Some(layout.state.clone())),
            calls: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn uninstall_keeps_the_state_and_says_where_it_is() {
        let root = root();
        let layout = layout(&root.0);
        installed(&layout);
        let host = fake(&layout, true);
        let outcome = uninstall(&layout, false, &host).unwrap();
        assert!(outcome.stopped);
        assert_eq!(
            outcome.removed,
            [
                layout.unit.clone(),
                layout.program_dir.clone(),
                layout.link.clone()
            ]
        );
        assert_eq!(
            *host.calls.borrow(),
            [
                "systemctl disable --quiet switchboard-server.service",
                "is-active",
                "systemctl stop switchboard-server.service",
                "systemctl daemon-reload",
                "systemctl reset-failed switchboard-server.service"
            ]
        );
        let kinds: Vec<&str> = outcome.kept.iter().map(|k| k.kind).collect();
        assert_eq!(
            kinds,
            [
                "configuration",
                "registry",
                "invites",
                "messages",
                "certificates"
            ]
        );
        assert_eq!(outcome.kept[1].machines, Some(1));
        assert!(outcome.account_kept);
        let text = render(&outcome, &Theme::plain(), 200);
        assert!(
            text.starts_with(
                "✓ Stopped and disabled switchboard-server.service.\n✓ Removed:\n    "
            ),
            "{text}"
        );
        let read = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(read.contains("Registry "), "{text}");
        assert!(read.contains("peers.json, 1 machine"), "{text}");
        assert!(
            text.contains("\n  Account        switchboard, which owns the files above\n"),
            "{text}"
        );
        assert!(
            text.ends_with("\n      sudo switchboard server uninstall --purge\n"),
            "{text}"
        );
        let value = json(&outcome);
        assert_eq!(value["kept"][1]["machines"], 1);
        // Again: nothing to do, and the same things kept.
        let again = uninstall(&layout, false, &host).unwrap();
        assert!(again.nothing_done());
        assert_eq!(again.kept, outcome.kept);
        let text = render(&again, &Theme::plain(), 200);
        assert!(text.starts_with("✓ The Switchboard server isn't installed here; there was nothing to remove.\n\n  Kept\n"), "{text}");
        // Then purge: the rest goes, the account last.
        let purged = uninstall(&layout, true, &host).unwrap();
        assert_eq!(
            purged.removed,
            [layout.config_dir.clone(), layout.state.clone()]
        );
        assert!(purged.account_removed);
        assert!(purged.kept.is_empty() && !purged.account_kept);
        assert!(!exists(&layout.config_dir) && !exists(&layout.state));
        let text = render(&purged, &Theme::plain(), 200);
        assert!(
            text.contains("✓ Removed the switchboard account.\n"),
            "{text}"
        );
        assert!(text.ends_with("\n      switchboard uninstall\n"), "{text}");
        let last = uninstall(&layout, true, &host).unwrap();
        assert!(last.nothing_done() && last.kept.is_empty());
    }

    #[test]
    fn purge_in_one_step_removes_everything_the_installer_made() {
        let root = root();
        let layout = layout(&root.0);
        installed(&layout);
        // Temporary files the installer and server leave when interrupted.
        write(&format!("{}/.install-a1B2c3", layout.program_dir), "");
        write(
            &format!("{}/.peers.json.{}", layout.state, "0".repeat(32)),
            "",
        );
        write(
            &format!("{}/tls/acme-account-0123456789abcdef.json", layout.state),
            "",
        );
        write(
            &format!("{}/tls/acme-sb.example.com-cert.pem", layout.state),
            "",
        );
        write(
            &format!("{}/tls/private-key.pem.{}", layout.state, "f".repeat(32)),
            "",
        );
        let host = fake(&layout, false);
        let outcome = uninstall(&layout, true, &host).unwrap();
        // Without systemd nothing is stopped, but the files go.
        assert!(!outcome.stopped);
        assert_eq!(*host.calls.borrow(), ["userdel"]);
        assert_eq!(
            outcome.removed,
            [
                layout.unit.clone(),
                layout.program_dir.clone(),
                layout.link.clone(),
                layout.config_dir.clone(),
                layout.state.clone()
            ]
        );
        assert!(outcome.kept.is_empty());
    }

    #[test]
    fn files_the_installer_did_not_write_are_never_touched() {
        let root = root();
        let layout = layout(&root.0);
        // A unit someone else wrote: refused before anything changes.
        installed(&layout);
        write(&layout.unit, "[Service]\nExecStart=/bin/true\n");
        let host = fake(&layout, true);
        assert_eq!(
            uninstall(&layout, false, &host).unwrap_err(),
            format!("refusing unmanaged unit: {}", layout.unit)
        );
        assert!(host.calls.borrow().is_empty());
        assert!(exists(&layout.program));
        std::fs::remove_file(&layout.unit).unwrap();
        // A program directory without the marker.
        std::fs::remove_file(layout.program_marker()).unwrap();
        assert_eq!(
            uninstall(&layout, false, &host).unwrap_err(),
            format!("refusing unmanaged directory: {}", layout.program_dir)
        );
        write(&layout.program_marker(), "1\n");
        // An unmanaged /etc/switchboard only matters to --purge.
        std::fs::remove_file(layout.config_marker()).unwrap();
        assert_eq!(
            uninstall(&layout, true, &host).unwrap_err(),
            format!("refusing unmanaged directory: {}", layout.config_dir)
        );
        write(&layout.config_marker(), "1\n");
        // Foreign files inside the installer's directories stay, with their
        // directory and its marker; a link elsewhere stays too.
        write(&format!("{}/notes.txt", layout.program_dir), "mine");
        write(&format!("{}/backup.tgz", layout.state), "mine");
        write(&format!("{}/tls/mine.pem", layout.state), "mine");
        std::fs::remove_file(&layout.link).unwrap();
        std::os::unix::fs::symlink("/bin/sh", &layout.link).unwrap();
        let outcome = uninstall(&layout, true, &host).unwrap();
        assert!(exists(&format!("{}/notes.txt", layout.program_dir)));
        assert!(exists(&layout.program_marker()));
        assert!(!exists(&layout.program));
        assert!(exists(&format!("{}/backup.tgz", layout.state)));
        assert!(exists(&format!("{}/tls/mine.pem", layout.state)));
        assert!(!exists(&format!("{}/tls/private-key.pem", layout.state)));
        assert!(!exists(&format!("{}/peers.json", layout.state)));
        assert_eq!(
            std::fs::read_link(&layout.link).unwrap().to_string_lossy(),
            "/bin/sh"
        );
        assert!(!outcome.account_removed && outcome.account_kept);
        let others: Vec<&str> = outcome
            .kept
            .iter()
            .filter(|k| k.kind == "other")
            .map(|k| k.path.as_str())
            .collect();
        assert_eq!(
            others,
            [
                format!("{}/notes.txt", layout.program_dir),
                format!("{}/tls/mine.pem", layout.state),
                format!("{}/backup.tgz", layout.state),
                layout.link.clone()
            ]
        );
        let text = render(&outcome, &Theme::plain(), 200);
        assert!(
            text.contains("Left in place, since Switchboard didn't write them:\n"),
            "{text}"
        );
        // Running it again changes nothing and lists the same.
        let again = uninstall(&layout, true, &host).unwrap();
        assert!(again.nothing_done(), "{again:?}");
        assert_eq!(again.kept, outcome.kept);
    }

    #[test]
    fn a_link_in_place_of_the_certificates_is_refused() {
        let root = root();
        let layout = layout(&root.0);
        installed(&layout);
        let outside = format!("{}/outside", root.0);
        write(&format!("{outside}/private-key.pem"), "keep");
        std::fs::remove_dir_all(layout.tls()).unwrap();
        std::os::unix::fs::symlink(&outside, layout.tls()).unwrap();
        let host = fake(&layout, false);
        assert_eq!(
            uninstall(&layout, true, &host).unwrap_err(),
            format!("refusing non-directory: {}", layout.tls())
        );
        // Refused before anything changed.
        assert!(exists(&format!("{outside}/private-key.pem")));
        assert!(exists(&layout.unit) && exists(&layout.program));
    }

    #[test]
    fn names_the_server_writes_are_recognized() {
        assert!(state_file("peers.json"));
        assert!(state_file(&format!(".invites.json.{}", "a".repeat(32))));
        assert!(state_file(".enroll-Ab12Cd"));
        assert!(!state_file("peers.json.bak"));
        assert!(!state_file(&format!(".notes.{}", "a".repeat(32))));
        assert!(tls_file("acme-sb.example.com-key.pem"));
        assert!(tls_file(&format!(
            "acme-account-0011.json.{}",
            "b".repeat(32)
        )));
        assert!(!tls_file("other.pem"));
        assert!(program_file("switchboard") && !program_file("switchboard.old"));
        assert!(config_file("server.json") && !config_file("server.json.bak"));
        assert_eq!(
            parse(&["--purge".into(), "--json".into()]),
            Ok(Some((true, true)))
        );
        assert!(parse(&["--force".into()]).is_err());
        assert!(parse(&["now".into()]).is_err());
    }
}
