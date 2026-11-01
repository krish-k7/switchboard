//! TLS for the three TCP listeners.
//!
//! - Direct mode (mutual TLS, used by tests): the server certificate and key
//!   come from files and client certificates must chain to `client_ca`.
//! - Public certificates (ACME) and private-network mode: any client key may
//!   finish the handshake, proving it holds the key; the registry fingerprint
//!   is the only authorization, as with Caddy's `client_auth mode require`
//!   without a trust pool.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{Acceptor, WebPkiClientVerifier};
use rustls::{
    DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig, ServerConnection,
    SignatureScheme,
};
use sha2::{Digest, Sha256};

use super::acme::Certificates;

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn fingerprint(der: &[u8]) -> String {
    hex(&Sha256::digest(der))
}

/// Requires a client certificate and checks only that the client holds its
/// key (the handshake signature); trust comes from the registry.
#[derive(Debug)]
struct AnyClientKey {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn any_client_key() -> Arc<dyn ClientCertVerifier> {
    Arc::new(AnyClientKey {
        algorithms: provider().signature_verification_algorithms,
    })
}

fn builder() -> Result<rustls::ConfigBuilder<ServerConfig, rustls::WantsVerifier>, String> {
    ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|exc| exc.to_string())
}

pub fn read_chain(path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let chain = CertificateDer::pem_file_iter(path)
        .and_then(|items| items.collect::<Result<Vec<_>, _>>())
        .map_err(|exc| format!("{path}: {exc}"))?;
    if chain.is_empty() {
        return Err(format!("{path}: no certificate"));
    }
    Ok(chain)
}

pub fn read_key(path: &str) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::from_pem_file(path).map_err(|exc| format!("{path}: {exc}"))
}

/// Direct mode: TLS 1.2 or newer, client certificates required
/// and verified against `client_ca`.
pub fn direct(cert: &str, key: &str, client_ca: &str) -> Result<Arc<ServerConfig>, String> {
    let chain = read_chain(cert)?;
    let key = read_key(key)?;
    let mut roots = RootCertStore::empty();
    for ca in read_chain(client_ca)? {
        roots.add(ca).map_err(|exc| format!("{client_ca}: {exc}"))?;
    }
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .clear_root_hint_subjects()
        .build()
        .map_err(|exc| exc.to_string())?;
    let config = builder()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|exc| exc.to_string())?;
    Ok(Arc::new(config))
}

/// Private-network mode: a fixed self-signed server certificate.
pub fn private(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, String> {
    let config = builder()?
        .with_client_cert_verifier(any_client_key())
        .with_single_cert(chain, key)
        .map_err(|exc| exc.to_string())?;
    Ok(Arc::new(config))
}

/// Public certificates: the current ACME certificate for normal handshakes.
pub fn public(certificates: Arc<Certificates>) -> Result<Arc<ServerConfig>, String> {
    let config = builder()?
        .with_client_cert_verifier(any_client_key())
        .with_cert_resolver(certificates);
    Ok(Arc::new(config))
}

/// TLS-ALPN-01 validation handshakes: no client certificate, only the
/// `acme-tls/1` protocol, answered with the challenge certificate.
pub fn challenge(certificates: Arc<Certificates>) -> Result<Arc<ServerConfig>, String> {
    let resolver = super::acme::ChallengeResolver(certificates);
    let mut config = builder()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    config.alpn_protocols = vec![super::acme::ACME_TLS_ALPN.to_vec()];
    Ok(Arc::new(config))
}

/// How the TCP listener terminates TLS.
pub enum Termination {
    /// Direct or private-network mode: one configuration for every handshake.
    Fixed(Arc<ServerConfig>),
    /// Public certificates: the client hello picks the configuration.
    Public {
        hostname: String,
        normal: Arc<ServerConfig>,
        challenge: Arc<ServerConfig>,
    },
}

/// A finished handshake, or `None` when the connection should just close.
pub struct Handshake {
    pub stream: rustls::StreamOwned<ServerConnection, TcpStream>,
    pub fingerprint: String,
}

fn complete(
    mut conn: ServerConnection,
    mut sock: TcpStream,
) -> Option<(ServerConnection, TcpStream)> {
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).ok()?;
    }
    if conn.wants_write() {
        conn.complete_io(&mut sock).ok()?;
    }
    Some((conn, sock))
}

