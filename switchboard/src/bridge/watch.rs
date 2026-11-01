//! Best-effort OS hints for verified discovery; never an authority for
//! membership.
//!
//! No registry contents or keys are read. A failure permanently selects the
//! caller's polling fallback; the caller still reconciles periodically and
//! calls `update`. Only verified selected records get content watches (at
//! most 32). Numeric namespace entries are bounded at 256 and tracked by
//! name and inode without reading contents.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::os::fd::RawFd;
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::fsutil;
use crate::identity::monotonic;
use crate::wirejson::dumps;

pub type Hint = Arc<dyn Fn() + Send + Sync>;

const MAX_FILES: usize = 256;

/// A watcher failure as written in the failure reason: an error name and its
/// arguments, such as `FileNotFoundError(2, 'No such file or directory')`.
#[derive(Clone, Debug)]
struct Fault(String);

type FResult<T> = Result<T, Fault>;

fn exception_name(code: i32) -> &'static str {
    match code {
        libc::ENOENT => "FileNotFoundError",
        libc::EACCES | libc::EPERM => "PermissionError",
        libc::ESRCH => "ProcessLookupError",
        libc::EEXIST => "FileExistsError",
        libc::ENOTDIR => "NotADirectoryError",
        libc::EISDIR => "IsADirectoryError",
        libc::EINTR => "InterruptedError",
        libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS => "BlockingIOError",
        libc::ECHILD => "ChildProcessError",
        libc::EPIPE | libc::ESHUTDOWN => "BrokenPipeError",
        libc::ECONNABORTED => "ConnectionAbortedError",
        libc::ECONNREFUSED => "ConnectionRefusedError",
        libc::ECONNRESET => "ConnectionResetError",
        libc::ETIMEDOUT => "TimeoutError",
        _ => "OSError",
    }
}

/// `Name(code, 'message')`, the name chosen by errno (`OSError` for the rest).
fn errno_fault(code: i32, message: &str) -> Fault {
    Fault(format!(
        "{}({}, {})",
        exception_name(code),
        code,
        crate::text::repr_str(message)
    ))
}

#[cfg(target_os = "macos")]
fn last_errno_fault() -> Fault {
    let code = crate::error::Error::last_os();
    errno_fault(code, &crate::error::strerror(code))
}

/// `OSError('message')`, for a failure with no errno.
fn message_fault(message: &str) -> Fault {
    Fault(format!("OSError({})", crate::text::repr_str(message)))
}

type Stamp = Option<(u64, u64, u64, i128, i128)>;

/// The path's own status fields (links not followed), None when it is missing.
fn stamp(path: &str) -> FResult<Stamp> {
    match std::fs::symlink_metadata(path) {
        Ok(info) => Ok(Some((
            info.dev(),
            info.ino(),
            info.size(),
            info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128,
            info.ctime() as i128 * 1_000_000_000 + info.ctime_nsec() as i128,
        ))),
        Err(exc) if exc.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(exc) => {
            let code = exc.raw_os_error().unwrap_or(0);
            Err(errno_fault(code, &crate::error::strerror(code)))
        }
    }
}

type Snapshot = (
    (Stamp, Option<(u64, u64)>),
    Vec<(String, u64)>,
    Vec<(String, Stamp)>,
);
type Trigger = (Stamp, Stamp, Vec<(i64, Stamp)>);

struct Shared {
    config_path: String,
    sessions: String,
    callback: Hint,
    records: Mutex<Vec<(Value, Value)>>,
    dead: Mutex<HashSet<i64>>,
    healthy: AtomicBool,
    failure_reason: Mutex<Option<String>>,
    closed: AtomicBool,
    read_fd: RawFd,
    write_fd: RawFd,
    finished: Mutex<bool>,
    finished_changed: Condvar,
}

impl Shared {
    fn notify(&self) {
        (self.callback)();
    }

    fn fault(&self, error: Option<Fault>) {
        *self.failure_reason.lock().expect("watch") = error.map(|f| f.0);
        self.healthy.store(false, Ordering::SeqCst);
        self.notify();
    }

    fn wake(&self) {
        let byte = b'u';
        // SAFETY: write(2) of one byte to an owned nonblocking pipe.
        unsafe { libc::write(self.write_fd, &byte as *const u8 as *const libc::c_void, 1) };
    }

