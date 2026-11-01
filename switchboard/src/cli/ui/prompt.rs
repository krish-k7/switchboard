//! Prompts and steps, drawn in the visual language of `ui`.
//!
//! One `Ui` serves every kind of run:
//! - `Terminal`: an interactive terminal. Prompts redraw in place (only the
//!   lines they own), steps show a spinner, and each settles into one line.
//! - `Basic`: a terminal that cannot move the cursor (`TERM=dumb`): the same
//!   questions as numbered choices and typed lines.
//! - `Lines`: no terminal. Nothing is asked: a question without an answer
//!   from the command line stops with the flag that answers it. Steps print
//!   their settled lines.
//! - `Silent`: `--json`. As `Lines`, but nothing is printed.
//! - `Script`: tests. Answers come from a list and the settled lines are
//!   kept as a transcript.
//!
//! Answers settle into aligned `label  value` lines (`Ui::label_width`). A
//! text prompt shows its default as a dim placeholder: typing replaces it,
//! Enter accepts it, and Tab (or the right arrow) copies it in to edit. Esc
//! goes back to the question before, where the flow allows it (`Ui::back`):
//! the flow rewinds the screen to a checkpoint and asks again.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::term::{self, Key};
use super::{item, para, text_width, visible_width, wrap_rich, Mark, Theme, Tone, INDENT};
use crate::cli::format::{display_width, padded};

/// What a prompt says when a paste was over `term::PASTE_LIMIT` and dropped.
const PASTE_TOO_LARGE: &str =
    "That paste was too large (over 64 KiB), so it was ignored. Paste only the text asked for.";

/// Why a prompt did not return an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Ctrl-C or Ctrl-D at a prompt.
    Cancelled,
    /// Esc, where the flow allows going back (`Ui::back`): ask the
    /// question before this one again.
    Back,
    /// No terminal to ask on; the flag (or variable) that answers it.
    Missing(String),
}

/// Something that went wrong: what, in plain words, and the exact fix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Problem {
    /// One line: what went wrong.
    pub title: String,
    /// More about why, when it helps.
    pub detail: Option<String>,
    /// What to do about it (commands in backticks).
    pub fix: Option<String>,
    /// A short machine-readable reason, for flows that recover from some.
    pub code: Option<&'static str>,
    /// How people see the detail and the fix, where that differs from the
    /// sentences `--json` gives (boxed: most problems don't have it).
    pub shown: Option<Box<Shown>>,
}

/// A problem's detail and fix laid out for people.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    /// The detail as paragraphs and quotes (see `ui::layout`).
    pub detail: Option<String>,
    /// The fix as steps in the order they are done, commands on lines of
    /// their own. Empty: the steps come from `fix` (`ui::fix_steps`).
    pub steps: Vec<String>,
}

impl Problem {
    pub fn new(title: impl Into<String>) -> Problem {
        Problem {
            title: title.into(),
            ..Problem::default()
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Problem {
        self.detail = Some(detail.into());
        self
    }

    /// The detail as people see it (see `ui::layout`).
    pub fn shown(mut self, shown: impl Into<String>) -> Problem {
        self.shown.get_or_insert_with(Box::default).detail = Some(shown.into());
        self
    }

    pub fn fix(mut self, fix: impl Into<String>) -> Problem {
        self.fix = Some(fix.into());
        self
    }

    /// The fix's steps for people, when splitting `fix` doesn't read well.
    pub fn steps<S: Into<String>>(mut self, steps: impl IntoIterator<Item = S>) -> Problem {
        self.shown.get_or_insert_with(Box::default).steps =
            steps.into_iter().map(Into::into).collect();
        self
    }

    /// The steps people see.
    pub fn fix_steps(&self) -> Vec<String> {
        if let Some(shown) = self.shown.as_ref().filter(|s| !s.steps.is_empty()) {
            return shown.steps.clone();
        }
        self.fix
            .as_deref()
            .map(super::fix_steps)
            .unwrap_or_default()
    }

    pub fn code(mut self, code: &'static str) -> Problem {
        self.code = Some(code);
        self
    }

    /// Without its fix: for a problem the next question takes care of.
    pub fn without_fix(mut self) -> Problem {
        self.fix = None;
        if let Some(shown) = self.shown.as_mut() {
            shown.steps.clear();
        }
        self
    }

    /// The whole problem as one line, for `--json` and logs: the title and
    /// the detail as sentences.
    pub fn sentence(&self) -> String {
        let mut text = self.title.trim_end_matches('.').to_string();
        if let Some(detail) = &self.detail {
            text.push_str(". ");
            text.push_str(detail.trim_end_matches('.'));
        }
        text.replace('`', "")
    }
}

/// One choice of a `Select`.
#[derive(Clone, Debug)]
pub struct Choice {
    pub label: String,
    /// Shown under the highlighted choice.
    pub detail: String,
    /// Why it cannot be chosen, if it cannot.
    pub unavailable: Option<String>,
    /// What the settled line says when it is chosen (default: the label).
    pub answer: Option<String>,
}

impl Choice {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Choice {
        Choice {
            label: label.into(),
            detail: detail.into(),
            unavailable: None,
            answer: None,
        }
    }

    pub fn unavailable(mut self, reason: impl Into<String>) -> Choice {
        self.unavailable = Some(reason.into());
        self
    }

    pub fn answer(mut self, answer: impl Into<String>) -> Choice {
        self.answer = Some(answer.into());
        self
    }
}

/// A list of choices.
pub struct Select<'a> {
    pub question: &'a str,
    /// What the settled line calls the answer; empty settles silently.
    pub label: &'a str,
    pub choices: Vec<Choice>,
    pub default: usize,
    /// The flag that answers it without a terminal.
    pub flag: &'a str,
}

/// A yes-or-no question whose choices name the actions. Yes settles
/// silently (the step that follows is the record); No settles as
/// `declined`, when given.
pub struct Confirm<'a> {
    pub question: &'a str,
    pub yes: &'a str,
    pub yes_detail: &'a str,
    pub no: &'a str,
    pub no_detail: &'a str,
    pub default_yes: bool,
    pub flag: &'a str,
    pub declined: Option<&'a str>,
}