pub fn accept(termination: &Termination, sock: TcpStream) -> Option<Handshake> {
    let timeout = Some(Duration::from_secs(10));
    sock.set_read_timeout(timeout).ok()?;
    sock.set_write_timeout(timeout).ok()?;
    let (conn, sock) = match termination {
        Termination::Fixed(config) => complete(ServerConnection::new(config.clone()).ok()?, sock)?,
        Termination::Public {
            hostname,
            normal,
            challenge,
        } => {
            let mut sock = sock;
            let mut acceptor = Acceptor::default();
            let accepted = loop {
                if acceptor.read_tls(&mut sock).ok()? == 0 {
                    return None;
                }
                match acceptor.accept() {
                    Ok(Some(accepted)) => break accepted,
                    Ok(None) => continue,
                    Err((_, mut alert)) => {
                        let _ = alert.write_all(&mut sock);
                        return None;
                    }
                }
            };
            let hello = accepted.client_hello();
            // Only the configured name is served, as Caddy's site block and
            // strict SNI host matching do.
            if !hello
                .server_name()
                .is_some_and(|name| name.eq_ignore_ascii_case(hostname))
            {
                return None;
            }
            let validation = hello
                .alpn()
                .is_some_and(|mut protocols| protocols.any(|p| p == super::acme::ACME_TLS_ALPN));
            let config = if validation {
                challenge.clone()
            } else {
                normal.clone()
            };
            let conn = match accepted.into_connection(config) {
                Ok(conn) => conn,
                Err((_, mut alert)) => {
                    let _ = alert.write_all(&mut sock);
                    return None;
                }
            };
            let (mut conn, mut sock) = complete(conn, sock)?;
            if validation {
                conn.send_close_notify();
                let _ = conn.complete_io(&mut sock);
                return None;
            }
            (conn, sock)
        }
    };
    let fingerprint = fingerprint(conn.peer_certificates()?.first()?);
    Some(Handshake {
        stream: rustls::StreamOwned::new(conn, sock),
        fingerprint,
    })
}

/// The persisted private-network certificate, created on first start.
pub struct PrivateIdentity {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    pub fingerprint: String,
    pub created: bool,
}

/// A new private-network certificate for `names`: its key and certificate PEM.
pub fn issue_private(names: &[String]) -> Result<(String, String), String> {
    let key = rcgen::KeyPair::generate().map_err(|exc| exc.to_string())?;
    let mut params =
        rcgen::CertificateParams::new(names.to_vec()).map_err(|exc| exc.to_string())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Switchboard server");
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let cert = params.self_signed(&key).map_err(|exc| exc.to_string())?;
    Ok((key.serialize_pem(), cert.pem()))
}

/// Whether a certificate (DER) is valid for every name (a DNS name or an
/// address), as a client connecting to that name would check it.
pub fn covers(der: &[u8], names: &[String]) -> bool {
    let der = CertificateDer::from(der);
    let Ok(parsed) = rustls::server::ParsedCertificate::try_from(&der) else {
        return false;
    };
    names.iter().all(|name| {
        rustls::pki_types::ServerName::try_from(name.as_str())
            .is_ok_and(|name| rustls::client::verify_server_name(&parsed, &name).is_ok())
    })
}

pub fn private_identity(directory: &str, names: &[String]) -> Result<PrivateIdentity, String> {
    let cert_path = format!("{directory}/private-cert.pem");
    let key_path = format!("{directory}/private-key.pem");
    let exists = std::fs::symlink_metadata(&cert_path).is_ok();
    let mut created = false;
    if !exists {
        let (key, cert) = issue_private(names)?;
        // The key first: a certificate without its key is never left behind.
        super::write_private(&key_path, key.as_bytes())?;
        super::write_private(&cert_path, cert.as_bytes())?;
        created = true;
    }
    let chain = read_chain(&cert_path)?;
    let key = read_key(&key_path)?;
    let fingerprint = fingerprint(&chain[0]);
    Ok(PrivateIdentity {
        chain,
        key,
        fingerprint,
        created,
    })
}
