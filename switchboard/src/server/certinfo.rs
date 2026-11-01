//! What a certificate says, read with a minimal DER walk: its validity (for
//! ACME renewal), and its subject, issuer, and names (for `server status`).
//! Only the fields Switchboard's own certificates and Let's Encrypt's use
//! are read; anything else reads as absent.

use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::CertificateDer;

/// One DER element: its tag, its content, and what follows it.
fn element(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)? as usize;
    let (length, header) = if first < 0x80 {
        (first, 2)
    } else {
        let count = first & 0x7F;
        if count == 0 || count > 4 {
            return None;
        }
        let mut length = 0usize;
        for index in 0..count {
            length = (length << 8) | *data.get(2 + index)? as usize;
        }
        (length, 2 + count)
    };
    let content = data.get(header..header.checked_add(length)?)?;
    Some((tag, content, &data[header + length..]))
}

/// The elements in `data`, in order (stopping at the first malformed one).
fn elements(mut data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut found = Vec::new();
    while let Some((tag, content, rest)) = element(data) {
        found.push((tag, content));
        data = rest;
    }
    found
}

/// A UTCTime (0x17) or GeneralizedTime (0x18) in UTC.
fn time(tag: u8, text: &[u8]) -> Option<SystemTime> {
    let text = std::str::from_utf8(text).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 => {
            let year: i64 = text.get(..2)?.parse().ok()?;
            (
                if year < 50 { 2000 + year } else { 1900 + year },
                text.get(2..)?,
            )
        }
        0x18 => (text.get(..4)?.parse().ok()?, text.get(4..)?),
        _ => return None,
    };
    let field = |range: std::ops::Range<usize>| -> Option<i64> { rest.get(range)?.parse().ok() };
    let (month, day, hour, minute, second) = (
        field(0..2)?,
        field(2..4)?,
        field(4..6)?,
        field(6..8)?,
        field(8..10)?,
    );
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).ok()?))
}

/// The parts of a certificate's to-be-signed body that are read here.
struct Body<'a> {
    issuer: &'a [u8],
    validity: &'a [u8],
    subject: &'a [u8],
    extensions: Option<&'a [u8]>,
}

fn body(der: &[u8]) -> Option<Body<'_>> {
    let (_, certificate, _) = element(der)?;
    let (_, tbs, _) = element(certificate)?;
    let mut rest = tbs;
    if rest.first() == Some(&0xA0) {
        rest = element(rest)?.2; // version
    }
    rest = element(rest)?.2; // serial
    rest = element(rest)?.2; // signature algorithm
    let (_, issuer, rest) = element(rest)?;
    let (_, validity, rest) = element(rest)?;
    let (_, subject, rest) = element(rest)?;
    let (_, _, rest) = element(rest)?; // subject public key
    let extensions = elements(rest)
        .into_iter()
        .find(|(tag, _)| *tag == 0xA3)
        .and_then(|(_, explicit)| element(explicit).map(|(_, list, _)| list));
    Some(Body {
        issuer,
        validity,
        subject,
        extensions,
    })
}

/// A certificate's notBefore and notAfter.
pub fn validity(cert: &CertificateDer<'_>) -> Option<(SystemTime, SystemTime)> {
    let body = body(cert.as_ref())?;
    let (before_tag, before, after_rest) = element(body.validity)?;
    let (after_tag, after, _) = element(after_rest)?;
    Some((time(before_tag, before)?, time(after_tag, after)?))
}

const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const ORGANIZATION: &[u8] = &[0x55, 0x04, 0x0A];
const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];

/// The common name and organization in a distinguished name.
fn name_parts(name: &[u8]) -> (Option<String>, Option<String>) {
    let (mut common, mut organization) = (None, None);
    for (_, set) in elements(name) {
        for (_, attribute) in elements(set) {
            let parts = elements(attribute);
            let [(0x06, oid), (tag, value), ..] = parts.as_slice() else {
                continue;
            };
            // UTF8String, PrintableString, TeletexString, IA5String.
            if ![0x0C, 0x13, 0x14, 0x16].contains(tag) {
                continue;
            }
            let text = String::from_utf8_lossy(value).into_owned();
            if *oid == COMMON_NAME {
                common.get_or_insert(text);
            } else if *oid == ORGANIZATION {
                organization.get_or_insert(text);
            }
        }
    }
    (common, organization)
}