/// A line of text with a default and a check that runs as you type.
pub struct Text<'a> {
    pub question: &'a str,
    /// What the settled line calls the answer.
    pub label: &'a str,
    pub default: String,
    pub hint: &'a str,
    pub check: &'a dyn Fn(&str) -> Result<(), String>,
    pub flag: &'a str,
    /// Turns each typed or pasted character into the one kept (for example
    /// lowercase for machine names).
    pub filter: Option<&'a dyn Fn(char) -> char>,
}

/// Turns a secret's raw text into its value and a description to show, or
/// says why it is not valid.
pub type SecretCheck<'a> = &'a dyn Fn(&str) -> Result<(String, String), String>;

/// A secret, pasted or typed without echo.
pub struct Secret<'a> {
    pub question: &'a str,
    /// What the settled line calls the answer; empty settles silently.
    pub label: &'a str,
    pub hint: &'a str,
    pub check: SecretCheck<'a>,
    pub flag: &'a str,
}

/// A scripted answer, for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Take the default.
    Enter,
    Choose(usize),
    Type(String),
    Cancel,
    /// Esc.
    Back,
}

/// The test backend's state.
#[derive(Debug, Default)]
pub struct Script {
    pub answers: VecDeque<Answer>,
    pub transcript: Vec<String>,
}

enum Backend {
    Terminal,
    Basic,
    Lines,
    Silent,
    Script(Script),
}

/// The text a step shows while it runs; work may change it.
#[derive(Clone)]
pub struct Progress(Arc<Mutex<String>>);

impl Progress {
    pub fn set(&self, value: impl Into<String>) {
        if let Ok(mut text) = self.0.lock() {
            *text = value.into();
        }
    }

    fn get(&self) -> String {
        self.0.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

/// How a finished step settles.
pub enum Settle {
    Done(String),
    Warn(String),
    /// A `label  value` line, aligned with the answers.
    Fact(String, String),
    /// Nothing printed (the step was not needed).
    Quiet,
}

/// The kind of lines last printed, so groups are set apart by a blank line:
/// answers (`label  value` rows) and the steps that follow them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Other,
    Facts,
    Steps,
    /// A block with its own structure (a numbered fix): whatever is marked
    /// after it starts a new group.
    Block,
}

pub struct Ui {
    backend: Backend,
    pub theme: Theme,
    /// Rows from the top of the drawn frame to the cursor (Terminal).
    frame_rows: usize,
    /// Fixed width, for tests.
    pub fixed_columns: Option<usize>,
    /// The last line printed was blank (so spacing never doubles).
    last_blank: bool,
    /// Esc at the next prompt goes back (`Stop::Back`).
    pub back: bool,
    /// The width settled labels are padded to, so values line up.
    pub label_width: usize,
    /// Every line printed: the rows it took, whether it was blank, and the
    /// group it ended, for rewinding to a checkpoint.
    printed: Vec<(usize, bool, Group)>,
    /// The group the last line printed belongs to.
    group: Group,
}

impl Ui {
    /// `interactive`: standard input and error are a terminal and the
    /// caller may ask; `json`: print nothing (the caller prints JSON).
    pub fn new(interactive: bool, json: bool, theme: Theme) -> Ui {
        let dumb = std::env::var("TERM").ok().as_deref() == Some("dumb");
        let backend = if json {
            Backend::Silent
        } else if interactive && dumb {
            Backend::Basic
        } else if interactive {
            Backend::Terminal
        } else {
            Backend::Lines
        };
        Ui {
            backend,
            theme,
            frame_rows: 0,
            fixed_columns: None,
            last_blank: true,
            back: false,
            label_width: 0,
            printed: Vec::new(),
            group: Group::Other,
        }
    }

    /// A scripted Ui for tests, `columns` wide, without color.
    pub fn script(answers: Vec<Answer>, columns: usize) -> Ui {
        Ui {
            backend: Backend::Script(Script {
                answers: answers.into(),
                transcript: Vec::new(),
            }),
            theme: Theme::plain(),
            frame_rows: 0,
            fixed_columns: Some(columns),
            last_blank: true,
            back: false,
            label_width: 0,
            printed: Vec::new(),
            group: Group::Other,
        }
    }

    /// A Ui that prints nothing and asks nothing, for tests.
    pub fn silent() -> Ui {
        Ui::new(false, true, Theme::plain())
    }

    /// The test transcript.
    pub fn transcript(&self) -> &[String] {
        match &self.backend {
            Backend::Script(script) => &script.transcript,
            _ => &[],
        }
    }

    /// Answers left in the script (tests).
    pub fn answers_left(&self) -> usize {
        match &self.backend {
            Backend::Script(script) => script.answers.len(),
            _ => 0,
        }
    }

    /// Whether questions can be asked.
    pub fn interactive(&self) -> bool {
        matches!(
            self.backend,
            Backend::Terminal | Backend::Basic | Backend::Script(_)
        )
    }

    /// Whether prompts can go back with Esc (a terminal that draws, or a
    /// test script).
    pub fn can_go_back(&self) -> bool {
        matches!(self.backend, Backend::Terminal | Backend::Script(_))
    }

    /// Whether anything is printed.
    pub fn prints(&self) -> bool {
        !matches!(self.backend, Backend::Silent)
    }

    pub fn columns(&self) -> usize {
        self.fixed_columns.unwrap_or_else(term::columns)
    }

    // Output ---------------------------------------------------------------

    /// Print finished lines.
    pub fn print(&mut self, lines: &[String]) {
        if let Some(last) = lines.last() {
            self.last_blank = last.is_empty();
        }
        let columns = self.columns();
        for line in lines {
            let rows = self.rows(line, columns);
            self.printed.push((rows, line.is_empty(), self.group));
        }
        match &mut self.backend {
            Backend::Silent => {}
            Backend::Script(script) => {
                script
                    .transcript
                    .extend(lines.iter().map(|l| super::strip_codes(l)));
            }
            _ => {
                let mut text = String::new();
                for line in lines {
                    text.push_str(line);
                    text.push('\n');
                }
                term::put(&text);
            }
        }
    }

