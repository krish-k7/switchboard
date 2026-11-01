//! The command-line role: the user commands, with human output and `--json`.
//!
//! Output: results go to stdout. Errors go to stderr in the visual language
//! of `ui`: a cross and what went wrong, then `To fix:` and what to do,
//! except with `--json`, where every result and error is one JSON document
//! on stdout, so scripts and the Claude Code plugin read a single stream.
//! Usage errors exit 2, other failures 1.
//!
//! Submodules: `args` (parsing and help), `format` (human output), `local`
//! (config, runtime, and local session reads), `view` (what each command
//! shows, shared with the Claude Code plugin), `doctor`, `roster`, `ping`,
//! `machines` (setup with an invite), `service` (up and down), `logs`,
//! `completion`, `update` (the check for a newer release), `install` (the
//! installer), `upgrade` (upgrade and uninstall), `release` (downloading
//! releases), `init` (the setup wizard), and `ui` (the visual language
//! prompts and human output share).

pub mod args;
pub mod completion;
pub mod doctor;
pub mod format;
pub mod init;
pub mod install;
pub mod local;
pub mod logs;
pub mod machines;
pub mod ping;
pub mod release;
pub mod roster;
pub mod service;
mod sha1;
pub mod ui;
mod unicode_cli;
pub mod update;
pub mod upgrade;
pub mod view;

use std::io::Write;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::error::{Error, Kind};
use crate::fmtutil::dumps_indent;
use crate::membership;
use crate::wirejson::{self, str_value};
use args::{Args, Parsed};
use format::text;

/// Print to stdout, ignoring a closed pipe (as a CLI must not panic).
pub fn out(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.flush();
}

/// Write to stdout; false once nobody reads it (a closed pipe).
pub fn out_ok(line: &str) -> bool {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// Whether the reader of descriptor `fd` is gone: a pipe whose other end
/// was closed (`logs --follow | head`). Asked without writing, so a follow
/// that has nothing new to print still ends.
pub fn reader_gone(fd: std::os::fd::RawFd) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: poll on one initialized pollfd, without waiting.
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    ready > 0 && poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

pub fn err(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(line.as_bytes());
    let _ = stderr.flush();
}

/// Whether an error is a missing file: OS error messages start
/// `[Errno N] `, and N is 2 for a missing file.
pub fn is_file_not_found(error: &Error) -> bool {
    error.kind == Kind::Os && error.message.starts_with("[Errno 2] ")
}

fn is_permission(error: &Error) -> bool {
    error.kind == Kind::Os
        && (error.message.starts_with("[Errno 13] ") || error.message.starts_with("[Errno 1] "))
}

/// The error for a machine without a configuration (scripts and the
/// Claude Code plugin read it in `--json` errors).
pub const NOT_SET_UP: &str = "Switchboard isn't set up on this machine; run switchboard init";

/// The error for `start` without the bridge's service file.
pub const NO_SERVICE: &str = "the bridge's background service isn't installed; run switchboard install --start to install it and start the bridge";

/// The command being run, for error messages that depend on it.
static COMMAND: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The fix to suggest for an error, if any (commands in backticks).
pub fn error_hint(message: &str, error: Option<&Error>) -> Option<&'static str> {
    if message.contains("badly formed hexadecimal UUID") {
        return Some("run `switchboard chats` to find the chat's full ID.");
    }
    if message.to_ascii_lowercase().starts_with("could not reach") {
        return Some("check that this machine is online and that the address can be reached (for your own server: that it is running), then try again.");
    }
    if message.contains("the bridge did not")
        || message.contains("bridge process")
        || message.contains("runtime file")
    {
        return Some(
            "run `switchboard doctor` to check the bridge and its connection to the server.",
        );
    }
    if error.is_some_and(is_file_not_found) {
        return Some("run `switchboard init`, or pass --config (or set SWITCHBOARD_CONFIG) if the configuration is elsewhere.");
    }
    if error.is_some_and(is_permission)
        || message.contains("mode 0600")
        || message.to_ascii_lowercase().contains("unsafe")
    {
        return Some("run `switchboard doctor` to find the private files that need attention.");
    }
    None
}

/// The message to show for an error: a missing configuration file says
/// that Switchboard is not set up, instead of the raw file error.
fn error_message(error: &Error, config_path: &str) -> String {
    if is_file_not_found(error) && error.message.contains(config_path) {
        return NOT_SET_UP.to_string();
    }
    error.message.clone()
}

/// `text` with the first letter capitalized and a period at the end.
fn sentence(text: &str) -> String {
    let mut chars = text.trim().chars();
    let mut out = match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => return String::new(),
    };
    if !out.ends_with(['.', '?', '!']) {
        out.push('.');
    }
    out
}

/// `text` with the first letter in lowercase (for after `To fix:`), unless
/// it starts a name.
fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if !text.starts_with("Switchboard") && !text.starts_with("Claude") => {
            format!("{}{}", first.to_lowercase(), chars.as_str())
        }
        _ => text.to_string(),
    }
}

