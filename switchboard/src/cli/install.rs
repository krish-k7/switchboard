//! The installer: puts this binary at `~/.local/bin/switchboard` (the
//! command itself, no wrapper) and installs the user service that runs it
//! (a launchd agent on macOS, a systemd user unit on Linux). The bridge's
//! logs, the installer's backups, and the service's working directory are
//! in `~/.local/share/switchboard`; the configuration stays in
//! `~/.config/switchboard`. It keeps the managed-file checks, backups, and
//! log handling of early releases' installer, so any earlier installation
//! upgrades in place, and it moves a bridge service installed under the
//! names earlier versions used (`service::LEGACY_LABEL`, `LEGACY_UNIT`):
//! it stops that service if it is running, removes it, and starts the new
//! one in its place. Otherwise it never starts anything unless `--start` is
//! given, never enrolls chats, and never edits Claude settings. A machine
//! taken down with `switchboard down` stays down: the service isn't enabled
//! or started, even with `--start`.
//!
//! Releases before 0.3.1 put the program in `~/.local/share/switchboard`,
//! the command in `~/bin/switchboard` (a wrapper script), and a link to it
//! in `~/.local/bin`; the Python releases before them also left their
//! modules in `~/.local/share/switchboard`. Installing moves such an
//! installation (`migrate`), touching only what an installer wrote: the
//! wrapper (recognized by its marker line), the link, the old program copy
//! (once no service runs it), the Python files by their exact names
//! (`PYTHON_FILES`), and installer backups beyond the newest
//! `KEEP_BACKUPS`. A `~/bin/switchboard` that is a Claude Code plugin's
//! only way to the command (plugins before 0.3.1 run that path) becomes a
//! link to the program instead, until that plugin is updated.

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::fmtutil::{local_compact_stamp, time_ns};
use crate::fsutil;

pub const LABEL: &str = super::service::LABEL;
/// The line that marks a command or unit file as this installer's.
pub const MARKER: &str = "# Managed by switchboard install";
/// The marker early releases' installer wrote into the command wrapper,
/// still recognized.
pub const LEGACY_MARKER: &str = "# Managed by Switchboard install_local.py";
pub const BINARY: &str = "switchboard";

/// Whether a command or unit file carries this installer's marker (or an
/// earlier installer's).
pub fn has_any_marker(text: &str) -> bool {
    text.contains(MARKER) || text.contains(LEGACY_MARKER)
}

/// The paths an installation owns, under one home directory.
pub struct Paths {
    /// `~/.local/share/switchboard`: the bridge's logs, the installer's
    /// backups and marker, and the service's working directory.
    pub destination: String,
    /// `~/.local/bin/switchboard`: the program, which is the command.
    pub program: String,
    /// `~/bin/switchboard`: where releases before 0.3.1 wrote the command
    /// (a wrapper script).
    pub old_command: String,
    /// `~/.local/share/switchboard/switchboard`: where releases before
    /// 0.3.1 put the program.
    pub old_program: String,
    pub service: String,
    pub wants: Option<String>,
    pub config: String,
}

/// Files the Python releases' installer copied into
/// `~/.local/share/switchboard`: every name any of its versions wrote.
pub const PYTHON_FILES: &[&str] = &[
    "switchboard.py",
    "bridge.py",
    "native_peer.py",
    "local_viewer.py",
    "discovery_watch.py",
    "hub_transport.py",
    "hub_requests.py",
    "doctor.py",
    "session_metadata.py",
    "roster_client.py",
    "membership.py",
    "ping_client.py",
    "enroll.py",
    "machines_client.py",
];

/// How many of the installer's backup folders are kept (the newest).
pub const KEEP_BACKUPS: usize = 5;

/// The first release that installs the program at `~/.local/bin/switchboard`
/// (earlier ones keep it in `~/.local/share/switchboard`, with a `~/bin`
/// command).
pub const LOCAL_BIN_SINCE: &str = "0.3.1";

/// The first plugin version that finds the command in `~/.local/bin`;
/// older ones run `~/bin/switchboard`.
pub const PLUGIN_FINDS_LOCAL_BIN: &str = "0.3.1";

/// The systemd user unit directory (`$XDG_CONFIG_HOME/systemd/user`).
fn unit_dir(home: &str) -> String {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(base) if base.starts_with('/') => fsutil::join(&base, "systemd/user"),
        _ => fsutil::join(home, ".config/systemd/user"),
    }
}

/// A bridge service installed under an earlier name.
pub struct Legacy {
    pub service: String,
    pub wants: Option<String>,
}

pub fn legacy_paths(home: &str) -> Legacy {
    if cfg!(target_os = "linux") {
        let dir = unit_dir(home);
        Legacy {
            service: fsutil::join(&dir, super::service::LEGACY_UNIT),
            wants: Some(fsutil::join(
                &dir,
                &format!("default.target.wants/{}", super::service::LEGACY_UNIT),
            )),
        }
    } else {
        Legacy {
            service: fsutil::join(
                home,
                &format!(
                    "Library/LaunchAgents/{}.plist",
                    super::service::LEGACY_LABEL
                ),
            ),
            wants: None,
        }
    }
}

