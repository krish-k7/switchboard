//! The server's built-in TLS modes, run as the real binary.
//!
//! - Private-network mode: the certificate is made once and kept, its
//!   fingerprint is printed, pinning clients connect, and any client key
//!   completes the handshake while the registry alone authorizes.
//! - Public certificates: a local mock ACME server (directory, account,
//!   order, TLS-ALPN-01 validation against the server's own port, and
//!   issuance by a test CA through the `openssl` command) issues the
//!   certificate the server then serves. No real certificate authority is
//!   contacted.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use switchboard::server::install;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let template = std::ffi::CString::new("/tmp/sbt-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
        raw.pop();
        TempDir(std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_private(path: &Path, data: &[u8]) {
    std::fs::write(path, data).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A client identity: certificate, key, and registry fingerprint.
struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key_der: Vec<u8>,
    fingerprint: String,
}

impl Identity {
    fn new(name: &str) -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        Identity {
            fingerprint: hex(&Sha256::digest(cert.der())),
            chain: vec![cert.der().clone()],
            key_der: key.serialize_der(),
        }
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::try_from(self.key_der.clone()).unwrap()
    }
}

/// Accept the server certificate only when its SHA-256 matches a pin.
#[derive(Debug)]
struct Pinned {
    fingerprint: Mutex<Option<String>>,
    seen: Mutex<Option<CertificateDer<'static>>>,
    /// The mock validator skips handshake signature checks: webpki refuses
    /// the critical acmeIdentifier extension that ACME servers accept.
    validator: bool,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(end_entity.clone().into_owned());
        match &*self.fingerprint.lock().unwrap() {
            Some(pin) if *pin != hex(&Sha256::digest(end_entity)) => {
                Err(rustls::Error::General("pin mismatch".into()))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        if self.validator {
            return Ok(HandshakeSignatureValid::assertion());
        }
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn pinned(pin: Option<String>) -> Arc<Pinned> {
    Arc::new(Pinned {
        fingerprint: Mutex::new(pin),
        seen: Mutex::new(None),
        validator: false,
    })
}

fn client_config(
    verifier: Option<Arc<Pinned>>,
    roots: Option<&Path>,
    identity: Option<&Identity>,
    alpn: &[&[u8]],
) -> Arc<ClientConfig> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = match (verifier, roots) {
        (Some(verifier), _) => builder
            .dangerous()
            .with_custom_certificate_verifier(verifier),
        (None, Some(path)) => {
            let mut store = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(path).unwrap() {
                store.add(cert.unwrap()).unwrap();
            }
            builder.with_root_certificates(store)
        }
        (None, None) => panic!("a verifier or roots"),
    };
    let mut config = match identity {
        Some(identity) => builder
            .with_client_auth_cert(identity.chain.clone(), identity.key())
            .unwrap(),
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// One HTTP exchange over TLS; `Err` when the handshake fails.
fn exchange(
    port: u16,
    name: &str,
    config: Arc<ClientConfig>,
    request: &str,
) -> Result<(u16, String), String> {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let server_name = ServerName::try_from(name.to_string()).unwrap();
    let mut conn = rustls::ClientConnection::new(config, server_name).unwrap();
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).map_err(|e| e.to_string())?;
    }
    let mut stream = rustls::Stream::new(&mut conn, &mut sock);
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(e) if !raw.is_empty() && e.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(e) => return Err(e.to_string()),
    }
    let text = String::from_utf8(raw).unwrap();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("no response: {text:?}"))?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Ok((status, body))
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")
}

fn post(path: &str, body: &str) -> String {
    format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
}

