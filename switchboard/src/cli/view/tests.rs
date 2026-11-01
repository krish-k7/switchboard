//! The displays both surfaces show, checked here and written for the
//! plugin's tests (`claude-plugin/tests/display.json`), so a change to what
//! a command shows fails until both sides agree on it.

use serde_json::{json, Map, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;
const ID: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";
const REMOTE: &str = "33333333-3333-4333-8333-333333333333";

fn obj(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn local_rows() -> Value {
    json!([
        {"id": ID, "name": "Local work", "status": "idle", "enrolled": true,
         "joined_name": "release-planner", "expires_at": NOW + 6.0 * 86400.0 - 60.0,
         "project": "Local project", "repository": "example/monorepo", "branch": "tools/example"},
        {"id": OTHER, "name": "Shell work", "status": "shell", "enrolled": false}
    ])
}

fn connected() -> Map<String, Value> {
    obj(
        json!({"enabled": true, "connected": true, "machine": "laptop", "pid": 7,
        "server_url": "https://sb.example.com", "sessions": local_rows()}),
    )
}

fn roster() -> Map<String, Value> {
    obj(
        json!({"enabled": true, "machine": "laptop", "sessions": local_rows(),
        "remote_available": true, "remote_sessions": [
            {"id": REMOTE, "machine": "worker", "owner": "dana", "name": "Remote research", "status": "busy",
             "project": "Remote project", "repository": "example/monorepo", "branch": "research/example",
             "route": format!("worker/{REMOTE}")}]}),
    )
}

fn view<'a>(
    scope: Scope,
    current: Option<&'a str>,
    search: &'a str,
    details: bool,
) -> ChatsView<'a> {
    ChatsView {
        scope,
        current,
        search,
        details,
        filtered: !search.is_empty(),
        hint: Some(match scope {
            Scope::All => "switchboard chats --details".into(),
            Scope::Local => "switchboard chats --local --details".into(),
            Scope::Remote => "switchboard chats --remote --details".into(),
        }),
        machine: Some("laptop".into()),
    }
}

fn doctor_report(ok: bool) -> Map<String, Value> {
    let mut checks = vec![
        json!({"name": "program", "status": "pass", "summary": "Switchboard 0.3.0, installed in ~/.local/bin."}),
        json!({"name": "configuration", "status": "pass", "summary": "Configuration and key are private and valid."}),
    ];
    if ok {
        checks.push(json!({"name": "bridge", "status": "pass", "summary": "The bridge is running and connected."}));
    } else {
        checks.push(json!({"name": "bridge", "status": "fail", "summary": "The bridge isn't running.", "fix": "Run `switchboard up`."}));
        checks.push(json!({"name": "discovery", "status": "warn", "summary": "Chats are found by polling, so new chats can take a few seconds to appear."}));
    }
    checks.push(json!({"name": "server", "status": "pass", "summary": "The server knows this machine as laptop (owner k)."}));
    checks.push(json!({"name": "chats", "status": "pass", "summary": "1 chat joined, open now."}));
    obj(json!({"ok": ok, "checks": checks}))
}

