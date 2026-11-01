//! `switchboard helper`: command line, stdin/stdout IPC loop, and shutdown.
//!
//! Spawned by the bridge as
//! `switchboard helper --config RUNTIME --route MACHINE/UUID --name NAME
//! --socket-dir DIR [--status idle|busy|waiting|shell|unknown]`.
//! Commands arrive one JSON object per line on stdin; events leave one JSON
//! object per line on stdout. End of stdin, SIGTERM, or SIGINT stops it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::error::{Error, Kind, Result};
use crate::helper::{decode, drain, Emit, Peer};
use crate::net::{poll_fds, pollfd};
use crate::obj;
use crate::protocol::{ACTIVITY_STATES, FRAME_LIMIT};
use crate::{signals, wirejson};

const PROG: &str = "switchboard helper";
const USAGE: &str = "usage: switchboard helper [-h] --config CONFIG --route ROUTE --name NAME\n                          --socket-dir SOCKET_DIR\n                          [--status {idle,busy,waiting,shell,unknown}]\n";
const HELP: &str = "
Native Claude proxy helper. One real process owns each remote peer. Claude
product versions are descriptive registry metadata, not acceptance gates.
Native protocol and process identity checks still apply. The bridge alone
supplies trusted IPC commands; local senders must prove their kernel PID and
match an explicitly enrolled live native registry identity. No shell commands,
settings changes, or native management controls are exposed.

options:
  -h, --help            show this help message and exit
  --config CONFIG
  --route ROUTE
  --name NAME
  --socket-dir SOCKET_DIR
  --status {idle,busy,waiting,shell,unknown}
";

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub config: String,
    pub route: String,
    pub name: String,
    pub socket_dir: String,
    pub status: String,
}

pub enum Parsed {
    Run(Args),
    Help,
    Error(String),
}

const OPTIONS: [&str; 6] = [
    "--config",
    "--route",
    "--name",
    "--socket-dir",
    "--status",
    "--help",
];

fn looks_negative_number(value: &str) -> bool {
    let rest = &value[1..];
    !rest.is_empty()
        && (rest.bytes().all(|b| b.is_ascii_digit())
            || rest.split_once('.').is_some_and(|(a, b)| {
                a.bytes().all(|c| c.is_ascii_digit())
                    && !b.is_empty()
                    && b.bytes().all(|c| c.is_ascii_digit())
            }))
}

/// Whether an argument is an option rather than a value: it starts with `-`,
/// is longer than `-`, isn't a negative number, and has no space.
fn option_like(value: &str) -> bool {
    value.starts_with('-')
        && value.len() > 1
        && !looks_negative_number(value)
        && !value.contains(' ')
}

