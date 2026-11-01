//! Public server certificates through ACME (RFC 8555) with the TLS-ALPN-01
//! challenge (RFC 8737) on the server's own TLS port, as Caddy obtains them
//! today. The account, certificate, and key persist in the state directory;
//! a background thread renews the certificate when a third of its lifetime
//! remains and retries failures with backoff.
//!
//! The ACME protocol is `instant-acme`'s; its HTTP requests go through the
//! small blocking HTTPS client below (rustls, HTTP/1.1, one connection per
//! request), so the only async runtime is a single-threaded one inside the
//! renewal thread.

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse, ChallengeType,
    HttpClient, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};

use super::tls;

pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// What to obtain and where to keep it.
#[derive(Clone)]
pub struct Settings {
    pub hostname: String,
    pub directory: String,
    /// PEM roots trusted for the ACME server instead of the public roots
    /// (a test CA such as Pebble's).
    pub ca_file: Option<String>,
    pub email: Option<String>,
    pub storage: String,
}

/// The certificate served to clients, and challenge certificates for
/// validation handshakes in progress.
#[derive(Debug, Default)]
pub struct Certificates {
    current: RwLock<Option<Arc<CertifiedKey>>>,
    challenges: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.current.read().ok()?.clone()
    }
}

#[derive(Debug)]
pub struct ChallengeResolver(pub Arc<Certificates>);

impl ResolvesServerCert for ChallengeResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        self.0.challenges.lock().ok()?.get(&name).cloned()
    }
}

fn certified(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<CertifiedKey>, String> {
    let signer = tls::provider()
        .key_provider
        .load_private_key(key)
        .map_err(|exc| exc.to_string())?;
    Ok(Arc::new(CertifiedKey::new(chain, signer)))
}

fn log(event: &str, detail: &str) {
    let line = format!(
        "{{\"event\": \"{event}\", \"detail\": {}}}\n",
        super::jsonval::dumps(&detail.into(), super::jsonval::DEFAULT)
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
}

impl Settings {
    fn cert_path(&self) -> String {
        format!("{}/acme-{}-cert.pem", self.storage, self.hostname)
    }

    fn key_path(&self) -> String {
        format!("{}/acme-{}-key.pem", self.storage, self.hostname)
    }

    fn account_path(&self) -> String {
        let digest = Sha256::digest(self.directory.as_bytes());
        format!(
            "{}/acme-account-{}.json",
            self.storage,
            &tls::hex(&digest)[..16]
        )
    }
}

/// Load any stored certificate and start the renewal thread.
pub fn start(settings: Settings) -> Result<Arc<Certificates>, String> {
    let certificates = Arc::new(Certificates::default());
    let mut due = SystemTime::now();
    if let Ok(chain) = tls::read_chain(&settings.cert_path()) {
        if let (Ok(key), Some(window)) = (tls::read_key(&settings.key_path()), validity(&chain[0]))
        {
            *certificates.current.write().expect("lock") = Some(certified(chain, key)?);
            due = renewal_time(window);
        }
    }
    let shared = certificates.clone();
    std::thread::Builder::new()
        .name("acme".into())
        .spawn(move || renew_forever(settings, shared, due))
        .map_err(|exc| exc.to_string())?;
    Ok(certificates)
}

/// Renew when a third of the certificate's lifetime remains.
fn renewal_time((not_before, not_after): (SystemTime, SystemTime)) -> SystemTime {
    let lifetime = not_after.duration_since(not_before).unwrap_or_default();
    not_after - lifetime / 3
}

fn renew_forever(settings: Settings, certificates: Arc<Certificates>, mut due: SystemTime) {
    let mut backoff = Duration::from_secs(300);
    loop {
        let now = SystemTime::now();
        if now < due {
            let wait = due.duration_since(now).unwrap_or_default();
            std::thread::sleep(wait.min(Duration::from_secs(43200)));
            continue;
        }
        match obtain(&settings, &certificates) {
            Ok(window) => {
                log("certificate_obtained", &settings.hostname);
                due = renewal_time(window);
                backoff = Duration::from_secs(300);
            }
            Err(detail) => {
                log("certificate_error", &detail);
                due = SystemTime::now() + backoff;
                backoff = (backoff * 2).min(Duration::from_secs(21600));
            }
        }
    }
}

/// One order: account, authorization by TLS-ALPN-01, finalization, storage.
pub fn obtain(
    settings: &Settings,
    certificates: &Certificates,
) -> Result<(SystemTime, SystemTime), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|exc| exc.to_string())?;
    let result = runtime.block_on(order(settings, certificates));
    certificates.challenges.lock().map_err(|_| "lock")?.clear();
    let (chain_pem, key_pem) = result?;
    let chain = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|exc| exc.to_string())?;
    let window = chain
        .first()
        .and_then(validity)
        .ok_or("certificate without validity")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|exc| exc.to_string())?;
    let key_for_disk = key_pem.clone();
    // Key first, then certificate: a stored certificate always has its key.
    super::write_private(&settings.key_path(), key_for_disk.as_bytes())?;
    super::write_private(&settings.cert_path(), chain_pem.as_bytes())?;
    *certificates.current.write().map_err(|_| "lock")? = Some(certified(chain, key)?);
    Ok(window)
}

