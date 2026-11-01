//! Running the server here: the questions, the checks, and the install.
//!
//! The questions and checks run as the user. Installing needs root, so init
//! runs itself once through sudo (`switchboard init --privileged ...`): that
//! step draws its progress on the terminal and hands its result back as JSON
//! on standard output. Run as root, init installs directly.
//!
//! Public mode waits until Let's Encrypt has issued the certificate and the
//! server answers with it, reading the server's log for the CA's reason when
//! it fails (`acme_problem` turns the reason into a fix).
//!
//! With `SWITCHBOARD_INIT_STUB_SERVER=DIR` the privileged step is simulated
//! for trying init without root: a private test server runs as the current
//! user from DIR on 127.0.0.1, and nothing is installed system-wide.

use std::net::IpAddr;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::super::ui::prompt::{Choice, Confirm, Problem, Select, Settle, Stop, Text, Ui};
use super::super::ui::{term, Tone};
use super::{system, Facts, Fail, Options, ServerDone, Tls};
use crate::fsutil;
use crate::server::install as installer;
use crate::server::invites;

/// A private server's names, as typed: commas or spaces between them.
pub fn parse_names(text: &str) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for name in text.split([',', ' ']).filter(|n| !n.is_empty()) {
        let name = name.trim_matches(['[', ']']).to_ascii_lowercase();
        if !installer::is_dns_name(&name) && name.parse::<IpAddr>().is_err() {
            return Err(format!("{name} isn't a host name or an IP address."));
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return Err("Give at least one name or address machines can reach this server at.".into());
    }
    Ok(names)
}

/// A typed domain as a bare name: without a scheme, path, port, or
/// trailing dot, in lowercase.
pub fn clean_domain(text: &str) -> String {
    let name = text.trim();
    let name = name.split_once("://").map_or(name, |(_, rest)| rest);
    let name = name.split(['/', '?', '#']).next().unwrap_or("");
    let name = match name.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            host
        }
        _ => name,
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Why `text` cannot be the public domain (after `clean_domain`).
pub fn domain_check(text: &str) -> Result<(), String> {
    let name = clean_domain(text);
    if name.is_empty() {
        return Err("Type the name machines will use, such as switchboard.example.com.".into());
    }
    if name.parse::<IpAddr>().is_ok() {
        return Err("Let's Encrypt needs a domain name, not an IP address.".into());
    }
    if !name.contains('.') {
        return Err("Use the full name, such as switchboard.example.com.".into());
    }
    if !installer::is_dns_name(&name) {
        return Err("That isn't a valid domain name.".into());
    }
    Ok(())
}

/// The width the server's answers pad their labels to: the widest,
/// "Addresses".
pub const LABEL_WIDTH: usize = 9;

/// What Let's Encrypt's reason for not issuing means, and the fix.
pub fn acme_problem(detail: &str, domain: &str, port: u16) -> Problem {
    let lower = detail.to_ascii_lowercase();
    let rerun = "then run `switchboard init` again";
    let reason = || {
        // The CA's own words, without the library's framing.
        detail
            .split("API error: ")
            .nth(1)
            .unwrap_or(detail)
            .split(" (urn:")
            .next()
            .unwrap_or(detail)
            .trim()
            .to_string()
    };
    if lower.contains("caa") {
        return Problem::new(format!("A CAA record for {domain} doesn't allow Let's Encrypt."))
            .detail(reason())
            .fix(format!("add `0 issue \"letsencrypt.org\"` to the domain's CAA records (or remove them), {rerun}."));
    }
    if lower.contains("ratelimited") || lower.contains("rate limit") || lower.contains("too many") {
        return Problem::new(format!(
            "Let's Encrypt is rate limiting certificates for {domain}."
        ))
        .detail(reason())
        .fix(format!(
            "wait as long as it says, {rerun}. The server also keeps retrying on its own."
        ));
    }
    if lower.contains("nxdomain") || lower.contains("dns problem") || lower.contains("error:dns") {
        return Problem::new(format!("Let's Encrypt couldn't find {domain} in DNS."))
            .detail(reason())
            .fix(format!("create an A record (and AAAA for IPv6) for {domain} pointing at this machine, wait a few minutes for it to spread, {rerun}."));
    }
    if lower.contains("timeout during connect")
        || lower.contains("connection refused")
        || lower.contains("error:connection")
        || lower.contains("timeout after connect")
    {
        return Problem::new(format!("Let's Encrypt couldn't reach this machine on port {port}."))
            .detail(format!(
                "It connected to {domain} to check that this machine answers for it, and got: {}. If the domain has an AAAA (IPv6) record, that address must reach this machine too.",
                reason()
            ))
            .shown(format!(
                "It connected to {domain} to check that this machine answers for it, and got:\n> {}\nIf the domain has an AAAA (IPv6) record, that address must reach this machine too.",
                reason()
            ))
            .fix(format!(
                "allow incoming TCP on port {port} in this machine's firewall (for example `sudo ufw allow {port}/tcp`) and in your cloud provider's security group or router, {rerun}."
            ))
            .steps([
                format!("Allow incoming TCP on port {port} in this machine's firewall, for example\n`sudo ufw allow {port}/tcp`"),
                "Allow it in your cloud provider's security group or router, too.".to_string(),
                super::RERUN.to_string(),
            ]);
    }
    if lower.contains("error:tls")
        || lower.contains("incorrect validation certificate")
        || lower.contains("error:unauthorized")
    {
        return Problem::new(format!("Something else answered for {domain} on port {port}."))
            .detail(format!(
                "Let's Encrypt reached a server that isn't this one, such as a proxy, a CDN, or another web server: {}.",
                reason()
            ))
            .fix(format!(
                "point {domain} straight at this machine (turn off any proxy, such as Cloudflare's), {rerun}."
            ));
    }
    if lower.contains("error sending request")
        || lower.contains("dns error")
        || lower.contains("tcp connect")
        || lower.contains("connect error")
        || lower.contains("timed out")
    {
        return Problem::new("This machine can't reach Let's Encrypt.")
            .detail(detail.to_string())
            .fix(format!(
                "make sure this machine can make outgoing HTTPS connections (to acme-v02.api.letsencrypt.org), {rerun}."
            ));
    }
    Problem::new("Let's Encrypt didn't issue a certificate.")
        .detail(detail.to_string())
        .fix(format!(
            "see the server's log with `sudo switchboard server logs`, fix the cause, {rerun}."
        ))
        .steps([
            "See the server's log with\n`sudo switchboard server logs`",
            "Fix the cause.",
            super::RERUN,
        ])
}

