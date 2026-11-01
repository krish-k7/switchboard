//! Joining a server: from an invite to a running, connected bridge.
//!
//! Order matters: the invite is read and the server's certificate checked
//! (against the invite's fingerprint, or the system's roots) before any key
//! exists; the name and owner are settled next; then the key is made, the
//! invite redeemed, and the background service installed and started.
//!
//! A refusal the server gives before enrolling anything (a wrong invite, a
//! taken name) removes the key and config this run made, so the machine is
//! back where it started; on a terminal init asks again instead. When the
//! outcome is unknown (the server did not answer) the key stays, and the next
//! run reuses it: the server may have added it.
//!
//! Until the key is made, Esc goes back one question (the invite, the name,
//! the owner), erasing the answers after it.

use std::time::Duration;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::super::ui::prompt::{Problem, Secret, Settle, Stop, Text, Ui};
use super::super::ui::term;
use super::{Facts, Fail, InviteArg, Joined, Options};
use crate::fsutil;
use crate::hub_transport::{HubClient, HubError};
use crate::invite::Invite;

pub const INVITE_VARIABLE: &str = super::super::machines::INVITE_VARIABLE;

/// Pasted text as an invite candidate: white space anywhere (line breaks a
/// chat app added) and quotes or code marks around it are dropped.
pub fn clean_invite(raw: &str) -> String {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .trim_matches(|c| {
            matches!(
                c,
                '`' | '"' | '\'' | '<' | '>' | '*' | '‘' | '’' | '“' | '”'
            )
        })
        .to_string()
}

/// Read pasted text as an invite, or say plainly what is wrong with it.
pub fn read_invite(raw: &str) -> Result<Invite, String> {
    let cleaned = clean_invite(raw);
    if cleaned.is_empty() {
        return Err("Nothing was pasted yet.".into());
    }
    Invite::parse(&cleaned).map_err(|reason| {
        let mut chars = reason.chars();
        let first = chars
            .next()
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_default();
        let sentence = format!("{first}{}", chars.as_str());
        if sentence.starts_with("That is not") {
            "That isn't a Switchboard invite. Invites start with sbi1.".to_string()
        } else {
            format!("{sentence}.")
        }
    })
}

