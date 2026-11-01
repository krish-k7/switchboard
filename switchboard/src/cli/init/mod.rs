//! `switchboard init`: the setup wizard, from a fresh install to connected
//! chats.
//!
//! It asks what this machine should do (join a server, run the server, or
//! both), asks only what that needs, checks each thing before the step that
//! depends on it, and finishes with what to do next. Where only one of those
//! can work here (a Mac can't run the server), it doesn't ask. Every question
//! has a flag, so without a terminal (scripts, agents, `--json`) it never
//! asks: a missing answer stops it with the flag that gives it. Esc goes back
//! one question until something is made.
//!
//! Steps are safe to repeat: rerunning picks up where a run stopped, and a
//! machine that is already set up gets its current state and the actions
//! that make sense for it.
//!
//! Submodules: `join` (an invite to a running bridge), `server` (the server
//! here, through sudo once), `configured` (rerunning on a set-up machine),
//! `plugin` (the Claude Code plugin), `path` (offering to put ~/.local/bin on PATH),
//! and `system` (host facts: addresses, DNS, commands on PATH).

pub mod configured;
pub mod join;
pub mod path;
pub mod plugin;
pub mod server;
pub mod system;

use serde_json::{Map, Value};

use super::ui::prompt::{Choice, Confirm, Problem, Select, Stop, Ui};
use super::ui::{term, Theme, Tone};
use crate::wirejson;

/// The variable that names a directory for trying the server path without
/// root: the privileged step then runs a private test server as the current
/// user from that directory instead of installing the system service.
pub const STUB_VARIABLE: &str = "SWITCHBOARD_INIT_STUB_SERVER";

/// Set by install.sh, which has already shown the title.
pub const TITLE_SHOWN_VARIABLE: &str = "SWITCHBOARD_TITLE_SHOWN";

/// The width settled labels are padded to when joining: the widest,
/// "Machine name". Each group of answers pads to its own widest label, so
/// the column stays tight (`server::LABEL_WIDTH`, `configured::LABEL_WIDTH`).
pub const LABEL_WIDTH: usize = 12;

/// Every option, for suggestions.
const OPTIONS: [&str; 22] = [
    "--help",
    "--config",
    "--invite",
    "--join",
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
    "--yes",
    "--json",
    "--machine",
    "--color",
    "--privileged",
    "-h",
];

pub const HELP: &str = "Usage: switchboard init [options]

Set up Switchboard on this machine: join a server with an invite, run the
server here, or both. In a terminal it asks for what it needs. Every question
also has an option, so scripts and agents can run it without a terminal; then
it asks nothing and stops on a missing answer, naming the option. Running it
again is safe: it picks up where it stopped, or shows how this machine is set
up and what you can do next.

Join a server:
  --invite [-]         join with an invite, made on the server with sudo
                       switchboard server invite: read from standard input,
                       or asked for without echo in a terminal;
                       SWITCHBOARD_INVITE works too. Never on the command
                       line, where other users can see it
  --name NAME          this machine's name (default: its host name)
  --owner NAME         who it belongs to (default: your user name)
  --plugin             install the Claude Code plugin (/switchboard commands)
  --no-plugin          don't install it (the default without a terminal)
  --no-start           don't install or start the bridge (the background
                       service that connects this machine's chats)
  --replace            join the invite's server even if this machine is set
                       up for another one; the current setup is moved aside

Run the server here (Linux with systemd; sudo is used once):
  --server             run the server on this machine
  --tls MODE           private: for a private network (Tailscale or LAN),
                       with a certificate machines check through the invite;
                       acme: public, with a Let's Encrypt certificate for
                       --domain on port 443
  --domain NAME        acme: the server's public DNS name
  --email ADDRESS      acme: a contact for the certificate account (optional)
  --server-name NAME   private: a name or address machines use to reach the
                       server; repeat for more (default: its Tailscale and
                       network addresses and host name)
  --port PORT          the port to listen on (default: 443, or 8743 private)
  --join               also join this machine to the new server

  --config PATH        the bridge configuration (default:
                       ~/.config/switchboard/bridge.json, or SWITCHBOARD_CONFIG)
  --yes                take the default answer to every question (it never
                       edits your shell's startup file)
  --json               print the result as JSON, and ask nothing
  -h, --help           show this help
";

/// `--invite` (or `--invite -`): the invite comes from standard input,
/// `SWITCHBOARD_INVITE`, or a prompt, never from the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InviteArg {
    /// Read from standard input, or ask.
    Ask,
}

/// Why `--invite VALUE` is refused, and how to pass the invite instead.
pub const INLINE_INVITE: &str = "--invite doesn't take the invite on the command line, where other users can see it (in ps) and your shell keeps it in its history. Pass it on standard input (printf '%s\\n' \"$INVITE\" | switchboard init --invite), set SWITCHBOARD_INVITE, or run switchboard init --invite in a terminal and paste it";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tls {
    Private,
    Acme,
}

impl Tls {
    pub fn flag(self) -> &'static str {
        match self {
            Tls::Private => "private",
            Tls::Acme => "acme",
        }
    }
}

/// What this machine becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Join,
    Server,
    Both,
}

