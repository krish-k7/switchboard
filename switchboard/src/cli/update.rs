//! Whether a newer release exists, for the one line `status` adds.
//!
//! The latest release is asked of GitHub at most once a day; the answer
//! (or that there was none) is kept in the bridge's state directory as
//! `update-check.json`. The question runs on its own thread while the
//! command does its work, and is given at most two seconds in all, so it
//! never holds up what the command prints. It is off when
//! `SWITCHBOARD_NO_UPDATE_CHECK` is set (to anything but `0`) and in CI
//! (`CI` set to anything but `false` or `0`). A failure (no network, a
//! private repository, or no answer within the two seconds) is quiet, and
//! is kept for an hour, so a network that hangs costs one wait an hour, not
//! one per command.

use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::release::{Source, Version};
use crate::fsutil;

/// How long the answer is kept.
const DAY: f64 = 86400.0;
/// How long a failure to get one is kept.
const HOUR: f64 = 3600.0;
/// The longest the question may take.
const BUDGET: Duration = Duration::from_secs(2);
const FILE: &str = "update-check.json";

/// Whether the check is turned off for this run.
pub fn disabled() -> bool {
    let set = |name: &str, off: &[&str]| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty() && !off.contains(&v.to_ascii_lowercase().as_str()))
            .is_some()
    };
    set("SWITCHBOARD_NO_UPDATE_CHECK", &["0"]) || set("CI", &["0", "false"])
}

/// A check under way.
pub struct Check {
    began: Instant,
    cache: Option<String>,
    /// The newest release known now, from the cache.
    known: Option<Option<String>>,
    /// The latest release tag (None for none usable), or Err when the
    /// question failed.
    pending: Option<Receiver<Result<Option<String>, ()>>>,
}

/// The newer of `latest` and this version, if `latest` is newer.
fn newer(latest: Option<&str>) -> Option<String> {
    let latest = latest?;
    let theirs = Version::parse(latest)?;
    let ours = Version::parse(crate::VERSION)?;
    (theirs > ours).then(|| latest.trim_start_matches('v').to_string())
}

fn read_cache(path: &str, now: f64) -> Option<Option<String>> {
    let text = std::fs::read(path).ok()?;
    let value: Map<String, Value> = serde_json::from_slice(&text).ok()?;
    let checked = value.get("checked_at").and_then(Value::as_f64)?;
    let kept = if value.get("failed") == Some(&Value::Bool(true)) {
        HOUR
    } else {
        DAY
    };
    if !(0.0..kept).contains(&(now - checked)) {
        return None;
    }
    Some(
        value
            .get("latest")
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

/// Keep the answer (`latest`), or that there was none because the question
/// failed (`failed`).
fn write_cache(path: &str, latest: Option<&str>, failed: bool, now: f64) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = fsutil::parent(path);
    if !fsutil::lstat(&dir).is_ok_and(|info| info.is_dir()) {
        return;
    }
    let temporary = format!("{path}.{}.tmp", std::process::id());
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| {
            let mut value = json!({"checked_at": now.floor(), "latest": latest});
            if failed {
                value["failed"] = Value::Bool(true);
            }
            file.write_all(format!("{value}\n").as_bytes())
        });
    if written.is_err() || fsutil::replace(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Start the check for the bridge whose state directory is `state_dir`.
pub fn start(state_dir: Option<&str>) -> Check {
    let began = Instant::now();
    let mut check = Check {
        began,
        cache: state_dir.map(|dir| fsutil::join(dir, FILE)),
        known: None,
        pending: None,
    };
    if disabled() {
        check.known = Some(None);
        return check;
    }
    let now = crate::fmtutil::time_now();
    if let Some(known) = check
        .cache
        .as_deref()
        .and_then(|path| read_cache(path, now))
    {
        check.known = Some(known);
        return check;
    }
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let latest = Source::from_env()
            .and_then(|source| source.latest_tag())
            .map(|tag| Some(tag).filter(|tag| Version::parse(tag).is_some()))
            .map_err(|_| ());
        let _ = sender.send(latest);
    });
    check.pending = Some(receiver);
    check
}

impl Check {
    /// The newer release, if one is known within the two seconds; the
    /// answer is kept for a day, and a failure or no answer for an hour.
    pub fn finish(self) -> Option<String> {
        if let Some(known) = self.known {
            return newer(known.as_deref());
        }
        let receiver = self.pending?;
        let left = BUDGET.saturating_sub(self.began.elapsed());
        // No answer in time counts as a failure.
        let answer = receiver.recv_timeout(left).unwrap_or(Err(()));
        let latest = answer.clone().unwrap_or(None);
        if let Some(path) = &self.cache {
            write_cache(
                path,
                latest.as_deref(),
                answer.is_err(),
                crate::fmtutil::time_now(),
            );
        }
        newer(latest.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_newer_release_counts() {
        assert_eq!(newer(Some("v999.0.0")).as_deref(), Some("999.0.0"));
        assert_eq!(newer(Some(&format!("v{}", crate::VERSION))), None);
        assert_eq!(newer(Some("v0.0.1")), None);
        assert_eq!(newer(Some("garbage")), None);
        assert_eq!(newer(None), None);
    }

    #[test]
    fn the_answer_is_kept_for_a_day() {
        let dir = std::env::temp_dir().join(format!("sbupdate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE).to_string_lossy().into_owned();
        write_cache(&path, Some("v9.9.9"), false, 1000.0);
        assert_eq!(
            read_cache(&path, 1000.0 + 60.0),
            Some(Some("v9.9.9".into()))
        );
        assert_eq!(
            read_cache(&path, 1000.0 + 2.0 * HOUR),
            Some(Some("v9.9.9".into()))
        );
        assert_eq!(read_cache(&path, 1000.0 + DAY + 1.0), None);
        write_cache(&path, None, false, 1000.0);
        assert_eq!(read_cache(&path, 1001.0), Some(None));
        // A failure (or no answer in time) is kept for an hour.
        write_cache(&path, None, true, 1000.0);
        assert_eq!(read_cache(&path, 1000.0 + HOUR - 1.0), Some(None));
        assert_eq!(read_cache(&path, 1000.0 + HOUR + 1.0), None);
        // A clock set back is not trusted either.
        assert_eq!(read_cache(&path, 10.0), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
