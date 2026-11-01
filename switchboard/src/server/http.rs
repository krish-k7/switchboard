//! One HTTP/1.0 exchange per connection: request-line and header parsing
//! (header blocks parsed as MIME message headers), size limits and read
//! deadlines, fixed status lines and headers, and the JSON error envelope.
//! Older clients rely on these exact bytes. Every response closes its
//! connection.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::hub::{Fail, Hub, MAX_BODY};
use super::jsonval::{self, dict, Val};
use crate::text;

pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LINE: usize = 65536;
const MAX_HEADERS: usize = 100;

/// The byte stream under one request.
pub enum Link {
    Unix(UnixStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Link {
    fn fd(&self) -> i32 {
        match self {
            Link::Unix(stream) => stream.as_raw_fd(),
            Link::Tls(stream) => stream.sock.as_raw_fd(),
        }
    }

    fn set_read_timeout(&self, timeout: Duration) {
        let _ = match self {
            Link::Unix(stream) => stream.set_read_timeout(Some(timeout)),
            Link::Tls(stream) => stream.sock.set_read_timeout(Some(timeout)),
        };
    }

    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Link::Unix(stream) => stream.read(buffer),
            Link::Tls(stream) => match stream.read(buffer) {
                // A peer that closes without close_notify reads as end of
                // stream (a ragged EOF is not an error).
                Err(exc) if exc.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Link::Unix(stream) => stream.write_all(data),
            Link::Tls(stream) => {
                stream.write_all(data)?;
                stream.flush()
            }
        }
    }

    /// End the write side (after TLS close_notify), then close.
    pub fn finish(mut self) {
        if let Link::Tls(stream) = &mut self {
            stream.conn.send_close_notify();
            let _ = stream.flush();
        }
        // SAFETY: the descriptor stays open until self drops below.
        unsafe { libc::shutdown(self.fd(), libc::SHUT_WR) };
    }
}

/// Who is asking: the Unix ingress trusts the proxy's identity headers; a
/// TLS listener knows the client certificate and address itself.
pub enum Identity {
    Proxy,
    Certificate {
        fingerprint: String,
        address: String,
    },
}

/// Buffered reads with two timers: a socket timeout per read, and a deadline
/// after which the socket is shut down (reads then see end of stream and
/// writes fail).
struct Conn {
    link: Link,
    buffer: Vec<u8>,
    start: usize,
    deadline: Option<Instant>,
    expired: bool,
}

enum ReadError {
    /// A timeout or a connection reset: no response is attempted.
    Quiet,
    /// Any other I/O error.
    Other,
}

impl From<io::Error> for ReadError {
    fn from(exc: io::Error) -> Self {
        use io::ErrorKind::*;
        match exc.kind() {
            WouldBlock | TimedOut | ConnectionReset | ConnectionAborted | BrokenPipe => {
                ReadError::Quiet
            }
            _ => ReadError::Other,
        }
    }
}

impl Conn {
    fn expire(&mut self) {
        if !self.expired {
            // SAFETY: shutting down an open descriptor.
            unsafe { libc::shutdown(self.link.fd(), libc::SHUT_RDWR) };
            self.expired = true;
        }
    }

