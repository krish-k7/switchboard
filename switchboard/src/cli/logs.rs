//! `switchboard logs [--follow] [--lines N]`: what the bridge has been doing.
//!
//! The bridge writes one JSON object per line to
//! `~/.local/share/switchboard/daemon.log` (`at`, the UTC time, then
//! `event` and its fields), and anything it prints on standard error to
//! `daemon-error.log`. `logs` shows the latest events as aligned rows of
//! local time, event, and detail; `--json` prints the log's own lines.

use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use serde_json::{Map, Value};

use super::format::{display_width, text};
use super::ui::{self, Theme, Tone};
use crate::fsutil;

/// The default number of events.
pub const DEFAULT_LINES: usize = 20;
pub const MAX_LINES: usize = 10000;
/// Cells for the time column: `Oct 31 14:02:11`.
const TIME_WIDTH: usize = 15;
/// Cells for the event column: `request failed`.
const EVENT_WIDTH: usize = 14;

pub fn log_paths() -> (String, String) {
    let dir = fsutil::join(&super::local::home_dir(), ".local/share/switchboard");
    (
        fsutil::join(&dir, "daemon.log"),
        fsutil::join(&dir, "daemon-error.log"),
    )
}

/// The last `count` lines of the file at `path` (fewer if it has fewer),
/// and its length, read from the end so a long log costs little.
pub fn tail(path: &str, count: usize) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut start = length;
    let mut buffer: Vec<u8> = Vec::new();
    const CHUNK: u64 = 64 * 1024;
    loop {
        let newlines = buffer.iter().filter(|b| **b == b'\n').count();
        // One more newline than lines wanted: the line before the first.
        if start == 0 || newlines > count {
            break;
        }
        let step = CHUNK.min(start);
        start -= step;
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0; step as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buffer);
        buffer = chunk;
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<String> = text
        .split('\n')
        .map(str::to_string)
        .filter(|l| !l.trim().is_empty())
        .collect();
    // A line cut by the chunk boundary is not shown.
    if start > 0 && !lines.is_empty() && !buffer.starts_with(b"\n") {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(count);
    Ok((lines.split_off(skip), length))
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `2026-10-31T21:02:11Z` as seconds since the epoch.
pub fn parse_stamp(stamp: &str) -> Option<i64> {
    let stamp = stamp.strip_suffix('Z')?;
    let (date, time) = stamp.split_once('T')?;
    let parts: Vec<i64> = date
        .split('-')
        .chain(time.split(':'))
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let [year, month, day, hour, minute, second] = parts.try_into().ok()?;
    if !(0..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The local time of a log stamp: `Oct 31 14:02:11`.
pub fn local_stamp(stamp: &str) -> String {
    match parse_stamp(stamp) {
        Some(seconds) => {
            let tm = crate::fmtutil::localtime(seconds);
            format!(
                "{} {:2} {:02}:{:02}:{:02}",
                MONTHS[(tm.month as usize).clamp(1, 12) - 1],
                tm.day,
                tm.hour,
                tm.minute,
                tm.second
            )
        }
        None => text(stamp),
    }
}

fn field(line: &Map<String, Value>, key: &str) -> String {
    match line.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => text(s),
        Some(other) => text(&other.to_string()),
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// An event as `(event, detail)` in words.
pub fn describe(line: &Map<String, Value>) -> (String, String) {
    let event = field(line, "event");
    let detail = field(line, "detail");
    let with = |head: String| -> String {
        match (head.is_empty(), detail.is_empty()) {
            (_, true) => head,
            (true, false) => detail.clone(),
            (false, false) => format!("{head}: {detail}"),
        }
    };
    match event.as_str() {
        "ready" => {
            let machine = field(line, "machine");
            (
                "connected".into(),
                if machine.is_empty() {
                    "to the server".into()
                } else {
                    format!("to the server as {machine}")
                },
            )
        }
        "offline" => ("offline".into(), detail),
        "disabled" => (
            "turned off".into(),
            "the bridge configuration has \"enabled\": false".into(),
        ),
        "status" => {
            let state = match field(line, "state").as_str() {
                "handed_to_claude" => "handed to the chat".to_string(),
                "held" => "held for approval".into(),
                "refused" => "refused".into(),
                "failed" => "failed".into(),
                "unknown" => "outcome unknown".into(),
                other => other.replace('_', " "),
            };
            (
                "delivery".into(),
                with(format!("message {} {state}", short(&field(line, "id")))),
            )
        }
        "request_failed" => {
            let mut head = field(line, "operation").replace('_', " ");
            let id = field(line, "id");
            if !id.is_empty() {
                head.push_str(&format!(" (message {})", short(&id)));
            }
            ("request failed".into(), with(head))
        }
        "helper_failed" => (
            "helper failed".into(),
            format!("for {}", field(line, "route")),
        ),
        "native_rejected" => {
            let route = field(line, "route");
            (
                "refused".into(),
                if route.is_empty() {
                    detail
                } else {
                    with(format!("from {route}"))
                },
            )
        }
        "session_expired" => (
            "chat left".into(),
            with(format!("chat {}", short(&field(line, "session")))),
        ),
        "error" => ("error".into(), detail),
        "" => (String::new(), String::new()),
        other => {
            let rest: Vec<String> = line
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "at" | "event"))
                .map(|(k, _)| format!("{k}={}", field(line, k)))
                .collect();
            (other.replace('_', " "), rest.join(" "))
        }
    }
}

/// One log line as a row: time, event, detail, wrapped under the detail
/// column. A line that isn't an event is shown as it is.
pub fn row(theme: &Theme, line: &str, columns: usize) -> Vec<String> {
    let parsed: Option<Map<String, Value>> = serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let (time, event, detail) = match parsed {
        Some(map) => {
            let (event, detail) = describe(&map);
            (local_stamp(&field(&map, "at")), event, detail)
        }
        None => (String::new(), "output".into(), text(line)),
    };
    cells(theme, &time, &event, &detail, columns)
}

fn cells(theme: &Theme, time: &str, event: &str, detail: &str, columns: usize) -> Vec<String> {
    let lead = ui::INDENT + TIME_WIDTH + 2 + EVENT_WIDTH + 2;
    // Rows of data, like a table: as wide as the terminal.
    let room = columns.saturating_sub(1).saturating_sub(lead);
    let head = format!(
        "{}{}{}  {}{}  ",
        " ".repeat(ui::INDENT),
        time,
        " ".repeat(TIME_WIDTH.saturating_sub(display_width(time))),
        theme.strong(event),
        " ".repeat(EVENT_WIDTH.saturating_sub(display_width(event))),
    );
    if room < 20 {
        // Too narrow for columns: the detail under the time and event.
        let mut lines = vec![format!(
            "{}{time}  {}",
            " ".repeat(ui::INDENT),
            theme.strong(event)
        )];
        if !detail.is_empty() {
            lines.extend(
                ui::para(theme, detail, Tone::Normal, columns)
                    .into_iter()
                    .map(|l| format!("  {l}")),
            );
        }
        return lines;
    }
    super::format::wrap_cells(detail, room)
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            if index == 0 {
                format!("{head}{part}").trim_end().to_string()
            } else {
                format!("{}{part}", " ".repeat(lead))
            }
        })
        .collect()
}

