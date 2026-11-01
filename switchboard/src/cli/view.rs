//! What each command shows, defined once for the terminal and the Claude
//! Code plugin.
//!
//! Every command the two share (`status`, `chats`, `join`, `leave`, `ping`,
//! `doctor`, `up`, `down`, and the help's command list) builds a *display*:
//! a list of blocks that says what to show, in what order, with what labels
//! and words. The terminal draws it in the command line's visual language
//! (`render`); with `--json --display` the CLI adds it to its JSON as
//! `display`, and the plugin draws the same blocks with Claude Code's
//! renderer. So the two surfaces can't drift: there is one set of words.
//!
//! Blocks are JSON objects with a `kind`:
//!
//! - `title` `{text, tone?}`: the answer, first. `tone` is `success`,
//!   `warning`, or `error` (a check mark, `!`, or a cross in a terminal).
//! - `heading` `{text}`: names the group under it.
//! - `fact` `{label, value}`: consecutive facts share one label column.
//! - `text` `{text, tone?, strong?}`: a sentence; `strong` in bold.
//! - `next` `{text, command}`: the one thing to do next, the command on a
//!   line of its own.
//! - `item` `{text, tone, fix?}`: a check's result, and how to fix it.
//! - `table` `{columns, rows: [{cells, note?}]}`: the first cell is a name;
//!   `note` follows it (`(this chat)`).
//! - `code` `{text}`: a command to copy, on a line of its own.
//!
//! Any block may have `space: true` for a blank line before it. Commands in
//! text are in backticks. The plugin shows `` `switchboard X` `` as
//! `/switchboard X` when X is one of its own commands (`SLASH_COMMANDS`).

use serde_json::{json, Map, Value};

use super::format::{display_width, text, text_value};
use super::ui::{self, Mark, Theme, Tone};
use crate::wirejson::{str_value, truthy};

/// The plugin's commands: in a chat, `switchboard X` reads `/switchboard X`.
pub const SLASH_COMMANDS: [&str; 9] = [
    "status", "chats", "join", "leave", "ping", "up", "down", "doctor", "help",
];

/// The commands both surfaces list in their help, in order, with what
/// each does. The CLI's overview (`args::HELP`) uses the same words.
pub const SHARED_COMMANDS: [(&str, &str, &str); 8] = [
    // (command, its argument as the plugin writes it, what it does)
    (
        "status",
        "",
        "Whether this machine is connected, and this chat",
    ),
    ("chats", "", "List chats on this machine and others"),
    (
        "join",
        "[name]",
        "Join this chat, so chats on other machines can message it",
    ),
    (
        "leave",
        "",
        "Leave this chat; other machines can no longer see it",
    ),
    (
        "ping",
        "<chat>",
        "Check that a chat on another machine is reachable",
    ),
    ("up", "", "Bring this machine back online"),
    (
        "down",
        "",
        "Take this machine offline until you bring it back up",
    ),
    (
        "doctor",
        "",
        "Check everything, with the fix for each problem",
    ),
];

/// How many chats `chats` lists before asking for search words.
pub const CHAT_LIMIT: usize = 50;

pub type Doc = Vec<Value>;

type Row = Map<String, Value>;
/// Chats on one machine: whether it is this one, its name, and its chats.
type Group = (bool, String, Vec<Row>);
/// Which checks a part of the doctor's list holds, and its tone.
type Part<'a> = (&'a dyn Fn(&Row) -> bool, Option<&'a str>);

fn block(kind: &str) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("kind".into(), Value::from(kind));
    value
}

fn title(value: &str, tone: Option<&str>) -> Value {
    let mut b = block("title");
    b.insert("text".into(), Value::from(value));
    if let Some(tone) = tone {
        b.insert("tone".into(), Value::from(tone));
    }
    Value::Object(b)
}

fn fact(label: &str, value: &str) -> Value {
    json!({"kind": "fact", "label": label, "value": value})
}

fn sentence(value: &str, space: bool) -> Value {
    let mut b = block("text");
    b.insert("text".into(), Value::from(value));
    if space {
        b.insert("space".into(), Value::Bool(true));
    }
    Value::Object(b)
}

fn next(value: &str, command: &str) -> Value {
    json!({"kind": "next", "space": true, "text": value, "command": command})
}

