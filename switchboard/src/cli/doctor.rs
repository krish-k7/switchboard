//! Read-only, bounded local diagnostics; never returns credential or server
//! text. The slow checks (chat count, key pairing, server health) run on
//! bounded worker threads with deadlines.
//!
//! The checks, in order: `program` (this version and its installed copy),
//! `configuration` (the configuration, key, and certificate), `bridge` (its
//! service, whether it runs, and its connection), `discovery` (only when the
//! bridge finds chats slowly or doesn't say how), `server` (whether it knows
//! this machine), and `chats` (joined chats that are open and reachable).
//! Summaries and fixes quote commands in backticks; `--json` drops them.

use std::os::unix::fs::MetadataExt;

use serde_json::{Map, Value};

use super::local::{int_pid, kill0, read_json_bounded, safe_path};
use super::roster::bounded;
use crate::discovery::{auth_key, discover, Selection};
use crate::error::{Error, Result};
use crate::fsutil;
use crate::hub_transport::{tls_config, ClientConfig, HubClient, HubError};
use crate::protocol::valid_uuid;
use crate::urlparse::urlsplit;
use crate::wirejson::{str_value, truthy, value_eq};

struct Checks(Vec<Value>);

impl Checks {
    fn add(&mut self, name: &str, status: &str, summary: &str, fix: Option<&str>) {
        let mut item = Map::new();
        item.insert("name".into(), Value::from(name));
        item.insert("status".into(), Value::from(status));
        item.insert("summary".into(), Value::from(summary));
        if let Some(fix) = fix.filter(|f| !f.is_empty()) {
            item.insert("fix".into(), Value::from(fix));
        }
        self.0.push(Value::Object(item));
    }
}

fn text_safe(value: &str) -> String {
    let kept: String = value
        .chars()
        .filter(|c| crate::text::is_printable(*c))
        .collect();
    crate::text::take_chars(&kept, 64)
}

/// The installed binary (no size limit) with `safe_path`'s other checks.
fn safe_binary(path: &str) -> Result<()> {
    safe_path(&fsutil::parent(path), true, false)?;
    let info = fsutil::lstat(path)?;
    if !fsutil::is_type(&info, fsutil::S_IFREG)
        || info.uid() != fsutil::getuid()
        || info.mode() & 0o022 != 0
    {
        return Err(Error::value("unsafe path"));
    }
    Ok(())
}

/// The validated config, with paths joined to the config's directory.
fn load_config(config_path: &str) -> Result<Map<String, Value>> {
    let path = safe_path(config_path, false, true)?;
    let mut config = read_json_bounded(&path, 65536)?;
    if !matches!(config.get("enabled"), Some(Value::Bool(_))) {
        return Err(Error::value("enabled required"));
    }
    let sessions = match config.get("sessions") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= 32 => items.clone(),
        _ => return Err(Error::value("sessions")),
    };
    for sid in &sessions {
        valid_uuid(Some(sid))?;
    }
    for (index, sid) in sessions.iter().enumerate() {
        if sessions[..index].iter().any(|other| value_eq(other, sid)) {
            return Err(Error::value("duplicate sessions"));
        }
    }
    config.insert("sessions".into(), Value::Array(sessions));
    let parent = fsutil::parent(&path);
    for key in ["cert", "key", "state_dir", "config_dir"] {
        match config.get(key) {
            Some(Value::String(value)) if !value.is_empty() => {
                let joined = join_path(&parent, value);
                config.insert(key.into(), Value::from(joined));
            }
            _ => return Err(Error::value("missing path")),
        }
    }
    match config.get("ca") {
        None | Some(Value::Null) => {}
        Some(Value::String(value)) if !value.is_empty() => {
            let joined = join_path(&parent, value);
            config.insert("ca".into(), Value::from(joined));
        }
        _ => return Err(Error::value("invalid CA")),
    }
    let hub_url = match crate::config::server_url(&config) {
        Some(Value::String(url)) => url.clone(),
        _ => return Err(Error::value("invalid server address")),
    };
    let parsed = urlsplit(&hub_url)?;
    if parsed.scheme != "https"
        || parsed.hostname().is_none()
        || parsed.username().is_some()
        || parsed.password().is_some()
        || !parsed.query.is_empty()
        || !parsed.fragment.is_empty()
        || !(parsed.path.is_empty() || parsed.path == "/")
        || parsed.port()? == Some(0)
    {
        return Err(Error::value("invalid server address"));
    }
    Ok(config)
}

