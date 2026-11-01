//! HTTPS requests to the server with bounded connection reuse; failed
//! requests are never retried.
//!
//! Each calling thread owns one lane (at most eight): a cached TLS
//! connection reused for that thread's next request. Idle lanes expire after
//! 30 seconds and lanes rotate after 60 seconds of age once their request
//! finishes. Every socket operation has a 15-second timeout. Responses are
//! read with `http.client` semantics so that the same replies are accepted,
//! refused, and classified the same way.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::identity::monotonic;
use crate::urlparse::urlsplit;
use crate::wirejson;

pub const ENDPOINTS: [&str; 9] = [
    "/v1/peers",
    "/v1/register",
    "/v1/poll",
    "/v1/send",
    "/v1/status",
    "/v1/ping",
    "/v1/pong",
    "/v1/enroll",
    "/health",
];
pub const MAX_CONNECTIONS: usize = 8;
pub const MAX_AGE: f64 = 60.0;
pub const MAX_IDLE: f64 = 30.0;
pub const DEFAULT_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
const MAXLINE: usize = 65536;
const MAXHEADERS: usize = 100;

/// A failed server request, by the kind of failure callers handle
/// differently. `Display` is the error text shown to people and written into
/// error details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubError {
    /// The server answered with a non-200 status.
    Http { status: u16, code: Option<String> },
    /// A system-level failure (connection, timeout, TLS, closed client).
    Os(String),
    /// A malformed or truncated HTTP response.
    Protocol(String),
    /// An invalid request or reply.
    Value(String),
    /// Queued work cancelled before it started.
    Cancelled,
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::Http { status, code } => match code {
                Some(code) => write!(f, "server HTTP {status} ({code})"),
                None => write!(f, "server HTTP {status}"),
            },
            HubError::Os(message) | HubError::Protocol(message) | HubError::Value(message) => {
                f.write_str(message)
            }
            HubError::Cancelled => f.write_str("server request cancelled before start"),
        }
    }
}

impl std::error::Error for HubError {}

