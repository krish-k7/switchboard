//! Finding and downloading a GitHub release, as `install.sh` does: the
//! asset for this platform and the release's `SHA256SUMS`, from the public
//! download addresses, or through the API with `GITHUB_TOKEN` while the
//! repository is private. The same environment variables select the
//! repository and the GitHub addresses (`SWITCHBOARD_REPO`,
//! `SWITCHBOARD_GITHUB`, `SWITCHBOARD_GITHUB_API`).
//!
//! Requests use HTTPS with the system's roots and Mozilla's; plain HTTP is
//! allowed only to the loopback addresses tests serve from. A token is sent
//! only to the host it was meant for, never to a host a redirect leads to.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const DEFAULT_REPO: &str = "krish-k7/switchboard";
const TIMEOUT: Duration = Duration::from_secs(30);
/// Larger than any release asset.
const LIMIT: usize = 256 * 1024 * 1024;
const REDIRECTS: usize = 10;

fn fail(message: impl Into<String>) -> Error {
    Error::value(message)
}

/// A parsed `http` or `https` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Url {
    https: bool,
    host: String,
    port: u16,
    target: String,
}

impl Url {
    fn parse(url: &str) -> Result<Url> {
        let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(fail(format!("unsupported address: {url}")));
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, target) = rest.split_at(end);
        let target = target.split('#').next().unwrap_or("");
        let target = if target.is_empty() {
            "/".to_string()
        } else if target.starts_with('?') {
            format!("/{target}")
        } else {
            target.to_string()
        };
        if authority.contains('@') || authority.is_empty() {
            return Err(fail(format!("unsupported address: {url}")));
        }
        let (host, port) = match authority.strip_prefix('[') {
            Some(bracketed) => {
                let (host, after) = bracketed
                    .split_once(']')
                    .ok_or_else(|| fail(format!("unsupported address: {url}")))?;
                (host.to_string(), after.strip_prefix(':'))
            }
            None => match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), Some(port)),
                None => (authority.to_string(), None),
            },
        };
        let port = match port {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| fail(format!("unsupported address: {url}")))?,
            None if https => 443,
            None => 80,
        };
        let url = Url {
            https,
            host: host.to_ascii_lowercase(),
            port,
            target,
        };
        if !url.https && !url.loopback() {
            return Err(fail(format!(
                "refusing to download over plain HTTP from {}",
                url.host
            )));
        }
        Ok(url)
    }

    fn loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }

    fn origin(&self) -> (bool, String, u16) {
        (self.https, self.host.clone(), self.port)
    }

    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == if self.https { 443 } else { 80 } {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// Resolve a `Location` header against this URL.
    fn join(&self, location: &str) -> Result<Url> {
        if location.starts_with("http://") || location.starts_with("https://") {
            return Url::parse(location);
        }
        let scheme = if self.https { "https" } else { "http" };
        let base = format!("{scheme}://{}", self.host_header());
        if location.starts_with("//") {
            return Url::parse(&format!("{scheme}:{location}"));
        }
        if location.starts_with('/') {
            return Url::parse(&format!("{base}{location}"));
        }
        let path = self.target.split('?').next().unwrap_or("/");
        let directory = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        Url::parse(&format!("{base}{directory}{location}"))
    }
}

/// One HTTP response.
pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = crate::hub_transport::system_roots();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

fn connect(url: &Url) -> Result<TcpStream> {
    let stream = crate::hub_transport::tcp_connect(&url.host, url.port)
        .map_err(|error| fail(format!("could not reach {}: {error}", url.host)))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|exc| fail(format!("could not reach {}: {exc}", url.host)))?;
    Ok(stream)
}

