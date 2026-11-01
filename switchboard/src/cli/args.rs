//! The command line's argument parser and help text.
//!
//! One command and at most one argument (a name, chat, or version),
//! plus options anywhere: `--option VALUE` or `--option=VALUE`, flags, and
//! `--` to end options. Option names are matched exactly.

use crate::text::repr_str;

pub const COMMANDS: [&str; 18] = [
    "join",
    "leave",
    "chats",
    "sessions",
    "status",
    "up",
    "down",
    "start",
    "stop",
    "doctor",
    "ping",
    "logs",
    "setup",
    "init",
    "upgrade",
    "uninstall",
    "completion",
    "help",
];

/// Older names that still work but are no longer shown: (old, current).
/// `setup` is the non-interactive join `init` replaced; `sessions` is
/// `chats`; `start` and `stop` are `up` and `down`.
pub const HIDDEN: [(&str, &str); 4] = [
    ("sessions", "chats"),
    ("setup", "init"),
    ("start", "up"),
    ("stop", "down"),
];

pub const HELP: &str = "Usage: switchboard COMMAND [options]

Lets Claude Code chats on different machines message each other.

Set up:
  init               Set up this machine: join a server, or run one
  status             Whether this machine is connected, and this chat
  doctor             Check everything, with the fix for each problem

Chats:
  join [NAME]        Join this chat, so chats on other machines can message it
  leave              Leave this chat; other machines can no longer see it
  chats              List chats on this machine and others
  ping CHAT          Check that a chat on another machine is reachable

This machine:
  up                 Bring this machine back online
  down               Take this machine offline until you bring it back up
  logs               Show what the bridge has been doing
  upgrade [VERSION]  Install the latest release
  completion SHELL   Print Tab completion for bash, zsh, or fish
  uninstall          Remove Switchboard from this machine

Run switchboard COMMAND --help for its options; every command takes --json
(output for scripts) and --config PATH. Servers: switchboard server --help.
";

/// The options every command takes, at the end of each command's help.
const COMMON: &str = "  --json             print the result as JSON
  --config PATH      the bridge configuration (default:
                     ~/.config/switchboard/bridge.json, or SWITCHBOARD_CONFIG)
  -h, --help         show this help
";

const JOIN_HELP: &str = "Usage: switchboard join [NAME] [--chat ID]

Join a Claude Code chat, so chats on other machines can message it. Run it
inside the chat: /switchboard join with the plugin, or !switchboard join
without it. Other machines see NAME, or the chat's own name. A joined chat
leaves on its own after 7 days without work or messages.

Options:
  --chat ID          the chat to join, from another terminal (its full ID,
                     from switchboard chats --details)
";

const LEAVE_HELP: &str = "Usage: switchboard leave [--chat ID]

Leave a chat: other machines can no longer see or message it. Run it
inside the chat, or pass --chat with its ID (a joined chat that was closed
can be left this way too).

Options:
  --chat ID          the chat to leave, from another terminal
";

const CHATS_HELP: &str = "Usage: switchboard chats [SEARCH] [--local | --remote] [--details]
                         [--name TEXT] [--project TEXT] [--machine TEXT]

List Claude Code chats: those open on this machine, with what each is doing
and whether it is joined, and the joined chats on other machines. SEARCH
keeps the chats whose name, ID, machine, project, repository, or branch
contains each of its words.

Options:
  --local, --remote  only chats on this machine, or only on others
  --details          add each chat's full ID, project, and branch
  --name TEXT        chats whose name contains TEXT
  --project TEXT     chats whose project or repository contains TEXT
  --machine TEXT     chats on machines whose name contains TEXT
  --chat ID          mark this chat (inside a chat it is found on its own)
";

const STATUS_HELP: &str = "Usage: switchboard status [--chat ID]

Whether this machine is connected to its server, whether this chat is
joined (run inside a chat, or given one), and how many chats here are
joined. switchboard doctor checks everything in detail.

Options:
  --chat ID          the chat to report on (inside a chat it is found on its
                     own)
";

const DOCTOR_HELP: &str = "Usage: switchboard doctor

Check this machine's setup: the program, the configuration and key, the
bridge, the server, and joined chats, with the fix for each problem. Exits 1
when a check fails.

Options:
";

