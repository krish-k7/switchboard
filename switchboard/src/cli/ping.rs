//! Reachability check answered by a remote bridge, never by a remote Claude
//! session. It runs on a bounded worker thread (10 seconds, after the
//! 8-second roster lookup).

use serde_json::{Map, Value};

use crate::hub_transport::{HubClient, HubError};
use crate::membership::dash_unsafe;
use crate::protocol::clean_name;
use crate::text::take_chars;
use crate::wirejson::str_value;

fn error_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "target_offline" => "that chat is offline or no longer joined",
        "ping_unsupported" => "the remote Switchboard is too old to answer ping",
        "ping_timeout" => "no reply within 5 seconds",
        "not_found" => "the server is too old to support ping",
        "rate_limited" => "the server is rate limiting this machine; try again shortly",
        "queue_full" => "the remote machine's queue is full; try again shortly",
        "authorization" => "the server refused authorization for this machine",
        "transport" => "could not reach the server",
        "invalid" => "the server returned an invalid ping reply",
        "timeout" => "no reply within the deadline",
        _ => return None,
    })
}

/// The native alias a bridge gives a remote chat, before collision suffixes.
pub fn proxy_alias(machine: &str, name: &str, sid: &str) -> String {
    let source = if name.is_empty() { "Claude" } else { name };
    let base = dash_unsafe(&clean_name(source));
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "Claude" } else { base };
    let host = dash_unsafe(machine);
    format!(
        "switchboard/{}/{}-{}",
        take_chars(&host, 16),
        take_chars(base, 26),
        take_chars(sid, 8)
    )
}

fn field(row: &Map<String, Value>, key: &str) -> String {
    row.get(key).map(str_value).unwrap_or_default()
}

/// The row a ping target names: alias, machine/name, name, route, or UUID prefix.
pub fn resolve(rows: &[Map<String, Value>], target: &str) -> Result<Map<String, Value>, String> {
    use super::format::casefold;
    let wanted = casefold(crate::text::strip(target));
    if wanted.is_empty() {
        return Err("name a chat on another machine to ping".into());
    }
    let mut matches = Vec::new();
    for row in rows {
        let (machine, name, id) = (field(row, "machine"), field(row, "name"), field(row, "id"));
        let alias = casefold(&proxy_alias(&machine, &name, &id));
        let names = [
            alias.clone(),
            alias
                .strip_prefix("switchboard/")
                .unwrap_or(&alias)
                .to_string(),
            casefold(&name),
            casefold(&format!("{machine}/{name}")),
            casefold(&field(row, "route")),
        ];
        let prefix = wanted.chars().count() >= 4
            && wanted
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
            && id.starts_with(&wanted);
        if names.contains(&wanted) || prefix {
            matches.push(row.clone());
        }
    }
    if matches.is_empty() {
        return Err(format!(
            "no chat on another machine matches {}; run switchboard chats --remote",
            quoted(target)
        ));
    }
    if matches.len() > 1 {
        let listed = matches
            .iter()
            .take(8)
            .map(|r| proxy_alias(&field(r, "machine"), &field(r, "name"), &field(r, "id")))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "{} matches {} chats ({}); use a full alias or UUID",
            quoted(target),
            matches.len(),
            listed
        ));
    }
    Ok(matches.remove(0))
}

/// `f"'{target}'"`: plain single quotes, not repr.
fn quoted(target: &str) -> String {
    format!("'{target}'")
}

fn failure(error: &str) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("ok".into(), Value::Bool(false));
    value.insert("error".into(), Value::from(error));
    value
}

