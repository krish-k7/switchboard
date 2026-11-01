//! Joining a machine by key: the key, the server's certificate, and the
//! invite that `init` uses, and `setup`, the older non-interactive join
//! (kept, out of the help, so scripts keep working).
//!
//! The server authorizes every request by the client certificate's key
//! fingerprint. A new machine reads an invite (`crate::invite`), pins the
//! server's certificate when the invite carries its fingerprint, makes its
//! own key, and redeems the invite, which enrolls the key at once. Listing
//! and revoking machines and making invites happen on the server
//! (`switchboard server invite`, `machines`, `revoke`).

use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::time::Duration;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::args::Args;
use super::local::private_bytes;
use super::{err, out, print_error};
use crate::error::{Error, Kind, Result};
use crate::fsutil;
use crate::hub_transport::{ClientConfig, HubClient, HubError};
use crate::invite::Invite;
use crate::text::{repr_str, take_chars};
use crate::wirejson::{self, str_value, truthy};

pub const IDENTITY_DAYS: u64 = 3650;

/// A `MachineError`: shown as its message.
fn machine_error(message: impl Into<String>) -> Error {
    Error {
        kind: Kind::Value,
        message: message.into(),
    }
}

fn error_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "invalid_invite" => "the server didn't accept the invite (it may have expired, been used, or been revoked); ask for a new one (sudo switchboard server invite on the server)",
        "invite_used" => "this invite was already used; ask for a new one (sudo switchboard server invite on the server)",
        "invite_expired" => "this invite has expired; ask for a new one (sudo switchboard server invite on the server)",
        "invite_required" => "this machine isn't on the server; ask for an invite (sudo switchboard server invite on the server), then run switchboard init here",
        "invalid_enrollment" => "the server doesn't accept invites; update the Switchboard server",
        "machine_taken" => "a machine with that name is already on the server; choose another with --machine, or ask for an invite without a fixed name",
        "rate_limited" => "the server is limiting requests from this address; try again shortly",
        "not_enrolled" => "the server doesn't know this machine's key; run switchboard init",
        "not_found" => "the server is too old for this command",
        "enrollment_unavailable" => "the server's machine registry is unavailable",
        "invalid_machine" => "machine names use lowercase letters, digits, and hyphens, starting with a letter",
        "invalid_label" => "the owner must be a single line of at most 64 characters",
        _ => return None,
    })
}

/// The client settings from the bridge config.
/// A config that does not exist means the machine is not set up; any other
/// failure is reported with its reason.
pub fn hub_config(config_path: &str) -> Result<ClientConfig> {
    super::local::client_config(config_path)
        .map(|(_, config)| config)
        .map_err(|error| {
            let path = fsutil::absolute(config_path).unwrap_or_else(|_| config_path.to_string());
            let missing = std::fs::symlink_metadata(&path)
                .is_err_and(|exc| exc.kind() == std::io::ErrorKind::NotFound);
            if missing {
                machine_error(super::NOT_SET_UP)
            } else {
                let reason = match error.message.as_str() {
                    "unsafe ancestor" => "a directory above it, or above a file it names, is a symbolic link or writable by other users".to_string(),
                    "unsafe path" | "unsafe JSON file" => "it, or a file it names, is a symbolic link, not a regular file you own, or readable by others".to_string(),
                    // A missing field: the error is the quoted field name.
                    quoted if quoted.len() > 2 && quoted.starts_with('\'') && quoted.ends_with('\'') => {
                        format!("it has no {} field", &quoted[1..quoted.len() - 1])
                    }
                    other => other.to_string(),
                };
                machine_error(format!(
                    "the bridge config {} cannot be used: {reason}",
                    super::install::tilde(&path),
                ))
            }
        })
}

/// One authenticated request to the server, as this machine.
pub fn call(
    config: &ClientConfig,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Map<String, Value>> {
    let client = HubClient::new(config).map_err(|error| {
        machine_error(format!(
            "the identity files cannot be loaded: {}",
            error.truncated(200)
        ))
    })?;
    let reply = client.request(method, path, body);
    client.close();
    match reply {
        Ok(value) => Ok(value),
        Err(HubError::Http { status, code }) => {
            Err(machine_error(match code.as_deref().and_then(error_text) {
                Some(text) => text.to_string(),
                None => format!(
                    "the server refused the request ({})",
                    code.clone().unwrap_or_else(|| status.to_string())
                ),
            }))
        }
        Err(error) => Err(machine_error(format!(
            "could not reach the server: {}",
            error.truncated(200)
        ))),
    }
}

/// This computer's host name (gethostname(3)).
fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most buffer.len() bytes.
    unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// This computer's host name as a machine name.
pub fn default_machine() -> String {
    let host = hostname();
    let first = host.split('.').next().unwrap_or("").to_lowercase();
    let mut name = String::new();
    let mut in_run = false;
    for c in first.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            name.push(c);
            in_run = false;
        } else if !in_run {
            name.push('-');
            in_run = true;
        }
    }
    let name = name.trim_matches('-').to_string();
    let name = if name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name
    } else {
        format!("machine-{name}")
    };
    let cut = take_chars(&name, 48);
    let cut = cut.trim_end_matches('-');
    if cut.is_empty() {
        "machine".into()
    } else {
        cut.to_string()
    }
}

