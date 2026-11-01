//! String predicates and formatting for observable text.
//!
//! Notices, error details, and file names are built from Unicode character
//! classes (printable, whitespace), whitespace splitting and stripping,
//! lengths and truncation in code points, and quoted string literals. These
//! rules are fixed because the exact text is part of what clients and tests
//! see.

use crate::unicode_tables::{NAME_REMOVED, NOT_PRINTABLE, WHITESPACE};

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

/// Printable: not in categories Cc, Cf, Cs, Co, Cn, Zl, Zp, or Zs, except
/// that U+0020 (space) is printable.
pub fn is_printable(c: char) -> bool {
    !in_table(NOT_PRINTABLE, c)
}

/// Whitespace: category Zs or bidirectional class WS, B, or S (this includes
/// U+001C to U+001F and U+0085, but not U+200B).
pub fn is_space(c: char) -> bool {
    in_table(WHITESPACE, c)
}

/// Unicode categories Cf, Cc, Cs, Zl, or Zp.
pub fn is_name_removed(c: char) -> bool {
    in_table(NAME_REMOVED, c)
}

/// The first `limit` code points (not bytes).
pub fn take_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// The length in code points.
pub fn char_len(value: &str) -> usize {
    value.chars().count()
}

/// Split on runs of whitespace ([`is_space`]), drop empty parts, and join
/// with single spaces.
pub fn collapse_whitespace(value: &str) -> String {
    value
        .split(is_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Trim whitespace ([`is_space`]) from both ends.
pub fn strip(value: &str) -> &str {
    value.trim_matches(is_space)
}

/// A quoted string literal: single quotes, or double quotes when the text
/// has a `'` and no `"`; backslash, newline, carriage return, tab, and the
/// quote character escaped; other non-printable characters as `\xHH`,
/// `\uHHHH`, or `\UHHHHHHHH`.
pub fn repr_str(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_printable(c) => out.push(c),
            c if (c as u32) < 0x100 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_str_quotes_and_escapes() {
        assert_eq!(repr_str("/tmp/a"), "'/tmp/a'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(
            repr_str("\u{7f}\u{200b}λ\u{e000}\u{10ffff}"),
            "'\\x7f\\u200bλ\\ue000\\U0010ffff'"
        );
    }

    #[test]
    fn whitespace_includes_unicode_separators() {
        assert_eq!(collapse_whitespace(" a\u{3000}b\x1c c\u{85} "), "a b c");
        assert_eq!(strip("\u{a0} x \u{2028}"), "x");
        assert!(!is_space('\u{200b}'));
    }
}