const PING_HELP: &str = "Usage: switchboard ping CHAT

Check that a joined chat on another machine is reachable, without waking it:
the round trip through the server to that machine's bridge. CHAT is the
chat's name, MACHINE/NAME, or its ID (see switchboard chats --remote).

Options:
";

const UP_HELP: &str = "Usage: switchboard up

Bring this machine back online after switchboard down: start the bridge, the
background service that connects this machine's chats to the server, let it
start again when you log in, and wait until it has connected. start is an
older name for up.

Options:
";

const DOWN_HELP: &str = "Usage: switchboard down

Take this machine offline: stop the bridge and keep it from starting at
login or after a restart, until switchboard up. The configuration, key, and
joined chats are kept. stop is an older name for down.

Options:
";

const LOGS_HELP: &str = "Usage: switchboard logs [--follow] [--lines N]

Show what the bridge has been doing: when it connected, messages it could
not deliver, chats that left, and errors, one event per line with its local
time. With --json, the bridge's own log lines, one JSON object per line.

Options:
  --follow           keep showing new events until Ctrl-C
  --lines N          how many of the latest events to show (default 20)
";

const COMPLETION_HELP: &str = "Usage: switchboard completion bash|zsh|fish

Print a script that completes switchboard's commands, options, and chat
names with Tab. switchboard init offers to add it to your shell; to add it
yourself, put this line in your shell's startup file:

  bash, zsh:  eval \"$(switchboard completion zsh)\"   (or bash)
  fish:       switchboard completion fish | source

Options:
";

const UPGRADE_HELP: &str = "Usage: switchboard upgrade [VERSION] [--force]

Download the latest release (or VERSION), check it against the release's
checksums, install it, and restart the bridge if it was running.

Options:
  --force            reinstall the same version, or move to an older one
";

const UNINSTALL_HELP: &str = "Usage: switchboard uninstall [--purge]

Remove Switchboard from this machine: stop the bridge and remove its
background service, the switchboard command, and the program and its
backups. This machine's configuration and key (so joining again is quick),
its chat membership, and its logs are kept.

This machine stays on the server until it is removed there, with
sudo switchboard server revoke NAME on the server.

Options:
  --purge            also delete ~/.config/switchboard and
                     ~/.local/share/switchboard (configuration, key, chat
                     membership, and logs); a configuration elsewhere
                     (SWITCHBOARD_CONFIG) is kept
";

const SETUP_HELP: &str = "Usage: switchboard setup [--invite INVITE|-] [options]

The older way to join a server without a terminal; switchboard init does the
same and more. setup makes this machine's key, joins with the invite, and
starts the bridge. On a machine that is set up, it confirms the server still
knows this machine, and can trust a server's new certificate.

Options:
  --invite INVITE    the invite (from sudo switchboard server invite); --invite
                     - reads it from standard input, and SWITCHBOARD_INVITE
                     works too
  --machine NAME     this machine's name (default: its host name)
  --owner NAME       who it belongs to (default: your user name)
  --server-url URL   the server's address; it must match the invite's
  --server-pin SHA256
                     the SHA-256 fingerprint of the server's certificate, if
                     the invite doesn't carry it
  --no-start         don't install or start the bridge
";

