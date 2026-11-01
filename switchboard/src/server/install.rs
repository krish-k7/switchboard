//! `switchboard server install`: install this binary as the Switchboard
//! server on a Linux machine with systemd, run as root.
//!
//! It creates the `switchboard` service account and its directories, copies
//! the running binary to `/usr/local/lib/switchboard/switchboard`, writes the
//! server config and the systemd unit for the chosen listener, and starts the
//! service. While no machine has joined it prints an invite for the first
//! one (single use, for an hour): `switchboard init` on that machine asks
//! for it and joins. `--no-invite` skips it, for setups that make their own.
//!
//! The command is linked from `/usr/local/bin/switchboard`. Some
//! distributions' sudo resets PATH to a `secure_path` without
//! `/usr/local/bin`, where `sudo switchboard` isn't found; the installer
//! asks sudo, notes the answer in the program's marker, and the server's
//! hints then name the full path (`sudo_switchboard`).
//!
//! Listener modes:
//! - `--tls acme --domain NAME`: public certificates through ACME
//!   (TLS-ALPN-01 on port 443), with `CAP_NET_BIND_SERVICE`.
//! - `--tls private`: a self-signed certificate made now and kept; clients
//!   check its SHA-256, which every invite carries.
//! - `--tls caddy`: a private Unix socket behind a Caddy ingress that pins
//!   client keys (REFERENCE.md gives its configuration); Caddy itself is set
//!   up separately.
//!
//! Like the other installers it is idempotent, never replaces a file it did
//! not write (each managed file carries a marker, or sits beside one), and
//! refuses to run beside an older server's service (`switchboard-hub.service`).

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::jsonval::{self, Val};

pub const CONFIG: &str = "/etc/switchboard/server.json";
pub(crate) const CONFIG_DIR: &str = "/etc/switchboard";
pub(crate) const CONFIG_MARKER: &str = "/etc/switchboard/.server-install";
pub(crate) const STATE: &str = "/var/lib/switchboard";
pub(crate) const DB: &str = "/var/lib/switchboard/messages.sqlite3";
pub(crate) const REGISTRY: &str = "/var/lib/switchboard/peers.json";
pub(crate) const TLS_DIR: &str = "/var/lib/switchboard/tls";
pub(crate) const PROGRAM_DIR: &str = "/usr/local/lib/switchboard";
pub(crate) const PROGRAM: &str = "/usr/local/lib/switchboard/switchboard";
pub(crate) const PROGRAM_MARKER: &str = "/usr/local/lib/switchboard/.server-install";
pub(crate) const LINK: &str = "/usr/local/bin/switchboard";
pub(crate) const UNIT_NAME: &str = "switchboard-server.service";
pub(crate) const UNIT: &str = "/etc/systemd/system/switchboard-server.service";
pub(crate) const SOCKET: &str = "/run/switchboard/hub.sock";
pub(crate) const ACCOUNT: &str = "switchboard";
pub(crate) const MARKER: &str = "# Managed by switchboard server install";
pub(crate) const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

const USAGE: &str = "Usage: switchboard server install --tls acme|private|caddy [options]
";

const HELP: &str = "
Install this binary as the Switchboard server (Linux with systemd; run as root).
Running it again is safe: it changes only what differs.

Options:
  --tls MODE        acme: public certificates from Let's Encrypt for --domain
                    (port 443); private: a self-signed certificate that clients
                    pin (port 8743); caddy: a private Unix socket behind a Caddy
                    ingress you set up separately
  --domain NAME     acme: the public DNS name; caddy: the name clients use (only
                    for the printed invite)
  --email ADDRESS   acme: the contact for the certificate account (optional)
  --name NAME       private: a host name or address clients connect to; repeat
                    for more (default: this machine's host name and address)
  --port PORT       acme and private: the port to listen on
  --no-start        write everything, but do not start or restart the service
  --no-invite       don't make the first machine's invite (for setups that make
                    their own with switchboard server invite)
  --json            print the result as JSON
  -h, --help        show this help
";

/// The listener a server installation uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Acme,
    Private,
    Caddy,
}

/// The parsed command line.
#[derive(Debug, Default)]
pub struct Options {
    pub mode: Option<Mode>,
    pub domain: Option<String>,
    pub email: Option<String>,
    pub names: Vec<String>,
    pub port: Option<u16>,
    pub no_start: bool,
    pub no_invite: bool,
    pub json: bool,
}

fn usage_error(message: &str) -> String {
    format!("switchboard server install: {message}\n{USAGE}")
}

/// Parse the arguments; `Ok(None)` for `--help`.
pub fn parse(argv: &[String]) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < argv.len() {
        let argument = argv[index].as_str();
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (argument, None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            index += 1;
            match argv.get(index) {
                Some(value) if !value.starts_with("--") => Ok(value.clone()),
                _ => Err(usage_error(&format!("{name} needs a value"))),
            }
        };
        match name {
            "-h" | "--help" => return Ok(None),
            "--tls" => {
                options.mode = Some(match value()?.as_str() {
                    "acme" => Mode::Acme,
                    "private" => Mode::Private,
                    "caddy" => Mode::Caddy,
                    other => {
                        return Err(usage_error(&format!(
                            "--tls must be acme, private, or caddy, not '{other}'"
                        )))
                    }
                })
            }
            "--domain" => options.domain = Some(value()?.to_ascii_lowercase()),
            "--email" => options.email = Some(value()?),
            "--name" => options.names.push(value()?),
            "--port" => {
                let text = value()?;
                options.port = Some(
                    text.parse::<u16>()
                        .ok()
                        .filter(|port| *port != 0)
                        .ok_or_else(|| {
                            usage_error(&format!("--port must be a port number, not '{text}'"))
                        })?,
                )
            }
            "--no-start" => options.no_start = true,
            "--no-invite" => options.no_invite = true,
            "--json" => options.json = true,
            _ if argument.starts_with('-') => {
                return Err(usage_error(&format!("unknown option {name}")))
            }
            _ => return Err(usage_error(&format!("unexpected argument {argument}"))),
        }
        index += 1;
    }
    let Some(mode) = options.mode else {
        return Err(usage_error("--tls is required"));
    };
    if mode == Mode::Acme && options.domain.is_none() {
        return Err(usage_error("--tls acme requires --domain"));
    }
    if let Some(domain) = &options.domain {
        if !is_dns_name(domain) || !domain.contains('.') {
            return Err(usage_error(&format!("invalid DNS name: {domain}")));
        }
    }
    if mode != Mode::Acme && options.email.is_some() {
        return Err(usage_error("--email only works with --tls acme"));
    }
    if mode != Mode::Private && !options.names.is_empty() {
        return Err(usage_error("--name only works with --tls private"));
    }
    if mode == Mode::Caddy && options.port.is_some() {
        return Err(usage_error(
            "--port does not apply to --tls caddy (Caddy listens)",
        ));
    }
    for name in &options.names {
        if !is_dns_name(name) && name.parse::<std::net::IpAddr>().is_err() {
            return Err(usage_error(&format!(
                "invalid host name or address: {name}"
            )));
        }
    }
    Ok(Some(options))
}

