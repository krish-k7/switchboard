//! Text helpers for human output: one-line sanitized text, terminal cell
//! widths, wrapping, and colors. What each command shows is in `view`.

use serde_json::Value;

use super::unicode_cli::{CASEFOLD, WIDE, ZERO_WIDTH};
use crate::text::{is_printable, is_space};
use crate::wirejson::str_value;

fn in_table(table: &[(u32, u32)], c: char) -> bool {
    let code = c as u32;
    table
        .binary_search_by(|&(low, high)| {
            if high < code {
                std::cmp::Ordering::Less
            } else if low > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Unicode full case folding (so `Straße` folds to `strasse`).
pub fn casefold(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match CASEFOLD.binary_search_by(|(code, _)| code.cmp(&(c as u32))) {
            Ok(index) => out.push_str(CASEFOLD[index].1),
            Err(_) => out.push(c),
        }
    }
    out
}

/// One line without terminal controls.
pub fn text(value: &str) -> String {
    let replaced: String = value
        .chars()
        .map(|c| if is_printable(c) { c } else { ' ' })
        .collect();
    split(&replaced).join(" ")
}

/// `text` for any decoded JSON value (absent or null is empty).
pub fn text_value(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(other) => text(&str_value(other)),
    }
}

/// Split on runs of whitespace (`text::is_space`), dropping empty pieces.
pub fn split(value: &str) -> Vec<&str> {
    value.split(is_space).filter(|p| !p.is_empty()).collect()
}

pub fn style(value: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}

/// Terminal cells for sanitized text (CJK wide, combining marks zero).
pub fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|c| {
            if in_table(ZERO_WIDTH, c) {
                0
            } else if in_table(WIDE, c) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// The terminal width in columns: `COLUMNS` when set, else the terminal's,
/// else 80; at least 1.
pub fn output_width() -> usize {
    super::args::terminal_columns().max(1) as usize
}

/// Wrap words by terminal cells, splitting only words wider than the line.
pub fn wrap_cells(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let cleaned = text(value);
    for word in split(&cleaned) {
        if !line.is_empty() && display_width(&format!("{line} {word}")) <= width {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        for c in word.chars() {
            let mut candidate = line.clone();
            candidate.push(c);
            if !line.is_empty() && display_width(&candidate) > width {
                lines.push(std::mem::take(&mut line));
            }
            line.push(c);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub fn padded(value: &str, width: usize) -> String {
    let used = display_width(value);
    format!("{value}{}", " ".repeat(width.saturating_sub(used)))
}

/// Color on a descriptor: a tty, `NO_COLOR` absent, and `TERM` not dumb.
pub fn terminal_color(fd: i32) -> bool {
    // SAFETY: isatty on a standard descriptor.
    let tty = unsafe { libc::isatty(fd) == 1 };
    tty && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").ok().as_deref() != Some("dumb")
}

/// How long ago a machine was seen, in words (`seen 5 min ago`).
pub fn ago(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else {
        return "not seen since the server started".into();
    };
    let seconds = (seconds.max(0.0)) as i64;
    if seconds < 60 {
        "seen just now".into()
    } else if seconds < 3600 {
        format!("seen {} min ago", seconds / 60)
    } else if seconds < 86400 {
        format!("seen {} h ago", seconds / 3600)
    } else {
        format!("seen {} days ago", seconds / 86400)
    }
}

/// Whole days left, rounding up, never negative.
pub fn days_left(expires_at: f64, now: f64) -> i64 {
    let remaining = expires_at - now;
    let ceil = -((-remaining / 86400.0).floor());
    (ceil as i64).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_casefold_and_sanitizing() {
        assert_eq!(display_width("☕\u{fe0f}"), 2);
        assert_eq!(display_width("café"), 4);
        assert_eq!(display_width("a\u{20dd}"), 1);
        assert_eq!(display_width("模型"), 4);
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(text("a\x1b[2J\nb\r\t"), "a [2J b");
        assert_eq!(wrap_cells("aaaa bbbb", 4), ["aaaa", "bbbb"]);
        assert_eq!(wrap_cells("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap_cells("", 3), [""]);
        assert_eq!(days_left(86400.0 * 2.0 - 1.0, 0.0), 2);
        assert_eq!(days_left(-5.0, 0.0), 0);
    }
}