    fn selected_pids(&self) -> FResult<BTreeSet<i64>> {
        let records = self.records.lock().expect("watch");
        let pids: BTreeSet<i64> = records
            .iter()
            .filter_map(|(pid, _)| crate::wirejson::as_int(pid))
            .filter_map(|digits| digits.parse::<i64>().ok())
            .filter(|pid| *pid > 1)
            .collect();
        if pids.len() > 32 {
            return Err(message_fault("selected process watch capacity exceeded"));
        }
        Ok(pids)
    }

    fn record_path(&self, pid: i64) -> String {
        fsutil::join(&self.sessions, &format!("{pid}.json"))
    }

    fn snapshot(&self) -> FResult<Snapshot> {
        let mut entries = Vec::new();
        match std::fs::read_dir(&self.sessions) {
            Ok(directory) => {
                for entry in directory {
                    let entry = entry.map_err(|exc| {
                        let code = exc.raw_os_error().unwrap_or(0);
                        errno_fault(code, &crate::error::strerror(code))
                    })?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(stem) = name.strip_suffix(".json") {
                        if !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()) {
                            entries.push((name.clone(), entry.ino()));
                            if entries.len() > MAX_FILES {
                                return Err(message_fault(
                                    "registry exceeds bounded namespace snapshot capacity",
                                ));
                            }
                        }
                    }
                }
            }
            Err(exc) if exc.raw_os_error() == Some(libc::ENOENT) => {}
            Err(exc) => {
                let code = exc.raw_os_error().unwrap_or(0);
                return Err(errno_fault(code, &crate::error::strerror(code)));
            }
        }
        entries.sort();
        let mut selected = Vec::new();
        for pid in self.selected_pids()? {
            let path = self.record_path(pid);
            let print = stamp(&path)?;
            selected.push((path, print));
        }
        let sessions = stamp(&self.sessions)?.map(|s| (s.0, s.1));
        Ok(((stamp(&self.config_path)?, sessions), entries, selected))
    }

    fn trigger_stamp(&self) -> FResult<Trigger> {
        let mut selected = Vec::new();
        for pid in self.selected_pids()? {
            selected.push((pid, stamp(&self.record_path(pid))?));
        }
        Ok((stamp(&self.config_path)?, stamp(&self.sessions)?, selected))
    }
}

fn is_dir(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// The nearest existing ancestor directory and its parent.
fn parents(path: &str) -> Vec<String> {
    let mut parent = fsutil::parent(path);
    while !is_dir(&parent) && parent != fsutil::parent(&parent) {
        parent = fsutil::parent(&parent);
    }
    let grand = fsutil::parent(&parent);
    if grand == parent {
        vec![parent]
    } else {
        vec![parent, grand]
    }
}

/// The OS notification backend.
enum Backend {
    #[cfg(target_os = "macos")]
    Kqueue(RawFd),
    #[cfg(target_os = "linux")]
    Inotify(RawFd),
}

impl Backend {
    fn create() -> FResult<Backend> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: kqueue(2) takes no arguments.
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                return Err(last_errno_fault());
            }
            set_cloexec(fd);
            Ok(Backend::Kqueue(fd))
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: inotify_init1 with valid flags.
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                let code = crate::error::Error::last_os();
                return Err(errno_fault(code, "inotify_init1"));
            }
            Ok(Backend::Inotify(fd))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Err(message_fault("OS notification support unavailable"))
        }
    }

    fn fd(&self) -> RawFd {
        match self {
            #[cfg(target_os = "macos")]
            Backend::Kqueue(fd) => *fd,
            #[cfg(target_os = "linux")]
            Backend::Inotify(fd) => *fd,
        }
    }
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on an owned descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
}

fn close_fd(fd: RawFd) {
    // SAFETY: closing a descriptor this module owns.
    unsafe { libc::close(fd) };
}

/// Watch state owned by the watcher thread.
struct Armed {
    backend: Option<Backend>,
    fds: Vec<RawFd>,
    processes: HashMap<RawFd, i64>,
    hint: bool,
    observed_trigger: Option<Trigger>,
}