struct Server {
    child: Child,
    events: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(config: &Path) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_switchboard"));
        command
            .args(["server", "--config"])
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // The server keeps an inherited "ignore" for SIGINT, and these tests
        // stop it with SIGINT: give it the default disposition even when the
        // test runner's shell ignores SIGINT (for example, a background job in
        // a non-interactive shell).
        // SAFETY: signal(2) is async-signal-safe and runs in the child
        // between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                sink.lock().unwrap().push(line.unwrap_or_default());
            }
        });
        Server { child, events }
    }

    fn wait_event(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.contains(needle))
            {
                return line.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no {needle} event; saw {:?}",
                self.events.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn interrupt(mut self) -> i32 {
        // SAFETY: signalling our own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
        let status = self.child.wait().unwrap();
        status.code().unwrap_or(-1)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_listening(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn registry(dir: &TempDir, member: &Identity) {
    let text = format!(
        r#"{{"{}": {{"machine": "member", "owner": "test", "enabled": true, "admin": true}}}}"#,
        member.fingerprint
    );
    write_private(&dir.path("peers.json"), text.as_bytes());
}

#[test]
fn private_network_mode_pins_a_persistent_certificate() {
    let dir = TempDir::new();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let member = Identity::new("member");
    registry(&dir, &member);
    let port = free_port();
    let config = format!(
        r#"{{"tls": "private", "listen": "127.0.0.1", "port": {port}, "names": ["localhost", "127.0.0.1"], "db": "state/messages.db", "peers_file": "peers.json"}}"#
    );
    write_private(&dir.path("server.json"), config.as_bytes());
    let server = Server::start(&dir.path("server.json"));
    let event = server.wait_event("private_certificate", Duration::from_secs(10));
    assert!(event.contains("\"created\": true"), "{event}");
    let pin = event.split("\"sha256\": \"").nth(1).unwrap()[..64].to_string();
    let stored = std::fs::read(dir.path("state/tls/private-cert.pem")).unwrap();
    let der = CertificateDer::from_pem_slice(&stored).unwrap();
    assert_eq!(hex(&Sha256::digest(&der)), pin);
    let key_mode = std::fs::metadata(dir.path("state/tls/private-key.pem"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(key_mode & 0o777, 0o600);
    wait_listening(port);

    // The enrolled key is authorized; the pin holds.
    let config = client_config(Some(pinned(Some(pin.clone()))), None, Some(&member), &[]);
    let (status, body) = exchange(port, "localhost", config, &get("/health")).unwrap();
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"ok": true, "machine": "member", "owner": "test"}"#)
    );
    // Any other key completes the handshake but may only redeem an invite;
    // the client address comes from the connection.
    let stranger = Identity::new("stranger");
    let config = client_config(Some(pinned(Some(pin.clone()))), None, Some(&stranger), &[]);
    let (status, body) = exchange(port, "localhost", config.clone(), &get("/health")).unwrap();
    assert_eq!(
        (status, body.as_str()),
        (403, r#"{"ok": false, "error": "not_enrolled"}"#)
    );
    let (status, body) = exchange(
        port,
        "localhost",
        config,
        &post("/v1/enroll", r#"{"machine": "stranger", "owner": "sam"}"#),
    )
    .unwrap();
    assert_eq!(
        (status, body.as_str()),
        (403, r#"{"ok": false, "error": "invite_required"}"#)
    );
    // No client certificate: the handshake fails. A wrong pin is refused by the client.
    assert!(exchange(
        port,
        "localhost",
        client_config(Some(pinned(Some(pin.clone()))), None, None, &[]),
        &get("/health")
    )
    .is_err());
    assert!(exchange(
        port,
        "localhost",
        client_config(Some(pinned(Some("0".repeat(64)))), None, Some(&member), &[]),
        &get("/health")
    )
    .is_err());
    // What server install waits for: an HTTP answer over TLS from the pinned
    // certificate (and nothing from another).
    assert!(install::answers_tls(port, "localhost", Some(&pin)));
    assert!(!install::answers_tls(
        port,
        "localhost",
        Some(&"0".repeat(64))
    ));
    assert_eq!(server.interrupt(), 0);
    assert!(!install::answers_tls(port, "localhost", Some(&pin)));

    // A restart keeps the certificate, so pins stay valid.
    let server = Server::start(&dir.path("server.json"));
    let event = server.wait_event("private_certificate", Duration::from_secs(10));
    assert!(
        event.contains(&pin) && event.contains("\"created\": false"),
        "{event}"
    );
    wait_listening(port);
    let config = client_config(Some(pinned(Some(pin))), None, Some(&member), &[]);
    assert_eq!(
        exchange(port, "localhost", config, &get("/health"))
            .unwrap()
            .0,
        200
    );
    assert_eq!(server.interrupt(), 0);
}

// Mock ACME server --------------------------------------------------------------------------

fn b64url_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("not base64url"),
        }
    };
    let mut out = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for byte in text.bytes().filter(|b| *b != b'=') {
        bits = (bits << 6) | value(byte);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out
}

fn b64url(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let value = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..=chunk.len() {
            out.push(TABLE[(value >> (18 - 6 * index) & 63) as usize] as char);
        }
    }
    out
}

#[derive(Default)]
struct AcmeState {
    thumbprint: String,
    token: String,
    validated: bool,
    certificate: Option<String>,
    orders: usize,
    validations: Vec<String>,
}

struct MockAcme {
    port: u16,
    ca_pem: PathBuf,
    state: Arc<Mutex<AcmeState>>,
}

/// The extension holding the TLS-ALPN-01 digest: OID 1.3.6.1.5.5.7.1.31,
/// critical, an OCTET STRING wrapping an OCTET STRING of the digest.
fn has_acme_identifier(der: &[u8], digest: &[u8]) -> bool {
    let oid = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x1f];
    let mut inner = vec![0x04, 0x20];
    inner.extend_from_slice(digest);
    der.windows(oid.len()).any(|w| w == oid)
        && der.windows(inner.len()).any(|w| w == inner.as_slice())
}

impl MockAcme {
    fn start(dir: &TempDir, server_port: u16, hostname: &'static str) -> MockAcme {
        // Test CA, and a TLS certificate for the mock's own HTTPS.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Mock ACME CA");
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = dir.path("acme-ca.pem");
        write_private(&ca_pem, ca.pem().as_bytes());
        write_private(&dir.path("acme-ca.key"), ca_key.serialize_pem().as_bytes());
        let issuer = rcgen::Issuer::new(ca_params, ca_key);
        let tls_key = rcgen::KeyPair::generate().unwrap();
        let tls_cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&tls_key, &issuer)
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![tls_cert.der().clone()],
                PrivateKeyDer::try_from(tls_key.serialize_der()).unwrap(),
            )
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(AcmeState {
            token: b64url(&rand_bytes()),
            ..AcmeState::default()
        }));
        let shared = state.clone();
        let root = dir.0.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (config, state, root) = (config.clone(), shared.clone(), root.clone());
                std::thread::spawn(move || {
                    let _ = serve_acme(stream, config, &state, &root, port, server_port, hostname);
                });
            }
        });
        MockAcme {
            port,
            ca_pem,
            state,
        }
    }
}

