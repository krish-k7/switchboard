//! Splitting a URL into scheme, network location, path, query, and fragment
//! (the generic RFC 3986 split, lenient about what it accepts), and reading
//! the user, host, and port from it, for the checks the bridge and CLI make
//! on `hub_url`. Error texts are fixed: callers show them and tests pin them.

use crate::error::{Error, Result};
use crate::text::repr_str;

/// The five parts of a split URL; absent parts are empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitResult {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

fn is_c0_or_space(c: char) -> bool {
    (c as u32) <= 0x20
}

/// Split `url`: leading C0 controls and spaces are dropped and tabs and line
/// breaks removed; the scheme is lowercased; a bracketed host must be a valid
/// IPv6 or IPvFuture address.
pub fn urlsplit(url: &str) -> Result<SplitResult> {
    let mut url: String = url
        .trim_start_matches(is_c0_or_space)
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut scheme = String::new();
    let mut netloc = String::new();
    let mut query = String::new();
    let mut fragment = String::new();
    if let Some(i) = url.find(':') {
        let first = url.chars().next().expect("nonempty");
        if i > 0
            && first.is_ascii_alphabetic()
            && url[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            scheme = url[..i].to_ascii_lowercase();
            url = url[i + 1..].to_string();
        }
    }
    if url.starts_with("//") {
        let rest = &url[2..];
        let delim = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        netloc = rest[..delim].to_string();
        url = rest[delim..].to_string();
        let open = netloc.contains('[');
        let close = netloc.contains(']');
        if open != close {
            return Err(Error::value("Invalid IPv6 URL"));
        }
        if open {
            let after = netloc.split_once('[').map(|(_, b)| b).unwrap_or("");
            let host = after.split_once(']').map(|(a, _)| a).unwrap_or(after);
            check_bracketed_host(host)?;
        }
    }
    if let Some((before, after)) = url.split_once('#') {
        fragment = after.to_string();
        url = before.to_string();
    }
    if let Some((before, after)) = url.split_once('?') {
        query = after.to_string();
        url = before.to_string();
    }
    Ok(SplitResult {
        scheme,
        netloc,
        path: url,
        query,
        fragment,
    })
}

fn check_bracketed_host(host: &str) -> Result<()> {
    if let Some(rest) = host.strip_prefix('v') {
        let valid = rest.split_once('.').is_some_and(|(hex, tail)| {
            !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) && !tail.is_empty()
        });
        if !valid {
            return Err(Error::value("IPvFuture address is invalid"));
        }
        return Ok(());
    }
    let address = host.split_once('%').map(|(a, _)| a).unwrap_or(host);
    if address.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(());
    }
    if address.parse::<std::net::Ipv4Addr>().is_ok() && !host.contains('%') {
        return Err(Error::value("An IPv4 address cannot be in brackets"));
    }
    Err(Error::value(format!(
        "{} does not appear to be an IPv4 or IPv6 address",
        repr_str(host)
    )))
}

impl SplitResult {
    fn userinfo(&self) -> Option<&str> {
        self.netloc.rsplit_once('@').map(|(user, _)| user)
    }

    fn hostinfo(&self) -> (&str, Option<&str>) {
        let hostinfo = self
            .netloc
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(&self.netloc);
        let (host, port) = if let Some((_, bracketed)) = hostinfo.split_once('[') {
            let (host, rest) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, rest.split_once(':').map(|(_, p)| p).unwrap_or(""))
        } else {
            hostinfo.split_once(':').unwrap_or((hostinfo, ""))
        };
        (host, if port.is_empty() { None } else { Some(port) })
    }

    /// The user: None without `@`, else the user information before any `:`.
    pub fn username(&self) -> Option<String> {
        self.userinfo().map(|info| {
            info.split_once(':')
                .map(|(u, _)| u)
                .unwrap_or(info)
                .to_string()
        })
    }

    /// The password: None without a `:` in the user information.
    pub fn password(&self) -> Option<String> {
        self.userinfo()
            .and_then(|info| info.split_once(':').map(|(_, p)| p.to_string()))
    }

    /// The host, lowercased (an IPv6 zone after `%` kept as is), None when empty.
    pub fn hostname(&self) -> Option<String> {
        let (host, _) = self.hostinfo();
        if host.is_empty() {
            return None;
        }
        Some(match host.split_once('%') {
            Some((name, zone)) => format!("{}%{}", name.to_lowercase(), zone),
            None => host.to_lowercase(),
        })
    }

    /// The port: None when absent or empty, a value error when it isn't all
    /// digits or is above 65535.
    pub fn port(&self) -> Result<Option<u32>> {
        let Some(port) = self.hostinfo().1 else {
            return Ok(None);
        };
        if !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::value(format!(
                "Port could not be cast to integer value as {}",
                repr_str(port)
            )));
        }
        let digits = port.trim_start_matches('0');
        if digits.len() > 5 || digits.parse::<u32>().unwrap_or(0) > 65535 {
            return Err(Error::value("Port out of range 0-65535"));
        }
        Ok(Some(digits.parse::<u32>().unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_urls_and_reads_parts() {
        let parts = urlsplit("HTTPS://User:pw@Host.Example:8443/path?q=1#frag").unwrap();
        assert_eq!(parts.scheme, "https");
        assert_eq!(parts.netloc, "User:pw@Host.Example:8443");
        assert_eq!(parts.path, "/path");
        assert_eq!(parts.query, "q=1");
        assert_eq!(parts.fragment, "frag");
        assert_eq!(parts.username().as_deref(), Some("User"));
        assert_eq!(parts.password().as_deref(), Some("pw"));
        assert_eq!(parts.hostname().as_deref(), Some("host.example"));
        assert_eq!(parts.port().unwrap(), Some(8443));
        let bare = urlsplit("https://@host").unwrap();
        assert_eq!(bare.username().as_deref(), Some(""));
        assert_eq!(bare.password(), None);
        assert_eq!(urlsplit("https://h:").unwrap().port().unwrap(), None);
        assert_eq!(
            urlsplit("https://h:x").unwrap().port().unwrap_err().message,
            "Port could not be cast to integer value as 'x'"
        );
        assert_eq!(
            urlsplit("https://h:70000")
                .unwrap()
                .port()
                .unwrap_err()
                .message,
            "Port out of range 0-65535"
        );
        assert_eq!(
            urlsplit("https://[::1]:9/").unwrap().hostname().as_deref(),
            Some("::1")
        );
        assert_eq!(
            urlsplit("https://[::1/").unwrap_err().message,
            "Invalid IPv6 URL"
        );
        assert_eq!(urlsplit("  \thttps://a\n/b").unwrap().netloc, "a");
        assert_eq!(urlsplit("localhost:1").unwrap().scheme, "localhost");
        assert_eq!(urlsplit("1http://x").unwrap().scheme, "");
    }
}