    /// A point to come back to with `rewind`.
    pub fn checkpoint(&self) -> usize {
        self.printed.len()
    }

    /// Take back everything printed since `checkpoint` (going back to an
    /// earlier question). On a terminal the lines are erased, when they are
    /// still on the screen.
    pub fn rewind(&mut self, checkpoint: usize) {
        if checkpoint >= self.printed.len() {
            return;
        }
        let rows: usize = self.printed[checkpoint..].iter().map(|(r, _, _)| r).sum();
        match &mut self.backend {
            Backend::Terminal => {
                if rows > 0 && term::rows().is_some_and(|height| rows < height) {
                    term::put(&format!("\x1b[{rows}A\r\x1b[J"));
                }
            }
            Backend::Script(script) => script.transcript.truncate(checkpoint),
            _ => {}
        }
        self.printed.truncate(checkpoint);
        self.last_blank = self.printed.last().is_none_or(|(_, blank, _)| *blank);
        self.group = self
            .printed
            .last()
            .map_or(Group::Other, |(_, _, group)| *group);
    }

    /// Start a group of lines: a blank line first when the lines before
    /// are a different group of answers or steps.
    fn enter(&mut self, group: Group) {
        if group != self.group && self.group != Group::Other {
            self.blank();
        }
        self.group = group;
    }

    /// An empty line, unless the last line printed was one.
    pub fn blank(&mut self) {
        if !self.last_blank {
            self.print(&[String::new()]);
        }
    }

    /// The program's name in bold and its version dimmed, then what it is
    /// for, set apart by blank lines.
    pub fn title(&mut self, name: &str, version: &str, subtitle: &str) {
        let columns = self.columns();
        let mut lines = if self.last_blank {
            Vec::new()
        } else {
            vec![String::new()]
        };
        if matches!(self.backend, Backend::Terminal | Backend::Basic) {
            lines.insert(0, String::new());
        }
        let theme = self.theme;
        let mut head = format!("{}{}", " ".repeat(INDENT), theme.strong(name));
        if !version.is_empty() {
            head.push_str(&theme.muted(&format!(" {version}")));
        }
        lines.push(head);
        if !subtitle.is_empty() {
            lines.extend(para(&theme, subtitle, Tone::Normal, columns));
        }
        lines.push(String::new());
        self.group = Group::Other;
        self.print(&lines);
    }

    /// A finished step.
    pub fn done(&mut self, value: &str) {
        self.enter(Group::Steps);
        let lines = item(&self.theme, Mark::Done, value, Tone::Normal, self.columns());
        self.print(&lines);
    }

    /// A short bold heading for the group that follows, after a blank line.
    pub fn heading(&mut self, value: &str) {
        self.blank();
        self.group = Group::Other;
        let lines = super::heading(&self.theme, value, self.columns());
        self.print(&lines);
    }

    /// Steps to do next, numbered when there are several.
    pub fn steps(&mut self, steps: &[String]) {
        self.group = Group::Other;
        let lines = super::steps(&self.theme, steps, self.columns());
        self.print(&lines);
    }

    pub fn warn(&mut self, value: &str) {
        let lines = item(
            &self.theme,
            Mark::Warned,
            value,
            Tone::Normal,
            self.columns(),
        );
        self.print(&lines);
    }

    /// Unmarked text in the text column.
    pub fn say(&mut self, value: &str, tone: Tone) {
        self.group = Group::Other;
        let lines = para(&self.theme, value, tone, self.columns());
        self.print(&lines);
    }

    /// A command to copy, on lines of its own.
    pub fn command(&mut self, value: &str) {
        self.group = Group::Other;
        let lines = super::command(&self.theme, value, self.columns());
        self.print(&lines);
    }

    /// A finished question and its answer.
    pub fn answered(&mut self, label: &str, answer: &str) {
        self.fact(Mark::Done, label, answer);
    }

    /// A `label  value` line with a mark; labels are padded to
    /// `label_width` so values line up, and a long value wraps under its
    /// own column.
    pub fn fact(&mut self, mark: Mark, label: &str, value: &str) {
        self.enter(Group::Facts);
        let lines = fact_lines(
            &self.theme,
            mark,
            label,
            value,
            self.label_width,
            self.columns(),
        );
        self.print(&lines);
    }

    /// A problem: the cross and what went wrong, why, and the fix. It ends
    /// the steps before it, so it joins their group.
    pub fn problem(&mut self, problem: &Problem) {
        self.enter(Group::Steps);
        let lines = problem_lines(&self.theme, problem, self.columns());
        self.print(&lines);
        if problem.fix_steps().len() > 1 {
            self.group = Group::Block;
        }
    }

    /// A warning with an explanation and a fix.
    pub fn caution(&mut self, problem: &Problem) {
        let lines = caution_lines(&self.theme, problem, self.columns());
        self.print(&lines);
        if problem.fix_steps().len() > 1 {
            self.group = Group::Block;
        }
    }

    // Drawing (Terminal) ---------------------------------------------------

    fn rows(&self, line: &str, columns: usize) -> usize {
        visible_width(line).max(1).div_ceil(columns.max(1))
    }

    /// Redraw the frame: `lines`, with the cursor at (line, cell) or hidden.
    fn draw(&mut self, lines: &[String], cursor: Option<(usize, usize)>) {
        let columns = self.columns();
        let mut out = String::new();
        if self.frame_rows > 0 {
            out.push_str(&format!("\x1b[{}A", self.frame_rows));
        }
        out.push_str("\r\x1b[J");
        out.push_str(if cursor.is_some() {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        });
        out.push_str(&lines.join("\n"));
        let rows: Vec<usize> = lines.iter().map(|l| self.rows(l, columns)).collect();
        let total: usize = rows.iter().sum::<usize>().max(1);
        let mut at = total - 1;
        if let Some((line, cell)) = cursor {
            let row = rows[..line.min(rows.len())].iter().sum::<usize>() + cell / columns;
            let up = (total - 1).saturating_sub(row);
            if up > 0 {
                out.push_str(&format!("\x1b[{up}A"));
            }
            out.push('\r');
            if cell % columns > 0 {
                out.push_str(&format!("\x1b[{}C", cell % columns));
            }
            at = row;
        }
        self.frame_rows = at;
        term::mark_drawn();
        term::put(&out);
    }