async fn order(
    settings: &Settings,
    certificates: &Certificates,
) -> Result<(String, String), String> {
    let text = |exc: instant_acme::Error| exc.to_string();
    let http = Box::new(BlockingHttp::new(settings.ca_file.as_deref())?);
    let account_path = settings.account_path();
    let account = match std::fs::read(&account_path) {
        Ok(raw) => {
            let credentials: AccountCredentials =
                serde_json::from_slice(&raw).map_err(|exc| exc.to_string())?;
            Account::builder_with_http(http)
                .from_credentials(credentials)
                .await
                .map_err(text)?
        }
        Err(_) => {
            let contact = settings
                .email
                .as_ref()
                .map(|email| format!("mailto:{email}"));
            let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
            let new = NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            };
            let (account, credentials) = Account::builder_with_http(http)
                .create(&new, settings.directory.clone(), None)
                .await
                .map_err(text)?;
            let raw = serde_json::to_vec(&credentials).map_err(|exc| exc.to_string())?;
            super::write_private(&account_path, &raw)?;
            account
        }
    };
    let identifiers = [Identifier::Dns(settings.hostname.clone())];
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .map_err(text)?;
    let mut authorizations = order.authorizations();
    while let Some(authorization) = authorizations.next().await {
        let mut authorization = authorization.map_err(text)?;
        match authorization.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            other => return Err(format!("authorization {other:?}")),
        }
        let mut challenge = authorization
            .challenge(ChallengeType::TlsAlpn01)
            .ok_or("the ACME server offered no tls-alpn-01 challenge")?;
        let digest = challenge.key_authorization().digest();
        let proof = challenge_certificate(&settings.hostname, digest.as_ref())?;
        certificates
            .challenges
            .lock()
            .map_err(|_| "lock")?
            .insert(settings.hostname.to_ascii_lowercase(), proof);
        challenge.set_ready().await.map_err(text)?;
    }
    let patience = RetryPolicy::new().timeout(Duration::from_secs(180));
    let status = order.poll_ready(&patience).await.map_err(text)?;
    if status != OrderStatus::Ready {
        // Say why: the challenge's error carries the CA's reason (such as a
        // firewall timeout or a DNS failure), which `init` turns into a fix.
        let mut reasons = Vec::new();
        let mut authorizations = order.authorizations();
        while let Some(Ok(mut authorization)) = authorizations.next().await {
            if let Ok(state) = authorization.refresh().await {
                reasons.extend(
                    state
                        .challenges
                        .iter()
                        .filter_map(|challenge| challenge.error.as_ref())
                        .map(|problem| problem.to_string()),
                );
            }
        }
        return Err(if reasons.is_empty() {
            format!("order {status:?}")
        } else {
            format!("order {status:?}: {}", reasons.join("; "))
        });
    }
    let key = rcgen::KeyPair::generate().map_err(|exc| exc.to_string())?;
    // The request's subject must name only the ordered identifier: a CA
    // rejects any other common name (rcgen's default is a placeholder).
    let mut params = rcgen::CertificateParams::new(vec![settings.hostname.clone()])
        .map_err(|exc| exc.to_string())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, settings.hostname.clone());
    let request = params
        .serialize_request(&key)
        .map_err(|exc| exc.to_string())?;
    order.finalize_csr(request.der()).await.map_err(text)?;
    let chain = order.poll_certificate(&patience).await.map_err(text)?;
    Ok((chain, key.serialize_pem()))
}