/// `value` under `parent`, unless `value` is absolute (then normalized alone).
fn join_path(parent: &str, value: &str) -> String {
    if value.starts_with('/') {
        fsutil::normalize(value)
    } else {
        fsutil::join(parent, value)
    }
}

fn string(config: &Map<String, Value>, key: &str) -> String {
    config.get(key).map(str_value).unwrap_or_default()
}

/// Run every doctor check against the config at `config_path`.
pub fn run_checks(config_path: &str) -> Map<String, Value> {
    let mut checks = Checks(Vec::new());
    let runtime_root = fsutil::join(&super::local::home_dir(), ".local/share/switchboard");
    let installed = (|| -> Result<()> {
        safe_path(&runtime_root, true, false)?;
        safe_path(
            &fsutil::join(&runtime_root, ".switchboard-install"),
            false,
            false,
        )?;
        safe_binary(&fsutil::join(&runtime_root, super::install::BINARY))
    })();
    match installed {
        Ok(()) => checks.add(
            "program",
            "pass",
            &format!("Switchboard {}, installed in ~/bin.", crate::VERSION),
            None,
        ),
        Err(_) => {
            let running = super::install::current_binary()
                .ok()
                .map(|p| fsutil::parent(&p))
                .is_some_and(|dir| dir == runtime_root);
            checks.add(
                "program",
                if running { "fail" } else { "warn" },
                &format!(
                    "Switchboard {} is running, but its installed copy is missing or unsafe.",
                    crate::VERSION
                ),
                Some("Run `switchboard install` to install it again."),
            );
        }
    }

    let config = match load_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            if super::is_file_not_found(&error) {
                checks.add(
                    "configuration",
                    "fail",
                    "This machine isn't set up yet.",
                    Some("Run `switchboard init`."),
                );
            } else {
                checks.add(
                    "configuration",
                    "fail",
                    "The configuration is invalid or unsafe.",
                    Some(
                        "Check its fields, that you own it, its private mode (0600), and that no directory above it is a symbolic link.",
                    ),
                );
            }
            return result(checks);
        }
    };
    let enabled = config.get("enabled") == Some(&Value::Bool(true));
    let tls = (|| -> Result<ClientConfig> {
        let mut client = ClientConfig {
            hub_url: crate::config::server_url(&config)
                .map(str_value)
                .unwrap_or_default(),
            ..Default::default()
        };
        for key in ["cert", "key", "ca"] {
            let value = config.get(key).filter(|v| truthy(Some(v)));
            let Some(value) = value else { continue };
            let checked = safe_path(&str_value(value), false, true)?;
            match key {
                "cert" => client.cert = checked,
                "key" => client.key = checked,
                _ => client.ca = Some(checked),
            }
        }
        let probe = client.clone();
        bounded(5.0, move || tls_config(&probe).map(|_| ()))
            .ok_or_else(|| Error::subprocess("TLS check timed out"))?
            .map_err(|_| Error::value("invalid TLS files"))?;
        Ok(client)
    })();
    match &tls {
        Ok(_) => checks.add(
            "configuration",
            "pass",
            "Configuration and key are private and valid.",
            None,
        ),
        Err(_) => checks.add(
            "configuration",
            "fail",
            "This machine's key or certificate is unsafe or invalid, or the two don't match.",
            Some("Check that you own them and that they are private (mode 0600); keep the identity files."),
        ),
    }

    let daemon = (|| -> Result<Map<String, Value>> {
        let root = safe_path(&string(&config, "state_dir"), true, true)?;
        let runtime = read_json_bounded(
            &safe_path(&fsutil::join(&root, "runtime.json"), false, true)?,
            4096,
        )?;
        let pid = match int_pid(runtime.get("pid")) {
            Some(pid) if pid > 1 && pid <= i32::MAX as i64 => pid,
            _ => return Err(Error::value("invalid PID")),
        };
        kill0(pid)?;
        if !truthy(runtime.get("procStart"))
            || !value_eq(
                &Value::String(crate::identity::process_start(pid)?),
                runtime.get("procStart").unwrap_or(&Value::Null),
            )
        {
            return Err(Error::value("stale identity"));
        }
        Ok(runtime)
    })();
    let runtime = match daemon {
        Ok(runtime) => {
            let connected = runtime.get("connected") == Some(&Value::Bool(true));
            if !enabled {
                checks.add(
                    "bridge",
                    "warn",
                    "The bridge is running, but turned off in its configuration.",
                    Some("Set \"enabled\" to true in the configuration, then run `switchboard down` and `switchboard up`."),
                );
            } else if connected {
                checks.add(
                    "bridge",
                    "pass",
                    "The bridge is running and connected.",
                    None,
                );
            } else if runtime.get("server_refused").and_then(Value::as_str) == Some("not_enrolled")
            {
                // The bridge records the server's refusal of this machine's
                // key; retrying and the network can't fix that.
                checks.add(
                    "bridge",
                    "fail",
                    "The bridge is running, but the server doesn't accept this machine's key: it was removed from the server, or never added.",
                    Some("Make a new invite on the server (`sudo switchboard server invite`), then run `switchboard init` here and choose to join again."),
                );
            } else {
                checks.add(
                    "bridge",
                    "fail",
                    "The bridge is running, but not connected to the server.",
                    Some("Check this machine's network and that the server is running; the bridge keeps trying."),
                );
            }
            Some(runtime)
        }
        Err(error)
            if super::is_file_not_found(&error) || error.message.starts_with("[Errno 3] ") =>
        {
            let (_, service_summary, _) = super::service::doctor_check();
            let (summary, fix) = if super::service::is_down(config_path) {
                ("", "")
            } else if !enabled {
                (
                    "The bridge is turned off in its configuration.",
                    "Set \"enabled\" to true in the configuration, then run `switchboard up`.",
                )
            } else if service_summary.contains("old name") {
                (
                    "The bridge isn't running, and its service is still installed under an old name.",
                    "Run `switchboard install` to move it, then `switchboard up`.",
                )
            } else if super::service::supported() && !super::service::installed() {
                (
                    "The bridge isn't running, and its background service isn't installed.",
                    "Run `switchboard install --start` to install it and start the bridge.",
                )
            } else {
                ("The bridge isn't running.", "Run `switchboard up`.")
            };
            if summary.is_empty() {
                // Taken down on purpose: a state, not a problem to fix.
                checks.0.push(serde_json::json!({
                    "name": "bridge",
                    "status": "pass",
                    "summary": "Down. Run `switchboard up` to reconnect.",
                    "down": true,
                }));
            } else {
                checks.add(
                    "bridge",
                    if enabled { "fail" } else { "warn" },
                    summary,
                    Some(fix),
                );
            }
            None
        }
        Err(_) => {
            checks.add(
                "bridge",
                "fail",
                "The bridge's runtime file is unsafe or out of date.",
                Some("Run `switchboard down`, then `switchboard up`."),
            );
            None
        }
    };
    if let Some(runtime) = &runtime {
        match runtime.get("discovery_mode").and_then(Value::as_str) {
            Some("events") => {}
            Some("polling") => checks.add(
                "discovery",
                "warn",
                "Chats are found by polling, so new chats can take a few seconds to appear.",
                None,
            ),
            _ => checks.add(
                "discovery",
                "warn",
                "The bridge doesn't say how it finds chats; it may be an older version.",
                Some("Run `switchboard down`, then `switchboard up`, to run this version."),
            ),
        }
    }

    if let Ok(client) = tls {
        let reply = health(client);
        let state = reply
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("transport")
            .to_string();
        if state == "healthy" {
            let summary = match (
                reply.get("machine").and_then(Value::as_str),
                reply.get("owner").and_then(Value::as_str),
            ) {
                (Some(machine), Some(owner)) => format!(
                    "The server knows this machine as {} (owner {}).",
                    text_safe(machine),
                    text_safe(owner)
                ),
                _ => "The server knows this machine.".to_string(),
            };
            checks.add("server", "pass", &summary, None);
        } else {
            let (summary, fix) = match state.as_str() {
                "authorization" => (
                    "The server doesn't accept this machine's key.",
                    "Make a new invite on the server (`sudo switchboard server invite`), then run `switchboard init` here and choose to join again.",
                ),
                "http" => (
                    "The server answered with an error.",
                    "Check the server's log: run `sudo switchboard server logs` on the server.",
                ),
                "timeout" => (
                    "The server didn't answer within 8 seconds.",
                    "Check the network and that the server is running.",
                ),
                "invalid" => (
                    "The server's answer couldn't be read.",
                    "Update Switchboard on the server and here (`switchboard upgrade`).",
                ),
                _ => (
                    "Can't connect to the server, or its certificate isn't the one this machine trusts.",
                    "Check the network and that the server is running. If its certificate changed, run `switchboard init` with a new invite from it.",
                ),
            };
            checks.add("server", "fail", summary, Some(fix));
        }
    }

    let sessions = (|| -> Result<(usize, usize, usize)> {
        let root = safe_path(&string(&config, "config_dir"), true, false)?;
        safe_path(&fsutil::join(&root, "sessions"), true, true)?;
        let selected: Selection = config
            .get("sessions")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let count = selected.len();
        let (open, live) = bounded(5.0, move || -> Result<(usize, usize)> {
            let records = discover(&root, &selected)?;
            let reachable = records
                .values()
                .filter(|record| auth_key(&root, record).is_ok())
                .count();
            Ok((records.len(), reachable))
        })
        .ok_or_else(|| Error::subprocess("session check timed out"))??;
        Ok((count, open, live))
    })();
    match sessions {
        Ok((0, _, _)) => checks.add(
            "chats",
            "pass",
            "No chats joined yet. To connect one, run `/switchboard join` in it.",
            None,
        ),
        Ok((joined, open, live)) if live == open => {
            checks.add("chats", "pass", &chats_summary(joined, open), None)
        }
        Ok((_, open, live)) => checks.add(
            "chats",
            "warn",
            &format!(
                "{} of the open joined chats can't be reached.",
                open - live
            ),
            Some("Check that you own Claude Code's session files (~/.claude/sessions) and that they are private."),
        ),
        Err(_) => checks.add(
            "chats",
            "warn",
            "Joined chats can't be checked safely.",
            Some("Check that you own Claude Code's session files (~/.claude/sessions) and that they are private."),
        ),
    }
    result(checks)
}

