//! `switchboard server --config FILE`: the central server that routes
//! messages between enrolled machines.
//!
//! Listeners, chosen by the config:
//! - `unix_socket` (the Caddy mode): a private Unix socket behind a TLS
//!   proxy that passes the client certificate fingerprint and address in
//!   `X-Switchboard-Client-Fingerprint` and `X-Switchboard-Client-Address`.
//! - `cert`, `key`, `client_ca` (direct mode): mutual TLS on TCP with client
//!   certificates issued by `client_ca`.
//! - `"tls": "acme"`: public certificates obtained with TLS-ALPN-01, any
//!   client key accepted at the handshake (the registry decides).
//! - `"tls": "private"`: a self-signed server certificate made once and kept,
//!   whose fingerprint clients pin; any client key accepted at the handshake.
//!
//! Connections are served by one thread each, at most 128 at a time; a long
//! poll holds its thread for up to ten seconds, so threads are cheaper and
//! simpler here than an async runtime.
//! Polls and pings together hold at most `hub::WAITING_LIMIT` (96) of them,
//! so short requests always find a handler; deadlines are socket timeouts,
//! not timer threads.

pub mod acme;
pub mod certinfo;
pub mod enroll_local;
pub mod http;
pub mod hub;
pub mod install;
pub mod invites;
pub mod jsonval;
pub mod logs;
pub mod status;
pub mod system;
pub mod tls;
pub mod uninstall;

use std::io::Write;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;

use self::hub::Hub;
use self::jsonval::{get, Dict, Val};
use crate::fsutil;

const USAGE: &str = "Usage: switchboard server --config FILE
       switchboard server install --tls acme|private|caddy [options]
       switchboard server status | logs [options]
       switchboard server uninstall [--purge]
       switchboard server invite [options]
       switchboard server invite revoke ID
       switchboard server invites | machines
       switchboard server revoke MACHINE
       switchboard server enroll-local
";

const HELP: &str = "
Run the Switchboard server with the configuration in FILE (the service that
switchboard server install sets up runs it this way).

  install         Set up this machine as the server (see install --help)
  status          Whether the server is healthy, with the fix for each problem
  logs            The server's events from the system journal
  uninstall       Remove the server; --purge also deletes its data
  invite          Make an invite a new machine joins with (see invite --help)
  invite revoke   Cancel an invite that has not been used up
  invites         List the open invites
  machines        List the machines on this server and when each was last seen
  revoke MACHINE  Remove a machine from this server; its key stops working
  enroll-local    Enroll one machine from a request on standard input (as
                  root, over SSH)

Run these on the server as root or as the server's service account (logs
needs root; install and uninstall need root).

Options:
  --config FILE  The server configuration
  -h, --help     Show this help
";
const HANDLERS: usize = 128;
const BACKLOG: i32 = 128;
// Waiting requests leave handlers free for registration, sends, and statuses.
const _: () = assert!(hub::WAITING_LIMIT + 32 <= HANDLERS);

/// How startup ended: exit status 2 for usage and unsafe-file errors, 1 for
/// any other failure.
enum Exit {
    Usage(String),
    Failure(String),
}

fn failure(message: impl Into<String>) -> Exit {
    Exit::Failure(message.into())
}

pub fn main(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some("enroll-local") => return enroll_local::main(&argv[1..]),
        Some("install") => return install::main(&argv[1..]),
        Some("status") => return status::main(&argv[1..]),
        Some("logs") => return logs::main(&argv[1..]),
        Some("uninstall") => return uninstall::main(&argv[1..]),
        Some("invite") => return invites::invite_main(&argv[1..]),
        Some("invites") => return invites::invites_main(&argv[1..]),
        Some("machines") => return invites::machines_main(&argv[1..]),
        Some("revoke") => return invites::revoke_main(&argv[1..]),
        _ => {}
    }
    let config = match parse_args(argv) {
        Ok(Some(config)) => config,
        Ok(None) => {
            crate::cli::print_help(&format!("{USAGE}{HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(
                &format!("switchboard server: {message}"),
                "switchboard server --help",
            );
            return 2;
        }
    };
    // SAFETY: umask cannot fail.
    unsafe { libc::umask(0o077) };
    match run(&config) {
        Ok(()) => 0,
        Err(Exit::Usage(message)) => {
            eprintln!("switchboard server: {message}");
            2
        }
        Err(Exit::Failure(message)) => {
            eprintln!("switchboard server: {message}");
            1
        }
    }
}