/// The parsed command line.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub config: String,
    pub invite: Option<InviteArg>,
    pub join: bool,
    pub name: Option<String>,
    pub owner: Option<String>,
    pub plugin: Option<bool>,
    pub no_start: bool,
    pub replace: bool,
    pub server: bool,
    pub tls: Option<Tls>,
    pub domain: Option<String>,
    pub email: Option<String>,
    pub server_names: Vec<String>,
    pub port: Option<u16>,
    pub yes: bool,
    pub json: bool,
    /// The server installation step, run by init itself through sudo.
    pub privileged: bool,
    /// `--color always|never` (passed to the privileged step).
    pub color: Option<bool>,
}

impl Options {
    /// The role the flags ask for, if they ask for one.
    pub fn role(&self) -> Option<Role> {
        match (self.server, self.join || self.invite.is_some()) {
            (true, true) => Some(Role::Both),
            (true, false) => Some(Role::Server),
            (false, true) => Some(Role::Join),
            (false, false) => None,
        }
    }
}

/// Parse `switchboard init`'s arguments; `Ok(None)` for `--help`.
pub fn parse(argv: &[String], default_config: &str) -> Result<Option<Options>, String> {
    let mut o = Options {
        config: default_config.to_string(),
        ..Options::default()
    };
    let mut index = 0;
    while index < argv.len() {
        let argument = argv[index].as_str();
        index += 1;
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (argument, None),
        };
        let value = |index: &mut usize| -> Result<String, String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            match argv.get(*index) {
                Some(next) if !next.starts_with("--") => {
                    *index += 1;
                    Ok(next.clone())
                }
                _ => Err(format!("{name} needs a value")),
            }
        };
        let flag = |inline: &Option<String>| -> Result<(), String> {
            match inline {
                Some(_) => Err(format!("{name} takes no value")),
                None => Ok(()),
            }
        };
        match name {
            "-h" | "--help" => return Ok(None),
            "--config" => o.config = crate::fsutil::normalize(&value(&mut index)?),
            "--invite" => {
                let given = match &inline {
                    Some(value) => Some(value.clone()),
                    None => match argv.get(index) {
                        Some(next) if next == "-" || !next.starts_with('-') => {
                            index += 1;
                            Some(next.clone())
                        }
                        _ => None,
                    },
                };
                if given.as_deref().is_some_and(|text| text != "-") {
                    return Err(INLINE_INVITE.into());
                }
                o.invite = Some(InviteArg::Ask);
            }
            "--join" => {
                flag(&inline)?;
                o.join = true;
            }
            "--name" | "--machine" => o.name = Some(value(&mut index)?),
            "--owner" => o.owner = Some(value(&mut index)?),
            "--plugin" | "--no-plugin" => {
                flag(&inline)?;
                let wanted = name == "--plugin";
                if o.plugin.is_some_and(|p| p != wanted) {
                    return Err("--plugin and --no-plugin can't be used together".into());
                }
                o.plugin = Some(wanted);
            }
            "--no-start" => {
                flag(&inline)?;
                o.no_start = true;
            }
            "--replace" => {
                flag(&inline)?;
                o.replace = true;
            }
            "--server" => {
                flag(&inline)?;
                o.server = true;
            }
            "--tls" => {
                o.tls = Some(match value(&mut index)?.as_str() {
                    "private" => Tls::Private,
                    "acme" => Tls::Acme,
                    other => {
                        return Err(format!(
                            "--tls must be private or acme, not '{other}' (for Caddy, use switchboard server install --tls caddy)"
                        ))
                    }
                });
            }
            "--domain" => o.domain = Some(value(&mut index)?.to_ascii_lowercase()),
            "--email" => o.email = Some(value(&mut index)?),
            "--server-name" => o.server_names.push(value(&mut index)?),
            "--port" => {
                let text = value(&mut index)?;
                o.port = Some(
                    text.parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or_else(|| format!("--port must be a port number, not '{text}'"))?,
                );
            }
            "--yes" | "-y" => {
                flag(&inline)?;
                o.yes = true;
            }
            "--json" => {
                flag(&inline)?;
                o.json = true;
            }
            "--privileged" => {
                flag(&inline)?;
                o.privileged = true;
            }
            "--color" => {
                o.color = match value(&mut index)?.as_str() {
                    "always" => Some(true),
                    "never" => Some(false),
                    "auto" => None,
                    other => {
                        return Err(format!(
                            "--color must be always, never, or auto, not '{other}'"
                        ))
                    }
                }
            }
            _ if argument.starts_with('-') => {
                return Err(match super::args::close_match(name, &OPTIONS) {
                    Some(closest) => format!("unknown option {name}; did you mean {closest}?"),
                    None => format!("unknown option {name}"),
                })
            }
            _ => return Err(format!("unexpected argument {argument}")),
        }
    }
    // Options that only make sense for the server imply it.
    if o.domain.is_some() && o.tls.is_none() {
        o.tls = Some(Tls::Acme);
    }
    if !o.server_names.is_empty() && o.tls.is_none() {
        o.tls = Some(Tls::Private);
    }
    if o.tls.is_some() || o.email.is_some() {
        o.server = true;
    }
    match o.tls {
        Some(Tls::Private) if o.domain.is_some() => {
            return Err("--domain is for --tls acme; a private server takes --server-name".into())
        }
        Some(Tls::Private) if o.email.is_some() => {
            return Err("--email only works with --tls acme".into())
        }
        Some(Tls::Acme) if !o.server_names.is_empty() => {
            return Err("--server-name is for --tls private; a public server takes --domain".into())
        }
        _ => {}
    }
    if o.port.is_some() && !o.server {
        return Err("--port is for running the server (--server)".into());
    }
    if o.server && !o.join && o.invite.is_some() {
        return Err(
            "--invite is for joining a server; with --server, --join joins the new one".into(),
        );
    }
    Ok(Some(o))
}