/// A public DNS name, as `install_hub.sh` accepts it: lowercase labels of
/// letters, digits, and inner hyphens, at least two, a final label of 2-63
/// letters, at most 253 characters. Single-label names are accepted too for
/// private networks (`--name`), so only the label rules are checked here when
/// there is one label.
pub(crate) fn is_dns_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |label: &str| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && bytes[0] != b'-'
            && bytes[bytes.len() - 1] != b'-'
    };
    if !labels.iter().all(|label| label_ok(label)) {
        return false;
    }
    let last = labels[labels.len() - 1];
    labels.len() == 1 || (last.len() >= 2 && last.bytes().all(|b| b.is_ascii_lowercase()))
}

/// The server config for a mode (JSON with a two-space indent, keys in the order written).
pub fn render_config(options: &Options, names: &[String]) -> String {
    let mut map = jsonval::Dict::new();
    let mode = options.mode.expect("parsed");
    match mode {
        Mode::Acme => {
            jsonval::set(&mut map, "tls", "acme");
            jsonval::set(
                &mut map,
                "hostname",
                options.domain.clone().unwrap_or_default().as_str(),
            );
            if let Some(email) = &options.email {
                jsonval::set(&mut map, "acme_email", email.as_str());
            }
        }
        Mode::Private => {
            jsonval::set(&mut map, "tls", "private");
            jsonval::set(
                &mut map,
                "names",
                Val::List(names.iter().map(|n| Val::from(n.clone())).collect()),
            );
        }
        Mode::Caddy => jsonval::set(&mut map, "unix_socket", SOCKET),
    }
    if let Some(port) = options.port {
        jsonval::set(&mut map, "port", Val::Int(port.to_string()));
    }
    jsonval::set(&mut map, "db", DB);
    jsonval::set(&mut map, "peers_file", REGISTRY);
    let mut text = jsonval::dumps(
        &Val::Dict(map),
        jsonval::Style {
            sort_keys: false,
            compact: false,
            indent: Some(2),
        },
    );
    text.push('\n');
    text
}

/// The listening port a mode uses.
pub(crate) fn port(options: &Options) -> u16 {
    options.port.unwrap_or(match options.mode {
        Some(Mode::Acme) => 443,
        _ => 8743,
    })
}

/// The systemd unit: `install_hub.sh`'s hardening for the server, with the
/// network and capabilities the listener needs.
pub fn render_unit(options: &Options) -> String {
    let mode = options.mode.expect("parsed");
    let caddy = mode == Mode::Caddy;
    let privileged = !caddy && port(options) < 1024;
    let capability = if privileged {
        "CAP_NET_BIND_SERVICE"
    } else {
        ""
    };
    let mut unit = format!(
        "{MARKER}
[Unit]
Description=Switchboard server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={ACCOUNT}
Group={group}
# Lets the server keep the registry's switchboard group when it rewrites peers.json.
SupplementaryGroups={ACCOUNT}
",
        group = if caddy { "caddy" } else { ACCOUNT },
    );
    if caddy {
        unit.push_str("RuntimeDirectory=switchboard\nRuntimeDirectoryMode=0750\n");
    }
    unit.push_str(&format!(
        "WorkingDirectory={STATE}
ExecStart={PROGRAM} server --config {CONFIG}
Restart=on-failure
RestartSec=3
UMask=0077
NoNewPrivileges=true
CapabilityBoundingSet={capability}
AmbientCapabilities={capability}
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
RestrictNamespaces=true
LockPersonality=true
RestrictAddressFamilies={families}
ReadWritePaths={paths}
MemoryMax=256M
TasksMax=180
LimitNOFILE=512
TimeoutStopSec=15

[Install]
WantedBy=multi-user.target
",
        families = if caddy {
            "AF_UNIX"
        } else {
            "AF_INET AF_INET6 AF_UNIX"
        },
        paths = if caddy {
            format!("{STATE} /run/switchboard")
        } else {
            STATE.to_string()
        },
    ));
    unit
}

// System access --------------------------------------------------------------

pub(crate) struct Account {
    pub uid: u32,
    pub gid: u32,
    pub home: String,
}

pub(crate) fn user(name: &str) -> Option<Account> {
    let cname = CString::new(name).ok()?;
    // SAFETY: getpwnam returns static storage or null; fields are read at once.
    unsafe {
        let entry = libc::getpwnam(cname.as_ptr());
        if entry.is_null() {
            return None;
        }
        Some(Account {
            uid: (*entry).pw_uid,
            gid: (*entry).pw_gid,
            home: std::ffi::CStr::from_ptr((*entry).pw_dir)
                .to_string_lossy()
                .into_owned(),
        })
    }
}

pub(crate) fn group(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    // SAFETY: getgrnam returns static storage or null; read at once.
    unsafe {
        let entry = libc::getgrnam(cname.as_ptr());
        (!entry.is_null()).then(|| (*entry).gr_gid)
    }
}

/// Run a system command with a fixed PATH; its exit status and output.
pub(crate) fn run(program: &str, arguments: &[&str]) -> Result<(i32, String), String> {
    let output = Command::new(program)
        .args(arguments)
        .env("PATH", PATH)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|exc| format!("{program}: {exc}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.code().unwrap_or(-1), text.trim().to_string()))
}

pub(crate) fn checked(program: &str, arguments: &[&str]) -> Result<(), String> {
    let (code, text) = run(program, arguments)?;
    if code != 0 {
        return Err(format!(
            "{program} {} failed ({code}): {text}",
            arguments.join(" ")
        ));
    }
    Ok(())
}

fn systemctl_quiet(arguments: &[&str]) -> bool {
    run("systemctl", arguments).is_ok_and(|(code, _)| code == 0)
}

fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

pub(crate) fn c_path(path: &str) -> Result<CString, String> {
    CString::new(path).map_err(|_| format!("{path}: embedded null byte"))
}

/// Open the directory `path` without following a link in its last component.
pub(crate) fn open_directory(path: &str) -> Result<OwnedFd, String> {
    let c = c_path(path)?;
    // SAFETY: open with a valid path; the descriptor is owned below.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => format!("refusing non-directory: {path}"),
            _ => format!("{path}: {error}"),
        });
    }
    // SAFETY: fd is a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A directory owned `uid:gid` with `mode`, created if missing; never a link.
