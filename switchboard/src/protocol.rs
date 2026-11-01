//! Claude Code's native peer protocol as Switchboard speaks it: attribution
//! envelopes, addresses, identifiers, receipt markers, and the text of every
//! Switchboard-authored notice.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::text;

pub const ACTIVITY_STATES: [&str; 5] = ["idle", "busy", "waiting", "shell", "unknown"];
pub const FRAME_LIMIT: usize = 65536;
pub const TEXT_LIMIT: usize = 8192;
pub const DROP_REASONS: [&str; 5] = [
    "rate-limited",
    "duplicate",
    "hop-loop",
    "hop-runaway",
    "queue-full",
];
pub const NOTICE_STATES: [&str; 3] = ["failed", "refused", "unknown"];
pub const MODES: [&str; 2] = ["prompting", "bypass"];

/// Native receipt status -> Switchboard delivery state.
pub fn receipt_state(status: &str) -> Option<&'static str> {
    match status {
        "held" => Some("held"),
        "denied" => Some("refused"),
        "refused" => Some("refused"),
        "expired" => Some("failed"),
        "dropped" => Some("failed"),
        "delivered" => Some("handed_to_claude"),
        _ => None,
    }
}

/// Every native receipt status, in a fixed order.
pub const RECEIPT_STATUSES: [&str; 6] = [
    "held",
    "denied",
    "refused",
    "expired",
    "dropped",
    "delivered",
];

/// Every Switchboard-authored notice: author line, what happened, what to do.
pub fn notice(name: &str, happened: &str, advice: &str) -> String {
    format!("Switchboard on this machine wrote this, not {name}; don't reply to it.\n\n{happened}\n\n{advice}")
}

pub fn unjoined_text(name: &str) -> String {
    notice(
        name,
        &format!("Your message to {name} was not delivered: this chat hasn't joined Switchboard."),
        "Ask the user to run /switchboard join (or !switchboard join), then resend it.",
    )
}

/// A delivery notice; native receipts cannot say "uncertain" truthfully.
pub fn notice_text(name: &str, state: &str, detail: Option<&Value>, excerpt: &str) -> String {
    let detail = if crate::wirejson::truthy(detail) {
        crate::wirejson::str_value(detail.expect("truthy value"))
    } else {
        String::new()
    };
    let printable: String = detail
        .chars()
        .map(|c| if text::is_printable(c) { c } else { ' ' })
        .collect();
    let mut reason = text::take_chars(&text::collapse_whitespace(&printable), 200);
    if reason.is_empty() {
        reason = if state != "unknown" {
            "no reason was reported"
        } else {
            "no outcome was reported"
        }
        .into();
    }
    let quoted_full = text::collapse_whitespace(excerpt);
    let mut quoted = text::take_chars(&quoted_full, 120);
    if text::char_len(&quoted_full) > 120 {
        quoted.push_str("...");
    }
    let (happened, advice) = if state == "unknown" {
        (
            format!(
                "Switchboard couldn't confirm that your message to {name} arrived ({reason}); \
                 it may or may not have reached that chat. Your message began: \"{quoted}\""
            ),
            "If it matters, resend it once or tell the user.",
        )
    } else {
        (
            format!("Your message to {name} was not delivered ({reason}). Your message began: \"{quoted}\""),
            "Resend it later or tell the user.",
        )
    };
    notice(name, &happened, advice)
}

/// Recognize only canonical native observations, never transport failures.
pub fn receipt_marker(detail: Option<&Value>) -> Option<(String, Option<String>)> {
    let detail = detail?.as_str()?;
    if detail == "delivered" {
        return Some(("delivered".into(), None));
    }
    if text::char_len(detail) > 96 {
        return None;
    }
    let rest = detail.strip_prefix("native receipt: ")?;
    let (status, reason) = match rest.split_once("; drop_reason=") {
        Some((status, reason)) => (status, Some(reason)),
        None => (rest, None),
    };
    if !["held", "denied", "expired", "refused", "dropped"].contains(&status) {
        return None;
    }
    if let Some(reason) = reason {
        if reason.is_empty() || !reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            return None;
        }
        if status != "dropped" || !DROP_REASONS.contains(&reason) {
            return None;
        }
    }
    Some((status.to_string(), reason.map(str::to_string)))
}