fn spaced(mut value: Value) -> Value {
    if let Some(map) = value.as_object_mut() {
        map.insert("space".into(), Value::Bool(true));
    }
    value
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

fn rows_of(result: &Map<String, Value>, key: &str) -> Vec<Map<String, Value>> {
    result
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

/// `text` with a capital first letter and a final period.
fn as_sentence(value: &str) -> String {
    let value = value.trim();
    let mut out = ui::capitalized(value);
    if !out.is_empty() && !out.ends_with(['.', '?', '!']) {
        out.push('.');
    }
    out
}

/// `expires in 6 days`, `expires today`, or nothing for a chat that isn't
/// joined.
pub fn expiry(row: &Map<String, Value>, now: f64) -> String {
    if !truthy(row.get("enrolled")) {
        return String::new();
    }
    let Some(at) = number(row.get("expires_at")) else {
        return String::new();
    };
    match super::format::days_left(at, now) {
        0 => "expires today".into(),
        1 => "expires in 1 day".into(),
        days => format!("expires in {days} days"),
    }
}

/// What a chat is doing, as a person says it.
pub fn activity(status: &str) -> &'static str {
    match status {
        "idle" => "Idle",
        "busy" => "Busy",
        "waiting" => "Waiting",
        "shell" => "Shell",
        _ => "",
    }
}

/// The name other machines see for a joined chat.
fn joined_name(row: &Map<String, Value>) -> String {
    let chosen = text_value(row.get("joined_name"));
    if chosen.is_empty() {
        chat_name(row)
    } else {
        chosen
    }
}

fn chat_name(row: &Map<String, Value>) -> String {
    let name = text_value(row.get("name"));
    if name.is_empty() {
        "(unnamed chat)".into()
    } else {
        name
    }
}

/// The server's host for people: `sb.example.com` or `100.64.0.5:8743`.
fn host(result: &Map<String, Value>) -> String {
    result
        .get("server_url")
        .and_then(Value::as_str)
        .map(super::init::join::host_label)
        .map(|h| text(&h))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "the server".into())
}

/// What `status` shows: is this machine connected, and is this chat in?
///
/// `current`: this chat's ID, when the command runs in a chat (or is given
/// one). `update`: a newer release. `service_missing`: the bridge's service
/// file isn't installed. This is what the plugin shows, so a chat is told
/// to run `/switchboard join`; the terminal's `status` names the command
/// that fits this machine (`status_for`).
pub fn status(
    result: &Map<String, Value>,
    current: Option<&str>,
    update: Option<&str>,
    service_missing: bool,
    now: f64,
) -> Doc {
    status_for(result, current, update, service_missing, now, &|| {
        "/switchboard join".into()
    })
}