/// `--config FILE` (or `--config=FILE`; the last one wins); `None` for
/// `-h`/`--help`.
fn parse_args(argv: &[String]) -> Result<Option<String>, String> {
    let mut config = None;
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        if arg == "-h" || arg == "--help" {
            return Ok(None);
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        if name == "--config" {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    match argv.get(index) {
                        Some(value) if !value.starts_with('-') || value.len() == 1 => value.clone(),
                        _ => return Err("--config needs a value".into()),
                    }
                }
            };
            config = Some(value);
            index += 1;
            continue;
        }
        return Err(if arg.starts_with('-') {
            format!("unknown option {arg}")
        } else {
            format!("unexpected argument {arg}")
        });
    }
    config
        .map(Some)
        .ok_or_else(|| "--config is required".into())
}

/// `name` under `base`, unless `name` is absolute (then it replaces `base`).
fn path_join(base: &str, name: &str) -> String {
    if fsutil::is_absolute(name) {
        fsutil::normalize(name)
    } else {
        fsutil::join(base, name)
    }
}

fn os_failure(error: crate::error::Error) -> Exit {
    failure(error.message)
}

fn truthy(value: &Val) -> bool {
    match value {
        Val::Null => false,
        Val::Bool(flag) => *flag,
        Val::Int(digits) => digits != "0",
        Val::Float(number) => *number != 0.0,
        Val::Str(text) => !text.is_empty(),
        Val::List(items) => !items.is_empty(),
        Val::Dict(map) => !map.is_empty(),
    }
}

/// The listener a config asks for.
enum Mode {
    Unix,
    Direct,
    Acme,
    Private,
}

fn text_field<'a>(config: &'a Dict, name: &str) -> Result<&'a str, Exit> {
    match get(config, name) {
        None => Err(failure(format!("KeyError: '{name}'"))),
        Some(value) => value
            .as_str()
            .ok_or_else(|| failure(format!("{name}: expected a string"))),
    }
}