/// The host and port of a server address, for messages.
pub fn host_label(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

fn describe(invite: &Invite) -> String {
    let mut text = format!("An invite for {}", host_label(&invite.url));
    if let Some(machine) = &invite.machine {
        text.push_str(&format!(" (machine {machine})"));
    }
    text
}

fn bad_invite(reason: String) -> Fail {
    Fail::Problem(
        Problem::new("The invite can't be used.")
            .detail(reason)
            .fix("copy the whole invite again (it starts with sbi1.), or make a new one on the server with `sudo switchboard server invite`."),
    )
}

/// Where an invite comes from, for the prompt's hint.
const INVITE_HINT: &str = "The invite stays hidden; line breaks and spaces are fine.\n\nGet one from whoever runs your Switchboard server. They make it with\n`sudo switchboard server invite`\nNo server yet? Set one up on a Linux machine first, with the same install command.";

/// The hint when asking again (the first invite was refused, or a new one).
const NEW_INVITE_HINT: &str = "It stays hidden; line breaks and spaces are fine.\n\nThe server makes one with\n`sudo switchboard server invite`";

/// The invite: from standard input (`--invite` without a terminal),
/// `SWITCHBOARD_INVITE`, or asked for on the terminal; and whether it was
/// asked for.
pub fn obtain_invite(
    ui: &mut Ui,
    options: &Options,
    question: &str,
) -> Result<(Invite, bool), Fail> {
    if options.invite == Some(InviteArg::Ask) && !term::is_tty(0) {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(
            &mut std::io::Read::take(std::io::stdin().lock(), 8192),
            &mut text,
        );
        if crate::text::strip(&text).is_empty() {
            return Err(Fail::Problem(
                Problem::new("No invite on standard input.")
                    .detail("--invite without a value reads the invite from standard input when it isn't a terminal.")
                    .fix("pipe the invite in (`printf '%s\\n' \"$INVITE\" | switchboard init --invite`), or set SWITCHBOARD_INVITE.")
                    .steps(["Set SWITCHBOARD_INVITE, or pipe the invite in:\n`printf '%s\\n' \"$INVITE\" | switchboard init --invite`"]),
            ));
        }
        return Ok((read_invite(&text).map_err(bad_invite)?, false));
    }
    if let Ok(text) = std::env::var(INVITE_VARIABLE) {
        if !crate::text::strip(&text).is_empty() {
            // Keep it out of the commands and services started from here.
            std::env::remove_var(INVITE_VARIABLE);
            return Ok((read_invite(&text).map_err(bad_invite)?, false));
        }
    }
    let check = |raw: &str| read_invite(raw).map(|invite| (clean_invite(raw), describe(&invite)));
    let raw = ui.secret(&Secret {
        question,
        label: "",
        hint: if question == "Paste the invite" {
            INVITE_HINT
        } else {
            NEW_INVITE_HINT
        },
        check: &check,
        flag: "--invite (with the invite on standard input) or SWITCHBOARD_INVITE",
    })?;
    Ok((read_invite(&raw).map_err(bad_invite)?, true))
}

fn unreachable(invite: &Invite, detail: &str) -> Problem {
    let host = host_label(&invite.url);
    Problem::new(format!("Can't reach the server at {host}."))
        .detail(plain_error(detail.trim_start_matches("could not reach the server: ")))
        .fix(format!(
            "check that the server is running and that this machine can reach {host} (a firewall, VPN, or Tailscale being off can block it), then run `switchboard init` again."
        ))
        .code("unreachable")
}

/// `text` without an OS error's `[Errno N] ` prefix.
pub fn strip_errno(text: &str) -> String {
    match text.find("[Errno ") {
        Some(start) => match text[start..].find("] ") {
            Some(end) => format!("{}{}", &text[..start], &text[start + end + 2..]),
            None => text.to_string(),
        },
        None => text.to_string(),
    }
}

/// An OS error without its `[Errno N]` prefix, as a sentence.
pub fn plain_error(text: &str) -> String {
    let text = strip_errno(text);
    let mut chars = text.trim().chars();
    match chars.next() {
        Some(first) => {
            let sentence = format!("{}{}", first.to_uppercase(), chars.as_str());
            if sentence.ends_with('.') {
                sentence
            } else {
                format!("{sentence}.")
            }
        }
        None => String::new(),
    }
}

/// Check the server's certificate before anything is made: it must be the
/// one the invite names, or (without one) trusted by this system.
pub fn check_server(ui: &mut Ui, invite: &Invite) -> Result<(), Fail> {
    let host = host_label(&invite.url);
    ui.step(
        &format!("Checking the server at {host}"),
        |_| match &invite.pin {
            Some(pin) => {
                let der = super::super::machines::server_certificate(&invite.url)
                    .map_err(|error| unreachable(invite, &error.message))?;
                let found = super::super::machines::hex(&Sha256::digest(&der));
                if &found != pin {
                    return Err(Problem::new("The server sent a different certificate than the invite names.")
                        .detail(format!(
                            "The invite expects a certificate whose SHA-256 starts {}, but {host} sent one that starts {}. Either the server's certificate changed since the invite was made, or something is intercepting the connection. Nothing was made on this machine.",
                            &pin[..12], &found[..12]
                        ))
                        .fix("ask for a new invite (on the server: `sudo switchboard server invite`), then run `switchboard init` again.")
                        .steps([
                            "Ask whoever runs the server for a new invite. They make it with\n`sudo switchboard server invite`",
                            super::RERUN,
                        ])
                        .code("pin_mismatch"));
                }
                Ok(true)
            }
            None => {
                let parsed = crate::urlparse::urlsplit(&invite.url)
                    .map_err(|error| unreachable(invite, &error.message))?;
                let name = parsed.hostname().unwrap_or_default();
                let port = parsed.port().ok().flatten().filter(|p| *p != 0).unwrap_or(443) as u16;
                crate::hub_transport::tcp_connect(&name, port)
                    .map_err(|error| unreachable(invite, &error.truncated(200)))?;
                if let Some((der, reason)) = super::super::machines::untrusted_certificate(&invite.url) {
                    return Err(Problem::new("This machine doesn't trust the server's certificate.")
                        .detail(format!(
                            "{host} sent a certificate this system doesn't trust ({reason}; SHA-256 {}). An invite from a server with a private certificate carries its fingerprint, and this one doesn't.",
                            super::super::machines::hex(&Sha256::digest(&der))
                        ))
                        .fix("make a new invite on the server with `sudo switchboard server invite` (it adds the fingerprint), then run `switchboard init` again.")
                        .steps([
                            "Make a new invite on the server, which adds the fingerprint, with\n`sudo switchboard server invite`",
                            super::RERUN,
                        ])
                        .code("untrusted"));
                }
                Ok(false)
            }
        },
        |pinned| {
            Settle::Fact(
                "Server".into(),
                if *pinned {
                    format!("{host}, certificate matches the invite")
                } else {
                    format!("{host}, certificate trusted by this system")
                },
            )
        },
    )
    .map(|_| ())
    .map_err(Fail::Shown)
}

/// What typing a machine name keeps: lowercase, and hyphens for spaces,
/// underscores, and dots, so most names are right as typed.
pub fn name_filter(c: char) -> char {
    match c {
        ' ' | '_' | '.' => '-',
        c => c.to_ascii_lowercase(),
    }
}

/// A valid machine name close to `name`, if one is and it differs.
pub fn suggest_name(name: &str) -> Option<String> {
    normalize_name(name).filter(|out| out != name)
}

/// `name` made into a valid machine name, if it can be.
fn normalize_name(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.chars().map(name_filter) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || (c == '-' && !out.ends_with('-')) {
            out.push(c);
        }
    }
    let out: String = out.trim_matches('-').chars().take(48).collect();
    let out = out.trim_end_matches('-').to_string();
    crate::invite::is_machine_name(&out).then_some(out)
}