    /// A frame's lines, after a blank line when the lines above it are not
    /// one, so the question stands apart; the blank goes when the frame does.
    fn spaced(&self, mut lines: Vec<String>) -> Vec<String> {
        if !self.last_blank {
            lines.insert(0, String::new());
        }
        lines
    }

    /// The frame line the value is on (after the blank `spaced` adds).
    fn value_line(&self) -> usize {
        if self.last_blank {
            1
        } else {
            2
        }
    }

    /// Erase the frame.
    fn clear_frame(&mut self) {
        let mut out = String::new();
        if self.frame_rows > 0 {
            out.push_str(&format!("\x1b[{}A", self.frame_rows));
        }
        out.push_str("\r\x1b[J\x1b[?25h");
        self.frame_rows = 0;
        term::put(&out);
    }

    fn missing(&self, flag: &str) -> Stop {
        Stop::Missing(flag.to_string())
    }

    fn scripted(&mut self) -> Option<Answer> {
        let back = self.back;
        match &mut self.backend {
            Backend::Script(script) => {
                let answer = script.answers.pop_front().unwrap_or(Answer::Cancel);
                assert!(
                    answer != Answer::Back || back,
                    "scripted Back where the flow cannot go back"
                );
                Some(answer)
            }
            _ => None,
        }
    }

    /// The key line under a prompt: `keys`, and Esc when it goes back.
    fn keys_line(&self, keys: &str) -> String {
        let text = match (keys.is_empty(), self.back) {
            (true, true) => "Esc to go back".to_string(),
            (false, true) => format!("{keys}, Esc to go back"),
            (_, false) => keys.to_string(),
        };
        format!("{}{}", " ".repeat(INDENT), self.theme.muted(&text))
    }

    /// The key hints, after a blank line.
    fn push_keys(&self, lines: &mut Vec<String>, keys: &str) {
        lines.push(String::new());
        lines.push(self.keys_line(keys));
    }

    // Select ---------------------------------------------------------------

    /// Ask for one of `select.choices`; returns its index.
    pub fn select(&mut self, select: &Select<'_>) -> Result<usize, Stop> {
        let available: Vec<usize> = (0..select.choices.len())
            .filter(|i| select.choices[*i].unavailable.is_none())
            .collect();
        let default = if available.contains(&select.default) {
            select.default
        } else {
            *available.first().ok_or_else(|| self.missing(select.flag))?
        };
        let chosen = match &self.backend {
            Backend::Lines | Backend::Silent => return Err(self.missing(select.flag)),
            Backend::Script(_) => match self.scripted().expect("script") {
                Answer::Enter => default,
                Answer::Choose(index) if available.contains(&index) => index,
                Answer::Cancel => return Err(Stop::Cancelled),
                Answer::Back => return Err(Stop::Back),
                other => panic!("scripted {other:?} for select {:?}", select.question),
            },
            Backend::Basic => self.select_basic(select, &available, default)?,
            Backend::Terminal => self.select_terminal(select, &available, default)?,
        };
        if matches!(self.backend, Backend::Terminal) {
            self.clear_frame();
        }
        if !select.label.is_empty() {
            let choice = &select.choices[chosen];
            let answer = choice
                .answer
                .clone()
                .unwrap_or_else(|| choice.label.clone());
            self.answered(select.label, &answer);
        }
        Ok(chosen)
    }

    /// The question, its choices (the highlighted one bold, after the
    /// pointer), what the highlighted choice does, and the keys.
    fn select_frame(&self, select: &Select<'_>, highlighted: usize) -> Vec<String> {
        let theme = self.theme;
        let columns = self.columns();
        let width = text_width(columns);
        let mut lines = vec![format!(
            "{}{}",
            theme.mark(Mark::Ask),
            theme.strong(select.question)
        )];
        let dash = if theme.unicode { "—" } else { "-" };
        for (index, choice) in select.choices.iter().enumerate() {
            let pointer = if index == highlighted {
                theme.accent(theme.pointer())
            } else {
                " ".into()
            };
            // An unavailable choice: its label, then why, after a dash.
            let plain = match &choice.unavailable {
                Some(reason) => format!("{} {dash} {reason}", choice.label),
                None => choice.label.clone(),
            };
            for (n, piece) in wrap_rich(&Theme::plain(), &plain, width - 2, Tone::Normal)
                .into_iter()
                .enumerate()
            {
                let painted = if index == highlighted && choice.unavailable.is_none() {
                    theme.strong(&piece)
                } else {
                    piece
                };
                if n == 0 {
                    lines.push(format!("{}{pointer} {painted}", " ".repeat(INDENT)));
                } else {
                    lines.push(format!("{}{painted}", " ".repeat(INDENT + 2)));
                }
            }
        }
        if let Some(choice) = select.choices.get(highlighted) {
            if !choice.detail.is_empty() {
                lines.push(String::new());
                lines.extend(para(&theme, &choice.detail, Tone::Normal, columns));
            }
        }
        let keys = if theme.unicode {
            "↑ and ↓ to move, Enter to choose"
        } else {
            "Up and Down to move, Enter to choose"
        };
        self.push_keys(&mut lines, keys);
        self.spaced(lines)
    }

