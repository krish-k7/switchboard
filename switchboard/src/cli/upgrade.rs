//! `upgrade` and `uninstall`.
//!
//! `upgrade [VERSION]` downloads the latest release (or the one named),
//! checks it against the release's `SHA256SUMS`, and runs the new binary's
//! own installer, so whatever that version needs to move from older
//! installations happens as it would with `install.sh`. A bridge that was
//! running is restarted on the new binary. Moving to an older version needs
//! `--force`; the current service is removed first, so the older installer
//! can set up its own without two services running the bridge.
//!
//! `uninstall` stops the bridge and removes the service and the program
//! (`~/.local/bin/switchboard`, and what releases before 0.3.1 installed in
//! its place, when an installer wrote it). The configuration, this
//! machine's key, chat membership, and the bridge's logs stay unless
//! `--purge` is given.

use serde_json::{Map, Value};

use super::install::{self, legacy_paths, links_to, managed_service, paths, tilde, Found};
use super::machines::run_capture;
use super::release::{self, Source, Version};
use super::service;
use crate::error::{Error, Result};
use crate::fsutil;

fn fail(message: impl Into<String>) -> Error {
    Error::value(message)
}

fn version_of(binary: &str) -> Result<String> {
    let (code, stdout, _) = run_capture(&[binary.to_string(), "--version".into()], 30.0, true)
        .map_err(|error| {
            fail(format!(
                "the downloaded binary does not run on this machine ({})",
                error.message
            ))
        })?;
    match stdout.trim().strip_prefix("switchboard ") {
        Some(version) if code == 0 => Ok(version.to_string()),
        _ => Err(fail("the downloaded binary does not run on this machine")),
    }
}

/// A private temporary directory, removed on drop.
struct TempDir(String);

impl TempDir {
    fn new() -> Result<TempDir> {
        let base = std::env::temp_dir();
        let template = format!("{}/switchboard-upgrade-XXXXXX", base.display());
        let mut raw = std::ffi::CString::new(template)
            .map_err(|_| fail("invalid temporary directory"))?
            .into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        let made = unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) };
        if made.is_null() {
            return Err(Error::errno(Error::last_os()));
        }
        raw.pop();
        Ok(TempDir(
            String::from_utf8(raw).map_err(|_| fail("invalid temporary directory"))?,
        ))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_executable(path: &str, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))
}

/// A child's error: the JSON `error` it printed, else its stderr.
fn child_error(stdout: &str, stderr: &str) -> String {
    if let Ok(Value::Object(value)) = serde_json::from_str::<Value>(stdout.trim()) {
        if let Some(Value::String(error)) = value.get("error") {
            return error.clone();
        }
    }
    let text = stderr.trim();
    let text = text.strip_prefix("switchboard install: ").unwrap_or(text);
    if text.is_empty() {
        "it exited with an error".into()
    } else {
        text.to_string()
    }
}