/// The owner and mode are set through a descriptor opened without following
/// links, so a link put in its place after a check cannot redirect them; the
/// descriptor is returned for work inside the directory.
fn directory(path: &str, uid: u32, gid: u32, mode: u32) -> Result<OwnedFd, String> {
    if let Err(exc) = std::fs::create_dir(path) {
        if exc.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("{path}: {exc}"));
        }
    }
    let fd = open_directory(path)?;
    // SAFETY: fd is an open descriptor; the calls take plain integers.
    unsafe {
        if libc::fchown(fd.as_raw_fd(), uid, gid) != 0
            || libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) != 0
        {
            return Err(format!("{path}: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(fd)
}

/// An existing regular file with one link, opened without following a link
/// (and without blocking on a FIFO), with its contents; `None` when missing.
/// `dir` makes `name` relative to that directory's descriptor; `label` is
/// the path for messages.
fn read_file(
    dir: Option<&OwnedFd>,
    name: &str,
    label: &str,
) -> Result<Option<(std::fs::File, Vec<u8>)>, String> {
    let c = c_path(name)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: open/openat with a valid path; the descriptor is owned below.
    let fd = unsafe {
        match dir {
            Some(dir) => libc::openat(dir.as_raw_fd(), c.as_ptr(), flags),
            None => libc::open(c.as_ptr(), flags),
        }
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            Some(libc::ELOOP) => Err(format!("refusing unsafe file: {label}")),
            _ => Err(format!("{label}: {error}")),
        };
    }
    // SAFETY: fd is a new descriptor that nothing else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let info = file.metadata().map_err(|exc| format!("{label}: {exc}"))?;
    if !info.file_type().is_file() || info.nlink() != 1 {
        return Err(format!("refusing unsafe file: {label}"));
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|exc| format!("{label}: {exc}"))?;
    Ok(Some((file, data)))
}

/// Replace `path` atomically with `data`, owned `uid:gid` with `mode`;
/// `false` when it already had exactly that content, owner, and mode.
fn put(path: &str, data: &[u8], uid: u32, gid: u32, mode: u32) -> Result<bool, String> {
    if let Some((file, current)) = read_file(None, path, path)? {
        let info = file.metadata().map_err(|exc| format!("{path}: {exc}"))?;
        if (info.uid(), info.gid(), info.mode() & 0o7777) == (uid, gid, mode) && current == data {
            return Ok(false);
        }
    }
    let directory = crate::fsutil::parent(path);
    let template = CString::new(format!("{directory}/.install-XXXXXX"))
        .map_err(|_| "embedded null byte".to_string())?;
    let mut raw = template.into_bytes_with_nul();
    // SAFETY: raw is a writable NUL-terminated template.
    let fd = unsafe { libc::mkstemp(raw.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        return Err(format!("{directory}: {}", std::io::Error::last_os_error()));
    }
    raw.pop();
    let temporary = String::from_utf8_lossy(&raw).into_owned();
    // SAFETY: fd is a new descriptor owned by this file.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let result = (|| -> Result<(), String> {
        let fail = |exc: std::io::Error| format!("{temporary}: {exc}");
        // SAFETY: file is open; the calls take plain integers.
        unsafe {
            if libc::fchown(file.as_raw_fd(), uid, gid) != 0
                || libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) != 0
            {
                return Err(fail(std::io::Error::last_os_error()));
            }
        }
        file.write_all(data).map_err(fail)?;
        file.sync_all().map_err(fail)?;
        std::fs::rename(&temporary, path).map_err(fail)
    })();
    if exists(&temporary) {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map(|_| true)
}

/// Write `name` in the directory `dir` atomically (a new file renamed into
/// place, never through a link), owned `uid:gid` with mode 0600.
fn write_at(
    dir: &OwnedFd,
    label: &str,
    name: &str,
    data: &[u8],
    uid: u32,
    gid: u32,
) -> Result<(), String> {
    let temporary = c_path(&format!(".{name}.{}", crate::identity::uuid4_hex()))?;
    let target = c_path(name)?;
    let fail = |error: std::io::Error| format!("{label}: {error}");
    // SAFETY: openat with a valid name; the descriptor is owned below.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(fail(std::io::Error::last_os_error()));
    }
    // SAFETY: fd is a new descriptor that nothing else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let result = (|| -> Result<(), String> {
        // SAFETY: file is open; the calls take plain integers.
        unsafe {
            if libc::fchown(file.as_raw_fd(), uid, gid) != 0
                || libc::fchmod(file.as_raw_fd(), 0o600) != 0
            {
                return Err(fail(std::io::Error::last_os_error()));
            }
        }
        file.write_all(data).map_err(fail)?;
        file.sync_all().map_err(fail)?;
        // SAFETY: renameat within one directory descriptor, valid names.
        if unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                temporary.as_ptr(),
                dir.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(fail(std::io::Error::last_os_error()));
        }
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: unlinkat of the temporary name in the same directory.
        unsafe { libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    result
}

/// The private certificate, kept in `TLS_DIR` (`dir`, a descriptor for it).
/// It is issued for `names` when there is none, or when `--name` asked for
/// names it was not made for: names that differ from the ones the previous
/// config recorded, or that it is not valid for. A rerun without `--name`
/// keeps it, so clients' pins keep working. Every file is reached through
/// the directory's descriptor without following links and is owned by the
/// service account: the account owns the directory, and must not be able to
/// point root at another file. Its SHA-256, and whether it was issued now.
fn private_certificate(
    dir: &OwnedFd,
    names: &[String],
    explicit: bool,
    previous: Option<&[String]>,
    uid: u32,
    gid: u32,
) -> Result<(String, bool), String> {
    use rustls::pki_types::pem::PemObject;
    let cert_path = format!("{TLS_DIR}/private-cert.pem");
    let key_path = format!("{TLS_DIR}/private-key.pem");
    let first = |pem: &[u8]| -> Result<Vec<u8>, String> {
        rustls::pki_types::CertificateDer::pem_slice_iter(pem)
            .next()
            .and_then(|item| item.ok())
            .map(|der| der.as_ref().to_vec())
            .ok_or_else(|| format!("{cert_path}: no certificate"))
    };
    let issue = match read_file(Some(dir), "private-cert.pem", &cert_path)? {
        None => true,
        Some((_, pem)) => {
            let set = |list: &[String]| {
                list.iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
            };
            explicit
                && (previous.map(set) != Some(set(names))
                    || !super::tls::covers(&first(&pem)?, names))
        }
    };
    if issue {
        let (key, cert) = super::tls::issue_private(names)?;
        // The key first: a certificate without its key is never left behind.
        write_at(dir, &key_path, "private-key.pem", key.as_bytes(), uid, gid)?;
        write_at(
            dir,
            &cert_path,
            "private-cert.pem",
            cert.as_bytes(),
            uid,
            gid,
        )?;
    }
    let mut contents = Vec::new();
    for (name, path) in [
        ("private-cert.pem", &cert_path),
        ("private-key.pem", &key_path),
    ] {
        let (file, data) = read_file(Some(dir), name, path)?
            .ok_or_else(|| format!("{path}: No such file or directory"))?;
        // SAFETY: file is open; the calls take plain integers.
        unsafe {
            if libc::fchown(file.as_raw_fd(), uid, gid) != 0
                || libc::fchmod(file.as_raw_fd(), 0o600) != 0
            {
                return Err(format!("{path}: {}", std::io::Error::last_os_error()));
            }
        }
        contents.push(data);
    }
    rustls::pki_types::PrivateKeyDer::from_pem_slice(&contents[1])
        .map_err(|exc| format!("{key_path}: {exc}"))?;
    Ok((hex(&Sha256::digest(first(&contents[0])?)), issue))
}