impl Armed {
    fn clear(&mut self) -> FResult<()> {
        for fd in self.fds.drain(..) {
            close_fd(fd);
        }
        self.processes.clear();
        if let Some(backend) = self.backend.take() {
            close_fd(backend.fd());
            self.backend = Some(Backend::create()?);
        }
        Ok(())
    }

    fn arm(&mut self, shared: &Shared) -> FResult<Snapshot> {
        self.clear()?;
        let snap = shared.snapshot()?;
        let mut paths: BTreeSet<String> = BTreeSet::new();
        paths.extend(parents(&shared.config_path));
        paths.extend(parents(&shared.sessions));
        if is_dir(&shared.sessions) {
            paths.insert(shared.sessions.clone());
        }
        let mut content: BTreeSet<String> = snap
            .2
            .iter()
            .filter(|(_, print)| print.is_some())
            .map(|(path, _)| path.clone())
            .collect();
        if fsutil::exists(&shared.config_path) {
            content.insert(shared.config_path.clone());
        }
        let backend_fd = self.backend.as_ref().map(Backend::fd).unwrap_or(-1);
        #[cfg(target_os = "macos")]
        {
            paths.extend(content.iter().cloned());
            for path in &paths {
                let c = std::ffi::CString::new(path.as_str())
                    .map_err(|_| message_fault("embedded null byte"))?;
                // SAFETY: open(2) with a valid NUL-terminated path.
                let fd = unsafe {
                    libc::open(
                        c.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    )
                };
                if fd < 0 {
                    return Err(last_errno_fault());
                }
                self.fds.push(fd);
                let change = libc::kevent {
                    ident: fd as libc::uintptr_t,
                    filter: libc::EVFILT_VNODE,
                    flags: libc::EV_ADD | libc::EV_CLEAR,
                    fflags: libc::NOTE_WRITE
                        | libc::NOTE_DELETE
                        | libc::NOTE_RENAME
                        | libc::NOTE_ATTRIB
                        | libc::NOTE_REVOKE,
                    data: 0,
                    udata: std::ptr::null_mut(),
                };
                // SAFETY: one valid change, no event buffer.
                let result = unsafe {
                    libc::kevent(
                        backend_fd,
                        &change,
                        1,
                        std::ptr::null_mut(),
                        0,
                        &libc::timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        },
                    )
                };
                if result < 0 {
                    return Err(last_errno_fault());
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            let all: BTreeSet<String> = paths.union(&content).cloned().collect();
            for path in &all {
                let mask: u32 = if content.contains(path) {
                    0x0000_0C0E
                } else {
                    0x0000_0FC0
                };
                let c = std::ffi::CString::new(path.as_str())
                    .map_err(|_| message_fault("embedded null byte"))?;
                // SAFETY: inotify_add_watch with a valid descriptor and path.
                if unsafe { libc::inotify_add_watch(backend_fd, c.as_ptr(), mask) } < 0 {
                    let code = crate::error::Error::last_os();
                    return Err(errno_fault(code, "inotify_add_watch"));
                }
            }
        }
        let _ = backend_fd;
        let pids = shared.selected_pids()?;
        let dead: HashSet<i64> = shared.dead.lock().expect("watch").clone();
        for pid in pids.iter().filter(|pid| !dead.contains(pid)) {
            match self.watch_process(*pid) {
                Ok(()) => {}
                Err(code) if code == libc::ESRCH => {
                    shared.dead.lock().expect("watch").insert(*pid);
                    self.hint = true;
                }
                Err(code) => return Err(errno_fault(code, &crate::error::strerror(code))),
            }
        }
        self.observed_trigger = Some(shared.trigger_stamp()?);
        let current = shared.snapshot()?;
        if current != snap {
            self.hint = true;
        }
        Ok(current)
    }

    /// Register a process-exit watch; Err(errno) on failure.
    fn watch_process(&mut self, pid: i64) -> Result<(), i32> {
        #[cfg(target_os = "macos")]
        {
            let fd = self.backend.as_ref().map(Backend::fd).unwrap_or(-1);
            let change = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one valid change, no event buffer.
            let result = unsafe {
                libc::kevent(
                    fd,
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    &libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    },
                )
            };
            if result < 0 {
                return Err(crate::error::Error::last_os());
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: pidfd_open(pid, 0) via syscall.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as RawFd;
            if fd < 0 {
                return Err(crate::error::Error::last_os());
            }
            set_cloexec(fd);
            self.fds.push(fd);
            self.processes.insert(fd, pid);
            Ok(())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = pid;
            Err(libc::ENOSYS)
        }
    }

    /// Consume ready OS events; true if a selected process exited.
    fn events(&mut self, shared: &Shared, ready: &HashSet<RawFd>) -> FResult<bool> {
        let mut process = false;
        if let Some(backend) = &self.backend {
            let fd = backend.fd();
            if ready.contains(&fd) {
                #[cfg(target_os = "macos")]
                {
                    // SAFETY: kevent zeroed is valid plain data; the buffer
                    // holds 512 events.
                    let mut buffer: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; 512];
                    let count = unsafe {
                        libc::kevent(
                            fd,
                            std::ptr::null(),
                            0,
                            buffer.as_mut_ptr(),
                            512,
                            &libc::timespec {
                                tv_sec: 0,
                                tv_nsec: 0,
                            },
                        )
                    };
                    if count < 0 {
                        return Err(last_errno_fault());
                    }
                    for event in &buffer[..count as usize] {
                        if event.flags & libc::EV_ERROR != 0 {
                            return Err(errno_fault(event.data as i32, "kqueue event error"));
                        }
                        if event.filter == libc::EVFILT_PROC {
                            process = true;
                            shared
                                .dead
                                .lock()
                                .expect("watch")
                                .insert(event.ident as i64);
                        }
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    let mut buffer = vec![0u8; 65536];
                    loop {
                        // SAFETY: read into an owned buffer.
                        let count = unsafe {
                            libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len())
                        };
                        if count < 0 {
                            let code = crate::error::Error::last_os();
                            if code == libc::EAGAIN {
                                break;
                            }
                            return Err(errno_fault(code, &crate::error::strerror(code)));
                        }
                        if count == 0 {
                            return Err(message_fault("inotify closed"));
                        }
                        let data = &buffer[..count as usize];
                        let mut offset = 0;
                        while offset + 16 <= data.len() {
                            let mask = u32::from_ne_bytes(
                                data[offset + 4..offset + 8].try_into().expect("4"),
                            );
                            let size = u32::from_ne_bytes(
                                data[offset + 12..offset + 16].try_into().expect("4"),
                            ) as usize;
                            if mask & (0x4000 | 0x2000) != 0 {
                                return Err(message_fault("inotify overflow or unmount"));
                            }
                            offset += 16 + size;
                        }
                    }
                }
            }
        }
        for (fd, pid) in &self.processes {
            if ready.contains(fd) {
                process = true;
                shared.dead.lock().expect("watch").insert(*pid);
            }
        }
        Ok(process)
    }
}