/// The plain cause in an installer error, and how to fix it.
fn install_problem(message: &str) -> Problem {
    let again = "then run `switchboard init` again; it picks up where it stopped.";
    if let Some(path) = message
        .strip_prefix("refusing unmanaged unit: ")
        .or_else(|| message.strip_prefix("refusing unmanaged config: "))
        .or_else(|| message.strip_prefix("refusing unmanaged directory: "))
    {
        return Problem::new(format!("{path} is in the way."))
            .detail("It wasn't written by this installer, so it isn't replaced.")
            .fix(format!(
                "move it aside if it is not needed (`sudo mv {path} {path}.old`), {again}"
            ))
            .steps([
                format!("If it isn't needed, move it aside with\n`sudo mv {path} {path}.old`"),
                super::RERUN_RESUMES.to_string(),
            ]);
    }
    if message.contains("switchboard-hub.service") {
        return Problem::new("An older Switchboard server (switchboard-hub.service) is still set up here.")
            .fix(format!("stop and disable it with `sudo systemctl disable --now switchboard-hub.service`, {again}"));
    }
    if message.contains("did not become ready") {
        let lines: Vec<&str> = message
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let telling: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| {
                let lower = line.to_ascii_lowercase();
                lower.contains("error") || lower.contains("address") || lower.contains("failed")
            })
            .collect();
        let shown = if telling.is_empty() {
            lines
                .iter()
                .rev()
                .take(2)
                .rev()
                .copied()
                .collect::<Vec<_>>()
        } else {
            telling.into_iter().rev().take(2).rev().collect()
        };
        return Problem::new("The server started but didn't answer within 15 seconds.")
            .detail(shown.join(" "))
            .fix(format!(
                "see why with `sudo switchboard server logs` (often another program already uses the port), fix it, {again}"
            ))
            .steps([
                "See why with\n`sudo switchboard server logs`\nOften another program already uses the port.",
                "Fix it.",
                super::RERUN_RESUMES,
            ]);
    }
    Problem::new("The server installation stopped.")
        .detail(message.to_string())
        .fix(format!("fix that, {again}"))
}

fn service_active() -> bool {
    system::which("systemctl").is_some_and(|systemctl| {
        system::run(
            &[
                systemctl,
                "is-active".into(),
                "--quiet".into(),
                installer::UNIT_NAME.into(),
            ],
            Duration::from_secs(5),
        )
        .is_ok_and(|(code, _, _)| code == 0)
    })
}

/// What the server will be: its mode and the names or domain.
struct Plan {
    tls: Tls,
    domain: Option<String>,
    names: Vec<String>,
    port: u16,
}

/// The mode, and whether it was asked for.
fn choose_mode(ui: &mut Ui, options: &Options, earlier: Tls) -> Result<(Tls, bool), Fail> {
    if let Some(tls) = options.tls {
        return Ok((tls, false));
    }
    if options.yes || !ui.interactive() {
        // The private mode needs no answers, so it is the default.
        return Ok((Tls::Private, false));
    }
    let chosen = ui.select(&Select {
        question: "Where will machines connect from?",
        label: "Network",
        choices: vec![
            Choice::new(
                "A private network (Tailscale or LAN)",
                "The server makes its own certificate, and each invite carries its fingerprint, so joining machines can check it. Listens on port 8743.",
            )
            .answer("Private (Tailscale or LAN)"),
            Choice::new(
                "Anywhere, with a public domain name",
                "Needs a DNS name pointing at this machine and port 443 open to the internet. The certificate comes from Let's Encrypt.",
            )
            .answer("Public, with a domain name"),
        ],
        default: if earlier == Tls::Acme { 1 } else { 0 },
        flag: "--tls private or --tls acme --domain NAME",
    })?;
    Ok(([Tls::Private, Tls::Acme][chosen], true))
}