    fn select_terminal(
        &mut self,
        select: &Select<'_>,
        available: &[usize],
        default: usize,
    ) -> Result<usize, Stop> {
        let raw = term::Raw::enter().map_err(|_| self.missing(select.flag))?;
        let mut highlighted = default;
        loop {
            let frame = self.select_frame(select, highlighted);
            self.draw(&frame, None);
            let key = term::read_key().map_err(|_| Stop::Cancelled);
            let position = available
                .iter()
                .position(|i| *i == highlighted)
                .unwrap_or(0);
            match key {
                Ok(Key::Up) | Ok(Key::Char('k')) => {
                    highlighted = available[(position + available.len() - 1) % available.len()];
                }
                Ok(Key::Down) | Ok(Key::Char('j')) | Ok(Key::Tab) => {
                    highlighted = available[(position + 1) % available.len()];
                }
                Ok(Key::Char(c)) if c.is_ascii_digit() => {
                    let index = (c as usize).wrapping_sub('1' as usize);
                    if available.contains(&index) {
                        highlighted = index;
                    }
                }
                Ok(Key::Enter) => {
                    drop(raw);
                    return Ok(highlighted);
                }
                Ok(Key::Escape) if self.back => {
                    drop(raw);
                    self.clear_frame();
                    return Err(Stop::Back);
                }
                Ok(Key::Interrupt) | Ok(Key::EndOfInput) | Err(_) => {
                    drop(raw);
                    self.clear_frame();
                    return Err(Stop::Cancelled);
                }
                _ => {}
            }
        }
    }

    /// Numbered choices, for a terminal that cannot draw: only the ones that
    /// can be chosen, with a note for the others.
    fn select_basic(
        &mut self,
        select: &Select<'_>,
        available: &[usize],
        default: usize,
    ) -> Result<usize, Stop> {
        let theme = self.theme;
        let mut lines = self.spaced(vec![format!(
            "{}{}",
            theme.mark(Mark::Ask),
            theme.strong(select.question)
        )]);
        for (number, index) in available.iter().enumerate() {
            lines.push(format!(
                "    {}. {}",
                number + 1,
                select.choices[*index].label
            ));
        }
        for choice in &select.choices {
            if let Some(reason) = &choice.unavailable {
                lines.push(format!("  Not available here: {} ({reason})", choice.label));
            }
        }
        self.print(&lines);
        let default_number = available.iter().position(|i| *i == default).unwrap_or(0) + 1;
        loop {
            term::put(&format!(
                "  Choose 1-{} [{default_number}]: ",
                available.len()
            ));
            let line = read_line().ok_or(Stop::Cancelled)?;
            let line = line.trim();
            if line.is_empty() {
                return Ok(default);
            }
            if let Ok(number) = line.parse::<usize>() {
                if number >= 1 && number <= available.len() {
                    return Ok(available[number - 1]);
                }
            }
        }
    }

    /// Yes or no, as two choices that name the actions.
    pub fn confirm(&mut self, confirm: &Confirm<'_>) -> Result<bool, Stop> {
        let select = Select {
            question: confirm.question,
            label: "",
            choices: vec![
                Choice::new(confirm.yes, confirm.yes_detail),
                Choice::new(confirm.no, confirm.no_detail),
            ],
            default: if confirm.default_yes { 0 } else { 1 },
            flag: confirm.flag,
        };
        let yes = self.select(&select)? == 0;
        if !yes {
            if let Some(declined) = confirm.declined {
                self.done(declined);
            }
        }
        Ok(yes)
    }

    // Text -----------------------------------------------------------------

    /// Ask for a line of text, checked as it is typed.
    pub fn text(&mut self, text: &Text<'_>) -> Result<String, Stop> {
        let value = match &self.backend {
            Backend::Lines | Backend::Silent => return Err(self.missing(text.flag)),
            Backend::Script(_) => match self.scripted().expect("script") {
                Answer::Enter => text.default.clone(),
                Answer::Type(value) => match text.filter {
                    Some(filter) => value.chars().map(filter).collect(),
                    None => value,
                },
                Answer::Cancel => return Err(Stop::Cancelled),
                Answer::Back => return Err(Stop::Back),
                other => panic!("scripted {other:?} for text {:?}", text.question),
            },
            Backend::Basic => loop {
                if text.default.is_empty() {
                    term::put(&format!("? {}: ", text.question));
                } else {
                    term::put(&format!("? {} [{}]: ", text.question, text.default));
                }
                let line = read_line().ok_or(Stop::Cancelled)?;
                let value = if line.trim().is_empty() {
                    text.default.clone()
                } else {
                    let typed = line.trim();
                    match text.filter {
                        Some(filter) => typed.chars().map(filter).collect(),
                        None => typed.to_string(),
                    }
                };
                match (text.check)(&value) {
                    Ok(()) => break value,
                    Err(reason) => term::put(&format!("  ! {reason}\n")),
                }
            },
            Backend::Terminal => self.text_terminal(text)?,
        };
        if let Err(reason) = (text.check)(&value) {
            // Only scripts can get here with a bad value.
            panic!(
                "scripted invalid {value:?} for {:?}: {reason}",
                text.question
            );
        }
        if matches!(self.backend, Backend::Terminal) {
            self.clear_frame();
        }
        self.answered(text.label, &value);
        Ok(value)
    }

    /// A prompt's frame: the question, the value line, the hint (or what is
    /// wrong, after a warning mark), and the keys.
    fn text_frame(
        &self,
        question: &str,
        shown: &str,
        message: Result<String, String>,
        keys: &str,
    ) -> Vec<String> {
        let theme = self.theme;
        let columns = self.columns();
        let mut lines = vec![format!(
            "{}{}",
            theme.mark(Mark::Ask),
            theme.strong(question)
        )];
        lines.push(format!(
            "{}{} {shown}",
            " ".repeat(INDENT),
            theme.accent(theme.pointer())
        ));
        match &message {
            Ok(hint) if hint.is_empty() => {}
            Ok(hint) => lines.extend(para(&theme, hint, Tone::Normal, columns)),
            Err(reason) => lines.extend(item(&theme, Mark::Warned, reason, Tone::Normal, columns)),
        }
        if !keys.is_empty() || self.back {
            self.push_keys(&mut lines, keys);
        }
        self.spaced(lines)
    }