/// How many chats are joined and open, when all the open ones are reachable.
fn chats_summary(joined: usize, open: usize) -> String {
    let chats = super::ui::count(joined, "chat", "chats");
    match open {
        0 => format!("{chats} joined; none open now."),
        n if n == joined && n == 1 => format!("{chats} joined, open now."),
        n if n == joined => format!("{chats} joined, all open now."),
        n => format!("{chats} joined; {n} open now."),
    }
}

/// An authenticated GET /health within eight seconds.
fn health(config: ClientConfig) -> Map<String, Value> {
    let state = |name: &str| {
        let mut value = Map::new();
        value.insert("state".into(), Value::from(name));
        value
    };
    let outcome = bounded(8.0, move || -> Map<String, Value> {
        let client = match HubClient::with_limit(&config, 65536) {
            Ok(client) => client,
            Err(_) => return state("transport"),
        };
        let reply = client.request("GET", "/health", None);
        client.close();
        match reply {
            Ok(result) => {
                if result.get("ok") != Some(&Value::Bool(true)) {
                    return state("invalid");
                }
                let mut identity = Map::new();
                for key in ["machine", "owner"] {
                    if let Some(Value::String(value)) = result.get(key) {
                        identity.insert(key.into(), Value::from(value.clone()));
                    }
                }
                identity.insert("state".into(), Value::from("healthy"));
                identity
            }
            Err(HubError::Http { status, .. }) if status == 401 || status == 403 => {
                state("authorization")
            }
            Err(HubError::Http { .. }) => state("http"),
            Err(_) => state("transport"),
        }
    });
    outcome.unwrap_or_else(|| state("timeout"))
}