fn run(config_arg: &str) -> Result<(), Exit> {
    let path = fsutil::resolve(config_arg).map_err(os_failure)?;
    let info = fsutil::stat(&path).map_err(os_failure)?;
    if info.mode() & 0o077 != 0 {
        return Err(Exit::Usage("config must have mode 0600".into()));
    }
    let text = std::fs::read_to_string(&path).map_err(|exc| failure(format!("{path}: {exc}")))?;
    let mut config = match jsonval::loads_str(&text) {
        Some(Val::Dict(map)) => map,
        Some(_) => return Err(failure("the config must be a JSON object")),
        None => return Err(failure(format!("{path}: invalid JSON"))),
    };
    let directory = fsutil::parent(&path);
    let has = |config: &Dict, name: &str| get(config, name).is_some();
    let mode = if has(&config, "unix_socket") {
        Mode::Unix
    } else if let Some(tls) = get(&config, "tls") {
        match tls.as_str() {
            Some("acme") => Mode::Acme,
            Some("private") => Mode::Private,
            _ => return Err(failure("tls must be \"acme\" or \"private\"")),
        }
    } else {
        Mode::Direct
    };
    let paths: &[&str] = match mode {
        Mode::Unix => &["db", "peers_file", "unix_socket"],
        Mode::Direct => &["cert", "key", "client_ca", "db", "peers_file"],
        Mode::Acme => &["db", "peers_file", "acme_ca"],
        Mode::Private => &["db", "peers_file"],
    };
    for name in paths {
        if *name == "acme_ca" && !has(&config, name) {
            continue;
        }
        let joined = path_join(&directory, text_field(&config, name)?);
        let value = if *name == "unix_socket" {
            fsutil::absolute(&joined).map_err(os_failure)?
        } else {
            fsutil::resolve(&joined).map_err(os_failure)?
        };
        jsonval::set(&mut config, name, value);
    }
    let private: &[&str] = match mode {
        Mode::Direct => &["key", "peers_file"],
        _ => &["peers_file"],
    };
    for name in private {
        let info = fsutil::stat(text_field(&config, name)?).map_err(os_failure)?;
        if info.mode() & 0o077 != 0 {
            return Err(Exit::Usage(format!("{name} must have mode 0600")));
        }
    }
    let db = text_field(&config, "db")?.to_string();
    let state = fsutil::parent(&db);
    fsutil::mkdir_parents(&state, 0o700).map_err(os_failure)?;
    if fsutil::stat(&state).map_err(os_failure)?.mode() & 0o077 != 0 {
        return Err(Exit::Usage("state directory must have mode 0700".into()));
    }
    let hub_config = hub::Config {
        db: db.clone(),
        peers_file: text_field(&config, "peers_file")?.to_string(),
        registry_lock: get(&config, "registry_lock").filter(|v| truthy(v)).cloned(),
    };
    let open_hub = move || Hub::open(hub_config).map_err(failure);
    match mode {
        Mode::Unix => {
            let conflicting = ["listen", "port", "cert", "key", "client_ca", "tls"];
            if conflicting.iter().any(|key| has(&config, key)) {
                return Err(failure(
                    "Unix ingress cannot be combined with TCP or TLS listener settings",
                ));
            }
            let hub = Arc::new(open_hub()?);
            let socket = text_field(&config, "unix_socket")?.to_string();
            let listener = bind_unix(&socket)?;
            serve_unix(hub, listener, &socket);
        }
        Mode::Direct => {
            let termination = tls::Termination::Fixed(
                tls::direct(
                    text_field(&config, "cert")?,
                    text_field(&config, "key")?,
                    text_field(&config, "client_ca")?,
                )
                .map_err(failure)?,
            );
            let address = listen_address(&config, "127.0.0.1", 8743, true)?;
            let hub = Arc::new(open_hub()?);
            let listener = bind_tcp(address)?;
            serve_tcp(hub, listener, Arc::new(termination));
        }
        Mode::Acme | Mode::Private => {
            let conflicting = ["cert", "key", "client_ca"];
            if conflicting.iter().any(|key| has(&config, key)) {
                return Err(failure(
                    "built-in TLS cannot be combined with cert, key, or client_ca",
                ));
            }
            let storage = format!("{state}/tls");
            fsutil::mkdir_parents(&storage, 0o700).map_err(os_failure)?;
            let info = fsutil::lstat(&storage).map_err(os_failure)?;
            if !info.is_dir() || info.mode() & 0o077 != 0 || info.uid() != fsutil::getuid() {
                return Err(Exit::Usage(
                    "tls state directory must be owned with mode 0700".into(),
                ));
            }
            if matches!(mode, Mode::Private) {
                let names = match get(&config, "names") {
                    None => vec!["localhost".to_string(), "127.0.0.1".into(), "::1".into()],
                    Some(Val::List(items)) if !items.is_empty() => items
                        .iter()
                        .map(|item| item.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| {
                            failure("names must be a list of host names or addresses")
                        })?,
                    Some(_) => {
                        return Err(failure("names must be a list of host names or addresses"))
                    }
                };
                let identity = tls::private_identity(&storage, &names).map_err(failure)?;
                let line = format!(
                    "{{\"event\": \"private_certificate\", \"sha256\": \"{}\", \"certificate\": {}, \"created\": {}}}\n",
                    identity.fingerprint,
                    jsonval::dumps(&Val::from(format!("{storage}/private-cert.pem")), jsonval::DEFAULT),
                    identity.created
                );
                let _ = std::io::stderr().write_all(line.as_bytes());
                let termination = tls::Termination::Fixed(
                    tls::private(identity.chain, identity.key).map_err(failure)?,
                );
                let address = listen_address(&config, "::", 8743, false)?;
                let hub = Arc::new(open_hub()?);
                let listener = bind_tcp(address)?;
                serve_tcp(hub, listener, Arc::new(termination));
            } else {
                let hostname = text_field(&config, "hostname")?.to_ascii_lowercase();
                let optional = |name: &str| -> Result<Option<String>, Exit> {
                    match get(&config, name) {
                        None => Ok(None),
                        Some(_) => Ok(Some(text_field(&config, name)?.to_string())),
                    }
                };
                let settings = acme::Settings {
                    hostname: hostname.clone(),
                    directory: optional("acme_directory")?
                        .unwrap_or_else(|| acme::LETS_ENCRYPT.into()),
                    ca_file: optional("acme_ca")?,
                    email: optional("acme_email")?,
                    storage,
                };
                let address = listen_address(&config, "::", 443, false)?;
                let hub = Arc::new(open_hub()?);
                let listener = bind_tcp(address)?;
                // The listener is up before the first order, so the ACME
                // server's validation handshake finds it.
                let certificates = acme::start(settings).map_err(failure)?;
                let termination = tls::Termination::Public {
                    hostname,
                    normal: tls::public(certificates.clone()).map_err(failure)?,
                    challenge: tls::challenge(certificates).map_err(failure)?,
                };
                serve_tcp(hub, listener, Arc::new(termination));
            }
        }
    }
    Ok(())
}

