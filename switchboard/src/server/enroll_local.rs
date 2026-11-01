//! `switchboard server enroll-local`: the root SSH fallback that adds one
//! machine's public certificate to the registry.
//!
//! Reads one enrollment request (`{machine, owner, certificate_pem}`; older
//! clients also send a `view` flag, which is accepted and ignored) on stdin,
//! validates the self-signed client certificate with the system `openssl`,
//! and under the registry lock adds or confirms the fingerprint. It requires
//! an ingress that accepts any client key and leaves authorization to the
//! registry: the server's own TLS
//! (`"tls": "acme"` or `"private"` in `/etc/switchboard/server.json`, as
//! `switchboard server install` writes it), or else the Caddy pinned-key
//! ingress (`client_auth mode require`, no trust pool). It changes no trust
//! files and restarts nothing. Prints one JSON line; failures exit 1.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::hub::label;
use super::jsonval::{self, dict, get, Dict, JsonStr, Val};
use crate::text;

const MAX_REQUEST: usize = 32768;
pub const OPENSSL: &str = "/usr/bin/openssl";

/// The installed locations (defaults below; tests substitute their own).
pub struct Paths {
    pub registry: String,
    pub caddyfile: String,
    pub lock: String,
    /// The server config `switchboard server install` writes; when present,
    /// its listener mode decides which ingress checks apply.
    pub config: String,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            registry: "/var/lib/switchboard/peers.json".into(),
            caddyfile: "/etc/switchboard-caddy/Caddyfile".into(),
            lock: "/var/lib/switchboard/peers.json.lock".into(),
            config: super::install::CONFIG.into(),
        }
    }
}

/// Which ingress terminates client TLS in front of the registry.
#[derive(Debug, PartialEq, Eq)]
enum Ingress {
    /// The server's built-in TLS (`acme` or `private`): any client key
    /// completes the handshake and the registry alone authorizes.
    BuiltIn,
    /// Caddy in front of the Unix socket (no server config, or one with
    /// `unix_socket`).
    Caddy,
}

fn ingress(paths: &Paths) -> Result<Ingress> {
    if std::fs::symlink_metadata(&paths.config).is_err() {
        return Ok(Ingress::Caddy);
    }
    let (data, _) = read_file(&paths.config)?;
    let Some(Val::Dict(config)) = jsonval::loads(&data) else {
        return refuse("invalid server config");
    };
    match (get(&config, "tls"), get(&config, "unix_socket")) {
        (Some(mode), None) if mode.is_str("acme") || mode.is_str("private") => {
            Ok(Ingress::BuiltIn)
        }
        (None, Some(_)) => Ok(Ingress::Caddy),
        _ => refuse(
            "the server must accept any client key and authorize by the registry (tls acme or private, or Caddy in front of unix_socket)",
        ),
    }
}

/// `Reported` failures (a refused request, an invalid value, a file-system or
/// command error) print `{"ok": false, "error": ...}` on stdout; `Other` is an
/// unexpected failure, printed on stderr instead.
#[derive(Debug)]
pub enum Failure {
    Reported(String),
    Other(String),
}

pub(super) type Result<T> = std::result::Result<T, Failure>;

pub(super) fn refuse<T>(message: &str) -> Result<T> {
    Err(Failure::Reported(message.into()))
}

pub(super) fn os_error(exc: std::io::Error, path: &str) -> Failure {
    match exc.raw_os_error() {
        Some(code) => Failure::Reported(format!(
            "[Errno {code}] {}: {}",
            crate::error::strerror(code),
            text::repr_str(path)
        )),
        None => Failure::Reported(exc.to_string()),
    }
}

/// Run a command with a fixed minimal environment, capturing its output and
/// killing it after 45 seconds; a nonzero exit fails with the command's own
/// message (stderr, else stdout).
fn run(command: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = Command::new(command[0])
        .args(&command[1..])
        .env_clear()
        .envs([
            ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
            ("LC_ALL", "C"),
            ("HOME", "/root"),
        ])
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|exc| os_error(exc, command[0]))?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Reported(format!(
                    "Command '{command:?}' timed out after 45 seconds"
                )));
            }
            Err(exc) => return Err(os_error(exc, command[0])),
        }
    };
    let (stdout, stderr) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    if !status.success() {
        let chosen = if stderr.is_empty() { &stdout } else { &stderr };
        let detail = String::from_utf8_lossy(chosen);
        let detail = text::take_chars(text::strip(&detail), 2048);
        return Err(Failure::Reported(format!(
            "command failed: {}: {detail}",
            command[0]
        )));
    }
    Ok(stdout)
}

fn ascii(data: &[u8]) -> String {
    data.iter()
        .map(|b| if b.is_ascii() { *b as char } else { '\u{fffd}' })
        .collect()
}

fn is_regex_space(c: char) -> bool {
    text::is_space(c)
}