/// The names machines reach a private server at, and whether they were
/// asked for.
fn choose_names(
    ui: &mut Ui,
    options: &Options,
    facts: &Facts,
) -> Result<(Vec<String>, bool), Fail> {
    if !options.server_names.is_empty() {
        let names = parse_names(&options.server_names.join(",")).map_err(|reason| {
            Fail::Problem(Problem::new("A --server-name can't be used.").detail(reason))
        })?;
        ui.answered("Addresses", &names.join(", "));
        return Ok((names, false));
    }
    let defaults = if facts.stub.is_some() {
        vec!["localhost".to_string(), "127.0.0.1".to_string()]
    } else {
        system::default_server_names()
    };
    if defaults.is_empty() && !ui.interactive() {
        return Err(Fail::Problem(
            Problem::new("No host name or address found for the server's certificate.")
                .fix("pass `--server-name NAME` with a name or address machines use to reach it."),
        ));
    }
    if options.yes || !ui.interactive() {
        ui.answered("Addresses", &defaults.join(", "));
        return Ok((defaults, false));
    }
    let check = |text: &str| parse_names(text).map(|_| ());
    let typed = ui.text(&Text {
        question: "Addresses machines use to reach this server",
        label: "Addresses",
        default: defaults.join(", "),
        hint: "Invites point at the first; the certificate covers them all. Separate them with commas.",
        check: &check,
        flag: "--server-name",
        filter: None,
    })?;
    Ok((parse_names(&typed).expect("checked"), true))
}

/// The public domain, and whether it was asked for.
fn choose_domain(ui: &mut Ui, options: &Options, retry: bool) -> Result<(String, bool), Fail> {
    if let (Some(domain), false) = (&options.domain, retry) {
        let domain = clean_domain(domain);
        domain_check(&domain).map_err(|reason| {
            Fail::Problem(
                Problem::new(format!("{domain} can't be the server's domain.")).detail(reason),
            )
        })?;
        ui.answered("Domain", &domain);
        return Ok((domain, false));
    }
    let lower = |c: char| c.to_ascii_lowercase();
    let typed = ui.text(&Text {
        question: "Domain name",
        label: "Domain",
        default: String::new(),
        hint: "The public name machines will connect to, such as switchboard.example.com. Its DNS record must point at this machine.",
        check: &domain_check,
        flag: "--domain",
        filter: Some(&lower),
    })?;
    Ok((clean_domain(&typed), true))
}

/// Look the domain up and compare it with this machine's addresses. Not
/// resolving stops (Let's Encrypt would fail); pointing elsewhere warns.
fn check_domain(ui: &mut Ui, options: &Options, mut domain: String) -> Result<String, Fail> {
    loop {
        let found = ui
            .step(
                &format!("Looking up {domain}"),
                |_| Ok(system::resolve(&domain)),
                |_| Settle::Quiet,
            )
            .map_err(Fail::Shown)?;
        match found {
            Ok(addresses) => {
                let local = system::local_addresses();
                match addresses.iter().find(|a| local.contains(a)) {
                    Some(here) => ui.done(&format!("{domain} points to this machine ({here})")),
                    None => ui.caution(
                        &Problem::new(format!("{domain} points to {}.", elsewhere(&addresses)))
                        .detail(
                            "That's expected behind NAT or a cloud load balancer that forwards port 443 here. Otherwise, fix the DNS record now, or Let's Encrypt will fail.",
                        ),
                    ),
                }
                return Ok(domain);
            }
            Err(reason) => {
                let why = if reason == system::NO_RECORD {
                    format!("No DNS record exists for {domain} yet.")
                } else {
                    format!("Looking it up failed: {reason}.")
                };
                let problem = Problem::new(format!("{domain} doesn't resolve to an address."))
                    .detail(format!("{why} Let's Encrypt has to reach this machine at that name."))
                    .fix(format!("add an A record (or AAAA for IPv6) for {domain} pointing at this machine's public address, then check again."))
                    .steps([format!("Add an A record (or AAAA for IPv6) for {domain} pointing at this machine's public address, then check again.")]);
                if !ui.interactive() || options.yes {
                    return Err(Fail::Problem(problem));
                }
                ui.problem(&problem);
                let next = ui.select(&Select {
                    question: "What next?",
                    label: "",
                    choices: vec![
                        Choice::new("Check again", "After adding or fixing the DNS record."),
                        Choice::new("Use a different domain", ""),
                        Choice::new("Stop here", "Nothing has been installed."),
                    ],
                    default: 0,
                    flag: "--domain",
                })?;
                match next {
                    0 => {}
                    1 => domain = choose_domain(ui, options, true)?.0,
                    _ => return Err(Fail::Declined("Stopped. Nothing was installed.".into())),
                }
            }
        }
    }
}

/// Where `domain` points when none of it is this machine, in words.
fn elsewhere(addresses: &[IpAddr]) -> String {
    let shown: Vec<String> = addresses.iter().map(IpAddr::to_string).collect();
    match shown.len() {
        1 => format!("{}, which isn't this machine's address", shown[0]),
        2 => format!(
            "{} and {}, neither of them this machine's",
            shown[0], shown[1]
        ),
        n => format!(
            "{} and {} other addresses, none of them this machine's",
            shown[0],
            n - 1
        ),
    }
}

/// Which question the server setup is on, for going back.
enum Stage {
    Mode,
    Where,
    Confirm,
}

