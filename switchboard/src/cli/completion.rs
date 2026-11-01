//! `switchboard completion bash|zsh|fish`: a script that completes the
//! commands, their options, and chat names with Tab.
//!
//! The scripts are made from one table (`COMMANDS`), so they list what the
//! command line accepts. Chat names come from `switchboard completion
//! --words chats` (the chats on other machines this machine's bridge shows
//! Claude Code, read from Claude Code's session files) and chat IDs from
//! `--words ids` (the chats open here); both read local files only, so Tab
//! never waits on the server.

use serde_json::Value;

use crate::fsutil;

/// A command, what it does, and its own options.
struct Command {
    name: &'static str,
    about: &'static str,
    options: &'static [&'static str],
}

const COMMANDS: &[Command] = &[
    Command {
        name: "init",
        about: "Set up this machine",
        options: &[
            "--invite",
            "--name",
            "--owner",
            "--plugin",
            "--no-plugin",
            "--no-start",
            "--replace",
            "--server",
            "--tls",
            "--domain",
            "--email",
            "--server-name",
            "--port",
            "--join",
            "--yes",
        ],
    },
    Command {
        name: "status",
        about: "Whether this machine is connected, and this chat",
        options: &["--chat"],
    },
    Command {
        name: "doctor",
        about: "Check everything, with the fix for each problem",
        options: &[],
    },
    Command {
        name: "join",
        about: "Join this chat",
        options: &["--chat"],
    },
    Command {
        name: "leave",
        about: "Leave this chat",
        options: &["--chat"],
    },
    Command {
        name: "chats",
        about: "List chats on this machine and others",
        options: &[
            "--local",
            "--remote",
            "--details",
            "--name",
            "--project",
            "--machine",
            "--chat",
        ],
    },
    Command {
        name: "ping",
        about: "Check that a chat on another machine is reachable",
        options: &[],
    },
    Command {
        name: "up",
        about: "Bring this machine back online",
        options: &[],
    },
    Command {
        name: "down",
        about: "Take this machine offline",
        options: &[],
    },
    Command {
        name: "logs",
        about: "Show what the bridge has been doing",
        options: &["--follow", "--lines"],
    },
    Command {
        name: "upgrade",
        about: "Install the latest release",
        options: &["--force"],
    },
    Command {
        name: "completion",
        about: "Print Tab completion for a shell",
        options: &[],
    },
    Command {
        name: "uninstall",
        about: "Remove Switchboard from this machine",
        options: &["--purge"],
    },
    Command {
        name: "help",
        about: "Show the commands, or one command's help",
        options: &[],
    },
];

/// Options every command takes.
const GLOBAL: [&str; 3] = ["--json", "--config", "--help"];

/// Options followed by a value of their own (skipped when looking for the
/// command, and completed with nothing but files or chats).
const VALUED: [&str; 14] = [
    "--config",
    "--chat",
    "--name",
    "--project",
    "--machine",
    "--lines",
    "--owner",
    "--tls",
    "--domain",
    "--email",
    "--server-name",
    "--port",
    "--session",
    "--words",
];

pub const SHELLS: [&str; 3] = ["bash", "zsh", "fish"];

fn names() -> String {
    COMMANDS
        .iter()
        .map(|c| c.name)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `case` arms giving each command's options (`$opts` in the scripts).
fn option_cases(indent: &str, close: &str) -> String {
    COMMANDS
        .iter()
        .filter(|c| !c.options.is_empty())
        .map(|c| {
            format!(
                "{indent}{}) opts=\"{}\" {close}\n",
                c.name,
                c.options.join(" ")
            )
        })
        .collect()
}

fn bash() -> String {
    format!(
        r#"# Tab completion for switchboard (bash). Load it with:
#   eval "$(switchboard completion bash)"
_switchboard_run() {{
    local program=${{COMP_WORDS[0]}}
    case $program in "~/"*) program="$HOME/${{program#\~/}}" ;; esac
    "$program" completion --words "$1" 2>/dev/null
}}
_switchboard() {{
    local cur=${{COMP_WORDS[COMP_CWORD]}} prev=${{COMP_WORDS[COMP_CWORD-1]}}
    local command= skip= word i opts= words=
    for ((i = 1; i < COMP_CWORD; i++)); do
        word=${{COMP_WORDS[i]}}
        if [ -n "$skip" ]; then skip=; continue; fi
        case $word in
            {valued}) skip=1 ;;
            -*) ;;
            *) command=$word; break ;;
        esac
    done
    case $prev in
        --chat) COMPREPLY=($(compgen -W "$(_switchboard_run ids)" -- "$cur")); return ;;
        --config) COMPREPLY=($(compgen -f -- "$cur")); return ;;
        {valued}) COMPREPLY=(); return ;;
    esac
    case $command in
        "") words="{commands}" ;;
        ping) words=$(_switchboard_run chats) ;;
        completion) words="{shells}" ;;
        help) words="{commands}" ;;
    esac
    case $command in
{cases}    esac
    COMPREPLY=($(compgen -W "$words $opts {global}" -- "$cur"))
}}
complete -F _switchboard switchboard
"#,
        valued = VALUED.join("|"),
        commands = names(),
        shells = SHELLS.join(" "),
        cases = option_cases("        ", ";;"),
        global = GLOBAL.join(" "),
    )
}