/// Why `name` is not a machine name, in words that say how to fix it.
pub fn machine_name_check(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Type a name for this machine.".into());
    }
    let try_this = || {
        suggest_name(name)
            .map(|s| format!(" Try {s}."))
            .unwrap_or_default()
    };
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(match c {
            c if c.is_ascii_uppercase() || " _.".contains(c) => format!(
                "Names use lowercase letters, digits, and hyphens.{}",
                try_this()
            ),
            other => format!(
                "{other} can't be in a name: use lowercase letters, digits, and hyphens.{}",
                try_this()
            ),
        });
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err("Names start with a letter, like box-1.".into());
    }
    if name.len() > 48 {
        return Err(format!(
            "Use at most 48 characters (this is {}).",
            name.len()
        ));
    }
    Ok(())
}

/// Why `owner` cannot be the owner's name.
pub fn owner_check(owner: &str) -> Result<(), String> {
    if crate::text::strip(owner).is_empty() {
        return Err("Type who this machine belongs to.".into());
    }
    if owner.chars().any(|c| c.is_control()) {
        return Err("Use a single line of text.".into());
    }
    if crate::text::char_len(owner) > 64 {
        return Err("Use at most 64 characters.".into());
    }
    Ok(())
}

const NAME_HINT: &str = "Other machines see this name. Lowercase letters, digits, and hyphens.";

/// A free name to offer when `taken` is: with the owner, then a number.
pub fn untaken(taken: &str, owner: &str) -> String {
    let with_owner = normalize_name(&format!("{taken}-{owner}"));
    match with_owner {
        Some(name) if name != taken && !name.ends_with(&format!("-{taken}")) => name,
        _ => format!("{taken}-2"),
    }
}

/// The machine name, and whether it was asked for.
fn choose_name(
    ui: &mut Ui,
    options: &Options,
    invite: &Invite,
    existing: Option<&str>,
    taken: Option<(&str, &str)>,
) -> Result<(String, bool), Fail> {
    if let Some(fixed) = &invite.machine {
        if let Some(given) = options.name.as_deref().filter(|g| *g != fixed) {
            return Err(Fail::Problem(
                Problem::new(format!(
                    "The invite is for the machine name {fixed}, not {given}."
                ))
                .fix("leave out --name, or ask for an invite without a fixed name."),
            ));
        }
        ui.answered("Machine name", &format!("{fixed} (set by the invite)"));
        return Ok((fixed.clone(), false));
    }
    if let (Some(given), None) = (&options.name, taken) {
        machine_name_check(given).map_err(|reason| {
            Fail::Problem(Problem::new(format!("{given} can't be a machine name.")).detail(reason))
        })?;
        ui.answered("Machine name", given);
        return Ok((given.clone(), false));
    }
    let default = existing
        .map(str::to_string)
        .unwrap_or_else(super::super::machines::default_machine);
    if taken.is_none() && (options.yes || !ui.interactive()) {
        ui.answered("Machine name", &default);
        return Ok((default, false));
    }
    let taken_name = taken.map(|(name, _)| name);
    let check = |name: &str| {
        machine_name_check(name)?;
        if Some(name) == taken_name {
            return Err(format!("{name} is taken on this server; choose another."));
        }
        Ok(())
    };
    let filter = name_filter;
    let name = ui.text(&Text {
        question: "Machine name",
        label: "Machine name",
        default: match taken {
            Some((name, owner)) if name == default => untaken(name, owner),
            _ => default,
        },
        hint: NAME_HINT,
        check: &check,
        flag: "--name",
        filter: Some(&filter),
    })?;
    Ok((name, true))
}