    fn fill(&mut self) -> Result<usize, ReadError> {
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        loop {
            if self.expired {
                return Ok(0);
            }
            let timeout = match self.deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        self.expire();
                        return Ok(0);
                    }
                    (deadline - now).min(TIMEOUT)
                }
                None => TIMEOUT,
            };
            self.link.set_read_timeout(timeout);
            let mut chunk = [0u8; 16384];
            match self.link.read(&mut chunk) {
                Ok(count) => {
                    self.buffer.extend_from_slice(&chunk[..count]);
                    return Ok(count);
                }
                Err(exc) if exc.kind() == io::ErrorKind::Interrupted => continue,
                Err(exc)
                    if matches!(
                        exc.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.deadline.is_some_and(|d| Instant::now() >= d) {
                        self.expire();
                        return Ok(0);
                    }
                    return Err(ReadError::Quiet);
                }
                Err(exc) => return Err(exc.into()),
            }
        }
    }

    /// One line including its `\n`, cut at `limit` bytes; the rest of the
    /// stream when it ends first.
    fn readline(&mut self, limit: usize) -> Result<Vec<u8>, ReadError> {
        let mut scanned = 0;
        loop {
            let available = &self.buffer[self.start..];
            if let Some(index) = available[scanned..].iter().position(|b| *b == b'\n') {
                let end = (scanned + index + 1).min(limit);
                return Ok(self.take(end));
            }
            if available.len() >= limit {
                return Ok(self.take(limit));
            }
            scanned = available.len();
            if self.fill()? == 0 {
                let rest = self.buffer.len() - self.start;
                return Ok(self.take(rest));
            }
        }
    }

    /// Up to `length` bytes, fewer at end of stream.
    fn read(&mut self, length: usize) -> Result<Vec<u8>, ReadError> {
        while self.buffer.len() - self.start < length {
            if self.fill()? == 0 {
                break;
            }
        }
        let count = length.min(self.buffer.len() - self.start);
        Ok(self.take(count))
    }

    fn take(&mut self, count: usize) -> Vec<u8> {
        let line = self.buffer[self.start..self.start + count].to_vec();
        self.start += count;
        line
    }
}

/// Header fields in order: names compared case-insensitively, values as parsed.
struct Headers(Vec<(String, String)>);

impl Headers {
    fn get_all(&self, name: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.get_all(name).into_iter().next()
    }

    /// The media type, lowercased, without parameters; `text/plain` when absent
    /// or not of the form `type/subtype`.
    fn content_type(&self) -> String {
        let Some(value) = self.get("content-type") else {
            return "text/plain".into();
        };
        let main = value.split(';').next().unwrap_or("");
        let ctype = text::strip(main).to_lowercase();
        if ctype.matches('/').count() != 1 {
            return "text/plain".into();
        }
        ctype
    }
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| *b as char).collect()
}

/// Parse a decoded header block: a line ends at CRLF, CR, or LF; folded
/// lines continue the field before them; the block ends at the first line
/// that is not a field, a continuation, or a Unix `From ` line.
fn parse_header_block(block: &str) -> Headers {
    let mut lines: Vec<&str> = Vec::new();
    let mut rest = block;
    while !rest.is_empty() {
        let end = match rest.find(['\r', '\n']) {
            None => rest.len(),
            Some(index) if rest[index..].starts_with("\r\n") => index + 2,
            Some(index) => index + 1,
        };
        lines.push(&rest[..end]);
        rest = &rest[end..];
    }
    // Header lines continue while they look like a field, a continuation, or
    // a Unix "From " line; the first other line (normally the blank one) ends them.
    let is_header = |line: &str| {
        if line.starts_with("From ") || line.starts_with([' ', '\t']) {
            return true;
        }
        let name_end = line
            .find(|c: char| !matches!(c, '!'..='9' | ';'..='~'))
            .unwrap_or(line.len());
        line[name_end..].starts_with(':')
    };
    let lines: Vec<&str> = lines
        .into_iter()
        .take_while(|line| is_header(line))
        .collect();
    let mut fields = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    let flush = |current: &mut Option<(String, Vec<&str>)>, fields: &mut Vec<(String, String)>| {
        if let Some((name, source)) = current.take() {
            let first = source[0].split_once(':').map_or("", |(_, value)| value);
            let joined: String = std::iter::once(first)
                .chain(source[1..].iter().copied())
                .collect();
            let value = joined
                .trim_start_matches([' ', '\t', '\r', '\n'])
                .trim_end_matches(['\r', '\n'])
                .to_string();
            fields.push((name, value));
        }
    };
    for (number, line) in lines.iter().enumerate() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, source)) = current.as_mut() {
                source.push(line);
            }
            continue;
        }
        flush(&mut current, &mut fields);
        if line.starts_with("From ") {
            let _ = number;
            continue;
        }
        let colon = line.find(':').unwrap_or(0);
        if colon == 0 {
            continue;
        }
        current = Some((line[..colon].to_string(), vec![line]));
    }
    flush(&mut current, &mut fields);
    Headers(fields)
}

