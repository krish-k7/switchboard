//! The command line's visual language, shared by `init` and the human output
//! of every command.
//!
//! Every line starts with a two-cell mark column, then the text: a check mark
//! for something finished, a cross for a failure, `!` for a warning, `?` for
//! the question being asked, and blanks for everything else, so text always
//! starts in the same column. A screen is grouped by blank lines: what was
//! chosen (aligned `label  value` rows), what was done (marked steps), and
//! what to do next, each group under a short bold heading when it needs a
//! name ("Next", "To fix"). Next actions are numbered in the order they are
//! done. A command to run, or the invite, sits alone on its own line, four
//! cells in from the text it belongs to, so it is easy to find and copy.
//! Prose wraps at `MEASURE` columns, even on a wider terminal.
//!
//! Text the reader needs is the terminal's own foreground, bold or normal,
//! which reads on light and dark themes alike; dim is only for the version,
//! key hints, and placeholders. One accent color (cyan) is used on marks
//! only: check marks, the question's mark, the pointer, and the spinner.
//! Red and yellow appear only on the marks of failures and warnings. Without
//! color (`NO_COLOR`, `TERM=dumb`, or not a terminal) the layout alone
//! carries the hierarchy; without a UTF-8 locale the marks fall back to
//! ASCII.
//!
//! Text passed to the line helpers may quote commands in backticks:
//! `` Run `switchboard doctor` `` shows the command in bold, drops the
//! backticks, and never breaks it across lines when it fits on one. A line
//! break (`\n`) starts a new paragraph, an empty line is a blank line, and a
//! paragraph that is only one quoted command is set on lines of its own at
//! the copy indent (`layout`).

pub mod prompt;
pub mod term;

use super::format::{display_width, text};

/// Cells before the text on every line: the mark and a space.
pub const INDENT: usize = 2;
/// How much further in than its text a command to copy sits.
pub const COPY: usize = 4;
/// The same inside a numbered step, whose text is already three cells in
/// past its number: every command in a step starts in one column, and the
/// install command still fits beside it on an 80-column terminal.
pub const STEP_COPY: usize = 2;
/// The column prose wraps before, even on a wider terminal, so lines stay
/// readable.
pub const MEASURE: usize = 78;

/// What a line's mark says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Finished.
    Done,
    /// Failed.
    Failed,
    /// Finished with a warning, or something to look at.
    Warned,
    /// The question being asked.
    Ask,
    /// No mark: aligned with the marked text.
    Blank,
}

/// Colors and symbols for one output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    pub unicode: bool,
}

/// Whether the locale says UTF-8 (`LC_ALL`, else `LC_CTYPE`, else `LANG`);
/// with none of them set, modern terminals are assumed to be UTF-8.
pub fn unicode_locale() -> bool {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                let value = value.to_ascii_lowercase();
                return value.contains("utf-8") || value.contains("utf8");
            }
        }
    }
    true
}

impl Theme {
    /// The theme for a standard stream: color on a terminal unless
    /// `NO_COLOR` is set or `TERM` is `dumb`.
    pub fn for_fd(fd: i32) -> Theme {
        Theme {
            color: super::format::terminal_color(fd),
            unicode: unicode_locale(),
        }
    }

    /// No color, Unicode marks: what tests compare.
    pub fn plain() -> Theme {
        Theme {
            color: false,
            unicode: true,
        }
    }

    fn paint(&self, code: &str, value: &str) -> String {
        if self.color && !value.is_empty() {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.to_string()
        }
    }

    /// The one accent color: what to act on.
    pub fn accent(&self, value: &str) -> String {
        self.paint("36", value)
    }

    /// The accent, bold: the question being asked.
    pub fn accent_strong(&self, value: &str) -> String {
        self.paint("1;36", value)
    }

    pub fn strong(&self, value: &str) -> String {
        self.paint("1", value)
    }

    /// A command to run, or other text to copy: bold, default foreground.
    pub fn command(&self, value: &str) -> String {
        self.paint("1", value)
    }

    /// Finished or secondary text.
    pub fn muted(&self, value: &str) -> String {
        self.paint("2", value)
    }