/// The result for `--json`: the backticks around commands in summaries and
/// fixes are for the human output only, and are dropped. (Names that came
/// from the server keep theirs: only fixed text has backticks.)
pub fn plain(mut result: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::Array(checks)) = result.get_mut("checks") {
        for check in checks.iter_mut().filter_map(Value::as_object_mut) {
            let name = check
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            for key in ["summary", "fix"] {
                if key == "summary" && name == "server" {
                    continue;
                }
                if let Some(Value::String(text)) = check.get_mut(key) {
                    *text = text.replace('`', "");
                }
            }
        }
    }
    result
}

fn result(checks: Checks) -> Map<String, Value> {
    let ok = !checks
        .0
        .iter()
        .any(|item| item.get("status").and_then(Value::as_str) == Some("fail"));
    let mut value = Map::new();
    value.insert("ok".into(), Value::Bool(ok));
    value.insert("checks".into(), Value::Array(checks.0));
    value
}

#[cfg(test)]
mod tests {
    #[test]
    fn joined_chats_read_naturally() {
        assert_eq!(super::chats_summary(1, 1), "1 chat joined, open now.");
        assert_eq!(super::chats_summary(3, 3), "3 chats joined, all open now.");
        assert_eq!(super::chats_summary(3, 1), "3 chats joined; 1 open now.");
        assert_eq!(super::chats_summary(2, 0), "2 chats joined; none open now.");
    }
}