/// Help text, written for 80 columns, refit to a narrower terminal:
/// paragraphs are rewrapped between words, a command's or option's
/// description stays under its column (or under the name, indented, when
/// that column leaves too little room), and a usage line's continuation stays
/// under the command.
pub fn fit(text: &str, width: usize) -> String {
    use super::format::display_width;
    if width >= 80 {
        return text.to_string();
    }
    let room = width.max(40) - 1;
    // (first-line prefix, words, hang) for each block; None for a blank line.
    let mut blocks: Vec<Option<(String, Vec<String>, usize)>> = Vec::new();
    // The column a row's description starts at, while in a row.
    let mut row_column: Option<usize> = None;
    let mut prose_indent: Option<usize> = None;
    let words = |text: &str| -> Vec<String> {
        text.split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            blocks.push(None);
            row_column = None;
            prose_indent = None;
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        // A continuation of the row or usage line above.
        if let (Some(column), Some(Some(block))) = (row_column, blocks.last_mut()) {
            if indent == column {
                block.1.extend(words(trimmed));
                continue;
            }
        }
        if trimmed.starts_with("Usage:") || (trimmed.starts_with('[') && row_column.is_some()) {
            // After `Usage: switchboard COMMAND `, where continuations line up.
            let command_end = trimmed
                .match_indices(' ')
                .nth(2)
                .map_or(trimmed.len(), |(at, _)| at + 1);
            let hang = indent + command_end;
            blocks.push(Some((
                format!("{}{}", " ".repeat(indent), &trimmed[..command_end]),
                words(&trimmed[command_end..]),
                hang,
            )));
            row_column = Some(hang);
            prose_indent = None;
            continue;
        }
        let column = (indent > 0)
            .then(|| {
                trimmed.find("  ").map(|at| {
                    let rest = &trimmed[at..];
                    indent + at + rest.len() - rest.trim_start().len()
                })
            })
            .flatten()
            .filter(|column| *column < line.len());
        if let Some(column) = column {
            blocks.push(Some((
                line[..column].to_string(),
                words(&line[column..]),
                column,
            )));
            row_column = Some(column);
            prose_indent = None;
            continue;
        }
        if prose_indent == Some(indent) && !trimmed.ends_with(':') {
            if let Some(Some(block)) = blocks.last_mut() {
                block.1.extend(words(trimmed));
                continue;
            }
        }
        blocks.push(Some((" ".repeat(indent), words(trimmed), indent)));
        row_column = None;
        prose_indent = (!trimmed.ends_with(':')).then_some(indent);
    }
    let mut out = String::new();
    for block in blocks {
        let Some((head, body, hang)) = block else {
            out.push('\n');
            continue;
        };
        let (mut current, hang) = if hang + 20 > room && display_width(head.trim()) > 0 {
            // Too little room beside the name: the description goes under it.
            out.push_str(head.trim_end());
            out.push('\n');
            let hang = head.len() - head.trim_start().len() + 4;
            (" ".repeat(hang), hang)
        } else {
            (head, hang)
        };
        let mut fresh = current.trim().is_empty() || current.ends_with(' ');
        for word in body {
            let needed = if fresh { 0 } else { 1 } + display_width(&word);
            if !current.trim().is_empty() && display_width(&current) + needed > room {
                out.push_str(current.trim_end());
                out.push('\n');
                current = " ".repeat(hang);
                fresh = true;
            }
            if !fresh {
                current.push(' ');
            }
            current.push_str(&word);
            fresh = false;
        }
        out.push_str(current.trim_end());
        out.push('\n');
    }
    out
}