/// The login name: the first non-empty `LOGNAME`, `USER`, `LNAME`, or
/// `USERNAME`, else the password database.
pub fn getuser() -> String {
    for key in ["LOGNAME", "USER", "LNAME", "USERNAME"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    // SAFETY: getpwuid returns a static record or NULL.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_name.is_null() {
            return std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned();
        }
    }
    String::new()
}

/// Create `path` and its parents if needed; it must be an owned directory
/// with mode 0700.
fn private_dir(path: &str) -> Result<()> {
    fsutil::mkdir_parents(path, 0o700)?;
    let info = fsutil::lstat(path)?;
    if !fsutil::is_type(&info, fsutil::S_IFDIR)
        || info.uid() != fsutil::getuid()
        || info.mode() & 0o077 != 0
    {
        return Err(Error::value(
            "enrollment directory must be owned, regular, and mode 0700",
        ));
    }
    Ok(())
}

/// A new 0600 file, fsynced.
fn write_new_bytes(path: &str, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new(path))))?;
    file.write_all(data)
        .map_err(|exc| Error::from_io(&exc, None))?;
    file.sync_all().map_err(|exc| Error::from_io(&exc, None))?;
    Ok(())
}

fn mkdtemp(dir: &str, prefix: &str) -> Result<String> {
    for _ in 0..100 {
        let bytes = crate::identity::random_bytes::<8>();
        let chars: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
        let middle: String = bytes
            .iter()
            .map(|b| chars[*b as usize % chars.len()] as char)
            .collect();
        let path = fsutil::join(dir, &format!("{prefix}{middle}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(exc) if exc.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(exc) => return Err(Error::from_io(&exc, Some(std::path::Path::new(&path)))),
        }
    }
    Err(Error::os(
        "[Errno 17] No usable temporary directory name found",
    ))
}

/// Make a P-256 key and a ten-year self-signed client certificate:
/// basicConstraints critical CA:FALSE, keyUsage critical digitalSignature,
/// extendedKeyUsage clientAuth, and SHA-1 subject/authority key identifiers,
/// as `openssl req -x509` makes them from `create_identity`'s config.
pub fn make_certificate(machine: &str) -> Result<(String, String)> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|exc| Error::value(format!("key generation failed: {exc}")))?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|exc| Error::value(format!("certificate parameters: {exc}")))?;
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, machine);
    params.distinguished_name = name;
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    params.key_identifier_method =
        rcgen::KeyIdMethod::PreSpecified(super::sha1::sha1(key.public_key_raw()).to_vec());
    params.use_authority_key_identifier_extension = true;
    let mut serial = crate::identity::random_bytes::<20>();
    serial[0] &= 0x7f;
    if serial[0] == 0 {
        serial[0] = 0x01;
    }
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let epoch = rcgen::date_time_ymd(1970, 1, 1);
    params.not_before = epoch + Duration::from_secs(now);
    params.not_after = epoch + Duration::from_secs(now + IDENTITY_DAYS * 86400);
    let cert = params
        .self_signed(&key)
        .map_err(|exc| Error::value(format!("certificate signing failed: {exc}")))?;
    Ok((key.serialize_pem(), cert.pem()))
}

/// A new identity directory, created
/// under a temporary name and renamed into place.
pub fn create_identity(root: &str, metadata: &Map<String, Value>) -> Result<String> {
    let identity = fsutil::join(root, "identity");
    let draft = mkdtemp(root, ".identity-")?;
    let result = (|| {
        let machine = metadata.get("machine").map(str_value).unwrap_or_default();
        let (key, cert) = make_certificate(&machine)?;
        write_new_bytes(&fsutil::join(&draft, "key.pem"), key.as_bytes())?;
        write_new_bytes(&fsutil::join(&draft, "cert.pem"), cert.as_bytes())?;
        write_new_bytes(
            &fsutil::join(&draft, "identity.json"),
            wirejson::dumps(&Value::Object(metadata.clone())).as_bytes(),
        )?;
        fsutil::replace(&draft, &identity)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&draft);
    }
    result.map(|_| identity)
}