    pub fn bad(&self, value: &str) -> String {
        self.paint("31", value)
    }

    pub fn caution(&self, value: &str) -> String {
        self.paint("33", value)
    }

    /// The symbol for a mark, without color.
    pub fn symbol(&self, mark: Mark) -> &'static str {
        match (mark, self.unicode) {
            (Mark::Done, true) => "✓",
            (Mark::Done, false) => "+",
            (Mark::Failed, true) => "✗",
            (Mark::Failed, false) => "x",
            (Mark::Warned, _) => "!",
            (Mark::Ask, _) => "?",
            (Mark::Blank, _) => " ",
        }
    }

    /// The mark column (mark and space), colored.
    pub fn mark(&self, mark: Mark) -> String {
        let symbol = self.symbol(mark);
        let painted = match mark {
            Mark::Done => self.accent(symbol),
            Mark::Ask => self.accent_strong(symbol),
            Mark::Failed => self.paint("1;31", symbol),
            Mark::Warned => self.paint("1;33", symbol),
            Mark::Blank => symbol.to_string(),
        };
        format!("{painted} ")
    }

    /// The pointer before the highlighted choice.
    pub fn pointer(&self) -> &'static str {
        if self.unicode {
            "›"
        } else {
            ">"
        }
    }

    /// Spinner frames.
    pub fn spinner(&self) -> &'static [&'static str] {
        if self.unicode {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        } else {
            &["|", "/", "-", "\\"]
        }
    }
}

/// How a run of text is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Strong,
    Muted,
    Accent,
}

impl Theme {
    pub fn tone(&self, tone: Tone, value: &str) -> String {
        match tone {
            Tone::Normal => value.to_string(),
            Tone::Strong => self.strong(value),
            Tone::Muted => self.muted(value),
            Tone::Accent => self.accent(value),
        }
    }
}

/// A word: runs of text and whether each was inside backticks (a word can
/// have both, as in `` `switchboard init`. ``). A quoted run is one word,
/// spaces and all, so a command is never broken across lines when it fits
/// on one.
type Word = Vec<(String, bool)>;

fn words(value: &str) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    // Whether the next run continues the last word (no space between).
    let mut glued = false;
    for (index, part) in value.split('`').enumerate() {
        let quoted = index % 2 == 1;
        let cleaned = text(part);
        let starts_spaced = part.starts_with(crate::text::is_space);
        let ends_spaced = part.ends_with(crate::text::is_space);
        let pieces: Vec<String> = if quoted {
            if cleaned.is_empty() {
                Vec::new()
            } else {
                vec![cleaned.clone()]
            }
        } else {
            super::format::split(&cleaned)
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        for (n, piece) in pieces.into_iter().enumerate() {
            if n == 0 && glued && !starts_spaced && !out.is_empty() {
                out.last_mut().expect("a word").push((piece, quoted));
            } else {
                out.push(vec![(piece, quoted)]);
            }
        }
        if !cleaned.is_empty() {
            glued = !ends_spaced;
        } else if starts_spaced {
            glued = false;
        }
    }
    out
}

/// A word whose quoted runs are broken at their spaces, for a command too
/// long for a line of its own.
fn expand(word: Word) -> Vec<Word> {
    let mut out: Vec<Word> = vec![Vec::new()];
    for (run, quoted) in word {
        if quoted && run.contains(' ') {
            for (n, token) in run.split(' ').enumerate() {
                if n > 0 {
                    out.push(Vec::new());
                }
                out.last_mut()
                    .expect("a word")
                    .push((token.to_string(), true));
            }
        } else {
            out.last_mut().expect("a word").push((run, quoted));
        }
    }
    out.retain(|word| !word.is_empty());
    out
}

fn word_width(word: &Word) -> usize {
    word.iter().map(|(t, _)| display_width(t)).sum()
}