pub fn paths(home: &str) -> Paths {
    let destination = fsutil::join(home, ".local/share/switchboard");
    let (service, wants) = if cfg!(target_os = "linux") {
        let dir = unit_dir(home);
        (
            fsutil::join(&dir, super::service::UNIT),
            Some(fsutil::join(
                &dir,
                &format!("default.target.wants/{}", super::service::UNIT),
            )),
        )
    } else {
        (
            fsutil::join(home, &format!("Library/LaunchAgents/{LABEL}.plist")),
            None,
        )
    };
    Paths {
        old_program: fsutil::join(&destination, BINARY),
        destination,
        program: fsutil::join(home, ".local/bin/switchboard"),
        old_command: fsutil::join(home, "bin/switchboard"),
        service,
        wants,
        config: fsutil::join(home, ".config/switchboard/bridge.json"),
    }
}

fn plist_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launch agent's property list (XML, tab-indented, keys sorted).
pub fn launch_agent(binary: &str, destination: &str, config: &str) -> String {
    let string = |value: &str| format!("\t<string>{}</string>\n", plist_escape(value));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    out.push_str("\t<key>ExitTimeOut</key>\n\t<integer>30</integer>\n");
    // Kept running (restarted after it exits) only while its configuration
    // exists: after leaving a server, or with the configuration removed,
    // it exits once instead of restarting every ten seconds.
    out.push_str("\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>PathState</key>\n\t\t<dict>\n");
    out.push_str(&format!(
        "\t\t\t<key>{}</key>\n\t\t\t<true/>\n",
        plist_escape(config)
    ));
    out.push_str("\t\t</dict>\n\t</dict>\n");
    out.push_str(&format!("\t<key>Label</key>\n{}", string(LABEL)));
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for argument in [binary, "bridge", "--config", config] {
        out.push_str(&format!("\t{}", string(argument)));
    }
    out.push_str("\t</array>\n");
    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    out.push_str(&format!(
        "\t<key>StandardErrorPath</key>\n{}",
        string(&fsutil::join(destination, "daemon-error.log"))
    ));
    out.push_str(&format!(
        "\t<key>StandardOutPath</key>\n{}",
        string(&fsutil::join(destination, "daemon.log"))
    ));
    out.push_str("\t<key>SwitchboardManaged</key>\n\t<true/>\n");
    out.push_str("\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n");
    out.push_str("\t<key>Umask</key>\n\t<integer>63</integer>\n");
    out.push_str(&format!(
        "\t<key>WorkingDirectory</key>\n{}",
        string(destination)
    ));
    out.push_str("</dict>\n</plist>\n");
    out
}

fn systemd_quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The systemd user unit: the launch agent's settings in systemd terms. It
/// starts (and restarts) only while the configuration exists, as the
/// launch agent's `PathState` does.
pub fn systemd_unit(binary: &str, destination: &str, config: &str) -> String {
    format!(
        "{MARKER}\n[Unit]\nDescription=Switchboard bridge\nConditionPathExists={}\n\n[Service]\nExecStart={} bridge --config {}\nWorkingDirectory={}\nRestart=always\nRestartSec=10\nTimeoutStopSec=30\nUMask=0077\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
        systemd_path(config),
        systemd_quote(binary),
        systemd_quote(config),
        systemd_quote(destination),
        systemd_path(&fsutil::join(destination, "daemon.log")),
        systemd_path(&fsutil::join(destination, "daemon-error.log")),
    )
}

/// A path after `append:` or in a condition, which systemd takes unquoted
/// but still expands `%` specifiers in, so a literal `%` is written `%%`.
fn systemd_path(value: &str) -> String {
    value.replace('%', "%%")
}