/// `status`, with `join` giving the command that joins a chat, asked only
/// when the answer suggests it.
pub fn status_for(
    result: &Map<String, Value>,
    current: Option<&str>,
    update: Option<&str>,
    service_missing: bool,
    now: f64,
    join: &dyn Fn() -> String,
) -> Doc {
    let rows = rows_of(result, "sessions");
    let enabled = truthy(result.get("enabled"));
    let connected = truthy(result.get("connected"));
    let down = truthy(result.get("down"));
    let machine = text_value(result.get("machine"));
    let detail = text_value(result.get("detail"));
    let host = host(result);
    let mut doc = Vec::new();
    let mut step: Option<Value> = None;
    if down {
        doc.push(title("Down. Run `switchboard up` to reconnect.", None));
    } else if !enabled {
        doc.push(title(
            "Turned off in the bridge configuration, so this machine is offline.",
            Some("warning"),
        ));
        step = Some(next(
            "To turn it on, set \"enabled\" to true in the bridge configuration, then run",
            "switchboard up",
        ));
    } else if connected {
        doc.push(title(
            &if machine.is_empty() {
                format!("Connected to {host}")
            } else {
                format!("Connected to {host} as {machine}")
            },
            Some("success"),
        ));
    } else if detail == "the bridge is not running" {
        doc.push(title(
            "Not connected: the bridge isn't running.",
            Some("error"),
        ));
        step = Some(if service_missing {
            next(
                "To install the bridge's service and start it, run",
                "switchboard install --start",
            )
        } else {
            next("To reconnect, run", "switchboard up")
        });
    } else {
        let why = if detail.is_empty() {
            String::new()
        } else {
            let detail = detail.trim_end_matches('.');
            let mut chars = detail.chars();
            match chars.next() {
                Some(first) => format!(": {}{}", first.to_lowercase(), chars.as_str()),
                None => String::new(),
            }
        };
        doc.push(title(
            &format!("Not connected to {host}{why}."),
            Some("error"),
        ));
        step = Some(next("To see why, run", "switchboard doctor"));
    }
    if let Some(id) = current {
        let value = match rows
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        {
            Some(row) if truthy(row.get("enrolled")) => {
                let expires = expiry(row, now);
                if expires.is_empty() {
                    format!("Joined as {}", joined_name(row))
                } else {
                    format!("Joined as {}, {expires}", joined_name(row))
                }
            }
            Some(_) => format!("Not joined. Run {}.", join()),
            None => "Not found on this machine".into(),
        };
        doc.push(fact("This chat", &value));
    }
    let joined = rows.iter().filter(|r| truthy(r.get("enrolled"))).count();
    doc.push(fact(
        "Chats here",
        &format!("{joined} of {} joined", rows.len()),
    ));
    if let Some(version) = update {
        doc.push(update_block(version));
    }
    if step.is_none() && connected && current.is_none() && joined == 0 {
        step = Some(next("To connect a chat, run this in it:", &join()));
    }
    doc.extend(step);
    doc
}

/// The line `status` adds when a newer release exists.
pub fn update_line(version: &str) -> String {
    format!("Switchboard {version} is available. Run `switchboard upgrade`.")
}

/// The update line as a block.
pub fn update_block(version: &str) -> Value {
    sentence(&update_line(version), true)
}

/// What `status` shows on a machine that isn't set up.
pub fn not_set_up() -> Doc {
    vec![title("Not set up. Run `switchboard init`.", None)]
}

/// Which chats `chats` lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Local,
    Remote,
}

/// What `chats` shows: every chat here and on other machines, with what
/// each is doing and whether it is joined. `hint`: the command that shows
/// the same list with full IDs and details.
pub struct ChatsView<'a> {
    pub scope: Scope,
    pub current: Option<&'a str>,
    pub search: &'a str,
    pub details: bool,
    /// Whether any filter (search words, `--name`, ...) narrowed the list.
    pub filtered: bool,
    pub hint: Option<String>,
    /// This machine's name, when known.
    pub machine: Option<String>,
}