fn is_canonical_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, b)| match index {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Whether `value` is a hexadecimal integer literal: surrounding whitespace,
/// an optional sign, an optional `0x` prefix (which may be followed by one
/// `_`), and hex digits with single underscores between them.
fn hex_int_literal(value: &str) -> bool {
    let value = text::strip(value);
    let value = value.strip_prefix(['+', '-']).unwrap_or(value);
    let (value, prefixed) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(rest) => (rest, true),
        None => (value, false),
    };
    let value = if prefixed {
        value.strip_prefix('_').unwrap_or(value)
    } else {
        value
    };
    !value.is_empty()
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value.chars().all(|c| c.is_ascii_hexdigit() || c == '_')
}

/// Accept a canonical hyphenated UUID (either letter case) and refuse
/// anything else, with the error details clients already see: `badly formed
/// hexadecimal UUID string` when it doesn't have 32 hex positions, the
/// `invalid literal` text when they aren't hex, and `invalid UUID` otherwise.
pub fn valid_uuid_str(value: &str) -> Result<String> {
    if is_canonical_uuid(value) {
        return Ok(value.to_string());
    }
    let hex = value.replace("urn:", "").replace("uuid:", "");
    let hex = hex.trim_matches(['{', '}']).replace('-', "");
    if text::char_len(&hex) != 32 {
        return Err(Error::value("badly formed hexadecimal UUID string"));
    }
    if !hex_int_literal(&hex) {
        return Err(Error::value(format!(
            "invalid literal for int() with base 16: {}",
            text::repr_str(&hex)
        )));
    }
    Err(Error::value("invalid UUID"))
}

/// Validate a decoded value as a canonical UUID string (any letter case).
pub fn valid_uuid(value: Option<&Value>) -> Result<String> {
    match value {
        Some(Value::String(value)) => valid_uuid_str(value),
        _ => Err(Error::value("invalid UUID")),
    }
}

/// A display name made safe: drop quotes, angle brackets, and control/format
/// characters, strip whitespace, keep at most 64 code points.
pub fn clean_name(value: &str) -> String {
    let kept: String = value
        .chars()
        .filter(|c| !matches!(c, '"' | '<' | '>') && !text::is_name_removed(*c))
        .collect();
    text::take_chars(text::strip(&kept), 64)
}

fn quote_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_.-~/:\\".contains(&byte)
}