/// Whether an existing service file is one this installer manages.
pub fn managed_service(path: &str) -> Result<bool> {
    let raw = std::fs::read(path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    let text = String::from_utf8(raw)
        .map_err(|_| Error::value("'utf-8' codec can't decode the service file"))?;
    if cfg!(target_os = "linux") {
        return Ok(has_any_marker(&text));
    }
    // Managed when the property list's `SwitchboardManaged` key is true.
    if !text.trim_start().starts_with("<?xml")
        && !text.trim_start().starts_with("<!DOCTYPE")
        && !text.trim_start().starts_with("<plist")
    {
        return Err(Error::value("Invalid file"));
    }
    let Some(index) = text.find("<key>SwitchboardManaged</key>") else {
        return Ok(false);
    };
    let rest = text[index + "<key>SwitchboardManaged</key>".len()..].trim_start();
    Ok(rest.starts_with("<true/>"))
}

pub fn read_text(path: &str) -> Result<String> {
    let raw = std::fs::read(path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    String::from_utf8(raw).map_err(|_| Error::value("'utf-8' codec can't decode the command file"))
}

/// Whether `link` is a symbolic link to `target`.
pub fn links_to(link: &str, target: &str) -> bool {
    fsutil::is_symlink(link) && fsutil::resolve(link).ok() == fsutil::resolve(target).ok()
}

/// What is at `~/.local/bin/switchboard` or `~/bin/switchboard`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Absent,
    /// A file an installer wrote: a Switchboard program (it carries
    /// `PROGRAM_SIGNATURE`) or an earlier release's wrapper script (a line of
    /// it is the marker).
    Ours,
    /// A link an installer made: `~/.local/bin/switchboard` to the old
    /// `~/bin/switchboard`, or `~/bin/switchboard` to the program.
    OurLink,
    /// Anything else, which is left alone.
    Foreign,
}

/// Text every Switchboard program from 0.3.1 on carries (the search below
/// is for it, so the binary holds it): a file at `~/.local/bin/switchboard`
/// that has it is a Switchboard program. install.sh looks for it too.
pub const PROGRAM_SIGNATURE: &str = "switchboard-program-signature:7f3b9c1e5a2d4f60";

/// The contents of the regular file at `path` (this user's, not a link), if
/// it is at most `cap` bytes.
fn read_own_file(path: &str, cap: u64) -> Option<Vec<u8>> {
    let info = fsutil::lstat(path).ok()?;
    if !fsutil::is_type(&info, fsutil::S_IFREG)
        || info.uid() != fsutil::getuid()
        || info.len() > cap
    {
        return None;
    }
    std::fs::read(path).ok()
}

/// Whether text is a command script an installer wrote: one of its lines is
/// exactly the installer's marker (or an earlier installer's).
fn has_marker_line(text: &str) -> bool {
    text.lines()
        .any(|line| line == MARKER || line == LEGACY_MARKER)
}

/// Whether the file at `path` is a command script an installer wrote (at
/// most 1 MiB is read).
pub fn is_our_script(path: &str) -> bool {
    read_own_file(path, 1 << 20)
        .and_then(|data| String::from_utf8(data).ok())
        .is_some_and(|text| has_marker_line(&text))
}

/// Whether the file at `path` is a Switchboard program (it carries
/// `PROGRAM_SIGNATURE`) or a command script an installer wrote. The size
/// cap keeps a large foreign file from being read whole.
pub fn is_our_program(path: &str) -> bool {
    let Some(data) = read_own_file(path, 256 << 20) else {
        return false;
    };
    let signature = std::hint::black_box(PROGRAM_SIGNATURE).as_bytes();
    data.windows(signature.len())
        .any(|window| window == signature)
        || std::str::from_utf8(&data).is_ok_and(has_marker_line)
}

/// What is at the program's path, `~/.local/bin/switchboard`.
pub fn program_state(p: &Paths) -> Found {
    match fsutil::lstat(&p.program) {
        Err(_) => Found::Absent,
        Ok(info) if fsutil::is_type(&info, fsutil::S_IFLNK) => {
            if links_to(&p.program, &p.old_command) {
                Found::OurLink
            } else {
                Found::Foreign
            }
        }
        Ok(_) if is_our_program(&p.program) => Found::Ours,
        Ok(_) => Found::Foreign,
    }
}

/// What is at the old command's path, `~/bin/switchboard`.
pub fn old_command_state(p: &Paths) -> Found {
    match fsutil::lstat(&p.old_command) {
        Err(_) => Found::Absent,
        Ok(info) if fsutil::is_type(&info, fsutil::S_IFLNK) => {
            if fsutil::readlink(&p.old_command).ok().as_deref() == Some(p.program.as_str()) {
                Found::OurLink
            } else {
                Found::Foreign
            }
        }
        Ok(info) if fsutil::is_type(&info, fsutil::S_IFREG) => {
            if is_our_script(&p.old_command) {
                Found::Ours
            } else {
                Found::Foreign
            }
        }
        Ok(_) => Found::Foreign,
    }
}

/// Whether two paths name the same directory entry's file, following
/// links in the directories but not a final link: `~/bin` linked to
/// `~/.local/bin` would make `~/bin/switchboard` the program itself, while
/// a `~/bin/switchboard` link to the program is a file of its own.
fn same_file(a: &str, b: &str) -> bool {
    match (fsutil::lstat(a), fsutil::lstat(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Whether the installation directory is the installer's own.
fn managed_destination(p: &Paths) -> bool {
    fsutil::lstat(&p.destination).is_ok_and(|info| fsutil::is_type(&info, fsutil::S_IFDIR))
        && fsutil::lstat(&fsutil::join(&p.destination, ".switchboard-install"))
            .is_ok_and(|info| fsutil::is_type(&info, fsutil::S_IFREG))
}

/// Remove the program copy releases before 0.3.1 kept in
/// `~/.local/share/switchboard`. Only a regular file in the installer's own
/// directory goes. Returns whether it was there.
pub fn remove_old_program(p: &Paths) -> Result<bool> {
    if !managed_destination(p)
        || !fsutil::lstat(&p.old_program).is_ok_and(|info| fsutil::is_type(&info, fsutil::S_IFREG))
    {
        return Ok(false);
    }
    fsutil::unlink(&p.old_program)?;
    Ok(true)
}

/// The bridge calls this as it starts. Started from the program's own path,
/// it shows the service no longer runs the old copy, which goes now.
pub fn finish_move() {
    let Some(started_as) = std::env::args_os().next() else {
        return;
    };
    let p = paths(&super::local::home_dir());
    if started_as.to_string_lossy() == p.program.as_str() {
        let _ = remove_old_program(&p);
    }
}

/// Remove the Python releases' modules from the installer's directory, by
/// their exact names, and the bytecode Python compiled from them (only
/// `NAME.TAG.pyc` for those names, then `__pycache__` if that left it
/// empty). Returns the paths removed.
pub fn remove_python_files(p: &Paths) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    if !managed_destination(p) {
        return Ok(removed);
    }
    for name in PYTHON_FILES {
        let path = fsutil::join(&p.destination, name);
        if fsutil::lstat(&path).is_ok_and(|info| !info.is_dir()) {
            fsutil::unlink(&path)?;
            removed.push(path);
        }
    }
    let cache = fsutil::join(&p.destination, "__pycache__");
    if fsutil::lstat(&cache).is_ok_and(|info| fsutil::is_type(&info, fsutil::S_IFDIR)) {
        let stems: Vec<&str> = PYTHON_FILES
            .iter()
            .map(|name| name.trim_end_matches(".py"))
            .collect();
        let compiled = |name: &str| {
            name.ends_with(".pyc")
                && stems.iter().any(|stem| {
                    name.strip_prefix(stem)
                        .and_then(|rest| rest.strip_prefix('.'))
                        .is_some_and(|tag| {
                            tag.bytes().all(|b| {
                                b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_'
                            })
                        })
                })
        };
        let entries = std::fs::read_dir(&cache)
            .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&cache))))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = fsutil::join(&cache, &name);
            if compiled(&name) && fsutil::lstat(&path).is_ok_and(|info| !info.is_dir()) {
                fsutil::unlink(&path)?;
                removed.push(path);
            }
        }
        if std::fs::remove_dir(&cache).is_ok() {
            removed.push(cache);
        }
    }
    Ok(removed)
}

