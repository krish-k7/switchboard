//! The Claude Code plugin: `/switchboard` commands in every chat.
//!
//! Installed with Claude Code's own command (`claude plugin install
//! switchboard --marketplace OWNER/REPO`), only when asked: on a terminal
//! init offers it (Enter installs it); without one, only `--plugin` (or
//! `--yes`) installs it. A failure here never undoes the join: it says how
//! to install the plugin by hand.

use std::time::Duration;

use serde_json::{Map, Value};

use super::super::ui::prompt::{Confirm, Problem, Settle, Ui};
use super::{system, Fail, Options};

/// The `claude` command on PATH.
pub fn claude_path() -> Option<String> {
    system::which("claude")
}

/// The command that installs the plugin by hand.
pub fn manual_command() -> String {
    format!(
        "claude plugin install switchboard --marketplace {}",
        super::repository()
    )
}

/// Whether `claude plugin list --json` lists the Switchboard plugin.
pub fn installed(claude: &str) -> Result<bool, String> {
    let (code, stdout, stderr) = system::run(
        &[
            claude.into(),
            "plugin".into(),
            "list".into(),
            "--json".into(),
        ],
        Duration::from_secs(30),
    )?;
    if code != 0 {
        return Err(first_line(&stderr, &stdout, "claude plugin list failed"));
    }
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| "claude plugin list --json printed something other than JSON".to_string())?;
    Ok(lists_switchboard(&value))
}

/// The Switchboard plugin's entries in a `claude plugin list --json` result.
fn switchboard_entries(value: &Value) -> Vec<&Value> {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => match map.get("plugins") {
            Some(Value::Array(items)) => items.iter().collect(),
            _ => map.values().collect(),
        },
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .filter(|entry| {
            ["id", "name", "plugin"].iter().any(|key| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == "switchboard" || id.starts_with("switchboard@"))
            })
        })
        .collect()
}

/// Whether a `claude plugin list --json` result has the plugin.
pub fn lists_switchboard(value: &Value) -> bool {
    !switchboard_entries(value).is_empty()
}

/// Whether a `claude plugin list --json` result has a Switchboard plugin
/// older than `version` (or one whose version it doesn't say).
pub fn lists_switchboard_before(value: &Value, version: &str) -> bool {
    let Some(wanted) = super::super::release::Version::parse(version) else {
        return false;
    };
    switchboard_entries(value).iter().any(|entry| {
        entry
            .get("version")
            .and_then(Value::as_str)
            .and_then(super::super::release::Version::parse)
            .is_none_or(|found| found < wanted)
    })
}

/// Whether Claude Code has the Switchboard plugin at a version older than
/// `version`, asked as `installed` asks.
pub fn installed_before(claude: &str, version: &str) -> Result<bool, String> {
    let (code, stdout, stderr) = system::run(
        &[
            claude.into(),
            "plugin".into(),
            "list".into(),
            "--json".into(),
        ],
        Duration::from_secs(30),
    )?;
    if code != 0 {
        return Err(first_line(&stderr, &stdout, "claude plugin list failed"));
    }
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| "claude plugin list --json printed something other than JSON".to_string())?;
    Ok(lists_switchboard_before(&value, version))
}

fn first_line(stderr: &str, stdout: &str, fallback: &str) -> String {
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| crate::text::take_chars(line, 300))
        .unwrap_or_else(|| fallback.to_string())
}

/// Install the plugin with Claude Code's command.
pub fn install(claude: &str) -> Result<(), String> {
    let (code, stdout, stderr) = system::run(
        &[
            claude.into(),
            "plugin".into(),
            "install".into(),
            "switchboard".into(),
            "--marketplace".into(),
            super::repository(),
            "--json".into(),
        ],
        Duration::from_secs(180),
    )?;
    if code != 0 {
        return Err(first_line(&stderr, &stdout, "claude plugin install failed"));
    }
    Ok(())
}

/// Where the plugin stands after init offered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plugin {
    /// Installed by this run: chats already open need `/reload-plugins`.
    New,
    /// Installed before.
    Present,
    /// Not installed.
    Absent,
}

impl Plugin {
    pub fn installed(self) -> bool {
        self != Plugin::Absent
    }
}