impl HubError {
    pub fn code(&self) -> Option<&str> {
        match self {
            HubError::Http { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            HubError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The error text cut to its first `limit` code points.
    pub fn truncated(&self, limit: usize) -> String {
        crate::text::take_chars(&self.to_string(), limit)
    }
}

/// The parts of a bridge config the transport needs.
#[derive(Clone, Debug, Default)]
pub struct ClientConfig {
    pub hub_url: String,
    pub ca: Option<String>,
    pub cert: String,
    pub key: String,
}

impl ClientConfig {
    /// From a config object with `hub_url`, `cert`, `key`, and optional `ca`.
    pub fn from_map(config: &Map<String, Value>) -> Result<Self, HubError> {
        let text = |key: &str| -> Result<String, HubError> {
            match config.get(key) {
                Some(Value::String(value)) => Ok(value.clone()),
                _ => Err(HubError::Value(format!("{key} must be a string"))),
            }
        };
        Ok(ClientConfig {
            hub_url: match crate::config::server_url(config) {
                Some(Value::String(value)) => value.clone(),
                _ => return Err(HubError::Value("server_url must be a string".into())),
            },
            ca: match config.get("ca") {
                None | Some(Value::Null) => None,
                Some(Value::String(value)) => Some(value.clone()),
                Some(_) => return Err(HubError::Value("ca must be a string".into())),
            },
            cert: text("cert")?,
            key: text("key")?,
        })
    }
}

thread_local! {
    static THREAD_ALIVE: AliveGuard = AliveGuard(Arc::new(AtomicBool::new(true)));
}

struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn thread_alive_flag() -> Arc<AtomicBool> {
    THREAD_ALIVE
        .try_with(|guard| guard.0.clone())
        .unwrap_or_else(|_| Arc::new(AtomicBool::new(false)))
}

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

struct Lane {
    connection: Option<BufReader<TlsStream>>,
    created: f64,
    used: f64,
    busy: bool,
    alive: Arc<AtomicBool>,
}

struct State {
    lanes: HashMap<ThreadId, Lane>,
    closed: bool,
}

struct Inner {
    host: String,
    port: u16,
    tls: Arc<rustls::ClientConfig>,
    state: Mutex<State>,
    condition: Condvar,
    response_limit: usize,
}

/// `HubClient`: thread-owned HTTPS lanes to one server.
pub struct HubClient {
    inner: Arc<Inner>,
    reaper: Mutex<Option<JoinHandle<()>>>,
}

fn os_from_io(exc: &std::io::Error) -> HubError {
    if matches!(
        exc.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        return HubError::Os("timed out".into());
    }
    match exc.raw_os_error() {
        Some(code) => HubError::Os(format!("[Errno {code}] {}", crate::error::strerror(code))),
        None => HubError::Os(exc.to_string()),
    }
}

fn load_pem_file(path: &str) -> Result<Vec<u8>, HubError> {
    std::fs::read(path).map_err(|exc| match exc.raw_os_error() {
        Some(code) => HubError::Os(format!("[Errno {code}] {}", crate::error::strerror(code))),
        None => HubError::Os(exc.to_string()),
    })
}

fn pem_certificates(
    data: &[u8],
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, HubError> {
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(data)
        .collect::<Result<_, _>>()
        .map_err(|_| HubError::Os("[SSL] PEM lib: invalid certificate PEM".into()))?;
    if certs.is_empty() {
        return Err(HubError::Os("[SSL] PEM lib: no certificate found".into()));
    }
    Ok(certs)
}

/// The system's trusted roots, which the bridge uses when its config pins no
/// server certificate (`ca: null`).
pub fn system_roots() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    roots
}

/// Build the TLS client configuration: TLS 1.2 or 1.3, the server verified
/// against the pinned CA (or the system roots), and this machine's
/// certificate and key presented.
pub fn tls_config(config: &ClientConfig) -> Result<rustls::ClientConfig, HubError> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    match &config.ca {
        Some(ca) => {
            for cert in pem_certificates(&load_pem_file(ca)?)? {
                roots
                    .add(cert)
                    .map_err(|exc| HubError::Os(format!("[X509] {exc}")))?;
            }
        }
        None => roots = system_roots(),
    }
    let chain = pem_certificates(&load_pem_file(&config.cert)?)?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(&load_pem_file(&config.key)?)
        .map_err(|_| HubError::Os("[SSL] PEM lib: no private key found".into()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|exc| HubError::Os(exc.to_string()))?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|exc| HubError::Os(format!("[SSL] {exc}")))
}

impl HubClient {
    pub fn new(config: &ClientConfig) -> Result<Self, HubError> {
        Self::with_limit(config, DEFAULT_RESPONSE_LIMIT)
    }

    /// A client whose 200 responses are limited to `response_limit` bytes.
    pub fn with_limit(config: &ClientConfig, response_limit: usize) -> Result<Self, HubError> {
        let parsed = urlsplit(&config.hub_url).map_err(|exc| HubError::Value(exc.message))?;
        let host = parsed.hostname().unwrap_or_default();
        let port = parsed
            .port()
            .map_err(|exc| HubError::Value(exc.message))?
            .filter(|port| *port != 0)
            .unwrap_or(443) as u16;
        let tls = tls_config(config)?;
        let inner = Arc::new(Inner {
            host,
            port,
            tls: Arc::new(tls),
            state: Mutex::new(State {
                lanes: HashMap::new(),
                closed: false,
            }),
            condition: Condvar::new(),
            response_limit,
        });
        let reaper_inner = inner.clone();
        let reaper = std::thread::Builder::new()
            .name("switchboard-https-reaper".into())
            .stack_size(64 * 1024)
            .spawn(move || reap(&reaper_inner))
            .map_err(|exc| os_from_io(&exc))?;
        Ok(HubClient {
            inner,
            reaper: Mutex::new(Some(reaper)),
        })
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Map<String, Value>, HubError> {
        self.request("POST", path, Some(body))
    }

    /// One request on this thread's lane.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Map<String, Value>, HubError> {
        if !ENDPOINTS.contains(&path) {
            return Err(HubError::Value("unsupported server endpoint".into()));
        }
        let payload = body.map(|value| wirejson::dumps(value).into_bytes());
        if payload.as_ref().is_some_and(|p| p.len() > 65536) {
            return Err(HubError::Value("server request too large".into()));
        }
        let (thread, connection) = self.acquire()?;
        let mut connection = connection;
        let mut reusable = false;
        let result = self.exchange(
            &mut connection,
            method,
            path,
            payload.as_deref(),
            &mut reusable,
        );
        let mut state = self.inner.state.lock().expect("connection lanes");
        let now = monotonic();
        let closed = state.closed;
        let discard = match state.lanes.get_mut(&thread) {
            Some(lane) => {
                lane.busy = false;
                lane.used = now;
                if !reusable || closed || lane.used - lane.created >= MAX_AGE {
                    true
                } else {
                    lane.connection = connection.take();
                    false
                }
            }
            None => false,
        };
        if discard {
            state.lanes.remove(&thread);
        }
        drop(connection);
        self.inner.condition.notify_all();
        drop(state);
        result
    }

    fn acquire(&self) -> Result<(ThreadId, Option<BufReader<TlsStream>>), HubError> {
        let thread = std::thread::current().id();
        let mut state = self.inner.state.lock().expect("connection lanes");
        if state.closed {
            return Err(HubError::Os("server client is closed".into()));
        }
        let now = monotonic();
        expire(&mut state, now);
        if let Some(lane) = state.lanes.get(&thread) {
            if lane.busy {
                return Err(HubError::Os(
                    "server request already active on this thread".into(),
                ));
            }
        } else {
            if state.lanes.len() >= MAX_CONNECTIONS {
                let idle = state
                    .lanes
                    .iter()
                    .filter(|(_, lane)| !lane.busy)
                    .min_by(|a, b| a.1.used.total_cmp(&b.1.used))
                    .map(|(owner, _)| *owner);
                match idle {
                    Some(owner) => {
                        state.lanes.remove(&owner);
                    }
                    None => return Err(HubError::Os("server connection limit reached".into())),
                }
            }
            state.lanes.insert(
                thread,
                Lane {
                    connection: None,
                    created: now,
                    used: now,
                    busy: false,
                    alive: thread_alive_flag(),
                },
            );
        }
        let lane = state.lanes.get_mut(&thread).expect("lane");
        lane.busy = true;
        let connection = lane.connection.take();
        self.inner.condition.notify_all();
        Ok((thread, connection))
    }

    fn connect(&self) -> Result<BufReader<TlsStream>, HubError> {
        let stream = tcp_connect(&self.inner.host, self.inner.port)?;
        stream
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(SOCKET_TIMEOUT)))
            .map_err(|exc| os_from_io(&exc))?;
        let _ = stream.set_nodelay(true);
        let name = rustls::pki_types::ServerName::try_from(self.inner.host.clone())
            .map_err(|_| HubError::Value("invalid server host name".into()))?;
        let connection = rustls::ClientConnection::new(self.inner.tls.clone(), name)
            .map_err(|exc| HubError::Os(format!("[SSL] {exc}")))?;
        let mut tls = rustls::StreamOwned::new(connection, stream);
        // Complete the handshake on connect, before the request is written.
        while tls.conn.is_handshaking() {
            tls.conn
                .complete_io(&mut tls.sock)
                .map_err(|exc| tls_error(&exc))?;
        }
        Ok(BufReader::with_capacity(16 * 1024, tls))
    }

