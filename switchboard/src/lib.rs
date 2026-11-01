//! Switchboard: connects selected Claude Code chats across machines.
//!
//! One binary with roles: `helper` (the per-chat native peer the bridge
//! spawns), `bridge` (the daemon), `cli` (the user commands and the
//! installer), and `server` (the central server).
//!
//! Module layout:
//! - `protocol`: native envelope, addresses, receipt markers, notice text
//! - `discovery`: native session records, auth keys, and live discovery
//! - `identity`: process start tokens, PID domains, kernel peer credentials
//! - `config`: role configuration files
//! - `server`: the central server role, with built-in TLS
//! - `wirejson`, `text`, `fsutil`, `net`: JSON, text, file, and socket
//!   primitives with exact output rules, so every observable byte stays the
//!   same for clients, existing files, and tests
//! - `fmtutil`, `fileops`, `urlparse`: more of the same for the bridge and CLI
//!   (indented JSON, timestamps, temporary files, locks, URL splitting)
//! - `hub_transport`: HTTPS requests to the server over bounded lanes
//! - `invite`: the invite token a new machine joins with
//! - `membership`, `session_metadata`: joined-chat state and display labels
//! - `panics`: which threads a panic may end without ending the process

/// The release version: `SWITCHBOARD_VERSION` at build time (the release
/// workflow sets it from the version tag, without its leading `v`), else the
/// crate version.
pub const VERSION: &str = match option_env!("SWITCHBOARD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// The roles `switchboard ROLE ...` runs; anything else is a command.
pub const ROLES: [&str; 5] = ["helper", "server", "bridge", "cli", "install"];

/// Run `switchboard ARGS...` (the arguments after the program name): a role
/// from [`ROLES`], `--version` alone, or else a command, so `switchboard
/// status` works as `switchboard cli status` does.
pub fn run(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some("--version") if argv.len() == 1 => {
            println!("switchboard {VERSION}");
            0
        }
        Some("helper") => helper_main::main(&argv[1..]),
        Some("server") => server::main(&argv[1..]),
        Some("bridge") => bridge::main(&argv[1..]),
        Some("cli") => cli::main(&argv[1..]),
        Some("install") => cli::install_main(&argv[1..]),
        _ => cli::main(argv),
    }
}

pub mod bridge;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod error;
pub mod fileops;
pub mod fmtutil;
pub mod fsutil;
pub mod helper;
pub mod helper_main;
pub mod hub_transport;
pub mod identity;
pub mod invite;
pub mod membership;
pub mod net;
pub mod panics;
pub mod protocol;
pub mod server;
pub mod session_metadata;
pub mod signals;
pub mod text;
mod unicode_tables;
pub mod urlparse;
pub mod wirejson;
