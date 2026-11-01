//! `switchboard server logs`: the server's events over time, from the
//! systemd journal, one line each: when, what happened, and the detail.
//! The server's own JSON lines and systemd's lines about the service (start,
//! stop, exit, restart) are both shown; failures are marked with a cross
//! and warnings with `!`. `--json` prints one JSON object per line, with the
//! line as it was logged, so `--follow --json` streams.

use serde_json::{Map, Value};

use super::system::{self, Event, Level};
use crate::cli::ui::{self, Mark, Theme, Tone};

const USAGE: &str =
    "Usage: switchboard server logs [--follow] [--lines N] [--since TIME] [--json]\n";
const HELP: &str = "
The server's events from the system journal, oldest first: the time, what
happened, and the detail. Errors are marked. Run it on the server as root
(or as a member of the systemd-journal group).

Options:
  -f, --follow      keep printing new events as they are logged (Ctrl-C stops)
  -n, --lines N     how many of the latest events to show (default: 50)
  --since TIME      only events from TIME on: 30m, 2h, or 7d ago, or a date and
                    time such as \"2026-10-31 09:00\", today, or yesterday
  --json            print one JSON object per event: time, source, level,
                    event, detail, and the line as logged
  -h, --help        show this help
";

/// How many events are shown by default.
const DEFAULT_LINES: usize = 50;
/// The most `--lines` asks for.
const MAX_LINES: usize = 100_000;
/// The event column's width while following, when the widest is unknown.
const FOLLOW_WIDTH: usize = 20;

/// The parsed command line.
#[derive(Debug, Default, PartialEq)]
pub struct Options {
    pub follow: bool,
    pub lines: Option<usize>,
    /// The `--since` value as journalctl takes it.
    pub since: Option<String>,
    /// The `--since` value as given.
    pub since_text: Option<String>,
    pub json: bool,
}

/// `30m`, `2h`, `7d`, `90s`, or `1w` as journalctl's relative time
/// (`-30min`); anything else is passed on as a time journalctl reads.
pub fn since_argument(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("--since needs a time".into());
    }
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &text[digits.len()..];
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && digits.len() <= 9 {
        let unit = match unit {
            "s" => "s",
            "m" | "min" => "min",
            "h" => "h",
            "d" => "d",
            "w" => "w",
            _ => {
                return Err(format!(
                    "--since must be a time like 30m, 2h, 7d, or \"2026-10-31 09:00\", not '{text}'"
                ))
            }
        };
        return Ok(format!("-{digits}{unit}"));
    }
    if text.starts_with('-') && !text[1..].starts_with(|c: char| c.is_ascii_digit()) {
        return Err(format!(
            "--since must be a time like 30m, 2h, 7d, or \"2026-10-31 09:00\", not '{text}'"
        ));
    }
    Ok(text.to_string())
}

fn usage_error(message: &str) -> String {
    format!("switchboard server logs: {message}\n{USAGE}")
}

/// Parse the arguments; `Ok(None)` for `--help`.
pub fn parse(argv: &[String]) -> Result<Option<Options>, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < argv.len() {
        let argument = argv[index].as_str();
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (argument, None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            index += 1;
            match argv.get(index) {
                Some(value) if !value.starts_with("--") => Ok(value.clone()),
                _ => Err(usage_error(&format!("{name} needs a value"))),
            }
        };
        match name {
            "-h" | "--help" => return Ok(None),
            "-f" | "--follow" if inline.is_none() => options.follow = true,
            "--json" if inline.is_none() => options.json = true,
            "-n" | "--lines" => {
                let text = value()?;
                options.lines = Some(
                    text.parse::<usize>()
                        .ok()
                        .filter(|n| (1..=MAX_LINES).contains(n))
                        .ok_or_else(|| {
                            usage_error(&format!(
                                "--lines must be a count from 1 to {MAX_LINES}, not '{text}'"
                            ))
                        })?,
                );
            }
            "--since" => {
                let text = value()?;
                options.since = Some(since_argument(&text).map_err(|m| usage_error(&m))?);
                options.since_text = Some(text);
            }
            _ if argument.starts_with('-') => {
                return Err(usage_error(&format!("unknown option {name}")))
            }
            _ => return Err(usage_error(&format!("unexpected argument {argument}"))),
        }
        index += 1;
    }
    Ok(Some(options))
}

/// An event for scripts; the field names are stable.
pub fn event_json(event: &Event) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert(
        "time".into(),
        crate::fmtutil::float_value((event.time * 1e6).round() / 1e6),
    );
    value.insert("source".into(), Value::from(event.source));
    value.insert("level".into(), Value::from(event.level.name()));
    value.insert("event".into(), Value::from(event.event.clone()));
    value.insert("detail".into(), Value::from(event.detail.clone()));
    value.insert("message".into(), Value::from(event.message.clone()));
    value
}