/// Whether a backup folder's name is one the installer makes
/// (`YYYYMMDDTHHMMSS-NANOSECONDS`).
fn backup_name(name: &str) -> Option<(&str, u128)> {
    let (stamp, nanos) = name.split_once('-')?;
    let bytes = stamp.as_bytes();
    let stamp_ok = bytes.len() == 15
        && bytes[8] == b'T'
        && bytes
            .iter()
            .enumerate()
            .all(|(at, b)| at == 8 || b.is_ascii_digit());
    if !stamp_ok || nanos.is_empty() || !nanos.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((stamp, nanos.parse().ok()?))
}

/// Keep only the newest `KEEP_BACKUPS` of the installer's backup folders.
/// Only folders named as the installer names them are counted or removed.
/// Returns how many were removed.
pub fn prune_backups(p: &Paths) -> Result<usize> {
    let backups = fsutil::join(&p.destination, "backups");
    if !managed_destination(p)
        || !fsutil::lstat(&backups).is_ok_and(|info| fsutil::is_type(&info, fsutil::S_IFDIR))
    {
        return Ok(0);
    }
    let entries = std::fs::read_dir(&backups)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&backups))))?;
    let mut folders: Vec<(String, u128, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = fsutil::join(&backups, &name);
            let (stamp, nanos) = backup_name(&name)?;
            fsutil::lstat(&path)
                .is_ok_and(|info| fsutil::is_type(&info, fsutil::S_IFDIR))
                .then(|| (stamp.to_string(), nanos, path))
        })
        .collect();
    // Oldest first, by the nanoseconds since the epoch in each name (the
    // local time beside them can go back with the clock or the zone).
    folders.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    let surplus = folders.len().saturating_sub(KEEP_BACKUPS);
    for (_, _, path) in &folders[..surplus] {
        std::fs::remove_dir_all(path)
            .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    }
    Ok(surplus)
}

/// Whether Claude Code has a Switchboard plugin that only knows
/// `~/bin/switchboard` (before `PLUGIN_FINDS_LOCAL_BIN`): `Some(true)` when
/// it lists one, `Some(false)` when it lists none or isn't on PATH (init
/// finds the plugin the same way), `None` when it can't answer (it may have
/// one).
fn plugin_needs_old_command() -> Option<bool> {
    let Some(claude) = super::init::plugin::claude_path() else {
        return Some(false);
    };
    super::init::plugin::installed_before(&claude, PLUGIN_FINDS_LOCAL_BIN).ok()
}

/// Make `~/bin/switchboard` a link to the program, replacing the wrapper
/// there in one step.
fn link_old_command(p: &Paths) -> Result<()> {
    let temporary = fsutil::join(
        &fsutil::parent(&p.old_command),
        &format!(".switchboard-link-{}", std::process::id()),
    );
    let _ = fsutil::unlink(&temporary);
    std::os::unix::fs::symlink(&p.program, &temporary)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&temporary))))?;
    fsutil::replace(&temporary, &p.old_command).inspect_err(|_| {
        let _ = fsutil::unlink(&temporary);
    })
}

/// Move what releases before 0.3.1 left, once the program is in place and
/// the service runs it: the old command (removed, or a link to the program
/// while an older plugin needs it), the old link at the program's path is
/// already replaced, the old program copy (unless a service still runs
/// it), the Python modules, and backups beyond the newest few. Failures
/// here never undo the install; they are reported.
fn migrate(p: &Paths, result: &mut Map<String, Value>) {
    let mut removed: Vec<Value> = Vec::new();
    let mut problems: Vec<Value> = Vec::new();
    let mut note = |what: &str, error: Error| {
        problems.push(Value::from(format!("{what}: {}", error.message)));
    };
    let state = old_command_state(p);
    let needed = if matches!(state, Found::Ours | Found::OurLink) {
        plugin_needs_old_command()
    } else {
        Some(false)
    };
    match state {
        _ if same_file(&p.old_command, &p.program) => {}
        Found::Ours | Found::OurLink if needed != Some(false) => {
            if state == Found::Ours {
                match link_old_command(p) {
                    Ok(()) => {}
                    Err(error) => note(&p.old_command, error),
                }
            }
            result.insert("plugin_link".into(), Value::from(p.old_command.clone()));
            if needed.is_none() {
                // Claude Code couldn't say which plugin it has.
                result.insert("plugin_unknown".into(), Value::Bool(true));
            }
        }
        Found::Ours | Found::OurLink => match fsutil::unlink(&p.old_command) {
            Ok(()) => removed.push(Value::from(p.old_command.clone())),
            Err(error) => note(&p.old_command, error),
        },
        Found::Foreign => {
            result.insert("left".into(), Value::from(p.old_command.clone()));
        }
        Found::Absent => {}
    }
    if fsutil::lstat(&p.old_program).is_ok() {
        if super::service::running() {
            result.insert(
                "old_program_kept".into(),
                Value::from(p.old_program.clone()),
            );
        } else {
            match remove_old_program(p) {
                Ok(true) => removed.push(Value::from(p.old_program.clone())),
                Ok(false) => {}
                Err(error) => note(&p.old_program, error),
            }
        }
    }
    match remove_python_files(p) {
        Ok(paths) => removed.extend(paths.into_iter().map(Value::from)),
        Err(error) => note("the Python release's files", error),
    }
    match prune_backups(p) {
        Ok(0) => {}
        Ok(count) => {
            result.insert("pruned_backups".into(), Value::from(count));
        }
        Err(error) => note("old backups", error),
    }
    if !removed.is_empty() {
        result.insert("removed".into(), Value::Array(removed));
    }
    if !problems.is_empty() {
        result.insert("cleanup_failed".into(), Value::Array(problems));
    }
}