fn poll_ready(fds: &[RawFd], timeout: Option<f64>) -> FResult<HashSet<RawFd>> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let millis = match timeout {
        None => -1,
        Some(seconds) => (seconds.max(0.0) * 1000.0).ceil().min(i32::MAX as f64) as i32,
    };
    loop {
        // SAFETY: polls is a valid pollfd slice for the call.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let code = crate::error::Error::last_os();
            if code == libc::EINTR {
                continue;
            }
            return Err(errno_fault(code, &crate::error::strerror(code)));
        }
        return Ok(polls
            .iter()
            .filter(|p| p.revents != 0)
            .map(|p| p.fd)
            .collect());
    }
}

fn run(shared: &Shared, armed: &mut Armed) -> FResult<()> {
    let mut snap = armed.arm(shared)?;
    let mut due: Option<f64> = None;
    while !shared.closed.load(Ordering::SeqCst) && shared.healthy.load(Ordering::SeqCst) {
        if armed.hint {
            due = due.or(Some(monotonic() + 0.1));
            armed.hint = false;
        }
        let mut descriptors = vec![shared.read_fd];
        descriptors.extend(armed.processes.keys().copied());
        if let Some(backend) = &armed.backend {
            descriptors.push(backend.fd());
        }
        let mut ready = poll_ready(&descriptors, Some(0.0))?;
        if ready.is_empty() {
            ready = poll_ready(&descriptors, due.map(|d| (d - monotonic()).max(0.0)))?;
        }
        if shared.closed.load(Ordering::SeqCst) {
            break;
        }
        let woke = ready.contains(&shared.read_fd);
        if woke {
            let mut buffer = [0u8; 4096];
            loop {
                // SAFETY: read from the owned nonblocking pipe.
                let count = unsafe {
                    libc::read(
                        shared.read_fd,
                        buffer.as_mut_ptr() as *mut libc::c_void,
                        buffer.len(),
                    )
                };
                if count <= 0 {
                    break;
                }
            }
        }
        if !ready.is_empty() {
            // Consume OS events before rearming: update() may share this wake
            // with a process exit or an overflow.
            let process = armed.events(shared, &ready)?;
            let changed = Some(shared.trigger_stamp()?) != armed.observed_trigger;
            let current = if process || changed || woke {
                armed.arm(shared)?
            } else {
                snap.clone()
            };
            if process || current != snap || armed.hint {
                due = due.or(Some(monotonic() + 0.1));
            }
            armed.hint = false;
            snap = current;
        }
        if due.is_some_and(|d| monotonic() >= d) {
            shared.notify();
            due = None;
        }
    }
    Ok(())
}

