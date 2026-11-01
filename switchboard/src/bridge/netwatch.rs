//! A hint when the machine's network configuration changes, so an offline
//! bridge retries at once instead of waiting out its backoff.
//!
//! macOS: the system configuration daemon's `network_change` notification
//! (libSystem's notify(3), delivered on a file descriptor). Linux: a netlink
//! route socket subscribed to link and address changes only. Both stay silent
//! on a steady network, so the watcher thread sleeps until something really
//! changes. Elsewhere, or if neither can be opened, there is no hint and the
//! backoff's cap bounds the delay.

use std::os::fd::RawFd;
use std::sync::Arc;

pub type Hint = Arc<dyn Fn() + Send + Sync>;

/// The notification name configd posts on any network configuration change
/// (`kNotifySCNetworkChange`).
#[cfg(target_os = "macos")]
const NETWORK_CHANGE: &str = "com.apple.system.config.network_change";

#[cfg(target_os = "macos")]
extern "C" {
    fn notify_register_file_descriptor(
        name: *const libc::c_char,
        notify_fd: *mut libc::c_int,
        flags: libc::c_int,
        out_token: *mut libc::c_int,
    ) -> u32;
    fn notify_cancel(token: libc::c_int) -> u32;
}

pub struct NetWatcher {
    wake_write: RawFd,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The source of change notices: a readable descriptor and how to release it.
struct Source {
    fd: RawFd,
    #[cfg(target_os = "macos")]
    token: libc::c_int,
}

impl Source {
    #[cfg(target_os = "macos")]
    fn open(name: &str) -> Option<Source> {
        let name = std::ffi::CString::new(name).ok()?;
        let (mut fd, mut token) = (-1, -1);
        // SAFETY: a valid NUL-terminated name and two writable integers.
        let status =
            unsafe { notify_register_file_descriptor(name.as_ptr(), &mut fd, 0, &mut token) };
        if status != 0 || fd < 0 {
            return None;
        }
        crate::net::set_nonblocking(fd, true);
        // SAFETY: fcntl on the descriptor notify(3) just created.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        Some(Source { fd, token })
    }

    #[cfg(target_os = "linux")]
    fn open() -> Option<Source> {
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: sockaddr_nl is plain data; all-zero is a valid start.
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        address.nl_groups =
            (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV6_IFADDR) as u32;
        // SAFETY: bind with a valid sockaddr_nl and its size.
        let bound = unsafe {
            libc::bind(
                fd,
                &address as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bound != 0 {
            // SAFETY: closing the descriptor created above.
            unsafe { libc::close(fd) };
            return None;
        }
        Some(Source { fd })
    }

    /// Read every queued notice; true if there was any (or the queue
    /// overflowed, which also means changes happened).
    fn drain(&self) -> bool {
        let mut any = false;
        let mut buffer = [0u8; 8192];
        loop {
            // SAFETY: read into an owned buffer from a nonblocking descriptor.
            let count = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if count > 0 {
                any = true;
                continue;
            }
            if count < 0 {
                let code = crate::error::Error::last_os();
                if code == libc::EINTR {
                    continue;
                }
                // ENOBUFS: the kernel dropped notices because there were many.
                any = any || code == libc::ENOBUFS;
            }
            return any;
        }
    }

    fn close(self) {
        #[cfg(target_os = "macos")]
        // SAFETY: cancelling our own token closes its descriptor.
        unsafe {
            notify_cancel(self.token);
        }
        #[cfg(not(target_os = "macos"))]
        // SAFETY: closing the owned socket.
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl NetWatcher {
    /// Start watching; None where network notices are unavailable.
    pub fn new(hint: Hint) -> Option<NetWatcher> {
        #[cfg(target_os = "macos")]
        {
            Self::start(Source::open(NETWORK_CHANGE)?, hint)
        }
        #[cfg(target_os = "linux")]
        {
            Self::start(Source::open()?, hint)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = hint;
            None
        }
    }

    /// Watch a private notification name instead (tests post it).
    #[cfg(all(test, target_os = "macos"))]
    fn named(name: &str, hint: Hint) -> Option<NetWatcher> {
        Self::start(Source::open(name)?, hint)
    }

    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    fn start(source: Source, hint: Hint) -> Option<NetWatcher> {
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: pipe(2) fills two descriptors.
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            source.close();
            return None;
        }
        for fd in pipe {
            crate::net::set_nonblocking(fd, true);
            // SAFETY: fcntl on a fresh pipe descriptor.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let wake_read = pipe[0];
        let spawned = std::thread::Builder::new()
            .name("switchboard-network".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                loop {
                    let mut fds = [
                        libc::pollfd {
                            fd: source.fd,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: wake_read,
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    // SAFETY: two valid pollfds; no timeout, so it sleeps
                    // until a notice or the close.
                    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                    if ready < 0 {
                        if crate::error::Error::last_os() == libc::EINTR {
                            continue;
                        }
                        break;
                    }
                    if fds[1].revents != 0 {
                        break;
                    }
                    if fds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                        break;
                    }
                    if fds[0].revents != 0 && source.drain() {
                        hint();
                    }
                }
                source.close();
                // SAFETY: the read end belongs to this thread.
                unsafe { libc::close(wake_read) };
            });
        match spawned {
            Ok(thread) => Some(NetWatcher {
                wake_write: pipe[1],
                thread: Some(thread),
            }),
            Err(_) => {
                // The closure (and with it the source and read end) was dropped
                // unrun; release what it would have.
                // SAFETY: closing descriptors created above.
                unsafe {
                    libc::close(pipe[0]);
                    libc::close(pipe[1]);
                }
                None
            }
        }
    }

    /// Stop the thread and release its descriptors.
    pub fn close(&mut self) {
        if let Some(thread) = self.thread.take() {
            let byte = 0u8;
            // SAFETY: one byte to our own pipe.
            unsafe {
                libc::write(
                    self.wake_write,
                    &byte as *const u8 as *const libc::c_void,
                    1,
                )
            };
            let _ = thread.join();
            // SAFETY: the write end is ours and the thread is gone.
            unsafe { libc::close(self.wake_write) };
        }
    }
}

impl Drop for NetWatcher {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn closes_promptly() {
        let started = std::time::Instant::now();
        if let Some(mut watcher) = NetWatcher::new(Arc::new(|| {})) {
            watcher.close();
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_posted_notice_hints_once_per_burst() {
        extern "C" {
            fn notify_post(name: *const libc::c_char) -> u32;
        }
        let name = format!("dev.switchboard.test.{}", crate::identity::uuid4_hex());
        let (sent, seen) = std::sync::mpsc::channel();
        let mut watcher = NetWatcher::named(
            &name,
            Arc::new(move || {
                let _ = sent.send(());
            }),
        )
        .expect("notify(3) registration");
        let c_name = std::ffi::CString::new(name).unwrap();
        // SAFETY: a valid NUL-terminated name.
        assert_eq!(unsafe { notify_post(c_name.as_ptr()) }, 0);
        seen.recv_timeout(Duration::from_secs(5))
            .expect("the hint after a notice");
        // Nothing else arrives without another notice.
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err());
        watcher.close();
    }
}