/// The names the managed config recorded for `--tls private`, if any.
fn previous_names() -> Option<Vec<String>> {
    if !exists(CONFIG_MARKER) {
        return None;
    }
    let (_, data) = read_file(None, CONFIG, CONFIG).ok()??;
    let Val::Dict(config) = jsonval::loads(&data)? else {
        return None;
    };
    if jsonval::get(&config, "tls").and_then(Val::as_str) != Some("private") {
        return None;
    }
    match jsonval::get(&config, "names")? {
        Val::List(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => None,
    }
}

/// Whether `path` is a directory (not a link) with nothing in it.
pub(crate) fn empty_directory(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|info| info.is_dir())
        && std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// Whether the registry has an enabled machine.
fn has_machines() -> Result<bool, String> {
    let data = std::fs::read(REGISTRY).map_err(|exc| format!("{REGISTRY}: {exc}"))?;
    let registry = jsonval::loads(&data).ok_or_else(|| format!("{REGISTRY}: invalid JSON"))?;
    let peers = super::hub::parse_registry(&registry)
        .ok_or_else(|| format!("{REGISTRY}: invalid registry"))?;
    Ok(!peers.is_empty())
}

/// This machine's primary IPv4 address: the source address of a route to a
/// documentation address (no packet is sent).
pub(crate) fn primary_address() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_unspecified() && !address.is_loopback()).then(|| address.to_string())
}

pub(crate) fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most buffer.len() bytes.
    unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
    is_dns_name(&name).then_some(name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The line in the program's marker that says sudo doesn't find
/// `switchboard` by name.
const SUDO_FULL_PATH: &str = "sudo-needs-full-path";

/// The program marker's text.
fn program_marker(full_path: bool) -> String {
    if full_path {
        format!("1\n{SUDO_FULL_PATH}\n")
    } else {
        "1\n".into()
    }
}

/// What `sudo -n sh -c 'command -v switchboard || echo NOT-FOUND'` said:
/// whether hints need the full path. Found at the link: no. Not found, or
/// found elsewhere (another program of that name comes first): yes. A sudo
/// that refused (one that needs a terminal, say) can't tell, and the full
/// path works whatever its PATH is.
fn probe_needs_full_path(succeeded: bool, output: &str) -> bool {
    let last = output.lines().last().unwrap_or("").trim();
    !(succeeded && last == LINK)
}

/// Whether `sudo switchboard` misses this program: sudo resets PATH to its
/// `secure_path` when one is set (some distributions leave
/// `/usr/local/bin` out), else keeps the caller's, so it is asked with the
/// PATH a person's shell has. Root needs no password for it. Without sudo
/// there is nothing to say.
fn sudo_needs_full_path() -> bool {
    let Some(sudo) = ["/usr/bin/sudo", "/bin/sudo", "/usr/local/bin/sudo"]
        .into_iter()
        .find(|path| exists(path))
    else {
        return false;
    };
    let Ok(output) = Command::new(sudo)
        .args([
            "-n",
            "/bin/sh",
            "-c",
            "command -v switchboard || echo switchboard-not-found",
        ])
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    probe_needs_full_path(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
    )
}

/// How a person runs the server's commands with sudo on this machine:
/// `sudo switchboard`, or `sudo /usr/local/bin/switchboard` where the
/// installer found sudo doesn't find it by name. Every hint about the
/// server that this machine prints uses it.
pub fn sudo_switchboard() -> &'static str {
    static FOUND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FOUND.get_or_init(|| sudo_command(std::fs::read_to_string(PROGRAM_MARKER).ok().as_deref()))
}

/// `sudo_switchboard`, to write as `{SUDO}` in a format string.
pub struct Sudo;

impl std::fmt::Display for Sudo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(sudo_switchboard())
    }
}

pub const SUDO: Sudo = Sudo;

/// `sudo_switchboard` for a marker's text.
fn sudo_command(marker: Option<&str>) -> &'static str {
    if marker.is_some_and(|text| text.lines().any(|line| line == SUDO_FULL_PATH)) {
        "sudo /usr/local/bin/switchboard"
    } else {
        "sudo switchboard"
    }
}

/// What the installation did, for the summary.
pub(crate) struct Outcome {
    pub changed: Vec<String>,
    pub hub_url: String,
    pub pin: Option<String>,
    /// The private certificate was replaced (new names), so its pin changed.
    pub reissued: bool,
    /// The first machine's invite, while none is enrolled.
    pub invite: Option<super::invites::Created>,
    pub started: bool,
}