/// `\s*-----BEGIN CERTIFICATE-----\r?\n[A-Za-z0-9+/=\r\n]+-----END CERTIFICATE-----\s*`
fn one_certificate(pem: &str) -> bool {
    let rest = pem.trim_start_matches(is_regex_space);
    let Some(rest) = rest.strip_prefix("-----BEGIN CERTIFICATE-----") else {
        return false;
    };
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix('\n') else {
        return false;
    };
    let body_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '\r' | '\n')))
        .unwrap_or(rest.len());
    if body_end == 0 {
        return false;
    }
    let Some(rest) = rest[body_end..].strip_prefix("-----END CERTIFICATE-----") else {
        return false;
    };
    rest.chars().all(is_regex_space)
}

/// The value of the first `X509v3 NAME:` extension in an `openssl x509 -text`
/// listing (optionally marked `critical`): the trimmed next non-blank line
/// after the header line, or empty when there is none.
fn extension(listing: &str, name: &str) -> String {
    let marker = format!("X509v3 {name}:");
    let mut from = 0;
    while let Some(found) = listing[from..].find(&marker) {
        let start = from + found + marker.len();
        let mut rest = &listing[start..];
        rest = rest.strip_prefix(" critical").unwrap_or(rest);
        // \s*\n: whitespace ending at a newline (backtracking to the last one).
        let spaces: usize = rest
            .char_indices()
            .find(|(_, c)| !is_regex_space(*c))
            .map_or(rest.len(), |(i, _)| i);
        if let Some(newline) = rest[..spaces].rfind('\n') {
            let after = &rest[newline + 1..];
            let indent = after
                .char_indices()
                .find(|(_, c)| !is_regex_space(*c))
                .map_or(after.len(), |(i, _)| i);
            // \s+ needs at least one character and may not eat the whole line.
            let mut cut = indent;
            while cut > 0 {
                let value = &after[cut..];
                let line_end = value.find('\n').unwrap_or(value.len());
                if line_end > 0 {
                    return value[..line_end].trim_matches(is_regex_space).to_string();
                }
                cut -= after[..cut].chars().next_back().map_or(1, char::len_utf8);
            }
        }
        from = start;
    }
    String::new()
}

/// Validate the request and its certificate; the canonical PEM and the
/// SHA-256 fingerprint of its DER encoding.
fn certificate(request: &Val, openssl: &str) -> Result<(Vec<u8>, String)> {
    let Some(map) = request.as_dict() else {
        return refuse("invalid request fields");
    };
    // `view` is the removed browser viewer's permission flag, which older
    // clients still send; it is accepted and ignored.
    let fields = ["machine", "owner", "certificate_pem"];
    let legacy = usize::from(get(map, "view").is_some());
    if map.len() != fields.len() + legacy || !fields.iter().all(|f| get(map, f).is_some()) {
        return refuse("invalid request fields");
    }
    let machine_ok = get(map, "machine").and_then(Val::as_str).is_some_and(|m| {
        (1..=64).contains(&m.len())
            && m.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if !machine_ok {
        return refuse("invalid machine");
    }
    if label(get(map, "owner"), 64).is_err() {
        return refuse("invalid owner");
    }
    let pem = get(map, "certificate_pem").and_then(Val::as_json_str);
    let Some(pem) = pem.filter(|p| p.char_len() <= 16384) else {
        return refuse("expected one public certificate");
    };
    let Some(pem) = pem.as_str().filter(|p| one_certificate(p)) else {
        return refuse("expected one public certificate");
    };
    if !pem.is_ascii() {
        return refuse("'ascii' codec can't encode characters in the certificate");
    }
    let directory = TempDir::new()?;
    let cert = format!("{}/client.pem", directory.0);
    std::fs::write(&cert, pem).map_err(|exc| os_error(exc, &cert))?;
    let base = [openssl, "x509", "-in", cert.as_str()];
    let with = |extra: &[&'static str]| -> Vec<&str> {
        base.iter().copied().chain(extra.iter().copied()).collect()
    };
    let canonical = run(&with(&["-outform", "PEM"]), None)?;
    let der = run(&with(&["-outform", "DER"]), None)?;
    let listing = ascii(&run(&with(&["-text", "-noout"]), None)?);
    let names = ascii(&run(
        &with(&["-subject", "-issuer", "-noout", "-nameopt", "RFC2253"]),
        None,
    )?);
    let names: Vec<&str> = split_lines(&names);
    let value = |line: &str| {
        line.split_once('=')
            .map_or("", |(_, v)| v)
            .trim_matches(is_regex_space)
            .to_string()
    };
    if names.len() != 2 || value(names[0]) != value(names[1]) {
        return refuse("certificate must be self-signed");
    }
    let extensions = ascii(&run(
        &with(&[
            "-ext",
            "basicConstraints,keyUsage,extendedKeyUsage",
            "-noout",
        ]),
        None,
    )?);
    if extension(&extensions, "Basic Constraints") != "CA:FALSE" {
        return refuse("certificate must be a non-CA leaf");
    }
    if extension(&extensions, "Extended Key Usage") != "TLS Web Client Authentication" {
        return refuse("certificate must have clientAuth-only extended key usage");
    }
    let usage = extension(&extensions, "Key Usage");
    let usage: Vec<&str> = usage.split(", ").collect();
    if !usage.contains(&"Digital Signature") || usage.contains(&"Certificate Sign") {
        return refuse("certificate requires signing usage without certificate-signing authority");
    }
    let public_key = run(&with(&["-pubkey", "-noout"]), None)?;
    let key_text = ascii(&run(
        &[openssl, "pkey", "-pubin", "-text", "-noout"],
        Some(&public_key),
    )?);
    let lines: Vec<&str> = key_text.split('\n').collect();
    let strength = lines
        .iter()
        .find_map(|line| {
            let rest = line
                .strip_prefix("Public-Key:")?
                .trim_start_matches(is_regex_space);
            let digits = rest.strip_prefix('(')?;
            let end = digits.find(|c: char| !c.is_ascii_digit())?;
            let after = &digits[end..];
            (end > 0
                && after.starts_with(" bit")
                && !after[4..].starts_with(|c: char| c.is_alphanumeric() || c == '_'))
            .then(|| digits[..end].parse::<u64>().unwrap_or(0))
        })
        .unwrap_or(0);
    let rsa = lines.iter().any(|line| line.starts_with("Modulus:"));
    let ec = lines
        .iter()
        .any(|line| line.starts_with("ASN1 OID:") || line.starts_with("Field Type:"));
    let ed25519 = key_text.starts_with("ED25519 Public-Key:");
    if !((rsa && strength >= 2048) || (ec && strength >= 256) || ed25519) {
        return refuse("unsupported or weak public key");
    }
    let signatures: Vec<String> = listing
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim_start_matches(is_regex_space);
            if trimmed.len() == line.len() {
                return None;
            }
            let rest = trimmed
                .strip_prefix("Signature Algorithm:")?
                .trim_start_matches(is_regex_space);
            let word: String = rest.chars().take_while(|c| !is_regex_space(*c)).collect();
            (!word.is_empty()).then_some(word)
        })
        .collect();
    if signatures.is_empty()
        || signatures
            .iter()
            .any(|s| s.to_lowercase().contains("sha1") || s.to_lowercase().contains("md5"))
    {
        return refuse("weak certificate signature");
    }
    // Verify the leaf's self-signature and EKU; the registry pins keys, so dates are not checked.
    run(
        &[
            openssl,
            "verify",
            "-partial_chain",
            "-check_ss_sig",
            "-no_check_time",
            "-purpose",
            "sslclient",
            "-CAfile",
            &cert,
            &cert,
        ],
        None,
    )?;
    Ok((canonical, super::tls::hex(&Sha256::digest(&der))))
}