/// Best-effort change hints for discovery: calls `callback` when a watched
/// file or directory may have changed.
pub struct DiscoveryWatcher {
    shared: Arc<Shared>,
    started: bool,
}

impl DiscoveryWatcher {
    /// Start watching; construction failures mark the watcher unhealthy
    /// (polling fallback) instead of failing, except for the wake pipe.
    pub fn new(config_path: &str, config_dir: &str, callback: Hint) -> crate::error::Result<Self> {
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: pipe(2) fills two descriptors.
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            return Err(crate::error::Error::errno(crate::error::Error::last_os()));
        }
        for fd in pipe {
            crate::net::set_nonblocking(fd, true);
            set_cloexec(fd);
        }
        let shared = Arc::new(Shared {
            config_path: fsutil::normalize(config_path),
            sessions: fsutil::join(config_dir, "sessions"),
            callback,
            records: Mutex::new(Vec::new()),
            dead: Mutex::new(HashSet::new()),
            healthy: AtomicBool::new(false),
            failure_reason: Mutex::new(None),
            closed: AtomicBool::new(false),
            read_fd: pipe[0],
            write_fd: pipe[1],
            finished: Mutex::new(false),
            finished_changed: Condvar::new(),
        });
        let mut watcher = DiscoveryWatcher {
            shared: shared.clone(),
            started: false,
        };
        match Backend::create() {
            Ok(backend) => {
                shared.healthy.store(true, Ordering::SeqCst);
                let worker = shared.clone();
                let spawned = std::thread::Builder::new()
                    .name("discovery-watch".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        let mut armed = Armed {
                            backend: Some(backend),
                            fds: Vec::new(),
                            processes: HashMap::new(),
                            hint: false,
                            observed_trigger: None,
                        };
                        if let Err(fault) = run(&worker, &mut armed) {
                            if !worker.closed.load(Ordering::SeqCst) {
                                worker.fault(Some(fault));
                            }
                        }
                        for fd in armed.fds.drain(..) {
                            close_fd(fd);
                        }
                        if let Some(backend) = armed.backend.take() {
                            close_fd(backend.fd());
                        }
                        *worker.finished.lock().expect("watch") = true;
                        worker.finished_changed.notify_all();
                    });
                match spawned {
                    Ok(_) => watcher.started = true,
                    Err(exc) => {
                        let code = exc.raw_os_error().unwrap_or(0);
                        shared.fault(Some(errno_fault(code, &crate::error::strerror(code))));
                    }
                }
            }
            Err(fault) => shared.fault(Some(fault)),
        }
        Ok(watcher)
    }

    pub fn healthy(&self) -> bool {
        self.shared.healthy.load(Ordering::SeqCst)
    }

    pub fn failure_reason(&self) -> Option<String> {
        self.shared.failure_reason.lock().expect("watch").clone()
    }

    /// Replace the verified selected records (`(pid, procStart)` pairs).
    pub fn update(&self, records: Vec<(Value, Value)>) {
        {
            let mut current = self.shared.records.lock().expect("watch");
            let key = |pair: &(Value, Value)| (dumps(&pair.0), dumps(&pair.1));
            let old: HashSet<(String, String)> = current.iter().map(key).collect();
            let mut dead = self.shared.dead.lock().expect("watch");
            for pair in &records {
                if !old.contains(&key(pair)) {
                    if let Some(pid) =
                        crate::wirejson::as_int(&pair.0).and_then(|d| d.parse::<i64>().ok())
                    {
                        dead.remove(&pid);
                    }
                }
            }
            *current = records;
        }
        self.shared.wake();
    }

    pub fn close(&mut self) {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shared.wake();
        let mut finished = !self.started;
        if self.started {
            let guard = self.shared.finished.lock().expect("watch");
            let (guard, _) = self
                .shared
                .finished_changed
                .wait_timeout_while(guard, Duration::from_secs(2), |done| !*done)
                .expect("watch");
            finished = *guard;
        }
        // A thread still running keeps its descriptors; never close them under it.
        if finished {
            close_fd(self.shared.read_fd);
            close_fd(self.shared.write_fd);
        }
    }
}