/// One GET request (`Connection: close`), without following redirects.
fn request(url: &Url, headers: &[(String, String)]) -> Result<Response> {
    let mut head = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: switchboard/{}\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        url.target,
        url.host_header(),
        crate::VERSION
    );
    for (name, value) in headers {
        if value.contains(['\r', '\n']) {
            return Err(fail("invalid request header"));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut sock = connect(url)?;
    let mut raw = Vec::new();
    let read = |stream: &mut dyn Read, raw: &mut Vec<u8>| -> std::io::Result<()> {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    raw.extend_from_slice(&buffer[..count]);
                    if raw.len() > LIMIT + 65536 {
                        return Err(std::io::Error::other("response too large"));
                    }
                }
                Err(exc) if exc.kind() == std::io::ErrorKind::Interrupted => {}
                // A server that closes without close_notify after a complete
                // response: the length checks below decide.
                Err(exc) if exc.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(exc) => return Err(exc),
            }
        }
    };
    let failed = |exc: std::io::Error| {
        let reason = match exc.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "timed out".to_string()
            }
            _ => exc.to_string(),
        };
        fail(format!("download from {} failed: {reason}", url.host))
    };
    if url.https {
        let name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|_| fail(format!("invalid host name: {}", url.host)))?;
        let connection = rustls::ClientConnection::new(tls_config(), name)
            .map_err(|exc| fail(exc.to_string()))?;
        let mut stream = rustls::StreamOwned::new(connection, &mut sock);
        stream.write_all(head.as_bytes()).map_err(failed)?;
        stream.flush().map_err(failed)?;
        read(&mut stream, &mut raw).map_err(failed)?;
    } else {
        sock.write_all(head.as_bytes()).map_err(failed)?;
        read(&mut sock, &mut raw).map_err(failed)?;
    }
    parse_response(&raw).map_err(|detail| fail(format!("{} sent {detail}", url.host)))
}

fn parse_response(raw: &[u8]) -> std::result::Result<Response, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("a truncated response")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "an invalid response")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("an invalid status line")?;
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("an invalid header")?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    let body = &raw[split + 4..];
    let chunked = response
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let content = if chunked {
        let mut out = Vec::new();
        let mut rest = body;
        loop {
            let end = rest
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or("a truncated body")?;
            let size = std::str::from_utf8(&rest[..end])
                .ok()
                .and_then(|text| {
                    usize::from_str_radix(text.split(';').next().unwrap_or("").trim(), 16).ok()
                })
                .ok_or("an invalid chunk")?;
            rest = &rest[end + 2..];
            if size == 0 {
                break;
            }
            out.extend_from_slice(rest.get(..size).ok_or("a truncated body")?);
            rest = rest.get(size + 2..).ok_or("a truncated body")?;
        }
        out
    } else {
        match response.header("content-length") {
            Some(length) => {
                let length: usize = length.parse().map_err(|_| "an invalid length")?;
                body.get(..length).ok_or("a truncated body")?.to_vec()
            }
            None => body.to_vec(),
        }
    };
    Ok(Response {
        body: content,
        ..response
    })
}

/// GET `url`, following redirects. `auth` is sent only to `url`'s own
/// origin, so a token never reaches a storage host a redirect leads to.
fn get(url: &str, headers: &[(String, String)], auth: Option<&str>) -> Result<Response> {
    let first = Url::parse(url)?;
    let mut current = first.clone();
    for _ in 0..=REDIRECTS {
        let mut all = headers.to_vec();
        if let Some(token) = auth {
            if current.origin() == first.origin() {
                all.push(("Authorization".into(), format!("Bearer {token}")));
            }
        }
        let response = request(&current, &all)?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            let location = response
                .header("location")
                .ok_or_else(|| fail("a redirect without a location"))?;
            current = current.join(location)?;
            continue;
        }
        return Ok(response);
    }
    Err(fail(format!("too many redirects from {url}")))
}

/// Where releases come from.
pub struct Source {
    repo: String,
    web: String,
    api: String,
    token: Option<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl Source {
    pub fn from_env() -> Result<Source> {
        let token = env("GITHUB_TOKEN");
        if let Some(token) = &token {
            if !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
            {
                return Err(fail(
                    "GITHUB_TOKEN has characters a GitHub token does not have",
                ));
            }
        }
        let trim = |value: String| value.trim_end_matches('/').to_string();
        Ok(Source {
            repo: env("SWITCHBOARD_REPO").unwrap_or_else(|| DEFAULT_REPO.into()),
            web: trim(env("SWITCHBOARD_GITHUB").unwrap_or_else(|| "https://github.com".into())),
            api: trim(
                env("SWITCHBOARD_GITHUB_API").unwrap_or_else(|| "https://api.github.com".into()),
            ),
            token,
        })
    }

    fn private_hint(&self) -> &'static str {
        if self.token.is_some() {
            ", or GITHUB_TOKEN cannot read it"
        } else {
            " (while the repository is private, set GITHUB_TOKEN)"
        }
    }