pub(crate) fn install(options: &Options) -> Result<Outcome, String> {
    let mode = options.mode.expect("parsed");
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err("run as root".into());
    }
    if !std::path::Path::new("/run/systemd/system").is_dir() {
        return Err("this installer needs systemd".into());
    }
    // Refuse before changing anything.
    if exists(UNIT) && !std::fs::read_to_string(UNIT).is_ok_and(|text| text.starts_with(MARKER)) {
        return Err(format!("refusing unmanaged unit: {UNIT}"));
    }
    if exists(CONFIG) && !exists(CONFIG_MARKER) {
        return Err(format!("refusing unmanaged config: {CONFIG}"));
    }
    if exists(CONFIG_DIR) && !exists(CONFIG_MARKER) && !empty_directory(CONFIG_DIR) {
        return Err(format!("refusing unmanaged directory: {CONFIG_DIR}"));
    }
    if exists(PROGRAM_DIR) && !exists(PROGRAM_MARKER) {
        return Err(format!("refusing unmanaged directory: {PROGRAM_DIR}"));
    }
    // An older server's service; both can't hold the port and state.
    let old_service = "switchboard-hub.service";
    if systemctl_quiet(&["is-active", "--quiet", old_service])
        || systemctl_quiet(&["is-enabled", "--quiet", old_service])
    {
        return Err(format!(
            "refusing to install beside the enabled or active {old_service} (an older Switchboard server); stop and disable it first"
        ));
    }
    let caddy_gid = if mode == Mode::Caddy {
        Some(group("caddy").ok_or("--tls caddy needs the caddy group; install Caddy first")?)
    } else {
        None
    };
    match user(ACCOUNT) {
        Some(account) if account.home != STATE => {
            return Err(format!(
                "the existing {ACCOUNT} account has home {}, not {STATE}",
                account.home
            ))
        }
        Some(_) => {}
        None => {
            if group(ACCOUNT).is_some() {
                return Err(format!(
                    "the {ACCOUNT} group exists without the {ACCOUNT} account"
                ));
            }
            checked(
                "useradd",
                &[
                    "--system",
                    "--user-group",
                    "--home-dir",
                    STATE,
                    "--shell",
                    "/usr/sbin/nologin",
                    ACCOUNT,
                ],
            )?;
        }
    }
    let account = user(ACCOUNT).ok_or("the service account was not created")?;
    let (uid, gid) = (account.uid, account.gid);
    if group(ACCOUNT) != Some(gid) {
        return Err(format!(
            "the {ACCOUNT} account's primary group is not {ACCOUNT}"
        ));
    }
    let mut changed = Vec::new();
    let mut note = |what: &str, did: bool| {
        if did {
            changed.push(what.to_string());
        }
    };

    // The program. The marker is there before anything else, so a run that
    // stops halfway leaves a directory the next run knows as its own; what
    // sudo finds is noted in it once the link is in place.
    directory(PROGRAM_DIR, 0, 0, 0o755)?;
    if !exists(PROGRAM_MARKER) {
        note(PROGRAM_MARKER, put(PROGRAM_MARKER, b"1\n", 0, 0, 0o644)?);
    }
    let binary = std::env::current_exe()
        .and_then(std::fs::read)
        .map_err(|exc| format!("cannot read the running binary: {exc}"))?;
    let program_changed = put(PROGRAM, &binary, 0, 0, 0o755)?;
    note(PROGRAM, program_changed);
    if std::fs::symlink_metadata(LINK).is_err() {
        std::os::unix::fs::symlink(PROGRAM, LINK).map_err(|exc| format!("{LINK}: {exc}"))?;
        note(LINK, true);
    }
    let marker = program_marker(sudo_needs_full_path());
    note(
        PROGRAM_MARKER,
        put(PROGRAM_MARKER, marker.as_bytes(), 0, 0, 0o644)?,
    );

    // State and the registry.
    directory(STATE, uid, gid, 0o700)?;
    if !exists(REGISTRY) {
        note(REGISTRY, put(REGISTRY, b"{}\n", uid, gid, 0o600)?);
    }
    let info = std::fs::symlink_metadata(REGISTRY).map_err(|exc| format!("{REGISTRY}: {exc}"))?;
    if !info.file_type().is_file()
        || info.nlink() != 1
        || (info.uid(), info.gid(), info.mode() & 0o7777) != (uid, gid, 0o600)
    {
        return Err(format!("unsafe registry owner, group, or mode: {REGISTRY}"));
    }

    // The config. Without --name a private server keeps the names it has, so
    // a changed address does not change its config or certificate.
    let previous = previous_names().filter(|names| !names.is_empty());
    let names = if mode == Mode::Private {
        if !options.names.is_empty() {
            options.names.clone()
        } else if let Some(previous) = &previous {
            previous.clone()
        } else {
            let mut names: Vec<String> = host_name().into_iter().collect();
            names.extend(primary_address());
            if names.is_empty() {
                return Err("no host name or address found; pass --name".into());
            }
            names
        }
    } else {
        Vec::new()
    };
    directory(CONFIG_DIR, uid, gid, 0o700)?;
    note(CONFIG_MARKER, put(CONFIG_MARKER, b"1\n", 0, 0, 0o600)?);
    let config_changed = put(
        CONFIG,
        render_config(options, &names).as_bytes(),
        uid,
        gid,
        0o600,
    )?;
    note(CONFIG, config_changed);

    // The private certificate, made now so its pin can be printed.
    let mut pin = None;
    let mut reissued = false;
    if mode == Mode::Private {
        let tls = directory(TLS_DIR, uid, gid, 0o700)?;
        let had = read_file(
            Some(&tls),
            "private-cert.pem",
            &format!("{TLS_DIR}/private-cert.pem"),
        )?
        .is_some();
        let (fingerprint, issued) = private_certificate(
            &tls,
            &names,
            !options.names.is_empty(),
            previous.as_deref(),
            uid,
            gid,
        )?;
        note(&format!("{TLS_DIR}/private-cert.pem"), issued);
        reissued = issued && had;
        pin = Some(fingerprint);
    }

    // The unit.
    let unit_changed = put(UNIT, render_unit(options).as_bytes(), 0, 0, 0o644)?;
    note(UNIT, unit_changed);
    let restart = program_changed || config_changed || unit_changed || reissued;
    if run("systemd-analyze", &["--version"]).is_ok() {
        checked("systemd-analyze", &["verify", UNIT])?;
    }

    // A token file from an installer before invites; nothing reads it now.
    let bootstrap = format!("{STATE}/bootstrap-token");
    if std::fs::symlink_metadata(&bootstrap).is_ok() {
        std::fs::remove_file(&bootstrap).map_err(|exc| format!("{bootstrap}: {exc}"))?;
        note(&format!("{bootstrap} (removed)"), true);
    }
    // The service.
    let mut started = false;
    if !options.no_start {
        checked("systemctl", &["daemon-reload"])?;
        checked("systemctl", &["enable", "--quiet", UNIT_NAME])?;
        let action = if restart { "restart" } else { "start" };
        checked("systemctl", &[action, UNIT_NAME])?;
        wait_ready(options, caddy_gid, uid, pin.as_deref())?;
        started = true;
    }
    let host = match mode {
        Mode::Acme => options.domain.clone().unwrap_or_default(),
        Mode::Private => names[0].clone(),
        Mode::Caddy => options
            .domain
            .clone()
            .unwrap_or_else(|| "YOUR-DOMAIN".into()),
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let listen = if mode == Mode::Caddy {
        443
    } else {
        port(options)
    };
    let hub_url = if listen == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{listen}")
    };
    // The first machine's invite, while none is enrolled; each run replaces
    // the one an earlier run printed. Caddy mode without --domain does not
    // know the address machines use, so it prints none.
    let known_address = mode != Mode::Caddy || options.domain.is_some();
    let invite = if !options.no_invite && !has_machines()? && known_address {
        let local = super::invites::Local {
            db: DB.into(),
            peers: REGISTRY.into(),
            lock: format!("{REGISTRY}.lock"),
            url: Some(hub_url.clone()),
            pin: pin.clone(),
        };
        let created = super::invites::create(
            &local,
            &super::invites::Request {
                url: &hub_url,
                pin: pin.as_deref(),
                expires: super::invites::INSTALL_EXPIRY,
                uses: 1,
                machine: None,
                source: "install",
                replace: true,
            },
        )
        .map_err(|failure| match failure {
            super::enroll_local::Failure::Reported(message)
            | super::enroll_local::Failure::Other(message) => message,
        })?;
        Some(created)
    } else {
        None
    };
    Ok(Outcome {
        changed,
        hub_url,
        pin,
        reissued,
        invite,
        started,
    })
}