    fn exchange(
        &self,
        slot: &mut Option<BufReader<TlsStream>>,
        method: &str,
        path: &str,
        payload: Option<&[u8]>,
        reusable: &mut bool,
    ) -> Result<Map<String, Value>, HubError> {
        if slot.is_none() {
            *slot = Some(self.connect()?);
        }
        let reader = slot.as_mut().expect("connection");
        let mut head = format!("{method} {path} HTTP/1.1\r\n");
        let host = if self.inner.host.contains(':') {
            format!("[{}]", self.inner.host)
        } else {
            self.inner.host.clone()
        };
        if self.inner.port == 443 {
            head.push_str(&format!("Host: {host}\r\n"));
        } else {
            head.push_str(&format!("Host: {host}:{}\r\n", self.inner.port));
        }
        head.push_str("Accept-Encoding: identity\r\n");
        match payload {
            Some(body) => head.push_str(&format!("Content-Length: {}\r\n", body.len())),
            None if method == "POST" => head.push_str("Content-Length: 0\r\n"),
            None => {}
        }
        head.push_str("Content-Type: application/json\r\nAccept: application/json\r\n\r\n");
        let mut message = head.into_bytes();
        if let Some(body) = payload {
            message.extend_from_slice(body);
        }
        {
            let stream = reader.get_mut();
            stream.write_all(&message).map_err(|exc| tls_error(&exc))?;
            stream.flush().map_err(|exc| tls_error(&exc))?;
        }
        let mut response = Response::begin(reader)?;
        let limit = if response.status == 200 {
            self.inner.response_limit
        } else {
            65536
        };
        let raw = response.read(reader, limit + 1)?;
        if response.status != 200 {
            let mut code = None;
            if raw.len() <= limit
                && response.closed
                && response.content_type().split(';').next() == Some("application/json")
            {
                if let Ok(Value::Object(error)) = wirejson::loads(&raw) {
                    if let Some(Value::String(candidate)) = error.get("error") {
                        if valid_error_code(candidate) {
                            code = Some(candidate.clone());
                        }
                    }
                }
            }
            return Err(HubError::Http {
                status: response.status,
                code,
            });
        }
        if raw.len() > limit {
            return Err(HubError::Value("server response too large".into()));
        }
        if !response.closed {
            return Err(HubError::Protocol(format!(
                "IncompleteRead({} bytes read)",
                raw.len()
            )));
        }
        if response.content_type().split(';').next() != Some("application/json") {
            return Err(HubError::Value(
                "server returned a non-JSON response".into(),
            ));
        }
        let value = match wirejson::loads(&raw).map_err(|exc| HubError::Value(exc.message))? {
            Value::Object(map) if map.get("ok") != Some(&Value::Bool(false)) => map,
            _ => {
                return Err(HubError::Value(
                    "server returned an invalid response".into(),
                ))
            }
        };
        *reusable = !response.will_close;
        Ok(value)
    }