/// Split ASCII text into lines at CRLF, CR, LF, VT, FF, and the separators
/// 0x1C-0x1E, without the line endings.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest
            .find(['\r', '\n', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}'])
            .unwrap_or(rest.len());
        lines.push(&rest[..end]);
        rest = &rest[end..];
        if rest.starts_with("\r\n") {
            rest = &rest[2..];
        } else if !rest.is_empty() {
            rest = &rest[1..];
        }
    }
    lines
}

struct TempDir(String);

impl TempDir {
    fn new() -> Result<Self> {
        let base = std::env::var("TMPDIR")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "/tmp".into());
        let template = CString::new(format!(
            "{}/switchboard-enroll-XXXXXX",
            base.trim_end_matches('/')
        ))
        .map_err(|_| Failure::Reported("embedded null byte".into()))?;
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        if unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null() {
            return Err(os_error(std::io::Error::last_os_error(), &base));
        }
        raw.pop();
        Ok(TempDir(String::from_utf8_lossy(&raw).into_owned()))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `read_file`: a regular, singly linked file of at most 4 MiB, never a link.
pub(super) fn read_file(path: &str) -> Result<(Vec<u8>, std::fs::Metadata)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|exc| os_error(exc, path))?;
    let info = file.metadata().map_err(|exc| os_error(exc, path))?;
    if !info.file_type().is_file() || info.nlink() != 1 || info.size() > 4 * 1024 * 1024 {
        return refuse("invalid authoritative file");
    }
    let mut data = Vec::new();
    (&file)
        .read_to_end(&mut data)
        .map_err(|exc| os_error(exc, path))?;
    Ok((data, info))
}

/// Replace `path` atomically with `data`, keeping its mode, owner, and group.
pub(super) fn atomic(path: &str, data: &[u8], info: &std::fs::Metadata) -> Result<()> {
    let directory = crate::fsutil::parent(path);
    let template = CString::new(format!("{directory}/.enroll-XXXXXX"))
        .map_err(|_| Failure::Reported("embedded null byte".into()))?;
    let mut raw = template.into_bytes_with_nul();
    // SAFETY: raw is a writable NUL-terminated template.
    let fd = unsafe { libc::mkstemp(raw.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        return Err(os_error(std::io::Error::last_os_error(), &directory));
    }
    raw.pop();
    let temporary = String::from_utf8_lossy(&raw).into_owned();
    // SAFETY: fd is a new descriptor owned by this file.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let result = (|| -> Result<()> {
        let fail = |exc: std::io::Error| os_error(exc, &temporary);
        // SAFETY: file is open; the calls take plain integers.
        unsafe {
            if libc::fchmod(file.as_raw_fd(), (info.mode() & 0o7777) as libc::mode_t) != 0
                || libc::fchown(file.as_raw_fd(), info.uid(), info.gid()) != 0
            {
                return Err(fail(std::io::Error::last_os_error()));
            }
        }
        file.write_all(data).map_err(fail)?;
        file.flush().map_err(fail)?;
        // SAFETY: file is open.
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return Err(fail(std::io::Error::last_os_error()));
        }
        std::fs::rename(&temporary, path).map_err(fail)?;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(&directory)
            .map_err(|exc| os_error(exc, &directory))?;
        // SAFETY: dir is open.
        if unsafe { libc::fsync(dir.as_raw_fd()) } != 0 {
            return Err(os_error(std::io::Error::last_os_error(), &directory));
        }
        Ok(())
    })();
    if std::path::Path::new(&temporary).exists() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The registry lock at `path`, owned like the registry (`owner`) so the