fn zsh() -> String {
    let described: String = COMMANDS
        .iter()
        .map(|c| format!("    '{}:{}'\n", c.name, c.about.replace('\'', "")))
        .collect();
    format!(
        r#"# Tab completion for switchboard (zsh). Load it with:
#   eval "$(switchboard completion zsh)"
_switchboard_run() {{
    ${{~words[1]}} completion --words "$1" 2>/dev/null
}}
_switchboard() {{
    local command= skip= word prev=${{words[CURRENT-1]}} opts= i
    local -a found commands
    commands=(
{described}    )
    for (( i = 2; i < CURRENT; i++ )); do
        word=${{words[i]}}
        if [[ -n $skip ]]; then skip=; continue; fi
        case $word in
            {valued}) skip=1 ;;
            -*) ;;
            *) command=$word; break ;;
        esac
    done
    case $prev in
        --chat) found=(${{(f)"$(_switchboard_run ids)"}}); compadd -a found; return ;;
        --config) _files; return ;;
        {valued}) return ;;
    esac
    case $command in
{cases}    esac
    if [[ -z $command ]]; then
        _describe 'command' commands
    elif [[ $command == ping ]]; then
        found=(${{(f)"$(_switchboard_run chats)"}}); compadd -a found
    elif [[ $command == completion ]]; then
        compadd {shells}
    elif [[ $command == help ]]; then
        compadd {names}
    fi
    found=(${{=opts}} {global})
    compadd -a found
}}
if (( ! $+functions[compdef] )); then
    autoload -Uz compinit && compinit -i
fi
compdef _switchboard switchboard
"#,
        described = described,
        valued = VALUED.join("|"),
        cases = option_cases("        ", ";;"),
        shells = SHELLS.join(" "),
        names = names(),
        global = GLOBAL.join(" "),
    )
}

fn fish() -> String {
    let mut out = String::from(
        "# Tab completion for switchboard (fish). Load it with:\n#   switchboard completion fish | source\n",
    );
    out.push_str(&format!(
        r#"function __switchboard_command
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l skip 0
    for token in $tokens
        if test $skip = 1
            set skip 0
            continue
        end
        switch $token
            case {valued}
                set skip 1
            case '-*'
            case '*'
                echo $token
                return
        end
    end
end
function __switchboard_using
    set -l command (__switchboard_command)
    test "$command" = "$argv[1]"
end
function __switchboard_run
    set -l program (commandline -opc)[1]
    $program completion --words $argv[1] 2>/dev/null
end
complete -c switchboard -f
"#,
        valued = VALUED.join(" "),
    ));
    for command in COMMANDS {
        out.push_str(&format!(
            "complete -c switchboard -n 'test -z (__switchboard_command)' -a {} -d '{}'\n",
            command.name,
            command.about.replace('\'', "")
        ));
        for option in command.options {
            let valued = VALUED.contains(option);
            out.push_str(&format!(
                "complete -c switchboard -n '__switchboard_using {}' -l {}{}\n",
                command.name,
                &option[2..],
                if valued { " -x" } else { "" }
            ));
        }
    }
    out.push_str(
        "complete -c switchboard -n '__switchboard_using ping' -a '(__switchboard_run chats)'\n",
    );
    out.push_str(&format!(
        "complete -c switchboard -n '__switchboard_using completion' -a '{}'\n",
        SHELLS.join(" ")
    ));
    out.push_str(&format!(
        "complete -c switchboard -n '__switchboard_using help' -a '{}'\n",
        names()
    ));
    out.push_str("complete -c switchboard -l chat -x -a '(__switchboard_run ids)'\n");
    out.push_str("complete -c switchboard -l json -d 'Print the result as JSON'\n");
    out.push_str("complete -c switchboard -l config -r -F -d 'The bridge configuration'\n");
    out.push_str("complete -c switchboard -s h -l help -d 'Show the help'\n");
    out
}

/// The completion script for `shell`.
pub fn script(shell: &str) -> Option<String> {
    match shell {
        "bash" => Some(bash()),
        "zsh" => Some(zsh()),
        "fish" => Some(fish()),
        _ => None,
    }
}