/// The config's `listen` and `port`, each defaulting to the given value; direct
/// mode binds IPv4 only. The certificate
/// modes default to `::`, which `bind_tcp` serves dual-stack.
fn listen_address(
    config: &Dict,
    host: &str,
    port: u16,
    ipv4_only: bool,
) -> Result<SocketAddr, Exit> {
    let host = match get(config, "listen") {
        None => host.to_string(),
        Some(value) => value
            .as_str()
            .ok_or_else(|| failure("listen must be a string"))?
            .to_string(),
    };
    let port = match get(config, "port") {
        None => port,
        Some(Val::Bool(flag)) => u16::from(*flag),
        Some(Val::Int(digits)) => digits
            .parse::<u16>()
            .map_err(|_| failure("port must be 0-65535"))?,
        Some(_) => return Err(failure("port must be an integer")),
    };
    let host = if host.is_empty() {
        "0.0.0.0".to_string()
    } else {
        host
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|exc| failure(format!("{host}: {exc}")))?
        .find(|address| !ipv4_only || address.is_ipv4())
        .ok_or_else(|| failure(format!("{host}: no usable address")))
}

fn bind_tcp(address: SocketAddr) -> Result<TcpListener, Exit> {
    if let SocketAddr::V6(v6) = address {
        if v6.ip().is_unspecified() {
            return match bind_dual_stack(v6.port()) {
                Ok(listener) => Ok(listener),
                // A host without IPv6 still serves IPv4.
                Err(exc)
                    if matches!(
                        exc.raw_os_error(),
                        Some(libc::EAFNOSUPPORT)
                            | Some(libc::EADDRNOTAVAIL)
                            | Some(libc::EPROTONOSUPPORT)
                    ) =>
                {
                    bind_tcp(SocketAddr::from(([0, 0, 0, 0], v6.port())))
                }
                Err(exc) => Err(failure(format!("{address}: {exc}"))),
            };
        }
    }
    let listener =
        TcpListener::bind(address).map_err(|exc| failure(format!("{address}: {exc}")))?;
    // SAFETY: re-listening on a listening socket only sets its backlog.
    unsafe { libc::listen(listener.as_raw_fd(), BACKLOG) };
    Ok(listener)
}

/// One IPv6 socket on every address that also accepts IPv4 (IPV6_V6ONLY off,
/// set before bind so a host's bindv6only default doesn't matter).
fn bind_dual_stack(port: u16) -> std::io::Result<TcpListener> {
    use std::os::fd::FromRawFd;
    let last = std::io::Error::last_os_error;
    // SAFETY: plain socket calls on a descriptor this function owns; it is
    // closed on every error path and otherwise handed to TcpListener.
    unsafe {
        let fd = libc::socket(libc::AF_INET6, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(last());
        }
        let fail = |fd: RawFd| {
            let error = last();
            libc::close(fd);
            Err(error)
        };
        if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return fail(fd);
        }
        let on: libc::c_int = 1;
        let off: libc::c_int = 0;
        let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        if libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&on as *const libc::c_int).cast(),
            size,
        ) < 0
            || libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                (&off as *const libc::c_int).cast(),
                size,
            ) < 0
        {
            return fail(fd);
        }
        let mut address: libc::sockaddr_in6 = std::mem::zeroed();
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            address.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
        }
        address.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        address.sin6_port = port.to_be();
        if libc::bind(
            fd,
            (&address as *const libc::sockaddr_in6).cast(),
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        ) < 0
            || libc::listen(fd, BACKLOG) < 0
        {
            return fail(fd);
        }
        Ok(TcpListener::from_raw_fd(fd))
    }
}