/// Reuse this machine's
/// identity, or create one and its private bridge config. `invited` says
/// `hub_url` came from an invite, so an existing config for another server
/// is refused rather than kept.
pub fn prepare(
    config_path: &str,
    machine: Option<&str>,
    owner: Option<&str>,
    hub_url: &str,
    server_pin: Option<&str>,
    invited: bool,
) -> Result<(Map<String, Value>, String, String)> {
    let server_pin = server_pin.map(normal_pin).transpose()?;
    let config_path = fsutil::absolute(config_path)?;
    let root = fsutil::parent(&config_path);
    private_dir(&root)?;
    if fsutil::lstat(&config_path).is_ok() {
        let config = match wirejson::loads(&private_bytes(&config_path)?)? {
            Value::Object(map) => map,
            other => {
                return Err(Error::type_error(format!(
                    "{} object is not subscriptable",
                    repr_str(crate::discovery::type_name(Some(&other)))
                )))
            }
        };
        let cert = crate::discovery::path_value(Some(
            config.get("cert").ok_or_else(|| Error::value("'cert'"))?,
        ))?;
        let identity_file = fsutil::join(&fsutil::parent(&cert), "identity.json");
        let known = private_bytes(&identity_file)
            .ok()
            .and_then(|raw| wirejson::loads(&raw).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let pick = |given: Option<&str>, key: &str| -> Option<String> {
            match given.filter(|g| !g.is_empty()) {
                Some(given) => Some(given.to_string()),
                None => known.get(key).filter(|v| truthy(Some(v))).map(str_value),
            }
        };
        let (machine, owner) =
            match (pick(machine, "machine"), pick(owner, "owner")) {
                (Some(machine), Some(owner)) => (machine, owner),
                _ => return Err(machine_error(
                    "this machine already has a key; pass --machine and --owner to finish joining",
                )),
            };
        let config_url = match crate::config::server_url(&config) {
            Some(Value::String(url)) => url.clone(),
            _ => hub_url.to_string(),
        };
        if invited && config_url.trim_end_matches('/') != hub_url.trim_end_matches('/') {
            return Err(machine_error(format!(
                "this machine is already set up for the server at {config_url}, not {hub_url}; move ~/.config/switchboard aside to join another server"
            )));
        }
        let pinned = !matches!(config.get("ca"), None | Some(Value::Null));
        let config = match &server_pin {
            Some(pin) => repin(&config_path, &root, config, &config_url, pin)?,
            None => {
                if !pinned {
                    require_system_trust(&config_url)?;
                }
                config
            }
        };
        return Ok((config, machine, owner));
    }
    let machine = machine
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_machine);
    let owner = owner
        .filter(|o| !o.is_empty())
        .map(str::to_string)
        .unwrap_or_else(getuser);
    let valid_machine = {
        let bytes = machine.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 48
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    };
    if !valid_machine {
        return Err(machine_error(error_text("invalid_machine").expect("known")));
    }
    if crate::text::strip(&owner).is_empty()
        || crate::text::char_len(&owner) > 64
        || owner.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
    {
        return Err(machine_error(error_text("invalid_label").expect("known")));
    }
    let identity = fsutil::join(&root, "identity");
    if fsutil::lstat(&identity).is_ok() {
        return Err(machine_error(
            "an identity exists without a bridge config; move ~/.config/switchboard/identity aside first",
        ));
    }
    // A pinned server certificate is fetched and checked before any key is
    // made, so a wrong pin leaves nothing behind.
    let server_certificate = match &server_pin {
        Some(pin) => {
            let der = server_certificate(hub_url)?;
            let found = hex(&Sha256::digest(&der));
            if &found != pin {
                return Err(machine_error(format!(
                    "the server's certificate does not match the pinned SHA-256 (the server presented {found})"
                )));
            }
            Some(der)
        }
        None => {
            require_system_trust(hub_url)?;
            None
        }
    };
    let mut metadata = Map::new();
    metadata.insert("machine".into(), Value::from(machine.clone()));
    metadata.insert("owner".into(), Value::from(owner.clone()));
    metadata.insert("server_url".into(), Value::from(hub_url));
    metadata.insert("joined_by".into(), Value::from("invite"));
    create_identity(&root, &metadata)?;
    let ca = match server_certificate {
        Some(der) => {
            let path = fsutil::join(&root, "server-cert.pem");
            write_replace(&path, certificate_pem(&der).as_bytes())?;
            Value::from(path)
        }
        None => Value::Null,
    };
    let mut config = Map::new();
    config.insert("enabled".into(), Value::Bool(true));
    config.insert("server_url".into(), Value::from(hub_url));
    config.insert("ca".into(), ca);
    config.insert(
        "cert".into(),
        Value::from(fsutil::join(&identity, "cert.pem")),
    );
    config.insert(
        "key".into(),
        Value::from(fsutil::join(&identity, "key.pem")),
    );
    config.insert(
        "config_dir".into(),
        Value::from(format!("{}/.claude", super::local::home_dir())),
    );
    config.insert(
        "state_dir".into(),
        Value::from(fsutil::join(&root, "state")),
    );
    config.insert("sessions".into(), Value::Array(Vec::new()));
    private_dir(&fsutil::join(&root, "state"))?;
    let text = format!(
        "{}\n",
        crate::fmtutil::dumps_indent(&Value::Object(config.clone()), 2, false)
    );
    write_new_bytes(&config_path, text.as_bytes())?;
    Ok((config, machine, owner))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `--server-pin` on a machine that already has a config. When the config
/// already trusts that certificate nothing changes. Otherwise the server's
/// certificate is fetched, and it is trusted only if it is exactly the one
/// pinned and the config trusted no certificate yet (an earlier setup run
/// without `--server-pin`) or the server no longer presents the one the
/// config trusts (its certificate was reissued). A pin that differs from a
/// certificate the server still presents is refused rather than retrusted.
fn repin(
    config_path: &str,
    root: &str,
    mut config: Map<String, Value>,
    hub_url: &str,
    pin: &str,
) -> Result<Map<String, Value>> {
    let trusted = match config.get("ca") {
        Some(Value::String(ca)) => Some(fingerprint_file(ca).ok().map(|f| hex(&f))),
        _ => None,
    };
    if trusted.as_ref().is_some_and(|t| t.as_deref() == Some(pin)) {
        return Ok(config);
    }
    let der = server_certificate(hub_url)?;
    let presented = hex(&Sha256::digest(&der));
    if trusted.is_some() && presented != pin {
        return Err(machine_error(
            "this machine's Switchboard config trusts a different server certificate than the pinned SHA-256",
        ));
    }
    if presented != pin {
        return Err(machine_error(format!(
            "the server's certificate does not match the pinned SHA-256 (the server presented {presented})"
        )));
    }
    let path = fsutil::join(root, "server-cert.pem");
    write_replace(&path, certificate_pem(&der).as_bytes())?;
    config.insert("ca".into(), Value::from(path));
    let text = format!(
        "{}\n",
        crate::fmtutil::dumps_indent(&Value::Object(config.clone()), 2, false)
    );
    write_replace(config_path, text.as_bytes())?;
    Ok(config)
}

/// Without `--server-pin` the bridge trusts the system's roots. A server with
/// a private certificate fails that check, so stop before anything is
/// written and say how to pin it. A server that cannot be reached passes
/// (setup reports it when it asks to enroll, as before).
fn require_system_trust(hub_url: &str) -> Result<()> {
    let Some((der, reason)) = untrusted_certificate(hub_url) else {
        return Ok(());
    };
    Err(machine_error(format!(
        "this machine does not trust the certificate of the server at {hub_url} ({reason}); \
         for a server with a private certificate, rerun with --server-pin and the SHA-256 that \
         switchboard server install printed. The server presented SHA-256 {}: use it only if it \
         matches that output",
        hex(&Sha256::digest(&der))
    )))
}

/// Checks the server's certificate as the system's roots would and keeps it
/// when it is refused.
#[derive(Debug)]
struct SystemTrust {
    verifier: std::sync::Arc<rustls::client::WebPkiServerVerifier>,
    refused: std::sync::Mutex<Option<(Vec<u8>, String)>>,
}

impl rustls::client::danger::ServerCertVerifier for SystemTrust {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let result = self.verifier.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        );
        if let Err(error) = &result {
            if let Ok(mut slot) = self.refused.lock() {
                *slot = Some((end_entity.as_ref().to_vec(), error.to_string()));
            }
        }
        result
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

/// The certificate (DER) the server at `hub_url` presents and why the
/// system's roots refuse it; `None` when they accept it or the server cannot
/// be reached. Nothing is sent after the server's certificate.
pub(crate) fn untrusted_certificate(hub_url: &str) -> Option<(Vec<u8>, String)> {
    use std::sync::Arc;
    let parsed = crate::urlparse::urlsplit(hub_url).ok()?;
    let host = parsed.hostname().unwrap_or_default();
    let port = parsed.port().ok()?.filter(|port| *port != 0).unwrap_or(443) as u16;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = Arc::new(crate::hub_transport::system_roots());
    // No system roots at all: the bridge could trust no server either, and
    // the enrollment request reports that.
    let verifier =
        rustls::client::WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
            .build()
            .ok()?;
    let check = Arc::new(SystemTrust {
        verifier,
        refused: std::sync::Mutex::new(None),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(check.clone())
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.clone()).ok()?;
    let mut stream = crate::hub_transport::tcp_connect(&host, port).ok()?;
    let timeout = Some(Duration::from_secs(10));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let mut connection = rustls::ClientConnection::new(Arc::new(config), name).ok()?;
    while connection.is_handshaking() {
        if connection.complete_io(&mut stream).is_err() {
            break;
        }
    }
    let refused = check.refused.lock().ok()?.take();
    drop(connection);
    refused
}

/// `--server-pin`: 64 hex digits (any case, colons allowed between bytes).
fn normal_pin(value: &str) -> Result<String> {
    let pin = crate::text::strip(value)
        .replace(':', "")
        .to_ascii_lowercase();
    if pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(machine_error(
            "--server-pin takes the server certificate's SHA-256 fingerprint (64 hex digits)",
        ));
    }
    Ok(pin)
}

/// Standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let value = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(TABLE[(value >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One certificate as PEM.
fn certificate_pem(der: &[u8]) -> String {
    let encoded = base64(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

/// Replace a private (0600) file atomically.
fn write_replace(path: &str, data: &[u8]) -> Result<()> {
    let temporary = format!("{path}.{}", crate::identity::uuid4_hex());
    write_new_bytes(&temporary, data)?;
    fsutil::replace(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Records the certificate the server presents and stops the handshake:
/// nothing is trusted or sent before the pin is checked.
#[derive(Debug)]
struct Capture(
    std::sync::Mutex<Option<Vec<u8>>>,
    Vec<rustls::SignatureScheme>,
);

impl rustls::client::danger::ServerCertVerifier for Capture {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(end_entity.as_ref().to_vec());
        }
        Err(rustls::Error::General("certificate captured".into()))
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("certificate captured".into()))
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("certificate captured".into()))
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.1.clone()
    }
}

/// The certificate (DER) the server at `hub_url` presents, untrusted.
pub(crate) fn server_certificate(hub_url: &str) -> Result<Vec<u8>> {
    use std::sync::Arc;
    let unreachable =
        |detail: String| machine_error(format!("could not reach the server: {detail}"));
    let parsed = crate::urlparse::urlsplit(hub_url)?;
    let host = parsed.hostname().unwrap_or_default();
    let port = parsed.port()?.filter(|port| *port != 0).unwrap_or(443) as u16;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let capture = Arc::new(Capture(std::sync::Mutex::new(None), schemes));
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|exc| unreachable(exc.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(capture.clone())
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|_| machine_error("invalid server host name"))?;
    let mut stream = crate::hub_transport::tcp_connect(&host, port)
        .map_err(|error| unreachable(error.truncated(200)))?;
    let timeout = Some(Duration::from_secs(10));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let mut connection = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|exc| unreachable(exc.to_string()))?;
    while connection.is_handshaking() {
        if connection.complete_io(&mut stream).is_err() {
            break;
        }
    }
    let captured = capture.0.lock().ok().and_then(|mut slot| slot.take());
    captured.ok_or_else(|| unreachable("the server presented no certificate".into()))
}