/// Parse helper arguments: unique long-option prefixes are accepted,
/// `--opt=value` works, the last occurrence wins, and an error exits with
/// status 2.
pub fn parse_args(argv: &[String]) -> Parsed {
    let mut values: [Option<String>; 5] = Default::default();
    let mut extras: Vec<String> = Vec::new();
    let mut index = 0;
    let mut positional_only = false;
    while index < argv.len() {
        let arg = &argv[index];
        index += 1;
        if positional_only || !option_like(arg) {
            if arg == "--" && !positional_only {
                positional_only = true;
                continue;
            }
            extras.push(arg.clone());
            continue;
        }
        if arg == "-h" {
            return Parsed::Help;
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let matches: Vec<&str> = if OPTIONS.contains(&flag) {
            vec![flag]
        } else if flag.starts_with("--") {
            OPTIONS
                .iter()
                .copied()
                .filter(|o| o.starts_with(flag))
                .collect()
        } else {
            vec![]
        };
        let option = match matches.as_slice() {
            [one] => *one,
            [] => {
                extras.push(arg.clone());
                continue;
            }
            many => {
                return Parsed::Error(format!(
                    "ambiguous option: {flag} could match {}",
                    many.join(", ")
                ))
            }
        };
        if option == "--help" {
            return Parsed::Help;
        }
        let value = match inline {
            Some(value) => value,
            None => match argv.get(index) {
                Some(next) if !option_like(next) && next != "--" => {
                    index += 1;
                    next.clone()
                }
                _ => return Parsed::Error(format!("argument {option}: expected one argument")),
            },
        };
        if option == "--status" && !ACTIVITY_STATES.contains(&value.as_str()) {
            return Parsed::Error(format!(
                "argument --status: invalid choice: {} (choose from 'idle', 'busy', 'waiting', 'shell', 'unknown')",
                crate::text::repr_str(&value)
            ));
        }
        let slot = OPTIONS
            .iter()
            .position(|o| *o == option)
            .expect("known option");
        values[slot] = Some(value);
    }
    let missing: Vec<&str> = OPTIONS[..4]
        .iter()
        .zip(&values)
        .filter(|(_, v)| v.is_none())
        .map(|(o, _)| *o)
        .collect();
    if !missing.is_empty() {
        return Parsed::Error(format!(
            "the following arguments are required: {}",
            missing.join(", ")
        ));
    }
    if !extras.is_empty() {
        return Parsed::Error(format!("unrecognized arguments: {}", extras.join(" ")));
    }
    let [config, route, name, socket_dir, status] = values;
    Parsed::Run(Args {
        config: config.unwrap(),
        route: route.unwrap(),
        name: name.unwrap(),
        socket_dir: socket_dir.unwrap(),
        status: status.unwrap_or_else(|| "idle".into()),
    })
}

/// Run the helper; returns the process exit status.
pub fn main(argv: &[String]) -> i32 {
    let args = match parse_args(argv) {
        Parsed::Run(args) => args,
        Parsed::Help => {
            print!("{USAGE}{HELP}");
            return 0;
        }
        Parsed::Error(message) => {
            eprintln!("{USAGE}{PROG}: error: {message}");
            return 2;
        }
    };
    match run(&args) {
        Ok(()) => 0,
        Err(exc) => {
            eprintln!("{PROG}: {}", exc.message);
            1
        }
    }
}

fn stdout_emit() -> Emit {
    let lock = Arc::new(Mutex::new(()));
    Arc::new(move |event: Map<String, Value>| {
        let _guard = lock.lock().unwrap();
        let mut line = wirejson::dumps(&Value::Object(event));
        line.push('\n');
        let mut out = std::io::stdout().lock();
        out.write_all(line.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|exc| Error {
                kind: Kind::Output,
                message: Error::from_io(&exc, None).message,
            })
    })
}

fn run(args: &Args) -> Result<()> {
    let config = crate::config::read_runtime(&args.config)?;
    let emit = stdout_emit();
    let peer = Peer::new(
        &config,
        &args.route,
        &args.name,
        &args.socket_dir,
        emit.clone(),
        &args.status,
    )?;
    let result = serve_ipc(&peer, &emit);
    peer.request_stop();
    let closed = peer.close();
    result.and(closed)
}

fn serve_ipc(peer: &Arc<Peer>, emit: &Emit) -> Result<()> {
    let (main_fd, listener_fd) = peer
        .signal_fds()
        .ok_or_else(|| Error::value("helper closed"))?;
    signals::install(main_fd, listener_fd);
    let listener = Arc::clone(peer);
    std::thread::spawn(move || {
        if let Err(exc) = listener.serve() {
            // The listener thread ends; the helper reports why on stderr.
            eprintln!("{PROG}: listener stopped: {}", exc.message);
        }
    });
    emit(
        obj! {"event" => "ready", "pid" => peer.pid, "sessionId" => peer.sid.clone(), "route" => peer.route.clone()},
    )?;
    let main_read = peer
        .main_read_fd()
        .ok_or_else(|| Error::value("helper closed"))?;
    let stdin = libc::STDIN_FILENO;
    let mut pending: Vec<u8> = Vec::new();
    while !peer.stopped() {
        let mut fds = [pollfd(stdin, libc::POLLIN), pollfd(main_read, libc::POLLIN)];
        poll_fds(&mut fds, None)?;
        if fds[1].revents != 0 {
            drain(main_read);
        }
        if signals::signalled() {
            peer.request_stop();
        }
        if peer.stopped() {
            break;
        }
        if fds[0].revents == 0 {
            continue;
        }
        let mut chunk = vec![0u8; 65536];
        // SAFETY: chunk is a valid writable buffer.
        let count =
            unsafe { libc::read(stdin, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if count < 0 {
            let code = Error::last_os();
            if code == libc::EINTR || code == libc::EAGAIN {
                continue;
            }
            return Err(Error::errno(code));
        }
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..count as usize]);
        while !peer.stopped() {
            let Some(newline) = pending.iter().position(|b| *b == b'\n') else {
                break;
            };
            let mut line: Vec<u8> = pending.drain(..=newline).collect();
            line.truncate(line.len() - 1);
            line.push(b'\n');
            handle_command(peer, emit, &line)?;
        }
        if pending.len() > FRAME_LIMIT {
            return Err(Error::value("IPC input exceeds limit"));
        }
    }
    Ok(())
}

fn handle_command(peer: &Arc<Peer>, emit: &Emit, line: &[u8]) -> Result<()> {
    let mut command = Map::new();
    let result = decode(line).and_then(|decoded| {
        command = decoded;
        match command.get("op").and_then(Value::as_str) {
            Some("deliver") => peer.deliver(&command),
            Some("configure") => peer.configure(&command),
            Some("receipt") => peer.relay_receipt(&command),
            Some("track") => peer.track(&command),
            _ => Err(Error::value("unsupported IPC operation")),
        }
    });
    let exc = match result {
        Ok(()) => return Ok(()),
        Err(exc) if exc.is_handled() => exc,
        Err(exc) => return Err(exc),
    };
    let op = command.get("op").and_then(Value::as_str);
    match (command.get("id"), op) {
        (Some(Value::String(identifier)), Some("deliver")) => {
            peer.status(identifier, "failed", Some(&exc.truncated(256)))
        }
        (_, Some("configure")) => emit(obj! {
            "event" => "configure_rejected",
            "revision" => command.get("revision").cloned().unwrap_or(Value::Null),
            "detail" => exc.truncated(256),
        }),
        _ => emit(obj! {"event" => "rejected", "detail" => "invalid IPC command"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_prefixes_inline_values_and_negative_numbers() {
        let base = [
            "--config",
            "c",
            "--route",
            "r",
            "--name",
            "n",
            "--socket-dir",
            "s",
        ];
        match parse_args(&args(&base)) {
            Parsed::Run(parsed) => assert_eq!(parsed.status, "idle"),
            _ => panic!("expected run"),
        }
        match parse_args(&args(&[
            "--conf=c",
            "--ro",
            "r",
            "--name",
            "-1",
            "--socket-dir",
            "s",
            "--stat",
            "busy",
        ])) {
            Parsed::Run(parsed) => {
                assert_eq!(
                    (
                        parsed.config.as_str(),
                        parsed.name.as_str(),
                        parsed.status.as_str()
                    ),
                    ("c", "-1", "busy")
                )
            }
            _ => panic!("expected run"),
        }
        let error = |list: &[&str]| match parse_args(&args(list)) {
            Parsed::Error(message) => message,
            _ => panic!("expected error"),
        };
        assert_eq!(
            error(&["--s", "x"]),
            "ambiguous option: --s could match --socket-dir, --status"
        );
        assert_eq!(
            error(&["--config"]),
            "argument --config: expected one argument"
        );
        assert_eq!(
            error(&["--config", "-x"]),
            "argument --config: expected one argument"
        );
        assert_eq!(
            error(&["--config", "a"]),
            "the following arguments are required: --route, --name, --socket-dir"
        );
        let mut extra = base.to_vec();
        extra.extend(["extra", "-q"]);
        assert_eq!(error(&extra), "unrecognized arguments: extra -q");
        assert_eq!(
            error(&["--status", "x"]),
            "argument --status: invalid choice: 'x' (choose from 'idle', 'busy', 'waiting', 'shell', 'unknown')"
        );
    }
}