/// `switchboard upgrade [VERSION] [--force]`.
pub fn upgrade(
    wanted: Option<&str>,
    force: bool,
    say: &dyn Fn(&str),
) -> Result<Map<String, Value>> {
    if !service::supported() {
        return Err(fail(
            "upgrading needs launchd (macOS) or systemd (Linux); download the new binary yourself",
        ));
    }
    let current = crate::VERSION.to_string();
    let source = Source::from_env()?;
    let tag = match wanted {
        Some(version) => {
            let tag = if version.starts_with('v') {
                version.to_string()
            } else {
                format!("v{version}")
            };
            if Version::parse(&tag).is_none() {
                return Err(fail(format!(
                    "there's no release version called {version} (versions look like 0.2.0)"
                )));
            }
            tag
        }
        None => source.latest_tag()?,
    };
    let target = tag.strip_prefix('v').unwrap_or(&tag).to_string();
    let (Some(new), Some(old)) = (Version::parse(&target), Version::parse(&current)) else {
        return Err(fail(format!(
            "cannot compare versions {target} and {current}"
        )));
    };
    let mut result = Map::new();
    result.insert("ok".into(), Value::Bool(true));
    result.insert("action".into(), Value::from("upgrade"));
    result.insert("previous".into(), Value::from(current.clone()));
    result.insert("version".into(), Value::from(target.clone()));
    if new == old && !force {
        result.insert("changed".into(), Value::Bool(false));
        return Ok(result);
    }
    let downgrade = new < old;
    if downgrade && !force {
        return Err(fail(format!(
            "{target} is older than the installed {current}; pass --force to downgrade"
        )));
    }
    let asset = release::asset_name()?;
    say(&format!("  Downloading Switchboard {target} ({asset})..."));
    let files = source.download(&tag, &[&asset, "SHA256SUMS"])?;
    release::verify(&files[1], &asset, &files[0])?;
    let temp = TempDir::new()?;
    let binary = fsutil::join(&temp.0, &asset);
    write_executable(&binary, &files[0])?;
    let reported = version_of(&binary)?;
    if reported != target {
        return Err(fail(format!(
            "the downloaded binary reports version {reported}, not {target}; nothing was installed"
        )));
    }
    let home = super::local::home_dir();
    let was_running = service::running();
    // A copy of this binary, to put back if an older installer fails after
    // the current service was removed for it.
    let backup = fsutil::join(&temp.0, "current");
    if downgrade {
        let current =
            std::fs::read(install::current_binary()?).map_err(|exc| Error::from_io(&exc, None))?;
        write_executable(&backup, &current)?;
        if was_running {
            service::stop_current()?;
        }
        install::remove_service(&home).map(|_| ())?;
        // Releases before 0.3.1 refuse a program at ~/.local/bin/switchboard
        // (they link their ~/bin command there) and a link at
        // ~/bin/switchboard; both are this installation's, and the copy above
        // puts them back if the older installer fails.
        if Version::parse(install::LOCAL_BIN_SINCE).is_some_and(|first| new < first) {
            let p = paths(&home);
            if install::program_state(&p) == Found::Ours {
                fsutil::unlink(&p.program)?;
            }
            if install::old_command_state(&p) == Found::OurLink {
                fsutil::unlink(&p.old_command)?;
            }
        }
    }
    let (code, stdout, stderr) = run_capture(
        &[binary.clone(), "install".into(), "--json".into()],
        300.0,
        true,
    )?;
    if code != 0 {
        let reason = child_error(&stdout, &stderr);
        if downgrade {
            // The current service was removed for the older installer; put
            // this version back as it was.
            let restored = install::install(&home, &backup).is_ok()
                && (!was_running || service::service("start", None).is_ok());
            return Err(fail(format!(
                "the {target} installer failed ({reason}); {}",
                if restored {
                    "this version is still installed"
                } else {
                    "run switchboard install to restore this version"
                }
            )));
        }
        return Err(fail(format!("the {target} installer failed: {reason}")));
    }
    let installed: Map<String, Value> = match serde_json::from_str(stdout.trim()) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut restarted = installed.get("started") == Some(&Value::Bool(true));
    if was_running && !restarted {
        // The installer says where it put the program (`source`, its
        // directory); without that, where that version puts it.
        let p = paths(&home);
        let program = installed
            .get("source")
            .and_then(Value::as_str)
            .map(|dir| fsutil::join(dir, install::BINARY))
            .unwrap_or(
                if Version::parse(install::LOCAL_BIN_SINCE).is_some_and(|first| new < first) {
                    p.old_program
                } else {
                    p.program
                },
            );
        for action in ["stop", "start"] {
            let (code, stdout, stderr) = run_capture(
                &[
                    program.clone(),
                    "cli".into(),
                    action.into(),
                    "--json".into(),
                ],
                60.0,
                true,
            )?;
            if code != 0 {
                return Err(fail(format!(
                    "installed {target}, but the bridge did not restart ({}); run switchboard up",
                    child_error(&stdout, &stderr)
                )));
            }
        }
        restarted = true;
    }
    result.insert("changed".into(), Value::Bool(true));
    result.insert("restarted".into(), Value::Bool(restarted));
    if service::default_is_down() {
        result.insert("down".into(), Value::Bool(true));
    }
    Ok(result)
}