/// How a run ended without finishing.
#[derive(Debug)]
pub enum Fail {
    /// A problem already printed (by the step that failed).
    Shown(Problem),
    /// A problem still to print.
    Problem(Problem),
    Stop(Stop),
    /// The person chose not to go on; what that leaves (exit status 0).
    Declined(String),
}

impl From<Stop> for Fail {
    fn from(stop: Stop) -> Fail {
        Fail::Stop(stop)
    }
}

/// What init knows about this machine before it starts.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub macos: bool,
    pub linux: bool,
    pub systemd: bool,
    pub root: bool,
    /// `SUDO_USER`, when init runs under sudo.
    pub sudo_user: Option<String>,
    /// A bridge config exists.
    pub configured: bool,
    /// The server's unit is installed here.
    pub server_here: bool,
    /// The directory of the stand-in server, when trying init without root.
    pub stub: Option<String>,
}

impl Facts {
    pub fn gather(options: &Options) -> Facts {
        let stub = std::env::var(STUB_VARIABLE)
            .ok()
            .filter(|dir| !dir.is_empty());
        let linux = cfg!(target_os = "linux");
        Facts {
            macos: cfg!(target_os = "macos"),
            linux,
            systemd: linux && system::systemd(),
            root: system::is_root(),
            sudo_user: std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty()),
            configured: crate::fsutil::lstat(&options.config).is_ok(),
            server_here: linux
                && crate::fsutil::exists("/etc/systemd/system/switchboard-server.service"),
            stub,
        }
    }

    /// Why the server cannot run here, if it cannot.
    pub fn server_unavailable(&self) -> Option<String> {
        if self.stub.is_some() || (self.linux && self.systemd) {
            None
        } else if self.macos {
            Some("needs Linux with systemd; this is a Mac".into())
        } else if self.linux {
            Some("needs systemd, which isn't running here".into())
        } else {
            Some("needs Linux with systemd".into())
        }
    }

    /// When only one role can work here, it and the line that says why;
    /// init then doesn't ask.
    pub fn only_role(&self) -> Option<(Role, String)> {
        match (self.server_unavailable(), self.join_unavailable()) {
            (Some(_), None) => Some((
                Role::Join,
                if self.macos {
                    "This Mac will join a server. The server itself runs on Linux.".into()
                } else if self.linux {
                    "This machine will join a server. Running the server here needs systemd, which isn't running.".into()
                } else {
                    "This machine will join a server. The server itself runs on Linux.".into()
                },
            )),
            (None, Some(_)) => Some((
                Role::Server,
                "Under sudo, init sets up the server here. To connect this machine's chats too, run `switchboard init` again afterwards, without sudo.".into(),
            )),
            _ => None,
        }
    }

    /// Why this machine's chats cannot join from this run, if they cannot.
    pub fn join_unavailable(&self) -> Option<String> {
        match (&self.sudo_user, self.root) {
            (Some(user), true) if self.stub.is_none() => Some(format!(
                "run init as {user}, without sudo, so the bridge runs as you"
            )),
            _ => None,
        }
    }
}

/// The repository releases and the plugin come from.
pub fn repository() -> String {
    std::env::var("SWITCHBOARD_REPO")
        .ok()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "krish-k7/switchboard".into())
}

/// The one-line install command for another machine.
pub fn install_command() -> String {
    format!(
        "curl -fsSL https://raw.githubusercontent.com/{}/main/install.sh | sh",
        repository()
    )
}

fn default_config() -> String {
    match std::env::var("SWITCHBOARD_CONFIG") {
        Ok(value) if !value.is_empty() => crate::fsutil::normalize(&value),
        _ => crate::fsutil::normalize(&format!(
            "{}/.config/switchboard/bridge.json",
            super::local::home_dir()
        )),
    }
}

/// A usage error as a problem: what is wrong, and the fix (`help` names the
/// help command to point at).
pub fn usage_problem(message: &str, help: &str) -> Problem {
    let (head, rest) = match message.split_once("; ") {
        Some((head, rest)) => (head, Some(rest)),
        None => (message, None),
    };
    let mut chars = head.chars();
    let title = match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    };
    let fix = match rest.and_then(|r| r.strip_prefix("did you mean ")) {
        Some(suggestion) => format!(
            "use {}, or see `{help}` for the options.",
            suggestion.trim_end_matches('?')
        ),
        None => format!("see `{help}` for the options."),
    };
    Problem::new(title).fix(fix)
}

/// The title: the program and its version, and what it does.
pub fn title(ui: &mut Ui) {
    if std::env::var(TITLE_SHOWN_VARIABLE).is_ok_and(|v| v == "1") {
        // install.sh showed it, then its own steps: set the questions apart.
        ui.print(&[String::new()]);
        return;
    }
    ui.title(
        "Switchboard",
        crate::VERSION,
        "Connects Claude Code chats across your machines.",
    );
}

