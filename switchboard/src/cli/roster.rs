//! Read-only presence lookup with an eight-second deadline covering DNS,
//! TLS, and the reply, run on a bounded worker thread.

use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::hub_transport::{HubClient, HubError};
use crate::urlparse::urlsplit;

pub fn error_text(reason: &str) -> &'static str {
    match reason {
        "configuration" => {
            "Chats on other machines are unavailable: this machine's configuration or key files are missing or unsafe."
        }
        "authorization" => "Chats on other machines are unavailable: the server doesn't accept this machine's key.",
        "unsupported" => {
            "Chats on other machines are unavailable: the server is too old to list them."
        }
        "timeout" => "Chats on other machines are unavailable: the server didn't answer within 8 seconds.",
        "invalid" => "Chats on other machines are unavailable: the server's answer couldn't be read.",
        _ => "Chats on other machines are unavailable: can't connect to the server, or its certificate isn't the one this machine trusts.",
    }
}

pub fn unavailable(reason: &str) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("remote_available".into(), Value::Bool(false));
    value.insert("remote_sessions".into(), Value::Array(Vec::new()));
    value.insert("remote_error".into(), Value::from(error_text(reason)));
    value
}

fn label(value: Option<&Value>, maximum: usize) -> Result<String> {
    match value {
        Some(Value::String(text))
            if !text.is_empty()
                && crate::text::char_len(text) <= maximum
                && !text.chars().any(|c| (c as u32) < 32 || c as u32 == 127) =>
        {
            Ok(text.clone())
        }
        _ => Err(Error::value("invalid label")),
    }
}

/// A machine or owner identity: `[A-Za-z0-9_-]{1,64}`.
pub fn identity(value: Option<&Value>) -> Result<String> {
    match value {
        Some(Value::String(text))
            if (1..=64).contains(&text.len())
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') =>
        {
            Ok(text.clone())
        }
        _ => Err(Error::value("invalid identity")),
    }
}

fn lower_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, b)| match index {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
}

/// Check a roster reply's shape and return it.
pub fn validate_roster(value: &Map<String, Value>) -> Result<Map<String, Value>> {
    if value.get("ok") != Some(&Value::Bool(true)) {
        return Err(Error::value("invalid roster"));
    }
    let machine = identity(value.get("machine"))?;
    let peers = match value.get("peers") {
        Some(Value::Array(peers)) if peers.len() <= 32768 => peers,
        _ => return Err(Error::value("invalid roster")),
    };
    match value.get("peers_revision") {
        Some(Value::String(rev))
            if rev.len() == 64
                && rev
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => {}
        _ => return Err(Error::value("invalid roster")),
    }
    let mut rows = Vec::new();
    let mut routes = std::collections::HashSet::new();
    for peer in peers {
        let Value::Object(peer) = peer else {
            return Err(Error::value("invalid peer"));
        };
        let sid = match peer.get("id") {
            Some(Value::String(sid)) if lower_uuid(sid) => sid.clone(),
            _ => return Err(Error::value("invalid session ID")),
        };
        let mut row = Map::new();
        row.insert("id".into(), Value::from(sid.clone()));
        let row_machine = identity(peer.get("machine"))?;
        row.insert("machine".into(), Value::from(row_machine.clone()));
        row.insert("name".into(), Value::from(label(peer.get("name"), 64)?));
        let status = label(peer.get("status"), 64)?;
        row.insert("status".into(), Value::from(status.clone()));
        row.insert("owner".into(), Value::from(label(peer.get("owner"), 64)?));
        if !crate::protocol::ACTIVITY_STATES.contains(&status.as_str()) {
            return Err(Error::value("invalid activity"));
        }
        let route = format!("{row_machine}/{sid}");
        if !routes.insert(route.clone()) {
            return Err(Error::value("duplicate route"));
        }
        row.insert("route".into(), Value::from(route));
        for key in ["project", "repository", "branch"] {
            if peer.contains_key(key) {
                let limit = if key == "project" { 64 } else { 256 };
                row.insert(key.into(), Value::from(label(peer.get(key), limit)?));
            }
        }
        if row_machine != machine {
            rows.push(Value::Object(row));
        }
    }
    let mut result = Map::new();
    result.insert("remote_available".into(), Value::Bool(true));
    result.insert("remote_sessions".into(), Value::Array(rows));
    result.insert("machine".into(), Value::from(machine));
    Ok(result)
}

/// Run `work` on a thread; `None` when it did not finish within `seconds`.
pub fn bounded<T: Send + 'static>(
    seconds: f64,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("switchboard-bounded".into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .ok()?;
    receiver.recv_timeout(Duration::from_secs_f64(seconds)).ok()
}