/// Ping a chat; the result is the JSON contract.
pub fn ping(config_path: &str, target: Option<&str>) -> Map<String, Value> {
    let target = match target {
        Some(target) if !crate::text::strip(target).is_empty() => target,
        _ => {
            return failure(
                "name a chat on another machine to ping, for example switchboard ping research",
            )
        }
    };
    let lookup = super::roster::fetch_roster(config_path);
    if lookup.get("remote_available") != Some(&Value::Bool(true)) {
        let error = lookup
            .get("remote_error")
            .filter(|v| crate::wirejson::truthy(Some(v)))
            .map(str_value)
            .unwrap_or_else(|| "chats on other machines are unavailable".into());
        return failure(&error);
    }
    let rows: Vec<Map<String, Value>> = lookup
        .get("remote_sessions")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default();
    let row = match resolve(&rows, target) {
        Ok(row) => row,
        Err(error) => return failure(&error),
    };
    let config = match super::local::client_config(config_path) {
        Ok((_, config)) => config,
        Err(_) => return failure("configuration or TLS files are missing or unsafe"),
    };
    let route = field(&row, "route");
    let outcome = super::roster::bounded(10.0, move || -> Result<(i64, bool), String> {
        let client = HubClient::new(&config).map_err(|e| code_of(&e))?;
        let mut body = Map::new();
        body.insert("to".into(), Value::from(route));
        let reply = client.post("/v1/ping", &Value::Object(body));
        client.close();
        let value = match reply {
            Ok(value) => value,
            Err(error) => return Err(code_of(&error)),
        };
        let rtt = value
            .get("rtt_ms")
            .and_then(crate::wirejson::as_int)
            .and_then(|d| d.parse::<i64>().ok());
        match (value.get("ok"), rtt, value.get("live")) {
            (Some(Value::Bool(true)), Some(rtt), Some(Value::Bool(live))) => Ok((rtt, *live)),
            _ => Err("invalid".into()),
        }
    });
    let (rtt, live) = match outcome {
        None => return failure(error_text("timeout").expect("known")),
        Some(Err(code)) => {
            let message = match error_text(&code) {
                Some(text) => text.to_string(),
                None => format!("the server refused the ping ({})", take_chars(&code, 64)),
            };
            return failure(&message);
        }
        Some(Ok(reply)) => reply,
    };
    let (machine, name) = (field(&row, "machine"), field(&row, "name"));
    if !live {
        return failure(&format!(
            "{machine} answered, but that chat is no longer joined there"
        ));
    }
    let mut value = Map::new();
    value.insert("ok".into(), Value::Bool(true));
    value.insert(
        "target".into(),
        Value::from(proxy_alias(&machine, &name, &field(&row, "id"))),
    );
    value.insert("machine".into(), Value::from(machine));
    value.insert("name".into(), Value::from(name));
    value.insert("rtt_ms".into(), Value::from(rtt));
    value.insert("via".into(), Value::from("server"));
    value
}

fn code_of(error: &HubError) -> String {
    match error {
        HubError::Http { status, code } => {
            if (*status == 401 || *status == 403) && code.as_deref() != Some("pong_forbidden") {
                "authorization".into()
            } else {
                code.clone().unwrap_or_else(|| "transport".into())
            }
        }
        HubError::Value(_) => "invalid".into(),
        _ => "transport".into(),
    }
}

/// The human line for a ping result.
pub fn format_ping(result: &Map<String, Value>) -> String {
    let text = format!(
        "{}/{} replied in {} ms.",
        crate::cli::format::text_value(result.get("machine")),
        crate::cli::format::text_value(result.get("name")),
        result.get("rtt_ms").map(str_value).unwrap_or_default(),
    );
    crate::cli::ui::item(
        &crate::cli::ui::Theme::for_fd(1),
        crate::cli::ui::Mark::Done,
        &text,
        crate::cli::ui::Tone::Normal,
        crate::cli::format::output_width(),
    )
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(machine: &str, name: &str, id: &str) -> Map<String, Value> {
        json!({"machine": machine, "name": name, "id": id, "route": format!("{machine}/{id}")})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn aliases_match_bridge_proxy_names() {
        assert_eq!(
            proxy_alias("workstation.local", "research chat!", "12345678-aaaa"),
            "switchboard/workstation-loca/research-chat-12345678"
        );
        assert_eq!(
            proxy_alias("m", "", "abcdefgh"),
            "switchboard/m/Claude-abcdefgh"
        );
        assert_eq!(
            proxy_alias("m", "!!!", "abcdefgh"),
            "switchboard/m/Claude-abcdefgh"
        );
    }

    #[test]
    fn resolution_and_refusals() {
        let a = "aaaaaaaa-1111-4111-8111-111111111111";
        let b = "bbbbbbbb-1111-4111-8111-111111111111";
        let rows = [
            row("desk", "research", a),
            row("desk", "twin", b),
            row("lab", "twin", a),
        ];
        for target in [
            "research",
            "RESEARCH",
            "desk/research",
            "switchboard/desk/research-aaaaaaaa",
            "desk/research-aaaaaaaa",
            "aaaa",
        ] {
            if target == "aaaa" {
                assert!(resolve(&rows, target)
                    .unwrap_err()
                    .contains("matches 2 chats"));
            } else {
                assert_eq!(resolve(&rows, target).unwrap()["id"], a);
            }
        }
        assert_eq!(
            resolve(&rows, "twin").unwrap_err(),
            "'twin' matches 2 chats (switchboard/desk/twin-bbbbbbbb, switchboard/lab/twin-aaaaaaaa); use a full alias or UUID"
        );
        assert_eq!(
            resolve(&rows, "zzz").unwrap_err(),
            "no chat on another machine matches 'zzz'; run switchboard chats --remote"
        );
        assert_eq!(
            resolve(&rows, "  ").unwrap_err(),
            "name a chat on another machine to ping"
        );
    }
}