/// Wrap `value` (with backtick-quoted commands) into lines of at most
/// `width` cells, each drawn in `tone` with commands in bold.
pub fn wrap_rich(theme: &Theme, value: &str, width: usize, tone: Tone) -> Vec<String> {
    let width = width.max(8);
    let mut lines: Vec<Vec<Word>> = vec![Vec::new()];
    let mut used = 0usize;
    let mut queue: std::collections::VecDeque<Word> = words(value).into();
    while let Some(word) = queue.pop_front() {
        let cells = word_width(&word);
        if cells > width && word.iter().any(|(t, q)| *q && t.contains(' ')) {
            for part in expand(word).into_iter().rev() {
                queue.push_front(part);
            }
            continue;
        }
        if !lines.last().expect("a line").is_empty() && used + 1 + cells > width {
            lines.push(Vec::new());
            used = 0;
        }
        if cells > width {
            // A word wider than the line is split by cells.
            for (run, quoted) in word {
                for c in run.chars() {
                    let w = display_width(&c.to_string());
                    if used + w > width && used > 0 {
                        lines.push(Vec::new());
                        used = 0;
                    }
                    let line = lines.last_mut().expect("a line");
                    match line.last_mut() {
                        Some(last) if used > 0 => match last.last_mut() {
                            Some((text, q)) if *q == quoted => text.push(c),
                            _ => last.push((c.to_string(), quoted)),
                        },
                        _ => line.push(vec![(c.to_string(), quoted)]),
                    }
                    used += w;
                }
            }
            continue;
        }
        let line = lines.last_mut().expect("a line");
        if !line.is_empty() {
            used += 1;
        }
        line.push(word);
        used += cells;
    }
    lines
        .into_iter()
        .map(|line| {
            // Merge runs of the same style, spaces included, then paint.
            let mut runs: Vec<(String, bool)> = Vec::new();
            for (index, word) in line.into_iter().enumerate() {
                for (n, (text, quoted)) in word.into_iter().enumerate() {
                    let spaced = index > 0 && n == 0;
                    match runs.last_mut() {
                        Some((last, q)) if *q == quoted => {
                            if spaced {
                                last.push(' ');
                            }
                            last.push_str(&text);
                        }
                        _ => {
                            if spaced {
                                // The space goes with the plain side.
                                match runs.last_mut() {
                                    Some((last, false)) => last.push(' '),
                                    _ => runs.push((" ".into(), false)),
                                }
                            }
                            runs.push((text, quoted));
                        }
                    }
                }
            }
            runs.into_iter()
                .map(|(text, quoted)| {
                    if text == " " {
                        text
                    } else if quoted {
                        theme.command(&text)
                    } else {
                        // A plain run's edge spaces stay unpainted.
                        let trimmed = text.trim_end_matches(' ');
                        let tail = &text[trimmed.len()..];
                        format!("{}{tail}", theme.tone(tone, trimmed))
                    }
                })
                .collect::<String>()
        })
        .collect()
}

/// `value` as one line of safe text (no controls, single spaces).
pub fn format_text(value: &str) -> String {
    text(value)
}

