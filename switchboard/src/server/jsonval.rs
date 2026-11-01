//! JSON values as the server reads and writes them.
//!
//! Request bodies and the registry are decoded into these values and encoded
//! back. A JSON string may hold a lone UTF-16 surrogate escape (`"\ud800"`
//! parses), which a Rust `String` cannot, so strings here are kept as
//! generalized UTF-8 (UTF-8 that may also encode surrogate code points); the
//! server refuses such text only when it measures it (`invalid_unicode`).
//! Byte order of that encoding is code point order, so comparison and sorted
//! keys follow code points. Bytes are decoded with their encoding detected
//! from the first bytes (UTF-8, UTF-8 with BOM, UTF-16, or UTF-32), with
//! surrogates passed through.

use std::fmt::Write as _;

use indexmap::IndexMap;

use crate::wirejson::float_repr;

/// A JSON string: generalized UTF-8, possibly holding lone surrogates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JsonStr(Vec<u8>);

impl JsonStr {
    pub fn new(value: &str) -> Self {
        JsonStr(value.as_bytes().to_vec())
    }

    /// The text when it holds no surrogates (it can be encoded as UTF-8).
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The length in code points.
    pub fn char_len(&self) -> usize {
        self.0.iter().filter(|byte| *byte & 0xC0 != 0x80).count()
    }

    /// Code points in order, surrogates included.
    pub fn code_points(&self) -> impl Iterator<Item = u32> + '_ {
        let bytes = &self.0;
        let mut index = 0;
        std::iter::from_fn(move || {
            let first = *bytes.get(index)? as u32;
            let (width, initial) = match first {
                0x00..=0x7F => (1, first),
                0xC0..=0xDF => (2, first & 0x1F),
                0xE0..=0xEF => (3, first & 0x0F),
                _ => (4, first & 0x07),
            };
            let mut code = initial;
            for offset in 1..width {
                code = (code << 6) | (bytes[index + offset] as u32 & 0x3F);
            }
            index += width;
            Some(code)
        })
    }

    fn push(&mut self, code: u32) {
        match code {
            0..=0x7F => self.0.push(code as u8),
            0x80..=0x7FF => {
                self.0.push(0xC0 | (code >> 6) as u8);
                self.0.push(0x80 | (code & 0x3F) as u8);
            }
            0x800..=0xFFFF => {
                self.0.push(0xE0 | (code >> 12) as u8);
                self.0.push(0x80 | ((code >> 6) & 0x3F) as u8);
                self.0.push(0x80 | (code & 0x3F) as u8);
            }
            _ => {
                self.0.push(0xF0 | (code >> 18) as u8);
                self.0.push(0x80 | ((code >> 12) & 0x3F) as u8);
                self.0.push(0x80 | ((code >> 6) & 0x3F) as u8);
                self.0.push(0x80 | (code & 0x3F) as u8);
            }
        }
    }

    pub fn eq_str(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl From<&str> for JsonStr {
    fn from(value: &str) -> Self {
        JsonStr::new(value)
    }
}

impl From<String> for JsonStr {
    fn from(value: String) -> Self {
        JsonStr(value.into_bytes())
    }
}

pub type Dict = IndexMap<JsonStr, Val>;

/// A decoded JSON value: integers of any size (kept as their decimal text)
/// apart from floats, strings, lists, and insertion-ordered objects.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Int(String),
    Float(f64),
    Str(JsonStr),
    List(Vec<Val>),
    Dict(Dict),
}