pub fn chats(result: &Map<String, Value>, view: &ChatsView<'_>, now: f64) -> Doc {
    let field = |row: &Map<String, Value>, key: &str| text_value(row.get(key));
    let mut rows: Vec<(bool, Map<String, Value>)> = Vec::new();
    if view.scope != Scope::Remote {
        rows.extend(rows_of(result, "sessions").into_iter().map(|r| (true, r)));
    }
    let remote_listed = result.contains_key("remote_available");
    let remote_available = truthy(result.get("remote_available"));
    if view.scope != Scope::Local && remote_available {
        rows.extend(
            rows_of(result, "remote_sessions")
                .into_iter()
                .map(|r| (false, r)),
        );
    }
    let here = view.machine.clone().filter(|m| !m.is_empty());
    let machine_of = |local: bool, row: &Map<String, Value>| {
        if local {
            here.clone().unwrap_or_default()
        } else {
            field(row, "machine")
        }
    };
    let is_current = |local: bool, row: &Map<String, Value>| {
        local && view.current.is_some_and(|id| field(row, "id") == id)
    };
    rows.sort_by(|(la, a), (lb, b)| {
        lb.cmp(la)
            .then_with(|| machine_of(*la, a).cmp(&machine_of(*lb, b)))
            .then_with(|| is_current(*lb, b).cmp(&is_current(*la, a)))
            .then_with(|| truthy(b.get("enrolled")).cmp(&truthy(a.get("enrolled"))))
            .then_with(|| chat_name(a).cmp(&chat_name(b)))
            .then_with(|| field(a, "id").cmp(&field(b, "id")))
    });
    let total = rows.len();
    let heading = match view.scope {
        Scope::All => "Chats",
        Scope::Local => "Chats on this machine",
        Scope::Remote => "Chats on other machines",
    };
    let search = text(view.search);
    let mut doc = vec![title(
        &if search.is_empty() {
            heading.to_string()
        } else {
            format!("{heading} matching “{search}”")
        },
        None,
    )];
    // Groups by machine, in order: this machine first.
    let mut groups: Vec<Group> = Vec::new();
    for (local, row) in rows.into_iter().take(CHAT_LIMIT) {
        let machine = machine_of(local, &row);
        match groups
            .iter_mut()
            .find(|(l, m, _)| *l == local && *m == machine)
        {
            Some((_, _, group)) => group.push(row),
            None => groups.push((local, machine, vec![row])),
        }
    }
    for (local, machine, group) in &groups {
        let name = if *local {
            match &here {
                Some(here) => format!("{here} (this machine)"),
                None => "This machine".into(),
            }
        } else {
            machine.clone()
        };
        doc.push(spaced(json!({"kind": "heading", "text": name})));
        if view.details {
            for row in group {
                let mut shown = chat_name(row);
                if is_current(*local, row) {
                    shown.push_str(" (this chat)");
                }
                let mut name = sentence(&shown, true);
                name["strong"] = Value::Bool(true);
                doc.push(name);
                let enrolled = truthy(row.get("enrolled"));
                let mut facts = vec![("Activity", activity(&field(row, "status")).to_string())];
                if *local {
                    facts.push((
                        "Membership",
                        if enrolled { "Joined" } else { "Not joined" }.to_string(),
                    ));
                }
                if *local && enrolled {
                    facts.push(("Joined as", joined_name(row)));
                    facts.push((
                        "Expires",
                        expiry(row, now).trim_start_matches("expires ").to_string(),
                    ));
                }
                let project = field(row, "project");
                let repository = field(row, "repository");
                if project != repository {
                    facts.push(("Project", project));
                }
                facts.push(("Repository", repository));
                facts.push(("Branch", field(row, "branch")));
                facts.push(("ID", field(row, "id")));
                for (label, value) in facts {
                    if !value.is_empty() {
                        doc.push(fact(label, &value));
                    }
                }
            }
        } else {
            let columns: Vec<&str> = if *local {
                vec!["Chat", "Activity", "Membership", "Expires"]
            } else {
                vec!["Chat", "Activity"]
            };
            let table_rows: Vec<Value> = group
                .iter()
                .map(|row| {
                    let mut cells = vec![chat_name(row), activity(&field(row, "status")).into()];
                    if *local {
                        let enrolled = truthy(row.get("enrolled"));
                        cells.push(if enrolled {
                            format!("Joined as {}", joined_name(row))
                        } else {
                            "Not joined".into()
                        });
                        cells.push(expiry(row, now).trim_start_matches("expires ").to_string());
                    }
                    let mut value = json!({"cells": cells});
                    if is_current(*local, row) {
                        value["note"] = json!("(this chat)");
                    }
                    value
                })
                .collect();
            doc.push(json!({"kind": "table", "columns": columns, "rows": table_rows}));
        }
    }
    let unavailable = remote_listed && !remote_available && view.scope != Scope::Local;
    if total == 0 {
        let line = if unavailable {
            if view.scope == Scope::Remote {
                None
            } else if !view.filtered {
                Some("No chats on this machine.")
            } else {
                Some("No matching chats on this machine.")
            }
        } else if view.filtered {
            Some("No matching chats.")
        } else {
            Some("No chats found.")
        };
        if let Some(line) = line {
            doc.push(sentence(line, true));
        }
    }
    if total > CHAT_LIMIT {
        doc.push(sentence(
            &format!("Showing {CHAT_LIMIT} of {total} chats. Add search words to narrow the list."),
            true,
        ));
    }
    if unavailable {
        let error = text_value(result.get("remote_error"));
        let mut b = block("text");
        b.insert("space".into(), Value::Bool(true));
        b.insert("tone".into(), Value::from("warning"));
        b.insert(
            "text".into(),
            Value::from(if error.is_empty() {
                "Chats on other machines are unavailable.".to_string()
            } else {
                error
            }),
        );
        doc.push(Value::Object(b));
    }
    if let (false, true, Some(hint)) = (view.details, total > 0, &view.hint) {
        doc.push(next("Full IDs and details:", hint));
    }
    doc
}