/// Help text in the visual language: each section heading (a line of its
/// own after a blank line, with indented lines under it) in bold. The words
/// stay those of `help --json`, so without color the two are identical.
pub fn styled_help(text: &str, theme: &super::ui::Theme) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let after_blank = index == 0 || lines[index - 1].trim().is_empty();
        let before_items = lines
            .get(index + 1)
            .is_some_and(|next| next.starts_with(' ') && !next.trim().is_empty());
        let heading = !line.starts_with(' ')
            && line.ends_with(':')
            && !line.starts_with("Usage:")
            && after_blank
            && before_items
            && super::format::display_width(line) <= 40;
        if heading {
            out.push(theme.strong(line));
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

/// The help for `command` (`None` for the overview).
pub fn command_help(command: Option<&str>) -> String {
    let body = match command {
        Some("join") => JOIN_HELP,
        Some("leave") => LEAVE_HELP,
        Some("chats") | Some("sessions") => CHATS_HELP,
        Some("status") => STATUS_HELP,
        Some("doctor") => DOCTOR_HELP,
        Some("ping") => PING_HELP,
        Some("up") | Some("start") => UP_HELP,
        Some("down") | Some("stop") => DOWN_HELP,
        Some("logs") => LOGS_HELP,
        Some("completion") => COMPLETION_HELP,
        Some("upgrade") => UPGRADE_HELP,
        Some("uninstall") => UNINSTALL_HELP,
        Some("setup") => SETUP_HELP,
        _ => return HELP.to_string(),
    };
    // A command's own options come first, then the ones every command takes.
    let mut text = body.to_string();
    text.push_str(COMMON);
    if matches!(command, Some("chats") | Some("sessions") | Some("status")) {
        text.push_str(
            "\nWith --json, chats keep the key names of older versions (sessions,\nremote_sessions).\n",
        );
    }
    text
}

/// The parsed command line.
#[derive(Clone, Debug, Default)]
pub struct Args {
    pub config: String,
    pub command: String,
    pub target: Option<String>,
    pub session: Option<String>,
    pub json: bool,
    pub local: bool,
    pub remote: bool,
    pub name: Option<String>,
    pub project: Option<String>,
    pub machine: Option<String>,
    pub owner: Option<String>,
    /// `--server-url` (or the older spelling `--hub-url`).
    pub hub_url: Option<String>,
    pub server_pin: Option<String>,
    pub invite: Option<String>,
    pub no_start: bool,
    pub force: bool,
    pub purge: bool,
    /// `chats --details`: full IDs, project, and branch.
    pub details: bool,
    /// `--display`: with `--json`, add what the command shows (for the
    /// Claude Code plugin).
    pub display: bool,
    /// `logs --follow`.
    pub follow: bool,
    /// `logs --lines N`.
    pub lines: Option<String>,
    /// `completion --words KIND`: the words to complete (for the scripts).
    pub words: Option<String>,
}

/// Parse outcome: arguments, a help request (`-h`, with the command it is
/// for), or a usage error (with the command, when one was named).
#[derive(Debug)]
pub enum Parsed {
    Args(Box<Args>),
    Help(Option<String>),
    Error(String, Option<String>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Help,
    Config,
    Session,
    Json,
    Local,
    Remote,
    Name,
    Project,
    Machine,
    Owner,
    ServerUrl,
    ServerPin,
    Invite,
    NoStart,
    Force,
    Purge,
    Details,
    Display,
    Follow,
    Lines,
    Words,
}

/// Options that are only for other programs (the plugin and the completion
/// scripts), not in the help.
pub const UNLISTED: [&str; 4] = ["--hub-url", "--session", "--display", "--words"];

const OPTIONS: [(&str, Act); 24] = [
    ("-h", Act::Help),
    ("--help", Act::Help),
    ("--config", Act::Config),
    ("--chat", Act::Session),
    // The name earlier versions used, still accepted.
    ("--session", Act::Session),
    ("--json", Act::Json),
    ("--local", Act::Local),
    ("--remote", Act::Remote),
    ("--name", Act::Name),
    ("--project", Act::Project),
    ("--machine", Act::Machine),
    ("--owner", Act::Owner),
    ("--server-url", Act::ServerUrl),
    // The name earlier versions used, still accepted.
    ("--hub-url", Act::ServerUrl),
    ("--server-pin", Act::ServerPin),
    ("--invite", Act::Invite),
    ("--no-start", Act::NoStart),
    ("--force", Act::Force),
    ("--purge", Act::Purge),
    ("--details", Act::Details),
    ("--display", Act::Display),
    ("--follow", Act::Follow),
    ("--lines", Act::Lines),
    ("--words", Act::Words),
];

fn takes_value(act: Act) -> bool {
    !matches!(
        act,
        Act::Help
            | Act::Json
            | Act::Local
            | Act::Remote
            | Act::NoStart
            | Act::Force
            | Act::Purge
            | Act::Details
            | Act::Display
            | Act::Follow
    )
}

fn lookup(option: &str) -> Option<Act> {
    OPTIONS.iter().find(|(s, _)| *s == option).map(|(_, a)| *a)
}

fn looks_negative_number(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('-') else {
        return false;
    };
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if all_digits(rest) {
        return true;
    }
    match rest.split_once('.') {
        Some((whole, fraction)) => {
            whole.bytes().all(|b| b.is_ascii_digit()) && all_digits(fraction)
        }
        None => false,
    }
}

/// Whether `arg` is meant as an option: it starts with `-` and is not `-`
/// alone, a negative number, or text with a space in it (a chat name).
fn is_option(arg: &str) -> bool {
    arg.len() > 1
        && arg.starts_with('-')
        && !looks_negative_number(arg)
        && (!arg.contains(' ') || lookup(arg.split('=').next().unwrap_or(arg)).is_some())
}

fn check_command(value: &str) -> Result<(), String> {
    if COMMANDS.contains(&value) {
        return Ok(());
    }
    // `version` is an option here.
    let suggestion = if value == "version" {
        Some("--version")
    } else {
        close_match(value, &COMMANDS).map(|closest| {
            HIDDEN
                .iter()
                .find(|(old, _)| *old == closest)
                .map_or(closest, |(_, current)| *current)
        })
    };
    Err(match suggestion {
        Some(closest) => format!(
            "unknown command {}; did you mean {closest}? Run switchboard help for the commands",
            repr_str(value)
        ),
        None => format!(
            "unknown command {}; run switchboard help for the commands",
            repr_str(value)
        ),
    })
}

/// The command named in `argv`, if it names a known one.
fn named_command(argv: &[String]) -> Option<String> {
    let mut skip = false;
    for arg in argv {
        if skip {
            skip = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if is_option(arg) {
            let name = arg.split('=').next().unwrap_or(arg);
            skip = !arg.contains('=') && lookup(name).is_some_and(takes_value);
            continue;
        }
        return COMMANDS.contains(&arg.as_str()).then(|| arg.clone());
    }
    None
}

/// Parse `argv` (everything after the program name).
pub fn parse(argv: &[String], default_config: &str) -> Parsed {
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    if argv[..end].iter().any(|a| a == "-h" || a == "--help") {
        return Parsed::Help(named_command(argv));
    }
    match parse_args(argv, default_config) {
        Ok(args) => Parsed::Args(Box::new(args)),
        Err(message) => Parsed::Error(message, named_command(argv)),
    }
}

fn parse_args(argv: &[String], default_config: &str) -> Result<Args, String> {
    let mut a = Args {
        config: default_config.to_string(),
        command: "help".into(),
        ..Args::default()
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut dashed = false;
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        index += 1;
        if dashed || !is_option(arg) {
            positionals.push(arg.clone());
            continue;
        }
        if arg == "--" {
            dashed = true;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let act = lookup(name).ok_or_else(|| format!("unknown option {name}"))?;
        let value = if takes_value(act) {
            match inline {
                Some(value) => Some(value),
                None => match argv.get(index) {
                    Some(next) if !is_option(next) => {
                        index += 1;
                        Some(next.clone())
                    }
                    _ => return Err(format!("{name} needs a value")),
                },
            }
        } else if inline.is_some() {
            return Err(format!("{name} takes no value"));
        } else {
            None
        };
        let text = value.clone().unwrap_or_default();
        match act {
            Act::Help => {}
            Act::Config => a.config = crate::fsutil::normalize(&text),
            Act::Session => a.session = value,
            Act::Json => a.json = true,
            Act::Local | Act::Remote => {
                if (act == Act::Local && a.remote) || (act == Act::Remote && a.local) {
                    return Err("--local and --remote can't be used together".into());
                }
                if act == Act::Local {
                    a.local = true;
                } else {
                    a.remote = true;
                }
            }
            Act::Name => a.name = value,
            Act::Project => a.project = value,
            Act::Machine => a.machine = value,
            Act::Owner => a.owner = value,
            Act::ServerUrl => a.hub_url = value,
            Act::ServerPin => a.server_pin = value,
            Act::Invite => a.invite = value,
            Act::NoStart => a.no_start = true,
            Act::Force => a.force = true,
            Act::Purge => a.purge = true,
            Act::Details => a.details = true,
            Act::Display => a.display = true,
            Act::Follow => a.follow = true,
            Act::Lines => a.lines = value,
            Act::Words => a.words = value,
        }
    }
    let mut positionals = positionals.into_iter();
    if let Some(command) = positionals.next() {
        check_command(&command)?;
        a.command = command;
    }
    a.target = positionals.next();
    let extra: Vec<String> = positionals.collect();
    match extra.len() {
        0 => {}
        1 => return Err(format!("unexpected argument {}", extra[0])),
        _ => return Err(format!("unexpected arguments {}", extra.join(" "))),
    }
    Ok(a)
}

/// The closest command to a mistyped one (`difflib.get_close_matches` with
/// its 0.6 cutoff).
pub fn close_match<'a>(word: &str, possibilities: &[&'a str]) -> Option<&'a str> {
    let b: Vec<char> = word.chars().collect();
    let mut best: Option<(f64, &str)> = None;
    for candidate in possibilities {
        let a: Vec<char> = candidate.chars().collect();
        let ratio = sequence_ratio(&a, &b);
        if ratio >= 0.6 && best.is_none_or(|(score, x)| (ratio, *candidate) > (score, x)) {
            best = Some((ratio, candidate));
        }
    }
    best.map(|(_, x)| x)
}

/// `SequenceMatcher(None, a, b).ratio()`.
fn sequence_ratio(a: &[char], b: &[char]) -> f64 {
    use std::collections::HashMap;
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (index, c) in b.iter().enumerate() {
        b2j.entry(*c).or_default().push(index);
    }
    if b.len() >= 200 {
        let limit = b.len() / 100 + 1;
        b2j.retain(|_, indices| indices.len() <= limit);
    }
    let longest = |alo: usize, ahi: usize, blo: usize, bhi: usize| -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, c) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = b2j.get(c) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|p| j2len.get(&p))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    next.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = next;
        }
        while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    };
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut matches = 0usize;
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest(alo, ahi, blo, bhi);
        if k > 0 {
            matches += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    let total = a.len() + b.len();
    if total == 0 {
        1.0
    } else {
        2.0 * matches as f64 / total as f64
    }
}