/// Wait until the service answers: an HTTP reply over its Unix socket (with
/// the owner, group, and mode Caddy needs) or over TLS from the certificate
/// just pinned; for ACME, which may have no certificate yet, a TLS reply
/// (a handshake, or the alert a server without a certificate sends).
fn wait_ready(
    options: &Options,
    caddy_gid: Option<u32>,
    uid: u32,
    pin: Option<&str>,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ready = match options.mode {
            Some(Mode::Caddy) => {
                std::fs::symlink_metadata(SOCKET).is_ok_and(|info| {
                    (info.uid(), Some(info.gid()), info.mode() & 0o777) == (uid, caddy_gid, 0o660)
                }) && answers_unix(SOCKET)
            }
            Some(Mode::Acme) => answers_tls(
                port(options),
                options.domain.as_deref().unwrap_or_default(),
                None,
            ),
            _ => answers_tls(port(options), "localhost", pin),
        };
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let (_, status) = run(
                "systemctl",
                &["status", "--no-pager", "--lines=20", UNIT_NAME],
            )
            .unwrap_or_default();
            return Err(format!(
                "{UNIT_NAME} did not become ready within 15 seconds:\n{status}"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Accepts any server certificate and keeps it: the caller checks its pin.
#[derive(Debug)]
struct Presented(
    std::sync::Mutex<Option<Vec<u8>>>,
    Vec<rustls::SignatureScheme>,
);

impl rustls::client::danger::ServerCertVerifier for Presented {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(end_entity.as_ref().to_vec());
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = super::tls::provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = super::tls::provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.1.clone()
    }
}

/// Whether an HTTP reply comes back to `GET /health` (any status: an
/// unknown key is refused, which still shows the server answers).
fn answers_http(stream: &mut (impl Read + Write), headers: &str) -> bool {
    let request =
        format!("GET /health HTTP/1.1\r\nHost: localhost\r\n{headers}Connection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() || stream.flush().is_err() {
        return false;
    }
    let mut reply = [0u8; 9];
    let mut filled = 0;
    while filled < reply.len() {
        match stream.read(&mut reply[filled..]) {
            Ok(0) | Err(_) => return false,
            Ok(count) => filled += count,
        }
    }
    reply.starts_with(b"HTTP/1.")
}

/// Whether the server on the Unix socket `path` answers HTTP.
pub fn answers_unix(path: &str) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(path) else {
        return false;
    };
    let timeout = Some(Duration::from_secs(2));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    // A well-formed key the registry does not know: refused, but answered.
    let header = format!("X-Switchboard-Client-Fingerprint: {}\r\n", "0".repeat(64));
    answers_http(&mut stream, &header)
}

/// What a TLS connection to the server found.
#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// No connection, or the handshake failed without an alert.
    Unreachable,
    /// The server refused the handshake with a TLS alert, as an ACME server
    /// does before its first certificate.
    Alert,
    /// The handshake finished: the certificate's SHA-256, and whether an HTTP
    /// reply followed.
    Answered { fingerprint: String, http: bool },
}

/// Whether the server on 127.0.0.1:`port` answers TLS for `server_name`.
/// With `pin`, the handshake must finish with that certificate and an HTTP
/// reply follow. Without one (ACME), a finished handshake with an HTTP reply
/// or a TLS alert both count: before its first certificate the server
/// refuses handshakes with an alert.
pub fn answers_tls(port: u16, server_name: &str, pin: Option<&str>) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match probe_tls(address, server_name) {
        Probe::Unreachable => false,
        Probe::Alert => pin.is_none(),
        Probe::Answered { fingerprint, http } => pin.is_none_or(|pin| fingerprint == pin) && http,
    }
}

/// Connect to `address` with TLS for `server_name` (with a throwaway client
/// certificate, since the server asks every client for one), accept any
/// server certificate, and ask for `GET /health`.
pub fn probe_tls(address: std::net::SocketAddr, server_name: &str) -> Probe {
    use std::sync::Arc;
    let Ok(name) = rustls::pki_types::ServerName::try_from(server_name.to_string()) else {
        return Probe::Unreachable;
    };
    let Ok(key) = rcgen::KeyPair::generate() else {
        return Probe::Unreachable;
    };
    let Ok(cert) = rcgen::CertificateParams::new(vec!["switchboard-install-check".to_string()])
        .and_then(|params| params.self_signed(&key))
    else {
        return Probe::Unreachable;
    };
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
    let provider = super::tls::provider();
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let presented = Arc::new(Presented(std::sync::Mutex::new(None), schemes));
    let Ok(config) = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map(|builder| {
            builder
                .dangerous()
                .with_custom_certificate_verifier(presented.clone())
        })
        .and_then(|builder| builder.with_client_auth_cert(vec![cert.der().clone()], key))
    else {
        return Probe::Unreachable;
    };
    let Ok(mut socket) = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))
    else {
        return Probe::Unreachable;
    };
    let timeout = Some(Duration::from_secs(2));
    let _ = socket.set_read_timeout(timeout);
    let _ = socket.set_write_timeout(timeout);
    let Ok(mut connection) = rustls::ClientConnection::new(Arc::new(config), name) else {
        return Probe::Unreachable;
    };
    while connection.is_handshaking() {
        if let Err(error) = connection.complete_io(&mut socket) {
            let alert = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .is_some_and(|inner| matches!(inner, rustls::Error::AlertReceived(_)));
            return if alert {
                Probe::Alert
            } else {
                Probe::Unreachable
            };
        }
    }
    let fingerprint = presented
        .0
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|der| hex(&Sha256::digest(der))))
        .unwrap_or_default();
    let http = answers_http(&mut rustls::Stream::new(&mut connection, &mut socket), "");
    Probe::Answered { fingerprint, http }
}