/// Offer the plugin after joining.
pub fn offer(
    ui: &mut Ui,
    options: &Options,
    summary: &mut Map<String, Value>,
) -> Result<Plugin, Fail> {
    let record = |summary: &mut Map<String, Value>, state: &str| {
        summary.insert("plugin".into(), Value::from(state));
    };
    if options.plugin == Some(false) {
        record(summary, "skipped");
        return Ok(Plugin::Absent);
    }
    let Some(claude) = claude_path() else {
        record(summary, "unavailable");
        if options.plugin == Some(true) {
            ui.caution(
                &Problem::new("The Claude Code plugin wasn't installed: Claude Code isn't on this machine's PATH.")
                    .fix(format!("install Claude Code, then run `{}`.", manual_command())),
            );
        } else {
            ui.warn(&format!(
                "Claude Code isn't on this machine's PATH, so the plugin was skipped. Add it later with\n`{}`",
                manual_command()
            ));
        }
        return Ok(Plugin::Absent);
    };
    let present = ui
        .step(
            "Looking for the Claude Code plugin",
            |_| Ok(installed(&claude)),
            |found| match found {
                Ok(true) => Settle::Done("The Claude Code plugin is already installed".into()),
                _ => Settle::Quiet,
            },
        )
        .map_err(Fail::Shown)?;
    if present == Ok(true) {
        record(summary, "installed");
        return Ok(Plugin::Present);
    }
    let wanted = if options.plugin == Some(true) || options.yes {
        true
    } else if ui.interactive() {
        ui.confirm(&Confirm {
            question: "Install the Claude Code plugin?",
            yes: "Install it",
            yes_detail: "Adds /switchboard commands to every Claude Code chat (join, leave, chats, and status) and a label on joined chats.",
            no: "Skip",
            no_detail: "Use !switchboard join in chats instead. You can add the plugin later.",
            default_yes: true,
            flag: "--plugin or --no-plugin",
            declined: Some("Skipped the Claude Code plugin"),
        })?
    } else {
        ui.warn(&format!(
            "The Claude Code plugin wasn't installed (without a terminal, only --plugin installs it). Add it later with\n`{}`",
            manual_command()
        ));
        false
    };
    if !wanted {
        record(summary, "skipped");
        return Ok(Plugin::Absent);
    }
    let outcome = ui
        .step(
            "Installing the Claude Code plugin",
            |_| Ok(install(&claude)),
            |result| match result {
                Ok(()) => Settle::Done("Installed the Claude Code plugin".into()),
                Err(_) => Settle::Quiet,
            },
        )
        .map_err(Fail::Shown)?;
    match outcome {
        Ok(()) => {
            record(summary, "installed");
            Ok(Plugin::New)
        }
        Err(reason) => {
            record(summary, "failed");
            ui.caution(
                &Problem::new("The Claude Code plugin wasn't installed.")
                    .detail(reason)
                    .fix(format!(
                        "update Claude Code (`claude update`), then run `{}`. This machine is joined either way.",
                        manual_command()
                    )),
            );
            Ok(Plugin::Absent)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_lists_are_read_leniently() {
        let listed = serde_json::json!([{"id": "switchboard@switchboard", "version": "0.1.2"}]);
        assert!(lists_switchboard(&listed));
        assert!(lists_switchboard(
            &serde_json::json!({"plugins": [{"name": "switchboard"}]})
        ));
        assert!(!lists_switchboard(&serde_json::json!([])));
        assert!(!lists_switchboard(
            &serde_json::json!([{"id": "switchboard-extra@x"}])
        ));
    }

    #[test]
    fn older_plugins_are_found_by_version() {
        let list = |version: Value| serde_json::json!([{"id": "other@x", "version": "0.0.1"}, {"id": "switchboard@switchboard", "version": version}]);
        assert!(lists_switchboard_before(&list("0.3.0".into()), "0.3.1"));
        assert!(lists_switchboard_before(
            &list("0.3.1-rc.1".into()),
            "0.3.1"
        ));
        assert!(!lists_switchboard_before(&list("0.3.1".into()), "0.3.1"));
        assert!(!lists_switchboard_before(&list("1.0.0".into()), "0.3.1"));
        // A version it can't read may be an old one.
        assert!(lists_switchboard_before(&list(Value::Null), "0.3.1"));
        assert!(lists_switchboard_before(&list("unknown".into()), "0.3.1"));
        assert!(!lists_switchboard_before(
            &serde_json::json!([{"id": "other@x"}]),
            "0.3.1"
        ));
    }
}