/// `switchboard init ...`.
pub fn main(argv: &[String]) -> i32 {
    let options = match parse(argv, &default_config()) {
        Ok(Some(options)) => options,
        Ok(None) => {
            super::print_help(HELP);
            return 0;
        }
        Err(message) => {
            let json = argv.iter().any(|a| a == "--json");
            if json {
                print_json(&error_value(&format!("switchboard init: {message}"), None));
            } else {
                let theme = Theme::for_fd(2);
                let lines = super::ui::prompt::problem_lines(
                    &theme,
                    &usage_problem(&message, "switchboard init --help"),
                    term::columns(),
                );
                super::err(&format!("{}\n", lines.join("\n")));
            }
            return 2;
        }
    };
    term::install_handlers();
    let interactive = !options.json && term::is_tty(0) && term::is_tty(2);
    let mut theme = Theme::for_fd(2);
    if let Some(color) = options.color {
        theme.color = color;
    }
    let mut ui = Ui::new(interactive, options.json, theme);
    ui.label_width = LABEL_WIDTH;
    if !options.json {
        let line = |text: &str| {
            format!(
                "\n{}\n",
                super::ui::para(&theme, text, Tone::Normal, ui.columns()).join("\n")
            )
        };
        term::set_stop_lines(line(STOPPED_BEFORE), line(STOPPED_AFTER));
    }
    let mut summary = Map::new();
    let outcome = run(&options, &mut ui, &mut summary);
    ui.finish();
    let code = match outcome {
        Ok(()) => {
            summary.insert("ok".into(), Value::Bool(true));
            0
        }
        Err(Fail::Shown(problem)) | Err(Fail::Problem(problem))
            if options.json || options.privileged =>
        {
            summary = error_value(&problem.sentence(), problem.fix.as_deref());
            1
        }
        Err(Fail::Shown(_)) => 1,
        Err(Fail::Problem(problem)) => {
            ui.problem(&problem);
            1
        }
        Err(Fail::Declined(message)) => {
            ui.blank();
            ui.say(&message, Tone::Normal);
            summary = error_value("declined", None);
            summary.insert("ok".into(), Value::Bool(true));
            0
        }
        Err(Fail::Stop(Stop::Cancelled)) | Err(Fail::Stop(Stop::Back)) => {
            ui.blank();
            ui.say(
                if term::changed() {
                    STOPPED_AFTER
                } else {
                    STOPPED_BEFORE
                },
                Tone::Normal,
            );
            summary = error_value("cancelled", None);
            130
        }
        Err(Fail::Stop(Stop::Missing(flag))) => {
            let problem = missing(&flag);
            if options.json || options.privileged {
                summary = error_value(&problem.sentence(), problem.fix.as_deref());
                summary.insert("missing".into(), Value::from(flag));
            } else {
                ui.problem(&problem);
            }
            2
        }
    };
    if options.json || options.privileged {
        if !summary.contains_key("ok") {
            summary.insert("ok".into(), Value::Bool(code == 0));
        }
        print_json(&summary);
    }
    code
}

/// The last step of a fix that ends with running init again.
pub const RERUN: &str = "Run\n`switchboard init`";

/// The same, for a run that resumes where it stopped.
pub const RERUN_RESUMES: &str = "Run\n`switchboard init`\nIt picks up where it stopped.";

/// What Ctrl-C leaves, before and after anything was made.
const STOPPED_BEFORE: &str = "Stopped. Nothing was changed on this machine.";
const STOPPED_AFTER: &str =
    "Stopped. Run `switchboard init` again to finish; it keeps what's done.";

/// The problem for a question with no terminal to ask it on.
pub fn missing(flag: &str) -> Problem {
    Problem::new(format!("Missing {flag}."))
        .detail("There is no terminal to ask on, so the answer has to come from the command line.")
        .fix(format!(
            "pass {flag}, or run `switchboard init` in a terminal."
        ))
}

fn error_value(message: &str, fix: Option<&str>) -> Map<String, Value> {
    let mut value = Map::new();
    value.insert("ok".into(), Value::Bool(false));
    value.insert("error".into(), Value::from(message));
    if let Some(fix) = fix {
        value.insert("fix".into(), Value::from(fix.replace('`', "")));
    }
    value
}

fn print_json(value: &Map<String, Value>) {
    super::out(&format!(
        "{}\n",
        crate::fmtutil::dumps_indent(&Value::Object(value.clone()), 2, false)
    ));
}

/// The whole run, after parsing.
pub fn run(options: &Options, ui: &mut Ui, summary: &mut Map<String, Value>) -> Result<(), Fail> {
    if options.privileged {
        return server::privileged(options, ui, summary);
    }
    let facts = Facts::gather(options);
    if ui.interactive() {
        title(ui);
    }
    summary.insert("action".into(), Value::from("init"));
    // A machine that is already set up shows where it stands, unless the
    // flags only ask for the server.
    if facts.configured && !(options.server && !options.join) {
        return configured::run(ui, options, &facts, summary);
    }
    let mut earlier = 0;
    loop {
        let start = ui.checkpoint();
        let (role, asked) = match options.role() {
            Some(role) => (role, false),
            None if std::env::var(join::INVITE_VARIABLE)
                .is_ok_and(|v| !crate::text::strip(&v).is_empty()) =>
            {
                (Role::Join, false)
            }
            None if !ui.interactive() => {
                return Err(Fail::Problem(
                    Problem::new("Nothing to set up: no invite and no server options.")
                        .detail("There is no terminal to ask what this machine should do.")
                        .fix("pass `--invite -` (with the invite on standard input) to join a server, or `--server --tls private` to run the server here."),
                ))
            }
            None => match facts.only_role() {
                Some((role, why)) => {
                    ui.say(&why, Tone::Normal);
                    ui.blank();
                    (role, false)
                }
                None if options.yes => (Role::Join, false),
                None => {
                    let role = ask_role(ui, earlier)?;
                    earlier = [Role::Join, Role::Server, Role::Both]
                        .iter()
                        .position(|r| *r == role)
                        .unwrap_or(0);
                    (role, true)
                }
            },
        };
        match run_role(ui, options, &facts, summary, role, asked) {
            Err(Fail::Stop(Stop::Back)) if asked => ui.rewind(start),
            other => return other,
        }
    }
}