/// The terminal's width in columns: `COLUMNS` (with `LINES`), else the
/// terminal's own size, else 80.
pub fn terminal_columns() -> i64 {
    let parse = |key: &str| {
        std::env::var(key)
            .ok()
            .and_then(|v| crate::fmtutil::parse_int(&v))
            .unwrap_or(0)
    };
    let columns = parse("COLUMNS");
    let lines = parse("LINES");
    if columns > 0 && lines > 0 {
        return columns;
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize for descriptor 1.
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0;
    let queried = if ok { size.ws_col as i64 } else { 80 };
    if columns > 0 {
        columns
    } else if queried > 0 {
        queried
    } else {
        80
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn ok(list: &[&str]) -> Args {
        match parse(&args(list), "/c") {
            Parsed::Args(a) => *a,
            other => panic!("{other:?}"),
        }
    }

    fn err(list: &[&str]) -> String {
        match parse(&args(list), "/c") {
            Parsed::Error(e, _) => e,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn options_go_anywhere_around_the_command() {
        let a = ok(&["join", "--session", "X", "name"]);
        assert_eq!(
            (
                a.command.as_str(),
                a.target.as_deref(),
                a.session.as_deref()
            ),
            ("join", Some("name"), Some("X"))
        );
        assert_eq!(ok(&[]).command, "help");
        let a = ok(&["--session=x", "leave", "--json"]);
        assert!(a.json && a.session.as_deref() == Some("x") && a.command == "leave");
        assert_eq!(
            ok(&["status", "--", "join"]).target.as_deref(),
            Some("join")
        );
        assert_eq!(ok(&["join", "--", "-odd"]).target.as_deref(), Some("-odd"));
        assert_eq!(ok(&["setup", "--invite", "-"]).invite.as_deref(), Some("-"));
        assert_eq!(
            ok(&["setup", "--hub-url", "https://a"]).hub_url.as_deref(),
            Some("https://a")
        );
        assert_eq!(
            ok(&["setup", "--server-url=https://b"]).hub_url.as_deref(),
            Some("https://b")
        );
        let a = ok(&["upgrade", "0.3.0", "--force"]);
        assert_eq!((a.target.as_deref(), a.force), (Some("0.3.0"), true));
        assert!(ok(&["uninstall", "--purge"]).purge);
    }

    #[test]
    fn errors_read_plainly() {
        assert_eq!(
            err(&["sessions", "--local", "--remote"]),
            "--local and --remote can't be used together"
        );
        assert_eq!(err(&["setup", "--token", "x"]), "unknown option --token");
        assert_eq!(err(&["setup", "--wait", "5"]), "unknown option --wait");
        assert_eq!(err(&["status", "--config"]), "--config needs a value");
        assert_eq!(
            err(&["status", "--config", "--json"]),
            "--config needs a value"
        );
        assert_eq!(err(&["status", "--n", "x"]), "unknown option --n");
        assert_eq!(err(&["status", "--json=1"]), "--json takes no value");
        assert_eq!(err(&["status", "a", "b"]), "unexpected argument b");
        assert_eq!(err(&["status", "a", "b", "c"]), "unexpected arguments b c");
        assert_eq!(
            err(&["statu"]),
            "unknown command 'statu'; did you mean status? Run switchboard help for the commands"
        );
        assert_eq!(
            err(&["xyzzy"]),
            "unknown command 'xyzzy'; run switchboard help for the commands"
        );
        assert!(matches!(
            parse(&args(&["status", "-h"]), "/c"),
            Parsed::Help(Some(c)) if c == "status"
        ));
        assert!(matches!(
            parse(&args(&["--config", "x", "uninstall", "--help"]), "/c"),
            Parsed::Help(Some(c)) if c == "uninstall"
        ));
        assert!(matches!(
            parse(&args(&["--help"]), "/c"),
            Parsed::Help(None)
        ));
        assert_eq!(
            err(&["version"]),
            "unknown command 'version'; did you mean --version? Run switchboard help for the commands"
        );
        assert_eq!(
            err(&["sesions"]),
            "unknown command 'sesions'; did you mean chats? Run switchboard help for the commands"
        );
        assert_eq!(ok(&["chats", "--chat", "x"]).session.as_deref(), Some("x"));
        assert_eq!(err(&["-hx"]), "unknown option -hx");
    }

    #[test]
    fn close_matches_follow_difflib() {
        assert_eq!(close_match("stat", &COMMANDS), Some("start"));
        assert_eq!(close_match("statu", &COMMANDS), Some("status"));
        assert_eq!(close_match("xyzzy", &COMMANDS), None);
    }

    #[test]
    fn help_refits_narrow_terminals_without_breaking_words() {
        let narrow = fit(HELP, 60);
        for line in narrow.lines() {
            assert!(line.chars().count() <= 59, "{line}");
        }
        assert!(narrow.contains(
            "  down               Take this machine offline until you\n                     bring it back up\n"
        ));
        let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(words(&narrow), words(HELP));
        for command in COMMANDS {
            let help = command_help(Some(command));
            let narrow = fit(&help, 60);
            assert!(narrow.lines().all(|l| l.chars().count() <= 59), "{narrow}");
            assert_eq!(words(&narrow), words(&help));
        }
        let chats = fit(&command_help(Some("chats")), 60);
        assert!(chats.starts_with(
            "Usage: switchboard chats [SEARCH] [--local | --remote]\n                         [--details] [--name TEXT]\n                         [--project TEXT] [--machine TEXT]\n\nList"
        ), "{chats}");
        let init = fit(super::super::init::HELP, 60);
        assert!(init.lines().all(|l| l.chars().count() <= 59), "{init}");
        assert_eq!(words(&init), words(super::super::init::HELP));
        assert_eq!(fit(HELP, 100), HELP);
    }

    #[test]
    fn help_fits_80_columns_and_names_every_command_and_option() {
        let mut all = HELP.to_string();
        for command in COMMANDS {
            let help = command_help(Some(command));
            for line in help.lines() {
                assert!(line.chars().count() <= 80, "{line}");
            }
            all.push_str(&help);
        }
        for line in HELP.lines() {
            assert!(line.chars().count() <= 80, "{line}");
        }
        for command in COMMANDS {
            let hidden = HIDDEN.iter().any(|(old, _)| *old == command) || command == "help";
            let listed = HELP.contains(&format!("\n  {command} "));
            assert_eq!(listed, !hidden, "{command}");
        }
        for (option, _) in OPTIONS {
            if !UNLISTED.contains(&option) {
                assert!(all.contains(option), "{option}");
            }
        }
        assert!(!all.contains("hub"));
        assert!(!HELP.contains("setup") && !HELP.contains("sessions"));
        assert!(!HELP.contains("start") && !HELP.contains("stop"));
        // The overview describes each command the plugin also has in the
        // plugin's words.
        for (command, _, what) in super::super::view::SHARED_COMMANDS {
            assert!(HELP.contains(&format!("  {what}\n")), "{command}: {what}");
        }
        for gone in [
            "approve",
            "refuse",
            "administrator",
            "--token",
            "--wait",
            "--yes",
        ] {
            assert!(!all.contains(gone), "{gone}");
        }
    }
}