fn rand_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    // SAFETY: bytes is a valid writable buffer.
    unsafe { libc::getentropy(bytes.as_mut_ptr() as *mut libc::c_void, 16) };
    bytes
}

fn serve_acme(
    stream: TcpStream,
    config: Arc<rustls::ServerConfig>,
    state: &Mutex<AcmeState>,
    root: &Path,
    port: u16,
    server_port: u16,
    hostname: &str,
) -> Option<()> {
    let conn = rustls::ServerConnection::new(config).ok()?;
    let mut tls = rustls::StreamOwned::new(conn, stream);
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head, mut body) = loop {
        let count = tls.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..count]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break (
                String::from_utf8(raw[..end].to_vec()).ok()?,
                raw[end + 4..].to_vec(),
            );
        }
    };
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while body.len() < length {
        let count = tls.read(&mut chunk).ok()?;
        if count == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    let mut words = request_line.split(' ');
    let (method, path) = (words.next()?.to_string(), words.next()?.to_string());
    let base = format!("https://127.0.0.1:{port}");
    let (payload, protected) = if method == "POST" {
        let jws: serde_json::Value = serde_json::from_slice(&body).ok()?;
        let protected: serde_json::Value =
            serde_json::from_slice(&b64url_decode(jws["protected"].as_str()?)).ok()?;
        assert_eq!(
            protected["url"].as_str()?,
            format!("{base}{path}"),
            "JWS url must name the request"
        );
        assert!(protected["nonce"].is_string(), "JWS needs a nonce");
        (b64url_decode(jws["payload"].as_str()?), protected)
    } else {
        (Vec::new(), serde_json::Value::Null)
    };
    let mut location = None;
    let mut content_type = "application/json";
    let (status, response): (u16, String) = match path.as_str() {
        "/dir" => (200, serde_json::json!({
            "newNonce": format!("{base}/nonce"), "newAccount": format!("{base}/account"),
            "newOrder": format!("{base}/order"), "revokeCert": format!("{base}/revoke"), "keyChange": format!("{base}/key"),
        }).to_string()),
        "/nonce" => (200, String::new()),
        "/account" => {
            let jwk = &protected["jwk"];
            let canonical = format!(
                r#"{{"crv":"{}","kty":"{}","x":"{}","y":"{}"}}"#,
                jwk["crv"].as_str()?, jwk["kty"].as_str()?, jwk["x"].as_str()?, jwk["y"].as_str()?
            );
            state.lock().unwrap().thumbprint = b64url(&Sha256::digest(canonical.as_bytes()));
            location = Some(format!("{base}/account/1"));
            (201, r#"{"status": "valid"}"#.into())
        }
        "/order" | "/order/1" | "/finalize/1" => {
            let mut s = state.lock().unwrap();
            if path == "/order" {
                s.orders += 1;
                s.validated = false;
                s.certificate = None;
                location = Some(format!("{base}/order/1"));
            }
            if path == "/finalize/1" {
                let request: serde_json::Value = serde_json::from_slice(&payload).ok()?;
                let csr = b64url_decode(request["csr"].as_str()?);
                std::fs::write(root.join("request.der"), &csr).ok()?;
                std::fs::write(root.join("issue.ext"), format!("subjectAltName=DNS:{hostname}\nextendedKeyUsage=serverAuth\n")).ok()?;
                // Convert first and create a serial file: LibreSSL (the
                // macOS openssl) ignores -inform DER for x509 -req and needs
                // a serial, and signs with SHA-1 unless told; OpenSSL 3 and 4
                // accept either way.
                for args in [
                    &["req", "-inform", "DER", "-in", "request.der", "-out", "request.pem"][..],
                    &["x509", "-req", "-in", "request.pem", "-CA", "acme-ca.pem", "-CAkey", "acme-ca.key", "-CAcreateserial", "-sha256", "-days", "90", "-extfile", "issue.ext", "-out", "issued.pem"],
                ] {
                    let output = Command::new("openssl")
                        .args(args)
                        .current_dir(root)
                        .stdin(Stdio::null())
                        .output()
                        .ok()?;
                    assert!(
                        output.status.success(),
                        "openssl could not sign the request: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                // Like Let's Encrypt: every name in the request, including a
                // subject common name, must be the ordered identifier.
                let subject = Command::new("openssl")
                    .args(["req", "-in", "request.pem", "-noout", "-subject"])
                    .current_dir(root)
                    .stdin(Stdio::null())
                    .output()
                    .ok()?;
                let subject = String::from_utf8_lossy(&subject.stdout).to_string();
                for part in subject.trim().split([',', '/']) {
                    let part = part.trim();
                    let part = part.strip_prefix("subject=").unwrap_or(part).trim();
                    if part.starts_with("CN") {
                        let value = part.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
                        assert_eq!(value, hostname, "request names another identifier: {subject}");
                    }
                }
                let issued = std::fs::read_to_string(root.join("issued.pem")).ok()?;
                let ca = std::fs::read_to_string(root.join("acme-ca.pem")).ok()?;
                s.certificate = Some(format!("{issued}{ca}"));
            }
            let failed = s.validations.last().is_some_and(|v| v == "invalid");
            let status = match (&s.certificate, s.validated) {
                (Some(_), _) => "valid",
                (None, true) => "ready",
                (None, false) if failed => "invalid",
                (None, false) => "pending",
            };
            let mut order = serde_json::json!({
                "status": status, "identifiers": [{"type": "dns", "value": hostname}],
                "authorizations": [format!("{base}/authz/1")], "finalize": format!("{base}/finalize/1"),
            });
            if s.certificate.is_some() {
                order["certificate"] = serde_json::json!(format!("{base}/cert/1"));
            }
            (if path == "/order" { 201 } else { 200 }, order.to_string())
        }
        "/authz/1" => {
            let s = state.lock().unwrap();
            let status = if s.validated { "valid" } else { "pending" };
            (200, serde_json::json!({
                "status": status, "identifier": {"type": "dns", "value": hostname},
                "challenges": [{"type": "tls-alpn-01", "url": format!("{base}/chall/1"), "token": s.token, "status": status}],
            }).to_string())
        }
        "/chall/1" => {
            // Validate as an ACME server does: a handshake offering only
            // acme-tls/1, to the name, checking the key authorization digest.
            let (token, thumbprint) = {
                let s = state.lock().unwrap();
                (s.token.clone(), s.thumbprint.clone())
            };
            let digest = Sha256::digest(format!("{token}.{thumbprint}").as_bytes());
            let verifier = Arc::new(Pinned { fingerprint: Mutex::new(None), seen: Mutex::new(None), validator: true });
            let config = client_config(Some(verifier.clone()), None, None, &[b"acme-tls/1"]);
            let mut sock = TcpStream::connect(("127.0.0.1", server_port)).ok()?;
            let mut conn = rustls::ClientConnection::new(config, ServerName::try_from(hostname.to_string()).ok()?).ok()?;
            let handshake = (|| {
                while conn.is_handshaking() {
                            conn.complete_io(&mut sock).ok()?;
                }
                Some(())
            })();
            let cert = verifier.seen.lock().unwrap().clone();
            let ok = handshake.is_some()
                && conn.alpn_protocol() == Some(b"acme-tls/1")
                && cert.is_some_and(|c| has_acme_identifier(&c, &digest));
            let mut s = state.lock().unwrap();
            s.validations.push(if ok { "valid".into() } else { "invalid".into() });
            s.validated = ok;
            let status = if ok { "valid" } else { "invalid" };
            (200, serde_json::json!({"type": "tls-alpn-01", "url": format!("{base}/chall/1"), "token": token, "status": status}).to_string())
        }
        "/cert/1" => {
            content_type = "application/pem-certificate-chain";
            (200, state.lock().unwrap().certificate.clone().unwrap_or_default())
        }
        _ => (404, "{}".into()),
    };
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nReplay-Nonce: {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        b64url(&rand_bytes()),
        response.len()
    );
    if let Some(location) = location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    tls.write_all(head.as_bytes()).ok()?;
    if method != "HEAD" {
        tls.write_all(response.as_bytes()).ok()?;
    }
    tls.conn.send_close_notify();
    tls.flush().ok()
}

#[test]
fn public_certificates_come_from_acme_with_tls_alpn_validation() {
    const HOST: &str = "switchboard.test";
    let dir = TempDir::new();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let member = Identity::new("member");
    registry(&dir, &member);
    let port = free_port();
    let acme = MockAcme::start(&dir, port, HOST);
    let config = format!(
        r#"{{"tls": "acme", "hostname": "{HOST}", "listen": "127.0.0.1", "port": {port}, "acme_directory": "https://127.0.0.1:{}/dir", "acme_ca": "{}", "db": "state/messages.db", "peers_file": "peers.json"}}"#,
        acme.port,
        acme.ca_pem.display()
    );
    write_private(&dir.path("server.json"), config.as_bytes());
    let server = Server::start(&dir.path("server.json"));
    server.wait_event("certificate_obtained", Duration::from_secs(30));
    // What server install waits for, with a certificate: an HTTP answer.
    assert!(install::answers_tls(port, HOST, None));
    assert_eq!(
        acme.state.lock().unwrap().validations,
        vec!["valid".to_string()]
    );
    assert_eq!(acme.state.lock().unwrap().orders, 1);

    // Clients trusting the CA reach the server by name with any client key;
    // the registry decides.
    let trusted =
        |identity: Option<&Identity>| client_config(None, Some(&acme.ca_pem), identity, &[]);
    let (status, body) = exchange(port, HOST, trusted(Some(&member)), &get("/health")).unwrap();
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"ok": true, "machine": "member", "owner": "test"}"#)
    );
    let stranger = Identity::new("stranger");
    assert_eq!(
        exchange(port, HOST, trusted(Some(&stranger)), &get("/health"))
            .unwrap()
            .0,
        403
    );
    // A client certificate is required, and only the configured name is served.
    assert!(exchange(port, HOST, trusted(None), &get("/health")).is_err());
    assert!(exchange(
        port,
        "other.test",
        client_config(Some(pinned(None)), None, Some(&member), &[]),
        &get("/health")
    )
    .is_err());
    for name in [
        "acme-account-",
        &format!("acme-{HOST}-cert.pem"),
        &format!("acme-{HOST}-key.pem"),
    ] {
        let found = std::fs::read_dir(dir.path("state/tls"))
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.file_name().to_string_lossy().starts_with(name))
            .unwrap();
        assert_eq!(
            found.metadata().unwrap().permissions().mode() & 0o777,
            0o600,
            "{name}"
        );
    }
    assert_eq!(server.interrupt(), 0);

    // A restart serves the stored certificate at once, without a new order.
    let server = Server::start(&dir.path("server.json"));
    wait_listening(port);
    assert_eq!(
        exchange(port, HOST, trusted(Some(&member)), &get("/health"))
            .unwrap()
            .0,
        200
    );
    assert_eq!(acme.state.lock().unwrap().orders, 1);
    assert_eq!(server.interrupt(), 0);
}