    /// The release's API description (token path only).
    fn release_json(&self, tag: Option<&str>) -> Result<serde_json::Map<String, Value>> {
        let url = match tag {
            Some(tag) => format!("{}/repos/{}/releases/tags/{tag}", self.api, self.repo),
            None => format!("{}/repos/{}/releases/latest", self.api, self.repo),
        };
        let response = get(
            &url,
            &[("Accept".into(), "application/vnd.github+json".into())],
            self.token.as_deref(),
        )?;
        if response.status != 200 {
            return Err(fail(format!(
                "no release {} in {}{}",
                tag.unwrap_or("(latest)"),
                self.repo,
                self.private_hint()
            )));
        }
        match serde_json::from_slice(&response.body) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(fail("GitHub returned an invalid release description")),
        }
    }

    /// The latest release's tag, such as `v0.2.0`.
    pub fn latest_tag(&self) -> Result<String> {
        if self.token.is_some() {
            return match self.release_json(None)?.get("tag_name") {
                Some(Value::String(tag)) if !tag.is_empty() => Ok(tag.clone()),
                _ => Err(fail("GitHub returned an invalid release description")),
            };
        }
        // github.com/OWNER/REPO/releases/latest redirects to .../releases/tag/TAG.
        let url = Url::parse(&format!("{}/{}/releases/latest", self.web, self.repo))?;
        let response = request(&url, &[])?;
        let tag = response
            .header("location")
            .filter(|_| (300..400).contains(&response.status))
            .and_then(|location| location.split_once("/releases/tag/"))
            .map(|(_, tag)| tag.split(['?', '#', '/']).next().unwrap_or("").to_string())
            .filter(|tag| !tag.is_empty());
        tag.ok_or_else(|| {
            fail(format!(
                "could not find the latest release of {}{}",
                self.repo,
                self.private_hint()
            ))
        })
    }

    /// The named assets of release `tag`, in order.
    pub fn download(&self, tag: &str, names: &[&str]) -> Result<Vec<Vec<u8>>> {
        let mut files = Vec::new();
        if self.token.is_some() {
            let release = self.release_json(Some(tag))?;
            let assets = release
                .get("assets")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for name in names {
                let url = assets
                    .iter()
                    .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
                    .and_then(|asset| asset.get("url").and_then(Value::as_str))
                    .ok_or_else(|| fail(format!("release {tag} has no {name}")))?;
                let response = get(
                    url,
                    &[("Accept".into(), "application/octet-stream".into())],
                    self.token.as_deref(),
                )?;
                if response.status != 200 {
                    return Err(fail(format!(
                        "could not download {name} (HTTP {})",
                        response.status
                    )));
                }
                files.push(response.body);
            }
            return Ok(files);
        }
        let base = format!("{}/{}/releases/download/{tag}", self.web, self.repo);
        for name in names {
            let response = get(&format!("{base}/{name}"), &[], None)?;
            if response.status != 200 {
                return Err(fail(format!(
                    "could not download {name} from {base} (HTTP {}){}",
                    response.status,
                    self.private_hint()
                )));
            }
            files.push(response.body);
        }
        Ok(files)
    }
}

/// The release asset for this machine, as `install.sh` names it.
pub fn asset_name() -> Result<String> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return Err(fail("releases are built for macOS and Linux only"));
    };
    let mut arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        return Err(fail("releases are built for x86_64 and arm64 only"));
    };
    // An x86_64 build under Rosetta on Apple silicon gets the native binary.
    if os == "darwin" && arch == "x86_64" && rosetta() {
        arch = "arm64";
    }
    Ok(format!("switchboard-{os}-{arch}"))
}

#[cfg(target_os = "macos")]
fn rosetta() -> bool {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let name = c"sysctl.proc_translated";
    // SAFETY: sysctlbyname writes at most `size` bytes into `value`.
    let found = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut libc::c_int as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    found && value == 1
}

#[cfg(not(target_os = "macos"))]
fn rosetta() -> bool {
    false
}

/// Check `data` against its line in `SHA256SUMS`.
pub fn verify(sums: &[u8], name: &str, data: &[u8]) -> Result<()> {
    let text = String::from_utf8_lossy(sums);
    let expected = text
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, file)| {
            let file = file.trim();
            file == name || file.strip_prefix('*') == Some(name)
        })
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .ok_or_else(|| fail(format!("SHA256SUMS lists no {name}")))?;
    let actual: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(fail(format!(
            "checksum mismatch for {name} (expected {expected}, got {actual}); nothing was installed"
        )));
    }
    Ok(())
}