fn choose_owner(ui: &mut Ui, options: &Options, existing: Option<&str>) -> Result<String, Fail> {
    if let Some(given) = &options.owner {
        owner_check(given).map_err(|reason| {
            Fail::Problem(Problem::new("The --owner value can't be used.").detail(reason))
        })?;
        ui.answered("Owner", given);
        return Ok(given.clone());
    }
    let default = existing
        .map(str::to_string)
        .unwrap_or_else(super::super::machines::getuser);
    let default = if owner_check(&default).is_ok() {
        default
    } else {
        "me".into()
    };
    if options.yes || !ui.interactive() {
        ui.answered("Owner", &default);
        return Ok(default);
    }
    Ok(ui.text(&Text {
        question: "Owner",
        label: "Owner",
        default,
        hint: "Shown beside the machine's name, so others know whose it is.",
        check: &owner_check,
        flag: "--owner",
        filter: None,
    })?)
}

/// The machine and owner of an existing setup, from its identity.
pub fn existing_identity(config_path: &str) -> Option<(String, String, String)> {
    let config = super::super::local::private_json(config_path).ok()?;
    let cert = config.get("cert")?.as_str()?;
    let identity = fsutil::join(&fsutil::parent(cert), "identity.json");
    let known = super::super::local::private_bytes(&identity)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let field = |key: &str| {
        known
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let url = crate::config::server_url(&config)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((field("machine"), field("owner"), url))
}

/// Remove what a fresh setup made (the server enrolled nothing).
pub fn rollback(config_path: &str) {
    term::critical(|| {
        term::set_changed(false);
        let root = fsutil::parent(config_path);
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_file(fsutil::join(&root, "server-cert.pem"));
        let identity = fsutil::join(&root, "identity");
        if fsutil::lstat(&identity).is_ok_and(|info| info.is_dir()) {
            let _ = std::fs::remove_dir_all(&identity);
        }
        let _ = std::fs::remove_dir(fsutil::join(&root, "state"));
        let _ = std::fs::remove_dir(&root);
    });
}

/// Ask the server to add this machine's key with the invite's secret.
fn enroll(config_path: &str, machine: &str, owner: &str, secret: &str) -> Result<String, Problem> {
    let config = super::super::machines::hub_config(config_path).map_err(|error| {
        Problem::new("This machine's configuration can't be read.").detail(error.message)
    })?;
    let client = HubClient::new(&config).map_err(|error| {
        Problem::new("This machine's key can't be loaded.").detail(error.truncated(200))
    })?;
    let mut body = Map::new();
    body.insert("machine".into(), Value::from(machine));
    body.insert("owner".into(), Value::from(owner));
    body.insert("invite".into(), Value::from(secret));
    let reply = client.request("POST", "/v1/enroll", Some(&Value::Object(body)));
    client.close();
    let host = host_label(&config.hub_url);
    match reply {
        Ok(value) if value.get("state").and_then(Value::as_str) == Some("approved") => Ok(value
            .get("machine")
            .and_then(Value::as_str)
            .unwrap_or(machine)
            .to_string()),
        Ok(_) => Err(Problem::new("The server sent a reply this version doesn't understand.")
            .fix("update Switchboard on the server and here (`switchboard upgrade`).")
            .code("refused")),
        Err(HubError::Http { status, code }) => Err(match code.as_deref() {
            Some("invite_used") => Problem::new("This invite was already used.")
                .fix("ask for a new invite (on the server: `sudo switchboard server invite`), then run `switchboard init` again.")
                .code("invalid_invite"),
            Some("invite_expired") => Problem::new("This invite has expired.")
                .fix("ask for a new invite (on the server: `sudo switchboard server invite`), then run `switchboard init` again.")
                .code("invalid_invite"),
            Some("invalid_invite") => Problem::new("The server didn't accept this invite.")
                .detail("It may have expired, been used already, or been revoked.")
                .fix("ask for a new invite (on the server: `sudo switchboard server invite`), then run `switchboard init` again.")
                .code("invalid_invite"),
            Some("machine_taken") => Problem::new(format!("A machine named {machine} is already on this server."))
                .fix("choose another name: `switchboard init --name ANOTHER-NAME`.")
                .code("machine_taken"),
            Some("rate_limited") => Problem::new("The server is limiting requests from this address.")
                .fix("wait a minute, then run `switchboard init` again.")
                .code("rate_limited"),
            Some("enrollment_unavailable") => Problem::new("The server can't add machines right now.")
                .detail("Its machine registry is unavailable.")
                .fix("check the server's log (`sudo switchboard server logs` on the server), then run `switchboard init` again.")
                .code("refused"),
            Some("invalid_machine") => Problem::new(format!("The server refused the name {machine}."))
                .fix("choose another name with `--name`.")
                .code("invalid_machine"),
            Some("invalid_label") => Problem::new("The server refused the owner name.")
                .detail("Owners are one line of at most 64 characters.")
                .code("invalid_label"),
            other => Problem::new(format!(
                "The server at {host} refused to add this machine ({}).",
                other.map(str::to_string).unwrap_or_else(|| status.to_string())
            ))
            .fix("update Switchboard on the server and here, then run `switchboard init` again.")
            .code("refused"),
        }),
        Err(error) => Err(Problem::new(format!("The server at {host} didn't answer.")).detail(error.truncated(200))
            .fix("check the network and that the server is running, then run `switchboard init` again; it reuses this machine's key.")
            .code("unreachable")),
    }
}

/// Refusals that mean the server added nothing.
fn definitive(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            "invalid_invite"
                | "machine_taken"
                | "invalid_machine"
                | "invalid_label"
                | "rate_limited"
                | "refused"
        )
    )
}

