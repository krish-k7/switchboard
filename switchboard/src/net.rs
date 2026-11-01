//! Unix stream sockets with a per-call timeout (each connect, whole send, or
//! receive fails with `timed out` when it runs out), and the native
//! frame writer every Switchboard-to-Claude message goes through.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::discovery::{auth_key, session_record, Record, Selection};
use crate::error::{Error, Result};
use crate::identity::{monotonic, process_start_value};
use crate::protocol::FRAME_LIMIT;
use crate::wirejson::{dumps, value_eq_opt};

/// Poll until a descriptor is ready, retrying on EINTR; Ok(false) when the
/// timeout (seconds, None for no limit) runs out first.
pub fn poll_fds(fds: &mut [libc::pollfd], timeout: Option<f64>) -> Result<bool> {
    let deadline = timeout.map(|t| monotonic() + t);
    loop {
        let millis = match deadline {
            None => -1,
            Some(deadline) => {
                let left = (deadline - monotonic()).max(0.0);
                (left * 1000.0).ceil().min(i32::MAX as f64) as i32
            }
        };
        // SAFETY: fds is a valid slice of pollfd for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let code = Error::last_os();
            if code == libc::EINTR {
                continue;
            }
            return Err(Error::errno(code));
        }
        return Ok(ready > 0);
    }
}

pub fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

pub fn set_nonblocking(fd: RawFd, on: bool) {
    // SAFETY: fcntl on a valid descriptor.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        let flags = if on {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        libc::fcntl(fd, libc::F_SETFL, flags);
    }
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a valid descriptor.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

#[cfg(target_os = "linux")]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const SEND_FLAGS: libc::c_int = 0;

fn no_sigpipe(fd: RawFd) {
    #[cfg(target_os = "macos")]
    {
        let one: libc::c_int = 1;
        // SAFETY: setsockopt with a valid int option value.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    let _ = fd;
}

/// A Unix stream socket whose operations each time out like `settimeout`.
pub struct Stream {
    fd: OwnedFd,
    pub timeout: f64,
}

pub fn sockaddr(path: &str) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: sockaddr_un is plain old data; zeroed is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() > addr.sun_path.len() {
        return Err(Error::os("AF_UNIX path too long"));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let base = std::mem::size_of::<libc::sockaddr_un>() - addr.sun_path.len();
    let len = (base + bytes.len()) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = len as u8;
    }
    Ok((addr, len))
}