/// What the response writer knows about the request so far.
#[derive(Clone, Copy, PartialEq)]
enum Version {
    /// `request_version` is `''` or a real version: a status line is sent.
    Lines,
    /// HTTP/0.9: the body alone, no status line or headers.
    Bare,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        414 => "URI Too Long",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// The current time as an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn http_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let days = seconds.div_euclid(86400);
    let time = seconds.rem_euclid(86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        months[(month - 1) as usize],
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

struct Exchange<'a> {
    hub: &'a Hub,
    conn: Conn,
    version: Version,
    identity: Identity,
    /// The `Allow` header of a 405 reply.
    allow: Option<&'static str>,
    /// A reply to `HEAD`: headers only.
    head_only: bool,
}

impl Exchange<'_> {
    fn reply_bytes(&mut self, status: u16, data: &[u8], content_type: &str, csp: &str) {
        let mut out = Vec::new();
        if self.version == Version::Lines {
            let allow = match self.allow {
                Some(methods) if status == 405 => format!("Allow: {methods}\r\n"),
                _ => String::new(),
            };
            let head = format!(
                "HTTP/1.0 {status} {}\r\nServer: switchboard\r\nDate: {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{allow}Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: {csp}\r\n\r\n",
                reason(status),
                http_date(),
                data.len()
            );
            out.extend_from_slice(head.as_bytes());
        }
        if !self.head_only {
            out.extend_from_slice(data);
        }
        self.conn.link.set_read_timeout(TIMEOUT);
        let _ = match &self.conn.link {
            Link::Unix(stream) => stream.set_write_timeout(Some(TIMEOUT)),
            Link::Tls(stream) => stream.sock.set_write_timeout(Some(TIMEOUT)),
        };
        let _ = self.conn.link.write_all(&out);
    }

    fn reply(&mut self, status: u16, value: &Val) {
        let raw = jsonval::dumps(value, jsonval::DEFAULT);
        self.reply_bytes(status, raw.as_bytes(), "application/json", CSP);
    }

    fn error(&mut self, status: u16, code: &str) {
        let value = Val::Dict(dict([("ok", false.into()), ("error", code.into())]));
        self.reply(status, &value);
    }

    /// Every protocol-level failure is `invalid_request`.
    fn send_error(&mut self, status: u16) {
        self.error(status, "invalid_request");
    }

    /// `405 method_not_allowed`, naming the methods the path takes.
    fn method_not_allowed(&mut self, allow: &'static str) {
        self.allow = Some(allow);
        self.error(405, "method_not_allowed");
    }
}

/// Handle one connection's single request and close it.
pub fn serve(hub: &Hub, link: Link, identity: Identity) {
    let conn = Conn {
        link,
        buffer: Vec::new(),
        start: 0,
        deadline: Some(Instant::now() + TIMEOUT),
        expired: false,
    };
    let mut exchange = Exchange {
        hub,
        conn,
        version: Version::Lines,
        identity,
        allow: None,
        head_only: false,
    };
    handle_one_request(&mut exchange);
    exchange.conn.link.finish();
}

fn handle_one_request(ex: &mut Exchange<'_>) {
    let Ok(raw) = ex.conn.readline(MAX_LINE + 1) else {
        return;
    };
    if raw.len() > MAX_LINE {
        ex.version = Version::Lines;
        ex.send_error(414);
        return;
    }
    if raw.is_empty() {
        return;
    }
    let Some((command, path, headers)) = parse_request(ex, &raw) else {
        return;
    };
    if command != "GET" && command != "POST" {
        // Every endpoint takes GET or POST; a reply to HEAD has no body.
        ex.head_only = command == "HEAD";
        ex.method_not_allowed("GET, POST");
        return;
    }
    handle_request(ex, &command, &path, headers.as_ref());
}