/// Install and start the background service, then wait until the bridge
/// reports a connection to the server. `(started, connected)`.
pub fn start(
    ui: &mut Ui,
    config_path: &str,
    host: &str,
    install: bool,
) -> Result<(bool, bool), Fail> {
    let home = super::super::local::home_dir();
    if super::super::service::is_down(config_path) {
        // Taken down on purpose: init doesn't bring it back up by itself.
        ui.caution(
            &Problem::new("This machine is down (taken offline with `switchboard down`), so the bridge wasn't started.")
                .fix("run `switchboard up` when you want it back online."),
        );
        return Ok((false, false));
    }
    if install {
        ui.step(
            "Installing the bridge's background service",
            |_| {
                super::super::install::install_current(&home).map_err(|error| {
                    Problem::new("The bridge's background service couldn't be installed.")
                        .detail(error.message)
                        .fix("fix that, then run `switchboard init` again; this machine is still on the server.")
                })
            },
            |_| Settle::Quiet,
        )
        .map_err(Fail::Shown)?;
        term::set_changed(true);
    }
    // The bridge watches Claude Code's directory and exits without it.
    let claude_dir = super::super::local::private_json(config_path)
        .ok()
        .and_then(|config| {
            config
                .get("config_dir")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("{home}/.claude"));
    if !fsutil::exists(&claude_dir) {
        ui.caution(
            &Problem::new(format!(
                "Claude Code hasn't run on this machine yet ({} is missing), so the bridge wasn't started.",
                super::super::install::tilde(&claude_dir)
            ))
            .fix("run Claude Code once, then `switchboard up`."),
        );
        return Ok((false, false));
    }
    let connected = ui
        .step(
            "Starting the bridge",
            |progress| {
                // A bridge already running read the old configuration (a
                // server whose certificate changed, say): restart it.
                super::super::service::stop_current().map_err(|error| {
                    Problem::new("The bridge didn't stop, so it couldn't restart with the new setup.")
                        .detail(error.message)
                        .fix("run `switchboard down`, then `switchboard up`.")
                })?;
                super::super::service::service("start", Some(config_path)).map_err(|error| {
                    Problem::new("The bridge didn't start.")
                        .detail(error.message)
                        .fix("run `switchboard doctor` to see why, then `switchboard up`.")
                })?;
                let began = std::time::Instant::now();
                while began.elapsed() < Duration::from_secs(30) {
                    let status =
                        super::super::local::private_json(config_path).and_then(|config| {
                            super::super::local::runtime_status(&config, config_path)
                        });
                    if let Ok(status) = status {
                        if status.get("connected") == Some(&Value::Bool(true)) {
                            return Ok(true);
                        }
                    }
                    progress.set(format!(
                        "Starting the bridge and waiting for it to connect to {host} ({} s)",
                        began.elapsed().as_secs()
                    ));
                    std::thread::sleep(Duration::from_millis(250));
                }
                Ok(false)
            },
            |connected| {
                if *connected {
                    Settle::Done(
                        "Started the bridge, the background service that connects this machine's chats"
                            .into(),
                    )
                } else {
                    Settle::Warn(
                        "Started the bridge (the background service that connects this machine's chats), but it hasn't connected to the server yet".into(),
                    )
                }
            },
        )
        .map_err(Fail::Shown)?;
    Ok((true, connected))
}

/// An invite init already has: the new server's first one, or one read
/// and checked before the current setup was moved aside.
pub struct Given {
    pub invite: Invite,
    /// The server's certificate was already checked.
    pub checked: bool,
}

/// Which question the join is on, for going back.
enum Stage {
    Invite,
    Name,
    Owner,
}

/// Join with `given`, or with an invite from the options or the terminal.
/// `back`: Esc at the first question asked goes back to the caller's.
pub fn run(
    ui: &mut Ui,
    options: &Options,
    _facts: &Facts,
    given: Option<Given>,
    back: bool,
    summary: &mut Map<String, Value>,
) -> Result<Joined, Fail> {
    ui.label_width = super::LABEL_WIDTH;
    let config_path = fsutil::absolute(&options.config).map_err(|error| {
        Fail::Problem(Problem::new("The configuration path can't be used.").detail(error.message))
    })?;
    let existing = existing_identity(&config_path);
    let existing_machine = existing
        .as_ref()
        .map(|e| e.0.clone())
        .filter(|m| !m.is_empty());
    let existing_owner = existing
        .as_ref()
        .map(|e| e.1.clone())
        .filter(|o| !o.is_empty());
    let can_go_back = ui.can_go_back();
    let mut invite: Option<Invite> = None;
    let mut stage = Stage::Invite;
    if let Some(given) = given {
        if !given.checked {
            check_server(ui, &given.invite)?;
        }
        invite = Some(given.invite);
        stage = Stage::Name;
    }
    let (mut invite_asked, mut name_asked) = (false, false);
    let (mut at_invite, mut at_name) = (0, 0);
    let mut machine = String::new();
    let owner;
    loop {
        match stage {
            Stage::Invite => {
                at_invite = ui.checkpoint();
                ui.back = back && can_go_back;
                let obtained = obtain_invite(ui, options, "Paste the invite");
                ui.back = false;
                let (next, asked) = obtained?;
                invite_asked = asked;
                if let Some((_, _, url)) = &existing {
                    if !url.is_empty() && url.trim_end_matches('/') != next.url {
                        return Err(Fail::Problem(
                            Problem::new(format!("This machine is set up for the server at {url}, not {}.", next.url))
                                .fix("run `switchboard init` in a terminal and choose to join a different server, or pass --replace."),
                        ));
                    }
                }
                check_server(ui, &next)?;
                invite = Some(next);
                stage = Stage::Name;
            }
            Stage::Name => {
                at_name = ui.checkpoint();
                ui.back = (invite_asked || back) && can_go_back;
                // Back at this question, the name typed before is the default.
                let earlier = (!machine.is_empty()).then(|| machine.clone());
                let chosen = choose_name(
                    ui,
                    options,
                    invite.as_ref().expect("an invite"),
                    earlier.as_deref().or(existing_machine.as_deref()),
                    None,
                );
                ui.back = false;
                match chosen {
                    Ok((name, asked)) => {
                        machine = name;
                        name_asked = asked;
                        stage = Stage::Owner;
                    }
                    Err(Fail::Stop(Stop::Back)) if invite_asked => {
                        ui.rewind(at_invite);
                        stage = Stage::Invite;
                    }
                    Err(other) => return Err(other),
                }
            }
            Stage::Owner => {
                ui.back = (name_asked || invite_asked || back) && can_go_back;
                let chosen = choose_owner(ui, options, existing_owner.as_deref());
                ui.back = false;
                match chosen {
                    Ok(chosen) => {
                        owner = chosen;
                        break;
                    }
                    Err(Fail::Stop(Stop::Back)) if name_asked => {
                        ui.rewind(at_name);
                        stage = Stage::Name;
                    }
                    Err(Fail::Stop(Stop::Back)) if invite_asked => {
                        ui.rewind(at_invite);
                        stage = Stage::Invite;
                    }
                    Err(other) => return Err(other),
                }
            }
        }
    }
    let mut invite = invite.expect("an invite");
    let host = host_label(&invite.url);
    loop {
        let fresh = fsutil::lstat(&config_path).is_err();
        ui.step(
            "Creating this machine's key",
            |_| {
                term::critical(|| {
                    if fresh {
                        term::set_changed(true);
                    }
                    super::super::machines::prepare(
                        &config_path,
                        Some(&machine),
                        Some(&owner),
                        &invite.url,
                        invite.pin.as_deref(),
                        true,
                    )
                })
                .map_err(|error| {
                    Problem::new("This machine's key couldn't be made.")
                        .detail(error.message)
                        .fix("fix that, then run `switchboard init` again.")
                })
            },
            |_| Settle::Quiet,
        )
        .map_err(Fail::Shown)?;
        if fresh {
            term::set_changed(true);
        }
        let asks_again = ui.interactive();
        let fixed_name = invite.machine.is_some();
        let joined = ui.step(
            "Joining the server",
            |_| {
                enroll(&config_path, &machine, &owner, &invite.secret).map_err(|problem| {
                    // On a terminal the next question is the fix.
                    match problem.code {
                        Some("invalid_invite") if asks_again => problem.without_fix(),
                        Some("machine_taken") if asks_again && !fixed_name => problem.without_fix(),
                        _ => problem,
                    }
                })
            },
            |name| Settle::Done(format!("Joined the server as {name}")),
        );
        match joined {
            Ok(name) => {
                machine = name;
                break;
            }
            Err(problem) if ui.interactive() && problem.code == Some("invalid_invite") => {
                if fresh {
                    rollback(&config_path);
                    term::set_changed(false);
                }
                let (next, _) = obtain_invite(ui, &Options::default(), "Paste a new invite")?;
                if next.url != invite.url || next.pin != invite.pin {
                    check_server(ui, &next)?;
                }
                if let Some(fixed) = &next.machine {
                    machine = fixed.clone();
                }
                invite = next;
            }
            Err(problem)
                if ui.interactive()
                    && problem.code == Some("machine_taken")
                    && invite.machine.is_none() =>
            {
                if fresh {
                    rollback(&config_path);
                    term::set_changed(false);
                }
                let taken = machine.clone();
                machine = choose_name(
                    ui,
                    &Options::default(),
                    &invite,
                    Some(&taken),
                    Some((&taken, &owner)),
                )?
                .0;
            }
            Err(problem) => {
                if fresh && definitive(problem.code) {
                    rollback(&config_path);
                    term::set_changed(false);
                    ui.blank();
                    ui.say(
                        "Nothing was left on this machine.",
                        super::super::ui::Tone::Normal,
                    );
                }
                return Err(Fail::Shown(problem));
            }
        }
    }
    let (started, connected) = if options.no_start {
        ui.say(
            "Didn't install or start the bridge (--no-start).",
            super::super::ui::Tone::Normal,
        );
        (false, false)
    } else {
        start(ui, &config_path, &host, true)?
    };
    summary.insert("machine".into(), Value::from(machine.clone()));
    summary.insert("owner".into(), Value::from(owner));
    summary.insert("server_url".into(), Value::from(invite.url.clone()));
    summary.insert("started".into(), Value::Bool(started));
    summary.insert("connected".into(), Value::Bool(connected));
    Ok(Joined {
        machine,
        server_url: invite.url,
        started,
        connected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> String {
        Invite {
            url: "https://10.0.0.5:8743".into(),
            pin: Some("ab".repeat(32)),
            secret: crate::invite::new_secret(),
            machine: None,
        }
        .encode()
    }

    #[test]
    fn pasted_invites_survive_chat_apps() {
        let token = token();
        let (head, tail) = token.split_at(20);
        for pasted in [
            token.clone(),
            format!("  {token}\n"),
            format!("{head}\n{tail}"),
            format!("{head}\r\n  {tail}"),
            format!("`{token}`"),
            format!("\"{token}\""),
            format!("```\n{token}\n```"),
        ] {
            assert_eq!(
                read_invite(&pasted).unwrap().url,
                "https://10.0.0.5:8743",
                "{pasted:?}"
            );
        }
    }

    #[test]
    fn bad_invites_say_what_is_wrong() {
        assert_eq!(read_invite("  ").unwrap_err(), "Nothing was pasted yet.");
        assert_eq!(
            read_invite("hello").unwrap_err(),
            "That isn't a Switchboard invite. Invites start with sbi1."
        );
        let token = token();
        assert_eq!(
            read_invite(&token[..token.len() - 5]).unwrap_err(),
            "The invite is damaged; copy it again in full."
        );
    }

    #[test]
    fn machine_names_explain_themselves() {
        assert_eq!(machine_name_check("krish-mbp"), Ok(()));
        assert_eq!(
            machine_name_check(""),
            Err("Type a name for this machine.".into())
        );
        assert_eq!(
            machine_name_check("My Box"),
            Err("Names use lowercase letters, digits, and hyphens. Try my-box.".into())
        );
        assert_eq!(
            machine_name_check("my_box"),
            Err("Names use lowercase letters, digits, and hyphens. Try my-box.".into())
        );
        assert_eq!(
            machine_name_check("box!"),
            Err(
                "! can't be in a name: use lowercase letters, digits, and hyphens. Try box.".into()
            )
        );
        assert_eq!(
            machine_name_check("2box"),
            Err("Names start with a letter, like box-1.".into())
        );
        assert_eq!(
            machine_name_check("-box"),
            Err("Names start with a letter, like box-1.".into())
        );
        assert!(machine_name_check(&"a".repeat(49))
            .unwrap_err()
            .contains("48"));
        // Whatever the check accepts, the server accepts.
        for name in ["a", "a-1", "abc-def-9", &"b".repeat(48)] {
            assert!(machine_name_check(name).is_ok() && crate::invite::is_machine_name(name));
        }
    }

    #[test]
    fn typed_names_are_made_valid_as_they_are_typed() {
        let typed: String = "My Box_2.local".chars().map(name_filter).collect();
        assert_eq!(typed, "my-box-2-local");
        assert_eq!(machine_name_check(&typed), Ok(()));
        assert_eq!(
            suggest_name("Krish's MacBook"),
            Some("krishs-macbook".into())
        );
        assert_eq!(suggest_name("ok"), None);
        assert_eq!(suggest_name("123"), None);
    }

    #[test]
    fn a_taken_name_suggests_one_with_the_owner_first() {
        assert_eq!(untaken("kmbp", "krish"), "kmbp-krish");
        assert_eq!(untaken("kmbp", "Krish K"), "kmbp-krish-k");
        assert_eq!(untaken("kmbp", "!!"), "kmbp-2");
        assert_eq!(untaken("box-krish", "krish"), "box-krish-krish");
    }

    #[test]
    fn os_errors_read_as_sentences() {
        assert_eq!(
            plain_error("[Errno 61] Connection refused"),
            "Connection refused."
        );
        assert_eq!(plain_error("timed out"), "Timed out.");
    }

    #[test]
    fn owners_are_one_short_line() {
        assert_eq!(owner_check("krish"), Ok(()));
        assert!(owner_check("  ").is_err());
        assert!(owner_check("a\nb").is_err());
        assert!(owner_check(&"x".repeat(65)).is_err());
    }

    #[test]
    fn only_refusals_before_enrollment_remove_the_new_key() {
        assert!(definitive(Some("invalid_invite")));
        assert!(definitive(Some("machine_taken")));
        assert!(!definitive(Some("unreachable")));
        assert!(!definitive(None));
    }
}