/// A native `uds:` address for a socket path: bytes other than ASCII
/// letters, digits, and `_.-~/:\` are percent-encoded with uppercase hex.
pub fn address(path: &str) -> String {
    let mut out = String::from("uds:");
    for byte in path.bytes() {
        if quote_safe(byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Decode percent escapes as UTF-8, replacing invalid sequences with U+FFFD
/// and leaving malformed escapes as written.
pub fn unquote(value: &str) -> String {
    if !value.contains('%') {
        return value.to_string();
    }
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let bytes = run.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%'
                && index + 2 < bytes.len()
                && bytes[index + 1].is_ascii_hexdigit()
                && bytes[index + 2].is_ascii_hexdigit()
            {
                let pair = std::str::from_utf8(&bytes[index + 1..index + 3]).expect("ascii");
                decoded.push(u8::from_str_radix(pair, 16).expect("hex"));
                index += 3;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        out.push_str(&String::from_utf8_lossy(&decoded));
        run.clear();
    };
    for c in value.chars() {
        if c.is_ascii() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// The resolved socket path a native `uds:` address names.
pub fn path_from_address(value: Option<&Value>) -> Result<String> {
    let value = match value {
        Some(Value::String(value)) if value.starts_with("uds:") => value,
        _ => return Err(Error::value("native uds address required")),
    };
    let path = crate::fsutil::normalize(&unquote(&value[4..]));
    if !crate::fsutil::is_absolute(&path) || crate::fsutil::has_parent_part(&path) {
        return Err(Error::value("invalid native address"));
    }
    crate::fsutil::resolve(&path)
}

/// Envelope attributes, in canonical order.
pub const ATTRS: [&str; 6] = [
    "from",
    "from-session",
    "hop-chain",
    "from-name",
    "from-mode",
    "from-plugin",
];
const OPEN: &str = "<cross-session-message";
const CLOSE: &str = "</cross-session-message";
const SUFFIX: &str = "\n</cross-session-message>";

pub type Attrs = indexmap::IndexMap<String, String>;

pub fn valid_hop_chain(value: Option<&Value>) -> Result<String> {
    let value = match value {
        Some(Value::String(value)) => value,
        _ => return Err(Error::value("invalid native hop chain")),
    };
    let parts: Vec<&str> = value.split(',').collect();
    let ok = (1..=32).contains(&parts.len())
        && parts.iter().all(|part| {
            part.len() == 24 && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
    if ok {
        Ok(value.clone())
    } else {
        Err(Error::value("invalid native hop chain"))
    }
}

/// Build the native attribution envelope around a message body.
pub fn envelope(attrs: &Attrs, body: &str) -> Result<String> {
    let mut out = String::from(OPEN);
    for key in ATTRS {
        if let Some(value) = attrs.get(key) {
            if value.is_empty() || value.contains(['"', '<', '>', '\r', '\n']) {
                return Err(Error::value("invalid envelope attribute"));
            }
            out.push_str(&format!(" {key}=\"{value}\""));
        }
    }
    out.push_str(">\n");
    out.push_str(&body.replace(CLOSE, "<\\/cross-session-message"));
    out.push_str(SUFFIX);
    Ok(out)
}

/// Parse attribute pairs ` key="value"` up to the closing `>`; None if the
/// opening tag does not match the native grammar.
fn parse_attrs(rest: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut pairs = Vec::new();
    let mut rest = rest;
    loop {
        if let Some(after) = rest.strip_prefix('>') {
            return Some((pairs, after));
        }
        let after_space = rest.strip_prefix(' ')?;
        let key_len = after_space
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || *b == b'-')
            .count();
        if key_len == 0 {
            return None;
        }
        let key = &after_space[..key_len];
        let after_key = after_space[key_len..].strip_prefix("=\"")?;
        let value_len = after_key
            .char_indices()
            .find(|(_, c)| matches!(c, '"' | '<' | '>' | '\n' | '\r'))
            .map(|(index, _)| index)?;
        if !after_key[value_len..].starts_with('"') {
            return None;
        }
        pairs.push((key.to_string(), after_key[..value_len].to_string()));
        rest = &after_key[value_len + 1..];
    }
}

/// Parse and canonically check a native attribution envelope.
pub fn parse_envelope(content: Option<&Value>) -> Result<(Attrs, String)> {
    let content = match content {
        Some(Value::String(content)) if content.len() <= TEXT_LIMIT + 4096 => content,
        _ => return Err(Error::value("invalid native content size")),
    };
    let required = || Error::value("native attribution envelope required");
    let rest = content.strip_prefix(OPEN).ok_or_else(required)?;
    let (pairs, after) = parse_attrs(rest).ok_or_else(required)?;
    let after = after.strip_prefix('\n').ok_or_else(required)?;
    if after.len() < SUFFIX.len() || !after.ends_with(SUFFIX) {
        return Err(required());
    }
    let raw_body = &after[..after.len() - SUFFIX.len()];
    let mut attrs = Attrs::new();
    for (key, value) in &pairs {
        attrs.insert(key.clone(), value.clone());
    }
    if attrs.len() != pairs.len() || attrs.keys().any(|key| !ATTRS.contains(&key.as_str())) {
        return Err(Error::value("unsupported envelope attributes"));
    }
    let body = raw_body.replace("<\\/cross-session-message", CLOSE);
    if envelope(&attrs, &body)? != *content {
        return Err(Error::value("noncanonical native envelope"));
    }
    let mode = attrs.get("from-mode").map(String::as_str);
    if !mode.is_some_and(|mode| MODES.contains(&mode)) || attrs.contains_key("from-plugin") {
        return Err(Error::value(
            "missing sender mode or unsupported plugin provenance",
        ));
    }
    if let Some(session) = attrs.get("from-session") {
        valid_uuid_str(session)?;
    }
    if let Some(chain) = attrs.get("hop-chain") {
        valid_hop_chain(Some(&Value::String(chain.clone())))?;
    }
    if body.len() > TEXT_LIMIT || body.is_empty() {
        return Err(Error::value("invalid message text"));
    }
    Ok((attrs, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(pairs: &[(&str, &str)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn envelope_round_trips_and_escapes_close_tag() {
        let a = attrs(&[("from-mode", "prompting"), ("from", "uds:/x")]);
        let text = envelope(&a, "hi </cross-session-message> there").unwrap();
        assert_eq!(
            text,
            "<cross-session-message from=\"uds:/x\" from-mode=\"prompting\">\nhi <\\/cross-session-message> there\n</cross-session-message>"
        );
        let (parsed, body) = parse_envelope(Some(&json!(text))).unwrap();
        assert_eq!(body, "hi </cross-session-message> there");
        assert_eq!(parsed.get("from").unwrap(), "uds:/x");
    }

    #[test]
    fn noncanonical_and_malformed_envelopes_fail() {
        let bad_order = "<cross-session-message from-mode=\"prompting\" from=\"uds:/x\">\nx\n</cross-session-message>";
        assert_eq!(
            parse_envelope(Some(&json!(bad_order))).unwrap_err().message,
            "noncanonical native envelope"
        );
        let empty = "<cross-session-message from=\"\" from-mode=\"prompting\">\nx\n</cross-session-message>";
        assert_eq!(
            parse_envelope(Some(&json!(empty))).unwrap_err().message,
            "invalid envelope attribute"
        );
        let dup = "<cross-session-message from=\"a\" from=\"a\" from-mode=\"prompting\">\nx\n</cross-session-message>";
        assert_eq!(
            parse_envelope(Some(&json!(dup))).unwrap_err().message,
            "unsupported envelope attributes"
        );
        let short = "<cross-session-message>\n</cross-session-message>";
        assert_eq!(
            parse_envelope(Some(&json!(short))).unwrap_err().message,
            "native attribution envelope required"
        );
        let empty_body =
            "<cross-session-message from-mode=\"prompting\">\n\n</cross-session-message>";
        assert_eq!(
            parse_envelope(Some(&json!(empty_body)))
                .unwrap_err()
                .message,
            "invalid message text"
        );
        assert_eq!(
            parse_envelope(Some(&json!(1))).unwrap_err().message,
            "invalid native content size"
        );
    }

    #[test]
    fn uuid_errors() {
        let upper = "12345678-1234-1234-1234-123456789ABC";
        assert_eq!(valid_uuid_str(upper).unwrap(), upper);
        assert_eq!(
            valid_uuid_str("x").unwrap_err().message,
            "badly formed hexadecimal UUID string"
        );
        assert_eq!(
            valid_uuid_str("{12345678-1234-1234-1234-123456789abc}")
                .unwrap_err()
                .message,
            "invalid UUID"
        );
        assert_eq!(
            valid_uuid_str(&"g".repeat(32)).unwrap_err().message,
            format!(
                "invalid literal for int() with base 16: '{}'",
                "g".repeat(32)
            )
        );
        assert_eq!(
            valid_uuid(Some(&json!(5))).unwrap_err().message,
            "invalid UUID"
        );
    }

    #[test]
    fn markers_and_hops() {
        assert_eq!(
            receipt_marker(Some(&json!("delivered"))),
            Some(("delivered".into(), None))
        );
        assert_eq!(
            receipt_marker(Some(&json!(
                "native receipt: dropped; drop_reason=queue-full"
            ))),
            Some(("dropped".into(), Some("queue-full".into())))
        );
        assert_eq!(
            receipt_marker(Some(&json!("native receipt: held; drop_reason=duplicate"))),
            None
        );
        assert_eq!(
            receipt_marker(Some(&json!("native receipt: dropped; drop_reason=other"))),
            None
        );
        assert_eq!(receipt_marker(Some(&json!("native receipt: lost"))), None);
        assert!(valid_hop_chain(Some(&json!("not-a-chain"))).is_err());
        let chain = format!("{},{}", "a".repeat(24), "b".repeat(24));
        assert_eq!(valid_hop_chain(Some(&json!(chain))).unwrap(), chain);
    }

    #[test]
    fn addresses_percent_encode() {
        assert_eq!(address("/a b/é~:\\x"), "uds:/a%20b/%C3%A9~:\\x");
        assert_eq!(unquote("/a%20b/%C3%A9%zz%"), "/a b/é%zz%");
        assert_eq!(unquote("%C3é%A9"), "\u{fffd}é\u{fffd}");
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name(" a\"<b>\u{200b}c\u{2028} "), "abc");
        assert_eq!(clean_name(&"x".repeat(70)).len(), 64);
    }
}