impl Val {
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Val::Dict(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_json_str(&self) -> Option<&JsonStr> {
        match self {
            Val::Str(text) => Some(text),
            _ => None,
        }
    }

    /// The value when it is a string without surrogates.
    pub fn as_str(&self) -> Option<&str> {
        self.as_json_str().and_then(JsonStr::as_str)
    }

    /// Whether the value is the string `literal`.
    pub fn is_str(&self, literal: &str) -> bool {
        matches!(self, Val::Str(text) if text.eq_str(literal))
    }
}

impl From<bool> for Val {
    fn from(value: bool) -> Self {
        Val::Bool(value)
    }
}

impl From<&str> for Val {
    fn from(value: &str) -> Self {
        Val::Str(JsonStr::new(value))
    }
}

impl From<String> for Val {
    fn from(value: String) -> Self {
        Val::Str(value.into())
    }
}

impl From<JsonStr> for Val {
    fn from(value: JsonStr) -> Self {
        Val::Str(value)
    }
}

impl From<f64> for Val {
    fn from(value: f64) -> Self {
        Val::Float(value)
    }
}

impl From<i64> for Val {
    fn from(value: i64) -> Self {
        Val::Int(value.to_string())
    }
}

impl From<Vec<Val>> for Val {
    fn from(value: Vec<Val>) -> Self {
        Val::List(value)
    }
}

impl From<Dict> for Val {
    fn from(value: Dict) -> Self {
        Val::Dict(value)
    }
}

impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(value: Option<T>) -> Self {
        value.map_or(Val::Null, Into::into)
    }
}

/// The value under `key`.
pub fn get<'a>(map: &'a Dict, key: &str) -> Option<&'a Val> {
    map.get(&JsonStr::new(key))
}

/// Whether `map` has a key not in `allowed`.
pub fn has_other_keys(map: &Dict, allowed: &[&str]) -> bool {
    map.keys()
        .any(|key| !allowed.iter().any(|name| key.eq_str(name)))
}

/// Build an object from ordered pairs.
pub fn dict<const N: usize>(pairs: [(&str, Val); N]) -> Dict {
    let mut map = Dict::with_capacity(N);
    for (key, value) in pairs {
        map.insert(JsonStr::new(key), value);
    }
    map
}

/// `map[key] = value` (an existing key keeps its position).
pub fn set(map: &mut Dict, key: &str, value: impl Into<Val>) {
    map.insert(JsonStr::new(key), value.into());
}

// Decoding -------------------------------------------------------------------

/// Decode one JSON document from bytes; `None` for invalid JSON, undecodable
/// bytes, nesting deeper than `MAX_DEPTH`, or an integer longer than
/// `MAX_INT_DIGITS`.
pub fn loads(raw: &[u8]) -> Option<Val> {
    let text = decode_bytes(raw)?;
    Parser {
        text: &text,
        depth: 0,
    }
    .document()
}

/// Decode one JSON document from already decoded text (a leading BOM is
/// refused).
pub fn loads_str(text: &str) -> Option<Val> {
    let points: Vec<u32> = text.chars().map(|c| c as u32).collect();
    if points.first() == Some(&0xFEFF) {
        return None;
    }
    Parser {
        text: &points,
        depth: 0,
    }
    .document()
}

/// Detect the encoding from the BOM or the zero bytes among the first four,
/// then decode, letting encoded surrogates through.
fn decode_bytes(raw: &[u8]) -> Option<Vec<u32>> {
    let starts = |prefix: &[u8]| raw.starts_with(prefix);
    if starts(&[0x00, 0x00, 0xFE, 0xFF]) || starts(&[0xFF, 0xFE, 0x00, 0x00]) {
        let big = starts(&[0x00, 0x00, 0xFE, 0xFF]);
        return decode_utf32(&raw[4..], big);
    }
    if starts(&[0xFE, 0xFF]) || starts(&[0xFF, 0xFE]) {
        return decode_utf16(&raw[2..], starts(&[0xFE, 0xFF]));
    }
    if starts(&[0xEF, 0xBB, 0xBF]) {
        return decode_utf8(&raw[3..]);
    }
    if raw.len() >= 4 {
        if raw[0] == 0 {
            return if raw[1] != 0 {
                decode_utf16(raw, true)
            } else {
                decode_utf32(raw, true)
            };
        }
        if raw[1] == 0 {
            return if raw[2] != 0 || raw[3] != 0 {
                decode_utf16(raw, false)
            } else {
                decode_utf32(raw, false)
            };
        }
    } else if raw.len() == 2 {
        if raw[0] == 0 {
            return decode_utf16(raw, true);
        }
        if raw[1] == 0 {
            return decode_utf16(raw, false);
        }
    }
    decode_utf8(raw)
}