/// The SHA-256 fingerprint of the DER of the first certificate in a PEM file.
pub fn fingerprint_file(cert_path: &str) -> Result<Vec<u8>> {
    use rustls::pki_types::pem::PemObject;
    let data = std::fs::read(cert_path)
        .map_err(|exc| Error::value(format!("openssl x509 failed: Could not open file or uri for loading certificate from {cert_path}: {exc}")))?;
    let der = rustls::pki_types::CertificateDer::pem_slice_iter(&data)
        .next()
        .and_then(|item| item.ok())
        .ok_or_else(|| Error::value("openssl x509 failed: Could not find certificate"))?;
    Ok(Sha256::digest(der.as_ref()).to_vec())
}

fn command_repr(argv: &[String]) -> String {
    format!(
        "[{}]",
        argv.iter()
            .map(|a| repr_str(a))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Run a command with its stdout discarded; a nonzero exit or a timeout fails.
pub fn run_checked(argv: &[String], timeout: f64) -> Result<()> {
    let output = run_capture(argv, timeout, false)?;
    if output.0 != 0 {
        return Err(Error::subprocess(format!(
            "Command '{}' returned non-zero exit status {}.",
            command_repr(argv),
            output.0
        )));
    }
    Ok(())
}

/// Run a command with a timeout; (exit status, stdout if captured).
pub fn run_capture(argv: &[String], timeout: f64, capture: bool) -> Result<(i32, String, String)> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()
        .map_err(|exc| match exc.raw_os_error() {
            Some(code) => Error::os(format!(
                "[Errno {code}] {}: {}",
                crate::error::strerror(code),
                repr_str(&argv[0])
            )),
            None => Error::os(exc.to_string()),
        })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let readers = (
        stdout.map(|mut s| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = std::io::Read::read_to_string(&mut s, &mut text);
                text
            })
        }),
        stderr.map(|mut s| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = std::io::Read::read_to_string(&mut s, &mut text);
                text
            })
        }),
    );
    let deadline = crate::identity::monotonic() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|exc| Error::from_io(&exc, None))? {
            use std::os::unix::process::ExitStatusExt;
            let code = status
                .code()
                .unwrap_or_else(|| -status.signal().unwrap_or(1));
            let out = readers
                .0
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            let errs = readers
                .1
                .map(|h| h.join().unwrap_or_default())
                .unwrap_or_default();
            return Ok((code, out, errs));
        }
        if crate::identity::monotonic() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::subprocess(format!(
                "Command '{}' timed out after {} seconds",
                command_repr(argv),
                crate::wirejson::float_repr(timeout).trim_end_matches(".0")
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Install and start the local bridge after enrolling.
pub fn start_bridge(say: &dyn Fn(&str)) -> Result<bool> {
    let home = super::local::home_dir();
    let command = format!("{home}/bin/switchboard");
    if !super::service::supported() {
        let binary = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "switchboard".into());
        say(&format!(
            "Joined. Start the bridge with: {binary} bridge --config ~/.config/switchboard/bridge.json"
        ));
        return Ok(false);
    }
    super::install::install_current(&home)?;
    run_checked(&[command.clone(), "start".into(), "--json".into()], 60.0)?;
    for _ in 0..30 {
        let (_, stdout, _) = run_capture(
            &[command.clone(), "status".into(), "--json".into()],
            15.0,
            true,
        )?;
        if let Ok(Value::Object(status)) = wirejson::loads(stdout.as_bytes()) {
            if status.get("connected") == Some(&Value::Bool(true)) {
                return Ok(true);
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(false)
}

/// What `setup` asks the server for: the machine and owner (defaults when
/// `None`), the server, a pinned server certificate, and an invite.
pub struct Enrollment<'a> {
    pub machine: Option<&'a str>,
    pub owner: Option<&'a str>,
    /// `--server-url` (or `--hub-url`), when given.
    pub hub_url: Option<&'a str>,
    pub server_pin: Option<&'a str>,
    pub invite: Option<&'a Invite>,
}

/// Make or reuse the identity, redeem the invite (or confirm
/// an earlier enrollment), then start the bridge.
pub fn setup(
    config_path: &str,
    enrollment: &Enrollment<'_>,
    start: bool,
    say: &dyn Fn(&str),
) -> Result<Map<String, Value>> {
    let invite = enrollment.invite;
    let given_pin = enrollment.server_pin.map(normal_pin).transpose()?;
    let (hub_url, server_pin, machine) = match invite {
        Some(invite) => {
            if let Some(url) = enrollment.hub_url.filter(|u| !u.is_empty()) {
                if url.trim_end_matches('/') != invite.url {
                    return Err(machine_error(format!(
                        "--server-url {url} is not the server the invite is for ({})",
                        invite.url
                    )));
                }
            }
            if let (Some(given), Some(pinned)) = (&given_pin, &invite.pin) {
                if given != pinned {
                    return Err(machine_error(
                        "--server-pin differs from the server certificate the invite names",
                    ));
                }
            }
            if let (Some(given), Some(fixed)) = (enrollment.machine, &invite.machine) {
                if !given.is_empty() && given != fixed {
                    return Err(machine_error(format!(
                        "the invite is for the machine name {fixed}, not {given}"
                    )));
                }
            }
            (
                invite.url.clone(),
                invite.pin.clone().or(given_pin),
                invite.machine.as_deref().or(enrollment.machine),
            )
        }
        None => {
            if fsutil::lstat(&fsutil::absolute(config_path)?).is_err() {
                return Err(machine_error(format!(
                    "setup needs an invite: on the server run switchboard server invite, then here run switchboard init and paste it (or set {INVITE_VARIABLE})"
                )));
            }
            (
                enrollment
                    .hub_url
                    .filter(|u| !u.is_empty())
                    .unwrap_or("")
                    .to_string(),
                given_pin,
                enrollment.machine,
            )
        }
    };
    let (_config, machine, owner) = prepare(
        config_path,
        machine,
        enrollment.owner,
        &hub_url,
        server_pin.as_deref(),
        invite.is_some(),
    )?;
    let client_config = hub_config(config_path)?;
    let mut body = Map::new();
    body.insert("machine".into(), Value::from(machine.clone()));
    body.insert("owner".into(), Value::from(owner.clone()));
    if let Some(invite) = invite {
        body.insert("invite".into(), Value::from(invite.secret.clone()));
    }
    let value = call(
        &client_config,
        "POST",
        "/v1/enroll",
        Some(&Value::Object(body)),
    )?;
    if value.get("state").and_then(Value::as_str) != Some("approved") {
        return Err(machine_error(
            "the server sent a reply this version doesn't understand; update Switchboard here and on the server",
        ));
    }
    // An enrolled key keeps the name it joined under.
    let machine = match value.get("machine") {
        Some(Value::String(name)) => name.clone(),
        _ => machine,
    };
    let mut result = Map::new();
    result.insert("ok".into(), Value::Bool(true));
    result.insert("action".into(), Value::from("setup"));
    result.insert("machine".into(), Value::from(machine));
    result.insert("owner".into(), Value::from(owner));
    result.insert("enrolled".into(), Value::Bool(true));
    result.insert("started".into(), Value::Bool(false));
    if start {
        // A machine taken down stays down, as installs and init leave it:
        // the service is installed but not started or enabled.
        if super::service::is_down(config_path) || super::service::default_is_down() {
            if super::service::supported() {
                super::install::install_current(&super::local::home_dir())?;
            }
            result.insert("down".into(), Value::Bool(true));
        } else {
            result.insert("started".into(), Value::Bool(start_bridge(say)?));
        }
    }
    Ok(result)
}

/// Why `setup --invite VALUE` is refused, and how to pass the invite.
pub const INLINE_INVITE: &str = "--invite doesn't take the invite on the command line, where other users can see it (in ps) and your shell keeps it in its history. Pass - and give it on standard input (printf '%s\\n' \"$INVITE\" | switchboard setup --invite -), or set SWITCHBOARD_INVITE";

/// The variable `setup` reads the invite from when `--invite` is not given.
pub const INVITE_VARIABLE: &str = "SWITCHBOARD_INVITE";

/// The invite, kept out of the command line where other users see it in
/// `ps` and the shell keeps it in its history: `--invite -` reads it from
/// standard input (without echo on a terminal), and without `--invite` it
/// comes from `SWITCHBOARD_INVITE`. `--invite VALUE` is refused, as init
/// refuses it.
fn setup_invite(given: Option<&str>) -> Result<Option<Invite>> {
    let text = match given {
        Some("-") => {
            let text = read_secret_line()?;
            if crate::text::strip(&text).is_empty() {
                return Err(machine_error(
                    "--invite - read no invite from standard input",
                ));
            }
            text
        }
        Some(_) => return Err(machine_error(INLINE_INVITE)),
        None => match std::env::var(INVITE_VARIABLE) {
            Ok(value) if !crate::text::strip(&value).is_empty() => value,
            _ => return Ok(None),
        },
    };
    Invite::parse(&super::init::join::clean_invite(&text))
        .map(Some)
        .map_err(machine_error)
}

/// The terminal settings to restore if Ctrl-C arrives while echo is off.
static SAVED_TERMINAL: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
static ECHO_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One line from standard input; on a terminal, after a prompt and without echo.
fn read_secret_line() -> Result<String> {
    use std::sync::atomic::Ordering;
    // SAFETY: isatty on descriptor 0.
    let terminal = unsafe { libc::isatty(0) } == 1;
    let mut quiet = false;
    if terminal {
        err("Invite (from switchboard server invite): ");
        // SAFETY: tcgetattr fills a zeroed termios; tcsetattr reads one.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut settings) == 0 {
                let saved = *SAVED_TERMINAL.get_or_init(|| settings);
                let mut silent = saved;
                silent.c_lflag &= !libc::ECHO;
                ECHO_OFF.store(true, Ordering::SeqCst);
                quiet = libc::tcsetattr(0, libc::TCSANOW, &silent) == 0;
            }
        }
    }
    // At most 64 KiB: an invite is far shorter, and endless input without a
    // line break must not grow without bound.
    let mut line = String::new();
    let read = std::io::Read::take(std::io::stdin().lock(), 64 * 1024).read_line(&mut line);
    if quiet {
        if let Some(saved) = SAVED_TERMINAL.get() {
            // SAFETY: restores the settings read above.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
        }
        err("\n");
    }
    ECHO_OFF.store(false, Ordering::SeqCst);
    read.map_err(|exc| Error::from_io(&exc, None))?;
    Ok(line)
}

extern "C" fn on_interrupt(_signal: libc::c_int) {
    if ECHO_OFF.load(std::sync::atomic::Ordering::SeqCst) {
        if let Some(saved) = SAVED_TERMINAL.get() {
            // SAFETY: tcsetattr is async-signal-safe; the settings were saved
            // before echo was turned off.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, saved) };
        }
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(130) };
}

/// `switchboard setup`.
pub fn command(args: &Args) -> i32 {
    // SAFETY: a handler that only restores the terminal and exits with
    // status 130.
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_interrupt as *const () as libc::sighandler_t,
        )
    };
    let outcome = (|| -> Result<Map<String, Value>> {
        let json = args.json;
        let say = move |line: &str| {
            if json {
                err(&format!("{line}\n"));
            } else {
                out(&format!("{line}\n"));
            }
        };
        let invite = setup_invite(args.invite.as_deref())?;
        let enrollment = Enrollment {
            machine: args.machine.as_deref(),
            owner: args.owner.as_deref(),
            hub_url: args.hub_url.as_deref(),
            server_pin: args.server_pin.as_deref(),
            invite: invite.as_ref(),
        };
        let result = setup(&args.config, &enrollment, !args.no_start, &say)?;
        if !args.json {
            let theme = super::ui::Theme::for_fd(1);
            let width = super::format::output_width();
            let mut lines = super::ui::item(
                &theme,
                super::ui::Mark::Done,
                &format!(
                    "On the server as {} (owner {})",
                    str_value(&result["machine"]),
                    str_value(&result["owner"]),
                ),
                super::ui::Tone::Normal,
                width,
            );
            if result.get("started") == Some(&Value::Bool(true)) {
                lines.extend(super::ui::item(
                    &theme,
                    super::ui::Mark::Done,
                    "The bridge is running and connected",
                    super::ui::Tone::Normal,
                    width,
                ));
            } else {
                if result.get("down") == Some(&Value::Bool(true)) {
                    lines.extend(super::ui::para(
                        &theme,
                        "This machine is down (taken offline with `switchboard down`), so the bridge wasn't started.",
                        super::ui::Tone::Normal,
                        width,
                    ));
                }
                lines.push(String::new());
                lines.extend(super::ui::heading(&theme, "Next", width));
                let next: Vec<String> = if result.get("down") == Some(&Value::Bool(true)) {
                    vec!["Bring it back online with\n`switchboard up`".into()]
                } else if args.no_start {
                    vec!["Install and start the bridge when you're ready, with\n`switchboard install --start`".into()]
                } else {
                    vec![
                        "Start the bridge with\n`switchboard up`".into(),
                        "Connect chats with\n`/switchboard join`".into(),
                    ]
                };
                lines.extend(super::ui::steps(&theme, &next, width));
            }
            out(&format!("{}\n", lines.join("\n")));
        }
        Ok(result)
    })();
    match outcome {
        Ok(result) => {
            if args.json {
                out(&format!(
                    "{}\n",
                    wirejson::dumps(&Value::Object(result.clone()))
                ));
            }
            if truthy(result.get("ok")) {
                0
            } else {
                1
            }
        }
        Err(error) => {
            if args.json {
                let mut value = Map::new();
                value.insert("ok".into(), Value::Bool(false));
                value.insert("error".into(), Value::from(error.message.clone()));
                out(&format!("{}\n", wirejson::dumps(&Value::Object(value))));
            } else {
                print_error(&error.message, Some(&error), false);
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"any carnal pleas"), "YW55IGNhcm5hbCBwbGVhcw==");
    }

    #[test]
    fn certificate_is_long_lived_self_signed_client_auth() {
        let (key, cert) = make_certificate("new-box").unwrap();
        assert!(key.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
    }
}
