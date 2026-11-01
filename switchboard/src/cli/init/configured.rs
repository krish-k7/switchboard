//! Rerunning init on a machine that is already set up: where it stands, and
//! the actions that make sense for it (join again with a new invite, start
//! the bridge, add the plugin, join a different server, run or invite for
//! the server here, leave the server), instead of failing.
//!
//! Joining a different server moves the current setup aside (the bridge
//! stopped first) and puts it back if joining fails, so the machine is never
//! left half set up. The invite and the new server are checked before
//! anything moves. Leaving stops the bridge, removes its background
//! service (so it doesn't start again at the next login), and moves the
//! setup aside; the setup itself is kept. Switching keeps the service: it
//! runs the new setup from the same path.

use std::time::Duration;

use serde_json::{Map, Value};

use super::super::ui::prompt::{Choice, Confirm, Problem, Select, Settle, Ui};
use super::super::ui::{term, Mark, Tone};
use super::{join, plugin, server, Facts, Fail, Joined, Options};
use crate::fsutil;
use crate::hub_transport::{HubClient, HubError};

/// The width this machine's state pads its labels to: "Server", "Bridge",
/// and "Plugin".
pub const LABEL_WIDTH: usize = 6;

/// How the server sees this machine's key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    Enrolled { machine: String, owner: String },
    Refused,
    Unreachable(String),
}