impl Drop for DiscoveryWatcher {
    fn drop(&mut self) {
        self.close();
    }
}

/// Tests against the real kernel notifications (no mocks).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::queue::Flag;
    use serde_json::json;
    use std::path::PathBuf;

    struct Fixture {
        root: PathBuf,
        config: PathBuf,
        sessions: PathBuf,
        event: Arc<Flag>,
        watcher: DiscoveryWatcher,
        notices: Option<Arc<Notices>>,
    }

    /// A fresh flag per notification: `wait` consumes it.
    struct Notices(Mutex<Arc<Flag>>);

    impl Fixture {
        fn new() -> Self {
            let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let root = base.join(format!("sbw-{}", crate::identity::uuid4_hex()));
            let sessions = root.join("sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            let config = root.join("bridge.json");
            std::fs::write(&config, "{}").unwrap();
            let event = Arc::new(Flag::new());
            let notices = Arc::new(Notices(Mutex::new(event.clone())));
            let hint = notices.clone();
            let watcher = DiscoveryWatcher::new(
                &config.to_string_lossy(),
                &root.to_string_lossy(),
                Arc::new(move || hint.0.lock().unwrap().set()),
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(150));
            assert!(watcher.healthy(), "{:?}", watcher.failure_reason());
            let mut fixture = Fixture {
                root,
                config,
                sessions,
                event,
                watcher,
                notices: None,
            };
            fixture.reset(&notices);
            fixture.notices = Some(notices);
            fixture
        }

        fn reset(&mut self, notices: &Arc<Notices>) {
            let fresh = Arc::new(Flag::new());
            *notices.0.lock().unwrap() = fresh.clone();
            self.event = fresh;
        }

        fn wait(&mut self) {
            assert!(
                self.event.wait(Duration::from_secs(2)),
                "notification missing"
            );
            let notices = self.notices.clone().unwrap();
            self.reset(&notices);
        }

        fn quiet(&mut self, seconds: f64) -> bool {
            !self.event.wait(Duration::from_secs_f64(seconds))
        }

        fn settle(&mut self) {
            std::thread::sleep(Duration::from_millis(300));
            let notices = self.notices.clone().unwrap();
            self.reset(&notices);
        }

        fn select(&self, pid: u32) {
            self.watcher.update(vec![(json!(pid), json!("fixture"))]);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.watcher.close();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn atomic_config_and_session_changes() {
        let mut f = Fixture::new();
        let tmp = f.root.join("new");
        std::fs::write(&tmp, r#"{"sessions":[]}"#).unwrap();
        std::fs::rename(&tmp, &f.config).unwrap();
        f.wait();
        let record = f.sessions.join(format!("{}.json", std::process::id()));
        std::fs::write(&record, "{}").unwrap();
        f.wait();
        f.select(std::process::id());
        f.wait();
        std::fs::write(&record, r#"{"status":"busy"}"#).unwrap();
        f.wait();
        std::fs::remove_file(&record).unwrap();
        f.wait();
    }

    #[test]
    fn directory_recreation() {
        let mut f = Fixture::new();
        std::fs::remove_dir(&f.sessions).unwrap();
        f.wait();
        std::fs::create_dir(&f.sessions).unwrap();
        let record = f.sessions.join(format!("{}.json", std::process::id()));
        std::fs::write(&record, "{}").unwrap();
        f.wait();
        f.select(std::process::id());
        f.wait();
        std::fs::write(&record, r#"{"status":"busy"}"#).unwrap();
        f.wait();
    }

    #[test]
    fn key_churn_and_idle() {
        let mut f = Fixture::new();
        std::fs::write(f.sessions.join("98765.secret.key"), "test fixture").unwrap();
        assert!(f.quiet(0.3));
        assert!(f.quiet(0.3));
    }

    #[test]
    fn unselected_and_proxy_content_churn_is_ignored() {
        let mut f = Fixture::new();
        let unselected = f.sessions.join("98765.json");
        let proxy = f.sessions.join("98766.json");
        std::fs::write(&unselected, "{}").unwrap();
        std::fs::write(&proxy, r#"{"peerBridgeProxy":true}"#).unwrap();
        f.wait();
        f.settle();
        for index in 0..20 {
            std::fs::write(&unselected, format!(r#"{{"status":{index}}}"#)).unwrap();
            std::fs::write(
                &proxy,
                format!(r#"{{"peerBridgeProxy":true,"status":{index}}}"#),
            )
            .unwrap();
        }
        assert!(f.quiet(0.3));
        std::fs::write(f.sessions.join("98767.json"), "{}").unwrap();
        f.wait();
        std::fs::write(&f.config, r#"{"sessions":["new-selected-id"]}"#).unwrap();
        f.wait();
    }

    #[test]
    fn partial_record_repair_requires_reconciliation_then_is_watched() {
        let mut f = Fixture::new();
        let record = f.sessions.join(format!("{}.json", std::process::id()));
        std::fs::write(&record, "{").unwrap();
        f.wait();
        std::fs::write(&record, "{}").unwrap();
        assert!(f.quiet(0.2));
        f.select(std::process::id());
        f.wait();
        std::fs::write(&record, r#"{"status":"busy"}"#).unwrap();
        f.wait();
    }

    #[test]
    fn process_exit_without_record_delete() {
        let mut f = Fixture::new();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let record = f.sessions.join(format!("{}.json", child.id()));
        std::fs::write(&record, "{}").unwrap();
        f.wait();
        f.select(child.id());
        f.settle();
        child.kill().unwrap();
        child.wait().unwrap();
        f.wait();
        assert!(record.exists());
        assert!(f.watcher.healthy(), "{:?}", f.watcher.failure_reason());
    }

    #[test]
    fn vanished_process_is_not_a_failure_and_is_not_rewatched() {
        let mut f = Fixture::new();
        f.select(4_000_000);
        f.wait();
        assert!(f.watcher.healthy(), "{:?}", f.watcher.failure_reason());
        assert!(f.watcher.shared.dead.lock().unwrap().contains(&4_000_000));
        f.settle();
        f.select(4_000_000);
        assert!(f.quiet(0.2));
    }

    #[test]
    fn registry_capacity_falls_back() {
        let mut f = Fixture::new();
        for pid in 0..=MAX_FILES {
            std::fs::write(f.sessions.join(format!("{}.json", 100_000 + pid)), "{}").unwrap();
        }
        f.wait();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!f.watcher.healthy());
        assert_eq!(
            f.watcher.failure_reason().as_deref(),
            Some("OSError('registry exceeds bounded namespace snapshot capacity')")
        );
    }

    #[test]
    fn repeated_atomic_session_replacement() {
        let mut f = Fixture::new();
        let record = f.sessions.join("98765.json");
        for number in 0..4 {
            let tmp = f.sessions.join("replacement");
            std::fs::write(&tmp, number.to_string()).unwrap();
            std::fs::rename(&tmp, &record).unwrap();
            f.wait();
        }
        assert!(f.watcher.healthy());
    }

    #[test]
    fn close_is_bounded() {
        let mut f = Fixture::new();
        let started = std::time::Instant::now();
        f.watcher.close();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