/// UTF-8 that also accepts encoded surrogates.
fn decode_utf8(raw: &[u8]) -> Option<Vec<u32>> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let first = raw[index];
        let (width, minimum, initial) = match first {
            0x00..=0x7F => {
                out.push(first as u32);
                index += 1;
                continue;
            }
            0xC2..=0xDF => (2, 0x80, (first & 0x1F) as u32),
            0xE0..=0xEF => (3, 0x800, (first & 0x0F) as u32),
            0xF0..=0xF4 => (4, 0x10000, (first & 0x07) as u32),
            _ => return None,
        };
        if index + width > raw.len() {
            return None;
        }
        let mut code = initial;
        for offset in 1..width {
            let byte = raw[index + offset];
            if byte & 0xC0 != 0x80 {
                return None;
            }
            code = (code << 6) | (byte & 0x3F) as u32;
        }
        if code < minimum || code > 0x10FFFF {
            return None;
        }
        out.push(code);
        index += width;
    }
    Some(out)
}

fn decode_utf16(raw: &[u8], big: bool) -> Option<Vec<u32>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u32> = raw
        .chunks(2)
        .map(|pair| {
            if big {
                u16::from_be_bytes([pair[0], pair[1]]) as u32
            } else {
                u16::from_le_bytes([pair[0], pair[1]]) as u32
            }
        })
        .collect();
    let mut out = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if (0xD800..0xDC00).contains(&unit)
            && index + 1 < units.len()
            && (0xDC00..0xE000).contains(&units[index + 1])
        {
            out.push(0x10000 + ((unit - 0xD800) << 10) + (units[index + 1] - 0xDC00));
            index += 2;
        } else {
            out.push(unit);
            index += 1;
        }
    }
    Some(out)
}

fn decode_utf32(raw: &[u8], big: bool) -> Option<Vec<u32>> {
    if !raw.len().is_multiple_of(4) {
        return None;
    }
    raw.chunks(4)
        .map(|quad| {
            let code = if big {
                u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]])
            } else {
                u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]])
            };
            (code <= 0x10FFFF).then_some(code)
        })
        .collect()
}

/// Nesting deeper than this is refused.
const MAX_DEPTH: usize = 1000;
/// Integers with more digits than this are refused.
const MAX_INT_DIGITS: usize = 4300;

struct Parser<'a> {
    text: &'a [u32],
    depth: usize,
}

fn is_ws(code: u32) -> bool {
    matches!(code, 0x20 | 0x09 | 0x0A | 0x0D)
}

fn is_digit(code: Option<&u32>) -> bool {
    matches!(code, Some(c) if (0x30..=0x39).contains(c))
}

