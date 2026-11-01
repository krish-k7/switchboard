//! Offering to put `~/bin` on PATH, so `switchboard` runs by name, and to
//! turn on Tab completion (`switchboard completion`) in the same file.
//!
//! install.sh writes the command to `~/bin/switchboard` (and links it from
//! `~/.local/bin`). When neither directory is on PATH, init finds the
//! startup file of the shell in `SHELL`, shows the exact line it would add,
//! and asks. It never adds a line twice: a startup file that already puts
//! `~/bin` on PATH is left alone. Without a terminal, or with `--yes`, it
//! only says what to add; it edits a startup file only when asked.
//!
//! Bash on macOS starts login shells, which read only the first of
//! `~/.bash_profile`, `~/.bash_login`, and `~/.profile` that exists, so the
//! line goes in that one. With none of them, init makes `~/.bash_profile`
//! with a line that also reads `~/.profile`, so a `~/.profile` another tool
//! writes later is still read.
//!
//! Completion follows the same rules: for bash, zsh, and fish, a line that
//! loads `switchboard completion SHELL` goes at the end of the same startup
//! file, never twice, and only when asked on a terminal.

use std::io::Write;

use serde_json::{Map, Value};

use super::super::ui::prompt::{Confirm, Problem, Ui};
use super::{Fail, Options};

/// A shell's startup file and the line that puts `~/bin` on PATH in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Startup {
    /// The file's absolute path.
    pub file: String,
    /// The file as a person reads it (`~/.zshrc`).
    pub shown: String,
    pub line: String,
    /// Written before the line when the file is made: bash stops reading
    /// `~/.profile` once `~/.bash_profile` exists, so a new one reads it.
    pub preamble: Option<&'static str>,
}

const POSIX_LINE: &str = "export PATH=\"$HOME/bin:$PATH\"";
const FISH_LINE: &str = "set -gx PATH $HOME/bin $PATH";
const READ_PROFILE: &str = "[ -r \"$HOME/.profile\" ] && . \"$HOME/.profile\"";

/// Whether `path` exists (through links) for a shell to read.
fn readable(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

/// The file a macOS bash login shell reads: the first of `.bash_profile`,
/// `.bash_login`, and `.profile` that exists, else a new `.bash_profile`.
fn bash_login_file(home: &str) -> (String, Option<&'static str>) {
    for name in [".bash_profile", ".bash_login", ".profile"] {
        let file = format!("{home}/{name}");
        if readable(&file) {
            return (file, None);
        }
    }
    (format!("{home}/.bash_profile"), Some(READ_PROFILE))
}

/// The startup file for `shell` (a path or name, as in `SHELL`).
pub fn startup_file(shell: &str, home: &str, zdotdir: Option<&str>, macos: bool) -> Startup {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    let home = home.trim_end_matches('/');
    let mut preamble = None;
    let (file, line) = match name {
        "zsh" => (
            format!(
                "{}/.zshrc",
                zdotdir
                    .filter(|d| !d.is_empty())
                    .map(|d| d.trim_end_matches('/'))
                    .unwrap_or(home)
            ),
            POSIX_LINE,
        ),
        // Terminal windows on macOS start login shells (see the module
        // comment); elsewhere they read .bashrc.
        "bash" if macos => {
            let (file, first) = bash_login_file(home);
            preamble = first;
            (file, POSIX_LINE)
        }
        "bash" => (format!("{home}/.bashrc"), POSIX_LINE),
        "fish" => (format!("{home}/.config/fish/config.fish"), FISH_LINE),
        _ => (format!("{home}/.profile"), POSIX_LINE),
    };
    let shown = match file.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => file.clone(),
    };
    Startup {
        file,
        shown,
        line: line.to_string(),
        preamble,
    }
}

/// Whether a startup file already puts `~/bin` on PATH (outside comments).
pub fn already_adds(contents: &str, home: &str) -> bool {
    let home_bin = format!("{}/bin", home.trim_end_matches('/'));
    let needles = ["$HOME/bin", "${HOME}/bin", "~/bin", home_bin.as_str()];
    contents.lines().any(|line| {
        let line = line.trim();
        // PATH itself (not GOPATH), zsh's path array, or fish_add_path.
        let sets_path = line
            .match_indices("PATH")
            .any(|(at, _)| !line[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
            || line.contains("path=(")
            || line.contains("path+=(")
            || line.contains("fish_add_path");
        if line.starts_with('#') || !sets_path {
            return false;
        }
        needles.iter().any(|needle| {
            line.match_indices(needle).any(|(at, _)| {
                // `~/bin` itself, not `~/bin2` or `~/bin/tools`.
                !line[at + needle.len()..]
                    .starts_with(|c: char| c.is_alphanumeric() || "/_-.".contains(c))
            })
        })
    })
}

/// Append the line to the startup file (made if missing, with its
/// preamble), on a line of its own with a comment that says where it came
/// from. A link is followed: the file it points to gets the line.
pub fn append(startup: &Startup) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(&startup.file).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let made = !readable(&startup.file);
    let existing = std::fs::read(&startup.file).unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&startup.file)?;
    let mut text = String::new();
    if !existing.is_empty() {
        if !existing.ends_with(b"\n") {
            text.push('\n');
        }
        text.push('\n');
    }
    text.push_str("# Added by switchboard init\n");
    if let (true, Some(preamble)) = (made, startup.preamble) {
        text.push_str(preamble);
        text.push('\n');
    }
    text.push_str(&startup.line);
    text.push('\n');
    file.write_all(text.as_bytes())
}