/// Install `binary_source` under `home`. When a bridge service installed
/// under an earlier name was running, it is stopped and the result carries
/// `"restart": true` for the caller to start the new service.
pub fn install(home: &str, binary_source: &str) -> Result<Map<String, Value>> {
    let home = fsutil::absolute(home)?;
    let p = paths(&home);
    let legacy = legacy_paths(&home);
    // Only a legacy service file this installer wrote is moved; anything
    // else under that name is left alone.
    let migrate_service = fsutil::lstat(&legacy.service).is_ok_and(|info| info.is_file())
        && managed_service(&legacy.service).unwrap_or(false);
    for target in [&p.destination, &p.service] {
        if fsutil::is_symlink(target) {
            return Err(Error::value(format!("refusing symlink: {target}")));
        }
    }
    if fsutil::exists(&p.destination)
        && !fsutil::stat(&fsutil::join(&p.destination, ".switchboard-install"))
            .is_ok_and(|m| m.is_file())
    {
        return Err(Error::value(format!(
            "unmanaged source directory: {}",
            p.destination
        )));
    }
    if program_state(&p) == Found::Foreign {
        return Err(Error::value(format!(
            "unmanaged command: {} (Switchboard didn't write it; move it aside, then install again)",
            p.program
        )));
    }
    if fsutil::exists(&p.service) && !managed_service(&p.service)? {
        let what = if cfg!(target_os = "linux") {
            "unmanaged user service"
        } else {
            "unmanaged launch agent"
        };
        return Err(Error::value(format!("{what}: {}", p.service)));
    }
    if let Some(wants) = &p.wants {
        if fsutil::lstat(wants).is_ok() && !links_to(wants, &p.service) {
            return Err(Error::value(format!("unmanaged user service: {wants}")));
        }
    }
    for name in ["daemon.log", "daemon-error.log"] {
        let path = fsutil::join(&p.destination, name);
        if let Ok(info) = fsutil::lstat(&path) {
            if !fsutil::is_type(&info, fsutil::S_IFREG) || info.uid() != fsutil::getuid() {
                return Err(Error::value(format!("unsafe daemon log: {path}")));
            }
        }
    }
    if !fsutil::stat(binary_source).is_ok_and(|m| m.is_file()) {
        return Err(Error::value(format!("missing runtime source: {BINARY}")));
    }
    let mut directories = vec![
        p.destination.clone(),
        fsutil::parent(&p.program),
        fsutil::parent(&p.service),
        fsutil::parent(&p.config),
    ];
    if let Some(wants) = &p.wants {
        directories.push(fsutil::parent(wants));
    }
    for directory in &directories {
        fsutil::mkdir_parents(directory, 0o700)?;
        let info = fsutil::lstat(directory)?;
        if !fsutil::is_type(&info, fsutil::S_IFDIR)
            || info.uid() != fsutil::getuid()
            || info.mode() & 0o022 != 0
        {
            return Err(Error::value(format!(
                "unsafe install directory: {directory}"
            )));
        }
    }
    let backups = fsutil::join(
        &p.destination,
        &format!("backups/{}-{}", local_compact_stamp(), time_ns()),
    );
    // Each file is written beside its place and renamed over it, so a
    // running bridge keeps the program it started with until it restarts.
    let write = |path: &str, content: &[u8], mode: u32| -> Result<()> {
        if fsutil::exists(path) {
            fsutil::mkdir_parents(&backups, 0o700)?;
            copy2(path, &fsutil::join(&backups, &crate::fileops::name(path)))?;
        }
        let (mut file, temporary) =
            crate::fileops::mkstemp(&fsutil::parent(path), ".switchboard-", "")?;
        let result = (|| {
            use std::io::Write;
            file.write_all(content)
                .map_err(|exc| Error::from_io(&exc, None))?;
            file.flush().map_err(|exc| Error::from_io(&exc, None))?;
            file.sync_all().map_err(|exc| Error::from_io(&exc, None))?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))
                .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(&temporary))))?;
            fsutil::replace(&temporary, path)
        })();
        if fsutil::lstat(&temporary).is_ok() {
            let _ = fsutil::unlink(&temporary);
        }
        result
    };
    let binary = std::fs::read(binary_source)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(binary_source))))?;
    write(&p.program, &binary, 0o700)?;
    write(
        &fsutil::join(&p.destination, ".switchboard-install"),
        b"1\n",
        0o600,
    )?;
    // The service manager opens these before applying the child's umask.
    // Preserve existing contents and inode, even while a daemon writes them.
    for name in ["daemon.log", "daemon-error.log"] {
        let path = fsutil::join(&p.destination, name);
        let existed = fsutil::exists(&path);
        let fd = open_log(&path)?;
        let info = std::fs::File::from(fd);
        let meta = info.metadata().map_err(|exc| Error::from_io(&exc, None))?;
        if !fsutil::is_type(&meta, fsutil::S_IFREG) || meta.uid() != fsutil::getuid() {
            return Err(Error::value(format!("unsafe daemon log: {path}")));
        }
        if existed && meta.mode() & 0o7777 != 0o600 {
            fsutil::mkdir_parents(&backups, 0o700)?;
            copy2(&path, &fsutil::join(&backups, name))?;
            std::fs::set_permissions(
                fsutil::join(&backups, name),
                std::fs::Permissions::from_mode(0o600),
            )
            .map_err(|exc| Error::from_io(&exc, None))?;
        }
        info.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|exc| Error::from_io(&exc, None))?;
    }
    // The old service is stopped and removed before the new one is written,
    // so the two are never installed together (both would load at the next
    // login). If it cannot be retired, the new service file is not left
    // installed, and the error says what state the machine is in.
    let mut restart = false;
    if migrate_service {
        restart = retire_legacy(&legacy, &backups).map_err(|failure| {
            let withdrawn = withdraw_service(&p, &backups);
            Error::value(format!("{failure}{withdrawn}"))
        })?;
    }
    let service = if cfg!(target_os = "linux") {
        systemd_unit(&p.program, &p.destination, &p.config)
    } else {
        launch_agent(&p.program, &p.destination, &p.config)
    };
    // A machine taken down stays down: its service isn't enabled again.
    let down = super::service::is_down(&p.config);
    let placed = write(&p.service, service.as_bytes(), 0o600).and_then(|()| {
        if let Some(wants) = &p.wants {
            if fsutil::lstat(wants).is_err() && !down {
                std::os::unix::fs::symlink(&p.service, wants)
                    .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(wants))))?;
            }
        }
        Ok(())
    });
    if let Err(error) = placed {
        if !migrate_service {
            return Err(error);
        }
        return Err(Error::value(format!(
            "the bridge service installed under its old name ({}) was removed (a copy is in {backups}), \
             but the new service could not be installed: {}; no bridge service is installed now. \
             Run switchboard install again",
            legacy.service, error.message
        )));
    }
    let mut result = Map::new();
    result.insert("command".into(), Value::from(p.program.clone()));
    // The program's directory: releases before 0.3.1 restart the bridge
    // after an upgrade with `source`/switchboard.
    result.insert("source".into(), Value::from(fsutil::parent(&p.program)));
    result.insert("data".into(), Value::from(p.destination.clone()));
    result.insert("config".into(), Value::from(p.config.clone()));
    let key = if cfg!(target_os = "linux") {
        "user_service"
    } else {
        "launch_agent"
    };
    result.insert(key.into(), Value::from(p.service.clone()));
    if migrate_service {
        result.insert("migrated_from".into(), Value::from(legacy.service.clone()));
    }
    migrate(&p, &mut result);
    result.insert("started".into(), Value::Bool(false));
    if down {
        result.insert("down".into(), Value::Bool(true));
    } else if restart {
        result.insert("restart".into(), Value::Bool(true));
    }
    Ok(result)
}

