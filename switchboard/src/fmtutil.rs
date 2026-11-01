//! Formatting shared by the bridge and CLI: indented JSON, UTC and local
//! timestamps, the wall clock, rounding, shell quoting, and integer parsing.

use serde_json::Value;

use crate::wirejson::dumps;

/// Indented JSON in the wire format: one item per line, `indent` spaces per
/// level, `,` at line ends and `": "` after keys, empty arrays and objects as
/// `[]` and `{}`, keys sorted by code point when `sort_keys`, no trailing
/// newline.
pub fn dumps_indent(value: &Value, indent: usize, sort_keys: bool) -> String {
    let mut out = String::new();
    write_indent(&mut out, value, indent, sort_keys, 0);
    out
}

fn write_indent(out: &mut String, value: &Value, indent: usize, sort_keys: bool, level: usize) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                out.push_str(if index == 0 { "\n" } else { ",\n" });
                out.push_str(&" ".repeat(indent * (level + 1)));
                write_indent(out, item, indent, sort_keys, level + 1);
            }
            out.push('\n');
            out.push_str(&" ".repeat(indent * level));
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                out.push_str(if index == 0 { "\n" } else { ",\n" });
                out.push_str(&" ".repeat(indent * (level + 1)));
                out.push_str(&dumps(&Value::String(key.clone())));
                out.push_str(": ");
                write_indent(out, item, indent, sort_keys, level + 1);
            }
            out.push('\n');
            out.push_str(&" ".repeat(indent * level));
            out.push('}');
        }
        other => out.push_str(&dumps(other)),
    }
}

/// Seconds since the Unix epoch, with a fractional part.
pub fn time_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A JSON number that always prints as a float (`2.0`, not `2`).
pub fn float_value(value: f64) -> Value {
    serde_json::from_str::<Value>(&crate::wirejson::float_repr(value)).unwrap_or(Value::Null)
}

/// The nearest integer, ties to even.
pub fn round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let diff = value - floor;
    let up = diff > 0.5 || (diff == 0.5 && (floor / 2.0).fract() != 0.0);
    let rounded = if up { floor + 1.0 } else { floor };
    rounded as i64
}

/// Broken-down calendar time.
#[derive(Clone, Copy, Debug)]
pub struct Tm {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Calendar time in UTC for seconds since the epoch.
pub fn gmtime(seconds: i64) -> Tm {
    let days = seconds.div_euclid(86400);
    let rest = seconds.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    Tm {
        year,
        month,
        day,
        hour: (rest / 3600) as u32,
        minute: (rest % 3600 / 60) as u32,
        second: (rest % 60) as u32,
    }
}

/// Calendar time in the local time zone (UTC if the C library can't say).
pub fn localtime(seconds: i64) -> Tm {
    // SAFETY: localtime_r writes into a caller-owned tm.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        // `time_t` is 64 bits on every supported target; libc marks the musl
        // alias deprecated only because of 32-bit musl.
        #[allow(deprecated)]
        let t: libc::time_t = seconds as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return gmtime(seconds);
        }
        Tm {
            year: tm.tm_year as i64 + 1900,
            month: tm.tm_mon as u32 + 1,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            minute: tm.tm_min as u32,
            second: tm.tm_sec as u32,
        }
    }
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`: the bridge log stamp.
pub fn utc_stamp() -> String {
    let tm = gmtime(time_now().floor() as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.year, tm.month, tm.day, tm.hour, tm.minute, tm.second
    )
}

/// The current local time as `YYYYMMDDTHHMMSS`.
pub fn local_compact_stamp() -> String {
    let tm = localtime(time_now().floor() as i64);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}",
        tm.year, tm.month, tm.day, tm.hour, tm.minute, tm.second
    )
}

/// Nanoseconds since the Unix epoch.
pub fn time_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Quote for a POSIX shell: unchanged if every character is ASCII
/// alphanumeric or one of `@%+=:,./-_`, otherwise single-quoted with each
/// `'` written as `'"'"'`; the empty string is `''`.
pub fn shlex_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    let safe = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// An integer from ASCII decimal text: surrounding whitespace, an optional
/// sign, and single underscores between digits are allowed. Large values clamp.
pub fn parse_int(text: &str) -> Option<i64> {
    let body = crate::text::strip(text);
    let (negative, digits) = match body.as_bytes().first() {
        Some(b'-') => (true, &body[1..]),
        Some(b'+') => (false, &body[1..]),
        _ => (false, body),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let mut value: i64 = 0;
    for byte in digits.bytes().filter(|b| *b != b'_') {
        value = value
            .saturating_mul(10)
            .saturating_add((byte - b'0') as i64);
    }
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indented_json_layout() {
        let value: Value =
            serde_json::from_str(r#"{"b": [1, {"z": [], "y": {}}], "a": "\u00e9"}"#).unwrap();
        assert_eq!(
            dumps_indent(&value, 2, true),
            "{\n  \"a\": \"\\u00e9\",\n  \"b\": [\n    1,\n    {\n      \"y\": {},\n      \"z\": []\n    }\n  ]\n}"
        );
        assert_eq!(dumps_indent(&serde_json::json!([]), 2, false), "[]");
    }

    #[test]
    fn time_rounding_quoting_and_integers() {
        let tm = gmtime(1761771713);
        assert_eq!(
            (tm.year, tm.month, tm.day, tm.hour, tm.minute, tm.second),
            (2025, 10, 29, 21, 1, 53)
        );
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(3.5), 4);
        assert_eq!(round_half_even(1761771713.6), 1761771714);
        assert_eq!(shlex_quote("/a/b-c.d"), "/a/b-c.d");
        assert_eq!(shlex_quote("/a b/it's"), "'/a b/it'\"'\"'s'");
        assert_eq!(parse_int(" 1_0 "), Some(10));
        assert_eq!(parse_int("+5"), Some(5));
        assert_eq!(parse_int("1__0"), None);
        assert_eq!(parse_int("x"), None);
    }
}