fn run_role(
    ui: &mut Ui,
    options: &Options,
    facts: &Facts,
    summary: &mut Map<String, Value>,
    role: Role,
    asked: bool,
) -> Result<(), Fail> {
    if role != Role::Join {
        if let Some(reason) = facts.server_unavailable() {
            return Err(Fail::Problem(
                Problem::new("The server can't run on this machine.")
                    .detail(format!("It {reason}."))
                    .fix("run `switchboard init` on a Linux machine with systemd to set up the server there, then join this machine with an invite from it."),
            ));
        }
    }
    if role != Role::Server {
        if let Some(reason) = facts.join_unavailable() {
            return Err(Fail::Problem(
                Problem::new("This machine's chats can't join from a run under sudo.")
                    .detail(format!("To connect them, {reason}."))
                    .fix("run `switchboard init` without sudo; it uses sudo itself for the server step."),
            ));
        }
    }
    summary.insert(
        "role".into(),
        Value::from(match role {
            Role::Join => "join",
            Role::Server => "server",
            Role::Both => "both",
        }),
    );
    match role {
        Role::Join => {
            let joined = join::run(ui, options, facts, None, asked, summary)?;
            let plugin = plugin::offer(ui, options, summary)?;
            path::offer(ui, options, summary)?;
            finish_client(ui, &joined, plugin, None);
            Ok(())
        }
        Role::Server | Role::Both => {
            let server = server::run(ui, options, facts, asked, summary)?;
            let also_join = role == Role::Both
                || (ui.interactive()
                    && !options.yes
                    && facts.join_unavailable().is_none()
                    && ui.confirm(&Confirm {
                        question: "Also connect this machine's Claude Code chats?",
                        yes: "Join it too",
                        yes_detail: "Joins the new server from here, with its first invite.",
                        no: "Only run the server",
                        no_detail: "The first invite is shown next, for another machine.",
                        default_yes: plugin::claude_path().is_some(),
                        flag: "--join",
                        declined: None,
                    })?);
            match (&server.invite, also_join) {
                (Some(invite), true) => {
                    let parsed = crate::invite::Invite::parse(invite).map_err(|reason| {
                        Fail::Problem(Problem::new(format!(
                            "The server's first invite can't be read: {reason}"
                        )))
                    })?;
                    ui.blank();
                    let given = join::Given {
                        invite: parsed,
                        checked: false,
                    };
                    let joined = join::run(ui, options, facts, Some(given), false, summary)?;
                    let plugin = plugin::offer(ui, options, summary)?;
                    path::offer(ui, options, summary)?;
                    finish_client(ui, &joined, plugin, Some(&server));
                }
                _ => finish_server(ui, &server, summary),
            }
            Ok(())
        }
    }
}

fn ask_role(ui: &mut Ui, default: usize) -> Result<Role, Fail> {
    let chosen = ui.select(&Select {
        question: "What should this machine do?",
        label: "",
        choices: vec![
            Choice::new(
                "Join a server",
                "Connect this machine's Claude Code chats to a Switchboard server, with an invite from whoever runs it.",
            ),
            Choice::new(
                "Run the server",
                "The server carries messages between machines. You make an invite here for each machine that joins.",
            ),
            Choice::new(
                "Run the server and join it",
                "Run the server here, and connect this machine's chats to it too.",
            ),
        ],
        default,
        flag: "--invite or --server",
    })?;
    // The answer goes in the column of the group it starts.
    ui.label_width = if chosen == 0 {
        LABEL_WIDTH
    } else {
        server::LABEL_WIDTH
    };
    ui.answered(
        "Setup",
        [
            "Join a server",
            "Run the server",
            "Run the server and join it",
        ][chosen],
    );
    Ok([Role::Join, Role::Server, Role::Both][chosen])
}

/// How this machine joined, for the closing screen.
pub struct Joined {
    pub machine: String,
    pub server_url: String,
    pub started: bool,
    pub connected: bool,
}

/// The finished server, for the closing screen.
pub struct ServerDone {
    pub url: String,
    pub pin: Option<String>,
    pub invite: Option<String>,
    pub invite_expires_at: Option<i64>,
    /// Running as a stand-in for trying init without root.
    pub stub: bool,
}

pub(crate) fn on_path(directory: &str) -> bool {
    std::env::var("PATH").is_ok_and(|path| {
        path.split(':')
            .any(|entry| crate::fsutil::normalize(entry) == crate::fsutil::normalize(directory))
    })
}

/// Whether `switchboard` runs by name here (~/.local/bin, where it is
/// installed, is on PATH).
pub(crate) fn command_on_path() -> bool {
    let home = super::local::home_dir();
    on_path(&format!("{home}/.local/bin"))
}