/// Every display the tests and the plugin's tests check, by name.
pub fn cases() -> Vec<(&'static str, Doc)> {
    let mut not_connected = connected();
    not_connected.insert("connected".into(), json!(false));
    not_connected.remove("pid");
    not_connected.insert("detail".into(), json!("The bridge reports it is offline."));
    let mut stopped = connected();
    stopped.insert("connected".into(), json!(false));
    stopped.insert("machine".into(), Value::Null);
    stopped.insert("detail".into(), json!("the bridge is not running"));
    let mut down_status = stopped.clone();
    down_status.insert("down".into(), json!(true));
    let mut off = stopped.clone();
    off.insert("enabled".into(), json!(false));
    let mut none_joined = connected();
    none_joined.insert(
        "sessions".into(),
        json!([{"id": OTHER, "name": "Shell work", "status": "shell", "enrolled": false}]),
    );
    let mut unavailable = roster();
    unavailable.insert("remote_available".into(), json!(false));
    unavailable.insert("remote_sessions".into(), json!([]));
    unavailable.insert(
        "remote_error".into(),
        json!(
            "Chats on other machines are unavailable: the server didn't answer within 8 seconds."
        ),
    );
    let mut empty = unavailable.clone();
    empty.insert("sessions".into(), json!([]));
    let mut doctor_down = doctor_report(true);
    doctor_down["checks"][2] = json!({"name": "bridge", "status": "pass", "summary": "Down. Run `switchboard up` to reconnect.", "down": true});
    vec![
        ("status-connected-in-chat", status(&connected(), Some(ID), None, false, NOW)),
        ("status-connected-chat-not-joined", status(&connected(), Some(OTHER), None, false, NOW)),
        ("status-connected-terminal", status(&connected(), None, None, false, NOW)),
        ("status-none-joined", status(&none_joined, None, None, false, NOW)),
        ("status-update", status(&connected(), Some(ID), Some("0.3.1"), false, NOW)),
        ("status-not-connected", status(&not_connected, Some(ID), None, false, NOW)),
        ("status-not-running", status(&stopped, None, None, false, NOW)),
        ("status-no-service", status(&stopped, None, None, true, NOW)),
        ("status-turned-off", status(&off, None, None, false, NOW)),
        ("status-down", status(&down_status, Some(ID), None, false, NOW)),
        ("status-not-set-up", not_set_up()),
        ("chats", chats(&roster(), &view(Scope::All, Some(ID), "", false), NOW)),
        ("chats-details", chats(&roster(), &view(Scope::All, Some(ID), "", true), NOW)),
        ("chats-local-search", chats(&roster(), &view(Scope::Local, Some(ID), "solver", false), NOW)),
        ("chats-remote-unavailable", chats(&unavailable, &view(Scope::All, Some(ID), "", false), NOW)),
        ("chats-nothing-here", chats(&empty, &view(Scope::All, None, "nothing", false), NOW)),
        ("chats-remote-only-unavailable", chats(&unavailable, &view(Scope::Remote, None, "", false), NOW)),
        ("doctor-problems", doctor(&doctor_report(false))),
        ("doctor-all-passed", doctor(&doctor_report(true))),
        ("doctor-down", doctor(&doctor_down)),
        ("join", selection(&obj(json!({"ok": true, "action": "join", "session": ID, "name": "solver", "confirmed": true, "remote_sessions": 4, "remote_machines": 2})))),
        ("join-alone", selection(&obj(json!({"ok": true, "action": "join", "session": ID, "name": "solver", "confirmed": true, "remote_sessions": 0, "remote_machines": 0})))),
        ("join-not-live", selection(&obj(json!({"ok": true, "action": "join", "session": ID, "name": "solver", "confirmed": false, "detail": "the bridge is not running (switchboard up)"})))),
        ("join-renamed", selection(&obj(json!({"ok": true, "action": "join", "session": ID, "name": "second", "confirmed": false, "published_name": "first", "detail": "the bridge has not published the new name within 10 seconds"})))),
        ("leave", selection(&obj(json!({"ok": true, "action": "leave", "session": ID, "name": "solver", "confirmed": true})))),
        ("leave-unchanged", selection(&obj(json!({"ok": true, "action": "leave", "session": ID, "name": "solver", "changed": false})))),
        ("leave-unconfirmed", selection(&obj(json!({"ok": true, "action": "leave", "session": ID, "name": "solver", "confirmed": false, "detail": "the bridge is not running (switchboard up)"})))),
        ("ping", ping_reply(&obj(json!({"ok": true, "machine": "worker", "name": "research", "rtt_ms": 182.4, "via": "server"})))),
        ("ping-failed", ping_failed("worker/nothing", "no joined chat on another machine matches worker/nothing; run switchboard chats --remote to see them")),
        ("up", up(&obj(json!({"ok": true, "action": "up", "connected": true, "server_url": "https://sb.example.com", "machine": "laptop"})))),
        ("up-not-connected", up(&obj(json!({"ok": true, "action": "up", "connected": false, "server_url": "https://sb.example.com"})))),
        ("down", down()),
        ("help", help()),
    ]
}