/// A distinguished name as people say it: `Let's Encrypt R11`, or the
/// common name alone when it already includes the organization.
fn name_text(name: &[u8]) -> String {
    match name_parts(name) {
        (Some(common), Some(organization)) if !common.contains(&organization) => {
            format!("{organization} {common}")
        }
        (Some(common), _) => common,
        (None, Some(organization)) => organization,
        (None, None) => String::new(),
    }
}

/// The DNS names and addresses in the subject alternative names.
fn alt_names(extensions: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for (_, extension) in elements(extensions) {
        let parts = elements(extension);
        let Some((0x06, oid)) = parts.first() else {
            continue;
        };
        if *oid != SUBJECT_ALT_NAME {
            continue;
        }
        let Some((_, value)) = parts.iter().find(|(tag, _)| *tag == 0x04) else {
            continue;
        };
        let Some((_, list, _)) = element(value) else {
            continue;
        };
        for (tag, content) in elements(list) {
            match (tag, content.len()) {
                (0x82, _) => names.push(String::from_utf8_lossy(content).into_owned()),
                (0x87, 4) => {
                    let octets: [u8; 4] = content.try_into().unwrap_or_default();
                    names.push(IpAddr::from(octets).to_string());
                }
                (0x87, 16) => {
                    let octets: [u8; 16] = content.try_into().unwrap_or_default();
                    names.push(IpAddr::from(octets).to_string());
                }
                _ => {}
            }
        }
    }
    names
}

/// What `server status` shows of a certificate.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    /// The subject's common name (or organization).
    pub subject: String,
    /// The issuer, as people say it.
    pub issuer: String,
    /// The certificate signed itself (its issuer is its subject).
    pub self_signed: bool,
    /// The names and addresses it covers.
    pub names: Vec<String>,
    pub not_before: SystemTime,
    pub not_after: SystemTime,
    /// The SHA-256 of the DER: the pin clients check in private mode.
    pub sha256: String,
}

/// Read a certificate (DER).
pub fn describe(der: &[u8]) -> Option<Described> {
    let body = body(der)?;
    let (before_tag, before, after_rest) = element(body.validity)?;
    let (after_tag, after, _) = element(after_rest)?;
    Some(Described {
        subject: name_text(body.subject),
        issuer: name_text(body.issuer),
        self_signed: body.issuer == body.subject,
        names: body.extensions.map(alt_names).unwrap_or_default(),
        not_before: time(before_tag, before)?,
        not_after: time(after_tag, after)?,
        sha256: super::tls::fingerprint(der),
    })
}

/// Read the first certificate in a PEM file's text.
pub fn describe_pem(pem: &[u8]) -> Option<Described> {
    use rustls::pki_types::pem::PemObject;
    let der = CertificateDer::pem_slice_iter(pem).next()?.ok()?;
    describe(der.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_certificate_reads_as_self_signed_for_its_names() {
        let names = vec!["sb.lan".to_string(), "10.0.0.5".into(), "::1".into()];
        let (_, pem) = super::super::tls::issue_private(&names).unwrap();
        let described = describe_pem(pem.as_bytes()).unwrap();
        assert_eq!(described.subject, "Switchboard server");
        assert_eq!(described.issuer, "Switchboard server");
        assert!(described.self_signed);
        assert_eq!(described.names, names);
        assert_eq!(
            described
                .not_after
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            4_891_363_200 // 2125-01-01
        );
        assert_eq!(described.sha256.len(), 64);
    }

    #[test]
    fn an_issued_certificate_names_its_issuer() {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::OrganizationName, "Let's Encrypt");
        ca.distinguished_name.push(rcgen::DnType::CommonName, "R11");
        let issuer = rcgen::Issuer::new(ca, ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let mut leaf =
            rcgen::CertificateParams::new(vec!["switchboard.example.com".to_string()]).unwrap();
        leaf.distinguished_name
            .push(rcgen::DnType::CommonName, "switchboard.example.com");
        leaf.not_before = rcgen::date_time_ymd(2026, 10, 1);
        leaf.not_after = rcgen::date_time_ymd(2026, 12, 30);
        let cert = leaf.signed_by(&key, &issuer).unwrap();
        let described = describe(cert.der()).unwrap();
        assert_eq!(described.subject, "switchboard.example.com");
        assert_eq!(described.issuer, "Let's Encrypt R11");
        assert!(!described.self_signed);
        assert_eq!(described.names, ["switchboard.example.com"]);
        assert_eq!(
            validity(cert.der()),
            Some((described.not_before, described.not_after))
        );
        assert!(describe(b"not a certificate").is_none());
        assert!(describe(&cert.der()[..40]).is_none());
    }
}
