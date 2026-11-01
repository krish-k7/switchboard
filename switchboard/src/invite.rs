//! The invite token: one string that `switchboard server invite` prints and
//! `switchboard init` reads, carrying everything a new machine
//! needs to join: the server's address, the SHA-256 of its certificate when
//! the certificate is private (clients pin it), a random secret, and the
//! machine name when the invite fixes one.
//!
//! Format: `sbi1.` followed by the unpadded base64url encoding of a JSON
//! object `{"url": ..., "pin": ..., "secret": ..., "machine": ...}` (`pin` and
//! `machine` only when present). The secret is base64url of at least 16
//! random bytes from the kernel; the server keeps only its SHA-256.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The prefix that names the format and its version.
pub const PREFIX: &str = "sbi1.";
/// Random bytes in a secret the server makes (192 bits).
pub const SECRET_BYTES: usize = 24;
/// The fewest secret bytes a token may carry (128 bits).
const MIN_SECRET_BYTES: usize = 16;
const MAX_TOKEN: usize = 2048;

/// A parsed invite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// The server's address, `https://host[:port]`.
    pub url: String,
    /// The server certificate's SHA-256 (64 lowercase hex digits), for a
    /// server with a private certificate.
    pub pin: Option<String>,
    /// The secret, base64url as sent to the server.
    pub secret: String,
    /// The machine name the invite is for, when it fixes one.
    pub machine: Option<String>,
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url.
pub fn base64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let value = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..=chunk.len() {
            out.push(ALPHABET[(value >> (18 - 6 * index) & 63) as usize] as char);
        }
    }
    out
}

/// Decode unpadded base64url; `None` for any other character, padding, a
/// dangling character, or nonzero trailing bits (so each value has exactly
/// one encoding).
pub fn unbase64url(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u32> = text
        .bytes()
        .map(|b| ALPHABET.iter().position(|a| *a == b).map(|p| p as u32))
        .collect::<Option<_>>()?;
    if digits.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        let mut value = 0u32;
        for (index, digit) in chunk.iter().enumerate() {
            value |= digit << (18 - 6 * index);
        }
        let bytes = [(value >> 16) as u8, (value >> 8) as u8, value as u8];
        let keep = chunk.len() - 1;
        let rest = match keep {
            1 => value & 0xffff,
            2 => value & 0xff,
            _ => 0,
        };
        if rest != 0 {
            return None;
        }
        out.extend_from_slice(&bytes[..keep]);
    }
    Some(out)
}

/// A machine name the server accepts: lowercase letters, digits, and
/// hyphens, starting with a letter, at most 48 characters.
pub fn is_machine_name(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn is_pin(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A server address an invite can carry: `https://HOST[:PORT]` and nothing
/// else (no user, path, query, or fragment), where HOST is a DNS name
/// (letters, digits, and hyphens in dot-separated labels), an IPv4 address,
/// or an IPv6 address in brackets, and PORT is 1 to 65535. `setup` checks an
/// invite's address with this, and `switchboard server invite` checks the
/// address before it makes an invite.
pub fn is_url(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("https://") else {
        return false;
    };
    if text.len() > 256 {
        return false;
    }
    let (host, port) = if let Some(after) = rest.strip_prefix('[') {
        let Some((address, tail)) = after.split_once(']') else {
            return false;
        };
        if address.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match tail {
            "" => (None, None),
            _ => match tail.strip_prefix(':') {
                Some(port) => (None, Some(port)),
                None => return false,
            },
        }
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (Some(host), Some(port)),
            None => (Some(rest), None),
        }
    };
    if let Some(port) = port {
        let valid = !port.is_empty()
            && port.len() <= 5
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n));
        if !valid {
            return false;
        }
    }
    match host {
        None => true,
        Some(host) => is_host_name(host),
    }
}