/// server can take it too.
pub(super) fn open_lock(path: &str, owner: &std::fs::Metadata) -> Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|exc| os_error(exc, path))?;
    let mut info = lock.metadata().map_err(|exc| os_error(exc, path))?;
    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    if info.uid() != owner.uid() && info.uid() == euid && info.size() == 0 {
        // Created just now by root.
        // SAFETY: lock is open.
        if unsafe { libc::fchown(lock.as_raw_fd(), owner.uid(), owner.gid()) } != 0 {
            return Err(os_error(std::io::Error::last_os_error(), path));
        }
        info = lock.metadata().map_err(|exc| os_error(exc, path))?;
    }
    if !info.file_type().is_file()
        || info.nlink() != 1
        || info.uid() != owner.uid()
        || info.mode() & 0o7777 != 0o600
    {
        return refuse("unsafe enrollment lock");
    }
    Ok(lock)
}

/// Equality of decoded JSON values where numbers compare by value and
/// booleans count as 0 and 1 (`true == 1 == 1.0`).
fn value_eq(left: &Val, right: &Val) -> bool {
    fn number(value: &Val) -> Option<f64> {
        match value {
            Val::Bool(flag) => Some(f64::from(u8::from(*flag))),
            Val::Int(digits) => digits.parse().ok(),
            Val::Float(number) => Some(*number),
            _ => None,
        }
    }
    match (left, right) {
        (Val::Int(a), Val::Int(b)) => a == b,
        (Val::Null, Val::Null) => true,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::List(a), Val::List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| value_eq(x, y))
        }
        (Val::Dict(a), Val::Dict(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, x)| b.get(k).is_some_and(|y| value_eq(x, y)))
        }
        _ => matches!((number(left), number(right)), (Some(a), Some(b)) if a == b),
    }
}