/// The TLS-ALPN-01 response: a self-signed certificate for the name with the
/// critical acmeIdentifier extension holding the key authorization digest.
fn challenge_certificate(hostname: &str, digest: &[u8]) -> Result<Arc<CertifiedKey>, String> {
    let key = rcgen::KeyPair::generate().map_err(|exc| exc.to_string())?;
    let mut params =
        rcgen::CertificateParams::new(vec![hostname.to_string()]).map_err(|exc| exc.to_string())?;
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(digest)];
    let cert = params.self_signed(&key).map_err(|exc| exc.to_string())?;
    let der = PrivateKeyDer::try_from(key.serialize_der()).map_err(|exc| exc.to_string())?;
    certified(vec![cert.der().clone()], der)
}

/// A blocking HTTPS client for the ACME server: HTTP/1.1, `Connection: close`.
struct BlockingHttp {
    config: Arc<rustls::ClientConfig>,
}

impl BlockingHttp {
    fn new(ca_file: Option<&str>) -> Result<Self, String> {
        let mut roots = rustls::RootCertStore::empty();
        match ca_file {
            Some(path) => {
                for cert in tls::read_chain(path)? {
                    roots.add(cert).map_err(|exc| exc.to_string())?;
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let config = rustls::ClientConfig::builder_with_provider(tls::provider())
            .with_safe_default_protocol_versions()
            .map_err(|exc| exc.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(BlockingHttp {
            config: Arc::new(config),
        })
    }

    fn send(&self, parts: http::request::Parts, body: Bytes) -> Result<BytesResponse, String> {
        let uri = &parts.uri;
        if uri.scheme_str() != Some("https") {
            return Err(format!("ACME URLs must use https: {uri}"));
        }
        let host = uri.host().ok_or("URL without host")?.to_string();
        let port = uri.port_u16().unwrap_or(443);
        let target = uri.path_and_query().map_or("/", |p| p.as_str());
        let address = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|exc| exc.to_string())?
            .next()
            .ok_or("no address")?;
        let timeout = Duration::from_secs(30);
        let mut sock =
            TcpStream::connect_timeout(&address, timeout).map_err(|exc| exc.to_string())?;
        sock.set_read_timeout(Some(timeout))
            .map_err(|exc| exc.to_string())?;
        sock.set_write_timeout(Some(timeout))
            .map_err(|exc| exc.to_string())?;
        let name = ServerName::try_from(host.clone()).map_err(|exc| exc.to_string())?;
        let conn = rustls::ClientConnection::new(self.config.clone(), name)
            .map_err(|exc| exc.to_string())?;
        let mut stream = rustls::StreamOwned::new(conn, &mut sock);
        let mut head = format!(
            "{} {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: switchboard\r\nConnection: close\r\nContent-Length: {}\r\n",
            parts.method,
            body.len()
        );
        for (name, value) in &parts.headers {
            if name != http::header::CONTENT_LENGTH
                && name != http::header::HOST
                && name != http::header::CONNECTION
            {
                head.push_str(&format!(
                    "{}: {}\r\n",
                    name,
                    value.to_str().map_err(|exc| exc.to_string())?
                ));
            }
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .map_err(|exc| exc.to_string())?;
        stream.write_all(&body).map_err(|exc| exc.to_string())?;
        stream.flush().map_err(|exc| exc.to_string())?;
        let mut raw = Vec::new();
        match stream.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(exc) if exc.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => {}
            Err(exc) => return Err(exc.to_string()),
        }
        parse_response(&raw, parts.method == http::Method::HEAD)
    }
}

fn parse_response(raw: &[u8], head_only: bool) -> Result<BytesResponse, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("truncated response")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|exc| exc.to_string())?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or("empty response")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or("bad status line")?;
    let mut builder = http::Response::builder().status(status);
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("bad header")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
        builder = builder.header(name.trim(), value);
    }
    let mut body = &raw[split + 4..];
    let content = if head_only {
        Vec::new()
    } else if chunked {
        let mut out = Vec::new();
        loop {
            let end = body
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or("bad chunk")?;
            let size_text = std::str::from_utf8(&body[..end]).map_err(|exc| exc.to_string())?;
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|exc| exc.to_string())?;
            body = &body[end + 2..];
            if size == 0 {
                break;
            }
            out.extend_from_slice(body.get(..size).ok_or("short chunk")?);
            body = body.get(size + 2..).ok_or("short chunk")?;
        }
        out
    } else {
        match length {
            Some(length) => body.get(..length).ok_or("short body")?.to_vec(),
            None => body.to_vec(),
        }
    };
    let response = builder.body(()).map_err(|exc| exc.to_string())?;
    let (parts, ()) = response.into_parts();
    Ok(BytesResponse {
        parts,
        body: Box::new(Bytes::from(content)),
    })
}