/// What a person calls a doctor check, for the plugin's and the terminal's
/// lists alike.
fn passes_shown(check: &Map<String, Value>) -> bool {
    // How many chats are joined is `status`'s answer, not a check.
    !(check.get("name").and_then(Value::as_str) == Some("chats")
        && check.get("status").and_then(Value::as_str) == Some("pass"))
}

/// What `doctor` shows: problems first, each with its fix, then warnings,
/// then a machine that is down, then the checks that passed, one per line,
/// then a summary.
pub fn doctor(result: &Map<String, Value>) -> Doc {
    let checks = rows_of(result, "checks");
    let status_of = |c: &Map<String, Value>| c.get("status").map(str_value).unwrap_or_default();
    let mut doc = Vec::new();
    let failed = |c: &Map<String, Value>| {
        let s = status_of(c);
        s != "pass" && s != "warn"
    };
    let down = |c: &Map<String, Value>| truthy(c.get("down"));
    let groups: [Part; 4] = [
        (&failed, Some("error")),
        (&|c| status_of(c) == "warn", Some("warning")),
        (&|c| status_of(c) == "pass" && down(c), None),
        (
            &|c| status_of(c) == "pass" && !down(c) && passes_shown(c),
            Some("success"),
        ),
    ];
    for (wanted, tone) in groups {
        for check in checks.iter().filter(|c| wanted(c)) {
            let mut b = block("item");
            b.insert(
                "text".into(),
                Value::from(text(
                    &check.get("summary").map(str_value).unwrap_or_default(),
                )),
            );
            if let Some(tone) = tone {
                b.insert("tone".into(), Value::from(tone));
            }
            if tone != Some("success") {
                if let Some(fix) = check.get("fix").filter(|v| truthy(Some(v))) {
                    b.insert("fix".into(), Value::from(text(&str_value(fix))));
                }
            }
            // A problem with a fix stands apart from the next one.
            let first = doc.is_empty();
            let previous_fixed = doc
                .last()
                .and_then(Value::as_object)
                .is_some_and(|p: &Map<String, Value>| p.contains_key("fix"));
            let mut value = Value::Object(b);
            if !first && (previous_fixed || tone != doc_tone(doc.last())) {
                value = spaced(value);
            }
            doc.push(value);
        }
    }
    let count =
        |test: &dyn Fn(&Map<String, Value>) -> bool| checks.iter().filter(|c| test(c)).count();
    let failures = count(&failed);
    let warnings = count(&|c| status_of(c) == "warn");
    let warnings_have_fixes = checks
        .iter()
        .any(|c| status_of(c) == "warn" && c.get("fix").is_some_and(|v| truthy(Some(v))));
    let summary = match (failures, warnings) {
        (0, 0) => "All checks passed.".to_string(),
        (0, w) if warnings_have_fixes => format!("{}.", ui::count(w, "warning", "warnings")),
        (0, w) => format!("{}, nothing to fix.", ui::count(w, "warning", "warnings")),
        (f, 0) => format!("{} to fix.", ui::count(f, "problem", "problems")),
        (f, w) => format!(
            "{} to fix, and {}.",
            ui::count(f, "problem", "problems"),
            ui::count(w, "warning", "warnings")
        ),
    };
    let mut b = block("text");
    b.insert("space".into(), Value::Bool(true));
    b.insert("strong".into(), Value::Bool(true));
    b.insert("text".into(), Value::from(summary));
    doc.push(Value::Object(b));
    doc
}

fn doc_tone(value: Option<&Value>) -> Option<&str> {
    value.and_then(|v| v.get("tone")).and_then(Value::as_str)
}