/// A DNS host name or a dotted IPv4 address.
fn is_host_name(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    // All digits and dots: only a real IPv4 address.
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return host.parse::<std::net::Ipv4Addr>().is_ok();
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// The SHA-256 of a secret, in hex: what the server stores and compares.
pub fn secret_hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A new secret from the kernel's random source.
pub fn new_secret() -> String {
    base64url(&crate::identity::random_bytes::<SECRET_BYTES>())
}

/// Whether `secret` has the shape of a secret (base64url of 16 to 64 bytes).
pub fn is_secret(secret: &str) -> bool {
    secret.len() <= 86 && unbase64url(secret).is_some_and(|bytes| bytes.len() >= MIN_SECRET_BYTES)
}

impl Invite {
    /// The token string.
    pub fn encode(&self) -> String {
        let mut map = Map::new();
        map.insert("url".into(), Value::from(self.url.clone()));
        if let Some(pin) = &self.pin {
            map.insert("pin".into(), Value::from(pin.clone()));
        }
        map.insert("secret".into(), Value::from(self.secret.clone()));
        if let Some(machine) = &self.machine {
            map.insert("machine".into(), Value::from(machine.clone()));
        }
        let json = serde_json::to_string(&Value::Object(map)).expect("plain JSON");
        format!("{PREFIX}{}", base64url(json.as_bytes()))
    }

    /// Parse a token, as pasted: surrounding white space is ignored.
    pub fn parse(text: &str) -> Result<Invite, &'static str> {
        const MALFORMED: &str = "that is not a Switchboard invite (it starts with sbi1.)";
        let text = text.trim();
        if text.len() > MAX_TOKEN {
            return Err(MALFORMED);
        }
        let Some(body) = text.strip_prefix(PREFIX) else {
            return Err(MALFORMED);
        };
        let json = unbase64url(body).ok_or("the invite is damaged; copy it again in full")?;
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&json) else {
            return Err("the invite is damaged; copy it again in full");
        };
        let known = ["url", "pin", "secret", "machine"];
        if map.keys().any(|key| !known.contains(&key.as_str())) {
            return Err("the invite has fields this Switchboard does not know; update it");
        }
        let text_field = |key: &str| -> Result<Option<String>, &'static str> {
            match map.get(key) {
                None => Ok(None),
                Some(Value::String(value)) => Ok(Some(value.clone())),
                Some(_) => Err("the invite is damaged; copy it again in full"),
            }
        };
        let url = text_field("url")?
            .filter(|url| is_url(url))
            .ok_or("the invite has no valid server address")?;
        let pin = text_field("pin")?;
        if pin.as_deref().is_some_and(|pin| !is_pin(pin)) {
            return Err("the invite's server certificate fingerprint is damaged");
        }
        let secret = text_field("secret")?
            .filter(|secret| is_secret(secret))
            .ok_or("the invite's secret is damaged; copy it again in full")?;
        let machine = text_field("machine")?;
        if machine
            .as_deref()
            .is_some_and(|name| !is_machine_name(name))
        {
            return Err("the invite names an invalid machine");
        }
        Ok(Invite {
            url,
            pin,
            secret,
            machine,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Invite {
        Invite {
            url: "https://10.0.0.5:8743".into(),
            pin: Some("ab".repeat(32)),
            secret: new_secret(),
            machine: Some("new-box".into()),
        }
    }

    #[test]
    fn base64url_round_trips_and_is_canonical() {
        for length in 0..40 {
            let data: Vec<u8> = (0..length).map(|i| (i * 37 + 11) as u8).collect();
            let text = base64url(&data);
            assert!(!text.contains('='));
            assert_eq!(unbase64url(&text).unwrap(), data);
        }
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
        assert_eq!(unbase64url("Zm8").unwrap(), b"fo");
        // Padding, other alphabets, a dangling digit, and nonzero trailing bits.
        for bad in ["Zg==", "Zm+v", "Zm/v", "Z", "Zh", "Zm9"] {
            assert!(unbase64url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tokens_round_trip_with_and_without_optional_fields() {
        let full = sample();
        let token = full.encode();
        assert!(token.starts_with(PREFIX));
        assert!(token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
        assert_eq!(Invite::parse(&token).unwrap(), full);
        assert_eq!(Invite::parse(&format!("  {token}\n")).unwrap(), full);
        let bare = Invite {
            pin: None,
            machine: None,
            ..full
        };
        let token = bare.encode();
        assert_eq!(Invite::parse(&token).unwrap(), bare);
        let json = String::from_utf8(unbase64url(&token[PREFIX.len()..]).unwrap()).unwrap();
        assert!(!json.contains("pin") && !json.contains("machine"), "{json}");
    }

    #[test]
    fn server_addresses_are_a_host_and_port_only() {
        for good in [
            "https://x",
            "https://switchboard.lan:8743",
            "https://a-b.example.com",
            "https://10.0.0.5:8743",
            "https://[::1]",
            "https://[fe80::1]:443",
            "https://h:1",
            "https://h:65535",
        ] {
            assert!(is_url(good), "{good}");
        }
        for bad in [
            "http://x",
            "https://",
            "https://x/",
            "https://x/path",
            "https://x?q=1",
            "https://x#f",
            "https://user@x",
            "https://user:pw@x",
            "https://x:0",
            "https://x:65536",
            "https://x:99999",
            "https://x:",
            "https://x:+80",
            "https://x:80:80",
            "https://:443",
            "https://.",
            "https://a..b",
            "https://-a.b",
            "https://a-.b",
            "https://a_b",
            "https://999.1.1.1",
            "https://1.2.3",
            "https://::1",
            "https://[::1",
            "https://[nothost]",
            "https://[::1]x",
            "https://[::1]:0",
            "https://x\r\nInjected: 1",
            "https://x y",
        ] {
            assert!(!is_url(bad), "{bad:?}");
        }
        assert!(!is_url(&format!("https://{}", "a".repeat(64))));
        assert!(!is_url(&format!("https://{}.x", "a.".repeat(130))));
    }

    #[test]
    fn secrets_carry_at_least_128_bits() {
        let secret = new_secret();
        assert_eq!(unbase64url(&secret).unwrap().len(), SECRET_BYTES);
        assert_ne!(secret, new_secret());
        assert!(is_secret(&base64url(&[7; 16])));
        assert!(!is_secret(&base64url(&[7; 15])));
        assert!(!is_secret(&base64url(&[7; 65])));
        assert_eq!(secret_hash("x").len(), 64);
    }

    #[test]
    fn damaged_or_foreign_tokens_are_refused() {
        let good = sample();
        let token = good.encode();
        let wrap = |json: &str| format!("{PREFIX}{}", base64url(json.as_bytes()));
        let secret = &good.secret;
        let cases = [
            String::new(),
            "hello".into(),
            token[PREFIX.len()..].to_string(),
            format!("sbi2.{}", &token[PREFIX.len()..]),
            token[..token.len() - 3].to_string(),
            format!("{token}!"),
            wrap("[]"),
            wrap("{"),
            wrap(&format!(r#"{{"secret": "{secret}"}}"#)),
            wrap(&format!(r#"{{"url": "http://x", "secret": "{secret}"}}"#)),
            wrap(&format!(
                r#"{{"url": "https://x/path", "secret": "{secret}"}}"#
            )),
            wrap(&format!(r#"{{"url": "https://", "secret": "{secret}"}}"#)),
            wrap(r#"{"url": "https://x"}"#),
            wrap(r#"{"url": "https://x", "secret": "short"}"#),
            wrap(&format!(
                r#"{{"url": "https://x", "secret": "{secret}", "pin": "AB"}}"#
            )),
            wrap(&format!(
                r#"{{"url": "https://x", "secret": "{secret}", "pin": "{}"}}"#,
                "AB".repeat(32)
            )),
            wrap(&format!(
                r#"{{"url": "https://x", "secret": "{secret}", "machine": "Bad_Name"}}"#
            )),
            wrap(&format!(
                r#"{{"url": "https://x", "secret": "{secret}", "machine": 5}}"#
            )),
            wrap(&format!(
                r#"{{"url": "https://x", "secret": "{secret}", "admin": true}}"#
            )),
            format!("{PREFIX}{}", "A".repeat(MAX_TOKEN)),
        ];
        for case in &cases {
            assert!(Invite::parse(case).is_err(), "accepted {case:?}");
        }
        assert!(Invite::parse(&wrap(&format!(
            r#"{{"url": "https://[::1]:8743", "secret": "{secret}"}}"#
        )))
        .is_ok());
    }

    #[test]
    fn machine_names() {
        assert!(is_machine_name("a"));
        assert!(is_machine_name(&format!("a{}", "b".repeat(47))));
        assert!(!is_machine_name(&format!("a{}", "b".repeat(48))));
        for bad in ["", "1box", "Box", "box_1", "-box", "bo x"] {
            assert!(!is_machine_name(bad), "{bad}");
        }
    }
}