pub fn enroll(request: &Val, paths: &Paths, openssl: &str) -> Result<Dict> {
    let (_pem, fingerprint) = certificate(request, openssl)?;
    let map = request.as_dict().expect("validated");
    if ingress(paths)? == Ingress::Caddy {
        let (config, _) = read_file(&paths.caddyfile)?;
        let contains = |needle: &[u8]| config.windows(needle.len()).any(|w| w == needle);
        if !contains(b"mode require\n") || contains(b"trust_pool") {
            return refuse("Caddy must pin client keys through the registry (client_auth mode require, no trust pool)");
        }
    }
    let owner =
        std::fs::symlink_metadata(&paths.registry).map_err(|exc| os_error(exc, &paths.registry))?;
    let lock = open_lock(&paths.lock, &owner)?;
    // SAFETY: lock is open for the rest of this function.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(os_error(std::io::Error::last_os_error(), &paths.lock));
    }
    let (old_registry, registry_info) = read_file(&paths.registry)?;
    if registry_info.mode() & 0o7777 != 0o600 {
        return refuse("unsafe authoritative file permissions");
    }
    let registry = jsonval::loads(&old_registry)
        .ok_or_else(|| Failure::Reported("Expecting value: invalid JSON registry".into()))?;
    let Some(registry) = registry.as_dict().filter(|r| r.len() <= 1024) else {
        return refuse("invalid or full registry");
    };
    let machine = get(map, "machine").cloned().unwrap_or(Val::Null);
    let desired = dict([
        ("machine", machine.clone()),
        ("owner", get(map, "owner").cloned().unwrap_or(Val::Null)),
        ("enabled", true.into()),
    ]);
    let result = |unchanged: bool| {
        dict([
            ("ok", true.into()),
            ("machine", machine.clone()),
            ("fingerprint", fingerprint.as_str().into()),
            ("unchanged", unchanged.into()),
        ])
    };
    for (existing, metadata) in registry {
        let valid_key = existing.as_str().is_some_and(|fp| {
            fp.len() == 64
                && fp
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        let Some(metadata) = metadata.as_dict().filter(|_| valid_key) else {
            return refuse("invalid existing registry");
        };
        if existing.eq_str(&fingerprint) {
            let same = desired
                .iter()
                .all(|(key, value)| value_eq(metadata.get(key).unwrap_or(&Val::Null), value));
            if !same {
                return refuse("identity exists with different metadata or is revoked");
            }
            return Ok(result(true));
        }
        if value_eq(get(metadata, "machine").unwrap_or(&Val::Null), &machine)
            && get(metadata, "enabled") == Some(&Val::Bool(true))
        {
            return refuse("machine already belongs to another certificate");
        }
    }
    if registry.len() == 1024 {
        return refuse("registry full");
    }
    let mut updated = registry.clone();
    updated.insert(JsonStr::new(&fingerprint), Val::Dict(desired));
    // The server rereads the registry on its next request; no service restarts.
    let mut text = jsonval::dumps(&Val::Dict(updated), jsonval::INDENT_SORTED);
    text.push('\n');
    atomic(&paths.registry, text.as_bytes(), &registry_info)?;
    drop(lock);
    Ok(result(false))
}

fn account(name: &str, group: bool) -> Result<u32> {
    let cname = CString::new(name).expect("static name");
    // SAFETY: getpwnam/getgrnam return static storage or null; read at once.
    unsafe {
        if group {
            let entry = libc::getgrnam(cname.as_ptr());
            if entry.is_null() {
                return Err(Failure::Other(format!(
                    "KeyError: \"getgrnam(): name not found: '{name}'\""
                )));
            }
            Ok((*entry).gr_gid)
        } else {
            let entry = libc::getpwnam(cname.as_ptr());
            if entry.is_null() {
                return Err(Failure::Other(format!(
                    "KeyError: \"getpwnam(): name not found: '{name}'\""
                )));
            }
            Ok((*entry).pw_uid)
        }
    }
}

fn checked_main(argv: &[String]) -> Result<Dict> {
    // SAFETY: umask and geteuid cannot fail.
    unsafe { libc::umask(0o077) };
    if unsafe { libc::geteuid() } != 0 || !argv.is_empty() {
        return refuse("run the installed helper as root with no arguments");
    }
    let paths = Paths::default();
    let (uid, gid) = (
        account("switchboard", false)?,
        account("switchboard", true)?,
    );
    let mut directories = vec![(crate::fsutil::parent(&paths.registry), uid, gid, 0o700)];
    let mut files = vec![(paths.registry.clone(), uid, gid, 0o600)];
    if std::fs::symlink_metadata(&paths.config).is_ok() {
        directories.push((crate::fsutil::parent(&paths.config), uid, gid, 0o700));
        files.push((paths.config.clone(), uid, gid, 0o600));
    }
    if ingress(&paths)? == Ingress::Caddy {
        let caddy = account("caddy", true)?;
        directories.push((crate::fsutil::parent(&paths.caddyfile), 0, caddy, 0o750));
        files.push((paths.caddyfile.clone(), 0, caddy, 0o640));
    }
    for (directory, owner, group, mode) in directories {
        let info =
            std::fs::symlink_metadata(&directory).map_err(|exc| os_error(exc, &directory))?;
        if !info.is_dir() || (info.uid(), info.gid(), info.mode() & 0o7777) != (owner, group, mode)
        {
            return refuse("unsafe installed directory ownership or mode");
        }
    }
    for (file, owner, group, mode) in &files {
        let (file, owner, group, mode) = (file.as_str(), *owner, *group, *mode);
        let info = std::fs::symlink_metadata(file).map_err(|exc| os_error(exc, file))?;
        if !info.file_type().is_file()
            || info.nlink() != 1
            || (info.uid(), info.gid(), info.mode() & 0o7777) != (owner, group, mode)
        {
            return refuse("unsafe installed file ownership or mode");
        }
    }
    let mut raw = Vec::new();
    std::io::stdin()
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|exc| Failure::Reported(exc.to_string()))?;
    if raw.len() > MAX_REQUEST {
        return refuse("request too large");
    }
    let request = jsonval::loads(&raw)
        .ok_or_else(|| Failure::Reported("Expecting value: the request is not JSON".into()))?;
    enroll(&request, &paths, OPENSSL)
}

const HELP: &str = "Usage: switchboard server enroll-local