/// Words as compared: without backticks, case, or a final period (a fix
/// reads on after `To fix,` in lowercase).
fn normalized(value: &str) -> String {
    value
        // Backticks quote commands; a command too long for its line goes on
        // with a shell continuation.
        .replace('`', "")
        .replace(" \\\n", " ")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches('.')
        .to_string()
}

/// The terminal drawing shows every word of the display, in order.
#[test]
fn the_terminal_shows_every_word_in_order() {
    for (name, doc) in cases() {
        for columns in [100, 60, 36] {
            let lines = render(&doc, &Theme::plain(), columns);
            for line in &lines {
                assert!(
                    display_width(line) < columns.max(40),
                    "{name} at {columns}: {line:?}"
                );
            }
            let shown = normalized(&lines.join("\n"));
            // A table too wide for the terminal stacks each chat, with the
            // column names as labels: check the rest in order.
            let shown_doc: Doc = if columns >= 100 {
                doc.clone()
            } else {
                doc.iter()
                    .map(|block| {
                        let mut block = block.clone();
                        if let Some(map) = block.as_object_mut() {
                            map.remove("columns");
                        }
                        block
                    })
                    .collect()
            };
            let mut at = 0;
            for word in words(&shown_doc) {
                let word = normalized(&word);
                match shown[at..].find(&word) {
                    Some(found) => at += found + word.len(),
                    None => panic!(
                        "{name} at {columns} lacks {word:?} after {at}:\n{}",
                        lines.join("\n")
                    ),
                }
            }
            let colored = render(
                &doc,
                &Theme {
                    color: true,
                    unicode: true,
                },
                columns,
            );
            assert_eq!(
                colored
                    .iter()
                    .map(|l| ui::strip_codes(l))
                    .collect::<Vec<_>>(),
                lines,
                "{name}"
            );
        }
    }
}

#[test]
fn status_answers_one_question() {
    let plain = |doc: &Doc| render(doc, &Theme::plain(), 100).join("\n");
    assert_eq!(
        plain(&status(&connected(), Some(ID), None, false, NOW)),
        "✓ Connected to sb.example.com as laptop\n  This chat   Joined as release-planner, expires in 6 days\n  Chats here  1 of 2 joined"
    );
    // In a terminal there is no current chat.
    assert_eq!(
        plain(&status(&connected(), None, Some("0.3.1"), false, NOW)),
        "✓ Connected to sb.example.com as laptop\n  Chats here  1 of 2 joined\n\n  Switchboard 0.3.1 is available. Run switchboard upgrade."
    );
    let mut down = connected();
    down.insert("connected".into(), json!(false));
    down.insert("down".into(), json!(true));
    assert_eq!(
        plain(&status(&down, None, None, false, NOW)),
        "  Down. Run switchboard up to reconnect.\n  Chats here  1 of 2 joined"
    );
    let mut stopped = connected();
    stopped.insert("connected".into(), json!(false));
    stopped.insert("detail".into(), json!("the bridge is not running"));
    assert_eq!(
        plain(&status(&stopped, None, None, false, NOW)),
        "✗ Not connected: the bridge isn't running.\n  Chats here  1 of 2 joined\n\n  To reconnect, run\n      switchboard up"
    );
    // No chat lists, IDs, or check results.
    let all = plain(&status(&connected(), Some(ID), Some("0.3.1"), false, NOW));
    assert!(!all.contains(ID) && !all.contains("Local work") && !all.contains("passed"));
}