/// What `join` and `leave` show: one confirmation; a join adds how many
/// chats can now reach it.
pub fn selection(result: &Map<String, Value>) -> Doc {
    let name = {
        let name = text_value(result.get("name"));
        if name.is_empty() {
            "this chat".to_string()
        } else {
            name
        }
    };
    let confirmed = truthy(result.get("confirmed"));
    let detail = as_sentence(&text_value(result.get("detail")));
    let doctor = || next("If it stays this way, run", "switchboard doctor");
    if result.get("action").and_then(Value::as_str) == Some("join") {
        if confirmed {
            let count = |key: &str| number(result.get(key)).unwrap_or(0.0).max(0.0) as i64;
            let (sessions, machines) = (count("remote_sessions"), count("remote_machines"));
            let reach = if sessions == 0 {
                "No other machine has a joined chat right now.".to_string()
            } else {
                format!(
                    "Reachable by {} on {} other {}.",
                    ui::count(sessions as usize, "chat", "chats"),
                    machines,
                    if machines == 1 { "machine" } else { "machines" }
                )
            };
            return vec![
                title(&format!("Joined as {name}."), Some("success")),
                sentence(&reach, false),
            ];
        }
        let headline = match result.get("published_name") {
            Some(Value::String(seen)) => format!(
                "Joined as {name}, but other machines still see it as {}.",
                text(seen)
            ),
            _ => format!("Joined as {name}, but not live yet."),
        };
        return vec![
            title(&headline, Some("warning")),
            sentence(&detail, false),
            doctor(),
        ];
    }
    if result.get("changed") == Some(&Value::Bool(false)) {
        return vec![title("This chat wasn't joined; nothing changed.", None)];
    }
    if confirmed {
        return vec![
            title("This chat left.", Some("success")),
            sentence("Other machines can no longer see it.", false),
        ];
    }
    vec![
        title(
            "This chat left, but the bridge hasn't confirmed it yet.",
            Some("warning"),
        ),
        sentence(&detail, false),
        doctor(),
    ]
}

/// What `ping` shows for a reply.
pub fn ping_reply(result: &Map<String, Value>) -> Doc {
    let rtt = number(result.get("rtt_ms")).unwrap_or(0.0).max(0.0).round() as i64;
    vec![title(
        &format!(
            "{}/{} replied in {rtt} ms.",
            text_value(result.get("machine")),
            text_value(result.get("name"))
        ),
        Some("success"),
    )]
}

/// What `ping` shows when no reply came: why, and how to fix it.
pub fn ping_failed(target: &str, error: &str) -> Doc {
    let target = super::format::wrap_cells(&text(target), 128)
        .into_iter()
        .next()
        .unwrap_or_default();
    let problem = super::problem_for(error, None, "ping");
    let mut why = block("item");
    why.insert("text".into(), Value::from(problem.title));
    if let Some(fix) = problem.fix {
        why.insert("fix".into(), Value::from(fix));
    }
    vec![
        title(&format!("No reply from {target}."), Some("error")),
        Value::Object(why),
    ]
}

/// What `up` shows: whether the bridge connected.
pub fn up(result: &Map<String, Value>) -> Doc {
    let host = host(result);
    let machine = text_value(result.get("machine"));
    if truthy(result.get("connected")) {
        let who = if machine.is_empty() {
            String::new()
        } else {
            format!(" as {machine}")
        };
        return vec![title(
            &format!("Up. Connected to {host}{who}."),
            Some("success"),
        )];
    }
    vec![
        title(
            &format!("Up, but not connected to {host} yet."),
            Some("warning"),
        ),
        next("To see why, run", "switchboard doctor"),
    ]
}

/// What `down` shows.
pub fn down() -> Doc {
    vec![
        title(
            "Down. This machine's chats are offline until you run `switchboard up`.",
            Some("success"),
        ),
        sentence(
            "It stays down after a restart. Its setup and joined chats are kept.",
            false,
        ),
    ]
}

/// The plugin's help: the shared commands, with what each does.
pub fn help() -> Doc {
    let mut rows: Vec<Value> = SHARED_COMMANDS
        .iter()
        .map(|(command, argument, what)| {
            let shown = if argument.is_empty() {
                command.to_string()
            } else {
                format!("{command} {argument}")
            };
            json!({"cells": [shown, what]})
        })
        .collect();
    rows.push(json!({"cells": ["help", "Show these commands"]}));
    let mut doc = vec![
        title("/switchboard [command]", None),
        json!({"kind": "table", "space": true, "columns": ["Command", "What it does"], "rows": rows}),
        spaced(json!({"kind": "heading", "text": "Examples"})),
    ];
    for example in [
        "/switchboard join release-planner",
        "/switchboard chats --remote research",
        "/switchboard chats --details project-name",
        "/switchboard ping laptop/research",
    ] {
        doc.push(json!({"kind": "code", "text": example}));
    }
    doc.push(sentence(
        "Chats takes --local or --remote, --details, and search words that match names, IDs, machines, projects, repositories, and branches.",
        true,
    ));
    doc.push(sentence(
        "A joined chat leaves after 7 days without work or messages. Closing or resuming a chat does not count.",
        false,
    ));
    doc.push(sentence(
        "To add a machine, make an invite on the server with `sudo switchboard server invite`, then run `switchboard init` on the new machine.",
        false,
    ));
    doc
}

