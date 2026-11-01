//! Process identity: start tokens, PID domains, kernel peer credentials, and
//! the random identifiers a native peer owns.

use std::io::Read;
use std::os::fd::RawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::fsutil;

/// The process start token native Claude records as `procStart`.
///
/// Linux: field 22 of `/proc/<pid>/stat`. macOS: `ps -o lstart=` in the C
/// locale and UTC, exactly as Claude reads it.
pub fn process_start(pid: i64) -> Result<String> {
    if pid <= 1 {
        return Err(Error::value("invalid PID"));
    }
    process_start_digits(&pid.to_string())
}

/// `process_start` for a decoded JSON PID, which must be an integer above 1.
pub fn process_start_value(pid: Option<&serde_json::Value>) -> Result<String> {
    match pid.and_then(crate::wirejson::as_int) {
        Some(digits) if crate::wirejson::int_cmp(&digits, "1") == std::cmp::Ordering::Greater => {
            process_start_digits(&digits)
        }
        _ => Err(Error::value("invalid PID")),
    }
}

fn process_start_digits(pid: &str) -> Result<String> {
    if cfg!(target_os = "linux") {
        let data = fsutil::read_text(&format!("/proc/{pid}/stat"))?;
        let close = data
            .rfind(')')
            .ok_or_else(|| Error::value("substring not found"))?;
        let tail = data.get(close + 2..).unwrap_or("");
        return tail
            .split_whitespace()
            .nth(19)
            .map(str::to_string)
            .ok_or_else(|| Error::value("missing process start field"));
    }
    if cfg!(target_os = "macos") {
        // Read from the kernel directly; `ps` (a process spawn per check)
        // only when that fails, so its errors stay what they were.
        #[cfg(target_os = "macos")]
        if let Some(value) = kernel_lstart(pid) {
            return Ok(value);
        }
        let value = ps_lstart(pid)?;
        let value = crate::text::strip(&value).to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }
    Err(Error::value(
        "unsupported platform or missing process start",
    ))
}

/// `ps -o lstart=` for a live process, as `ps` prints it in the C locale and
/// UTC (`Sat Oct 31 21:30:05 2026`), from the kernel's process start time.
#[cfg(target_os = "macos")]
fn kernel_lstart(pid: &str) -> Option<String> {
    let pid: libc::c_int = pid.parse().ok().filter(|pid| *pid > 0)?;
    // SAFETY: proc_bsdinfo is plain data; all-zero is valid.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a proc_bsdinfo of the size passed.
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            size,
        )
    };
    if filled != size || info.pbi_pid as libc::c_int != pid || info.pbi_start_tvsec == 0 {
        return None;
    }
    Some(lstart_text(info.pbi_start_tvsec as i64))
}

/// `strftime("%c")` in the C locale for a UTC time, which `ps` uses for lstart.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn lstart_text(seconds: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let tm = crate::fmtutil::gmtime(seconds);
    // 1970-01-01 was a Thursday.
    let weekday = (seconds.div_euclid(86400) + 4).rem_euclid(7) as usize;
    format!(
        "{} {} {:>2} {:02}:{:02}:{:02} {}",
        DAYS[weekday],
        MONTHS[(tm.month - 1) as usize],
        tm.day,
        tm.hour,
        tm.minute,
        tm.second,
        tm.year
    )
}

fn ps_lstart(pid: &str) -> Result<String> {
    let args = ["ps", "-o", "lstart=", "-p", pid].map(str::to_string);
    let shown = format!(
        "[{}]",
        args.iter()
            .map(|a| crate::text::repr_str(a))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut child = Command::new("ps")
        .args(&args[1..])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|exc| Error::from_io(&exc, Some(std::path::Path::new("ps"))))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stdout = child.stdout.take().expect("piped stdout");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&stdout);
    let mut output = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !wait_readable(fd, remaining) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::subprocess(format!(
                "Command '{shown}' timed out after 2 seconds"
            )));
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&chunk[..count]),
            Err(exc) if exc.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(exc) => return Err(Error::from_io(&exc, None)),
        }
    }
    let status = child.wait().map_err(|exc| Error::from_io(&exc, None))?;
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        let code = match (status.code(), status.signal()) {
            (Some(code), _) => code,
            (None, Some(signal)) => -signal,
            _ => -1,
        };
        return Err(Error::subprocess(format!(
            "Command '{shown}' returned non-zero exit status {code}."
        )));
    }
    String::from_utf8(output).map_err(|_| Error::value("'utf-8' codec can't decode ps output"))
}

fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    loop {
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut poll, 1, millis) };
        if ready < 0 && Error::last_os() == libc::EINTR {
            continue;
        }
        return ready > 0;
    }
}

/// Whether a native record's `pidDomain` is this machine's PID namespace.
pub fn domain_is_local(value: Option<&serde_json::Value>) -> Result<bool> {
    if cfg!(target_os = "macos") {
        return Ok(value.and_then(|v| v.as_str()) == Some("darwin"));
    }
    if cfg!(target_os = "linux") {
        if let Some(value) = value.and_then(|v| v.as_str()) {
            // Use the installed native record's exact domain, after
            // corroborating its machine and PID namespace.
            let text = fsutil::read_text("/etc/machine-id")?;
            let machine = crate::text::strip(&text);
            let namespace = fsutil::readlink("/proc/self/ns/pid")?;
            return Ok(!machine.is_empty() && value.ends_with(&format!(":{machine}:{namespace}")));
        }
    }
    Ok(false)
}