/// `text` with the `switchboard ...` commands in it in backticks, so they
/// are drawn as commands. A command ends at punctuation that ends a clause.
pub fn quote_commands(text: &str) -> String {
    if text.contains('`') {
        return text.to_string();
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(found) = rest.find("switchboard ") {
        let starts_word = found == 0 || rest[..found].ends_with([' ', '(']);
        // `sudo switchboard ...` is one command.
        let at = if starts_word && rest[..found].ends_with("sudo ") {
            found - "sudo ".len()
        } else {
            found
        };
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        if !starts_word {
            out.push_str("switchboard ");
            rest = &from["switchboard ".len()..];
            continue;
        }
        const ENDS: [&str; 11] = [
            " if ", " to ", " when ", " and ", " or ", " on ", " then ", " under ", " instead",
            " for ", " in ",
        ];
        let mut end = from.len();
        for (index, c) in from.char_indices() {
            let next = from[index + c.len_utf8()..].chars().next();
            if matches!(c, ',' | ';' | ')' | '(')
                || (matches!(c, '.' | '?' | '!') && next.is_none_or(|n| n == ' '))
                || (c == ' ' && ENDS.iter().any(|end| from[index..].starts_with(end)))
            {
                end = index;
                break;
            }
        }
        out.push('`');
        out.push_str(&from[..end]);
        out.push('`');
        rest = &from[end..];
    }
    out.push_str(rest);
    out
}

/// What went wrong and the fix, for a message from a command.
pub fn problem_for(message: &str, fix: Option<&str>, command: &str) -> ui::prompt::Problem {
    use ui::prompt::Problem;
    // An OS error's `[Errno N] ` prefix says nothing to a person.
    let message = init::join::strip_errno(&text(message));
    if message.starts_with("Switchboard isn't set up on this machine") {
        return Problem::new("This machine isn't set up yet.").fix("run `switchboard init`.");
    }
    if message == local::NOT_IN_CHAT {
        let (verb, other) = if command == "leave" {
            ("disconnects", "leave")
        } else {
            ("connects", "join")
        };
        return Problem::new(format!(
            "`switchboard {other}` {verb} the chat it runs in, and this is a terminal."
        ))
        .fix(format!(
            "run `/switchboard {other}` inside the chat, or pick a chat from `switchboard chats` and run `switchboard {other} --chat ID`."
        ));
    }
    if let Some(rest) = message.strip_prefix(local::NO_SUCH_CHAT) {
        let id = rest.split(';').next().unwrap_or("").trim();
        return Problem::new(format!("No open chat has the ID {id}."))
            .fix("run `switchboard chats` to find its full ID.");
    }
    if let Some(rest) = message.strip_prefix(NO_JOINED_CHAT) {
        let id = rest.split(';').next().unwrap_or("").trim();
        return Problem::new(format!(
            "No chat with the ID {id} is joined on this machine."
        ))
        .fix("run `switchboard chats` to find its ID.");
    }
    if message == NO_SERVICE {
        return Problem::new("The bridge's background service isn't installed.")
            .fix("run `switchboard install --start` to install it and start the bridge.");
    }
    if let Some(rest) = message.strip_prefix("unknown command ") {
        let name = rest
            .split(';')
            .next()
            .unwrap_or("")
            .trim_matches('\'')
            .to_string();
        let suggestion = rest
            .split("did you mean ")
            .nth(1)
            .and_then(|s| s.split('?').next())
            .map(str::to_string);
        return Problem::new(format!("There's no command called {name}.")).fix(match suggestion {
            Some(command) => {
                format!("run `switchboard {command}`, or `switchboard help` to see the commands.")
            }
            None => "run `switchboard help` to see the commands.".into(),
        });
    }
    // What follows "; " is the fix when it says what to do; otherwise it is
    // part of what happened ("...; nothing was removed").
    const ACTIONS: [&str; 25] = [
        "run ", "pass ", "ask ", "choose ", "use ", "set ", "add ", "try ", "leave ", "wait ",
        "check ", "update ", "install ", "remove ", "move ", "open ", "copy ", "make ", "start ",
        "stop ", "see ", "fix ", "name ", "restart ", "put ",
    ];
    let (head, rest) = match message.split_once("; ") {
        Some((head, rest))
            if ACTIONS
                .iter()
                .any(|verb| rest.to_ascii_lowercase().starts_with(verb)) =>
        {
            (head.to_string(), Some(rest.to_string()))
        }
        _ => (message.clone(), None),
    };
    // The message's own fix is more specific than a hint for its kind.
    let fix = rest
        .map(|r| sentence(&lowercase_first(&r)))
        .or_else(|| fix.map(str::to_string));
    let mut problem = Problem::new(sentence(&head));
    if let Some(fix) = fix {
        problem = problem.fix(quote_commands(&lowercase_first(&fix)));
    }
    problem
}

/// A usage message from a server command (`switchboard server X: what`,
/// then its usage lines), as a problem on stderr that points at `help`.
pub fn print_usage_text(text: &str, help: &str) {
    let first = text.lines().next().unwrap_or("");
    let message = first.split_once(": ").map_or(first, |(_, m)| m);
    print_problem(&init::usage_problem(message, help));
}

/// A failure message (`what; how to fix it`) as a problem on stderr.
pub fn print_failure(message: &str) {
    print_problem(&problem_for(message, None, ""));
}

/// Print a problem on stderr.
pub fn print_problem(problem: &ui::prompt::Problem) {
    let theme = ui::Theme::for_fd(2);
    let lines = ui::prompt::problem_lines(&theme, problem, ui::term::columns());
    err(&format!("{}\n", lines.join("\n")));
}

/// Print a help page on stdout, fitted to the terminal and styled.
pub fn print_help(text: &str) {
    let theme = ui::Theme::for_fd(1);
    out(&args::styled_help(
        &args::fit(text, format::output_width()),
        &theme,
    ));
}

/// Print an error: JSON on stdout with `--json`, else a problem on stderr.
pub fn print_error(message: &str, error: Option<&Error>, json_output: bool) {
    print_error_hint(message, error_hint(message, error), json_output)
}

fn print_error_hint(message: &str, hint: Option<&str>, json_output: bool) {
    if json_output {
        let mut value = Map::new();
        value.insert("ok".into(), Value::Bool(false));
        value.insert("error".into(), Value::from(message));
        out(&format!("{}\n", wirejson::dumps(&Value::Object(value))));
    } else {
        let command = COMMAND.get().map(String::as_str).unwrap_or("");
        print_problem(&problem_for(message, hint, command));
    }
}

/// The error for `leave --chat ID` when no chat with that ID is joined.
pub const NO_JOINED_CHAT: &str = "no chat is joined on this machine with the ID";

fn home() -> String {
    local::home_dir()
}

fn default_config() -> String {
    match std::env::var("SWITCHBOARD_CONFIG") {
        Ok(value) => crate::fsutil::normalize(&value),
        Err(_) => crate::fsutil::normalize(&format!("{}/.config/switchboard/bridge.json", home())),
    }
}

/// The help text.
pub fn help_text() -> String {
    args::HELP.to_string()
}

/// Whether `config` is a configuration that exists.
fn set_up(config: &str) -> bool {
    crate::fsutil::lstat(config).is_ok()
}

fn validate(args: &Args) -> Option<String> {
    let command = args.command.as_str();
    let only = |options: &str, commands: &str| {
        let verb = if options.contains(' ') {
            "work"
        } else {
            "works"
        };
        Some(format!("{options} only {verb} with {commands}"))
    };
    if command != "sessions"
        && (args.local || args.remote || args.name.is_some() || args.project.is_some())
    {
        return only("--local, --remote, --name, and --project", "chats");
    }
    if args.details && command != "sessions" {
        return only("--details", "chats");
    }
    if args.machine.is_some() && command != "sessions" && command != "setup" {
        return only("--machine", "chats");
    }
    if command != "setup" && (args.owner.is_some() || args.no_start || args.invite.is_some()) {
        return only("--owner, --no-start, and --invite", "init");
    }
    if command != "setup" && (args.hub_url.is_some() || args.server_pin.is_some()) {
        return only("--server-url and --server-pin", "setup");
    }
    if args.force && command != "upgrade" {
        return only("--force", "upgrade");
    }
    if args.purge && command != "uninstall" {
        return only("--purge", "uninstall");
    }
    if args.session.is_some() && !["join", "leave", "status", "sessions"].contains(&command) {
        return only("--chat", "join, leave, status, and chats");
    }
    if (args.follow || args.lines.is_some()) && command != "logs" {
        return only("--follow and --lines", "logs");
    }
    if args.words.is_some() && command != "completion" {
        return only("--words", "completion");
    }
    if args.target.is_some()
        && !["join", "ping", "upgrade", "help", "sessions", "completion"].contains(&command)
    {
        return Some(format!(
            "{command} takes no argument (unexpected {})",
            args.target.as_deref().unwrap_or("")
        ));
    }
    if command == "logs" {
        if let Some(lines) = &args.lines {
            if !lines
                .parse::<usize>()
                .is_ok_and(|n| (1..=logs::MAX_LINES).contains(&n))
            {
                return Some(format!(
                    "--lines needs a number from 1 to {} (not {lines})",
                    logs::MAX_LINES
                ));
            }
        }
    }
    if command == "completion" && args.words.is_none() {
        match args.target.as_deref() {
            None => return Some("completion needs a shell: bash, zsh, or fish".into()),
            Some(shell) if !completion::SHELLS.contains(&shell) => {
                return Some(format!(
                    "completion works with bash, zsh, or fish, not {shell}"
                ))
            }
            _ => {}
        }
    }
    None
}

/// `switchboard cli [--config PATH] COMMAND ...`.
pub fn main(argv: &[String]) -> i32 {
    // The installed command is a wrapper that runs `switchboard cli "$@"`, so
    // `switchboard server invites` typed at it arrives here as `server
    // invites`: the other roles, and `--version`, go where the binary itself
    // sends them.
    let first = argv.first().map(String::as_str);
    if first.is_some_and(|role| crate::ROLES.contains(&role))
        || (first == Some("--version") && argv.len() == 1)
    {
        return crate::run(argv);
    }
    // `init` has its own options (see `init.rs`).
    if argv.first().map(String::as_str) == Some("init") {
        return init::main(&argv[1..]);
    }
    let json_in_argv = argv.iter().any(|a| a == "--json");
    let usage = |message: &str, command: Option<&str>| {
        if json_in_argv {
            print_error_hint(message, None, true);
        } else {
            let help = match command {
                Some(command) if command != "help" => format!("switchboard {command} --help"),
                _ => "switchboard help".into(),
            };
            let problem = if message.starts_with("unknown command ") {
                problem_for(message, None, "")
            } else {
                init::usage_problem(message, &help)
            };
            print_problem(&problem);
        }
        2
    };
    let mut args = match args::parse(argv, &default_config()) {
        Parsed::Help(command) if command.as_deref() == Some("init") => {
            print_help(init::HELP);
            return 0;
        }
        Parsed::Help(command) => {
            print_help(&args::command_help(command.as_deref()));
            return 0;
        }
        Parsed::Error(message, command) => return usage(&message, command.as_deref()),
        Parsed::Args(args) => *args,
    };
    // `chats` is the name people see; `sessions` the one older versions
    // and the JSON keep.
    let shown_command = args.command.clone();
    if args.command == "chats" {
        args.command = "sessions".into();
    }
    let _ = COMMAND.set(shown_command.clone());
    let parser_error = |message: &str| usage(message, Some(&shown_command));
    if args.command == "init" {
        // Options before `init` (`switchboard --json init`): init parses all.
        let mut rest = argv.to_vec();
        if let Some(index) = rest.iter().position(|a| a == "init") {
            rest.remove(index);
        }
        return init::main(&rest);
    }
    if let Some(message) = validate(&args) {
        return parser_error(&message);
    }
    if args.command == "setup" {
        return machines::command(&args);
    }
    if args.command == "completion" {
        match &args.words {
            Some(kind) => {
                for word in completion::words(kind, &args.config) {
                    out(&format!("{word}\n"));
                }
            }
            None => {
                out(&completion::script(args.target.as_deref().unwrap_or("")).unwrap_or_default())
            }
        }
        return 0;
    }
    if args.command == "logs" {
        let lines = args
            .lines
            .as_deref()
            .and_then(|n| n.parse().ok())
            .unwrap_or(logs::DEFAULT_LINES);
        return logs::run(lines, args.follow, args.json);
    }
    let mut join_name = None;
    if args.command == "join" {
        if let Some(target) = &args.target {
            let name = membership::peer_name(target);
            if name.is_empty() {
                return parser_error("the name needs at least one letter or digit");
            }
            join_name = Some(name);
        }
    }
    if args.command == "ping" {
        let mut result = ping::ping(&args.config, args.target.as_deref());
        let ok = result.get("ok") == Some(&Value::Bool(true));
        let target = args.target.clone().unwrap_or_default();
        let error = result.get("error").map(str_value).unwrap_or_default();
        let doc = if ok {
            view::ping_reply(&result)
        } else {
            view::ping_failed(&target, &error)
        };
        if args.json {
            if args.display {
                result.insert("display".into(), Value::Array(doc));
            }
            out(&format!("{}\n", wirejson::dumps(&Value::Object(result))));
        } else if ok {
            out(&format!("{}\n", render(&doc, 1).join("\n")));
        } else {
            err(&format!("{}\n", render(&doc, 2).join("\n")));
        }
        return if ok { 0 } else { 1 };
    }
    if args.command == "upgrade" || args.command == "uninstall" {
        return lifecycle(&args);
    }
    if args.command == "help" {
        // `switchboard help COMMAND` is that command's help.
        let help = match args.target.as_deref() {
            Some("init") => init::HELP.to_string(),
            Some(command) if args::COMMANDS.contains(&command) => args::command_help(Some(command)),
            Some("server") => return crate::server::main(&["--help".to_string()]),
            Some(other) => return parser_error(&format!("there's no command called {other}")),
            None => help_text(),
        };
        if argv.is_empty() && !args.json && !set_up(&args.config) {
            // `switchboard` alone on a new machine: the next step first.
            let theme = ui::Theme::for_fd(1);
            let lines = ui::para(
                &theme,
                "Switchboard isn't set up on this machine yet. To set it up, run\n`switchboard init`",
                ui::Tone::Normal,
                format::output_width(),
            );
            out(&format!("{}\n\n", lines.join("\n")));
        }
        if args.json {
            let mut value = Map::new();
            value.insert("ok".into(), Value::Bool(true));
            value.insert("help".into(), Value::from(help));
            if args.display {
                value.insert("display".into(), Value::Array(view::help()));
            }
            out(&format!(
                "{}\n",
                dumps_indent(&Value::Object(value), 2, false)
            ));
        } else {
            print_help(&help);
        }
        return 0;
    }
    match run(&args, join_name) {
        Ok(code) => code,
        Err(error) => {
            let message = error_message(&error, &args.config);
            if args.command == "status" && !args.json && message == NOT_SET_UP {
                // Status answers the question it is asked: not set up.
                out(&format!("{}\n", render(&view::not_set_up(), 1).join("\n")));
                return 1;
            }
            print_error(&message, Some(&error), args.json);
            1
        }
    }
}

/// A display drawn for standard output (`fd` 1) or error (2).
fn render(doc: &[Value], fd: i32) -> Vec<String> {
    view::render(doc, &ui::Theme::for_fd(fd), format::output_width())
}

/// `upgrade` and `uninstall`.
fn lifecycle(args: &Args) -> i32 {
    let json = args.json;
    let say = move |line: &str| {
        if !json {
            out(&format!("{line}\n"));
        }
    };
    let result = if args.command == "upgrade" {
        upgrade::upgrade(args.target.as_deref(), args.force, &say)
    } else {
        upgrade::uninstall(args.purge)
    };
    match result {
        Ok(result) => {
            if json {
                out(&format!(
                    "{}\n",
                    dumps_indent(&Value::Object(result), 2, false)
                ));
            } else {
                let lines = if args.command == "upgrade" {
                    upgrade::upgrade_lines(&result, args.target.is_some())
                } else {
                    upgrade::uninstall_lines(&result)
                };
                out(&format!("{}\n", lines.join("\n")));
            }
            0
        }
        Err(error) => {
            print_error_hint(&error.message, None, json);
            1
        }
    }
}

/// The chat the command runs in: the one named with `--chat`, else the one
/// whose messaging socket Claude Code gave this process, if any.
fn current_chat(records: &[crate::discovery::Record], explicit: Option<&str>) -> Option<String> {
    if let Some(id) = explicit.filter(|id| !id.is_empty()) {
        return Some(id.to_string());
    }
    let raw = std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").unwrap_or_default();
    let target = raw.strip_prefix("uds:").unwrap_or(&raw).trim_matches('"');
    if target.is_empty() {
        return None;
    }
    let wanted = crate::fsutil::realpath(target).ok()?;
    let mut found = records.iter().filter(|record| {
        record
            .get("messagingSocketPath")
            .and_then(Value::as_str)
            .and_then(|path| crate::fsutil::realpath(path).ok())
            .is_some_and(|path| path == wanted)
    });
    let record = found.next()?;
    if found.next().is_some() {
        return None;
    }
    record
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Whether a chat row matches every search word (in its name, the name
/// other machines see, ID, machine, project, repository, or branch).
fn matches_search(row: &Map<String, Value>, machine: &str, words: &[String]) -> bool {
    let mut haystack = String::new();
    for key in [
        "name",
        "joined_name",
        "id",
        "machine",
        "project",
        "repository",
        "branch",
    ] {
        haystack.push_str(&format::text_value(row.get(key)));
        haystack.push(' ');
    }
    haystack.push_str(machine);
    let haystack = format::casefold(&haystack);
    words
        .iter()
        .all(|word| haystack.contains(&format::casefold(word)))
}

/// The command that shows the same chats with full IDs and details.
fn details_command(args: &Args) -> String {
    let mut command = vec!["switchboard".to_string(), "chats".to_string()];
    if args.local {
        command.push("--local".into());
    }
    if args.remote {
        command.push("--remote".into());
    }
    command.push("--details".into());
    for (option, value) in [
        ("--name", &args.name),
        ("--project", &args.project),
        ("--machine", &args.machine),
    ] {
        if let Some(value) = value {
            command.push(option.into());
            command.push(crate::fmtutil::shlex_quote(value));
        }
    }
    if let Some(search) = args.target.as_deref().filter(|s| !s.trim().is_empty()) {
        command.push("--".into());
        command.push(crate::fmtutil::shlex_quote(search.trim()));
    }
    command.join(" ")
}

fn run(args: &Args, join_name: Option<String>) -> Result<i32, Error> {
    if args.command == "doctor" {
        let result = doctor::run_checks(&args.config);
        let ok = result.get("ok") == Some(&Value::Bool(true));
        let doc = view::doctor(&result);
        out(&format!(
            "{}\n",
            if args.json {
                let mut plain = doctor::plain(result);
                if args.display {
                    plain.insert("display".into(), Value::Array(doc));
                }
                dumps_indent(&Value::Object(plain), 2, false)
            } else {
                render(&doc, 1).join("\n")
            }
        ));
        return Ok(if ok { 0 } else { 1 });
    }
    let mut result = Map::new();
    let mut doc: view::Doc = Vec::new();
    // `status` prints its answer before waiting on the update check.
    let mut update_check: Option<update::Check> = None;
    let up = args.command == "up" || args.command == "start";
    let down = args.command == "down" || args.command == "stop";
    if up || down {
        // Act only on a set-up machine (and start only with its service).
        if !set_up(&args.config) {
            return Err(Error::value(NOT_SET_UP));
        }
        result.insert("ok".into(), Value::Bool(true));
        result.insert("action".into(), Value::from(args.command.clone()));
        if up {
            if service::supported() && !service::installed() {
                return Err(Error::value(NO_SERVICE));
            }
            service::bring_up(&args.config)?;
            let (connected, url, machine) = wait_connected(&args.config, !args.json);
            result.insert("connected".into(), Value::Bool(connected));
            if let Some(url) = url {
                result.insert("server_url".into(), Value::from(url));
            }
            if let Some(machine) = machine {
                result.insert("machine".into(), Value::from(machine));
            }
            doc = view::up(&result);
        } else {
            let was_running = service::take_down(&args.config)?;
            result.insert("down".into(), Value::Bool(true));
            result.insert("stopped".into(), Value::Bool(was_running));
            doc = view::down();
        }
    } else if args.command == "join" || args.command == "leave" {
        let before = if args.command == "leave" {
            local::private_json(&args.config).ok()
        } else {
            None
        };
        let (session, config) = local::edit_config(
            &args.config,
            &args.command,
            args.session.as_deref(),
            join_name.clone(),
        )?;
        let sid = session.get("sessionId").map(str_value).unwrap_or_default();
        let was_joined = before.as_ref().is_some_and(|config| {
            config
                .get("sessions")
                .and_then(Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(sid.as_str())))
        });
        let chosen = before.as_ref().and_then(|config| {
            config
                .get("names")
                .and_then(Value::as_object)
                .and_then(|names| names.get(&sid))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        result.insert("ok".into(), Value::Bool(true));
        result.insert("action".into(), Value::from(args.command.clone()));
        result.insert("session".into(), Value::from(sid.clone()));
        // The name peers see (or saw): the chosen name, else the chat's own.
        let live = session.get("sessionId").is_some() && session.contains_key("pid");
        let peer = match session.get("name") {
            None => String::new(),
            Some(Value::String(name)) => membership::peer_name(name),
            Some(_) => return Err(Error::value("invalid name")),
        };
        let fallback = if !peer.is_empty() {
            peer
        } else if live || args.command == "join" {
            "Claude".into()
        } else {
            String::new()
        };
        let name = if args.command == "join" {
            join_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or(fallback)
        } else {
            chosen.unwrap_or(fallback)
        };
        result.insert("name".into(), Value::from(name));
        if args.command == "leave" && !was_joined {
            if !live {
                return Err(Error::value(format!(
                    "{NO_JOINED_CHAT} {sid}; run switchboard chats to find its ID"
                )));
            }
            // Live but never joined: nothing to tell the bridge.
            result.insert("changed".into(), Value::Bool(false));
        } else {
            // What the bridge publishes a joined chat as: the chosen name,
            // else the chat's own.
            let published_as = (args.command == "join").then(|| {
                match join_name.clone().filter(|n| !n.is_empty()) {
                    Some(name) => name,
                    None => match session.get("name") {
                        Some(Value::String(name)) => name.clone(),
                        _ => "Claude".into(),
                    },
                }
            });
            for (key, value) in local::await_bridge(
                &config,
                &args.config,
                &sid,
                args.command == "join",
                published_as.as_deref(),
                10.0,
            )? {
                result.insert(key, value);
            }
        }
        doc = view::selection(&result);
    } else {
        let config = local::private_json(&args.config)?;
        if args.command == "status" {
            update_check = Some(update::start(
                membership::state_dir(&config, &args.config).ok().as_deref(),
            ));
        }
        let records = if args.command == "sessions" && args.remote {
            Vec::new()
        } else {
            local::real_sessions(&config)?
        };
        let current = current_chat(&records, args.session.as_deref());
        let mut rows = local::session_rows(&config, &args.config, &records)?;
        result.insert(
            "enabled".into(),
            Value::Bool(config.get("enabled") == Some(&Value::Bool(true))),
        );
        if args.command == "status" {
            if let Some(Value::String(url)) = crate::config::server_url(&config) {
                result.insert("server_url".into(), Value::from(url.clone()));
            }
        }
        if args.command == "sessions" {
            let mut cache = crate::session_metadata::MetadataCache::new();
            for (row, record) in rows.iter_mut().zip(&records) {
                for (key, value) in cache.for_session(record) {
                    row.insert(key, Value::from(value));
                }
            }
        }
        result.insert(
            "sessions".into(),
            Value::Array(rows.iter().cloned().map(Value::Object).collect()),
        );
        if args.command == "sessions" {
            let search: Vec<String> = args
                .target
                .as_deref()
                .map(|s| format::split(s).into_iter().map(str::to_string).collect())
                .unwrap_or_default();
            // This machine's name: from the server, the bridge, or its
            // identity file.
            let identity_machine = || {
                local::runtime_status(&config, &args.config)
                    .ok()
                    .and_then(|runtime| roster::identity(runtime.get("machine")).ok())
                    .filter(|m| !m.is_empty())
                    .or_else(|| {
                        init::join::existing_identity(&args.config)
                            .map(|(machine, _, _)| machine)
                            .filter(|m| !m.is_empty())
                    })
            };
            if !args.local {
                for (key, value) in roster::fetch_roster(&args.config) {
                    result.insert(key, value);
                }
                let remote = rows_of(&result, "remote_sessions");
                let filtered: Vec<_> = filter_sessions(&remote, args, false, None)?
                    .into_iter()
                    .filter(|row| matches_search(row, "", &search))
                    .collect();
                result.insert(
                    "remote_sessions".into(),
                    Value::Array(filtered.into_iter().map(Value::Object).collect()),
                );
            }
            let mut local_machine = result
                .get("machine")
                .filter(|v| wirejson::truthy(Some(v)))
                .map(str_value);
            if local_machine.is_none() {
                local_machine = identity_machine();
            }
            let filtered: Vec<_> = filter_sessions(&rows, args, true, local_machine.as_deref())?
                .into_iter()
                .filter(|row| matches_search(row, local_machine.as_deref().unwrap_or(""), &search))
                .collect();
            result.insert(
                "sessions".into(),
                Value::Array(filtered.into_iter().map(Value::Object).collect()),
            );
            let scope = if args.local {
                view::Scope::Local
            } else if args.remote {
                view::Scope::Remote
            } else {
                view::Scope::All
            };
            let search_text = search.join(" ");
            doc = view::chats(
                &result,
                &view::ChatsView {
                    scope,
                    current: current.as_deref(),
                    search: &search_text,
                    details: args.details,
                    filtered: !search.is_empty()
                        || args.name.is_some()
                        || args.project.is_some()
                        || args.machine.is_some(),
                    hint: Some(details_command(args)),
                    machine: local_machine,
                },
                crate::fmtutil::time_now(),
            );
        }
        if args.command == "status" {
            match local::runtime_status(&config, &args.config) {
                Ok(runtime) => {
                    let enabled = result.get("enabled") == Some(&Value::Bool(true));
                    result.insert(
                        "connected".into(),
                        Value::Bool(
                            enabled && runtime.get("connected") == Some(&Value::Bool(true)),
                        ),
                    );
                    result.insert(
                        "machine".into(),
                        runtime.get("machine").cloned().unwrap_or(Value::Null),
                    );
                    result.insert(
                        "pid".into(),
                        runtime.get("pid").cloned().unwrap_or(Value::Null),
                    );
                }
                Err(error) if error.is_handled() => {
                    result.insert("connected".into(), Value::Bool(false));
                    let stopped =
                        is_file_not_found(&error) || error.message.starts_with("[Errno 3] ");
                    // Down on purpose: whatever else keeps the bridge from
                    // running, that is the answer.
                    if service::is_down(&args.config) {
                        result.insert("down".into(), Value::Bool(true));
                    }
                    result.insert(
                        "detail".into(),
                        Value::from(if stopped {
                            "the bridge is not running".to_string()
                        } else if error.message.len() > 2
                            && error.message.starts_with('\'')
                            && error.message.ends_with('\'')
                        {
                            // A missing field: the error is the quoted field name.
                            format!(
                                "the configuration has no {} field",
                                &error.message[1..error.message.len() - 1]
                            )
                        } else {
                            error.message
                        }),
                    );
                }
                Err(error) => return Err(error),
            }
            doc = view::status(
                &result,
                current.as_deref(),
                None,
                service::supported() && !service::installed(),
                crate::fmtutil::time_now(),
            );
        }
    }
    if args.command == "status" {
        return Ok(print_status(args, result, doc, update_check));
    }
    if args.json {
        if args.display {
            result.insert("display".into(), Value::Array(doc));
        }
        out(&format!(
            "{}\n",
            dumps_indent(&Value::Object(result.clone()), 2, false)
        ));
    } else {
        out(&format!("{}\n", render(&doc, 1).join("\n")));
    }
    let failed_remote = args.command == "sessions"
        && args.remote
        && result.get("remote_available") != Some(&Value::Bool(true));
    Ok(if failed_remote { 1 } else { 0 })
}

/// Print `status`: the answer at once, then the line about a newer release
/// when the check has one (it waits at most two seconds in all).
fn print_status(
    args: &Args,
    mut result: Map<String, Value>,
    doc: view::Doc,
    check: Option<update::Check>,
) -> i32 {
    // The update line goes before the next step.
    let split = doc
        .iter()
        .position(|block| block.get("kind").and_then(Value::as_str) == Some("next"))
        .unwrap_or(doc.len());
    if args.json {
        let newer = check.and_then(update::Check::finish);
        let mut doc = doc;
        if let Some(version) = &newer {
            result.insert("update_available".into(), Value::from(version.clone()));
            doc.insert(split, view::update_block(version));
        }
        if args.display {
            result.insert("display".into(), Value::Array(doc));
        }
        out(&format!(
            "{}\n",
            dumps_indent(&Value::Object(result), 2, false)
        ));
        return 0;
    }
    let head = render(&doc[..split], 1);
    out(&format!("{}\n", head.join("\n")));
    let mut whole = doc[..split].to_vec();
    if let Some(version) = check.and_then(update::Check::finish) {
        whole.push(view::update_block(&version));
    }
    whole.extend_from_slice(&doc[split..]);
    let all = render(&whole, 1);
    if all.len() > head.len() {
        out(&format!("{}\n", all[head.len()..].join("\n")));
    }
    0
}

/// After `up`: wait (with a spinner on a terminal) until the bridge says it
/// is connected, for at most 15 seconds. Whether it did, the server's
/// address, and the machine's name.
fn wait_connected(config_path: &str, spinner: bool) -> (bool, Option<String>, Option<String>) {
    let url = local::private_json(config_path).ok().and_then(|config| {
        crate::config::server_url(&config)
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let host = url
        .as_deref()
        .map(init::join::host_label)
        .unwrap_or_else(|| "the server".into());
    let mut ui = ui::prompt::Ui::new(spinner && ui::term::is_tty(2), false, ui::Theme::for_fd(2));
    let mut machine = None;
    let connected = ui.step(
        &format!("Starting the bridge and waiting for it to connect to {host}"),
        |_| {
            let began = std::time::Instant::now();
            while began.elapsed() < std::time::Duration::from_secs(15) {
                let status = local::private_json(config_path)
                    .and_then(|config| local::runtime_status(&config, config_path));
                if let Ok(status) = status {
                    if status.get("connected") == Some(&Value::Bool(true)) {
                        machine = status
                            .get("machine")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        return Ok(true);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Ok(false)
        },
        |_| ui::prompt::Settle::Quiet,
    );
    ui.finish();
    (connected.unwrap_or(false), url, machine)
}

fn rows_of(result: &Map<String, Value>, key: &str) -> Vec<Map<String, Value>> {
    result
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

fn field_text(row: &Map<String, Value>, key: &str) -> Result<String, Error> {
    match row.get(key) {
        None => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(other) => Err(Error::value(format!(
            "'{}' object has no attribute 'casefold'",
            crate::discovery::type_name(Some(other))
        ))),
    }
}

/// The rows whose name, project or repository, and machine contain the
/// filters in `args`, compared case-folded.
pub fn filter_sessions(
    rows: &[Map<String, Value>],
    args: &Args,
    local: bool,
    machine: Option<&str>,
) -> Result<Vec<Map<String, Value>>, Error> {
    let mut kept = Vec::new();
    for row in rows {
        let mut matches = true;
        if let Some(name) = &args.name {
            matches = format::casefold(&field_text(row, "name")?).contains(&format::casefold(name));
        }
        if matches {
            if let Some(project) = &args.project {
                let wanted = format::casefold(project);
                let mut any = false;
                for key in ["project", "repository"] {
                    if format::casefold(&field_text(row, key)?).contains(&wanted) {
                        any = true;
                        break;
                    }
                }
                matches = any;
            }
        }
        if matches {
            if let Some(wanted) = &args.machine {
                let haystack = if local {
                    machine
                        .filter(|m| !m.is_empty())
                        .unwrap_or("local")
                        .to_string()
                } else {
                    field_text(row, "machine")?
                };
                matches = format::casefold(&haystack).contains(&format::casefold(wanted));
            }
        }
        if matches {
            kept.push(row.clone());
        }
    }
    Ok(kept)
}

/// Names the bridge would show for chats (session UUID -> live name).
pub type LiveNames = IndexMap<String, String>;

/// `switchboard install [--start]`: install this binary as the user service.
pub fn install_main(argv: &[String]) -> i32 {
    install::main(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_in_messages_are_quoted() {
        assert_eq!(
            quote_commands("run switchboard chats --remote to see them"),
            "run `switchboard chats --remote` to see them"
        );
        assert_eq!(
            quote_commands("pass --force, or run switchboard upgrade 0.2.0."),
            "pass --force, or run `switchboard upgrade 0.2.0`."
        );
        assert_eq!(quote_commands("already `quoted`"), "already `quoted`");
        assert_eq!(
            quote_commands("run switchboard bridge under your own supervisor instead"),
            "run `switchboard bridge` under your own supervisor instead"
        );
        assert_eq!(
            quote_commands("ask for one (sudo switchboard server invite on the server)"),
            "ask for one (`sudo switchboard server invite` on the server)"
        );
        assert_eq!(quote_commands("noswitchboard here"), "noswitchboard here");
    }

    #[test]
    fn errors_become_problems_with_one_fix() {
        let unknown = problem_for(
            "unknown command 'frobnicate'; run switchboard help for the commands",
            None,
            "",
        );
        assert_eq!(unknown.title, "There's no command called frobnicate.");
        assert_eq!(
            unknown.fix.as_deref(),
            Some("run `switchboard help` to see the commands.")
        );
        let terminal = problem_for(local::NOT_IN_CHAT, None, "join");
        assert_eq!(
            terminal.title,
            "`switchboard join` connects the chat it runs in, and this is a terminal."
        );
        assert!(terminal
            .fix
            .unwrap()
            .contains("`switchboard join --chat ID`"));
        let set_up = problem_for(NOT_SET_UP, None, "status");
        assert_eq!(
            (set_up.title.as_str(), set_up.fix.as_deref()),
            (
                "This machine isn't set up yet.",
                Some("run `switchboard init`.")
            )
        );
        // What follows "; " is a fix only when it says what to do.
        let stuck = problem_for(
            "could not stop the bridge (busy); nothing was removed",
            None,
            "",
        );
        assert_eq!(
            (stuck.title.as_str(), stuck.fix),
            (
                "Could not stop the bridge (busy); nothing was removed.",
                None
            )
        );
        // The message's own fix wins over the hint for its kind.
        let restart = problem_for(
            "installed 0.2.0, but the bridge did not restart (busy); run switchboard start",
            error_hint("the bridge did not restart", None),
            "upgrade",
        );
        assert_eq!(restart.fix.as_deref(), Some("run `switchboard start`."));
        let uuid = problem_for(
            "badly formed hexadecimal UUID string",
            error_hint("badly formed hexadecimal UUID string", None),
            "leave",
        );
        assert_eq!(
            uuid.fix.as_deref(),
            Some("run `switchboard chats` to find the chat's full ID.")
        );
        let ghost = problem_for(
            &format!("{NO_JOINED_CHAT} 0000; run switchboard chats to find its ID"),
            None,
            "leave",
        );
        assert_eq!(
            ghost.title,
            "No chat with the ID 0000 is joined on this machine."
        );
    }
}