/// Events as lines: the mark (a cross for an error, `!` for a warning), the
/// time, the event padded to `width` (default: the widest), and the detail,
/// wrapped under its own column.
pub fn event_lines(
    theme: &Theme,
    events: &[Event],
    width: Option<usize>,
    columns: usize,
) -> Vec<String> {
    use crate::cli::format::{display_width, padded, wrap_cells};
    let width = width.unwrap_or_else(|| {
        events
            .iter()
            .map(|e| display_width(&e.event))
            .max()
            .unwrap_or(0)
    });
    let mut lines = Vec::new();
    for event in events {
        let mark = match event.level {
            Level::Error => Mark::Failed,
            Level::Warning => Mark::Warned,
            Level::Info => Mark::Blank,
        };
        let time = system::clock(event.time);
        let name = ui::format_text(&event.event);
        let detail = ui::format_text(&event.detail);
        let head = format!("{time}  {}", padded(&name, width));
        let hang = ui::INDENT + display_width(&head) + 2;
        let room = columns.saturating_sub(hang + 1).max(24);
        let mut wrapped = wrap_cells(&detail, room).into_iter();
        let first = wrapped.next().unwrap_or_default();
        lines.push(
            format!("{}{head}  {first}", theme.mark(mark))
                .trim_end()
                .to_string(),
        );
        for more in wrapped {
            lines.push(format!("{}{more}", " ".repeat(hang)));
        }
    }
    lines
}

/// The heading over the events.
fn heading(theme: &Theme, summary: &str) -> String {
    format!(
        "{}{}  {summary}",
        " ".repeat(ui::INDENT),
        theme.strong("Server events")
    )
}

/// `switchboard server logs ...`.
pub fn main(argv: &[String]) -> i32 {
    let options = match parse(argv) {
        Ok(Some(options)) => options,
        Ok(None) => {
            crate::cli::print_help(&format!("{USAGE}{HELP}"));
            return 0;
        }
        Err(message) => {
            crate::cli::print_usage_text(&message, "switchboard server logs --help");
            return 2;
        }
    };
    let fail = |problem: ui::prompt::Problem, message: &str| {
        if options.json {
            let mut value = Map::new();
            value.insert("ok".into(), Value::Bool(false));
            value.insert("error".into(), Value::from(message));
            crate::cli::out(&format!(
                "{}\n",
                crate::wirejson::dumps(&Value::Object(value))
            ));
        } else {
            crate::cli::print_problem(&problem);
        }
        1
    };
    if !system::has_systemd() {
        return fail(
            ui::prompt::Problem::new(
                "The server's events are in the systemd journal, and this machine doesn't run systemd.",
            )
            .without_fix(),
            "this machine doesn't run systemd",
        );
    }
    if !system::can_read_journal() {
        return fail(
            ui::prompt::Problem::new("You can't read the system journal as this user.")
                .fix("run it with sudo: `sudo switchboard server logs`"),
            "cannot read the system journal; run this as root",
        );
    }
    let lines = options.lines.unwrap_or(DEFAULT_LINES);
    let theme = Theme::for_fd(1);
    let columns = crate::cli::format::output_width();
    let since = options.since.as_deref();
    if options.follow {
        if !options.json {
            crate::cli::out(&format!(
                "{}\n\n",
                heading(
                    &theme,
                    &format!("following, times in {}; Ctrl-C stops", system::zone())
                )
            ));
        }
        let result = system::follow_journal(lines, since, |event| {
            use std::io::Write;
            let text = if options.json {
                crate::wirejson::dumps(&Value::Object(event_json(&event)))
            } else {
                event_lines(&theme, &[event], Some(FOLLOW_WIDTH), columns).join("\n")
            };
            // Once nobody reads it (a closed pipe), following stops.
            let mut out = std::io::stdout().lock();
            writeln!(out, "{text}").and_then(|()| out.flush()).is_ok()
        });
        return match result {
            Ok(()) => 0,
            Err(message) => fail(
                ui::prompt::Problem::new("The journal couldn't be read.")
                    .detail(format!("{message}."))
                    .without_fix(),
                &message,
            ),
        };
    }
    let events = match system::read_journal(lines, since) {
        Ok(events) => events,
        Err(message) => {
            return fail(
                ui::prompt::Problem::new("The journal couldn't be read.")
                    .detail(format!("{message}."))
                    .fix("check the time given to `--since`, then try again."),
                &message,
            )
        }
    };
    if options.json {
        for event in &events {
            if !crate::cli::out_ok(&format!(
                "{}\n",
                crate::wirejson::dumps(&Value::Object(event_json(event)))
            )) {
                break;
            }
        }
        return 0;
    }
    // A closed pipe (`| head`) ends the output quietly, not with a panic.
    crate::cli::out(&render(
        &theme,
        &events,
        options.since_text.as_deref(),
        columns,
    ));
    0
}