fn checked_config(config_path: &str) -> Result<crate::hub_transport::ClientConfig> {
    let (source, config) = super::local::client_config(config_path)?;
    let hub_url = match crate::config::server_url(&source) {
        Some(Value::String(url)) if !url.is_empty() => url.clone(),
        _ => return Err(Error::value("invalid origin")),
    };
    let url = urlsplit(&hub_url)?;
    if url.scheme != "https"
        || url.hostname().is_none()
        || url.username().is_some()
        || url.password().is_some()
        || !url.query.is_empty()
        || !url.fragment.is_empty()
        || !(url.path.is_empty() || url.path == "/")
        || url.port()? == Some(0)
    {
        return Err(Error::value("invalid origin"));
    }
    for key in ["cert", "key", "ca"] {
        if key == "ca" && source.get(key).is_none_or(Value::is_null) {
            continue;
        }
        if !matches!(source.get(key), Some(Value::String(v)) if !v.is_empty()) {
            return Err(Error::value("invalid path"));
        }
    }
    Ok(config)
}

/// The remote roster, or why it is unavailable.
pub fn fetch_roster(config_path: &str) -> Map<String, Value> {
    let config = match checked_config(config_path) {
        Ok(config) => config,
        Err(_) => return unavailable("configuration"),
    };
    let outcome = bounded(
        8.0,
        move || -> std::result::Result<Map<String, Value>, &'static str> {
            let client =
                HubClient::with_limit(&config, 16 * 1024 * 1024).map_err(|e| classify(&e))?;
            let reply = client.request("GET", "/v1/peers", None);
            client.close();
            match reply {
                Ok(value) => validate_roster(&value).map_err(|_| "invalid"),
                Err(error) => Err(classify(&error)),
            }
        },
    );
    match outcome {
        None => unavailable("timeout"),
        Some(Ok(value)) => value,
        Some(Err(reason)) => unavailable(reason),
    }
}

fn classify(error: &HubError) -> &'static str {
    match error {
        HubError::Http { status, .. } if *status == 401 || *status == 403 => "authorization",
        HubError::Http { status: 404, .. } => "unsupported",
        HubError::Value(_) => "invalid",
        _ => "transport",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roster(peers: Value) -> Map<String, Value> {
        json!({"ok": true, "machine": "laptop", "peers": peers, "peers_revision": "a".repeat(64)})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn valid_roster_excludes_own_machine_and_keeps_routes() {
        let sid = "11111111-1111-4111-8111-111111111111";
        let value = validate_roster(&roster(json!([
            {"machine": "laptop", "owner": "k", "id": sid, "name": "mine", "status": "idle", "last_seen": 1},
            {"machine": "desk", "owner": "d", "id": sid, "name": "theirs", "status": "busy", "branch": "main"},
        ])))
        .unwrap();
        assert_eq!(
            Value::Object(value),
            json!({"remote_available": true, "remote_sessions": [
                {"id": sid, "machine": "desk", "name": "theirs", "status": "busy", "owner": "d",
                 "route": format!("desk/{sid}"), "branch": "main"}], "machine": "laptop"})
        );
    }

    #[test]
    fn invalid_rosters_fail_closed() {
        let sid = "11111111-1111-4111-8111-111111111111";
        for peers in [
            json!([{"machine": "d", "owner": "o", "id": "AAAAAAAA-1111-4111-8111-111111111111", "name": "n", "status": "idle"}]),
            json!([{"machine": "d e", "owner": "o", "id": sid, "name": "n", "status": "idle"}]),
            json!([{"machine": "d", "owner": "o", "id": sid, "name": "n\u{7}", "status": "idle"}]),
            json!([{"machine": "d", "owner": "o", "id": sid, "name": "n", "status": "sleeping"}]),
            json!([{"machine": "d", "owner": "o", "id": sid, "name": "n", "status": "idle"},
                   {"machine": "d", "owner": "o", "id": sid, "name": "n", "status": "idle"}]),
            json!([1]),
        ] {
            assert!(validate_roster(&roster(peers.clone())).is_err(), "{peers}");
        }
        let mut bad = roster(json!([]));
        bad.insert("peers_revision".into(), json!("short"));
        assert!(validate_roster(&bad).is_err());
    }

    #[test]
    fn unsafe_config_never_starts_a_request() {
        assert_eq!(
            fetch_roster("/nonexistent/switchboard/bridge.json"),
            unavailable("configuration")
        );
    }
}