/// A release version, compared as semantic versions are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    /// Parse `[v]MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`. The whole text is
    /// checked: pre-release and build identifiers are non-empty and use only
    /// `[0-9A-Za-z-]` (semver), so nothing else (line breaks, `/`, `?`,
    /// `..`) can reach a request line or URL path built from a version.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        let identifiers = |part: &str| {
            part.split('.').all(|identifier| {
                !identifier.is_empty()
                    && identifier
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        };
        let text = match text.split_once('+') {
            Some((text, build)) if identifiers(build) => text,
            Some(_) => return None,
            None => text,
        };
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if identifiers(pre) => {
                (core, pre.split('.').map(str::to_string).collect())
            }
            Some(_) => return None,
            None => (text, Vec::new()),
        };
        let numbers: Vec<u64> = core
            .split('.')
            .map(|part| {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                    None
                } else {
                    part.parse().ok()
                }
            })
            .collect::<Option<_>>()?;
        let core: [u64; 3] = numbers.try_into().ok()?;
        Some(Version { core, pre })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.core.cmp(&other.core) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_as_semver() {
        let v = |text| Version::parse(text).unwrap();
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("v1.0.0") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-beta"));
        assert_eq!(v("1.2.3"), v("v1.2.3"));
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.x.3").is_none());
        assert_eq!(v("1.0.0-rc.1+build.5"), v("1.0.0-rc.1"));
        assert!(v("1.0.0-x-y.2") < v("1.0.0"));
    }

    #[test]
    fn versions_admit_only_semver_characters() {
        for bad in [
            "9.9.9-x\r\nX-Injected: 1",
            "9.9.9+../../etc",
            "1.0.0-../../../evil?x=1",
            "1.0.0-rc/1",
            "1.0.0-rc..1",
            "1.0.0-",
            "1.0.0+",
            "1.0.0-rc.",
            "1.0.0+a+b",
            "1.0.0 ",
            "1.0.0-rc#1",
            "1.0.0\n",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn urls_and_redirect_targets() {
        let url = Url::parse("https://github.com/o/r/releases/latest").unwrap();
        assert_eq!((url.host.as_str(), url.port), ("github.com", 443));
        assert_eq!(
            url.join("/o/r/releases/tag/v1.0.0").unwrap().target,
            "/o/r/releases/tag/v1.0.0"
        );
        assert_eq!(url.join("other").unwrap().target, "/o/r/releases/other");
        let storage = url.join("https://objects.example.com/a?b=c").unwrap();
        assert_eq!(
            (storage.host.as_str(), storage.target.as_str()),
            ("objects.example.com", "/a?b=c")
        );
        assert!(Url::parse("http://github.com/x").is_err());
        assert!(Url::parse("http://127.0.0.1:8080/x").is_ok());
        assert!(Url::parse("ftp://x/y").is_err());
        assert!(Url::parse("https://user@host/").is_err());
    }

    #[test]
    fn checksums_match_their_line() {
        let data = b"binary";
        let hash: String = Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!(
            "{}  other\n{hash}  switchboard-linux-x86_64\n",
            "0".repeat(64)
        );
        assert!(verify(sums.as_bytes(), "switchboard-linux-x86_64", data).is_ok());
        let starred = format!("{hash} *switchboard-linux-x86_64\n");
        assert!(verify(starred.as_bytes(), "switchboard-linux-x86_64", data).is_ok());
        assert!(
            verify(sums.as_bytes(), "switchboard-linux-x86_64", b"tampered")
                .unwrap_err()
                .message
                .contains("checksum mismatch")
        );
        assert!(verify(sums.as_bytes(), "missing", data)
            .unwrap_err()
            .message
            .contains("lists no missing"));
    }

    #[test]
    fn responses_parse_with_length_or_chunks() {
        let plain = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcdef").unwrap();
        assert_eq!((plain.status, plain.body.as_slice()), (200, &b"abc"[..]));
        let chunked = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(chunked.body, b"abcde");
        let redirect =
            parse_response(b"HTTP/1.1 302 Found\r\nLocation: /x\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        assert_eq!(redirect.header("LOCATION"), Some("/x"));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc").is_err());
    }
}