// Drawing a display in a terminal ------------------------------------------

fn tone_mark(tone: Option<&str>) -> Mark {
    match tone {
        Some("success") => Mark::Done,
        Some("warning") => Mark::Warned,
        Some("error") => Mark::Failed,
        _ => Mark::Blank,
    }
}

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A display in the command line's visual language: marks for titles and
/// checks, an aligned label column for facts, each command on a line of
/// its own.
pub fn render(doc: &[Value], theme: &Theme, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut index = 0;
    while index < doc.len() {
        let value = &doc[index];
        if value.get("space") == Some(&Value::Bool(true))
            && lines.last().is_some_and(|l| !l.is_empty())
        {
            lines.push(String::new());
        }
        let tone = value.get("tone").and_then(Value::as_str);
        match s(value, "kind") {
            "title" => {
                let mark = tone_mark(tone);
                let weight = if mark == Mark::Blank {
                    Tone::Strong
                } else {
                    Tone::Normal
                };
                lines.extend(ui::item(theme, mark, s(value, "text"), weight, columns));
            }
            "heading" => lines.extend(ui::heading(theme, &text(s(value, "text")), columns)),
            "fact" => {
                // A run of facts shares one label column.
                let mut end = index;
                while end < doc.len()
                    && s(&doc[end], "kind") == "fact"
                    && (end == index || doc[end].get("space") != Some(&Value::Bool(true)))
                {
                    end += 1;
                }
                let pad = doc[index..end]
                    .iter()
                    .map(|f| display_width(s(f, "label")))
                    .max()
                    .unwrap_or(0);
                for f in &doc[index..end] {
                    lines.extend(fact_rows(theme, s(f, "label"), s(f, "value"), pad, columns));
                }
                index = end;
                continue;
            }
            "text" => {
                let weight = if value.get("strong") == Some(&Value::Bool(true)) {
                    Tone::Strong
                } else {
                    Tone::Normal
                };
                let mark = if tone == Some("warning") {
                    Mark::Warned
                } else {
                    Mark::Blank
                };
                lines.extend(ui::item(theme, mark, s(value, "text"), weight, columns));
            }
            "next" => lines.extend(ui::para(
                theme,
                &format!("{}\n`{}`", s(value, "text"), s(value, "command")),
                Tone::Normal,
                columns,
            )),
            "code" => lines.extend(ui::command(theme, s(value, "text"), columns)),
            "item" => {
                let mark = tone_mark(tone);
                let weight = if mark == Mark::Failed {
                    Tone::Strong
                } else {
                    Tone::Normal
                };
                lines.extend(ui::item(theme, mark, s(value, "text"), weight, columns));
                if let Some(fix) = value.get("fix").and_then(Value::as_str) {
                    lines.extend(ui::fix_lines(theme, &ui::fix_steps(fix), columns));
                }
            }
            "table" => lines.extend(table(theme, value, columns)),
            _ => {}
        }
        index += 1;
    }
    lines
}

/// A `label  value` row. A value that is one long word (a chat's ID) is
/// never broken: it goes on the line under its label, or at the left edge
/// when even that is too narrow, so it still copies whole.
fn fact_rows(theme: &Theme, label: &str, value: &str, pad: usize, columns: usize) -> Vec<String> {
    let value = text(value);
    let room = ui::text_width(columns).saturating_sub(pad.max(display_width(label)) + 2);
    if value.contains(' ') || display_width(&value) <= room {
        return ui::prompt::fact_lines(theme, Mark::Blank, label, &value, pad, columns);
    }
    let mut lines = ui::para(theme, label, Tone::Normal, columns);
    if ui::INDENT + 2 + display_width(&value) < columns {
        lines.push(format!("{}{value}", " ".repeat(ui::INDENT + 2)));
    } else {
        lines.push(value);
    }
    lines
}