pub fn unix_socket() -> Result<OwnedFd> {
    // SAFETY: plain socket(2) call; the result is checked.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(Error::errno(Error::last_os()));
    }
    set_cloexec(fd);
    // SAFETY: fd is a fresh descriptor owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Stream {
    pub fn from_fd(fd: OwnedFd, timeout: f64) -> Self {
        set_nonblocking(fd.as_raw_fd(), true);
        no_sigpipe(fd.as_raw_fd());
        Stream { fd, timeout }
    }

    /// Connect a new Unix stream socket to `path` within `timeout` seconds;
    /// the stream keeps `timeout` for later sends and receives.
    pub fn connect(path: &str, timeout: f64) -> Result<Self> {
        let stream = Stream::from_fd(unix_socket()?, timeout);
        let (addr, len) = sockaddr(path)?;
        // SAFETY: addr/len describe a valid sockaddr_un.
        let result = unsafe {
            libc::connect(
                stream.raw(),
                &addr as *const _ as *const libc::sockaddr,
                len,
            )
        };
        if result == 0 {
            return Ok(stream);
        }
        let code = Error::last_os();
        if code != libc::EINPROGRESS && code != libc::EINTR {
            return Err(Error::errno(code));
        }
        let mut fds = [pollfd(stream.raw(), libc::POLLOUT)];
        if !poll_fds(&mut fds, Some(timeout))? {
            return Err(Error::timed_out());
        }
        let mut error: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: error/size describe a valid writable int.
        unsafe {
            libc::getsockopt(
                stream.raw(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut error as *mut _ as *mut libc::c_void,
                &mut size,
            );
        }
        if error != 0 {
            return Err(Error::errno(error));
        }
        Ok(stream)
    }

    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Send all of `data`; the timeout bounds the whole operation.
    pub fn send_all(&self, mut data: &[u8]) -> Result<()> {
        let deadline = monotonic() + self.timeout;
        while !data.is_empty() {
            let left = deadline - monotonic();
            let mut fds = [pollfd(self.raw(), libc::POLLOUT)];
            if left <= 0.0 || !poll_fds(&mut fds, Some(left))? {
                return Err(Error::timed_out());
            }
            // SAFETY: data is a valid readable buffer.
            let sent = unsafe {
                libc::send(
                    self.raw(),
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    SEND_FLAGS,
                )
            };
            if sent < 0 {
                let code = Error::last_os();
                if code == libc::EINTR || code == libc::EAGAIN || code == libc::EWOULDBLOCK {
                    continue;
                }
                return Err(Error::errno(code));
            }
            data = &data[sent as usize..];
        }
        Ok(())
    }

    /// Receive up to `buffer.len()` bytes (0 at end of stream), waiting at most
    /// the stream's timeout.
    pub fn recv(&self, buffer: &mut [u8]) -> Result<usize> {
        loop {
            let mut fds = [pollfd(self.raw(), libc::POLLIN)];
            if !poll_fds(&mut fds, Some(self.timeout))? {
                return Err(Error::timed_out());
            }
            // SAFETY: buffer is a valid writable buffer.
            let count = unsafe {
                libc::recv(
                    self.raw(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                    0,
                )
            };
            if count < 0 {
                let code = Error::last_os();
                if code == libc::EINTR || code == libc::EAGAIN || code == libc::EWOULDBLOCK {
                    continue;
                }
                return Err(Error::errno(code));
            }
            return Ok(count as usize);
        }
    }

    pub fn shutdown(&self, how: libc::c_int) -> Result<()> {
        // SAFETY: shutdown on a valid socket.
        if unsafe { libc::shutdown(self.raw(), how) } != 0 {
            return Err(Error::errno(Error::last_os()));
        }
        Ok(())
    }
}

/// Write one authenticated native frame to a live session and wait (bounded)
/// for the receiver to close.
///
/// Errors before the first byte is written are definite failures; any error
/// after that is `WriteUnknown`, because the receiver may have consumed it.
pub fn send_frame(
    config_dir: &str,
    selected: &Selection,
    record: &Record,
    frame: &Map<String, Value>,
) -> Result<()> {
    let pid = record.get("pid").cloned().unwrap_or(Value::Null);
    let current = session_record(config_dir, &pid, selected)?;
    if !value_eq_opt(current.get("sessionId"), record.get("sessionId"))
        || !value_eq_opt(current.get("procStart"), record.get("procStart"))
    {
        return Err(Error::value("target changed"));
    }
    let token = auth_key(config_dir, &current)?;
    let auth = crate::obj! {"type" => "auth", "token" => token};
    let mut data = dumps(&Value::Object(auth)).into_bytes();
    data.push(b'\n');
    data.extend_from_slice(dumps(&Value::Object(frame.clone())).as_bytes());
    data.push(b'\n');
    if data.len() > FRAME_LIMIT {
        return Err(Error::value("native frame too large"));
    }
    let socket = current["messagingSocketPath"]
        .as_str()
        .expect("validated")
        .to_string();
    let conn = Stream::connect(&socket, 3.0)?;
    let peer = crate::identity::peer_pid(conn.raw())?;
    let same_pid = value_eq_opt(Some(&Value::from(peer)), current.get("pid"));
    if !same_pid
        || !value_eq_opt(
            Some(&Value::String(process_start_value(current.get("pid"))?)),
            current.get("procStart"),
        )
    {
        return Err(Error::value("connected peer identity changed"));
    }
    write_and_wait(conn, &data).map_err(|exc| {
        if exc.is_os() {
            Error::write_unknown()
        } else {
            exc
        }
    })
}

fn write_and_wait(mut conn: Stream, data: &[u8]) -> Result<()> {
    conn.send_all(data)?;
    if cfg!(target_os = "macos") {
        std::thread::sleep(Duration::from_millis(150));
    }
    conn.shutdown(libc::SHUT_WR)?;
    let deadline = monotonic() + 3.0;
    let mut buffer = [0u8; 4096];
    while conn.recv(&mut buffer)? > 0 {
        conn.timeout = (deadline - monotonic()).max(0.001);
        if monotonic() >= deadline {
            return Err(Error::timed_out());
        }
    }
    Ok(())
}