/// This machine's native PID domain string, as native Claude writes it.
pub fn local_domain() -> Result<String> {
    if cfg!(target_os = "macos") {
        return Ok("darwin".into());
    }
    let text = fsutil::read_text("/etc/machine-id")?;
    let namespace = fsutil::readlink("/proc/self/ns/pid")?;
    Ok(format!("linux:{}:{}", crate::text::strip(&text), namespace))
}

/// The kernel-reported PID of the process at the other end of a Unix socket.
///
/// Linux checks the peer UID too. On macOS only LOCAL_PEERPID is read (no
/// peer UID check); the private socket directory is the UID boundary.
pub fn peer_pid(fd: RawFd) -> Result<i64> {
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: cred and size describe a valid writable buffer.
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut size,
            )
        };
        if result != 0 {
            return Err(Error::errno(Error::last_os()));
        }
        if cred.uid != fsutil::getuid() {
            return Err(Error::value("peer UID mismatch"));
        }
        Ok(cred.pid as i64)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: pid and size describe a valid writable buffer. SOL_LOCAL is
        // 0 and LOCAL_PEERPID is 2 on Darwin.
        let result = unsafe {
            libc::getsockopt(fd, 0, 2, &mut pid as *mut _ as *mut libc::c_void, &mut size)
        };
        if result != 0 {
            return Err(Error::errno(Error::last_os()));
        }
        Ok(pid as i64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        Err(Error::value("kernel peer identity unsupported"))
    }
}

/// Cryptographically secure random bytes from the kernel (as `os.urandom`).
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    for chunk in bytes.chunks_mut(256) {
        fill_random(chunk);
    }
    bytes
}

/// Fill at most 256 bytes from the kernel's random source: `getentropy` on
/// macOS, the `getrandom` system call on Linux, made directly: the libc
/// crate has no `getentropy` for musl, and the static musl build resolves the
/// `getrandom` wrapper to a null address. Both draw from the same kernel pool.
fn fill_random(chunk: &mut [u8]) {
    #[cfg(target_os = "linux")]
    {
        let flags: libc::c_uint = 0;
        let mut filled = 0;
        while filled < chunk.len() {
            // SAFETY: the pointer and length describe the unfilled tail of chunk.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_getrandom,
                    chunk[filled..].as_mut_ptr() as *mut libc::c_void,
                    chunk.len() - filled,
                    flags,
                )
            };
            if result < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EINTR),
                    "kernel entropy unavailable"
                );
                continue;
            }
            filled += result as usize;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: chunk is a valid writable buffer of at most 256 bytes.
        let result =
            unsafe { libc::getentropy(chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        assert_eq!(result, 0, "kernel entropy unavailable");
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 16 random bytes as 32 lowercase hex digits.
pub fn token_hex16() -> String {
    hex(&random_bytes::<16>())
}

/// A random (version 4) UUID as 32 lowercase hex digits, no hyphens.
pub fn uuid4_hex() -> String {
    let mut bytes = random_bytes::<16>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    hex(&bytes)
}

/// A random (version 4) UUID in the hyphenated 8-4-4-4-12 form.
pub fn uuid4() -> String {
    let h = uuid4_hex();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Seconds on the monotonic clock.
pub fn monotonic() -> f64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: now is a valid writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec as f64 + now.tv_nsec as f64 / 1e9
}

/// Milliseconds since the Unix epoch, truncated.
pub fn epoch_millis() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs_f64() * 1000.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_start_is_stable() {
        let pid = std::process::id() as i64;
        let first = process_start(pid).unwrap();
        assert!(!first.is_empty());
        assert_eq!(first, process_start(pid).unwrap());
        assert_eq!(process_start(1).unwrap_err().message, "invalid PID");
    }

    /// The kernel's start time reads exactly as `ps` prints it, for every
    /// process `ps` can see (the comparison native records rely on).
    #[cfg(target_os = "macos")]
    #[test]
    fn kernel_start_time_matches_ps_for_every_process() {
        let listed = Command::new("ps")
            .args(["-A", "-o", "pid="])
            .output()
            .unwrap();
        let mut compared = 0;
        for pid in String::from_utf8(listed.stdout).unwrap().split_whitespace() {
            let (Some(kernel), Ok(shown)) = (kernel_lstart(pid), ps_lstart(pid)) else {
                continue; // Exited in between, or not ours to read.
            };
            assert_eq!(kernel, crate::text::strip(&shown), "PID {pid}");
            compared += 1;
        }
        assert!(compared > 10, "only {compared} processes compared");
    }

    /// `%c` in the C locale pads one-digit days with a space.
    #[test]
    fn lstart_text_is_strftime_c() {
        assert_eq!(lstart_text(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(lstart_text(1_793_491_200), "Sun Nov  1 00:00:00 2026");
        assert_eq!(lstart_text(1_793_404_799), "Fri Oct 30 23:59:59 2026");
        assert_eq!(lstart_text(951_782_400), "Tue Feb 29 00:00:00 2000");
    }

    #[test]
    fn uuid4_is_canonical() {
        let value = uuid4();
        assert!(crate::protocol::valid_uuid_str(&value).is_ok());
        assert_eq!(&value[14..15], "4");
    }
}