    fn text_terminal(&mut self, text: &Text<'_>) -> Result<String, Stop> {
        let raw = term::Raw::enter().map_err(|_| self.missing(text.flag))?;
        term::paste_mode(true);
        // The default is a placeholder: shown dim, replaced by typing.
        let placeholder = text.default.as_str();
        let mut buffer: Vec<char> = Vec::new();
        let mut cursor = 0usize;
        let mut tried = false;
        let mut too_large = false;
        let keep = |c: char| match text.filter {
            Some(filter) => filter(c),
            None => c,
        };
        let result = loop {
            let typed: String = buffer.iter().collect();
            let showing = buffer.is_empty() && !placeholder.is_empty();
            let value = if buffer.is_empty() {
                placeholder.to_string()
            } else {
                typed.clone()
            };
            let checked = (text.check)(&value);
            let message = match &checked {
                _ if too_large => Err(PASTE_TOO_LARGE.to_string()),
                Ok(()) => Ok(text.hint.to_string()),
                Err(reason) if tried || !buffer.is_empty() => Err(reason.clone()),
                Err(_) => Ok(text.hint.to_string()),
            };
            too_large = false;
            let room = text_width(self.columns()).saturating_sub(3).max(8);
            let (shown, cell) = if showing {
                (
                    self.theme.muted(&super::truncate(placeholder, room)),
                    INDENT + 2,
                )
            } else {
                let start = cursor.saturating_sub(room);
                let shown: String = buffer[start..].iter().take(room + 1).collect();
                let cell =
                    INDENT + 2 + display_width(&buffer[start..cursor].iter().collect::<String>());
                (shown, cell)
            };
            let keys = if showing {
                "Enter to accept, Tab to edit"
            } else {
                "Enter to continue"
            };
            let frame = self.text_frame(text.question, &shown, message, keys);
            self.draw(&frame, Some((self.value_line(), cell)));
            match term::read_key() {
                Ok(Key::Enter) => {
                    if checked.is_ok() {
                        break Ok(value);
                    }
                    tried = true;
                }
                Ok(Key::Tab) | Ok(Key::Right) | Ok(Key::End) if showing => {
                    buffer = placeholder.chars().collect();
                    cursor = buffer.len();
                }
                Ok(Key::Char(c)) => {
                    buffer.insert(cursor, keep(c));
                    cursor += 1;
                }
                Ok(Key::Paste(pasted)) => {
                    for c in pasted.chars().filter(|c| !c.is_control()) {
                        buffer.insert(cursor, keep(c));
                        cursor += 1;
                    }
                }
                Ok(Key::PasteTooLarge) => too_large = true,
                Ok(Key::Backspace) if cursor > 0 => {
                    cursor -= 1;
                    buffer.remove(cursor);
                }
                Ok(Key::Delete) if cursor < buffer.len() => {
                    buffer.remove(cursor);
                }
                Ok(Key::Left) => cursor = cursor.saturating_sub(1),
                Ok(Key::Right) => cursor = (cursor + 1).min(buffer.len()),
                Ok(Key::Home) => cursor = 0,
                Ok(Key::End) => cursor = buffer.len(),
                Ok(Key::ClearLine) => {
                    buffer.drain(..cursor);
                    cursor = 0;
                }
                Ok(Key::DeleteWord) => {
                    let mut start = cursor;
                    while start > 0 && buffer[start - 1] == ' ' {
                        start -= 1;
                    }
                    while start > 0 && buffer[start - 1] != ' ' {
                        start -= 1;
                    }
                    buffer.drain(start..cursor);
                    cursor = start;
                }
                Ok(Key::Escape) if self.back => break Err(Stop::Back),
                Ok(Key::Interrupt) | Err(_) => break Err(Stop::Cancelled),
                Ok(Key::EndOfInput) if buffer.is_empty() => break Err(Stop::Cancelled),
                _ => {}
            }
        };
        term::paste_mode(false);
        drop(raw);
        if result.is_err() {
            self.clear_frame();
        }
        result
    }

    // Secret ---------------------------------------------------------------

    /// Ask for a secret: pasted (line breaks and spaces are dropped) or
    /// typed, never shown.
    pub fn secret(&mut self, secret: &Secret<'_>) -> Result<String, Stop> {
        let (value, description) = match &self.backend {
            Backend::Lines | Backend::Silent => return Err(self.missing(secret.flag)),
            Backend::Script(_) => match self.scripted().expect("script") {
                Answer::Type(raw) => match (secret.check)(&raw) {
                    Ok(found) => found,
                    Err(reason) => {
                        let lines = item(
                            &self.theme,
                            Mark::Warned,
                            &reason,
                            Tone::Normal,
                            self.columns(),
                        );
                        self.print(&lines);
                        return self.secret(secret);
                    }
                },
                Answer::Cancel => return Err(Stop::Cancelled),
                Answer::Back => return Err(Stop::Back),
                other => panic!("scripted {other:?} for secret {:?}", secret.question),
            },
            Backend::Basic => loop {
                term::put(&format!("? {} (hidden): ", secret.question));
                let quiet = term::Quiet::enter();
                let mut line = read_line().ok_or(Stop::Cancelled)?;
                // A paste with line breaks arrives as several lines at once.
                while term::input_ready(60) {
                    match read_line() {
                        Some(more) => line.push_str(&more),
                        None => break,
                    }
                }
                drop(quiet);
                term::put("\n");
                match (secret.check)(&line) {
                    Ok(found) => break found,
                    Err(reason) => term::put(&format!("  ! {reason}\n")),
                }
            },
            Backend::Terminal => self.secret_terminal(secret)?,
        };
        if matches!(self.backend, Backend::Terminal) {
            self.clear_frame();
        }
        if !secret.label.is_empty() {
            self.answered(secret.label, &description);
        }
        Ok(value)
    }