    /// Stop reuse promptly; active callers close their own sockets on return.
    pub fn close(&self) {
        {
            let mut state = self.inner.state.lock().expect("connection lanes");
            state.closed = true;
            state.lanes.retain(|_, lane| lane.busy);
            self.inner.condition.notify_all();
        }
        if let Some(reaper) = self.reaper.lock().expect("reaper").take() {
            // The reaper exits promptly once it sees `closed`.
            let _ = reaper.join();
        }
    }
}

impl Drop for HubClient {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("connection lanes");
        state.closed = true;
        self.inner.condition.notify_all();
    }
}

fn valid_error_code(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

fn expire(state: &mut State, now: f64) {
    state.lanes.retain(|_, lane| {
        lane.busy
            || (lane.alive.load(Ordering::SeqCst)
                && now - lane.created < MAX_AGE
                && now - lane.used < MAX_IDLE)
    });
}

fn reap(inner: &Inner) {
    let mut state = inner.state.lock().expect("connection lanes");
    while !state.closed {
        let now = monotonic();
        expire(&mut state, now);
        let deadline = state
            .lanes
            .values()
            .filter(|lane| !lane.busy)
            .map(|lane| (lane.created + MAX_AGE).min(lane.used + MAX_IDLE))
            .min_by(f64::total_cmp);
        state = match deadline {
            None => inner.condition.wait(state).expect("connection lanes"),
            Some(deadline) => {
                let wait = Duration::from_secs_f64((deadline - now).max(0.0) + 0.001);
                inner
                    .condition
                    .wait_timeout(state, wait)
                    .expect("connection lanes")
                    .0
            }
        };
    }
    state.lanes.retain(|_, lane| lane.busy);
}

fn tls_error(exc: &std::io::Error) -> HubError {
    if exc.kind() == std::io::ErrorKind::InvalidData {
        return HubError::Os(format!("[SSL] {exc}"));
    }
    os_from_io(exc)
}

/// `socket.create_connection((host, port), timeout=15)`: try each address
/// in order and raise the last failure.
pub fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, HubError> {
    let addresses = resolve(host, port)?;
    let mut last = HubError::Os("getaddrinfo returns an empty list".into());
    for address in addresses {
        match TcpStream::connect_timeout(&address, SOCKET_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(exc) => last = os_from_io(&exc),
        }
    }
    Err(last)
}

/// `getaddrinfo(host, port, 0, SOCK_STREAM)`, with `socket.gaierror` text.
pub fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, HubError> {
    use std::ffi::{CStr, CString};
    let node = CString::new(host).map_err(|_| HubError::Value("embedded null byte".into()))?;
    let service = CString::new(port.to_string()).expect("digits");
    // SAFETY: hints is plain old data; getaddrinfo fills `result`, which is
    // released with freeaddrinfo after copying the addresses out.
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut result: *mut libc::addrinfo = std::ptr::null_mut();
        let code = libc::getaddrinfo(node.as_ptr(), service.as_ptr(), &hints, &mut result);
        if code != 0 {
            let text = CStr::from_ptr(libc::gai_strerror(code))
                .to_string_lossy()
                .into_owned();
            return Err(HubError::Os(format!("[Errno {code}] {text}")));
        }
        let mut addresses = Vec::new();
        let mut cursor = result;
        while !cursor.is_null() {
            let info = &*cursor;
            if !info.ai_addr.is_null() {
                match info.ai_family {
                    libc::AF_INET => {
                        let raw = &*(info.ai_addr as *const libc::sockaddr_in);
                        let ip = std::net::Ipv4Addr::from(u32::from_be(raw.sin_addr.s_addr));
                        addresses.push(SocketAddr::from((ip, u16::from_be(raw.sin_port))));
                    }
                    libc::AF_INET6 => {
                        let raw = &*(info.ai_addr as *const libc::sockaddr_in6);
                        let ip = std::net::Ipv6Addr::from(raw.sin6_addr.s6_addr);
                        addresses.push(SocketAddr::V6(std::net::SocketAddrV6::new(
                            ip,
                            u16::from_be(raw.sin6_port),
                            raw.sin6_flowinfo,
                            raw.sin6_scope_id,
                        )));
                    }
                    _ => {}
                }
            }
            cursor = info.ai_next;
        }
        libc::freeaddrinfo(result);
        Ok(addresses)
    }
}

