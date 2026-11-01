//! JSON as Switchboard writes and compares it.
//!
//! Every file, frame, and event is written as ASCII-only JSON (non-ASCII
//! escaped as `\uXXXX` UTF-16 units) with `", "` and `": "` separators and
//! keys in insertion order, the format older bridges and existing files
//! expect byte for byte. Decoded values compare numerically across integers,
//! floats, and booleans (`1 == 1.0 == true`), so checks accept and refuse the
//! same values everywhere.

use serde_json::{Map, Number, Value};

use crate::error::Error;
use crate::text;

/// Decode one UTF-8 JSON document; a later duplicate key replaces an earlier one.
pub fn loads(raw: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice(raw).map_err(|exc| Error::value(json_error(&exc)))
}

fn json_error(exc: &serde_json::Error) -> String {
    // Callers act on the error kind (a value error), not its wording; serde's
    // text names the line and column.
    exc.to_string()
}

/// Encode in the wire format above: ASCII-only, `", "` and `": "` separators,
/// insertion order, no trailing newline.
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number_str(number)),
        Value::String(string) => write_string(out, string),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
        }
    }
    out.push('"');
}

/// A decoded JSON number: an integer literal (any size) or a float.
#[derive(Clone, Debug, PartialEq)]
pub enum NumKind {
    /// An integer literal in canonical decimal form (`-0` becomes `0`).
    Int(String),
    Float(f64),
}

pub fn num_kind(number: &Number) -> NumKind {
    let raw = number.as_str();
    if raw.contains(['.', 'e', 'E']) {
        NumKind::Float(raw.parse::<f64>().unwrap_or(f64::NAN))
    } else if raw == "-0" {
        NumKind::Int("0".into())
    } else {
        NumKind::Int(raw.to_string())
    }
}

fn number_str(number: &Number) -> String {
    match num_kind(number) {
        NumKind::Int(value) => value,
        NumKind::Float(value) => float_repr(value),
    }
}

/// The shortest decimal that round-trips the float: positional between
/// 1e-4 and 1e16 (always with a fractional part, `2.0`), otherwise exponent
/// form with a signed two-digit-minimum exponent (`1e+16`, `1.5e-07`);
/// `NaN`, `Infinity`, and `-Infinity` for non-finite values.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0.0");
    }
    // Rust's `{:e}` prints the shortest round-trip digits.
    let formatted = format!("{:e}", value.abs());
    let (mantissa, exponent) = formatted.split_once('e').expect("exponent form");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent.parse().expect("exponent digits");
    let point = exponent + 1;
    let body = if point > -4 && point <= 16 {
        if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        }
    } else {
        let rest = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{}{}e{}{:02}", &digits[..1], rest, exp_sign, exponent.abs())
    };
    format!("{sign}{body}")
}

/// The digits of a JSON integer (not a float, and not a boolean).
pub fn as_int(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => match num_kind(number) {
            NumKind::Int(value) => Some(value),
            NumKind::Float(_) => None,
        },
        _ => None,
    }
}

/// Compare two decimal integer strings numerically.
pub fn int_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (left_negative, left_digits) = left.strip_prefix('-').map_or((false, left), |d| (true, d));
    let (right_negative, right_digits) = right
        .strip_prefix('-')
        .map_or((false, right), |d| (true, d));
    let magnitude = left_digits
        .len()
        .cmp(&right_digits.len())
        .then_with(|| left_digits.cmp(right_digits));
    match (left_negative, right_negative) {
        (false, false) => magnitude,
        (true, true) => magnitude.reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

fn numeric(value: &Value) -> Option<NumKind> {
    match value {
        Value::Bool(flag) => Some(NumKind::Int(if *flag { "1" } else { "0" }.into())),
        Value::Number(number) => Some(num_kind(number)),
        _ => None,
    }
}

fn float_equals_int(float: f64, int: &str) -> bool {
    if !float.is_finite() || float.fract() != 0.0 || float.abs() >= 1.0e38 {
        return false;
    }
    int.parse::<i128>()
        .is_ok_and(|parsed| parsed == float as i128)
}

/// Equality of decoded JSON values: numbers and booleans compare by numeric
/// value (`1`, `1.0`, and `true` are equal), objects regardless of key order.
pub fn value_eq(left: &Value, right: &Value) -> bool {
    if let (Some(a), Some(b)) = (numeric(left), numeric(right)) {
        return match (a, b) {
            (NumKind::Int(a), NumKind::Int(b)) => a == b,
            (NumKind::Float(a), NumKind::Float(b)) => a == b,
            (NumKind::Int(a), NumKind::Float(b)) | (NumKind::Float(b), NumKind::Int(a)) => {
                float_equals_int(b, &a)
            }
        };
    }
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| value_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, x)| b.get(key).is_some_and(|y| value_eq(x, y)))
        }
        _ => false,
    }
}