/// The server's questions and checks, then the install (through sudo).
/// `back`: Esc at the first question asked goes back to the caller's.
pub fn run(
    ui: &mut Ui,
    options: &Options,
    facts: &Facts,
    back: bool,
    summary: &mut Map<String, Value>,
) -> Result<ServerDone, Fail> {
    ui.label_width = LABEL_WIDTH;
    let can_go_back = ui.can_go_back();
    let mut stage = Stage::Mode;
    let (mut at_mode, mut at_where) = (0, 0);
    let (mut mode_asked, mut where_asked) = (false, false);
    let mut plan = Plan {
        tls: Tls::Private,
        domain: None,
        names: Vec::new(),
        port: 0,
    };
    let root = facts.root || facts.stub.is_some();
    loop {
        match stage {
            Stage::Mode => {
                at_mode = ui.checkpoint();
                ui.back = back && can_go_back;
                let chosen = choose_mode(ui, options, plan.tls);
                ui.back = false;
                let (tls, asked) = chosen?;
                mode_asked = asked;
                plan.tls = tls;
                plan.port = options
                    .port
                    .unwrap_or(if tls == Tls::Acme { 443 } else { 8743 });
                stage = Stage::Where;
            }
            Stage::Where => {
                at_where = ui.checkpoint();
                ui.back = (mode_asked || back) && can_go_back;
                let chosen = match plan.tls {
                    Tls::Private => choose_names(ui, options, facts).map(|(names, asked)| {
                        plan.names = names;
                        plan.domain = None;
                        asked
                    }),
                    Tls::Acme => choose_domain(ui, options, false).map(|(domain, asked)| {
                        plan.domain = Some(domain);
                        plan.names = Vec::new();
                        asked
                    }),
                };
                ui.back = false;
                match chosen {
                    Ok(asked) => {
                        where_asked = asked;
                        if let Some(domain) = plan.domain.take() {
                            plan.domain = Some(check_domain(ui, options, domain)?);
                        }
                        stage = Stage::Confirm;
                    }
                    Err(Fail::Stop(Stop::Back)) if mode_asked => {
                        ui.rewind(at_mode);
                        stage = Stage::Mode;
                    }
                    Err(other) => return Err(other),
                }
            }
            Stage::Confirm => {
                if facts.stub.is_none() && system::port_in_use(plan.port) && !service_active() {
                    return Err(Fail::Problem(
                        Problem::new(format!("Port {} is already in use on this machine.", plan.port))
                            .detail("Another program (a web server, perhaps) is listening there, so the server couldn't start.")
                            .fix(format!(
                                "stop that program (see which with `sudo ss -ltnp 'sport = :{}'`), or pass `--port` with a free port.",
                                plan.port
                            ))
                            .steps([format!(
                                "Stop that program, or pass `--port` with a free port. See which program it is with\n`sudo ss -ltnp 'sport = :{}'`",
                                plan.port
                            )]),
                    ));
                }
                if !ui.interactive() || options.yes {
                    break;
                }
                ui.back = (where_asked || mode_asked || back) && can_go_back;
                let detail = if root {
                    "Installs the server as a system service that starts at boot."
                } else {
                    "Installs the server as a system service that starts at boot. Uses sudo once, which may ask for your password."
                };
                let chosen = ui.confirm(&Confirm {
                    question: "Install the server now?",
                    yes: "Install it",
                    yes_detail: detail,
                    no: "Not now",
                    no_detail: "Nothing is installed.",
                    default_yes: true,
                    flag: "--yes",
                    declined: None,
                });
                ui.back = false;
                match chosen {
                    Ok(true) => break,
                    Ok(false) => return Err(Fail::Declined("Nothing was installed.".into())),
                    Err(Stop::Back) if where_asked => {
                        ui.rewind(at_where);
                        stage = Stage::Where;
                    }
                    Err(Stop::Back) if mode_asked => {
                        ui.rewind(at_mode);
                        stage = Stage::Mode;
                    }
                    Err(other) => return Err(Fail::Stop(other)),
                }
            }
        }
    }
    let done = if root {
        install_here(ui, options, &plan, facts.stub.as_deref())?
    } else {
        through_sudo(ui, options, &plan)?
    };
    summary.insert("server_url".into(), Value::from(done.url.clone()));
    summary.insert(
        "server_pin".into(),
        done.pin.clone().map_or(Value::Null, Value::from),
    );
    Ok(done)
}

fn privileged_arguments(options: &Options, plan: &Plan, color: bool) -> Vec<String> {
    let mut arguments: Vec<String> = vec![
        "init".into(),
        "--privileged".into(),
        "--tls".into(),
        plan.tls.flag().into(),
        "--port".into(),
        plan.port.to_string(),
        "--color".into(),
        if color { "always" } else { "never" }.into(),
    ];
    if let Some(domain) = &plan.domain {
        arguments.extend(["--domain".into(), domain.clone()]);
    }
    if let Some(email) = &options.email {
        arguments.extend(["--email".into(), email.clone()]);
    }
    for name in &plan.names {
        arguments.extend(["--server-name".into(), name.clone()]);
    }
    arguments
}