/// The command that stops the old service by hand.
fn legacy_stop_command() -> String {
    if cfg!(target_os = "linux") {
        format!("systemctl --user stop {}", super::service::LEGACY_UNIT)
    } else {
        format!(
            "launchctl bootout gui/{}/{}",
            fsutil::getuid(),
            super::service::LEGACY_LABEL
        )
    }
}

/// Stop the bridge service installed under its old name (if it runs) and
/// remove its files, keeping a copy in `backups`. Returns whether it was
/// running. A failure is described in full: what was done, what is left,
/// and how to finish.
fn retire_legacy(legacy: &Legacy, backups: &str) -> std::result::Result<bool, String> {
    let running = super::service::stop_legacy().map_err(|error| {
        format!(
            "could not stop the bridge service installed under its old name ({}): {}. \
             The program was updated, but the service was not moved: the old service file is \
             still installed and the new one is not, so only the old one loads at login. \
             Stop it with {}, then run switchboard install again.",
            legacy.service,
            error.message,
            legacy_stop_command()
        )
    })?;
    let removed = (|| -> Result<()> {
        fsutil::mkdir_parents(backups, 0o700)?;
        copy2(
            &legacy.service,
            &fsutil::join(backups, &crate::fileops::name(&legacy.service)),
        )?;
        if let Some(wants) = &legacy.wants {
            if links_to(wants, &legacy.service) {
                fsutil::unlink(wants)?;
            }
        }
        fsutil::unlink(&legacy.service)
    })();
    super::service::reload_units();
    removed.map_err(|error| {
        let now = if running {
            "it was stopped, so the bridge is not running now"
        } else {
            "it was not running"
        };
        format!(
            "could not remove the bridge service installed under its old name ({}): {}. \
             The program was updated and {now}; the old service file is still installed \
             and the new one is not, so only the old one loads at login. Remove {} \
             yourself, then run switchboard install again.",
            legacy.service, error.message, legacy.service
        )
    })?;
    Ok(running)
}

/// After a failed move, take out a current-name service file an earlier
/// run left beside the old one (keeping a copy in `backups`), so the two are
/// never installed together. Returns a sentence for the error.
fn withdraw_service(p: &Paths, backups: &str) -> String {
    if fsutil::lstat(&p.service).is_err() {
        return String::new();
    }
    let removed = (|| -> Result<()> {
        fsutil::mkdir_parents(backups, 0o700)?;
        copy2(
            &p.service,
            &fsutil::join(backups, &crate::fileops::name(&p.service)),
        )?;
        if let Some(wants) = &p.wants {
            if links_to(wants, &p.service) {
                fsutil::unlink(wants)?;
            }
        }
        fsutil::unlink(&p.service)
    })();
    super::service::reload_units();
    match removed {
        Ok(()) => format!(
            " A new service file an earlier run left beside it ({}) was removed (a copy is in {backups}).",
            p.service
        ),
        Err(error) => format!(
            " A new service file an earlier run left beside it ({}) could not be removed ({}), \
             so both are installed: remove {} too before logging in again.",
            p.service, error.message, p.service
        ),
    }
}