impl HttpClient for BlockingHttp {
    fn request(
        &self,
        req: http::Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let client = BlockingHttp {
            config: self.config.clone(),
        };
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let body = request_body(body);
            client
                .send(parts, body)
                .map_err(|detail| instant_acme::Error::Other(detail.into()))
        })
    }
}

/// The request body, which instant-acme always supplies whole.
fn request_body(body: BodyWrapper<Bytes>) -> Bytes {
    use http_body::Body;
    use std::task::{Context, Poll, Waker};
    let mut body = std::pin::pin!(body);
    let mut context = Context::from_waker(Waker::noop());
    let mut data = Vec::new();
    while let Poll::Ready(Some(Ok(frame))) = body.as_mut().poll_frame(&mut context) {
        if let Ok(chunk) = frame.into_data() {
            data.extend_from_slice(&chunk);
        }
    }
    Bytes::from(data)
}

pub use super::certinfo::validity;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn validity_is_read_from_the_certificate() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["example.test".into()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2026, 10, 30);
        params.not_after = rcgen::date_time_ymd(2051, 1, 28);
        let cert = params.self_signed(&key).unwrap();
        let (before, after) = validity(cert.der()).unwrap();
        assert_eq!(
            before.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_793_318_400
        );
        assert_eq!(
            after.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            2_558_476_800
        );
        let due = renewal_time((before, after));
        assert_eq!(
            after.duration_since(due).unwrap(),
            after.duration_since(before).unwrap() / 3
        );
    }

    #[test]
    fn responses_parse_with_length_or_chunks() {
        let response = parse_response(
            b"HTTP/1.1 201 Created\r\nReplay-Nonce: abc\r\nContent-Length: 2\r\n\r\n{}extra",
            false,
        )
        .unwrap();
        assert_eq!(response.parts.status, 201);
        assert_eq!(response.parts.headers["replay-nonce"], "abc");
        let chunked = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n", false).unwrap();
        let mut body = chunked.body;
        let bytes = futures_ready(instant_acme::BytesBody::into_bytes(&mut *body));
        assert_eq!(&bytes[..], b"abcde");
    }

    fn futures_ready<T>(
        future: impl Future<Output = Result<T, Box<dyn std::error::Error + Send + Sync>>>,
    ) -> T {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(future).unwrap()
    }
}