/// A table: a header row, then a row per chat with its name in bold, the
/// columns aligned. On a terminal too narrow for the row, each chat is
/// stacked: its name, then its other columns as labeled lines.
fn table(theme: &Theme, value: &Value, columns: usize) -> Vec<String> {
    let headers: Vec<String> = value
        .get("columns")
        .and_then(Value::as_array)
        .map(|c| c.iter().map(|v| text(&str_value(v))).collect())
        .unwrap_or_default();
    let rows: Vec<(Vec<String>, String)> = value
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let cells = row
                        .get("cells")
                        .and_then(Value::as_array)
                        .map(|c| c.iter().map(|v| text(&str_value(v))).collect())
                        .unwrap_or_default();
                    (cells, text(s(row, "note")))
                })
                .collect()
        })
        .unwrap_or_default();
    let first = |cells: &[String], note: &str| -> usize {
        display_width(cells.first().map_or("", String::as_str))
            + if note.is_empty() {
                0
            } else {
                1 + display_width(note)
            }
    };
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for (cells, note) in &rows {
        for (index, cell) in cells.iter().enumerate() {
            let w = if index == 0 {
                first(cells, note)
            } else {
                display_width(cell)
            };
            if index < widths.len() {
                widths[index] = widths[index].max(w);
            }
        }
    }
    // The last column needs no padding: its width is only its own.
    let used = ui::INDENT + widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
    let indent = " ".repeat(ui::INDENT);
    let mut lines = Vec::new();
    if used < columns.saturating_sub(1) {
        let line = |cells: Vec<String>| -> String {
            let mut out = indent.clone();
            for (index, cell) in cells.iter().enumerate() {
                out.push_str(cell);
                if index + 1 < cells.len() {
                    let shown = ui::visible_width(cell);
                    out.push_str(&" ".repeat(widths[index].saturating_sub(shown) + 2));
                }
            }
            out.trim_end().to_string()
        };
        lines.push(line(headers.clone()));
        for (cells, note) in &rows {
            let mut painted = cells.clone();
            if let Some(name) = painted.first_mut() {
                *name = theme.strong(name);
                if !note.is_empty() {
                    name.push(' ');
                    name.push_str(note);
                }
            }
            lines.push(line(painted));
        }
        return lines;
    }
    let pad = headers
        .iter()
        .skip(1)
        .map(|h| display_width(h))
        .max()
        .unwrap_or(0);
    for (number, (cells, note)) in rows.iter().enumerate() {
        if number > 0 {
            lines.push(String::new());
        }
        let mut name = cells.first().cloned().unwrap_or_default();
        if !note.is_empty() {
            name = format!("{name} {note}");
        }
        lines.extend(ui::para(theme, &name, Tone::Strong, columns));
        for (index, cell) in cells.iter().enumerate().skip(1) {
            if !cell.is_empty() {
                // Two cells in, under the chat's name.
                lines.extend(
                    fact_rows(
                        theme,
                        headers.get(index).map_or("", String::as_str),
                        cell,
                        pad,
                        columns.saturating_sub(2),
                    )
                    .into_iter()
                    .map(|line| format!("  {line}")),
                );
            }
        }
    }
    lines
}

/// Every piece of text a display holds, in order: what a reader must see
/// on either surface.
pub fn words(doc: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for value in doc {
        for key in ["text", "label", "value", "command", "fix"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                out.push(text.to_string());
            }
        }
        if let Some(columns) = value.get("columns").and_then(Value::as_array) {
            out.extend(columns.iter().map(str_value));
        }
        for row in value
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // The note follows the first cell (the chat's name).
            for (index, cell) in row
                .get("cells")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                out.push(str_value(cell));
                if index == 0 {
                    if let Some(note) = row.get("note").and_then(Value::as_str) {
                        out.push(note.to_string());
                    }
                }
            }
        }
    }
    out.retain(|w| !w.is_empty());
    out
}

#[cfg(test)]
mod tests;