/// Remove the bridge's service files under `home` (the unit and its
/// enabling link, or the launch agent), if this installer wrote them, so
/// nothing starts the bridge at the next login. The service should already
/// be stopped. Returns whether a file was removed.
pub fn remove_service(home: &str) -> Result<bool> {
    let p = paths(home);
    let mut removed = false;
    if let Some(wants) = &p.wants {
        if links_to(wants, &p.service) {
            fsutil::unlink(wants)?;
            removed = true;
        }
    }
    if fsutil::lstat(&p.service).is_ok() && managed_service(&p.service).unwrap_or(false) {
        fsutil::unlink(&p.service)?;
        removed = true;
    }
    super::service::reload_units();
    Ok(removed)
}

fn open_log(path: &str) -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(path).map_err(|_| Error::value("embedded null byte"))?;
    // SAFETY: open(2) with a valid path; the descriptor is owned below.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_CREAT | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(Error::errno_path(
            Error::last_os(),
            std::path::Path::new(path),
        ));
    }
    // SAFETY: fd was just opened.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Copy a file with its contents, mode, and times.
fn copy2(source: &str, target: &str) -> Result<()> {
    std::fs::copy(source, target)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(source))))?;
    if let Ok(meta) = std::fs::metadata(source) {
        // See `fmtutil::localtime` on the deprecated musl `time_t` alias.
        #[allow(deprecated)]
        let times = [
            libc::timespec {
                tv_sec: meta.atime() as libc::time_t,
                tv_nsec: meta.atime_nsec() as _,
            },
            libc::timespec {
                tv_sec: meta.mtime() as libc::time_t,
                tv_nsec: meta.mtime_nsec() as _,
            },
        ];
        if let Ok(c) = std::ffi::CString::new(target) {
            // SAFETY: utimensat with a valid path and two timespecs.
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        }
    }
    Ok(())
}

/// The running binary's path.
pub fn current_binary() -> Result<String> {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|exc| Error::from_io(&exc, None))
}

const UNSUPPORTED: &str =
    "installing needs launchd (macOS) or systemd (Linux); run switchboard bridge under your own supervisor instead";

/// Install the running binary under `home`.
pub fn install_current(home: &str) -> Result<Map<String, Value>> {
    if !super::service::supported() {
        return Err(Error::value(UNSUPPORTED));
    }
    install(home, &current_binary()?)
}

const USAGE: &str = "Usage: switchboard install [--start] [--json]";

const HELP: &str = "Usage: switchboard install [--start] [--json]

Install this binary for the current user as the switchboard command,
~/.local/bin/switchboard, and install the bridge's user service (launchd on
macOS, systemd on Linux); the bridge's logs go in ~/.local/share/switchboard.
Running it again upgrades in place. An installation from before 0.3.1 is
moved: the ~/bin/switchboard command it wrote and its old program copy are
removed, and so are files the Python releases left. A bridge service an
earlier version installed under its old name is moved to the current one,
and restarted if it was running. It never joins chats or changes Claude Code
settings.

Options:
  --start     start the bridge afterwards
  --json      print the result as JSON
  -h, --help  show this help
";

fn fail(json: bool, message: &str) -> i32 {
    if json {
        let mut value = Map::new();
        value.insert("ok".into(), Value::Bool(false));
        value.insert("error".into(), Value::from(message));
        super::out(&format!(
            "{}\n",
            crate::wirejson::dumps(&Value::Object(value))
        ));
    } else {
        super::err(&format!("switchboard install: {message}\n"));
    }
    1
}