/// `value` cut to `width` cells, ending in an ellipsis when cut.
pub fn truncate(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_string();
    }
    let mut out = String::new();
    for c in value.chars() {
        if display_width(&out) + display_width(&c.to_string()) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// The cells `value` takes on screen, ignoring color codes.
pub fn visible_width(value: &str) -> usize {
    display_width(&strip_codes(value))
}

/// `value` without terminal color codes.
pub fn strip_codes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() || d == '~' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The text width for a terminal `columns` wide: everything after the mark
/// column, up to one cell short of the edge (a full row would wrap early on
/// some terminals) or `MEASURE`, whichever comes first.
pub fn text_width(columns: usize) -> usize {
    width_from(INDENT, columns)
}

/// The width left for text that starts `hang` cells in.
fn width_from(hang: usize, columns: usize) -> usize {
    columns
        .saturating_sub(1)
        .min(MEASURE)
        .saturating_sub(hang)
        .max(20)
}

/// One marked item: the mark, then `value` laid out under the text column
/// (see `layout`).
pub fn item(theme: &Theme, mark: Mark, value: &str, tone: Tone, columns: usize) -> Vec<String> {
    layout(theme, &theme.mark(mark), INDENT, value, tone, columns)
}

/// A finished step: the check mark and what was done.
pub fn done(theme: &Theme, value: &str, columns: usize) -> Vec<String> {
    item(theme, Mark::Done, value, Tone::Normal, columns)
}

/// Unmarked text in the text column.
pub fn para(theme: &Theme, value: &str, tone: Tone, columns: usize) -> Vec<String> {
    item(theme, Mark::Blank, value, tone, columns)
}

/// A short bold heading that names the group under it ("Next", "To fix").
pub fn heading(theme: &Theme, value: &str, columns: usize) -> Vec<String> {
    para(theme, value, Tone::Strong, columns)
}

/// Whether a paragraph is one quoted command and nothing else.
fn lone_command(paragraph: &str) -> Option<&str> {
    let inner = paragraph.trim().strip_prefix('`')?.strip_suffix('`')?;
    (!inner.is_empty() && !inner.contains('`')).then_some(inner)
}

/// `value` under a text column `hang` cells in, the first line starting with
/// `first` (a mark or a step number, `hang` cells wide). Each line of
/// `value` is a paragraph, wrapped at the measure; an empty one is a blank
/// line, one that is only a quoted command is set on lines of its own,
/// `COPY` cells further in (`command_at`), and one that starts with `> ` (a
/// message quoted from elsewhere) is wrapped at that indent too.
pub fn layout(
    theme: &Theme,
    first: &str,
    hang: usize,
    value: &str,
    tone: Tone,
    columns: usize,
) -> Vec<String> {
    layout_with(theme, first, hang, COPY, value, tone, columns)
}

/// `layout`, with commands and quotes `copy` cells in from the text.
fn layout_with(
    theme: &Theme,
    first: &str,
    hang: usize,
    copy: usize,
    value: &str,
    tone: Tone,
    columns: usize,
) -> Vec<String> {
    let pad = " ".repeat(hang);
    let mut lines: Vec<String> = Vec::new();
    for paragraph in value.split('\n') {
        let start = lines.len();
        if let Some(command) = lone_command(paragraph) {
            lines.extend(command_at(theme, command, hang + copy, columns));
        } else if let Some(quote) = paragraph.strip_prefix("> ") {
            let inset = " ".repeat(hang + copy);
            let wrapped = wrap_rich(theme, quote, width_from(hang + copy, columns), tone);
            lines.extend(wrapped.into_iter().map(|line| format!("{inset}{line}")));
        } else if paragraph.trim().is_empty() && value.contains('\n') {
            lines.push(String::new());
            continue;
        } else {
            let wrapped = wrap_rich(theme, paragraph, width_from(hang, columns), tone);
            lines.extend(wrapped.into_iter().map(|line| format!("{pad}{line}")));
        }
        if start == 0 {
            // The first line carries the mark (a command keeps its indent).
            if let Some(line) = lines.first_mut() {
                if let Some(rest) = line.strip_prefix(pad.as_str()) {
                    *line = format!("{first}{rest}");
                } else if visible_width(first.trim_end()) > 0 {
                    lines.insert(0, first.trim_end().to_string());
                }
            }
        }
    }
    lines
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Steps to do, in order: numbered when there is more than one, each with
/// its text under its number and its commands on lines of their own, all
/// `STEP_COPY` cells in from the step's text.
pub fn steps(theme: &Theme, steps: &[String], columns: usize) -> Vec<String> {
    if steps.len() == 1 {
        return para(theme, &steps[0], Tone::Normal, columns);
    }
    let mut lines = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let number = format!("{}", index + 1);
        let first = format!(
            "{}{}{}",
            " ".repeat(INDENT),
            theme.strong(&number),
            " ".repeat(3usize.saturating_sub(number.len()))
        );
        lines.extend(layout_with(
            theme,
            &first,
            INDENT + 3,
            STEP_COPY,
            step,
            Tone::Normal,
            columns,
        ));
    }
    lines
}

/// Whether a quoted run is a command to run (not a file, a flag, or a
/// value), so it can stand on a line of its own.
fn runnable(command: &str) -> bool {
    let first = command.split(' ').next().unwrap_or("");
    matches!(
        first,
        "switchboard"
            | "~/bin/switchboard"
            | "sudo"
            | "journalctl"
            | "claude"
            | "printf"
            | "curl"
            | "export"
            | "/switchboard"
            | "/reload-plugins"
    ) || (first.starts_with('!') && first.len() > 1)
}

/// `text` with its first letter in uppercase.
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// One step of a fix as laid-out text: a command it ends with (`run
/// `switchboard init` again.`) moves to a line of its own, and what follows
/// a semicolon after it becomes a sentence under it.
fn step_text(step: &str) -> String {
    let step = step.trim();
    let mut step = if step.starts_with('`') {
        format!("run {step}")
    } else {
        step.to_string()
    };
    if !step.ends_with(['.', '?', '!', ':']) && !step.ends_with('`') {
        step.push('.');
    }
    let step = capitalized(&step);
    // The last quoted run, and what comes after it.
    let Some(close) = step.rfind('`') else {
        return step;
    };
    let Some(open) = step[..close].rfind('`') else {
        return step;
    };
    let command = &step[open + 1..close];
    let before = step[..open].trim_end();
    let mut after = step[close + 1..].trim_start();
    if let Some(rest) = after.strip_prefix("again") {
        after = rest;
    }
    let remark = match after.chars().next() {
        None => "",
        Some('.') | Some(';') => after[1..].trim(),
        Some(_) => return step,
    };
    if !runnable(command) || before.is_empty() || before.ends_with('(') {
        return step;
    }
    let mut text = format!("{before}\n`{command}`");
    if !remark.is_empty() {
        text.push('\n');
        text.push_str(&capitalized(remark));
    }
    text
}

/// The steps of a fix written as one sentence: the parts joined by
/// `, then` (or `; then`) are done in that order.
pub fn fix_steps(fix: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = fix.trim();
    loop {
        let at = [", then ", "; then "]
            .iter()
            .filter_map(|sep| rest.find(sep).map(|i| (i, sep.len())))
            .min();
        // A `then` inside a quoted command is not a step.
        match at {
            Some((index, len)) if rest[..index].matches('`').count().is_multiple_of(2) => {
                parts.push(rest[..index].to_string());
                rest = &rest[index + len..];
            }
            _ => {
                parts.push(rest.to_string());
                break;
            }
        }
    }
    parts.iter().map(|part| step_text(part)).collect()
}

/// How to fix something, under what went wrong. One step reads as a
/// sentence (`To fix, run` and the command); several are numbered under a
/// `To fix` heading, set apart by a blank line.
pub fn fix_lines(theme: &Theme, steps: &[String], columns: usize) -> Vec<String> {
    match steps {
        [] => Vec::new(),
        [only] => {
            let mut chars = only.chars();
            let lowered = match chars.next() {
                Some(first) if !only.starts_with("Switchboard") && !only.starts_with("Claude") => {
                    format!("{}{}", first.to_lowercase(), chars.as_str())
                }
                _ => only.clone(),
            };
            para(theme, &format!("To fix, {lowered}"), Tone::Normal, columns)
        }
        many => {
            let mut lines = vec![String::new()];
            lines.extend(heading(theme, "To fix", columns));
            lines.extend(self::steps(theme, many, columns));
            lines
        }
    }
}

/// A command to copy, on lines of its own, `COPY` cells in from the text.
pub fn command(theme: &Theme, value: &str, columns: usize) -> Vec<String> {
    command_at(theme, value, INDENT + COPY, columns)
}

/// A command to copy, `indent` cells in. One too long for the terminal is
/// broken between words with shell line continuations, every line at the
/// same indent, so it still pastes and runs as one command and its lines
/// stay in its column. When even that cannot fit (a long address on a
/// narrow terminal), it is printed whole at the left edge, so the terminal
/// wraps it and it still copies as one line.
pub fn command_at(theme: &Theme, value: &str, indent: usize, columns: usize) -> Vec<String> {
    let room = columns.saturating_sub(1);
    if indent + display_width(value) <= room {
        return vec![format!("{}{}", " ".repeat(indent), theme.command(value))];
    }
    // A pipe stays with the command after it.
    let words: Vec<String> = value
        .replace("| ", "|\u{0}")
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(|w| w.replace('\u{0}', " "))
        .collect();
    if words
        .iter()
        .any(|word| indent + display_width(word) + 2 > room)
    {
        return vec![theme.command(value)];
    }
    let lead = " ".repeat(indent);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in &words {
        let candidate = if line.is_empty() {
            word.clone()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || indent + display_width(&candidate) + 2 <= room {
            line = candidate;
        } else {
            lines.push(format!("{lead}{} \\", theme.command(&line)));
            line = word.clone();
        }
    }
    lines.push(format!("{lead}{}", theme.command(&line)));
    lines
}

/// The invite (or another long token to copy) in pieces that end at the
/// measure, at the copy indent, so it reads as one block; line breaks in it
/// are fine where it is pasted. `wrap: false` keeps it on one line, for
/// scripts that read it.
pub fn token(theme: &Theme, value: &str, wrap: bool, columns: usize) -> Vec<String> {
    let indent = INDENT + COPY;
    if !wrap {
        return vec![format!("{}{}", " ".repeat(indent), theme.command(value))];
    }
    let width = width_from(indent, columns).max(16);
    let chars: Vec<char> = value.chars().collect();
    chars
        .chunks(width)
        .map(|chunk| {
            let piece: String = chunk.iter().collect();
            format!("{}{}", " ".repeat(indent), theme.command(&piece))
        })
        .collect()
}

/// A summary like `3 passed, 1 warning, 0 failed`.
pub fn count(number: usize, one: &str, many: &str) -> String {
    format!("{number} {}", if number == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_text_wraps_by_cells_and_colors_commands() {
        let plain = Theme::plain();
        assert_eq!(
            wrap_rich(
                &plain,
                "Run `switchboard doctor` to check.",
                12,
                Tone::Normal
            ),
            ["Run", "switchboard", "doctor to", "check."]
        );
        assert_eq!(
            wrap_rich(
                &plain,
                "with `switchboard server invite`.",
                30,
                Tone::Normal
            ),
            ["with", "switchboard server invite."]
        );
        let color = Theme {
            color: true,
            unicode: true,
        };
        assert_eq!(
            wrap_rich(&color, "Run `switchboard doctor` now", 80, Tone::Muted),
            ["\x1b[2mRun\x1b[0m \x1b[1mswitchboard doctor\x1b[0m \x1b[2mnow\x1b[0m"]
        );
        assert_eq!(
            wrap_rich(&plain, "abcdefghijklmnop", 8, Tone::Normal),
            ["abcdefgh", "ijklmnop"]
        );
        assert_eq!(wrap_rich(&plain, "", 8, Tone::Normal), [""]);
    }

    #[test]
    fn items_hang_under_the_text_column() {
        let plain = Theme::plain();
        let lines = item(
            &plain,
            Mark::Done,
            "Created this machine's key in the private configuration directory",
            Tone::Muted,
            40,
        );
        assert_eq!(
            lines,
            [
                "✓ Created this machine's key in the",
                "  private configuration directory"
            ]
        );
        for line in &lines {
            assert!(visible_width(line) < 40);
        }
        let ascii = Theme {
            color: false,
            unicode: false,
        };
        assert_eq!(item(&ascii, Mark::Failed, "x", Tone::Normal, 80), ["x x"]);
    }

    #[test]
    fn widths_ignore_color_codes() {
        assert_eq!(visible_width("\x1b[1;36m?\x1b[0m ab"), 4);
        assert_eq!(strip_codes("\x1b[?25la\x1b[2Kb"), "ab");
        assert_eq!(text_width(60), 57);
        assert_eq!(text_width(80), 76);
        assert_eq!(text_width(200), MEASURE - INDENT);
        assert_eq!(text_width(10), 20);
    }

    #[test]
    fn long_commands_continue_on_lines_of_their_own() {
        let plain = Theme::plain();
        let install =
            "curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh";
        assert_eq!(command(&plain, install, 120), [format!("      {install}")]);
        // Every line of a broken command stays in its column.
        assert_eq!(
            command(&plain, install, 80),
            [
                "      curl -fsSL \\",
                "      https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh \\",
                "      | sh"
            ]
        );
        // Inside a numbered step, at the step's command column.
        assert_eq!(
            command_at(&plain, install, INDENT + 3 + STEP_COPY, 80),
            [
                "       curl -fsSL \\",
                "       https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh \\",
                "       | sh"
            ]
        );
        assert_eq!(
            steps(
                &plain,
                &[
                    format!("Run\n`{install}`"),
                    "Then\n`switchboard init`".into()
                ],
                80
            )
            .iter()
            .filter(|l| l.contains("curl")
                || l.contains("https")
                || l.contains("| sh")
                || l.contains("init"))
            .map(|l| l.len() - l.trim_start().len())
            .collect::<Vec<_>>(),
            [7, 7, 7, 7]
        );
        assert_eq!(
            command(&plain, "switchboard start", 60),
            ["      switchboard start"]
        );
        // The address cannot fit 60 columns even on a line of its own.
        assert_eq!(command(&plain, install, 60), [install]);
    }

    #[test]
    fn paragraphs_commands_and_quotes_lay_out_under_the_text() {
        let plain = Theme::plain();
        assert_eq!(
            para(
                &plain,
                "Get one from the server. They make it with\n`sudo switchboard server invite`\n\nNo server yet?\n> quoted words",
                Tone::Normal,
                80
            ),
            [
                "  Get one from the server. They make it with",
                "      sudo switchboard server invite",
                "",
                "  No server yet?",
                "      quoted words"
            ]
        );
        // A mark stays in the gutter of a command that comes first.
        assert_eq!(
            item(
                &plain,
                Mark::Warned,
                "`switchboard start`",
                Tone::Normal,
                80
            ),
            ["!     switchboard start"]
        );
    }

    #[test]
    fn steps_are_numbered_only_when_there_are_several() {
        let plain = Theme::plain();
        assert_eq!(
            steps(&plain, &["Run\n`switchboard init`".into()], 80),
            ["  Run", "      switchboard init"]
        );
        assert_eq!(
            steps(
                &plain,
                &["Fix the cause.".into(), "Run\n`switchboard init`".into()],
                80
            ),
            ["  1  Fix the cause.", "  2  Run", "       switchboard init"]
        );
    }

    #[test]
    fn fixes_split_into_steps_with_their_commands_alone() {
        assert_eq!(
            fix_steps("run `switchboard start`."),
            ["Run\n`switchboard start`"]
        );
        assert_eq!(
            fix_steps("check the network, then run `switchboard init` again; it reuses this machine's key."),
            [
                "Check the network.",
                "Run\n`switchboard init`\nIt reuses this machine's key."
            ]
        );
        // A command in the middle of a sentence stays in it.
        assert_eq!(
            fix_steps("run `switchboard doctor` to see why, then `switchboard start`."),
            [
                "Run `switchboard doctor` to see why.",
                "Run\n`switchboard start`"
            ]
        );
        // Files and flags are not commands to run.
        assert_eq!(
            fix_steps("choose another name with `--name`."),
            ["Choose another name with `--name`."]
        );
        let plain = Theme::plain();
        assert_eq!(
            fix_lines(&plain, &fix_steps("run `switchboard start`."), 80),
            ["  To fix, run", "      switchboard start"]
        );
        assert_eq!(
            fix_lines(
                &plain,
                &fix_steps("fix that, then run `switchboard init` again."),
                80
            ),
            [
                "",
                "  To fix",
                "  1  Fix that.",
                "  2  Run",
                "       switchboard init"
            ]
        );
    }

    #[test]
    fn commands_in_prose_stay_whole() {
        let plain = Theme::plain();
        assert_eq!(
            wrap_rich(
                &plain,
                "To connect a chat, run `/switchboard join` in it. From a terminal: `switchboard join --chat ID`",
                40,
                Tone::Normal
            ),
            [
                "To connect a chat, run /switchboard join",
                "in it. From a terminal:",
                "switchboard join --chat ID"
            ]
        );
    }
}