/// Offer to put `~/bin` on PATH when `switchboard` doesn't run by name,
/// then to turn on Tab completion.
pub fn offer(ui: &mut Ui, options: &Options, summary: &mut Map<String, Value>) -> Result<(), Fail> {
    offer_path(ui, options, summary)?;
    offer_completion(ui, options, summary)
}

/// The startup file and line that would load completion for `shell`, or
/// `None` when the shell has none or the file already loads it.
pub fn completion_plan(
    shell: &str,
    home: &str,
    zdotdir: Option<&str>,
    macos: bool,
) -> Option<Startup> {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    let line = super::super::completion::startup_line(name)?;
    let startup = startup_file(shell, home, zdotdir, macos);
    let contents =
        String::from_utf8_lossy(&std::fs::read(&startup.file).unwrap_or_default()).into_owned();
    if super::super::completion::already_loads(&contents) {
        return None;
    }
    Some(Startup {
        line: line.to_string(),
        ..startup
    })
}

/// Offer to turn on Tab completion for switchboard (bash, zsh, and fish).
fn offer_completion(
    ui: &mut Ui,
    options: &Options,
    summary: &mut Map<String, Value>,
) -> Result<(), Fail> {
    let home = super::super::local::home_dir();
    let shell = std::env::var("SHELL").unwrap_or_default();
    let Some(startup) = completion_plan(
        &shell,
        &home,
        std::env::var("ZDOTDIR").ok().as_deref(),
        cfg!(target_os = "macos"),
    ) else {
        return Ok(());
    };
    // Only on a terminal, and only when asked: never with --yes.
    if !ui.interactive() || options.yes {
        return Ok(());
    }
    let add = ui.confirm(&Confirm {
        question: &format!("Turn on Tab completion for switchboard in {}?", startup.shown),
        yes: "Add it",
        yes_detail: &format!(
            "Adds a line that loads `switchboard completion` to the end of {}, so Tab completes commands and chat names in new terminals.",
            startup.shown
        ),
        no: "Not now",
        no_detail: "`switchboard completion --help` says how to add it later.",
        default_yes: true,
        flag: "",
        declined: Some("Left Tab completion off"),
    })?;
    if !add {
        summary.insert("completion".into(), Value::from("declined"));
        return Ok(());
    }
    match append(&startup) {
        Ok(()) => {
            summary.insert("completion".into(), Value::from("added"));
            ui.done(&format!(
                "Turned on Tab completion in `{}`; new terminals have it",
                startup.shown
            ));
        }
        Err(error) => {
            summary.insert("completion".into(), Value::from("failed"));
            ui.caution(
                &Problem::new(format!("Couldn't add Tab completion to {}.", startup.shown))
                    .detail(format!("{error}."))
                    .fix(format!("add this line to it yourself: `{}`", startup.line)),
            );
        }
    }
    Ok(())
}