impl Parser<'_> {
    fn document(&mut self) -> Option<Val> {
        let start = self.skip_ws(0);
        let (value, end) = self.value(start)?;
        (self.skip_ws(end) == self.text.len()).then_some(value)
    }

    fn skip_ws(&self, mut index: usize) -> usize {
        while index < self.text.len() && is_ws(self.text[index]) {
            index += 1;
        }
        index
    }

    fn literal(&self, index: usize, word: &str) -> bool {
        word.chars()
            .enumerate()
            .all(|(offset, c)| self.text.get(index + offset) == Some(&(c as u32)))
    }

    fn value(&mut self, index: usize) -> Option<(Val, usize)> {
        let code = *self.text.get(index)?;
        match char::from_u32(code)? {
            '"' => {
                let (text, end) = self.string(index + 1)?;
                Some((Val::Str(text), end))
            }
            '{' => self.object(index + 1),
            '[' => self.array(index + 1),
            'n' if self.literal(index, "null") => Some((Val::Null, index + 4)),
            't' if self.literal(index, "true") => Some((Val::Bool(true), index + 4)),
            'f' if self.literal(index, "false") => Some((Val::Bool(false), index + 5)),
            'N' if self.literal(index, "NaN") => Some((Val::Float(f64::NAN), index + 3)),
            'I' if self.literal(index, "Infinity") => Some((Val::Float(f64::INFINITY), index + 8)),
            '-' if self.literal(index, "-Infinity") => {
                Some((Val::Float(f64::NEG_INFINITY), index + 9))
            }
            '-' | '0'..='9' => self.number(index),
            _ => None,
        }
    }

    fn number(&self, start: usize) -> Option<(Val, usize)> {
        let text = self.text;
        let mut index = start;
        if text.get(index) == Some(&('-' as u32)) {
            index += 1;
        }
        match text.get(index) {
            Some(&c) if ('1' as u32..='9' as u32).contains(&c) => {
                while is_digit(text.get(index)) {
                    index += 1;
                }
            }
            Some(&c) if c == '0' as u32 => index += 1,
            _ => return None,
        }
        let integer_end = index;
        let mut float = false;
        if text.get(index) == Some(&('.' as u32)) && is_digit(text.get(index + 1)) {
            index += 1;
            while is_digit(text.get(index)) {
                index += 1;
            }
            float = true;
        }
        if matches!(text.get(index), Some(&c) if c == 'e' as u32 || c == 'E' as u32) {
            let mut probe = index + 1;
            if matches!(text.get(probe), Some(&c) if c == '+' as u32 || c == '-' as u32) {
                probe += 1;
            }
            if is_digit(text.get(probe)) {
                while is_digit(text.get(probe)) {
                    probe += 1;
                }
                index = probe;
                float = true;
            }
        }
        let literal: String = text[start..index]
            .iter()
            .map(|c| char::from_u32(*c).unwrap_or('?'))
            .collect();
        if float {
            return Some((Val::Float(literal.parse().ok()?), index));
        }
        let digits = integer_end - start - usize::from(literal.starts_with('-'));
        if digits > MAX_INT_DIGITS {
            return None;
        }
        let normalized = if literal == "-0" { "0".into() } else { literal };
        Some((Val::Int(normalized), index))
    }

    fn hex4(&self, index: usize) -> Option<u32> {
        let mut value = 0;
        for offset in 0..4 {
            let digit = char::from_u32(*self.text.get(index + offset)?)?.to_digit(16)?;
            value = (value << 4) | digit;
        }
        Some(value)
    }

    /// `scanstring` with strict=True, starting after the opening quote.
    fn string(&self, mut index: usize) -> Option<(JsonStr, usize)> {
        let mut out = JsonStr::default();
        loop {
            let code = *self.text.get(index)?;
            index += 1;
            match code {
                0x22 => return Some((out, index)),
                0x5C => {
                    let escape = *self.text.get(index)?;
                    index += 1;
                    let decoded = match char::from_u32(escape)? {
                        '"' => 0x22,
                        '\\' => 0x5C,
                        '/' => 0x2F,
                        'b' => 0x08,
                        'f' => 0x0C,
                        'n' => 0x0A,
                        'r' => 0x0D,
                        't' => 0x09,
                        'u' => {
                            let mut value = self.hex4(index)?;
                            index += 4;
                            if (0xD800..0xDC00).contains(&value)
                                && index + 6 < self.text.len()
                                && self.text[index] == 0x5C
                                && self.text[index + 1] == 'u' as u32
                            {
                                let low = self.hex4(index + 2)?;
                                if (0xDC00..0xE000).contains(&low) {
                                    value = 0x10000 + ((value - 0xD800) << 10) + (low - 0xDC00);
                                    index += 6;
                                }
                            }
                            value
                        }
                        _ => return None,
                    };
                    out.push(decoded);
                }
                0..=0x1F => return None,
                _ => out.push(code),
            }
        }
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MAX_DEPTH).then_some(())
    }

    fn object(&mut self, index: usize) -> Option<(Val, usize)> {
        self.enter()?;
        let mut map = Dict::new();
        let mut index = self.skip_ws(index);
        if self.text.get(index) == Some(&('}' as u32)) {
            self.depth -= 1;
            return Some((Val::Dict(map), index + 1));
        }
        loop {
            if self.text.get(index) != Some(&('"' as u32)) {
                return None;
            }
            let (key, end) = self.string(index + 1)?;
            index = self.skip_ws(end);
            if self.text.get(index) != Some(&(':' as u32)) {
                return None;
            }
            index = self.skip_ws(index + 1);
            let (value, end) = self.value(index)?;
            map.insert(key, value);
            index = self.skip_ws(end);
            match self.text.get(index).copied() {
                Some(0x7D) => {
                    self.depth -= 1;
                    return Some((Val::Dict(map), index + 1));
                }
                Some(0x2C) => index = self.skip_ws(index + 1),
                _ => return None,
            }
        }
    }

    fn array(&mut self, index: usize) -> Option<(Val, usize)> {
        self.enter()?;
        let mut items = Vec::new();
        let mut index = self.skip_ws(index);
        if self.text.get(index) == Some(&(']' as u32)) {
            self.depth -= 1;
            return Some((Val::List(items), index + 1));
        }
        loop {
            let (value, end) = self.value(index)?;
            items.push(value);
            index = self.skip_ws(end);
            match self.text.get(index).copied() {
                Some(0x5D) => {
                    self.depth -= 1;
                    return Some((Val::List(items), index + 1));
                }
                Some(0x2C) => index = self.skip_ws(index + 1),
                _ => return None,
            }
        }
    }
}