#[test]
fn chats_list_each_machine_as_a_table() {
    let plain = |doc: &Doc| render(doc, &Theme::plain(), 100).join("\n");
    assert_eq!(
        plain(&chats(
            &roster(),
            &view(Scope::All, Some(ID), "", false),
            NOW
        )),
        [
            "  Chats",
            "",
            "  laptop (this machine)",
            "  Chat                    Activity  Membership                 Expires",
            "  Local work (this chat)  Idle      Joined as release-planner  in 6 days",
            "  Shell work              Shell     Not joined",
            "",
            "  worker",
            "  Chat             Activity",
            "  Remote research  Busy",
            "",
            "  Full IDs and details:",
            "      switchboard chats --details",
        ]
        .join("\n")
    );
    let details = plain(&chats(&roster(), &view(Scope::All, None, "", true), NOW));
    assert!(
        details.contains(&format!("  ID          {ID}")),
        "{details}"
    );
    assert!(details.contains("  Branch      tools/example"), "{details}");
    assert!(!details.contains("Full IDs"), "{details}");
    // A chat list says nothing about the connection beyond why other
    // machines' chats are missing.
    assert!(
        !plain(&chats(&roster(), &view(Scope::All, None, "", false), NOW)).contains("Connected")
    );
    // At most CHAT_LIMIT chats, and a way to narrow the list.
    let mut many = roster();
    many.insert(
        "sessions".into(),
        Value::Array(
            (0..60)
                .map(|n| json!({"id": format!("{n:08}-1111-4111-8111-111111111111"), "name": format!("chat {n}"), "status": "idle", "enrolled": false}))
                .collect(),
        ),
    );
    let long = plain(&chats(&many, &view(Scope::Local, None, "", false), NOW));
    assert!(long.contains("Showing 50 of 60 chats."), "{long}");
}

#[test]
fn doctor_lists_problems_first_and_passes_one_per_line() {
    let plain = |doc: &Doc| render(doc, &Theme::plain(), 100).join("\n");
    assert_eq!(
        plain(&doctor(&doctor_report(true))),
        "✓ Switchboard 0.3.0, installed in ~/.local/bin.\n✓ Configuration and key are private and valid.\n✓ The bridge is running and connected.\n✓ The server knows this machine as laptop (owner k).\n\n  All checks passed."
    );
    let problems = plain(&doctor(&doctor_report(false)));
    assert!(problems.starts_with("✗ The bridge isn't running.\n  To fix, run\n      switchboard up\n\n! Chats are found by polling"), "{problems}");
    assert!(
        problems.ends_with("\n\n  1 problem to fix, and 1 warning."),
        "{problems}"
    );
    assert!(!problems.contains("chat joined"), "{problems}");
}

/// What the plugin's tests read: each display, its words, and the plugin's
/// command names, as a module (the plugin's test runner loads only code).
/// Rewritten with `SWITCHBOARD_BLESS=1`.
#[test]
fn the_plugin_fixture_matches() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../claude-plugin/tests/display-fixture.js"
    );
    let cases: Vec<Value> = cases()
        .into_iter()
        .map(|(name, doc)| json!({"name": name, "words": words(&doc), "display": doc}))
        .collect();
    let fixture = json!({
        "slash_commands": SLASH_COMMANDS,
        "cases": cases,
    });
    let text = format!(
        "// What each command shows, as the CLI's --json --display adds it. Written by\n// switchboard/src/cli/view/tests.rs: SWITCHBOARD_BLESS=1 cargo test the_plugin_fixture_matches\n// The plugin's tests check that it draws every word of every case.\nexport default {}\n",
        serde_json::to_string_pretty(&fixture).unwrap()
    );
    if std::env::var_os("SWITCHBOARD_BLESS").is_some() {
        std::fs::write(path, &text).unwrap();
    }
    let current = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        current == text,
        "{path} is out of date: what a command shows changed. Rerun with SWITCHBOARD_BLESS=1, then run the plugin's tests (claude plugin test in claude-plugin/)"
    );
}