#[test]
fn install_readiness_checks_need_an_answer_not_just_a_listener() {
    const HOST: &str = "switchboard.test";
    let dir = TempDir::new();
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    registry(&dir, &Identity::new("member"));
    // A listener that never answers is not ready.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    assert!(!install::answers_tls(silent_port, HOST, None));
    drop(silent);
    // ACME before its first certificate (the directory is unreachable): the
    // server answers handshakes with an alert, which counts.
    let port = free_port();
    let config = format!(
        r#"{{"tls": "acme", "hostname": "{HOST}", "listen": "127.0.0.1", "port": {port}, "acme_directory": "https://127.0.0.1:9/directory", "db": "state/messages.db", "peers_file": "peers.json"}}"#
    );
    write_private(&dir.path("server.json"), config.as_bytes());
    let server = Server::start(&dir.path("server.json"));
    wait_listening(port);
    assert!(install::answers_tls(port, HOST, None));
    assert_eq!(server.interrupt(), 0);
    // The Unix socket behind Caddy answers HTTP.
    let run = dir.path("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o750)).unwrap();
    // SAFETY: getegid cannot fail.
    std::os::unix::fs::chown(&run, None, Some(unsafe { libc::getegid() })).unwrap();
    let socket = run.join("hub.sock");
    let config = format!(
        r#"{{"unix_socket": "{}", "db": "state/messages.db", "peers_file": "peers.json"}}"#,
        socket.display()
    );
    write_private(&dir.path("server.json"), config.as_bytes());
    let socket = socket.to_str().unwrap().to_string();
    assert!(!install::answers_unix(&socket));
    let server = Server::start(&dir.path("server.json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !install::answers_unix(&socket) {
        assert!(Instant::now() < deadline, "the socket never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.interrupt(), 0);
}