// Encoding -------------------------------------------------------------------

/// How a value is encoded: sorted keys, compact separators, indentation.
#[derive(Clone, Copy)]
pub struct Style {
    pub sort_keys: bool,
    pub compact: bool,
    pub indent: Option<usize>,
}

/// `", "` and `": "` separators, keys in order.
pub const DEFAULT: Style = Style {
    sort_keys: false,
    compact: false,
    indent: None,
};
/// `", "` and `": "` separators, keys sorted.
pub const SORTED: Style = Style {
    sort_keys: true,
    compact: false,
    indent: None,
};
/// `,` and `:` separators, keys sorted.
pub const COMPACT_SORTED: Style = Style {
    sort_keys: true,
    compact: true,
    indent: None,
};
/// Two-space indent, keys sorted.
pub const INDENT_SORTED: Style = Style {
    sort_keys: true,
    compact: false,
    indent: Some(2),
};

pub fn dumps(value: &Val, style: Style) -> String {
    let mut out = String::new();
    write_value(&mut out, value, style, 0);
    out
}

fn newline(out: &mut String, style: Style, level: usize) {
    if let Some(width) = style.indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', width * level));
    }
}

fn separator(out: &mut String, style: Style) {
    out.push(',');
    if style.indent.is_none() && !style.compact {
        out.push(' ');
    }
}

fn write_value(out: &mut String, value: &Val, style: Style, level: usize) {
    match value {
        Val::Null => out.push_str("null"),
        Val::Bool(true) => out.push_str("true"),
        Val::Bool(false) => out.push_str("false"),
        Val::Int(digits) => out.push_str(digits),
        Val::Float(number) => out.push_str(&float_repr(*number)),
        Val::Str(text) => write_str(out, text),
        Val::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    separator(out, style);
                }
                newline(out, style, level + 1);
                write_value(out, item, style, level + 1);
            }
            newline(out, style, level);
            out.push(']');
        }
        Val::Dict(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut entries: Vec<(&JsonStr, &Val)> = map.iter().collect();
            if style.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    separator(out, style);
                }
                newline(out, style, level + 1);
                write_str(out, key);
                out.push_str(if style.compact { ":" } else { ": " });
                write_value(out, item, style, level + 1);
            }
            newline(out, style, level);
            out.push('}');
        }
    }
}