/// `parse_request`; `None` once any error response has been sent.
fn parse_request(ex: &mut Exchange<'_>, raw: &[u8]) -> Option<(String, String, Option<Headers>)> {
    // Until a version is read the request counts as HTTP/0.9.
    ex.version = Version::Bare;
    let line = latin1(raw);
    let line = line.trim_end_matches(['\r', '\n']);
    let words: Vec<&str> = line
        .split(text::is_space)
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    if words.len() >= 3 {
        let version = words[words.len() - 1];
        let parsed = version.strip_prefix("HTTP/").and_then(|number| {
            let parts: Vec<&str> = number.split('.').collect();
            if parts.len() != 2
                || parts
                    .iter()
                    .any(|p| p.is_empty() || p.len() > 10 || !p.bytes().all(|b| b.is_ascii_digit()))
            {
                return None;
            }
            Some((parts[0].parse::<u64>().ok()?, parts[1].parse::<u64>().ok()?))
        });
        let Some(number) = parsed else {
            ex.version = Version::Lines;
            ex.send_error(400);
            return None;
        };
        ex.version = Version::Lines;
        if number >= (2, 0) {
            ex.send_error(505);
            return None;
        }
    }
    if !(2..=3).contains(&words.len()) {
        ex.version = Version::Lines;
        ex.send_error(400);
        return None;
    }
    let command = words[0].to_string();
    let mut path = words[1].to_string();
    let http09 = words.len() == 2;
    if http09 && command != "GET" {
        ex.version = Version::Lines;
        ex.send_error(400);
        return None;
    }
    if path.starts_with("//") {
        path = format!("/{}", path.trim_start_matches('/'));
    }
    if http09 {
        return Some((command, path, None));
    }
    let mut lines: Vec<Vec<u8>> = Vec::new();
    loop {
        let Ok(line) = ex.conn.readline(MAX_LINE + 1) else {
            return None;
        };
        if line.len() > MAX_LINE {
            ex.send_error(431);
            return None;
        }
        let end = line.is_empty() || line == b"\r\n" || line == b"\n";
        lines.push(line);
        if lines.len() > MAX_HEADERS {
            ex.send_error(431);
            return None;
        }
        if end {
            break;
        }
    }
    let block = latin1(&lines.concat());
    Some((command, path, Some(parse_header_block(&block))))
}

const GET_PATHS: [&str; 2] = ["/v1/peers", "/health"];
const POST_PATHS: [&str; 7] = [
    "/v1/register",
    "/v1/poll",
    "/v1/send",
    "/v1/status",
    "/v1/ping",
    "/v1/pong",
    "/v1/enroll",
];

enum Stop {
    Fail(Fail),
    /// A connection error or timeout: no response.
    Quiet,
    /// Any other I/O error while reading: `503 unavailable`.
    Unavailable,
}

impl From<Fail> for Stop {
    fn from(fail: Fail) -> Self {
        Stop::Fail(fail)
    }
}

/// A request counted in its machine's active and polling limits, uncounted
/// when dropped: after the response, or as a panicking handler unwinds.
struct Entered<'a> {
    hub: &'a Hub,
    fingerprint: String,
    polling: bool,
    waiting: bool,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.hub
            .leave(&self.fingerprint, self.polling, self.waiting);
    }
}