    fn secret_terminal(&mut self, secret: &Secret<'_>) -> Result<(String, String), Stop> {
        let raw = term::Raw::enter().map_err(|_| self.missing(secret.flag))?;
        term::paste_mode(true);
        let mut buffer = String::new();
        let mut tried = false;
        let mut too_large = false;
        let result = loop {
            let checked = (secret.check)(&buffer);
            let (shown, message) = if buffer.is_empty() {
                (
                    String::new(),
                    if tried {
                        Err("Nothing was pasted yet.".to_string())
                    } else {
                        Ok(secret.hint.to_string())
                    },
                )
            } else {
                let count = buffer.chars().filter(|c| !c.is_whitespace()).count();
                let shown = format!(
                    "hidden, {count} {}",
                    if count == 1 {
                        "character"
                    } else {
                        "characters"
                    }
                );
                match &checked {
                    Ok((_, description)) => (
                        shown,
                        Ok(format!("{description}. Press Enter to continue.")),
                    ),
                    Err(reason) => (shown, Err(reason.clone())),
                }
            };
            let message = if too_large {
                Err(PASTE_TOO_LARGE.to_string())
            } else {
                message
            };
            too_large = false;
            let cell = INDENT + 2 + display_width(&shown);
            let frame = self.text_frame(secret.question, &self.theme.muted(&shown), message, "");
            self.draw(&frame, Some((self.value_line(), cell)));
            match term::read_key() {
                Ok(Key::Enter) => {
                    // Without bracketed paste a pasted line break arrives as
                    // Enter with more text right behind it: keep reading.
                    if term::input_ready(30) {
                        continue;
                    }
                    if let Ok(found) = checked {
                        break Ok(found);
                    }
                    tried = true;
                }
                Ok(Key::Char(c)) => buffer.push(c),
                Ok(Key::Paste(pasted)) => buffer.push_str(&pasted),
                Ok(Key::PasteTooLarge) => too_large = true,
                Ok(Key::Backspace) => {
                    buffer.pop();
                }
                Ok(Key::ClearLine) | Ok(Key::DeleteWord) => buffer.clear(),
                Ok(Key::Escape) if self.back => break Err(Stop::Back),
                Ok(Key::Interrupt) | Err(_) => break Err(Stop::Cancelled),
                Ok(Key::EndOfInput) if buffer.is_empty() => break Err(Stop::Cancelled),
                _ => {}
            }
        };
        term::paste_mode(false);
        drop(raw);
        if result.is_err() {
            self.clear_frame();
        }
        result
    }

    // Steps ----------------------------------------------------------------

    /// Run `work`, with a spinner on a terminal, and settle into one line:
    /// what `settle` says on success, or the problem on failure.
    pub fn step<T>(
        &mut self,
        running: &str,
        work: impl FnOnce(&Progress) -> Result<T, Problem>,
        settle: impl FnOnce(&T) -> Settle,
    ) -> Result<T, Problem> {
        let progress = Progress(Arc::new(Mutex::new(running.to_string())));
        let spinner = matches!(self.backend, Backend::Terminal)
            .then(|| Spinner::start(self.theme, progress.clone(), self.columns()));
        if matches!(self.backend, Backend::Basic) {
            term::put(&format!("  {running} ...\n"));
        }
        let result = work(&progress);
        if let Some(spinner) = spinner {
            spinner.stop();
        }
        match &result {
            Ok(value) => match settle(value) {
                Settle::Done(text) => self.done(&text),
                Settle::Warn(text) => self.warn(&text),
                Settle::Fact(label, value) => self.answered(&label, &value),
                Settle::Quiet => {}
            },
            Err(problem) => self.problem(problem),
        }
        result
    }

    /// Put the terminal back before printing a final message.
    pub fn finish(&mut self) {
        if matches!(self.backend, Backend::Terminal) && self.frame_rows > 0 {
            self.clear_frame();
        }
        term::restore();
    }
}

/// A `label  value` line (see `Ui::fact`).
pub fn fact_lines(
    theme: &Theme,
    mark: Mark,
    label: &str,
    value: &str,
    label_width: usize,
    columns: usize,
) -> Vec<String> {
    let width = text_width(columns);
    let pad = label_width.max(display_width(label));
    let room = width.saturating_sub(pad + 2);
    let value = super::format_text(value);
    if room < 16 {
        let mut lines = item(theme, mark, label, Tone::Normal, columns);
        lines.extend(para(theme, &value, Tone::Normal, columns));
        return lines;
    }
    let head = format!("{}{}  ", theme.mark(mark), padded(label, pad));
    let hang = " ".repeat(INDENT + pad + 2);
    let wrapped = crate::cli::format::wrap_cells(&value, room);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("{head}{line}").trim_end().to_string()
            } else {
                format!("{hang}{line}")
            }
        })
        .collect()
}

/// The lines of a problem: the cross and what went wrong in bold, why,
/// and how to fix it.
pub fn problem_lines(theme: &Theme, problem: &Problem, columns: usize) -> Vec<String> {
    let mut lines = item(theme, Mark::Failed, &problem.title, Tone::Strong, columns);
    lines.extend(problem_tail(theme, problem, columns));
    lines
}

/// A warning's lines: the mark and what to know, why, and what to do.
pub fn caution_lines(theme: &Theme, problem: &Problem, columns: usize) -> Vec<String> {
    let mut lines = item(theme, Mark::Warned, &problem.title, Tone::Normal, columns);
    lines.extend(problem_tail(theme, problem, columns));
    lines
}

fn problem_tail(theme: &Theme, problem: &Problem, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let shown = problem.shown.as_ref().and_then(|s| s.detail.as_ref());
    if let Some(detail) = shown.or(problem.detail.as_ref()) {
        lines.extend(para(theme, detail, Tone::Normal, columns));
    }
    lines.extend(super::fix_lines(theme, &problem.fix_steps(), columns));
    lines
}

/// One line typed on a terminal in canonical mode, without its newline.
fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
    }
}