/// A quoted string, ASCII only: `"`, `\\`, and the five control characters
/// with short forms use them, and every other character outside printable
/// ASCII is `\\uXXXX` (astral characters as surrogate pairs, lone surrogates
/// as themselves).
fn write_str(out: &mut String, text: &JsonStr) {
    out.push('"');
    for code in text.code_points() {
        match code {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x20..=0x7E => out.push(code as u8 as char),
            0x10000.. => {
                let value = code - 0x10000;
                let _ = write!(
                    out,
                    "\\u{:04x}\\u{:04x}",
                    0xD800 + (value >> 10),
                    0xDC00 + (value & 0x3FF)
                );
            }
            _ => {
                let _ = write!(out, "\\u{code:04x}");
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(raw: &[u8]) -> Option<String> {
        loads(raw).map(|value| dumps(&value, DEFAULT))
    }

    #[test]
    fn decodes_like_json_loads() {
        assert_eq!(
            round(br#"{"a": [1, 2.5, "x\u00e9\ud83d\ude00"], "b": null, "a": 3}"#).unwrap(),
            r#"{"a": 3, "b": null}"#
        );
        // Lone surrogates parse, including a high one followed by a non-low escape.
        assert_eq!(round(br#""\ud800""#).unwrap(), r#""\ud800""#);
        assert_eq!(
            round(br#""\ud800\u0041\udc00""#).unwrap(),
            r#""\ud800A\udc00""#
        );
        assert_eq!(
            round(b"[-0, 1E2, 1.50, NaN, -Infinity]").unwrap(),
            "[0, 100.0, 1.5, NaN, -Infinity]"
        );
        for bad in [
            &b"not json"[..],
            b"{\"sessions\": [}",
            b"\xff\xfe{}",
            b"",
            b"{} trailing",
            b"01",
            b"1.",
            b"[1,]",
            b"{\"a\":1,}",
            b"\"\x01\"",
            b"\"\\x\"",
            b"\"\\ud800\\uzzzz\"",
            b"\xc0\x80",
        ] {
            assert_eq!(round(bad), None, "{bad:?}");
        }
        // Raw surrogates in UTF-8 pass.
        assert_eq!(round(b"\"\xed\xa0\x80\"").unwrap(), r#""\ud800""#);
        // UTF-8 BOM, UTF-16, and UTF-32 bodies.
        assert_eq!(round(b"\xef\xbb\xbf[1]").unwrap(), "[1]");
        assert_eq!(round(b"\xff\xfe[\x001\x00]\x00").unwrap(), "[1]");
        assert_eq!(round(b"\x00[\x001\x00]").unwrap(), "[1]");
        assert_eq!(round(b"[\x00\x00\x00]\x00\x00\x00").unwrap(), "[]");
        assert_eq!(loads_str("\u{feff}[]"), None);
        assert_eq!(round(&[b'1'; 4301]), None);
        assert!(round(&[b'1'; 4300]).is_some());
        // Server threads have 8 MiB stacks; the deepest accepted body fits.
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                let deep = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
                assert!(round(deep.as_bytes()).is_some());
                let deeper = format!("[{deep}]");
                assert_eq!(round(deeper.as_bytes()), None);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn encodes_like_json_dumps() {
        let value = loads(
            "{\"b\": [1, {}], \"a\": \"h\u{e9}\u{2028}\u{1f600}\\u007f\", \"c\": []}".as_bytes(),
        )
        .unwrap();
        assert_eq!(
            dumps(&value, DEFAULT),
            r#"{"b": [1, {}], "a": "h\u00e9\u2028\ud83d\ude00\u007f", "c": []}"#
        );
        assert_eq!(
            dumps(&value, COMPACT_SORTED),
            r#"{"a":"h\u00e9\u2028\ud83d\ude00\u007f","b":[1,{}],"c":[]}"#
        );
        assert_eq!(
            dumps(&value, INDENT_SORTED),
            "{\n  \"a\": \"h\\u00e9\\u2028\\ud83d\\ude00\\u007f\",\n  \"b\": [\n    1,\n    {}\n  ],\n  \"c\": []\n}"
        );
    }

    #[test]
    fn strings_count_code_points() {
        let value = loads(br#""a\u00e9\ud800\ud83d\ude00""#).unwrap();
        let text = value.as_json_str().unwrap();
        assert_eq!(text.char_len(), 4);
        assert_eq!(text.as_str(), None);
        assert_eq!(
            text.code_points().collect::<Vec<_>>(),
            vec![0x61, 0xE9, 0xD800, 0x1F600]
        );
    }
}