/// Run the install step as root through sudo, once.
fn through_sudo(ui: &mut Ui, options: &Options, plan: &Plan) -> Result<ServerDone, Fail> {
    let rerun = format!(
        "run it as root instead: `sudo switchboard init --server --tls {}{}`",
        plan.tls.flag(),
        plan.domain
            .as_ref()
            .map(|d| format!(" --domain {d}"))
            .unwrap_or_default()
    );
    let Some(sudo) = system::which("sudo") else {
        return Err(Fail::Problem(
            Problem::new("Installing the server needs root, and sudo isn't installed.")
                .fix(format!("{rerun} (or as root, without sudo).")),
        ));
    };
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|exc| {
            Fail::Problem(
                Problem::new("Can't find this program's own path.").detail(exc.to_string()),
            )
        })?;
    if ui.interactive() {
        ui.say(
            "Installing with sudo. It may ask for your password.",
            Tone::Normal,
        );
    } else {
        ui.say(
            "Installing with sudo (it needs root to create the service account and system files).",
            Tone::Normal,
        );
    }
    let mut command = Command::new(sudo);
    if !ui.interactive() {
        command.arg("-n");
    }
    command
        .arg("--")
        .arg(&exe)
        .args(privileged_arguments(options, plan, ui.theme.color))
        .stdin(if ui.interactive() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(if ui.prints() {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
    // A Ctrl-C reaches the step too; wait for it to finish and clean up.
    term::set_changed(true);
    let output = term::critical(|| command.spawn().and_then(|child| child.wait_with_output()))
        .map_err(|exc| {
            Fail::Problem(Problem::new("sudo couldn't be started.").detail(exc.to_string()))
        })?;
    let parsed: Option<Map<String, Value>> = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let Some(result) = parsed else {
        return Err(Fail::Problem(
            Problem::new("The install step didn't run.")
                .detail(format!(
                    "sudo exited with status {} before it started; nothing was installed.",
                    output.status.code().unwrap_or(1)
                ))
                .fix(format!("{rerun}.")),
        ));
    };
    if result.get("ok") != Some(&Value::Bool(true)) {
        let text = |key: &str| {
            result
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let mut problem = Problem::new(text("error"));
        if !text("fix").is_empty() {
            problem = problem.fix(text("fix"));
        }
        return Err(Fail::Shown(problem));
    }
    Ok(done_from(&result))
}

fn done_from(result: &Map<String, Value>) -> ServerDone {
    let text = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_string);
    ServerDone {
        url: text("server_url").unwrap_or_default(),
        pin: text("server_pin"),
        invite: text("invite"),
        invite_expires_at: result.get("invite_expires_at").and_then(Value::as_i64),
        stub: result.get("stub") == Some(&Value::Bool(true)),
    }
}

/// `init --privileged`: the install step, as root (or simulated), with its
/// result as JSON for the run that started it.
pub fn privileged(
    options: &Options,
    ui: &mut Ui,
    summary: &mut Map<String, Value>,
) -> Result<(), Fail> {
    let tls = options
        .tls
        .ok_or_else(|| Fail::Problem(super::missing("--tls")))?;
    let plan = Plan {
        tls,
        domain: options.domain.clone(),
        names: options.server_names.clone(),
        port: options
            .port
            .unwrap_or(if tls == Tls::Acme { 443 } else { 8743 }),
    };
    if tls == Tls::Acme && plan.domain.is_none() {
        return Err(Fail::Problem(super::missing("--domain")));
    }
    let stub = std::env::var(super::STUB_VARIABLE)
        .ok()
        .filter(|d| !d.is_empty());
    if stub.is_none() && !system::is_root() {
        return Err(Fail::Problem(
            Problem::new("The install step needs root.")
                .fix("run `sudo switchboard init --server`."),
        ));
    }
    let done = install_here(ui, options, &plan, stub.as_deref())?;
    summary.insert("server_url".into(), Value::from(done.url));
    summary.insert(
        "server_pin".into(),
        done.pin.map_or(Value::Null, Value::from),
    );
    summary.insert(
        "invite".into(),
        done.invite.map_or(Value::Null, Value::from),
    );
    summary.insert(
        "invite_expires_at".into(),
        done.invite_expires_at.map_or(Value::Null, Value::from),
    );
    summary.insert("stub".into(), Value::Bool(done.stub));
    Ok(())
}

fn short(pin: &str) -> String {
    format!("{}…", &pin[..pin.len().min(16)])
}

/// Install (as root) or simulate (with a stub directory).
fn install_here(
    ui: &mut Ui,
    options: &Options,
    plan: &Plan,
    stub: Option<&str>,
) -> Result<ServerDone, Fail> {
    if let Some(dir) = stub {
        return stub_install(ui, plan, dir);
    }
    let install_options = installer::Options {
        mode: Some(match plan.tls {
            Tls::Acme => installer::Mode::Acme,
            Tls::Private => installer::Mode::Private,
        }),
        domain: plan.domain.clone(),
        email: options.email.clone(),
        names: plan.names.clone(),
        port: options
            .port
            .or(Some(plan.port))
            .filter(|p| *p != if plan.tls == Tls::Acme { 443 } else { 8743 }),
        no_start: false,
        no_invite: false,
        json: true,
    };
    let began = invites::now();
    term::set_changed(true);
    let outcome = ui
        .step(
            "Installing the server",
            |_| {
                term::critical(|| installer::install(&install_options))
                    .map_err(|m| install_problem(&m))
            },
            |outcome| {
                Settle::Done(if outcome.changed.is_empty() {
                    "The server was already installed and is running".into()
                } else {
                    format!(
                        "Installed the server as a system service ({}) and started it",
                        installer::UNIT_NAME
                    )
                })
            },
        )
        .map_err(Fail::Shown)?;
    if outcome.reissued {
        ui.caution(
            &Problem::new("The server's certificate changed to cover the new names, so machines that joined before can't connect.")
                .fix("make a new invite for each of them (`sudo switchboard server invite`), then on each one run `switchboard init`, choose to join again, and paste it.")
                .steps([
                    "Make a new invite for each of them with\n`sudo switchboard server invite`",
                    "On each one, run this, choose to join again, and paste the invite:\n`switchboard init`",
                ]),
        );
    }
    match plan.tls {
        Tls::Acme => {
            let domain = plan.domain.clone().unwrap_or_default();
            wait_certificate(
                ui,
                &domain,
                plan.port,
                began,
                outcome.changed.is_empty(),
                None,
            )?;
            ui.done(&format!(
                "The server answers at {}",
                super::join::host_label(&outcome.hub_url)
            ));
        }
        Tls::Private => {
            let pin = outcome.pin.clone().unwrap_or_default();
            ui.done(&format!(
                "The server answers at {}, certificate SHA-256 {}",
                super::join::host_label(&outcome.hub_url),
                short(&pin)
            ));
        }
    }
    let (invite, expires) = match &outcome.invite {
        Some(created) => (created.token.clone(), created.entry.expires_at),
        None => {
            let local = invites::load(installer::CONFIG).map_err(invite_problem)?;
            let created = invites::create(
                &local,
                &invite_request(&outcome.hub_url, outcome.pin.as_deref()),
            )
            .map_err(invite_problem)?;
            (created.token, created.entry.expires_at)
        }
    };
    Ok(ServerDone {
        url: outcome.hub_url,
        pin: outcome.pin,
        invite: Some(invite),
        invite_expires_at: Some(expires),
        stub: false,
    })
}

fn invite_request<'a>(url: &'a str, pin: Option<&'a str>) -> invites::Request<'a> {
    invites::Request {
        url,
        pin,
        expires: invites::INSTALL_EXPIRY,
        uses: 1,
        machine: None,
        source: "init",
        replace: true,
    }
}

fn invite_problem(failure: crate::server::enroll_local::Failure) -> Fail {
    let message = match failure {
        crate::server::enroll_local::Failure::Reported(m)
        | crate::server::enroll_local::Failure::Other(m) => m,
    };
    Fail::Problem(
        Problem::new("The server is running, but no invite could be made.")
            .detail(message)
            .fix("make one with `sudo switchboard server invite`."),
    )
}

/// The newest `certificate_error` detail (or `certificate_obtained`) the
/// server logged since `since`.
fn log_events(since: i64, stub_log: Option<&str>) -> Vec<(String, String)> {
    let text = match stub_log {
        Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
        None => {
            let Some(journalctl) = system::which("journalctl") else {
                return Vec::new();
            };
            system::run(
                &[
                    journalctl,
                    "-u".into(),
                    installer::UNIT_NAME.into(),
                    "-o".into(),
                    "cat".into(),
                    "--no-pager".into(),
                    "--since".into(),
                    format!("@{since}"),
                ],
                Duration::from_secs(5),
            )
            .map(|(_, out, _)| out)
            .unwrap_or_default()
        }
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| {
            let event = value.get("event")?.as_str()?.to_string();
            let detail = value
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((event, detail))
        })
        .filter(|(event, _)| event.starts_with("certificate_"))
        .collect()
}

/// Whether the server on 127.0.0.1:`port` completes a handshake for
/// `domain` with a certificate this system trusts.
fn trusted_certificate(domain: &str, port: u16) -> bool {
    use std::sync::Arc;
    let Ok(name) = rustls::pki_types::ServerName::try_from(domain.to_string()) else {
        return false;
    };
    let Ok(key) = rcgen::KeyPair::generate() else {
        return false;
    };
    let Ok(cert) = rcgen::CertificateParams::new(vec!["switchboard-init-check".to_string()])
        .and_then(|params| params.self_signed(&key))
    else {
        return false;
    };
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = Arc::new(crate::hub_transport::system_roots());
    let Ok(config) = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map(|builder| builder.with_root_certificates(roots))
        .and_then(|builder| builder.with_client_auth_cert(vec![cert.der().clone()], key))
    else {
        return false;
    };
    let Ok(mut socket) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = socket.set_write_timeout(Some(Duration::from_secs(3)));
    let Ok(mut connection) = rustls::ClientConnection::new(Arc::new(config), name) else {
        return false;
    };
    while connection.is_handshaking() {
        if connection.complete_io(&mut socket).is_err() {
            return false;
        }
    }
    true
}

/// Wait until the server has its certificate from Let's Encrypt and
/// answers with it, or say why not.
fn wait_certificate(
    ui: &mut Ui,
    domain: &str,
    port: u16,
    since: i64,
    restart: bool,
    stub_log: Option<&str>,
) -> Result<(), Fail> {
    if stub_log.is_none() && restart && !trusted_certificate(domain, port) {
        // An earlier attempt failed and the server is waiting to retry:
        // restarting it tries again now.
        if let Some(systemctl) = system::which("systemctl") {
            let _ = system::run(
                &[systemctl, "restart".into(), installer::UNIT_NAME.into()],
                Duration::from_secs(30),
            );
        }
    }
    let running = format!("Getting a certificate for {domain} from Let's Encrypt");
    ui.step(
        &running,
        |progress| {
            let began = Instant::now();
            loop {
                if stub_log.is_none() && trusted_certificate(domain, port) {
                    return Ok(());
                }
                for (event, detail) in log_events(since, stub_log).into_iter().rev() {
                    match event.as_str() {
                        "certificate_error" => return Err(acme_problem(&detail, domain, port)),
                        "certificate_obtained" if stub_log.is_some() => return Ok(()),
                        _ => {}
                    }
                }
                if began.elapsed() > Duration::from_secs(150) {
                    return Err(Problem::new(format!("No certificate for {domain} after two and a half minutes.")).detail(
                        "The server keeps asking Let's Encrypt in the background.",
                    ).fix("watch its log with `sudo switchboard server logs --follow`; once it says certificate_obtained, run `switchboard init` again for the invite.")
                    .steps([
                        "Watch its log with\n`sudo switchboard server logs --follow`",
                        "Once it says certificate_obtained, run this again for the invite:\n`switchboard init`",
                    ]));
                }
                progress.set(format!("{running} ({} s)", began.elapsed().as_secs()));
                std::thread::sleep(Duration::from_millis(500));
            }
        },
        |_| Settle::Done(format!("Got a certificate for {domain} from Let's Encrypt")),
    )
    .map_err(Fail::Shown)
}

// The stand-in for trying init without root ----------------------------------

fn write_private(path: &str, data: &[u8]) -> Result<(), String> {
    crate::server::write_private(path, data)
}

fn private_directory(path: &str) -> Result<(), String> {
    fsutil::mkdir_parents(path, 0o700).map_err(|e| e.message)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())
}

fn alive(pid_file: &str) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // SAFETY: signal 0 only checks that the process exists.
    (unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
}

/// A private test server as this user from `dir`, in place of the install.
fn stub_install(ui: &mut Ui, plan: &Plan, dir: &str) -> Result<ServerDone, Fail> {
    ui.warn(&format!(
        "Sandbox: the root step is simulated. A private test server runs as you from {dir} on 127.0.0.1; nothing is installed system-wide."
    ));
    let dir = fsutil::absolute(dir).map_err(|e| {
        Fail::Problem(Problem::new("The sandbox directory can't be used.").detail(e.message))
    })?;
    let log = fsutil::join(&dir, "server.log");
    let since = invites::now();
    let mut names = if plan.tls == Tls::Acme {
        vec!["localhost".to_string()]
    } else {
        plan.names.clone()
    };
    for name in ["localhost", "127.0.0.1"] {
        if !names.iter().any(|n| n == name) {
            names.push(name.into());
        }
    }
    let config_path = fsutil::join(&dir, "server.json");
    let pid_file = fsutil::join(&dir, "server.pid");
    let started = ui
        .step(
            "Installing the server (simulated)",
            |_| {
                let fail = |m: String| Problem::new("The test server didn't start.").detail(m);
                let port = match alive(&pid_file) {
                    Some(_) => {
                        let text = std::fs::read_to_string(&config_path)
                            .map_err(|e| fail(e.to_string()))?;
                        serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.get("port").and_then(Value::as_u64))
                            .unwrap_or(0) as u16
                    }
                    None => {
                        private_directory(&dir).map_err(fail)?;
                        private_directory(&fsutil::join(&dir, "state")).map_err(fail)?;
                        let peers = fsutil::join(&dir, "peers.json");
                        if !fsutil::exists(&peers) {
                            write_private(&peers, b"{}\n").map_err(fail)?;
                        }
                        let port =
                            system::free_port().ok_or_else(|| fail("no free port".into()))?;
                        let config = serde_json::json!({
                            "tls": "private",
                            "listen": "127.0.0.1",
                            "port": port,
                            "names": names,
                            "db": "state/messages.sqlite3",
                            "peers_file": "peers.json",
                        });
                        write_private(&config_path, format!("{config}\n").as_bytes())
                            .map_err(fail)?;
                        let exe = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
                        let log_file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&log)
                            .map_err(|e| fail(e.to_string()))?;
                        let child = Command::new(exe)
                            .args(["server", "--config", &config_path])
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(log_file)
                            .process_group(0)
                            .spawn()
                            .map_err(|e| fail(e.to_string()))?;
                        write_private(&pid_file, format!("{}\n", child.id()).as_bytes())
                            .map_err(fail)?;
                        port
                    }
                };
                let certificate = fsutil::join(&dir, "state/tls/private-cert.pem");
                let began = Instant::now();
                while began.elapsed() < Duration::from_secs(15) {
                    if let Ok(der) = super::super::machines::fingerprint_file(&certificate) {
                        let pin = super::super::machines::hex(&der);
                        if installer::answers_tls(port, "localhost", Some(&pin)) {
                            return Ok((port, pin));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(fail(format!(
                    "it did not answer within 15 seconds; see {log}"
                )))
            },
            |(port, _)| {
                Settle::Done(format!(
                    "Started a test server on 127.0.0.1:{port} (simulated install)"
                ))
            },
        )
        .map_err(Fail::Shown)?;
    let (port, pin) = started;
    let url = format!(
        "https://{}:{port}",
        if names[0].contains(':') {
            format!("[{}]", names[0])
        } else {
            names[0].clone()
        }
    );
    if plan.tls == Tls::Acme {
        // Simulated: after a moment the log says what Let's Encrypt did.
        let domain = plan.domain.clone().unwrap_or_default();
        let error = std::env::var("SWITCHBOARD_INIT_STUB_ACME_ERROR")
            .ok()
            .filter(|e| !e.is_empty());
        let log_path = log.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let line = match error {
                Some(detail) => serde_json::json!({"event": "certificate_error", "detail": detail}),
                None => serde_json::json!({"event": "certificate_obtained", "detail": domain}),
            };
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                use std::io::Write;
                let _ = writeln!(file, "{line}");
            }
        });
        wait_certificate(
            ui,
            plan.domain.as_deref().unwrap_or(""),
            port,
            since,
            false,
            Some(&log),
        )?;
    }
    ui.done(&format!(
        "The server answers at {}, certificate SHA-256 {}",
        super::join::host_label(&url),
        short(&pin)
    ));
    let local = invites::Local {
        db: fsutil::join(&dir, "state/messages.sqlite3"),
        peers: fsutil::join(&dir, "peers.json"),
        lock: fsutil::join(&dir, "peers.json.lock"),
        url: Some(url.clone()),
        pin: Some(pin.clone()),
    };
    let created =
        invites::create(&local, &invite_request(&url, Some(&pin))).map_err(invite_problem)?;
    Ok(ServerDone {
        url,
        pin: Some(pin),
        invite: Some(created.token),
        invite_expires_at: Some(created.entry.expires_at),
        stub: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_from_commas_or_spaces() {
        assert_eq!(
            parse_names("box.tail1234.ts.net, 100.101.102.103 10.0.0.5,box").unwrap(),
            ["box.tail1234.ts.net", "100.101.102.103", "10.0.0.5", "box"]
        );
        assert_eq!(parse_names("Box,box").unwrap(), ["box"]);
        assert_eq!(parse_names("[::1]").unwrap(), ["::1"]);
        assert_eq!(
            parse_names("bad_name").unwrap_err(),
            "bad_name isn't a host name or an IP address."
        );
        assert!(parse_names(" , ").is_err());
    }

    #[test]
    fn domains_are_full_public_names() {
        assert_eq!(domain_check("switchboard.example.com"), Ok(()));
        assert_eq!(domain_check("Switchboard.Example.COM"), Ok(()));
        assert!(domain_check("").is_err());
        assert_eq!(domain_check("https://sb.example.com"), Ok(()));
        assert_eq!(
            clean_domain("https://SB.Example.com:443/x?y"),
            "sb.example.com"
        );
        assert_eq!(clean_domain("sb.example.com."), "sb.example.com");
        assert_eq!(
            domain_check("https://Switchboard"),
            Err("Use the full name, such as switchboard.example.com.".into())
        );
        assert_eq!(
            elsewhere(&["104.20.23.154".parse().unwrap()]),
            "104.20.23.154, which isn't this machine's address"
        );
        assert_eq!(
            elsewhere(&[
                "104.20.23.154".parse().unwrap(),
                "172.66.147.243".parse().unwrap(),
                "::1".parse().unwrap(),
                "::2".parse().unwrap()
            ]),
            "104.20.23.154 and 3 other addresses, none of them this machine's"
        );
        assert_eq!(
            domain_check("10.0.0.5"),
            Err("Let's Encrypt needs a domain name, not an IP address.".into())
        );
        assert_eq!(
            domain_check("switchboard"),
            Err("Use the full name, such as switchboard.example.com.".into())
        );
        assert_eq!(
            domain_check("sb_.example.com"),
            Err("That isn't a valid domain name.".into())
        );
    }

    #[test]
    fn lets_encrypt_reasons_become_fixes() {
        let firewall = acme_problem(
            "order Invalid: API error: 203.0.113.7: Timeout during connect (likely firewall problem) (urn:ietf:params:acme:error:connection)",
            "sb.example.com",
            443,
        );
        assert_eq!(
            firewall.title,
            "Let's Encrypt couldn't reach this machine on port 443."
        );
        assert!(firewall
            .detail
            .as_deref()
            .unwrap()
            .contains("Timeout during connect (likely firewall problem)"));
        assert!(firewall
            .fix
            .as_deref()
            .unwrap()
            .contains("sudo ufw allow 443/tcp"));
        let dns = acme_problem(
            "order Invalid: API error: DNS problem: NXDOMAIN looking up A for sb.example.com (urn:ietf:params:acme:error:dns)",
            "sb.example.com",
            443,
        );
        assert_eq!(
            dns.title,
            "Let's Encrypt couldn't find sb.example.com in DNS."
        );
        let proxy = acme_problem("order Invalid: API error: Incorrect validation certificate for tls-alpn-01 challenge (urn:ietf:params:acme:error:unauthorized)", "sb.example.com", 443);
        assert!(proxy.title.starts_with("Something else answered"));
        let limited = acme_problem("API error: too many certificates already issued (urn:ietf:params:acme:error:rateLimited)", "sb.example.com", 443);
        assert!(limited.title.contains("rate limiting"));
        let caa = acme_problem("API error: CAA record for sb.example.com prevents issuance (urn:ietf:params:acme:error:caa)", "sb.example.com", 443);
        assert!(caa.title.contains("CAA"));
        let outbound = acme_problem(
            "error sending request for url (https://acme-v02.api.letsencrypt.org/directory)",
            "sb.example.com",
            443,
        );
        assert_eq!(outbound.title, "This machine can't reach Let's Encrypt.");
        let other = acme_problem("order Invalid", "sb.example.com", 443);
        assert_eq!(other.title, "Let's Encrypt didn't issue a certificate.");
    }

    #[test]
    fn installer_errors_become_fixes() {
        let unit = install_problem(
            "refusing unmanaged unit: /etc/systemd/system/switchboard-server.service",
        );
        assert_eq!(
            unit.title,
            "/etc/systemd/system/switchboard-server.service is in the way."
        );
        let slow = install_problem("switchboard-server.service did not become ready within 15 seconds:\n● switchboard-server.service\n   Active: failed\nOct 31 server: switchboard server: Address already in use (os error 98)\n");
        assert!(slow
            .detail
            .as_deref()
            .unwrap()
            .contains("Address already in use"));
        let old_server = install_problem("refusing to install beside the enabled or active switchboard-hub.service (an older Switchboard server); stop and disable it first");
        assert!(old_server.fix.as_deref().unwrap().contains("disable --now"));
    }

    #[test]
    fn the_privileged_step_gets_every_answer() {
        let plan = Plan {
            tls: Tls::Private,
            domain: None,
            names: vec!["box".into(), "10.0.0.5".into()],
            port: 8743,
        };
        assert_eq!(
            privileged_arguments(&Options::default(), &plan, false),
            [
                "init",
                "--privileged",
                "--tls",
                "private",
                "--port",
                "8743",
                "--color",
                "never",
                "--server-name",
                "box",
                "--server-name",
                "10.0.0.5"
            ]
        );
    }
}