fn handle_request(ex: &mut Exchange<'_>, method: &str, path: &str, headers: Option<&Headers>) {
    ex.conn.deadline = None;
    #[cfg(feature = "fault-injection")]
    if path == "/__panic" {
        panic!("forced panic before authentication (fault-injection build)");
    }
    let polling = path == "/v1/poll";
    let mut entered: Option<Entered<'_>> = None;
    let outcome = run_request(ex, method, path, headers, polling, &mut entered);
    match outcome {
        Ok(()) => {}
        Err(Stop::Fail(Fail::Unicode)) => ex.error(400, "invalid_unicode"),
        Err(Stop::Fail(Fail::Api(code, status))) => ex.error(status, code),
        Err(Stop::Quiet) => {}
        Err(Stop::Fail(Fail::Unavailable)) | Err(Stop::Unavailable) => ex.error(503, "unavailable"),
        Err(Stop::Fail(Fail::Abort(message))) => {
            let _ = writeln!(
                io::stderr(),
                "Exception occurred during processing of request: {message}"
            );
        }
    }
    drop(entered);
}

fn run_request<'a>(
    ex: &mut Exchange<'a>,
    method: &str,
    path: &str,
    headers: Option<&Headers>,
    polling: bool,
    entered: &mut Option<Entered<'a>>,
) -> Result<(), Stop> {
    let fingerprint = match &ex.identity {
        Identity::Proxy => {
            let Some(headers) = headers else {
                return Err(Stop::Fail(Fail::Abort(
                    "'dict' object has no attribute 'get_all'".into(),
                )));
            };
            let values = headers.get_all("X-Switchboard-Client-Fingerprint");
            if values.len() != 1 || !is_fingerprint(values[0]) {
                return Err(Fail::Api("invalid_proxy_identity", 401).into());
            }
            values[0].to_string()
        }
        Identity::Certificate { fingerprint, .. } => fingerprint.clone(),
    };
    let hub = ex.hub;
    if method == "POST" && path == "/v1/enroll" && !hub.enrolled(&fingerprint)? {
        // The only request an unknown key may make (redeeming an invite);
        // rate limited by address.
        let address = client_address(&ex.identity, headers)?;
        let body = read_body(ex, headers)?;
        let value = hub.enroll_unknown(&fingerprint, &address, &body)?;
        ex.reply(200, &value);
        return Ok(());
    }
    hub.auth(&fingerprint)?;
    // Polls and pings hold their handler while they wait; they may not take
    // the handlers kept free for short requests.
    let waiting = method == "POST" && (path == "/v1/poll" || path == "/v1/ping");
    hub.enter(&fingerprint, polling, waiting)?;
    *entered = Some(Entered {
        hub,
        fingerprint: fingerprint.clone(),
        polling,
        waiting,
    });
    #[cfg(feature = "fault-injection")]
    if path == "/__panic_entered" {
        panic!("forced panic in a counted request (fault-injection build)");
    }
    #[cfg(feature = "fault-injection")]
    if path == "/__panic_in_transaction" {
        hub.panic_in_transaction();
    }
    if (method == "GET" && !GET_PATHS.contains(&path))
        || (method == "POST" && !POST_PATHS.contains(&path))
    {
        // A path that exists for the other method.
        if GET_PATHS.contains(&path) {
            ex.allow = Some("GET");
            return Err(Fail::Api("method_not_allowed", 405).into());
        }
        if POST_PATHS.contains(&path) {
            ex.allow = Some("POST");
            return Err(Fail::Api("method_not_allowed", 405).into());
        }
        return Err(Fail::Api("not_found", 404).into());
    }
    let body = if method == "POST" {
        Some(read_body(ex, headers)?)
    } else {
        None
    };
    let value = hub.request(&fingerprint, path, body.as_ref())?;
    ex.reply(200, &value);
    Ok(())
}