/// The spinner of a running step.
struct Spinner {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn start(theme: Theme, progress: Progress, columns: usize) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        term::mark_drawn();
        let thread = std::thread::spawn(move || {
            let frames = theme.spinner();
            let mut index = 0;
            let width = columns.saturating_sub(INDENT + 1).max(8);
            while !flag.load(Ordering::SeqCst) {
                let text = super::truncate(&progress.get(), width);
                term::put(&format!(
                    "\x1b[?25l\r\x1b[K{} {}",
                    theme.accent(frames[index % frames.len()]),
                    text
                ));
                index += 1;
                for _ in 0..8 {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            term::put("\r\x1b[K\x1b[?25h");
        });
        Spinner {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_prompts_answer_and_settle_aligned() {
        let mut ui = Ui::script(
            vec![
                Answer::Enter,
                Answer::Type("Lap Top".into()),
                Answer::Type("bad".into()),
                Answer::Type("sbi1.ok".into()),
                Answer::Choose(1),
            ],
            80,
        );
        ui.label_width = 12;
        let choice = ui
            .select(&Select {
                question: "What should this machine do?",
                label: "Setup",
                choices: vec![
                    Choice::new("Join a server", "join it"),
                    Choice::new("Serve", "").unavailable("needs Linux"),
                ],
                default: 1,
                flag: "--invite",
            })
            .unwrap();
        assert_eq!(choice, 0);
        let lower = |c: char| {
            if c == ' ' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        };
        let name = ui
            .text(&Text {
                question: "Machine name",
                label: "Machine name",
                default: "host".into(),
                hint: "",
                check: &|_| Ok(()),
                flag: "--name",
                filter: Some(&lower),
            })
            .unwrap();
        assert_eq!(name, "lap-top");
        let check = |raw: &str| {
            if raw.starts_with("sbi1.") {
                Ok((raw.to_string(), "for the server".to_string()))
            } else {
                Err("That is not an invite.".to_string())
            }
        };
        let secret = ui
            .secret(&Secret {
                question: "Paste the invite",
                label: "",
                hint: "",
                check: &check,
                flag: "--invite -",
            })
            .unwrap();
        assert_eq!(secret, "sbi1.ok");
        let yes = ui
            .confirm(&Confirm {
                question: "Install the Claude Code plugin?",
                yes: "Install it",
                yes_detail: "",
                no: "Skip",
                no_detail: "",
                default_yes: true,
                flag: "--plugin",
                declined: Some("Skipped the Claude Code plugin"),
            })
            .unwrap();
        assert!(!yes);
        assert_eq!(
            ui.transcript(),
            [
                "✓ Setup         Join a server",
                "✓ Machine name  lap-top",
                "! That is not an invite.",
                "",
                "✓ Skipped the Claude Code plugin"
            ]
        );
    }

    #[test]
    fn problems_read_as_sentences_in_json() {
        let problem = Problem::new("Nothing to set up: no invite and no server options.")
            .detail("There is no terminal to ask what this machine should do.");
        assert_eq!(
            problem.sentence(),
            "Nothing to set up: no invite and no server options. There is no terminal to ask what this machine should do"
        );
    }

    #[test]
    fn long_values_wrap_under_their_column() {
        let lines = fact_lines(
            &Theme::plain(),
            Mark::Done,
            "Server",
            "localhost:50298, certificate matches the invite",
            12,
            50,
        );
        assert_eq!(
            lines,
            [
                "✓ Server        localhost:50298, certificate",
                "                matches the invite"
            ]
        );
    }

    #[test]
    fn going_back_rewinds_to_the_checkpoint() {
        let mut ui = Ui::script(vec![Answer::Enter, Answer::Back], 80);
        ui.say("Title", Tone::Strong);
        let mark = ui.checkpoint();
        ui.text(&Text {
            question: "Machine name",
            label: "Machine name",
            default: "host".into(),
            hint: "",
            check: &|_| Ok(()),
            flag: "--name",
            filter: None,
        })
        .unwrap();
        ui.back = true;
        let back = ui.text(&Text {
            question: "Owner",
            label: "Owner",
            default: "me".into(),
            hint: "",
            check: &|_| Ok(()),
            flag: "--owner",
            filter: None,
        });
        assert_eq!(back, Err(Stop::Back));
        ui.rewind(mark);
        assert_eq!(ui.transcript(), ["  Title"]);
    }

    #[test]
    fn without_a_terminal_questions_name_their_flag() {
        let mut ui = Ui::new(false, false, Theme::plain());
        let asked = ui.text(&Text {
            question: "Machine name",
            label: "Machine name",
            default: "host".into(),
            hint: "",
            check: &|_| Ok(()),
            flag: "--name",
            filter: None,
        });
        assert_eq!(asked, Err(Stop::Missing("--name".into())));
    }

    #[test]
    fn steps_settle_into_one_line_or_a_problem() {
        let mut ui = Ui::script(vec![], 80);
        ui.label_width = 12;
        let value = ui
            .step(
                "Working",
                |_| Ok(3),
                |n| Settle::Done(format!("Did {n} things")),
            )
            .unwrap();
        assert_eq!(value, 3);
        ui.step(
            "Checking",
            |_| Ok(()),
            |_| Settle::Fact("Server".into(), "localhost:1".into()),
        )
        .unwrap();
        let failed: Result<(), Problem> = ui.step(
            "Joining",
            |_| {
                Err(Problem::new("This invite was already used.")
                    .fix("ask for a new one with `sudo switchboard server invite`."))
            },
            |_| Settle::Quiet,
        );
        assert!(failed.is_err());
        assert_eq!(
            ui.transcript(),
            [
                "✓ Did 3 things",
                "",
                "✓ Server        localhost:1",
                "",
                "✗ This invite was already used.",
                "  To fix, ask for a new one with",
                "      sudo switchboard server invite"
            ]
        );
    }

    #[test]
    fn select_frames_mark_unavailable_choices_and_keys() {
        let mut ui = Ui::script(vec![], 80);
        let select = Select {
            question: "What should this machine do?",
            label: "Setup",
            choices: vec![
                Choice::new("Join a server", "With an invite."),
                Choice::new("Run the server", "").unavailable("needs Linux"),
            ],
            default: 0,
            flag: "--invite",
        };
        let frame: Vec<String> = ui
            .select_frame(&select, 0)
            .iter()
            .map(|l| super::super::strip_codes(l))
            .collect();
        assert_eq!(
            frame,
            [
                "? What should this machine do?",
                "  › Join a server",
                "    Run the server — needs Linux",
                "",
                "  With an invite.",
                "",
                "  ↑ and ↓ to move, Enter to choose"
            ]
        );
        ui.back = true;
        assert!(
            super::super::strip_codes(ui.select_frame(&select, 0).last().unwrap())
                .ends_with("Enter to choose, Esc to go back")
        );
    }
}