/// One HTTP response, read as `http.client.HTTPResponse` reads it.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    chunked: bool,
    chunk_left: Option<usize>,
    length: Option<usize>,
    will_close: bool,
    closed: bool,
}

fn read_line<R: BufRead>(reader: &mut R, what: &str) -> Result<Vec<u8>, HubError> {
    let mut line = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(exc) if exc.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(exc) if exc.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(exc) => return Err(tls_error(&exc)),
        };
        if available.is_empty() {
            break;
        }
        let room = MAXLINE + 1 - line.len();
        let window = &available[..available.len().min(room)];
        if let Some(index) = window.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&window[..=index]);
            reader.consume(index + 1);
            break;
        }
        let taken = window.len();
        line.extend_from_slice(window);
        reader.consume(taken);
        if line.len() > MAXLINE {
            break;
        }
    }
    if line.len() > MAXLINE {
        return Err(HubError::Protocol(format!(
            "got more than {MAXLINE} bytes when reading {what}"
        )));
    }
    Ok(line)
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| *b as char).collect()
}

impl Response {
    fn begin<R: BufRead>(reader: &mut R) -> Result<Self, HubError> {
        let (status, version) = loop {
            let line = latin1(&read_line(reader, "status line")?);
            if line.is_empty() {
                return Err(HubError::Os(
                    "Remote end closed connection without response".into(),
                ));
            }
            let mut words = line.split_whitespace();
            let version = words.next().unwrap_or("").to_string();
            let status_word = words.next().unwrap_or("");
            if !version.starts_with("HTTP/") {
                return Err(HubError::Protocol(line));
            }
            let status = match status_word.parse::<u16>() {
                Ok(status) if status_word.len() <= 3 && (100..=999).contains(&status) => status,
                _ => return Err(HubError::Protocol(line)),
            };
            if status != 100 {
                break (status, version);
            }
            // Skip the interim response's headers.
            loop {
                let header = read_line(reader, "header line")?;
                if header.is_empty() || header == b"\r\n" || header == b"\n" {
                    break;
                }
            }
        };
        let version = if version == "HTTP/1.0" || version == "HTTP/0.9" {
            10
        } else if version.starts_with("HTTP/1.") {
            11
        } else {
            return Err(HubError::Protocol(version));
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let line = read_line(reader, "header line")?;
            if line.is_empty() || line == b"\r\n" || line == b"\n" {
                break;
            }
            if headers.len() >= MAXHEADERS {
                return Err(HubError::Protocol(format!(
                    "got more than {MAXHEADERS} headers"
                )));
            }
            let text = latin1(&line);
            let text = text.trim_end_matches(['\r', '\n']);
            if text.starts_with([' ', '\t']) {
                if let Some(last) = headers.last_mut() {
                    last.1.push_str(&format!(" {}", text.trim()));
                }
                continue;
            }
            if let Some((name, value)) = text.split_once(':') {
                headers.push((
                    name.to_string(),
                    value.trim_matches([' ', '\t']).to_string(),
                ));
            }
        }
        let mut response = Response {
            status,
            headers,
            chunked: false,
            chunk_left: None,
            length: None,
            will_close: false,
            closed: false,
        };
        response.chunked = response
            .header("transfer-encoding")
            .is_some_and(|value| value.to_lowercase() == "chunked");
        response.will_close = response.check_close(version);
        if !response.chunked {
            if let Some(length) = response.header("content-length") {
                response.length = length
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|l| *l >= 0)
                    .map(|l| l as usize);
            }
        }
        if status == 204 || status == 304 || (100..200).contains(&status) {
            response.length = Some(0);
        }
        if !response.will_close && !response.chunked && response.length.is_none() {
            response.will_close = true;
        }
        if response.length == Some(0) && !response.chunked {
            // http.client closes a zero-length body on the first read.
        }
        Ok(response)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// `getheader('Content-Type', '')`: every value joined with ", ".
    fn content_type(&self) -> String {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn check_close(&self, version: u8) -> bool {
        let connection = self.header("connection");
        if version == 11 {
            return connection.is_some_and(|value| value.to_lowercase().contains("close"));
        }
        if self
            .header("keep-alive")
            .is_some_and(|value| !value.is_empty())
        {
            return false;
        }
        if connection.is_some_and(|value| value.to_lowercase().contains("keep-alive")) {
            return false;
        }
        if self
            .header("proxy-connection")
            .is_some_and(|value| value.to_lowercase().contains("keep-alive"))
        {
            return false;
        }
        true
    }

    /// `read(amt)`.
    fn read<R: BufRead>(&mut self, reader: &mut R, amt: usize) -> Result<Vec<u8>, HubError> {
        if self.closed {
            return Ok(Vec::new());
        }
        if self.chunked {
            return self.read_chunked(reader, amt);
        }
        let amt = match self.length {
            Some(length) if amt > length => length,
            _ => amt,
        };
        let data = read_upto(reader, amt)?;
        if data.is_empty() && amt > 0 {
            self.closed = true;
        } else if let Some(length) = self.length {
            let left = length - data.len();
            self.length = Some(left);
            if left == 0 {
                self.closed = true;
            }
        }
        if amt == 0 && self.length == Some(0) {
            self.closed = true;
        }
        Ok(data)
    }

    fn chunk_left<R: BufRead>(&mut self, reader: &mut R) -> Result<Option<usize>, HubError> {
        let mut left = self.chunk_left;
        if left.is_none_or(|l| l == 0) {
            if left == Some(0) {
                // Consume the CRLF that ends the previous chunk.
                safe_read(reader, 2)?;
            }
            let line = read_line(reader, "chunk size")?;
            let text = latin1(&line);
            let size = text.split(';').next().unwrap_or("").trim();
            let parsed = usize::from_str_radix(size, 16)
                .map_err(|_| HubError::Protocol("IncompleteRead(0 bytes read)".into()))?;
            if parsed == 0 {
                // Trailers, then the end of the body.
                loop {
                    let trailer = read_line(reader, "trailer line")?;
                    if trailer.is_empty() || trailer == b"\r\n" || trailer == b"\n" {
                        break;
                    }
                }
                self.closed = true;
                return Ok(None);
            }
            left = Some(parsed);
        }
        Ok(left)
    }

    fn read_chunked<R: BufRead>(
        &mut self,
        reader: &mut R,
        amt: usize,
    ) -> Result<Vec<u8>, HubError> {
        let mut value = Vec::new();
        let mut amt = amt;
        loop {
            let left = match self.chunk_left(reader) {
                Ok(Some(left)) => left,
                Ok(None) => break,
                Err(HubError::Protocol(_)) => {
                    return Err(HubError::Protocol(format!(
                        "IncompleteRead({} bytes read)",
                        value.len()
                    )))
                }
                Err(other) => return Err(other),
            };
            if amt <= left {
                value.extend(safe_read(reader, amt).map_err(|_| {
                    HubError::Protocol(format!("IncompleteRead({} bytes read)", value.len()))
                })?);
                self.chunk_left = Some(left - amt);
                break;
            }
            value.extend(safe_read(reader, left).map_err(|_| {
                HubError::Protocol(format!("IncompleteRead({} bytes read)", value.len()))
            })?);
            amt -= left;
            self.chunk_left = Some(0);
        }
        Ok(value)
    }
}

fn read_upto<R: Read>(reader: &mut R, amt: usize) -> Result<Vec<u8>, HubError> {
    let mut data = vec![0u8; amt.min(1 << 20)];
    let mut filled = 0;
    while filled < amt {
        if filled == data.len() {
            data.resize((data.len() * 2).min(amt), 0);
        }
        match reader.read(&mut data[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(exc) if exc.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(exc) if exc.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(exc) => return Err(tls_error(&exc)),
        }
    }
    data.truncate(filled);
    Ok(data)
}

fn safe_read<R: Read>(reader: &mut R, amt: usize) -> Result<Vec<u8>, HubError> {
    let data = read_upto(reader, amt)?;
    if data.len() < amt {
        return Err(HubError::Protocol(format!(
            "IncompleteRead({} bytes read, {} more expected)",
            data.len(),
            amt - data.len()
        )));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &[u8], amt: usize) -> (Response, Result<Vec<u8>, HubError>) {
        let mut reader = BufReader::new(raw);
        let mut response = Response::begin(&mut reader).unwrap();
        let body = response.read(&mut reader, amt);
        (response, body)
    }

    #[test]
    fn content_length_bodies_close_when_complete() {
        let (response, body) = parse(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            100,
        );
        assert_eq!(body.unwrap(), b"{}");
        assert!(response.closed);
        assert!(!response.will_close);
        assert_eq!(response.content_type(), "application/json");
    }

    #[test]
    fn unknown_length_bodies_stay_open_like_http_client() {
        let (response, body) = parse(b"HTTP/1.1 200 OK\r\n\r\nabc", 100);
        assert_eq!(body.unwrap(), b"abc");
        assert!(!response.closed);
        assert!(response.will_close);
    }

    #[test]
    fn chunked_bodies_and_connection_close() {
        let (response, body) = parse(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nab\r\n1\r\nc\r\n0\r\n\r\n",
            100,
        );
        assert_eq!(body.unwrap(), b"abc");
        assert!(response.closed);
        assert!(response.will_close);
    }

    #[test]
    fn malformed_status_lines() {
        let mut reader = BufReader::new(&b""[..]);
        assert_eq!(
            Response::begin(&mut reader).err().unwrap().to_string(),
            "Remote end closed connection without response"
        );
        let mut reader = BufReader::new(&b"FTP 200\r\n\r\n"[..]);
        assert!(matches!(
            Response::begin(&mut reader),
            Err(HubError::Protocol(_))
        ));
        assert_eq!(
            HubError::Http {
                status: 404,
                code: Some("not_found".into())
            }
            .to_string(),
            "server HTTP 404 (not_found)"
        );
    }

    #[test]
    fn error_codes_are_short_lowercase_identifiers() {
        assert!(valid_error_code("queue_full"));
        assert!(!valid_error_code("Queue"));
        assert!(!valid_error_code(""));
        assert!(!valid_error_code(&"a".repeat(65)));
    }
}