/// The events for people, under a heading with their count.
pub fn render(theme: &Theme, events: &[Event], since: Option<&str>, columns: usize) -> String {
    let mut lines = Vec::new();
    if events.is_empty() {
        lines.push(heading(theme, "none"));
        lines.push(String::new());
        let when = match since {
            Some(since) => format!(" since {since}"),
            None => String::new(),
        };
        lines.extend(ui::para(
            theme,
            &format!(
                "The journal has nothing from {}{when}.",
                super::install::UNIT_NAME
            ),
            Tone::Normal,
            columns,
        ));
    } else {
        let errors = events.iter().filter(|e| e.level == Level::Error).count();
        let mut summary = events.len().to_string();
        if errors > 0 {
            summary.push_str(&format!(", {}", ui::count(errors, "error", "errors")));
        }
        summary.push_str(&format!(", times in {}", system::zone()));
        lines.push(heading(theme, &summary));
        lines.push(String::new());
        lines.extend(event_lines(theme, events, None, columns));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
fn all_json(events: &[Event]) -> Vec<Value> {
    events
        .iter()
        .map(|e| Value::Object(event_json(e)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_parse_and_bad_ones_are_refused() {
        assert_eq!(parse(&args(&[])).unwrap().unwrap(), Options::default());
        let options = parse(&args(&["-f", "-n", "20", "--since", "2h", "--json"]))
            .unwrap()
            .unwrap();
        assert!(options.follow && options.json);
        assert_eq!(options.lines, Some(20));
        assert_eq!(options.since.as_deref(), Some("-2h"));
        assert_eq!(options.since_text.as_deref(), Some("2h"));
        assert_eq!(
            parse(&args(&["--lines=5", "--since=2026-10-31 09:00"]))
                .unwrap()
                .unwrap()
                .since
                .as_deref(),
            Some("2026-10-31 09:00")
        );
        assert!(parse(&args(&["--help"])).unwrap().is_none());
        for bad in [
            &["--lines"][..],
            &["--lines", "0"],
            &["--lines", "many"],
            &["--since"],
            &["--since", "5x"],
            &["--since", "-x"],
            &["--follow=yes"],
            &["--other"],
            &["extra"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn short_durations_become_relative_times() {
        assert_eq!(since_argument("30m").unwrap(), "-30min");
        assert_eq!(since_argument("90s").unwrap(), "-90s");
        assert_eq!(since_argument("7d").unwrap(), "-7d");
        assert_eq!(since_argument("1w").unwrap(), "-1w");
        assert_eq!(since_argument("yesterday").unwrap(), "yesterday");
        assert_eq!(since_argument("-1h").unwrap(), "-1h");
        assert!(since_argument("").is_err());
    }

    fn event(time: f64, name: &str, detail: &str, level: Level) -> Event {
        Event {
            time,
            source: "server",
            event: name.into(),
            detail: detail.into(),
            level,
            message: format!("{{\"event\": \"{name}\"}}"),
        }
    }

    #[test]
    fn events_line_up_with_errors_marked() {
        let plain = Theme::plain();
        let events = [
            event(1_793_318_400.0, "started", "", Level::Info),
            event(
                1_793_318_460.0,
                "certificate_error",
                "order Invalid: the server could not connect to the client to verify the domain",
                Level::Error,
            ),
            event(1_793_318_470.0, "restarting", "attempt 2", Level::Warning),
        ];
        let lines = event_lines(&plain, &events, None, 80);
        let time = system::clock(1_793_318_400.0);
        assert_eq!(lines[0], format!("  {time}  started"));
        assert!(lines[1].starts_with("✗ "), "{lines:?}");
        assert!(
            lines[1].contains("  certificate_error  order Invalid:"),
            "{lines:?}"
        );
        // The detail wraps under its own column.
        let column =
            crate::cli::format::display_width(&lines[1][..lines[1].find("order").unwrap()]);
        assert!(lines[2].starts_with(&" ".repeat(column)), "{lines:?}");
        assert!(lines[2]
            .trim_start()
            .starts_with(|c: char| c.is_alphabetic()));
        assert!(lines
            .iter()
            .all(|line| crate::cli::format::display_width(line) < 80));
        let last = lines.last().unwrap();
        assert!(last.starts_with("! "), "{lines:?}");
        assert!(last.contains("  restarting         attempt 2"), "{lines:?}");
        let text = render(&plain, &events, None, 80);
        assert!(
            text.starts_with("  Server events  3, 1 error, times in "),
            "{text}"
        );
        let empty = render(&plain, &[], Some("2h"), 80);
        assert_eq!(
            empty,
            "  Server events  none\n\n  The journal has nothing from switchboard-server.service since 2h.\n"
        );
    }

    #[test]
    fn json_keeps_the_logged_line() {
        let value = all_json(&[event(
            1_793_318_400.5,
            "invite_redeemed",
            "a joined",
            Level::Info,
        )]);
        assert_eq!(
            crate::wirejson::dumps(&value[0]),
            r#"{"time": 1793318400.5, "source": "server", "level": "info", "event": "invite_redeemed", "detail": "a joined", "message": "{\"event\": \"invite_redeemed\"}"}"#
        );
    }
}