/// The command a chat runs to join, with or without the plugin.
fn join_command(plugin: bool) -> String {
    if plugin {
        "/switchboard join".into()
    } else if command_on_path() {
        "!switchboard join".into()
    } else {
        "!~/.local/bin/switchboard join".into()
    }
}

/// `switchboard` as a person types it here.
fn switchboard() -> &'static str {
    if command_on_path() {
        "switchboard"
    } else {
        "~/.local/bin/switchboard"
    }
}

/// The closing screen after joining: the outcome, then what to do next,
/// in order.
pub(crate) fn finish_client(
    ui: &mut Ui,
    joined: &Joined,
    plugin: plugin::Plugin,
    server: Option<&ServerDone>,
) {
    let host = join::host_label(&joined.server_url);
    let machine = &joined.machine;
    ui.blank();
    let headline = match server {
        Some(_) => format!("Done. {machine} runs the server at {host} and is connected to it."),
        None if joined.connected => format!("Done. {machine} is connected to {host}."),
        None if joined.started => {
            format!("Almost done. {machine} joined {host}, but its bridge hasn't connected yet.")
        }
        None => format!("Done. {machine} joined {host}."),
    };
    ui.say(&headline, Tone::Strong);
    let command = switchboard();
    let mut next = Vec::new();
    if !joined.started {
        next.push(format!(
            "Start the bridge when you want this machine's chats connected:\n`{command} up`"
        ));
    } else if !joined.connected {
        next.push(format!("See what's in the way with\n`{command} doctor`"));
    }
    if plugin == plugin::Plugin::New {
        next.push(
            "In Claude Code chats that were already open, load the plugin with\n`/reload-plugins`"
                .into(),
        );
    }
    let join = join_command(plugin.installed());
    let mut chats = format!(
        "In each Claude Code chat you want connected, run\n`{join}`\nJoined chats can message each other. To choose the name other machines see, add it:\n`{join} research`"
    );
    if !plugin.installed() {
        chats.push_str("\nWithout the plugin, `!` runs the command in the chat's shell.");
    }
    next.push(chats);
    ui.heading("Next");
    ui.steps(&next);
    if let Some(server) = server {
        ui.heading("To add another machine");
        ui.steps(&[
            format!("Make an invite here:\n`{}`", more_invites(server.stub)),
            format!(
                "On that machine, run this, and paste the invite when it asks:\n`{}`",
                install_command()
            ),
        ]);
    }
}

/// The closing screen for the server alone: the invite, prominently.
fn finish_server(ui: &mut Ui, server: &ServerDone, summary: &mut Map<String, Value>) {
    ui.blank();
    ui.say(
        &format!(
            "Done. The server is running at {}.",
            join::host_label(&server.url)
        ),
        Tone::Strong,
    );
    match &server.invite {
        Some(invite) => {
            show_invite(ui, invite, server.invite_expires_at, true, server.stub);
            summary.insert("invite".into(), Value::from(invite.clone()));
        }
        None => {
            ui.heading("Next");
            ui.steps(&[
                "Make an invite for each machine that joins:\n`sudo switchboard server invite`"
                    .into(),
            ]);
        }
    }
}

/// How long until `at`, and when: `expires in 1 hour, at 15:11 PDT`.
pub fn expiry_text(at: i64, now: i64) -> String {
    format!(
        "expires in {}, at {}",
        crate::server::invites::span(at - now),
        system::local_time(at)
    )
}

/// What an invite is for, how to use it, and how to make more.
pub struct InviteText<'a> {
    pub invite: &'a str,
    /// What it is for: "the first machine", "a machine", "the machine kmbp".
    pub which: String,
    /// `single use`, `3 uses`.
    pub uses: String,
    pub expires_at: Option<i64>,
    /// The command for more invites, if the screen should say it.
    pub more: Option<&'a str>,
    /// Break the invite at the measure (on a terminal); otherwise it stays
    /// one line, for scripts.
    pub wrap: bool,
}

/// The lines that show an invite: a heading, how long it lasts, the invite
/// at the copy indent, and then what to do with it, in order.
pub fn invite_lines(theme: &Theme, text: &InviteText<'_>, columns: usize) -> Vec<String> {
    use super::ui::{heading, para, steps, token};
    let mut lines = Vec::new();
    let title = match text.which.as_str() {
        "the first machine" => "First invite".to_string(),
        "a machine" => "Invite".to_string(),
        which => format!(
            "Invite for {}",
            which.strip_prefix("the machine ").unwrap_or(which)
        ),
    };
    lines.extend(heading(theme, &title, columns));
    let expiry = text
        .expires_at
        .map(|at| format!(", {}", expiry_text(at, crate::server::invites::now())))
        .unwrap_or_default();
    let copy = if text.wrap {
        " Copy all of it; line breaks are fine."
    } else {
        ""
    };
    lines.extend(para(
        theme,
        &format!("{}{expiry}.{copy}", super::ui::capitalized(&text.uses)),
        Tone::Normal,
        columns,
    ));
    lines.extend(token(theme, text.invite, text.wrap, columns));
    lines.extend(para(
        theme,
        "Treat it like a password until it's used: anyone who has it can join one machine.",
        Tone::Normal,
        columns,
    ));
    lines.push(String::new());
    lines.extend(heading(theme, "Next", columns));
    let mut next = vec![format!(
        "On the machine you're adding, run this, and paste the invite when it asks:\n`{}`",
        install_command()
    )];
    if let Some(more) = text.more {
        next.push(format!(
            "Make a new invite for each machine after that:\n`{more}`"
        ));
    }
    lines.extend(steps(theme, &next, columns));
    lines
}