/// Words to complete: `chats` (chats on other machines, as `ping` takes
/// them) or `ids` (the IDs of chats open here). Local files only; nothing
/// on failure.
pub fn words(kind: &str, config_path: &str) -> Vec<String> {
    let config = super::local::private_json(config_path).unwrap_or_default();
    match kind {
        "ids" => super::local::real_sessions(&config)
            .unwrap_or_default()
            .iter()
            .filter_map(|record| record.get("sessionId").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        "chats" => {
            let dir = match config.get("config_dir").and_then(Value::as_str) {
                Some(dir) => dir.to_string(),
                None => format!("{}/.claude", super::local::home_dir()),
            };
            let sessions = fsutil::join(&dir, "sessions");
            let mut found: Vec<String> = std::fs::read_dir(&sessions)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
                .filter_map(|entry| std::fs::read(entry.path()).ok())
                .filter_map(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .filter(|record| record.get("peerBridgeProxy") == Some(&Value::Bool(true)))
                .filter_map(|record| {
                    record
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .map(|name| {
                    name.strip_prefix("switchboard/")
                        .unwrap_or(&name)
                        .to_string()
                })
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "/_-.".contains(c))
                })
                .collect();
            found.sort();
            found.dedup();
            found
        }
        _ => Vec::new(),
    }
}

/// The line init adds to a shell's startup file to load the completion.
pub fn startup_line(shell: &str) -> Option<&'static str> {
    match shell {
        "zsh" => Some(
            "[ -x \"$HOME/.local/bin/switchboard\" ] && eval \"$(\"$HOME/.local/bin/switchboard\" completion zsh)\"",
        ),
        "bash" => Some(
            "[ -x \"$HOME/.local/bin/switchboard\" ] && eval \"$(\"$HOME/.local/bin/switchboard\" completion bash)\"",
        ),
        "fish" => Some(
            "test -x $HOME/.local/bin/switchboard; and $HOME/.local/bin/switchboard completion fish | source",
        ),
        _ => None,
    }
}

/// Whether a startup file already loads the completion (outside comments):
/// a line that runs `switchboard completion SHELL` (the command by any path,
/// quoted or not) and evaluates its output (`eval`, `source`, or `.`).
/// A line that only mentions the command, or writes the script to a file,
/// doesn't count, and neither does one that runs `~/bin/switchboard`, where
/// releases before 0.3.1 put the command (its guard skips it once the
/// command has moved, so init offers the new line).
pub fn already_loads(contents: &str) -> bool {
    contents.lines().any(loads_completion)
}

fn loads_completion(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with('#') {
        return false;
    }
    let runs = ["zsh", "bash", "fish"].iter().any(|shell| {
        let call = format!("completion {shell}");
        line.match_indices(&call).any(|(at, _)| {
            let after = &line[at + call.len()..];
            let ends = after.is_empty() || after.starts_with([' ', ')', '"', '\'', '|', ';', '`']);
            let before = line[..at].trim_end().trim_end_matches(['"', '\'']);
            let moved = [
                "$HOME/bin/switchboard",
                "${HOME}/bin/switchboard",
                "~/bin/switchboard",
            ]
            .iter()
            .any(|old| before.ends_with(old));
            ends && before.ends_with("switchboard") && !moved
        })
    });
    let words: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || "\"'$();&|`{}".contains(c))
        .filter(|w| !w.is_empty())
        .collect();
    let evaluates = words.iter().any(|w| matches!(*w, "eval" | "source" | "."))
        && !line.contains(" > ")
        && !line.contains(" >> ");
    runs && evaluates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_name_every_visible_command_and_its_options() {
        for shell in SHELLS {
            let text = script(shell).unwrap();
            for command in super::super::args::COMMANDS {
                let hidden = super::super::args::HIDDEN
                    .iter()
                    .any(|(old, _)| *old == command);
                if !hidden {
                    assert!(text.contains(command), "{shell} lacks {command}");
                }
            }
            for option in [
                "--details",
                "--follow",
                "--lines",
                "--purge",
                "--force",
                "--chat",
            ] {
                assert!(
                    text.contains(option.trim_start_matches('-')),
                    "{shell} lacks {option}"
                );
            }
            assert!(text.contains("completion --words"), "{shell}");
        }
        assert!(script("tcsh").is_none());
    }

    #[test]
    fn the_startup_line_is_never_added_twice() {
        assert!(already_loads(&format!(
            "x\n{}\n",
            startup_line("zsh").unwrap()
        )));
        assert!(already_loads(startup_line("fish").unwrap()));
        assert!(!already_loads("# eval \"$(switchboard completion zsh)\""));
        assert!(!already_loads("export PATH=\"$HOME/bin:$PATH\""));
        // Other ways of loading it count.
        for loads in [
            "eval \"$(switchboard completion zsh)\"",
            "source <(switchboard completion bash)",
            ". <(\"$HOME/.local/bin/switchboard\" completion bash)",
            ". <(/usr/local/bin/switchboard completion bash)",
            "  switchboard completion fish | source",
            "command -v switchboard >/dev/null && eval \"$(switchboard completion zsh)\"",
        ] {
            assert!(already_loads(loads), "{loads}");
        }
        // A line that only mentions it, or saves the script, doesn't.
        for other in [
            "echo \"run switchboard completion zsh to see the script\"",
            "alias sbc='switchboard completion'",
            "switchboard completion zsh > ~/.zfunc/_switchboard",
            "eval \"$(other-tool completion zsh)\" # switchboard",
            "source ~/switchboard/notes completion bash",
            // The line releases before 0.3.1 added, for a command that moved.
            "[ -x \"$HOME/bin/switchboard\" ] && eval \"$(\"$HOME/bin/switchboard\" completion zsh)\"",
            "test -x $HOME/bin/switchboard; and $HOME/bin/switchboard completion fish | source",
        ] {
            assert!(!already_loads(other), "{other}");
        }
        assert!(startup_line("dash").is_none());
    }
}