/// The private ingress: the parent directory must be the service's own
/// (uid, primary gid, mode 0750) and an existing path is never replaced.
fn bind_unix(socket: &str) -> Result<UnixListener, Exit> {
    if !fsutil::is_absolute(socket) {
        return Err(failure("unix_socket must be absolute"));
    }
    let parent = fsutil::lstat(&fsutil::parent(socket)).map_err(os_failure)?;
    // SAFETY: geteuid and getegid cannot fail.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if !parent.is_dir()
        || parent.mode() & 0o7777 != 0o750
        || parent.uid() != euid
        || parent.gid() != egid
    {
        return Err(failure(
            "Unix socket directory must be owned by service uid/gid with mode 0750",
        ));
    }
    if fsutil::lstat(socket).is_ok() {
        return Err(failure("Unix socket path already exists"));
    }
    let listener = UnixListener::bind(socket).map_err(|exc| failure(format!("{socket}: {exc}")))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))
        .map_err(|exc| failure(format!("{socket}: {exc}")))?;
    // SAFETY: re-listening on a listening socket only sets its backlog.
    unsafe { libc::listen(listener.as_raw_fd(), BACKLOG) };
    Ok(listener)
}

static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    let fd = WAKE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = 0u8;
        // SAFETY: write(2) is async-signal-safe.
        unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
    }
}