/// The command that makes more invites (in the sandbox, its test server's).
fn more_invites(stub: bool) -> &'static str {
    if stub {
        "switchboard server invite --config SANDBOX/server.json"
    } else {
        "sudo switchboard server invite"
    }
}

/// An invite made by init, with how to use it.
pub(crate) fn show_invite(
    ui: &mut Ui,
    invite: &str,
    expires_at: Option<i64>,
    first: bool,
    stub: bool,
) {
    ui.blank();
    let lines = invite_lines(
        &ui.theme,
        &InviteText {
            invite,
            which: if first {
                "the first machine".into()
            } else {
                "a machine".into()
            },
            uses: "single use".into(),
            expires_at,
            more: Some(more_invites(stub)),
            wrap: ui.interactive(),
        },
        ui.columns(),
    );
    ui.print(&lines);
}

/// `value` as JSON text (for tests and messages).
pub fn json_text(value: &Map<String, Value>) -> String {
    wirejson::dumps(&Value::Object(value.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn ok(list: &[&str]) -> Options {
        parse(&args(list), "/c/bridge.json").unwrap().unwrap()
    }

    fn err(list: &[&str]) -> String {
        parse(&args(list), "/c/bridge.json").unwrap_err()
    }

    #[test]
    fn invite_takes_an_optional_value() {
        assert_eq!(ok(&["--invite"]).invite, Some(InviteArg::Ask));
        assert_eq!(ok(&["--invite", "-"]).invite, Some(InviteArg::Ask));
        assert_eq!(ok(&["--invite=-"]).invite, Some(InviteArg::Ask));
        // Never the invite itself: other users would see it in ps.
        assert_eq!(err(&["--invite", "sbi1.x", "--yes"]), INLINE_INVITE);
        assert_eq!(err(&["--invite=sbi1.y"]), INLINE_INVITE);
        let o = ok(&["--invite", "--name", "box"]);
        assert_eq!(
            (o.invite, o.name.as_deref()),
            (Some(InviteArg::Ask), Some("box"))
        );
        assert_eq!(ok(&["--invite"]).role(), Some(Role::Join));
        assert_eq!(ok(&[]).role(), None);
    }

    #[test]
    fn server_options_imply_the_server_and_its_mode() {
        let o = ok(&["--domain", "SB.Example.com"]);
        assert_eq!(
            (o.server, o.tls, o.domain.as_deref()),
            (true, Some(Tls::Acme), Some("sb.example.com"))
        );
        let o = ok(&["--server-name", "box", "--server-name", "10.0.0.5"]);
        assert_eq!(
            (o.server, o.tls, o.server_names.len()),
            (true, Some(Tls::Private), 2)
        );
        assert_eq!(ok(&["--server", "--join"]).role(), Some(Role::Both));
        assert_eq!(ok(&["--tls", "private"]).role(), Some(Role::Server));
    }

    #[test]
    fn conflicts_are_usage_errors() {
        assert_eq!(err(&["--tls", "caddy"]), "--tls must be private or acme, not 'caddy' (for Caddy, use switchboard server install --tls caddy)");
        assert_eq!(
            err(&["--tls", "private", "--domain", "a.b"]),
            "--domain is for --tls acme; a private server takes --server-name"
        );
        assert_eq!(
            err(&["--plugin", "--no-plugin"]),
            "--plugin and --no-plugin can't be used together"
        );
        assert_eq!(
            err(&["--port", "8000"]),
            "--port is for running the server (--server)"
        );
        assert_eq!(
            err(&["--port", "x", "--server"]),
            "--port must be a port number, not 'x'"
        );
        assert_eq!(
            err(&["--server", "--invite"]),
            "--invite is for joining a server; with --server, --join joins the new one"
        );
        assert_eq!(err(&["--name"]), "--name needs a value");
        assert_eq!(err(&["--yes=1"]), "--yes takes no value");
        assert_eq!(err(&["--bogus"]), "unknown option --bogus");
        assert_eq!(
            err(&["--nmae", "x"]),
            "unknown option --nmae; did you mean --name?"
        );
        assert_eq!(err(&["extra"]), "unexpected argument extra");
        assert!(parse(&args(&["--help"]), "/c").unwrap().is_none());
    }

    #[test]
    fn help_fits_80_columns() {
        for line in HELP.lines() {
            assert!(line.chars().count() <= 80, "{line}");
        }
    }

    #[test]
    fn the_server_is_offered_only_where_it_can_run() {
        let mac = Facts {
            macos: true,
            ..Facts::default()
        };
        assert_eq!(
            mac.server_unavailable().as_deref(),
            Some("needs Linux with systemd; this is a Mac")
        );
        let linux = Facts {
            linux: true,
            systemd: true,
            ..Facts::default()
        };
        assert_eq!(linux.server_unavailable(), None);
        let container = Facts {
            linux: true,
            ..Facts::default()
        };
        assert!(container.server_unavailable().is_some());
        let stub = Facts {
            macos: true,
            stub: Some("/tmp/x".into()),
            ..Facts::default()
        };
        assert_eq!(stub.server_unavailable(), None);
        let sudo = Facts {
            root: true,
            sudo_user: Some("krish".into()),
            ..Facts::default()
        };
        assert!(sudo.join_unavailable().unwrap().contains("as krish"));
        let root_login = Facts {
            root: true,
            ..Facts::default()
        };
        assert_eq!(root_login.join_unavailable(), None);
    }

    #[test]
    fn only_one_possible_role_is_not_asked() {
        let mac = Facts {
            macos: true,
            ..Facts::default()
        };
        let (role, why) = mac.only_role().unwrap();
        assert_eq!(role, Role::Join);
        assert!(why.starts_with("This Mac will join a server."));
        let sudo = Facts {
            linux: true,
            systemd: true,
            root: true,
            sudo_user: Some("krish".into()),
            ..Facts::default()
        };
        assert_eq!(sudo.only_role().unwrap().0, Role::Server);
        let linux = Facts {
            linux: true,
            systemd: true,
            ..Facts::default()
        };
        assert!(linux.only_role().is_none());
    }

    #[test]
    fn usage_errors_read_as_problems() {
        let problem = usage_problem(
            "unknown option --nmae; did you mean --name?",
            "switchboard init --help",
        );
        assert_eq!(problem.title, "Unknown option --nmae.");
        assert_eq!(
            problem.fix.as_deref(),
            Some("use --name, or see `switchboard init --help` for the options.")
        );
        let problem = usage_problem("--name needs a value", "switchboard init --help");
        assert_eq!(problem.title, "--name needs a value.");
    }

    #[test]
    fn invites_show_their_expiry_and_wrap_on_a_terminal() {
        let invite = format!("sbi1.{}", "a".repeat(120));
        let lines = invite_lines(
            &Theme::plain(),
            &InviteText {
                invite: &invite,
                which: "the first machine".into(),
                uses: "single use".into(),
                expires_at: None,
                more: Some("sudo switchboard server invite"),
                wrap: true,
            },
            60,
        );
        assert_eq!(lines[0], "  First invite");
        assert_eq!(
            lines[1],
            "  Single use. Copy all of it; line breaks are fine."
        );
        let pieces: Vec<&String> = lines.iter().filter(|l| l.contains("aaaa")).collect();
        assert_eq!(pieces.len(), 3);
        assert!(pieces
            .iter()
            .all(|l| l.starts_with("      ") && l.chars().count() <= 59));
        assert_eq!(pieces.iter().map(|l| l.trim()).collect::<String>(), invite);
        // What to do with it, numbered, ends with making more.
        assert!(lines.iter().any(|l| l == "  Next"));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("  2  Make a new invite")));
        assert!(lines
            .iter()
            .any(|l| l == "       sudo switchboard server invite"));
        let one = invite_lines(
            &Theme::plain(),
            &InviteText {
                invite: &invite,
                which: "a machine".into(),
                uses: "single use".into(),
                expires_at: None,
                more: None,
                wrap: false,
            },
            60,
        );
        assert_eq!(one[0], "  Invite");
        assert!(one.contains(&format!("      {invite}")));
        assert!(!one.iter().any(|l| l.contains("Make a new invite")));
    }

    /// A run of init's questions with scripted answers, where the server can
    /// run (the stand-in) and nothing is set up yet.
    fn scripted(answers: Vec<super::super::ui::prompt::Answer>) -> (Result<(), Fail>, Vec<String>) {
        let dir = std::env::temp_dir().join(format!("sbinit-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(STUB_VARIABLE, &dir);
        std::env::remove_var(join::INVITE_VARIABLE);
        let options = Options {
            config: dir
                .join("absent/bridge.json")
                .to_string_lossy()
                .into_owned(),
            ..Options::default()
        };
        let mut ui = Ui::script(answers, 80);
        ui.label_width = LABEL_WIDTH;
        let outcome = run(&options, &mut ui, &mut Map::new());
        std::env::remove_var(STUB_VARIABLE);
        let _ = std::fs::remove_dir_all(&dir);
        (outcome, ui.transcript().to_vec())
    }

    #[test]
    fn esc_goes_back_one_question_and_erases_the_answer() {
        use super::super::ui::prompt::Answer;
        // Join, then Esc at the invite; the server, then Esc at its first
        // question; then Ctrl-C at the first question again.
        let (outcome, transcript) = scripted(vec![
            Answer::Enter,
            Answer::Back,
            Answer::Choose(1),
            Answer::Back,
            Answer::Cancel,
        ]);
        assert!(
            matches!(outcome, Err(Fail::Stop(Stop::Cancelled))),
            "{outcome:?}"
        );
        assert!(
            transcript.iter().all(|line| !line.contains("Setup")),
            "{transcript:?}"
        );
        // Declining the install says what that leaves, and settles nothing.
        let (outcome, transcript) = scripted(vec![
            Answer::Choose(1),
            Answer::Enter,
            Answer::Enter,
            Answer::Choose(1),
        ]);
        match outcome {
            Err(Fail::Declined(message)) => assert_eq!(message, "Nothing was installed."),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            transcript
                .iter()
                .filter(|l| l.starts_with('✓'))
                .cloned()
                .collect::<Vec<_>>(),
            [
                "✓ Setup      Run the server",
                "✓ Network    Private (Tailscale or LAN)",
                "✓ Addresses  localhost, 127.0.0.1"
            ]
        );
    }

    #[test]
    fn missing_answers_name_their_flag() {
        let problem = missing("--invite -");
        assert_eq!(problem.title, "Missing --invite -.");
        assert!(problem.fix.unwrap().contains("--invite -"));
    }
}
