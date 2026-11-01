//! Facts about the host that init checks: commands on PATH, addresses, DNS,
//! Tailscale, systemd, and running commands with a deadline.

use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The first executable `name` on PATH.
pub fn which(name: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    path.split(':').filter(|d| !d.is_empty()).find_map(|dir| {
        let candidate = format!("{}/{name}", dir.trim_end_matches('/'));
        let info = std::fs::metadata(&candidate).ok()?;
        (info.is_file() && info.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}

/// Run a command with standard input closed and its output captured;
/// `(status, stdout, stderr)`, or why it could not run or finish in time.
pub fn run(argv: &[String], timeout: Duration) -> Result<(i32, String, String), String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|exc| format!("{} could not run: {exc}", argv[0]))?;
    let reader = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_string(&mut text);
            }
            text
        })
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                let code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
                return Ok((
                    code,
                    out.join().unwrap_or_default(),
                    err.join().unwrap_or_default(),
                ));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} did not finish within {} seconds",
                    argv[0],
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(exc) => return Err(exc.to_string()),
        }
    }
}

pub fn is_root() -> bool {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Whether systemd is the init system.
pub fn systemd() -> bool {
    std::path::Path::new("/run/systemd/system").is_dir()
}

/// This machine's own addresses (not loopback or link-local).
pub fn local_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list freed with freeifaddrs below; each
    // entry's address is read according to its family.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut entry = list;
        while !entry.is_null() {
            let address = (*entry).ifa_addr;
            if !address.is_null() {
                match (*address).sa_family as i32 {
                    libc::AF_INET => {
                        let v4 = &*(address as *const libc::sockaddr_in);
                        out.push(IpAddr::from(u32::from_be(v4.sin_addr.s_addr).to_be_bytes()));
                    }
                    libc::AF_INET6 => {
                        let v6 = &*(address as *const libc::sockaddr_in6);
                        out.push(IpAddr::from(v6.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            entry = (*entry).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out.retain(|ip| !ip.is_loopback() && !ip.is_unspecified() && !link_local(ip));
    out.dedup();
    out
}

fn link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Tailscale's range (100.64.0.0/10).
pub fn is_tailscale(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
}

/// What `resolve` says when a name has no DNS record.
pub const NO_RECORD: &str = "no DNS record was found for it";

/// The addresses `name` resolves to.
pub fn resolve(name: &str) -> Result<Vec<IpAddr>, String> {
    let mut found: Vec<IpAddr> = (name, 443)
        .to_socket_addrs()
        .map_err(|exc| {
            let text = exc.to_string();
            let lower = text.to_ascii_lowercase();
            if lower.contains("nodename nor servname")
                || lower.contains("name or service not known")
                || lower.contains("no address associated")
                || lower.contains("not known")
            {
                NO_RECORD.to_string()
            } else {
                text
            }
        })?
        .map(|address| address.ip())
        .collect();
    found.dedup();
    if found.is_empty() {
        return Err("no addresses".into());
    }
    Ok(found)
}

/// This machine's Tailscale name (MagicDNS) and addresses, if Tailscale runs.
pub fn tailscale() -> (Option<String>, Vec<String>) {
    let Some(binary) = which("tailscale") else {
        return (None, Vec::new());
    };
    let Ok((0, stdout, _)) = run(
        &[binary, "status".into(), "--json".into()],
        Duration::from_secs(3),
    ) else {
        return (None, Vec::new());
    };
    let Ok(serde_json::Value::Object(status)) = serde_json::from_str(&stdout) else {
        return (None, Vec::new());
    };
    let own = status.get("Self").and_then(|v| v.as_object());
    let name = own
        .and_then(|s| s.get("DNSName"))
        .and_then(|v| v.as_str())
        .map(|n| n.trim_end_matches('.').to_ascii_lowercase())
        .filter(|n| crate::server::install::is_dns_name(n));
    let addresses = own
        .and_then(|s| s.get("TailscaleIPs"))
        .and_then(|v| v.as_array())
        .map(|ips| {
            ips.iter()
                .filter_map(|ip| ip.as_str())
                .filter(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (name, addresses)
}

/// The names a private server's certificate should cover, most reachable
/// first (invites point at the first): the Tailscale name and address, the
/// primary network address, then the host name.
pub fn default_server_names() -> Vec<String> {
    let (tailnet_name, tailnet_ips) = tailscale();
    let mut names: Vec<String> = Vec::new();
    names.extend(tailnet_name);
    names.extend(tailnet_ips);
    if let Some(primary) = crate::server::install::primary_address() {
        names.push(primary);
    }
    if let Some(host) = crate::server::install::host_name() {
        names.push(host);
    }
    let mut seen = Vec::new();
    names.retain(|name| {
        let fresh = !seen.contains(name);
        seen.push(name.clone());
        fresh
    });
    names
}

/// Whether something accepts connections on 127.0.0.1:`port`.
pub fn port_in_use(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(800),
    )
    .is_ok()
}

/// A Unix time as local `HH:MM ZONE` (with the date when not today).
pub fn local_time(at: i64) -> String {
    // SAFETY: localtime_r fills a zeroed tm; strftime writes at most the
    // buffer's length.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let time = at as libc::time_t;
        if libc::localtime_r(&time, &mut tm).is_null() {
            return at.to_string();
        }
        let mut now_tm: libc::tm = std::mem::zeroed();
        let now = libc::time(std::ptr::null_mut());
        libc::localtime_r(&now, &mut now_tm);
        let same_day = now_tm.tm_yday == tm.tm_yday && now_tm.tm_year == tm.tm_year;
        let format: &[u8] = if same_day {
            b"%H:%M %Z\0"
        } else {
            b"%b %e %H:%M %Z\0"
        };
        let mut buffer = [0u8; 64];
        let written = libc::strftime(
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
            format.as_ptr() as *const libc::c_char,
            &tm,
        );
        String::from_utf8_lossy(&buffer[..written])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A local timestamp for file names: `2026-10-31-0142`.
pub fn file_stamp() -> String {
    // SAFETY: as in local_time.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let now = libc::time(std::ptr::null_mut());
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// A free TCP port on 127.0.0.1.
pub fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()
        .map(|a| a.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_addresses_are_recognized() {
        assert!(is_tailscale(&"100.101.102.103".parse().unwrap()));
        assert!(!is_tailscale(&"100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale(&"10.0.0.5".parse().unwrap()));
    }

    #[test]
    fn local_addresses_skip_loopback() {
        assert!(local_addresses().iter().all(|ip| !ip.is_loopback()));
    }

    #[test]
    fn commands_run_with_a_deadline() {
        let (code, out, _) = run(
            &["/bin/sh".into(), "-c".into(), "echo hi; exit 3".into()],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!((code, out.as_str()), (3, "hi\n"));
        let late = run(
            &["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            Duration::from_millis(200),
        );
        assert!(late.unwrap_err().contains("did not finish"));
        assert!(which("sh").is_some());
        assert!(which("no-such-command-here").is_none());
    }
}