/// [`value_eq`] where either side may be absent; absent equals `null`.
pub fn value_eq_opt(left: Option<&Value>, right: Option<&Value>) -> bool {
    value_eq(left.unwrap_or(&Value::Null), right.unwrap_or(&Value::Null))
}

/// Whether a decoded JSON value counts as set: false for absent, `null`,
/// `false`, zero, and empty strings, arrays, and objects.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => match num_kind(number) {
            NumKind::Int(value) => value != "0",
            NumKind::Float(value) => value != 0.0,
        },
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// The text of a decoded JSON value in error details: a string as is,
/// anything else as [`repr_value`] shows it.
pub fn str_value(value: &Value) -> String {
    match value {
        Value::String(string) => string.clone(),
        other => repr_value(other),
    }
}

/// A decoded JSON value as error details show it: `None`, `True`, `False`,
/// numbers as written, strings quoted by [`text::repr_str`], and `[...]` and
/// `{key: value}` with `, ` between items.
pub fn repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(number) => number_str(number),
        Value::String(string) => text::repr_str(string),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(repr_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| format!("{}: {}", text::repr_str(key), repr_value(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A decoded value that must be a JSON object; anything else is a value
/// error with `message`.
pub fn object(value: Value, message: &str) -> Result<Map<String, Value>, Error> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Error::value(message)),
    }
}

/// The value at `key` when it is a string.
pub fn get_str<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// Build a JSON object from ordered pairs.
#[macro_export]
macro_rules! obj {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = serde_json::Map::new();
        $( map.insert(String::from($key), serde_json::Value::from($value)); )*
        map
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_writes_the_ascii_wire_format() {
        let value =
            loads(br#"{"a": [1, 2.5, "x\u00e9\ud83d\ude00\u007f"], "b": null, "c": true, "a": 3}"#)
                .unwrap();
        assert_eq!(dumps(&value), r#"{"a": 3, "b": null, "c": true}"#);
        let value =
            loads("[\"\\u0001\\b\\f\\n\\r\\t\\\"\\\\\", \"λ😀\u{7f}\"]".as_bytes()).unwrap();
        assert_eq!(
            dumps(&value),
            r#"["\u0001\b\f\n\r\t\"\\", "\u03bb\ud83d\ude00\u007f"]"#
        );
    }

    #[test]
    fn floats_print_shortest_round_trip() {
        for (value, expected) in [
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5, "1.5"),
            (-2.0, "-2.0"),
            (123456.789, "123456.789"),
            (1.2e100, "1.2e+100"),
            (1.5e-7, "1.5e-07"),
            (-0.0, "-0.0"),
        ] {
            assert_eq!(float_repr(value), expected);
        }
        assert_eq!(
            dumps(&loads(b"[1E2, -0, 1.50]").unwrap()),
            "[100.0, 0, 1.5]"
        );
    }

    #[test]
    fn equality_is_numeric_across_types() {
        let one = loads(b"[1, 1.0, true, \"1\", 2]").unwrap();
        let items = one.as_array().unwrap();
        assert!(value_eq(&items[0], &items[1]));
        assert!(value_eq(&items[0], &items[2]));
        assert!(!value_eq(&items[0], &items[3]));
        assert!(!value_eq(&items[0], &items[4]));
        assert_eq!(int_cmp("-5", "3"), std::cmp::Ordering::Less);
        assert_eq!(
            int_cmp("100000000000000000000000", "99"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn str_value_quotes_nested_values() {
        let value = loads(br#"{"a": [1, null, true, "it's"], "b": 1.0}"#).unwrap();
        assert_eq!(
            str_value(&value),
            "{'a': [1, None, True, \"it's\"], 'b': 1.0}"
        );
    }
}