/// A log line for `--json`: as logged, but with control characters (and
/// the ones that reorder text) written as `\uXXXX`, so a line that isn't
/// JSON can't send escape sequences to the terminal. In a JSON line they
/// can only be inside strings, where the escape means the same.
pub fn json_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `logs`: print the latest events, then with `follow` keep printing new
/// ones. Returns the exit status.
pub fn run(lines: usize, follow: bool, json: bool) -> i32 {
    let (log, errors) = log_paths();
    let theme = Theme::for_fd(1);
    let columns = super::format::output_width();
    // Whether the line was written: once nobody reads the output (a closed
    // pipe), there is nothing more to do.
    let show = |line: &str| {
        if json {
            super::out_ok(&format!("{}\n", json_line(line)))
        } else {
            super::out_ok(&format!("{}\n", row(&theme, line, columns).join("\n")))
        }
    };
    let (latest, mut offset) = match tail(&log, lines) {
        Ok(found) => found,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), 0),
        Err(error) => {
            super::print_error(
                &format!("the bridge's log can't be read ({error})"),
                None,
                json,
            );
            return 1;
        }
    };
    if latest.is_empty() && !json {
        let what = if fsutil::exists(&log) {
            "The bridge hasn't logged anything yet."
        } else {
            "The bridge hasn't logged anything yet: there is no log at ~/.local/share/switchboard/daemon.log."
        };
        super::out(&format!(
            "{}\n",
            ui::para(&theme, what, Tone::Normal, columns).join("\n")
        ));
    }
    for line in &latest {
        if !show(line) {
            return 0;
        }
    }
    if !json {
        if let Ok((said, _)) = tail(&errors, lines.min(10)) {
            if !said.is_empty() {
                let mut out = vec![String::new()];
                out.extend(ui::heading(
                    &theme,
                    "The bridge's error output, latest last",
                    columns,
                ));
                for line in &said {
                    out.extend(ui::para(&theme, line, Tone::Normal, columns));
                }
                super::out(&format!("{}\n", out.join("\n")));
            }
        }
    }
    if !follow {
        return 0;
    }
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if super::reader_gone(1) {
            return 0;
        }
        let Ok(mut file) = std::fs::File::open(&log) else {
            continue;
        };
        let Ok(length) = file.metadata().map(|m| m.len()) else {
            continue;
        };
        if length < offset {
            // The log was replaced or emptied: start from its beginning.
            offset = 0;
        }
        if length == offset || file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut fresh = Vec::new();
        if file.read_to_end(&mut fresh).is_err() {
            continue;
        }
        // Only whole lines; a line still being written waits.
        let Some(end) = fresh.iter().rposition(|b| *b == b'\n') else {
            continue;
        };
        offset += end as u64 + 1;
        for line in String::from_utf8_lossy(&fresh[..end]).split('\n') {
            if !line.trim().is_empty() && !show(line) {
                return 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_read_back_as_the_same_instant() {
        assert_eq!(parse_stamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_stamp("2026-10-31T21:02:11Z"), Some(1793480531));
        assert_eq!(parse_stamp("2026-02-29T00:00:00"), None);
        assert_eq!(parse_stamp("nonsense"), None);
        // A year far out of range is refused, not overflowed.
        assert_eq!(parse_stamp("99999999999999999-01-01T00:00:00Z"), None);
        assert_eq!(parse_stamp("-9223372036854775808-01-01T00:00:00Z"), None);
        assert_eq!(parse_stamp("10000-01-01T00:00:00Z"), None);
        assert!(parse_stamp("9999-12-31T23:59:59Z").is_some());
    }

    #[test]
    fn json_lines_carry_no_escape_sequences() {
        assert_eq!(
            json_line("plain \u{1b}[2J\u{1b}]0;title\u{7}\r\u{9b}31m"),
            "plain \\u001b[2J\\u001b]0;title\\u0007\\u000d\\u009b31m"
        );
        assert_eq!(json_line("a\u{202e}b"), "a\\u202eb");
        let logged = r#"{"at": "2026-10-31T21:02:11Z", "event": "ready", "machine": "laptop é"}"#;
        assert_eq!(json_line(logged), logged);
        // In a JSON string the escape reads back as the same character.
        let value: Value =
            serde_json::from_str(&json_line("{\"detail\": \"x\u{7f}y\u{85}\"}")).unwrap();
        assert_eq!(value["detail"], "x\u{7f}y\u{85}");
    }

    #[test]
    fn events_read_as_words() {
        let line = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            describe(&line(
                serde_json::json!({"event": "ready", "machine": "laptop"})
            )),
            ("connected".into(), "to the server as laptop".into())
        );
        assert_eq!(
            describe(&line(
                serde_json::json!({"event": "status", "id": "0123456789abcdef", "state": "handed_to_claude"})
            )),
            (
                "delivery".into(),
                "message 01234567 handed to the chat".into()
            )
        );
        assert_eq!(
            describe(&line(
                serde_json::json!({"event": "request_failed", "operation": "send", "id": "abcdef0123", "detail": "queue full"})
            )),
            (
                "request failed".into(),
                "send (message abcdef01): queue full".into()
            )
        );
        assert_eq!(
            describe(&line(
                serde_json::json!({"event": "session_expired", "session": "11111111-2222", "detail": "no activity for 7 days"})
            )),
            (
                "chat left".into(),
                "chat 11111111: no activity for 7 days".into()
            )
        );
        assert_eq!(
            describe(&line(serde_json::json!({"event": "new_thing", "count": 3}))),
            ("new thing".into(), "count=3".into())
        );
    }

    #[test]
    fn rows_align_and_wrap_under_the_detail() {
        let theme = Theme::plain();
        let row = row(
            &theme,
            r#"{"at": "2026-10-31T21:02:11Z", "event": "offline", "detail": "server unavailable; native proxies withdrawn and a long tail of words to wrap"}"#,
            80,
        );
        assert!(row[0].starts_with("  "), "{row:?}");
        assert!(
            row[0].contains("  offline         server unavailable"),
            "{row:?}"
        );
        assert!(
            row.len() > 1
                && row[1].starts_with(&" ".repeat(ui::INDENT + TIME_WIDTH + 2 + EVENT_WIDTH + 2))
        );
        assert!(row.iter().all(|l| display_width(l) <= 79));
        assert_eq!(
            super::row(&theme, "not json", 80),
            [format!(
                "  {}  output          not json",
                " ".repeat(TIME_WIDTH)
            )]
        );
    }

    #[test]
    fn the_tail_is_read_from_the_end() {
        let dir = std::env::temp_dir().join(format!("sblogs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.log").to_string_lossy().into_owned();
        let text: String = (0..20000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, &text).unwrap();
        let (lines, length) = tail(&path, 3).unwrap();
        assert_eq!(lines, ["line 19997", "line 19998", "line 19999"]);
        assert_eq!(length, text.len() as u64);
        assert_eq!(tail(&path, 50000).unwrap().0.len(), 20000);
        std::fs::write(&path, "").unwrap();
        assert!(tail(&path, 3).unwrap().0.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