fn is_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn client_address(identity: &Identity, headers: Option<&Headers>) -> Result<String, Stop> {
    match identity {
        Identity::Certificate { address, .. } => Ok(address.clone()),
        Identity::Proxy => {
            let headers = headers.expect("proxy requests have headers");
            let values = headers.get_all("X-Switchboard-Client-Address");
            if values.len() > 1
                || values
                    .first()
                    .is_some_and(|v| v.chars().count() > 64 || !v.chars().all(text::is_printable))
            {
                return Err(Fail::Api("invalid_proxy_identity", 401).into());
            }
            Ok(values.first().map_or("unknown", |v| v).to_string())
        }
    }
}

fn read_body(ex: &mut Exchange<'_>, headers: Option<&Headers>) -> Result<Val, Stop> {
    let Some(headers) = headers else {
        return Err(Stop::Fail(Fail::Abort(
            "'dict' object has no attribute 'get_all'".into(),
        )));
    };
    let lengths = headers.get_all("Content-Length");
    let chunked = headers
        .get("Transfer-Encoding")
        .is_some_and(|v| !v.is_empty());
    // Other non-digits answer `invalid_length`; the superscript digits ², ³,
    // and ¹ pass this check, then fail the ASCII check below, which closes
    // the connection without a response.
    let is_digit = |c: char| c.is_ascii_digit() || matches!(c, '\u{b2}' | '\u{b3}' | '\u{b9}');
    if chunked || lengths.len() != 1 || lengths[0].is_empty() || !lengths[0].chars().all(is_digit) {
        return Err(Fail::Api("invalid_length", 400).into());
    }
    let digits = lengths[0];
    if !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 4300 {
        return Err(Fail::Abort(format!(
            "invalid literal for int() with base 10: {digits:?}"
        ))
        .into());
    }
    let significant = digits.trim_start_matches('0');
    let length = if significant.len() > 7 {
        usize::MAX
    } else {
        significant.parse::<usize>().unwrap_or(0)
    };
    if length > MAX_BODY {
        return Err(Fail::Api("body_too_large", 413).into());
    }
    if headers.content_type() != "application/json" {
        return Err(Fail::Api("expected_json", 400).into());
    }
    ex.conn.deadline = Some(Instant::now() + TIMEOUT);
    let raw = ex.conn.read(length);
    ex.conn.deadline = None;
    let raw = match raw {
        Ok(raw) => raw,
        Err(ReadError::Quiet) => return Err(Stop::Quiet),
        Err(ReadError::Other) => return Err(Stop::Unavailable),
    };
    if raw.len() != length {
        return Err(Fail::Api("incomplete_body", 400).into());
    }
    jsonval::loads(&raw).ok_or(Stop::Fail(Fail::Api("invalid_json", 400)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(block: &str) -> Vec<(String, String)> {
        parse_header_block(block).0
    }

    #[test]
    fn header_block_parsing() {
        assert_eq!(
            fields("A: 1\r\nB:2  \r\nC: x\r\n  y\r\n\r\n"),
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "2  ".into()),
                ("C".into(), "x\r\n  y".into())
            ]
        );
        // A bare CR ends a line; a line that is not a field ends the block.
        assert_eq!(
            fields("A: 1\rB: 2\r\nBad Name: 3\r\nC: 4\r\n\r\n"),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
        // Leading continuation and empty names are ignored; From lines skipped.
        assert_eq!(
            fields(" x\r\n: y\r\nFrom z\r\nD: 5\r\n\r\n"),
            vec![("D".into(), "5".into())]
        );
        assert_eq!(fields("E:\r\n f\r\n"), vec![("E".into(), "f".into())]);
        let headers = parse_header_block("Content-Type:  Application/JSON ; charset=x\r\n\r\n");
        assert_eq!(headers.content_type(), "application/json");
        assert_eq!(
            parse_header_block("Content-Type: json\r\n\r\n").content_type(),
            "text/plain"
        );
        assert_eq!(parse_header_block("\r\n").content_type(), "text/plain");
    }

    #[test]
    fn dates_are_rfc_1123() {
        let date = http_date();
        assert_eq!(date.len(), 29, "{date}");
        assert!(date.ends_with(" GMT"));
    }
}