A fallback over SSH for when no invite can be made. Run it as root on the
server, with no arguments: it reads one enrollment request on standard input,
{\"machine\": ..., \"owner\": ..., \"certificate_pem\": ...} (the certificate from
the new machine's ~/.config/switchboard/identity/cert.pem), checks the
certificate, adds the machine to the registry, and prints one JSON line. It
needs a listener that leaves authorization to the registry: the built-in TLS
modes, or Caddy requiring a client key with no trust pool. It restarts
nothing; the same request again changes nothing, and a revoked key stays
revoked.

Options:
  -h, --help         show this help
";

pub fn main(argv: &[String]) -> i32 {
    if let [flag] = argv {
        if flag == "-h" || flag == "--help" {
            crate::cli::print_help(HELP);
            return 0;
        }
    }
    match checked_main(argv) {
        Ok(result) => {
            println!("{}", jsonval::dumps(&Val::Dict(result), jsonval::DEFAULT));
            0
        }
        Err(Failure::Reported(message)) => {
            let value = Val::Dict(dict([("ok", false.into()), ("error", message.into())]));
            println!("{}", jsonval::dumps(&value, jsonval::DEFAULT));
            1
        }
        Err(Failure::Other(message)) => {
            eprintln!("{message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    struct Pems {
        _dir: TempDir,
        pems: std::collections::HashMap<&'static str, String>,
    }

    fn openssl() -> String {
        for candidate in [
            "/opt/homebrew/bin/openssl",
            "/usr/local/bin/openssl",
            "/usr/bin/openssl",
        ] {
            if std::path::Path::new(candidate).exists() {
                return candidate.into();
            }
        }
        "openssl".into()
    }

    fn pems() -> &'static std::collections::HashMap<&'static str, String> {
        static PEMS: OnceLock<Pems> = OnceLock::new();
        &PEMS
            .get_or_init(|| {
                let dir = TempDir::new().unwrap();
                let mut pems = std::collections::HashMap::new();
                let good = "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth";
                let wrong = "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth";
                let cases: [(&str, &str, &str, &str); 5] = [
                    ("valid", good, "ec", "CA:FALSE TLS Web Client Authentication Digital Signature"),
                    ("other", good, "ec", "CA:FALSE TLS Web Client Authentication Digital Signature"),
                    ("wrong_eku", wrong, "ec", "CA:FALSE TLS Web Client Authentication Digital Signature"),
                    ("forged_subject", "", "ec", "CA:FALSE TLS Web Client Authentication Digital Signature"),
                    ("weak_spoofed_subject", good, "rsa:1024", "Public Key Algorithm: rsaEncryption Public-Key: (2048 bit)"),
                ];
                for (name, extensions, key, cn) in cases {
                    let config = format!("{}/{name}.conf", dir.0);
                    std::fs::write(&config, format!("[req]\ndistinguished_name=dn\nx509_extensions=extensions\nprompt=no\n[dn]\nCN={cn}\n[extensions]\n{extensions}\n")).unwrap();
                    let mut command = Command::new(openssl());
                    command.args(["req", "-x509", "-newkey", key]);
                    if key == "ec" {
                        command.args(["-pkeyopt", "ec_paramgen_curve:prime256v1"]);
                    }
                    let out = format!("{}/{name}.pem", dir.0);
                    let status = command
                        .args(["-nodes", "-days", "1", "-keyout", &format!("{}/{name}.key", dir.0), "-out", &out, "-config", &config])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .unwrap();
                    assert!(status.success());
                    pems.insert(name, std::fs::read_to_string(out).unwrap());
                }
                Pems { _dir: dir, pems }
            })
            .pems
    }

    struct Case {
        dir: TempDir,
        paths: Paths,
    }

    fn original() -> String {
        format!(
            r#"{{"{}": {{"machine": "existing", "owner": "owner", "enabled": true, "view": false, "admin": true}}}}"#,
            "f".repeat(64)
        )
    }

    fn case() -> Case {
        let dir = TempDir::new().unwrap();
        let paths = Paths {
            registry: format!("{}/peers.json", dir.0),
            caddyfile: format!("{}/Caddyfile", dir.0),
            lock: format!("{}/peers.json.lock", dir.0),
            config: format!("{}/server.json", dir.0),
        };
        super::super::write_private(&paths.registry, original().as_bytes()).unwrap();
        std::fs::write(
            &paths.caddyfile,
            "client_auth {\n\t\t\tmode require\n\t\t}\n",
        )
        .unwrap();
        Case { dir, paths }
    }

    fn request(change: &[(&str, Val)]) -> Val {
        let mut map = dict([
            ("machine", "new-machine".into()),
            ("owner", "test".into()),
            ("certificate_pem", pems()["valid"].as_str().into()),
        ]);
        for (key, value) in change {
            jsonval::set(&mut map, key, value.clone());
        }
        Val::Dict(map)
    }

    fn registry(case: &Case) -> Val {
        jsonval::loads(&std::fs::read(&case.paths.registry).unwrap()).unwrap()
    }

    fn message(result: Result<Dict>) -> String {
        match result {
            Err(Failure::Reported(message)) => message,
            other => panic!("expected a reported failure, got {other:?}"),
        }
    }

    #[test]
    fn enroll_preserves_existing_and_rerun_is_noop() {
        let case = case();
        let result = enroll(&request(&[]), &case.paths, &openssl()).unwrap();
        assert_eq!(get(&result, "unchanged"), Some(&Val::Bool(false)));
        let fingerprint = get(&result, "fingerprint")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let value = registry(&case);
        let map = value.as_dict().unwrap();
        assert_eq!(
            map.get(&JsonStr::new(&"f".repeat(64))),
            jsonval::loads(original().as_bytes())
                .unwrap()
                .as_dict()
                .unwrap()
                .values()
                .next()
        );
        let entry = get(map, &fingerprint).unwrap().as_dict().unwrap();
        let keys: Vec<&str> = entry.keys().map(|key| key.as_str().unwrap()).collect();
        assert_eq!(keys, ["enabled", "machine", "owner"]);
        assert!(get(entry, "machine").unwrap().is_str("new-machine"));
        let before = std::fs::read(&case.paths.registry).unwrap();
        let again = enroll(&request(&[]), &case.paths, &openssl()).unwrap();
        assert_eq!(get(&again, "unchanged"), Some(&Val::Bool(true)));
        assert_eq!(std::fs::read(&case.paths.registry).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&case.paths.registry).unwrap().mode() & 0o777,
            0o600
        );
        let _ = &case.dir;
    }

    #[test]
    fn rerun_tolerates_fields_the_server_added_and_revocation_fails_closed() {
        let case = case();
        let result = enroll(&request(&[]), &case.paths, &openssl()).unwrap();
        let fingerprint = get(&result, "fingerprint")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let mut value = registry(&case);
        if let Val::Dict(map) = &mut value {
            if let Some(Val::Dict(entry)) = map.get_mut(&JsonStr::new(&fingerprint)) {
                jsonval::set(entry, "admin", false);
                jsonval::set(entry, "approved_by", "existing");
            }
        }
        std::fs::write(
            &case.paths.registry,
            jsonval::dumps(&value, jsonval::DEFAULT),
        )
        .unwrap();
        let again = enroll(&request(&[]), &case.paths, &openssl()).unwrap();
        assert_eq!(get(&again, "unchanged"), Some(&Val::Bool(true)));
        // The legacy viewer flag an older client sends is ignored.
        for view in [Val::from(true), Val::from(false), Val::Int("1".into())] {
            let legacy = enroll(&request(&[("view", view)]), &case.paths, &openssl()).unwrap();
            assert_eq!(get(&legacy, "unchanged"), Some(&Val::Bool(true)));
        }
        for change in [
            ("owner", "changed".into()),
            ("certificate_pem", pems()["other"].as_str().into()),
        ] {
            assert!(enroll(&request(&[change]), &case.paths, &openssl()).is_err());
        }
        if let Val::Dict(map) = &mut value {
            if let Some(Val::Dict(entry)) = map.get_mut(&JsonStr::new(&fingerprint)) {
                jsonval::set(entry, "enabled", false);
            }
        }
        std::fs::write(
            &case.paths.registry,
            jsonval::dumps(&value, jsonval::DEFAULT),
        )
        .unwrap();
        assert!(message(enroll(&request(&[]), &case.paths, &openssl())).contains("revoked"));
        // A revoked machine's name can go to a new key.
        let replacement = enroll(
            &request(&[("certificate_pem", pems()["other"].as_str().into())]),
            &case.paths,
            &openssl(),
        )
        .unwrap();
        assert_eq!(get(&replacement, "unchanged"), Some(&Val::Bool(false)));
    }

    #[test]
    fn requires_pinned_key_ingress() {
        let case = case();
        for text in [
            "mode require_and_verify\ntrust_pool file /x\n",
            "mode require\ntrust_pool file /x\n",
        ] {
            std::fs::write(&case.paths.caddyfile, text).unwrap();
            assert!(
                message(enroll(&request(&[]), &case.paths, &openssl())).contains("pin client keys")
            );
        }
        assert_eq!(
            registry(&case),
            jsonval::loads(original().as_bytes()).unwrap()
        );
    }

    #[test]
    fn built_in_tls_needs_no_caddy() {
        let case = case();
        std::fs::remove_file(&case.paths.caddyfile).unwrap();
        // Without a server config the Caddy ingress is still required.
        assert!(message(enroll(&request(&[]), &case.paths, &openssl())).contains("Caddyfile"));
        for mode in ["acme", "private"] {
            let config = format!(
                r#"{{"tls": "{mode}", "db": "messages.sqlite3", "peers_file": "peers.json"}}"#
            );
            super::super::write_private(&case.paths.config, config.as_bytes()).unwrap();
            let result = enroll(&request(&[]), &case.paths, &openssl()).unwrap();
            assert_eq!(get(&result, "ok"), Some(&Val::Bool(true)), "{mode}");
        }
        // A Unix-socket server still needs Caddy pinning keys; direct mode
        // (a trusted client CA) and unknown modes are refused.
        for config in [
            r#"{"unix_socket": "/run/switchboard/hub.sock"}"#,
            r#"{"cert": "c", "key": "k", "client_ca": "ca"}"#,
            r#"{"tls": "other"}"#,
            r#"{"tls": "private", "unix_socket": "/run/x.sock"}"#,
        ] {
            super::super::write_private(&case.paths.config, config.as_bytes()).unwrap();
            let refused = message(enroll(
                &request(&[("machine", "other-one".into())]),
                &case.paths,
                &openssl(),
            ));
            assert!(
                refused.contains("Caddyfile") || refused.contains("authorize by the registry"),
                "{config}: {refused}"
            );
        }
    }

    #[test]
    fn certificates_and_metadata_fail_closed() {
        let case = case();
        let both = format!("{}{}", pems()["valid"], pems()["other"]);
        for pem in [
            pems()["wrong_eku"].as_str(),
            pems()["forged_subject"].as_str(),
            both.as_str(),
            "-----BEGIN PRIVATE KEY-----",
        ] {
            assert!(enroll(
                &request(&[("certificate_pem", pem.into())]),
                &case.paths,
                &openssl()
            )
            .is_err());
        }
        for change in [
            ("machine", Val::from("../bad")),
            ("owner", "bad\nowner".into()),
            ("extra", Val::from(true)),
        ] {
            assert!(enroll(&request(&[change]), &case.paths, &openssl()).is_err());
        }
        assert_eq!(
            message(enroll(
                &request(&[(
                    "certificate_pem",
                    pems()["weak_spoofed_subject"].as_str().into()
                )]),
                &case.paths,
                &openssl()
            )),
            "unsupported or weak public key"
        );
        assert_eq!(
            registry(&case),
            jsonval::loads(original().as_bytes()).unwrap()
        );
    }

    #[test]
    fn unsafe_links_and_lock_are_rejected() {
        let case = case();
        let original_path = format!("{}/original.json", case.dir.0);
        std::fs::rename(&case.paths.registry, &original_path).unwrap();
        std::os::unix::fs::symlink(&original_path, &case.paths.registry).unwrap();
        assert!(message(enroll(&request(&[]), &case.paths, &openssl())).starts_with("[Errno"));
        std::fs::remove_file(&case.paths.registry).unwrap();
        std::fs::hard_link(&original_path, &case.paths.registry).unwrap();
        assert!(message(enroll(&request(&[]), &case.paths, &openssl())).contains("authoritative"));
        std::fs::remove_file(&case.paths.registry).unwrap();
        std::fs::rename(&original_path, &case.paths.registry).unwrap();
        let _ = std::fs::remove_file(&case.paths.lock);
        std::os::unix::fs::symlink(&case.paths.registry, &case.paths.lock).unwrap();
        assert!(message(enroll(&request(&[]), &case.paths, &openssl())).starts_with("[Errno"));
        std::fs::remove_file(&case.paths.lock).unwrap();
        std::fs::write(&case.paths.lock, "").unwrap();
        std::fs::set_permissions(
            &case.paths.lock,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
        assert!(message(enroll(&request(&[]), &case.paths, &openssl())).contains("lock"));
    }

    #[test]
    fn concurrent_merges_are_serialized() {
        let case = std::sync::Arc::new(case());
        let threads: Vec<_> = [
            request(&[]),
            request(&[
                ("machine", "second-machine".into()),
                ("certificate_pem", pems()["other"].as_str().into()),
            ]),
        ]
        .into_iter()
        .map(|req| {
            let case = case.clone();
            std::thread::spawn(move || enroll(&req, &case.paths, &openssl()).is_ok())
        })
        .collect();
        assert!(threads.into_iter().all(|t| t.join().unwrap()));
        assert_eq!(registry(&case).as_dict().unwrap().len(), 3);
    }

    #[test]
    fn server_and_helper_share_the_registry_lock() {
        let case = case();
        let db = format!("{}/messages.db", case.dir.0);
        let hub = std::sync::Arc::new(
            super::super::hub::Hub::open(super::super::hub::Config {
                db,
                peers_file: case.paths.registry.clone(),
                registry_lock: None,
            })
            .unwrap(),
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let writer = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                hub.update_registry_for_test(move |registry| {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(5));
                    registry.insert(
                        JsonStr::new(&"a".repeat(64)),
                        Val::Dict(dict([
                            ("machine", "from-hub".into()),
                            ("owner", "hub".into()),
                            ("enabled", true.into()),
                        ])),
                    );
                })
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let paths = Paths {
            ..case_paths(&case)
        };
        let helper = std::thread::spawn(move || enroll(&request(&[]), &paths, &openssl()).ok());
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !helper.is_finished(),
            "the helper must wait for the server to release the lock"
        );
        release_tx.send(()).unwrap();
        assert!(writer.join().unwrap().is_ok());
        let done = helper.join().unwrap().unwrap();
        let value = registry(&case);
        let map = value.as_dict().unwrap();
        assert!(get(map, &"a".repeat(64)).is_some());
        assert!(get(map, get(&done, "fingerprint").unwrap().as_str().unwrap()).is_some());
        assert_eq!(
            std::fs::metadata(&case.paths.registry).unwrap().mode() & 0o777,
            0o600
        );
        let _ = PathBuf::new();
    }

    fn case_paths(case: &Case) -> Paths {
        Paths {
            registry: case.paths.registry.clone(),
            caddyfile: case.paths.caddyfile.clone(),
            lock: case.paths.lock.clone(),
            config: case.paths.config.clone(),
        }
    }

    #[test]
    fn pem_and_extension_patterns() {
        assert!(one_certificate(
            " \n-----BEGIN CERTIFICATE-----\r\nAB+/=\n-----END CERTIFICATE-----\n\u{2003}"
        ));
        assert!(!one_certificate(
            "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----"
        ));
        assert!(!one_certificate(
            "-----BEGIN CERTIFICATE-----\nAB\n-----END CERTIFICATE-----x"
        ));
        let listing = "        X509v3 Basic Constraints: critical\n            CA:FALSE\n        X509v3 Key Usage: \n            Digital Signature, Key Encipherment\n";
        assert_eq!(extension(listing, "Basic Constraints"), "CA:FALSE");
        assert_eq!(
            extension(listing, "Key Usage"),
            "Digital Signature, Key Encipherment"
        );
        assert_eq!(extension(listing, "Extended Key Usage"), "");
    }
}