/// `switchboard server install ...`.
pub fn main(argv: &[String]) -> i32 {
    let options = match parse(argv) {
        Ok(Some(options)) => options,
        Ok(None) => {
            crate::cli::print_help(&format!("{USAGE}{HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server install --help");
            return 2;
        }
    };
    if !cfg!(target_os = "linux") {
        crate::cli::print_problem(&crate::cli::ui::prompt::Problem::new(
            "The server installer supports only Linux with systemd.",
        ));
        return 1;
    }
    // SAFETY: umask cannot fail.
    unsafe { libc::umask(0o077) };
    let outcome = match install(&options) {
        Ok(outcome) => outcome,
        Err(message) => {
            if options.json {
                let value = Val::Dict(jsonval::dict([
                    ("ok", false.into()),
                    ("error", message.into()),
                ]));
                println!("{}", jsonval::dumps(&value, jsonval::DEFAULT));
            } else {
                crate::cli::print_failure(&message);
            }
            return 1;
        }
    };
    if options.json {
        let optional = |value: &Option<String>| match value {
            Some(text) => Val::from(text.clone()),
            None => Val::Null,
        };
        let value = Val::Dict(jsonval::dict([
            ("ok", true.into()),
            ("server_url", outcome.hub_url.clone().into()),
            ("server_pin", optional(&outcome.pin)),
            ("certificate_reissued", outcome.reissued.into()),
            (
                "invite",
                optional(&outcome.invite.as_ref().map(|c| c.token.clone())),
            ),
            ("started", outcome.started.into()),
            (
                "changed",
                Val::List(
                    outcome
                        .changed
                        .iter()
                        .map(|c| Val::from(c.clone()))
                        .collect(),
                ),
            ),
            ("setup", "switchboard init".into()),
        ]));
        println!("{}", jsonval::dumps(&value, jsonval::DEFAULT));
        return 0;
    }
    print!("{}", human_outcome(&options, &outcome));
    0
}

/// What `server install` did and what to do next, for people.
fn human_outcome(options: &Options, outcome: &Outcome) -> String {
    use crate::cli::ui::{self, prompt::Problem, Mark, Theme, Tone};
    let theme = Theme::for_fd(1);
    let width = crate::cli::format::output_width();
    let mut lines = Vec::new();
    if outcome.changed.is_empty() {
        lines.extend(ui::done(
            &theme,
            "The Switchboard server installation was already up to date",
            width,
        ));
    } else {
        lines.extend(ui::done(&theme, "Installed or updated:", width));
        for path in &outcome.changed {
            lines.push(format!("{}{path}", " ".repeat(ui::INDENT + 2)));
        }
    }
    if outcome.started {
        lines.extend(ui::done(&theme, &format!("{UNIT_NAME} is running"), width));
    } else {
        lines.extend(ui::item(
            &theme,
            Mark::Warned,
            &format!(
                "{UNIT_NAME} isn't running. Start it with\n`systemctl enable --now {UNIT_NAME}`"
            ),
            Tone::Normal,
            width,
        ));
    }
    if let Some(pin) = &outcome.pin {
        lines.extend(ui::done(
            &theme,
            &format!("Server certificate SHA-256 (clients pin it): {pin}"),
            width,
        ));
    }
    if outcome.reissued {
        lines.extend(ui::prompt::caution_lines(
            &theme,
            &Problem::new("The certificate changed to cover the new names, so machines that joined before can't connect.")
                .steps([
                    format!("Make a new invite for each of them with\n`{SUDO} server invite`"),
                    "On each one, run this, choose to join again, and paste the invite:\n`switchboard init`".to_string(),
                ]),
            width,
        ));
    }
    let note = match options.mode {
        Some(Mode::Acme) => Some(format!(
            "Port {} must be reachable from the internet for the certificate; it is obtained on first start.",
            port(options)
        )),
        Some(Mode::Private) => Some(format!(
            "Clients connect to port {}; open it in the firewall if one is active.",
            port(options)
        )),
        Some(Mode::Caddy) => Some(format!(
            "Caddy must forward to unix/{SOCKET} with client_auth mode require and no trust pool (the Caddyfile is in the repository's REFERENCE.md)."
        )),
        None => None,
    };
    if let Some(note) = note {
        while lines.last().is_some_and(|l: &String| l.is_empty()) {
            lines.pop();
        }
        lines.push(String::new());
        lines.extend(ui::para(&theme, &note, Tone::Normal, width));
    }
    lines.push(String::new());
    match &outcome.invite {
        Some(created) => {
            lines.extend(ui::para(
                &theme,
                "No machine has joined yet.",
                Tone::Strong,
                width,
            ));
            lines.push(String::new());
            let (_, text) = super::invites::describe_for(created, true);
            let mut out = lines.join("\n");
            out.push('\n');
            out.push_str(&text);
            return out;
        }
        None => {
            let command = if options.mode == Some(Mode::Caddy) && options.domain.is_none() {
                format!("{SUDO} server invite --url https://YOUR-DOMAIN")
            } else {
                format!("{SUDO} server invite")
            };
            lines.extend(ui::heading(&theme, "Next", width));
            lines.extend(ui::para(
                &theme,
                &format!("Make an invite for each new machine with\n`{command}`"),
                Tone::Normal,
                width,
            ));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn parsed(list: &[&str]) -> Options {
        parse(&args(list)).unwrap().unwrap()
    }

    #[test]
    fn arguments_are_checked_per_mode() {
        assert_eq!(parsed(&["--tls", "private"]).mode, Some(Mode::Private));
        assert_eq!(
            parsed(&["--tls=acme", "--domain", "Switchboard.Example.com"]).domain,
            Some("switchboard.example.com".into())
        );
        assert!(parse(&args(&["--help"])).unwrap().is_none());
        for bad in [
            &[][..],
            &["--tls"],
            &["--tls", "other"],
            &["--tls", "acme"],
            &["--tls", "acme", "--domain", "-bad.example.com"],
            &["--tls", "acme", "--domain", "localhost"][..],
            &["--tls", "private", "--email", "a@b.c"],
            &["--tls", "acme", "--domain", "a.example.com", "--name", "x"],
            &["--tls", "caddy", "--port", "8443"],
            &["--tls", "private", "--port", "0"],
            &["--tls", "private", "--name", "Not A Name"],
            &["--tls", "private", "--extra"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
        let options = parsed(&[
            "--tls", "private", "--name", "box.lan", "--name", "10.0.0.5",
        ]);
        assert_eq!(options.names, ["box.lan", "10.0.0.5"]);
    }

    #[test]
    fn configs_match_the_server_modes() {
        let acme = parsed(&[
            "--tls",
            "acme",
            "--domain",
            "sb.example.com",
            "--email",
            "op@example.com",
        ]);
        assert_eq!(
            render_config(&acme, &[]),
            "{\n  \"tls\": \"acme\",\n  \"hostname\": \"sb.example.com\",\n  \"acme_email\": \"op@example.com\",\n  \"db\": \"/var/lib/switchboard/messages.sqlite3\",\n  \"peers_file\": \"/var/lib/switchboard/peers.json\"\n}\n"
        );
        let private = parsed(&["--tls", "private", "--port", "9443"]);
        assert_eq!(
            render_config(&private, &["box.lan".into(), "10.0.0.5".into()]),
            "{\n  \"tls\": \"private\",\n  \"names\": [\n    \"box.lan\",\n    \"10.0.0.5\"\n  ],\n  \"port\": 9443,\n  \"db\": \"/var/lib/switchboard/messages.sqlite3\",\n  \"peers_file\": \"/var/lib/switchboard/peers.json\"\n}\n"
        );
        let caddy = parsed(&["--tls", "caddy"]);
        assert!(
            render_config(&caddy, &[]).contains("\"unix_socket\": \"/run/switchboard/hub.sock\"")
        );
    }

    #[test]
    fn units_carry_the_hardening_and_only_the_needed_privilege() {
        let acme = render_unit(&parsed(&["--tls", "acme", "--domain", "sb.example.com"]));
        assert!(acme.starts_with(MARKER));
        assert!(acme.contains("\nAmbientCapabilities=CAP_NET_BIND_SERVICE\n"));
        assert!(acme.contains("\nCapabilityBoundingSet=CAP_NET_BIND_SERVICE\n"));
        assert!(acme.contains("\nRestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n"));
        assert!(acme.contains("\nGroup=switchboard\n"));
        assert!(!acme.contains("RuntimeDirectory"));
        let private = render_unit(&parsed(&["--tls", "private"]));
        assert!(private.contains("\nAmbientCapabilities=\n"));
        assert!(private.contains("\nCapabilityBoundingSet=\n"));
        let caddy = render_unit(&parsed(&["--tls", "caddy"]));
        assert!(caddy.contains("\nGroup=caddy\n"));
        assert!(caddy.contains("\nRuntimeDirectory=switchboard\nRuntimeDirectoryMode=0750\n"));
        assert!(caddy.contains("\nRestrictAddressFamilies=AF_UNIX\n"));
        assert!(caddy.contains("\nReadWritePaths=/var/lib/switchboard /run/switchboard\n"));
        for unit in [&acme, &private, &caddy] {
            for line in [
                "User=switchboard",
                "SupplementaryGroups=switchboard",
                "ExecStart=/usr/local/lib/switchboard/switchboard server --config /etc/switchboard/server.json",
                "NoNewPrivileges=true",
                "ProtectSystem=strict",
                "ProtectHome=true",
                "PrivateTmp=true",
                "PrivateDevices=true",
                "ProtectKernelTunables=true",
                "ProtectKernelModules=true",
                "ProtectControlGroups=true",
                "RestrictSUIDSGID=true",
                "RestrictNamespaces=true",
                "LockPersonality=true",
                "UMask=0077",
                "MemoryMax=256M",
                "TasksMax=180",
                "LimitNOFILE=512",
            ] {
                assert!(unit.contains(&format!("\n{line}\n")), "{line}\n{unit}");
            }
        }
    }

    fn temporary_directory() -> String {
        let template = CString::new("/tmp/sbinst-XXXXXX").unwrap();
        let mut raw = template.into_bytes_with_nul();
        // SAFETY: raw is a writable NUL-terminated template.
        assert!(!unsafe { libc::mkdtemp(raw.as_mut_ptr() as *mut libc::c_char) }.is_null());
        raw.pop();
        let path = std::fs::canonicalize(String::from_utf8(raw).unwrap()).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn ids() -> (u32, u32) {
        // SAFETY: getuid and getgid cannot fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn mode(path: &str) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn set_mode(path: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn directories_are_changed_through_a_descriptor_never_a_link() {
        let root = temporary_directory();
        let (uid, gid) = ids();
        let made = format!("{root}/made");
        directory(&made, uid, gid, 0o710).unwrap();
        assert_eq!(mode(&made), 0o710);
        directory(&made, uid, gid, 0o700).unwrap();
        assert_eq!(mode(&made), 0o700);
        // A link in the directory's place (as the service account could make
        // inside a directory it owns) is refused, and its target untouched.
        let target = format!("{root}/target");
        std::fs::create_dir(&target).unwrap();
        set_mode(&target, 0o755);
        let link = format!("{root}/link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            directory(&link, uid, gid, 0o700).unwrap_err(),
            format!("refusing non-directory: {link}")
        );
        assert_eq!(mode(&target), 0o755);
        let file = format!("{root}/file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            directory(&file, uid, gid, 0o700).unwrap_err(),
            format!("refusing non-directory: {file}")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn files_are_never_read_or_replaced_through_a_link() {
        let root = temporary_directory();
        let (uid, gid) = ids();
        let path = format!("{root}/config.json");
        assert!(put(&path, b"one\n", uid, gid, 0o600).unwrap());
        assert!(!put(&path, b"one\n", uid, gid, 0o600).unwrap());
        assert!(put(&path, b"two\n", uid, gid, 0o600).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"two\n");
        let outside = format!("{root}/outside");
        std::fs::write(&outside, "secret\n").unwrap();
        set_mode(&outside, 0o644);
        let link = format!("{root}/link.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(
            put(&link, b"x", uid, gid, 0o600).unwrap_err(),
            format!("refusing unsafe file: {link}")
        );
        let hard = format!("{root}/hard.json");
        std::fs::hard_link(&outside, &hard).unwrap();
        assert_eq!(
            put(&hard, b"x", uid, gid, 0o600).unwrap_err(),
            format!("refusing unsafe file: {hard}")
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"secret\n");
        assert_eq!(mode(&outside), 0o644);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_private_certificate_is_issued_again_only_for_new_names() {
        let root = temporary_directory();
        let (uid, gid) = ids();
        let tls_path = format!("{root}/tls");
        let tls = directory(&tls_path, uid, gid, 0o700).unwrap();
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let first = names(&["box.lan", "10.0.0.5"]);
        let (pin, issued) = private_certificate(&tls, &first, true, None, uid, gid).unwrap();
        assert!(issued);
        assert_eq!(pin.len(), 64);
        for name in ["private-cert.pem", "private-key.pem"] {
            assert_eq!(mode(&format!("{tls_path}/{name}")), 0o600, "{name}");
        }
        // The same names again (in any order), or no --name at all: kept.
        let reordered = names(&["10.0.0.5", "box.lan"]);
        assert_eq!(
            private_certificate(&tls, &reordered, true, Some(&first), uid, gid).unwrap(),
            (pin.clone(), false)
        );
        let other = names(&["box.lan", "10.0.0.6"]);
        assert_eq!(
            private_certificate(&tls, &other, false, Some(&first), uid, gid).unwrap(),
            (pin.clone(), false)
        );
        // New names: a new certificate valid for them.
        let (second, issued) =
            private_certificate(&tls, &other, true, Some(&first), uid, gid).unwrap();
        assert!(issued);
        assert_ne!(second, pin);
        let pem = std::fs::read(format!("{tls_path}/private-cert.pem")).unwrap();
        let der = {
            use rustls::pki_types::pem::PemObject;
            rustls::pki_types::CertificateDer::from_pem_slice(&pem).unwrap()
        };
        assert!(super::super::tls::covers(&der, &other));
        assert!(!super::super::tls::covers(&der, &first));
        // The recorded names match but the certificate is not valid for them
        // (it was made for others): issued again.
        let (key, cert) = super::super::tls::issue_private(&names(&["elsewhere.lan"])).unwrap();
        std::fs::write(format!("{tls_path}/private-key.pem"), key).unwrap();
        std::fs::write(format!("{tls_path}/private-cert.pem"), cert).unwrap();
        let (third, issued) =
            private_certificate(&tls, &other, true, Some(&other), uid, gid).unwrap();
        assert!(issued);
        assert_ne!(third, second);
        // A link in place of the key is refused, and its target untouched.
        let outside = format!("{root}/outside");
        std::fs::write(&outside, "secret\n").unwrap();
        set_mode(&outside, 0o644);
        std::fs::remove_file(format!("{tls_path}/private-key.pem")).unwrap();
        std::os::unix::fs::symlink(&outside, format!("{tls_path}/private-key.pem")).unwrap();
        assert_eq!(
            private_certificate(&tls, &other, false, Some(&other), uid, gid).unwrap_err(),
            format!("refusing unsafe file: {TLS_DIR}/private-key.pem")
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"secret\n");
        assert_eq!(mode(&outside), 0o644);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hints_name_the_full_path_only_where_sudo_misses_it() {
        assert!(!probe_needs_full_path(true, "/usr/local/bin/switchboard\n"));
        assert!(probe_needs_full_path(true, "switchboard-not-found\n"));
        assert!(probe_needs_full_path(true, "/usr/bin/switchboard\n"));
        assert!(probe_needs_full_path(
            false,
            "sorry, you must have a tty to run sudo"
        ));
        assert_eq!(sudo_command(None), "sudo switchboard");
        assert_eq!(
            sudo_command(Some(&program_marker(false))),
            "sudo switchboard"
        );
        assert_eq!(
            sudo_command(Some(&program_marker(true))),
            "sudo /usr/local/bin/switchboard"
        );
        assert_eq!(program_marker(false), "1\n");
    }

    #[test]
    fn the_first_invite_can_be_left_out() {
        assert!(parsed(&["--tls", "private", "--no-invite"]).no_invite);
        assert!(!parsed(&["--tls", "private"]).no_invite);
    }

    #[test]
    fn dns_names() {
        assert!(is_dns_name("switchboard.example.com"));
        assert!(is_dns_name("box"));
        assert!(!is_dns_name("a..b"));
        assert!(!is_dns_name("a.b1"));
        assert!(!is_dns_name("UPPER.example.com"));
        assert!(!is_dns_name(&format!("{}.com", "a".repeat(64))));
    }
}
