//! `switchboard <role> [options]`.
//!
//! Roles: `helper` (per-chat native Claude peer, spawned by the bridge),
//! `server` (the central server), `bridge` (the daemon), `cli` (the user
//! commands), and `install` (installs this binary as the user service).
//! Anything else runs the command line, so `switchboard status` works as
//! `switchboard cli status` does. `switchboard --version` prints the version.
//! The dispatch is `switchboard::run`, which `cli` also uses, so the installed
//! wrapper (`switchboard cli "$@"`) reaches every role too.

fn main() {
    switchboard::panics::install_hook();
    let argv: Vec<String> = match std::env::args_os().map(|a| a.into_string()).collect() {
        Ok(argv) => argv,
        Err(_) => {
            eprintln!("switchboard: arguments must be UTF-8");
            std::process::exit(2);
        }
    };
    std::process::exit(switchboard::run(argv.get(1..).unwrap_or(&[])));
}