/// The human lines for an upgrade result.
pub fn upgrade_lines(result: &Map<String, Value>, pinned: bool) -> Vec<String> {
    let text = |key: &str| {
        result
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let theme = super::ui::Theme::for_fd(1);
    let width = super::format::output_width();
    let done = |value: &str| super::ui::done(&theme, value, width);
    if result.get("changed") != Some(&Value::Bool(true)) {
        return done(&if pinned {
            format!("Switchboard {} is already installed", text("version"))
        } else {
            format!("Switchboard {} is the latest release", text("version"))
        });
    }
    let mut lines = done(&format!(
        "Installed Switchboard {} (was {})",
        text("version"),
        text("previous")
    ));
    if result.get("restarted") == Some(&Value::Bool(true)) {
        lines.extend(done("Restarted the bridge on the new version"));
    } else if result.get("down") == Some(&Value::Bool(true)) {
        lines.extend(super::ui::para(
            &theme,
            "This machine is down, so it stays down. To bring it back online, run\n`switchboard up`",
            super::ui::Tone::Normal,
            width,
        ));
    } else {
        lines.extend(super::ui::item(
            &theme,
            super::ui::Mark::Warned,
            "The bridge isn't running. Start it with\n`switchboard up`",
            super::ui::Tone::Normal,
            width,
        ));
    }
    lines
}

fn remove(path: &str, removed: &mut Vec<String>) -> Result<()> {
    match fsutil::lstat(path) {
        Ok(info) if info.is_dir() => std::fs::remove_dir_all(path)
            .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?,
        Ok(_) => fsutil::unlink(path)?,
        Err(_) => return Ok(()),
    }
    removed.push(path.to_string());
    Ok(())
}

/// `switchboard uninstall [--purge]`.
pub fn uninstall(purge: bool) -> Result<Map<String, Value>> {
    if !service::supported() {
        return Err(fail(
            "uninstalling needs launchd (macOS) or systemd (Linux); remove the files yourself",
        ));
    }
    let home = fsutil::absolute(&super::local::home_dir())?;
    let p = paths(&home);
    let legacy = legacy_paths(&home);
    // Stop the bridge under either name; a service that cannot be stopped
    // keeps its files.
    let stopped = |error: Error| {
        fail(format!(
            "could not stop the bridge ({}); nothing was removed",
            error.message
        ))
    };
    // The name this machine has on the server, to say how to remove it there.
    let machine = super::init::join::existing_identity(&p.config)
        .map(|(machine, _, _)| machine)
        .filter(|m| !m.is_empty());
    let stopped_now = service::stop_current().map_err(stopped)?;
    let stopped_legacy = service::stop_legacy().map_err(stopped)?;
    let mut removed = Vec::new();
    for (service_file, wants) in [(&p.service, &p.wants), (&legacy.service, &legacy.wants)] {
        if let Some(wants) = wants {
            if links_to(wants, service_file) {
                remove(wants, &mut removed)?;
            }
        }
        if fsutil::lstat(service_file).is_ok_and(|info| info.is_file())
            && managed_service(service_file).unwrap_or(false)
        {
            remove(service_file, &mut removed)?;
        }
    }
    service::reload_units();
    // The command, and the command and link releases before 0.3.1 wrote;
    // never a file of the same name that an installer didn't write.
    if matches!(install::old_command_state(&p), Found::Ours | Found::OurLink) {
        remove(&p.old_command, &mut removed)?;
    }
    if matches!(install::program_state(&p), Found::Ours | Found::OurLink) {
        remove(&p.program, &mut removed)?;
    }
    let marker = fsutil::join(&p.destination, ".switchboard-install");
    let managed = fsutil::lstat(&marker).is_ok_and(|info| info.is_file())
        && fsutil::lstat(&p.destination).is_ok_and(|info| info.is_dir());
    if managed {
        if purge {
            remove(&p.destination, &mut removed)?;
        } else {
            // The marker stays with the logs, so a later install still
            // recognizes the directory as its own.
            for name in [install::BINARY, "backups"] {
                remove(&fsutil::join(&p.destination, name), &mut removed)?;
            }
            removed.extend(install::remove_python_files(&p)?);
        }
    }
    let config_dir = fsutil::join(&home, ".config/switchboard");
    if purge && fsutil::lstat(&config_dir).is_ok_and(|info| info.is_dir()) {
        remove(&config_dir, &mut removed)?;
        // The setup that was down is gone: a new one may start the bridge.
        let _ = service::forget_down();
    }
    // Directories left with nothing of the user's in them go too.
    let entries = |path: &str| -> Option<Vec<String>> {
        let names = std::fs::read_dir(path).ok()?;
        Some(
            names
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
        )
    };
    // The installer creates the logs empty; empty ones hold nothing.
    let disposable = |name: &String| {
        name == ".switchboard-install"
            || ((name == "daemon.log" || name == "daemon-error.log")
                && fsutil::lstat(&fsutil::join(&p.destination, name))
                    .is_ok_and(|info| info.is_file() && info.len() == 0))
    };
    if managed && entries(&p.destination).is_some_and(|names| names.iter().all(disposable)) {
        remove(&p.destination, &mut removed)?;
    }
    if entries(&config_dir).is_some_and(|names| names.is_empty())
        && std::fs::remove_dir(&config_dir).is_ok()
    {
        removed.push(config_dir.clone());
    }
    let mut kept = Vec::new();
    for path in [&config_dir, &p.destination] {
        if fsutil::lstat(path).is_ok() {
            kept.push(Value::from(path.clone()));
        }
    }
    let mut result = Map::new();
    result.insert("ok".into(), Value::Bool(true));
    result.insert("action".into(), Value::from("uninstall"));
    result.insert("stopped".into(), Value::Bool(stopped_now || stopped_legacy));
    result.insert(
        "removed".into(),
        Value::Array(removed.into_iter().map(Value::from).collect()),
    );
    result.insert("kept".into(), Value::Array(kept));
    if let Some(machine) = machine {
        result.insert("machine".into(), Value::from(machine));
    }
    Ok(result)
}

/// The human lines for an uninstall result.
pub fn uninstall_lines(result: &Map<String, Value>) -> Vec<String> {
    let theme = super::ui::Theme::for_fd(1);
    let width = super::format::output_width();
    let list = |key: &str| -> Vec<String> {
        result
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(tilde).collect())
            .unwrap_or_default()
    };
    let done = |text: &str| super::ui::done(&theme, text, width);
    let say = |text: &str, tone| super::ui::para(&theme, text, tone, width);
    let removed = list("removed");
    let kept = list("kept");
    let mut lines = Vec::new();
    if result.get("stopped") == Some(&Value::Bool(true)) {
        lines.extend(done("Stopped the bridge"));
    }
    if removed.is_empty() {
        lines.extend(say(
            "Switchboard isn't installed for this user; nothing was removed.",
            super::ui::Tone::Normal,
        ));
    } else {
        lines.extend(done("Removed Switchboard from this machine:"));
        lines.extend(removed.iter().map(|path| format!("    {path}")));
    }
    if !kept.is_empty() {
        lines.push(String::new());
        lines.extend(say(
            "Kept this machine's configuration, key, chat membership, and logs:",
            super::ui::Tone::Normal,
        ));
        lines.extend(kept.iter().map(|path| format!("    {path}")));
        lines.push(String::new());
        lines.extend(say("To remove those too, run", super::ui::Tone::Normal));
        lines.extend(super::ui::command(
            &theme,
            &format!("rm -rf {}", kept.join(" ")),
            width,
        ));
    }
    if let Some(machine) = result.get("machine").and_then(Value::as_str) {
        lines.push(String::new());
        lines.extend(say(
            &format!(
                "The server still lists this machine as {machine}. To remove it there, run this on the server:"
            ),
            super::ui::Tone::Normal,
        ));
        lines.extend(super::ui::command(
            &theme,
            &format!("{} server revoke {machine}", crate::server::install::SUDO),
            width,
        ));
    }
    lines
}