/// An authenticated `GET /health` within eight seconds.
pub fn standing(config_path: &str) -> Standing {
    let config = match super::super::machines::hub_config(config_path) {
        Ok(config) => config,
        Err(error) => return Standing::Unreachable(error.message),
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match HubClient::with_limit(&config, 65536) {
            Ok(client) => {
                let reply = client.request("GET", "/health", None);
                client.close();
                match reply {
                    Ok(value) => {
                        let text = |k: &str| {
                            value
                                .get(k)
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        };
                        Standing::Enrolled {
                            machine: text("machine"),
                            owner: text("owner"),
                        }
                    }
                    Err(HubError::Http { status, .. }) if status == 401 || status == 403 => {
                        Standing::Refused
                    }
                    Err(error) => Standing::Unreachable(error.truncated(200)),
                }
            }
            Err(error) => Standing::Unreachable(error.truncated(200)),
        };
        let _ = sender.send(outcome);
    });
    receiver
        .recv_timeout(Duration::from_secs(8))
        .unwrap_or_else(|_| {
            Standing::Unreachable("the server did not answer within 8 seconds".into())
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Rejoin,
    Start,
    Plugin,
    Switch,
    Server,
    Invite,
    Leave,
    Nothing,
}

/// The rerun on a set-up machine.
pub fn run(
    ui: &mut Ui,
    options: &Options,
    facts: &Facts,
    summary: &mut Map<String, Value>,
) -> Result<(), Fail> {
    ui.label_width = LABEL_WIDTH;
    let config_path = fsutil::absolute(&options.config).map_err(|e| {
        Fail::Problem(Problem::new("The configuration path can't be used.").detail(e.message))
    })?;
    let Some((machine, owner, url)) = join::existing_identity(&config_path) else {
        return Err(Fail::Problem(
            Problem::new("This machine's Switchboard configuration can't be read.")
                .detail(format!(
                    "{} exists but is unsafe or damaged.",
                    super::super::install::tilde(&config_path)
                ))
                .fix("run `switchboard doctor` to see what is wrong with it."),
        ));
    };
    let host = join::host_label(&url);
    summary.insert("role".into(), Value::from("configured"));
    let standing = ui
        .step(
            &format!("Checking the connection to {host}"),
            |_| Ok(standing(&config_path)),
            |_| Settle::Quiet,
        )
        .map_err(Fail::Shown)?;
    let shown_machine = match &standing {
        Standing::Enrolled { machine, .. } if !machine.is_empty() => machine.clone(),
        _ => machine.clone(),
    };
    match &standing {
        Standing::Enrolled { owner: o, .. } => ui.fact(
            Mark::Done,
            "Server",
            &format!(
                "{host}, joined as {shown_machine} (owner {})",
                if o.is_empty() { &owner } else { o }
            ),
        ),
        Standing::Refused => {
            ui.fact(
                Mark::Warned,
                "Server",
                &format!("{host} doesn't accept this machine's key"),
            );
            ui.fact(
                Mark::Blank,
                "",
                "It was removed from the server, or an earlier setup stopped before the server accepted it.",
            );
        }
        Standing::Unreachable(reason) if reason.contains("invalid peer certificate") => {
            ui.fact(
                Mark::Warned,
                "Server",
                &format!("{host} sent a certificate this machine doesn't trust"),
            );
            ui.fact(
                Mark::Blank,
                "",
                "The server's certificate may have changed. Join again with a new invite from it.",
            );
        }
        Standing::Unreachable(reason) => {
            ui.fact(Mark::Warned, "Server", &format!("can't reach {host}"));
            ui.fact(Mark::Blank, "", &join::plain_error(reason));
        }
    }
    let runtime = super::super::local::private_json(&config_path)
        .and_then(|config| super::super::local::runtime_status(&config, &config_path));
    let (running, connected) = match &runtime {
        Ok(status) => (true, status.get("connected") == Some(&Value::Bool(true))),
        Err(_) => (false, false),
    };
    let down = !running && super::super::service::is_down(&config_path);
    match (running, connected) {
        (true, true) => ui.fact(Mark::Done, "Bridge", "running and connected"),
        (true, false) => ui.fact(Mark::Warned, "Bridge", "running, but not connected"),
        _ if down => ui.fact(
            Mark::Blank,
            "Bridge",
            "down: taken offline with switchboard down",
        ),
        _ => ui.fact(Mark::Warned, "Bridge", "not running"),
    }
    let claude = plugin::claude_path();
    let has_plugin = match &claude {
        Some(claude) => {
            let found = ui
                .step(
                    "Looking for the Claude Code plugin",
                    |_| Ok(plugin::installed(claude)),
                    |_| Settle::Quiet,
                )
                .map_err(Fail::Shown)?;
            let present = found == Ok(true);
            if present {
                ui.fact(Mark::Done, "Plugin", "installed");
            } else {
                ui.fact(Mark::Warned, "Plugin", "not installed");
            }
            Some(present)
        }
        None => None,
    };
    if facts.server_here {
        ui.done("This machine also runs the server");
    }
    summary.insert("machine".into(), Value::from(shown_machine.clone()));
    summary.insert("server_url".into(), Value::from(url.clone()));
    summary.insert(
        "enrolled".into(),
        Value::Bool(matches!(standing, Standing::Enrolled { .. })),
    );
    summary.insert("running".into(), Value::Bool(running));
    summary.insert("connected".into(), Value::Bool(connected));
    if let Some(present) = has_plugin {
        summary.insert(
            "plugin".into(),
            Value::from(if present { "installed" } else { "missing" }),
        );
    }
    let refused = standing == Standing::Refused;
    let unreachable = matches!(standing, Standing::Unreachable(_));
    let enrolled = matches!(standing, Standing::Enrolled { .. });

    // Without a terminal, the flags decide.
    let action = if options.invite.is_some() {
        Action::Switch
    } else if options.plugin == Some(true) && has_plugin == Some(false) {
        Action::Plugin
    } else if options.server && facts.server_unavailable().is_none() && !facts.server_here {
        Action::Server
    } else if !ui.interactive() || options.yes {
        Action::Nothing
    } else {
        let mut actions: Vec<(Action, Choice)> = Vec::new();
        if refused || unreachable {
            actions.push((
                Action::Rejoin,
                Choice::new(
                    "Join again with a new invite",
                    if refused {
                        "Keeps this machine's key and name; the server needs a new invite because it doesn't accept the key now."
                    } else {
                        "For a server whose certificate changed: a new invite carries the new one. Keeps this machine's key and name."
                    },
                ),
            ));
        }
        if enrolled && !running {
            actions.push((
                Action::Start,
                if down {
                    Choice::new(
                        "Bring this machine back up",
                        "Starts the bridge, and lets it start at login again, as `switchboard up` does.",
                    )
                } else {
                    Choice::new(
                        "Start the bridge",
                        "Connects this machine's joined chats again.",
                    )
                },
            ));
        }
        if has_plugin == Some(false) {
            actions.push((
                Action::Plugin,
                Choice::new(
                    "Install the Claude Code plugin",
                    "Adds /switchboard commands to every Claude Code chat.",
                ),
            ));
        }
        actions.push((
            Action::Switch,
            Choice::new(
                "Join a different server",
                "With an invite from another server. The current setup is moved aside, and put back if joining fails.",
            ),
        ));
        if facts.server_here {
            actions.push((
                Action::Invite,
                Choice::new(
                    "Make an invite for another machine",
                    "Runs `sudo switchboard server invite` here.",
                ),
            ));
        } else if facts.server_unavailable().is_none() && facts.stub.is_none() {
            actions.push((
                Action::Server,
                Choice::new(
                    "Also run the server here",
                    "Machines join it with invites you make here.",
                ),
            ));
        }
        actions.push((
            Action::Leave,
            Choice::new(
                "Leave this server",
                "Stops the bridge, removes its background service, and moves this machine's setup aside (kept, not deleted).",
            ),
        ));
        actions.push((Action::Nothing, Choice::new("Keep it as it is", "")));
        let healthy = enrolled && connected && has_plugin != Some(false);
        let kinds: Vec<Action> = actions.iter().map(|(action, _)| *action).collect();
        let default = default_choice(&kinds, healthy, down);
        ui.blank();
        let chosen = ui.select(&Select {
            question: "What would you like to do?",
            label: "",
            choices: actions.iter().map(|(_, c)| c.clone()).collect(),
            default,
            flag: "--invite, --plugin, or --server",
        })?;
        actions[chosen].0
    };
    match action {
        Action::Nothing => {
            ui.blank();
            if enrolled && connected {
                ui.say("Everything is set up.", Tone::Strong);
                connect_chats(ui);
            } else {
                ui.say("Nothing was changed.", Tone::Strong);
                ui.heading("Next");
                ui.steps(&["For details and fixes, run\n`switchboard doctor`".into()]);
            }
            Ok(())
        }
        Action::Start => {
            if down {
                super::super::service::allow_up(&config_path).map_err(|error| {
                    Fail::Problem(
                        Problem::new("This machine couldn't be brought back up.")
                            .detail(error.message)
                            .fix("run `switchboard up`."),
                    )
                })?;
            }
            let install = !super::super::service::installed();
            let (started, connected) = join::start(ui, &config_path, &host, install)?;
            ui.blank();
            if connected {
                ui.say("Done. The bridge is running and connected.", Tone::Strong);
                connect_chats(ui);
            } else if started {
                ui.say(
                    "Almost done. The bridge is running but not connected yet.",
                    Tone::Strong,
                );
                ui.heading("Next");
                ui.steps(&["See why with\n`switchboard doctor`".into()]);
            }
            Ok(())
        }
        Action::Plugin => {
            let forced = Options {
                plugin: Some(true),
                ..options.clone()
            };
            plugin::offer(ui, &forced, summary)?;
            Ok(())
        }
        Action::Invite => make_invite(ui),
        Action::Leave => leave(ui, &config_path, &host, &shown_machine, summary),
        Action::Server => {
            let done = server::run(ui, options, facts, false, summary)?;
            if let Some(invite) = &done.invite {
                super::show_invite(ui, invite, done.invite_expires_at, false, done.stub);
                summary.insert("invite".into(), Value::from(invite.clone()));
            }
            Ok(())
        }
        Action::Rejoin | Action::Switch => {
            ui.blank();
            let (invite, _) = join::obtain_invite(ui, options, "Paste the new invite")?;
            let same = invite.url == url.trim_end_matches('/');
            let joined = if same {
                let reuse = Options {
                    name: options
                        .name
                        .clone()
                        .or_else(|| (!machine.is_empty()).then(|| machine.clone())),
                    owner: options
                        .owner
                        .clone()
                        .or_else(|| (!owner.is_empty()).then(|| owner.clone())),
                    yes: true,
                    ..options.clone()
                };
                let given = join::Given {
                    invite,
                    checked: false,
                };
                join::run(ui, &reuse, facts, Some(given), false, summary)?
            } else {
                if !options.replace && !ui.interactive() {
                    return Err(Fail::Problem(
                        Problem::new(format!("This machine is set up for the server at {url}, not {}.", invite.url))
                            .fix("pass --replace to move the current setup aside and join the new server, or run `switchboard init` in a terminal."),
                    ));
                }
                if ui.interactive()
                    && !options.replace
                    && !options.yes
                    && !ui.confirm(&Confirm {
                        question: &format!(
                            "Leave {host} and join {}?",
                            join::host_label(&invite.url)
                        ),
                        yes: "Switch servers",
                        yes_detail: "The current setup is moved aside (not deleted), and put back if joining fails.",
                        no: "Keep the current setup",
                        no_detail: "Nothing changes.",
                        default_yes: true,
                        flag: "--replace",
                        declined: None,
                    })?
                {
                    return Err(Fail::Declined("Kept the current setup.".into()));
                }
                switch(ui, options, facts, &config_path, invite, summary)?
            };
            let mut state = plugin::offer(ui, options, summary)?;
            if state == plugin::Plugin::Absent && has_plugin == Some(true) {
                state = plugin::Plugin::Present;
            }
            super::path::offer(ui, options, summary)?;
            super::finish_client(ui, &joined, state, None);
            Ok(())
        }
    }
}

/// The action Enter picks: keeping things as they are for a healthy machine,
/// and for one taken down on purpose (bringing it back up is a choice, never
/// the default); otherwise the first fix offered.
fn default_choice(actions: &[Action], healthy: bool, down: bool) -> usize {
    let keep = actions.iter().position(|action| *action == Action::Nothing);
    match keep {
        Some(index) if healthy || down => index,
        _ => 0,
    }
}

/// What Switchboard keeps beside its configuration.
fn ours(name: &str) -> bool {
    // The configuration and membership files, their locks, and the
    // temporary files they are written through.
    matches!(name, "identity" | "state" | "server-cert.pem" | "down")
        || name.trim_start_matches('.').starts_with("bridge.json")
        || name.trim_start_matches('.').starts_with("membership.json")
}

/// The directory that holds this machine's setup, when it holds nothing
/// else (as `~/.config/switchboard` does), so moving it aside moves only
/// Switchboard's files.
fn setup_dir(config_path: &str) -> Result<String, Fail> {
    let root = fsutil::parent(config_path);
    let only_ours = std::fs::read_dir(&root).is_ok_and(|entries| {
        entries
            .filter_map(|entry| entry.ok())
            .all(|entry| ours(&entry.file_name().to_string_lossy()))
    });
    if only_ours && root != super::super::local::home_dir() {
        return Ok(root);
    }
    Err(Fail::Problem(
        Problem::new("This machine's setup isn't in a directory of its own, so it can't be moved aside.")
            .detail(format!(
                "The configuration is {}; its directory may hold other files.",
                super::super::install::tilde(config_path)
            ))
            .fix("move the configuration, its identity directory, and its state directory aside yourself."),
    ))
}

/// Leave the server: stop the bridge, move this machine's setup aside, and
/// remove the bridge's background service when it runs this setup, so it
/// doesn't start again at login without a configuration. The server keeps
/// listing the machine until it is revoked there.
fn leave(
    ui: &mut Ui,
    config_path: &str,
    host: &str,
    machine: &str,
    summary: &mut Map<String, Value>,
) -> Result<(), Fail> {
    ui.blank();
    if !ui.confirm(&Confirm {
        question: &format!("Leave {host}?"),
        yes: "Leave it",
        yes_detail: "Stops the bridge, removes its background service, and moves this machine's setup aside (kept, not deleted).",
        no: "Keep it",
        no_detail: "Nothing changes.",
        default_yes: false,
        flag: "",
        declined: None,
    })? {
        return Err(Fail::Declined("Nothing was changed.".into()));
    }
    let root = setup_dir(config_path)?;
    let aside = format!("{root}.left-{}", super::system::file_stamp());
    let was_down = super::super::service::is_down(config_path);
    term::set_changed(true);
    ui.step(
        "Leaving the server",
        |_| {
            term::critical(|| {
                super::super::service::stop_current().map_err(|error| {
                    Problem::new("The bridge couldn't be stopped, so nothing was moved.")
                        .detail(error.message)
                })?;
                std::fs::rename(&root, &aside).map_err(|exc| {
                    Problem::new("This machine's setup couldn't be moved aside.")
                        .detail(exc.to_string())
                })
            })
        },
        |_| {
            Settle::Done(format!(
                "Stopped the bridge and moved this machine's setup to {}",
                super::super::install::tilde(&aside)
            ))
        },
    )
    .map_err(Fail::Shown)?;
    summary.insert("left".into(), Value::Bool(true));
    // The service runs the default configuration; one elsewhere (--config)
    // leaves it alone.
    let home = super::super::local::home_dir();
    let paths = super::super::install::paths(&home);
    let runs_this = fsutil::absolute(&paths.config).is_ok_and(|default| default == config_path);
    if runs_this && super::super::service::installed() {
        match term::critical(|| super::super::install::remove_service(&home)) {
            Ok(_) => {
                summary.insert("service_removed".into(), Value::Bool(true));
                ui.done("Removed the bridge's background service");
            }
            Err(error) => {
                summary.insert("service_removed".into(), Value::Bool(false));
                ui.caution(
                    &Problem::new("The bridge's background service couldn't be removed.")
                        .detail(format!(
                            "{}. Without a configuration it won't keep restarting.",
                            error.message
                        ))
                        .fix(format!(
                            "remove {} yourself.",
                            super::super::install::tilde(&paths.service)
                        )),
                );
            }
        }
    }
    // The setup that was down is gone, so the service mustn't stay disabled
    // for the next one (unless the default setup is itself down).
    if was_down && !super::super::service::default_is_down() {
        if let Err(error) = term::critical(super::super::service::forget_down) {
            ui.caution(
                &Problem::new("The bridge's service is still kept from starting at login.")
                    .detail(error.message)
                    .fix("run `switchboard up` once this machine joins a server again."),
            );
        }
    }
    ui.blank();
    ui.say(&format!("Done. This machine left {host}."), Tone::Strong);
    ui.blank();
    ui.say(
        &format!(
            "The server still lists it as {machine}. To remove it there, run this on the server:\n`sudo switchboard server revoke {machine}`"
        ),
        Tone::Normal,
    );
    ui.blank();
    ui.say(
        "To join a server again, run\n`switchboard init`",
        Tone::Normal,
    );
    Ok(())
}

/// What to do next once this machine is connected.
fn connect_chats(ui: &mut Ui) {
    ui.heading("Next");
    ui.steps(&["In each Claude Code chat you want connected, run\n`/switchboard join`".into()]);
}

/// Move the current setup aside, join the new server, and put the old
/// setup back if that fails.
fn switch(
    ui: &mut Ui,
    options: &Options,
    facts: &Facts,
    config_path: &str,
    invite: crate::invite::Invite,
    summary: &mut Map<String, Value>,
) -> Result<Joined, Fail> {
    let root = setup_dir(config_path)?;
    join::check_server(ui, &invite)?;
    let backup = format!("{root}.before-{}", super::system::file_stamp());
    term::set_changed(true);
    let was_running = ui
        .step(
            "Moving the current setup aside",
            |_| {
                term::critical(|| {
                    let running = super::super::service::stop_current().map_err(|error| {
                        Problem::new("The bridge couldn't be stopped, so nothing was moved.")
                            .detail(error.message)
                    })?;
                    std::fs::rename(&root, &backup).map_err(|exc| {
                        Problem::new("The current setup couldn't be moved aside.")
                            .detail(exc.to_string())
                    })?;
                    Ok(running)
                })
            },
            |_| {
                Settle::Done(format!(
                    "Moved the current setup to {}",
                    super::super::install::tilde(&backup)
                ))
            },
        )
        .map_err(Fail::Shown)?;
    // A machine that was down stays down on the new server.
    let was_down = fsutil::lstat(&fsutil::join(&backup, super::super::service::DOWN_FILE)).is_ok();
    let fresh = Options {
        invite: None,
        no_start: options.no_start || was_down,
        ..options.clone()
    };
    let given = join::Given {
        invite,
        checked: true,
    };
    match join::run(ui, &fresh, facts, Some(given), false, summary) {
        Ok(joined) => {
            if was_down && fsutil::lstat(&root).is_ok_and(|info| info.is_dir()) {
                let marker = fsutil::join(&root, super::super::service::DOWN_FILE);
                let _ = std::fs::copy(
                    fsutil::join(&backup, super::super::service::DOWN_FILE),
                    &marker,
                );
            }
            Ok(joined)
        }
        Err(failure) => {
            term::critical(|| {
                let _ = super::super::service::stop_current();
                if fsutil::lstat(&root).is_ok() {
                    let _ = std::fs::remove_dir_all(&root);
                }
                let restored = std::fs::rename(&backup, &root).is_ok();
                if restored && was_running {
                    let _ = super::super::service::service("start", Some(config_path));
                }
                if restored {
                    ui.done("Put the previous setup back");
                } else {
                    ui.warn(&format!(
                        "The previous setup is still at {}; move it back to {} by hand.",
                        super::super::install::tilde(&backup),
                        super::super::install::tilde(&root)
                    ));
                }
            });
            Err(failure)
        }
    }
}

/// `sudo switchboard server invite`, shown like the first invite.
fn make_invite(ui: &mut Ui) -> Result<(), Fail> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "switchboard".into());
    let mut argv: Vec<String> = Vec::new();
    if !super::system::is_root() {
        argv.push("sudo".into());
    }
    argv.extend([exe, "server".into(), "invite".into(), "--json".into()]);
    ui.say(
        "Making an invite with sudo. It may ask for your password.",
        Tone::Normal,
    );
    let output = term::critical(|| {
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .output()
    })
    .map_err(|exc| {
        Fail::Problem(Problem::new("sudo couldn't be started.").detail(exc.to_string()))
    })?;
    let value: Option<Map<String, Value>> = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|v| v.as_object().cloned());
    match value {
        Some(map) if map.get("invite").and_then(Value::as_str).is_some() => {
            let invite = map["invite"].as_str().unwrap_or_default().to_string();
            super::show_invite(
                ui,
                &invite,
                map.get("expires_at").and_then(Value::as_i64),
                false,
                false,
            );
            Ok(())
        }
        Some(map) => Err(Fail::Problem(
            Problem::new("No invite was made.").detail(
                map.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the server's invite command failed")
                    .to_string(),
            ),
        )),
        None => Err(Fail::Problem(
            Problem::new("No invite was made.")
                .detail("sudo or the invite command did not finish."),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::Action;

    #[test]
    fn a_machine_that_is_down_defaults_to_keeping_it_down() {
        let offered = [
            Action::Start,
            Action::Plugin,
            Action::Switch,
            Action::Leave,
            Action::Nothing,
        ];
        let keep = offered.len() - 1;
        // Down on purpose: Enter keeps it down, even with the plugin missing.
        assert_eq!(super::default_choice(&offered, false, true), keep);
        assert_eq!(super::default_choice(&offered, true, false), keep);
        // Not running for another reason: the first fix.
        assert_eq!(super::default_choice(&offered, false, false), 0);
        assert_eq!(
            super::default_choice(&[Action::Rejoin, Action::Nothing], false, true),
            1
        );
    }

    #[test]
    fn only_a_directory_of_its_own_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("sbsetup-{}", std::process::id()));
        let setup = dir.join("switchboard");
        std::fs::create_dir_all(setup.join("identity")).unwrap();
        std::fs::write(setup.join("bridge.json"), "{}").unwrap();
        std::fs::write(setup.join("membership.json"), "{}").unwrap();
        std::fs::write(setup.join("bridge.json.lock"), "").unwrap();
        let config = setup.join("bridge.json").to_string_lossy().into_owned();
        assert_eq!(super::setup_dir(&config).unwrap(), setup.to_string_lossy());
        // A checkout that happens to be called switchboard is not moved.
        std::fs::write(setup.join("README.md"), "mine").unwrap();
        assert!(super::setup_dir(&config).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(super::setup_dir("/nonexistent/x/bridge.json").is_err());
    }
}