/// A path under the home directory, written with `~`.
pub fn tilde(path: &str) -> String {
    let home = super::local::home_dir();
    match path.strip_prefix(&home) {
        Some(rest) if rest.starts_with('/') && !home.is_empty() && home != "/" => {
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

/// `switchboard install [--start] [--json]`.
pub fn main(argv: &[String]) -> i32 {
    let mut start = false;
    let mut json = false;
    for argument in argv {
        match argument.as_str() {
            "--start" => start = true,
            "--json" => json = true,
            "-h" | "--help" => {
                super::out(HELP);
                return 0;
            }
            other => {
                let what = if other.starts_with('-') {
                    "unknown option"
                } else {
                    "unexpected argument"
                };
                super::err(&format!("switchboard install: {what} {other}\n{USAGE}\n"));
                return 2;
            }
        }
    }
    if !super::service::supported() {
        return fail(json, UNSUPPORTED);
    }
    let home = super::local::home_dir();
    let mut result = match install_current(&home) {
        Ok(result) => result,
        Err(error) => return fail(json, &error.message),
    };
    let restart = result.remove("restart") == Some(Value::Bool(true));
    let down = result.get("down") == Some(&Value::Bool(true));
    if (start || restart) && !down {
        let config = result
            .get("config")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Err(error) = super::service::service("start", config.as_deref()) {
            return fail(
                json,
                &format!("installed, but the bridge did not start: {}", error.message),
            );
        }
        result.insert("started".into(), Value::Bool(true));
    }
    if json {
        super::out(&format!(
            "{}\n",
            crate::fmtutil::dumps_indent(&Value::Object(result), 2, false)
        ));
        return 0;
    }
    let text = |key: &str| tilde(result.get(key).and_then(Value::as_str).unwrap_or(""));
    let service_key = if cfg!(target_os = "linux") {
        "user_service"
    } else {
        "launch_agent"
    };
    let mut lines = vec![format!(
        "Installed switchboard {} as {}.",
        crate::VERSION,
        text("command")
    )];
    let started = result.get("started") == Some(&Value::Bool(true));
    // An install over a running bridge leaves it running; say so instead of
    // suggesting it be started.
    let running = !started && !down && super::service::running();
    lines.push(format!(
        "Bridge service: {} ({})",
        text(service_key),
        if started {
            "started"
        } else if running {
            "running"
        } else {
            "not started"
        }
    ));
    if result.contains_key("migrated_from") {
        lines.push(format!(
            "Moved the bridge service from its old name ({}).",
            text("migrated_from")
        ));
    }
    lines.extend(migration_lines(&result));
    if down {
        lines.push(
            "This machine is down, so the bridge wasn't started. To bring it back online: switchboard up"
                .into(),
        );
    } else if !started && !running {
        let config = result.get("config").and_then(Value::as_str);
        if crate::fsutil::exists(config.unwrap_or_default()) {
            lines.push("Start the bridge with: switchboard up".into());
        } else {
            lines.push("Next, set up this machine with: switchboard init".into());
        }
    }
    super::out(&format!("{}\n", lines.join("\n")));
    0
}

/// What `install` says about moving an earlier installation.
pub fn migration_lines(result: &Map<String, Value>) -> Vec<String> {
    let text = |key: &str| tilde(result.get(key).and_then(Value::as_str).unwrap_or(""));
    let mut lines = Vec::new();
    if let Some(Value::Array(removed)) = result.get("removed") {
        lines.push("Removed what earlier versions left:".into());
        lines.extend(
            removed
                .iter()
                .filter_map(Value::as_str)
                .map(|path| format!("    {}", tilde(path))),
        );
    }
    if let Some(count) = result.get("pruned_backups").and_then(Value::as_u64) {
        lines.push(format!(
            "Removed {count} older installer backup{} (the newest {KEEP_BACKUPS} stay).",
            if count == 1 { "" } else { "s" }
        ));
    }
    if result.contains_key("plugin_link") {
        let why = if result.contains_key("plugin_unknown") {
            format!("Claude Code couldn't say which Switchboard plugin it has, and one older than {PLUGIN_FINDS_LOCAL_BIN} runs that path")
        } else {
            format!("the Claude Code plugin installed here is older than {PLUGIN_FINDS_LOCAL_BIN} and runs that path")
        };
        lines.push(format!(
            "Kept {} as a link to the program: {why}. Update the plugin (claude plugin marketplace update switchboard), and the next upgrade removes the link.",
            text("plugin_link")
        ));
    }
    if result.contains_key("old_program_kept") {
        lines.push(format!(
            "The running bridge still uses {}; it is removed when the bridge next starts (switchboard down, then switchboard up, restarts it).",
            text("old_program_kept")
        ));
    }
    if result.contains_key("left") {
        lines.push(format!(
            "Left {} alone: Switchboard didn't write it. If it comes first on your PATH, it runs instead of switchboard.",
            text("left")
        ));
    }
    if let Some(Value::Array(failed)) = result.get("cleanup_failed") {
        for failure in failed.iter().filter_map(Value::as_str) {
            lines.push(format!("Couldn't remove {failure}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_escapes_specifiers_in_every_path() {
        let unit = systemd_unit(
            "/home/a%b/.local/share/switchboard/switchboard",
            "/home/a%b/.local/share/switchboard",
            "/home/a%b/.config/switchboard/bridge.json",
        );
        assert!(unit.contains(
            "ExecStart=\"/home/a%%b/.local/share/switchboard/switchboard\" bridge --config \"/home/a%%b/.config/switchboard/bridge.json\"\n"
        ));
        assert!(unit.contains("WorkingDirectory=\"/home/a%%b/.local/share/switchboard\"\n"));
        assert!(
            unit.contains("StandardOutput=append:/home/a%%b/.local/share/switchboard/daemon.log\n")
        );
        assert!(unit.contains(
            "StandardError=append:/home/a%%b/.local/share/switchboard/daemon-error.log\n"
        ));
        assert!(!unit.replace("%%", "").contains('%'));
        assert!(unit.contains("ConditionPathExists=/home/a%%b/.config/switchboard/bridge.json\n"));
    }

    #[test]
    fn the_service_runs_only_while_its_configuration_exists() {
        let unit = systemd_unit("/h/s/switchboard", "/h/s", "/h/c/bridge.json");
        let unit_section = &unit[unit.find("[Unit]").unwrap()..unit.find("[Service]").unwrap()];
        assert!(unit_section.contains("\nConditionPathExists=/h/c/bridge.json\n"));
        assert!(unit.contains("\nRestart=always\nRestartSec=10\n"));
        let agent = launch_agent("/h/s/switchboard", "/h/s", "/h/c&d/bridge.json");
        assert!(agent.contains(
            "\t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>PathState</key>\n\t\t<dict>\n\t\t\t<key>/h/c&amp;d/bridge.json</key>\n\t\t\t<true/>\n\t\t</dict>\n\t</dict>\n"
        ));
        assert!(!agent.contains("<key>KeepAlive</key>\n\t<true/>"));
    }
}