/// SIGINT stops the server cleanly; SIGTERM keeps its default action.
fn interrupt_pipe() -> RawFd {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid two-element array; the handler is async-signal-safe.
    unsafe {
        libc::pipe(fds.as_mut_ptr());
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
        WAKE.store(fds[1], Ordering::SeqCst);
        // An inherited "ignore" for SIGINT stays: the server then can't be interrupted.
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current);
        if current.sa_sigaction == libc::SIG_IGN {
            return fds[0];
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_interrupt as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    fds[0]
}

/// Wait for a connection or SIGINT; `false` once interrupted.
fn wait_readable(listener: RawFd, wake: RawFd) -> bool {
    loop {
        let mut fds = [
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds is a valid array of two pollfd structures.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            continue;
        }
        if fds[1].revents != 0 {
            return false;
        }
        if fds[0].revents != 0 {
            return true;
        }
    }
}

/// At most 128 connections are handled at once; others are closed at once.
struct Slots(AtomicUsize);

impl Slots {
    fn acquire(&self) -> bool {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < HANDLERS).then_some(used + 1)
            })
            .is_ok()
    }

    fn release(&self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One acquired slot, released when dropped: when the handler returns, when
/// it panics and its thread unwinds, or when its thread cannot be started
/// (the unstarted closure, and the slot with it, is dropped).
struct Slot(Arc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Run `work` on its own thread holding an acquired slot. A panic in `work`
/// ends only that thread (see `crate::panics`); the connection it owned is
/// closed as the thread unwinds and the slot is released.
fn spawn(slots: &Arc<Slots>, work: impl FnOnce() + Send + 'static) {
    let slot = Slot(slots.clone());
    let _ = std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(move || {
            let _slot = slot;
            crate::panics::contain();
            work();
        });
}

fn serve_unix(hub: Arc<Hub>, listener: UnixListener, socket: &str) {
    let identity = fsutil::lstat(socket)
        .ok()
        .map(|info| (info.dev(), info.ino()));
    let wake = interrupt_pipe();
    let slots = Arc::new(Slots(AtomicUsize::new(0)));
    while wait_readable(listener.as_raw_fd(), wake) {
        let Ok((stream, _)) = listener.accept() else {
            continue;
        };
        if !slots.acquire() {
            drop(stream);
            continue;
        }
        let hub = hub.clone();
        spawn(&slots, move || {
            http::serve(&hub, http::Link::Unix(stream), http::Identity::Proxy)
        });
    }
    drop(listener);
    // Remove the socket only if it is still the one this process created.
    if let (Some(identity), Ok(info)) = (identity, fsutil::lstat(socket)) {
        if info.file_type().is_socket() && (info.dev(), info.ino()) == identity {
            let _ = std::fs::remove_file(socket);
        }
    }
    std::process::exit(0);
}

fn serve_tcp(hub: Arc<Hub>, listener: TcpListener, termination: Arc<tls::Termination>) {
    let wake = interrupt_pipe();
    let slots = Arc::new(Slots(AtomicUsize::new(0)));
    while wait_readable(listener.as_raw_fd(), wake) {
        let Ok((stream, peer)) = listener.accept() else {
            continue;
        };
        if !slots.acquire() {
            drop(stream);
            continue;
        }
        let hub = hub.clone();
        let termination = termination.clone();
        spawn(&slots, move || {
            let address = peer.ip().to_canonical().to_string();
            if let Some(handshake) = tls::accept(&termination, stream) {
                let link = http::Link::Tls(Box::new(handshake.stream));
                let identity = http::Identity::Certificate {
                    fingerprint: handshake.fingerprint,
                    address,
                };
                http::serve(&hub, link, identity);
            }
        });
    }
    std::process::exit(0);
}

use std::os::unix::fs::FileTypeExt;

/// Write a private (0600) file atomically: a new temporary file, then rename.
pub fn write_private(path: &str, data: &[u8]) -> Result<(), String> {
    let temporary = format!("{}.{}", path, crate::identity::uuid4_hex());
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|exc| format!("{path}: {exc}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn dual_stack_listener_accepts_ipv4_and_ipv6() {
        let Ok(listener) = super::bind_tcp("[::]:0".parse().unwrap()) else {
            panic!("could not bind the dual-stack listener");
        };
        let port = listener.local_addr().unwrap().port();
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        // Where the host has IPv6 loopback, the same socket must accept it.
        if std::net::TcpListener::bind("[::1]:0").is_ok() {
            assert!(std::net::TcpStream::connect(("::1", port)).is_ok());
        }
    }

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_panicking_handler_releases_its_slot() {
        let slots = Arc::new(Slots(AtomicUsize::new(0)));
        for _ in 0..(HANDLERS + 10) {
            assert!(slots.acquire());
            let (done, finished) = std::sync::mpsc::channel::<()>();
            spawn(&slots, move || {
                let _done = done;
                panic!("forced panic in a handler");
            });
            // The sender is dropped as the handler unwinds.
            assert!(finished.recv().is_err());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while slots.0.load(Ordering::SeqCst) != 0 {
                assert!(std::time::Instant::now() < deadline, "slot not released");
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn arguments_parse() {
        assert_eq!(parse_args(&args(&["--config", "a"])), Ok(Some("a".into())));
        assert_eq!(parse_args(&args(&["--config=a"])), Ok(Some("a".into())));
        assert_eq!(
            parse_args(&args(&["--config", "a", "--config", "b"])),
            Ok(Some("b".into()))
        );
        assert_eq!(parse_args(&args(&["-h"])), Ok(None));
        assert_eq!(parse_args(&args(&[])), Err("--config is required".into()));
        assert_eq!(
            parse_args(&args(&["--config"])),
            Err("--config needs a value".into())
        );
        assert_eq!(
            parse_args(&args(&["--config", "a", "extra"])),
            Err("unexpected argument extra".into())
        );
        assert_eq!(
            parse_args(&args(&["--conf", "a"])),
            Err("unknown option --conf".into())
        );
        assert!(parse_args(&args(&["--port", "1"])).is_err());
        assert_eq!(path_join("/etc/sb", "/var/x"), "/var/x");
        assert_eq!(path_join("/etc/sb", "db/../m.db"), "/etc/sb/db/../m.db");
    }
}