/// Offer to put `~/bin` on PATH when `switchboard` doesn't run by name.
fn offer_path(
    ui: &mut Ui,
    options: &Options,
    summary: &mut Map<String, Value>,
) -> Result<(), Fail> {
    if super::command_on_path() {
        return Ok(());
    }
    let home = super::super::local::home_dir();
    let startup = startup_file(
        &std::env::var("SHELL").unwrap_or_default(),
        &home,
        std::env::var("ZDOTDIR").ok().as_deref(),
        cfg!(target_os = "macos"),
    );
    // Not UTF-8 is still read, so a line already there is never added again.
    let contents =
        String::from_utf8_lossy(&std::fs::read(&startup.file).unwrap_or_default()).into_owned();
    let record = |summary: &mut Map<String, Value>, state: &str| {
        summary.insert("path".into(), Value::from(state));
    };
    if already_adds(&contents, &home) {
        record(summary, "in_startup_file");
        ui.done(&format!(
            "Your {} already puts ~/bin on your PATH, so new terminals find switchboard",
            startup.shown
        ));
        return Ok(());
    }
    if !ui.interactive() || options.yes {
        record(summary, "missing");
        ui.warn(&format!(
            "~/bin isn't on your PATH, so run `~/bin/switchboard`, or add this line to {}:\n`{}`",
            startup.shown, startup.line
        ));
        return Ok(());
    }
    let detail = if startup.preamble.is_some() && !readable(&startup.file) {
        format!(
            "Makes {} with `{}`, and a line that reads ~/.profile (bash skips ~/.profile once {} exists), so new terminals find `switchboard`.",
            startup.shown, startup.line, startup.shown
        )
    } else {
        format!(
            "Adds `{}` to the end of {}, so new terminals find `switchboard`.",
            startup.line, startup.shown
        )
    };
    let add = ui.confirm(&Confirm {
        question: &format!("Add ~/bin to your PATH in {}?", startup.shown),
        yes: "Add it",
        yes_detail: &detail,
        no: "Not now",
        no_detail: "Run it as `~/bin/switchboard` instead.",
        default_yes: true,
        flag: "",
        declined: Some("Left your PATH as it is"),
    })?;
    if !add {
        record(summary, "declined");
        return Ok(());
    }
    match append(&startup) {
        Ok(()) => {
            record(summary, "added");
            ui.done(&format!(
                "Added `~/bin` to your PATH in `{}`; new terminals find switchboard",
                startup.shown
            ));
        }
        Err(error) => {
            record(summary, "failed");
            ui.caution(
                &Problem::new(format!("Couldn't add ~/bin to {}.", startup.shown))
                    .detail(format!("{error}."))
                    .fix(format!("add this line to it yourself: `{}`", startup.line)),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_shell_gets_its_startup_file() {
        let zsh = startup_file("/bin/zsh", "/Users/k", None, true);
        assert_eq!(
            (zsh.file.as_str(), zsh.shown.as_str(), zsh.line.as_str()),
            (
                "/Users/k/.zshrc",
                "~/.zshrc",
                "export PATH=\"$HOME/bin:$PATH\""
            )
        );
        assert_eq!(
            startup_file("zsh", "/Users/k", Some("/Users/k/.config/zsh"), true).shown,
            "~/.config/zsh/.zshrc"
        );
        assert_eq!(
            startup_file("/bin/bash", "/Users/k", None, true).shown,
            "~/.bash_profile"
        );
        assert_eq!(
            startup_file("/usr/bin/bash", "/home/k", None, false).shown,
            "~/.bashrc"
        );
        let fish = startup_file("/usr/bin/fish", "/home/k", None, false);
        assert_eq!(
            (fish.shown.as_str(), fish.line.as_str()),
            ("~/.config/fish/config.fish", "set -gx PATH $HOME/bin $PATH")
        );
        assert_eq!(startup_file("", "/home/k", None, false).shown, "~/.profile");
        assert_eq!(
            startup_file("/bin/dash", "/home/k", None, false).shown,
            "~/.profile"
        );
    }

    #[test]
    fn an_existing_line_is_never_added_twice() {
        let home = "/home/k";
        assert!(already_adds("export PATH=\"$HOME/bin:$PATH\"\n", home));
        assert!(already_adds("PATH=~/bin:$PATH", home));
        assert!(already_adds("export PATH=${HOME}/bin:$PATH", home));
        assert!(already_adds("export PATH=/home/k/bin:$PATH", home));
        assert!(already_adds("set -gx PATH $HOME/bin $PATH", home));
        assert!(already_adds("fish_add_path ~/bin", home));
        assert!(already_adds("path=(~/bin $path)", home));
        assert!(!already_adds("export GOPATH=$HOME/bin", home));
        assert!(!already_adds("PYTHONPATH=~/bin", home));
        assert!(!already_adds("# export PATH=\"$HOME/bin:$PATH\"", home));
        assert!(!already_adds("export PATH=\"$HOME/bin2:$PATH\"", home));
        assert!(!already_adds("export PATH=\"$HOME/bin/tools:$PATH\"", home));
        assert!(!already_adds("alias b=~/bin/thing", home));
        assert!(!already_adds("", home));
    }

    #[test]
    fn the_line_is_appended_on_its_own_line() {
        let dir = std::env::temp_dir().join(format!("sbpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let startup = Startup {
            file: dir.join("fish/config.fish").to_string_lossy().into_owned(),
            shown: "config.fish".into(),
            line: FISH_LINE.into(),
            preamble: None,
        };
        append(&startup).unwrap();
        assert_eq!(
            std::fs::read_to_string(&startup.file).unwrap(),
            "# Added by switchboard init\nset -gx PATH $HOME/bin $PATH\n"
        );
        let posix = Startup {
            file: dir.join(".zshrc").to_string_lossy().into_owned(),
            shown: "~/.zshrc".into(),
            line: POSIX_LINE.into(),
            preamble: None,
        };
        std::fs::write(&posix.file, "alias ll='ls -l'").unwrap();
        append(&posix).unwrap();
        let text = std::fs::read_to_string(&posix.file).unwrap();
        assert_eq!(
            text,
            "alias ll='ls -l'\n\n# Added by switchboard init\nexport PATH=\"$HOME/bin:$PATH\"\n"
        );
        assert!(already_adds(&text, "/nowhere"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A private directory under /tmp for one test, removed on drop.
    struct Home(std::path::PathBuf);

    impl Home {
        fn new(tag: &str) -> Home {
            let dir = std::env::temp_dir().join(format!("sbpath-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Home(dir)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn file(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn macos_bash_gets_the_file_its_login_shell_reads() {
        let home = Home::new("bash");
        let chosen = |home: &Home| startup_file("/bin/bash", home.path(), None, true);
        // Only ~/.profile: a new ~/.bash_profile would make bash skip it.
        std::fs::write(home.file(".profile"), "export EDITOR=vi\n").unwrap();
        let startup = chosen(&home);
        assert_eq!(
            (startup.shown.as_str(), startup.preamble),
            ("~/.profile", None)
        );
        append(&startup).unwrap();
        assert!(!std::path::Path::new(&home.file(".bash_profile")).exists());
        assert_eq!(
            std::fs::read_to_string(home.file(".profile")).unwrap(),
            "export EDITOR=vi\n\n# Added by switchboard init\nexport PATH=\"$HOME/bin:$PATH\"\n"
        );
        // ~/.bash_login comes before ~/.profile, ~/.bash_profile before both.
        std::fs::write(home.file(".bash_login"), "").unwrap();
        assert_eq!(chosen(&home).shown, "~/.bash_login");
        std::fs::write(home.file(".bash_profile"), "").unwrap();
        assert_eq!(chosen(&home).shown, "~/.bash_profile");
        // A link counts as the file it points to, and the line goes there.
        let linked = Home::new("bashlink");
        std::fs::write(home.file("dotfiles-profile"), "").unwrap();
        std::os::unix::fs::symlink(home.file("dotfiles-profile"), linked.file(".profile")).unwrap();
        let startup = chosen(&linked);
        assert_eq!(startup.shown, "~/.profile");
        append(&startup).unwrap();
        assert!(already_adds(
            &std::fs::read_to_string(home.file("dotfiles-profile")).unwrap(),
            "/nowhere"
        ));
        assert!(std::fs::symlink_metadata(linked.file(".profile"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn completion_goes_in_the_same_file_and_never_twice() {
        let home = Home::new("completion");
        let plan = completion_plan("/bin/zsh", home.path(), None, true).unwrap();
        assert_eq!(plan.shown, "~/.zshrc");
        assert!(plan.line.contains("completion zsh"), "{}", plan.line);
        append(&plan).unwrap();
        // The PATH line can go in too; neither is added twice.
        append(&startup_file("/bin/zsh", home.path(), None, true)).unwrap();
        assert!(completion_plan("/bin/zsh", home.path(), None, true).is_none());
        let text = std::fs::read_to_string(home.file(".zshrc")).unwrap();
        assert!(already_adds(&text, home.path()));
        assert_eq!(text.matches("completion zsh").count(), 1);
        let fish = completion_plan("/usr/bin/fish", home.path(), None, false).unwrap();
        assert_eq!(fish.shown, "~/.config/fish/config.fish");
        assert!(fish.line.contains("completion fish | source"));
        assert!(completion_plan("/bin/bash", home.path(), None, false)
            .unwrap()
            .line
            .contains("completion bash"));
        assert!(completion_plan("/bin/dash", home.path(), None, false).is_none());
        assert!(completion_plan("", home.path(), None, false).is_none());
    }

    #[test]
    fn a_new_bash_profile_still_reads_profile() {
        let home = Home::new("fresh");
        let startup = startup_file("bash", home.path(), None, true);
        assert_eq!(startup.shown, "~/.bash_profile");
        append(&startup).unwrap();
        let text = std::fs::read_to_string(home.file(".bash_profile")).unwrap();
        assert_eq!(
            text,
            "# Added by switchboard init\n[ -r \"$HOME/.profile\" ] && . \"$HOME/.profile\"\nexport PATH=\"$HOME/bin:$PATH\"\n"
        );
        assert!(already_adds(&text, home.path()));
        // Appending to a file that exists adds only the PATH line.
        append(&startup).unwrap();
        let again = std::fs::read_to_string(home.file(".bash_profile")).unwrap();
        assert_eq!(again.matches(".profile\"").count(), 2);
        assert_eq!(again.matches("export PATH").count(), 2);
    }
}
