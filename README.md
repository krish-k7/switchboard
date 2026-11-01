# Switchboard

Switchboard lets Claude Code chats on different machines message each other.

Claude Code can already message other chats on the same machine: Claude lists them with its
`ListAgents` tool and writes to them with `SendMessage`. Switchboard extends that across machines.
You run a small server, and each machine you join to it runs a background service, the
**bridge**, that shows the chats on other machines to Claude as if they were local. Claude uses
the same two tools it already has. There is nothing new for it to learn and no tool to install
in the chat.

Nothing is shared by default. You choose each chat that takes part, and only those chats can
send or receive across machines.

## What it looks like

Say you have a chat called `api-refactor` on your laptop and a chat called `research` on a
workstation, and you have joined both to Switchboard. In the laptop chat you ask:

> Ask the research chat on the workstation which branch has the new retry logic.

Claude calls `ListAgents`, which now includes `switchboard/workstation/research-837bd3b0`, and
sends the question with `SendMessage`. The `research` chat receives it as a message from
`switchboard/laptop/api-refactor-306ccf58` and can answer the same way. Each chat keeps working
on its own machine, with its own files and tools.

From a terminal on the laptop, the same setup looks like this:

```text
$ switchboard chats --remote
  Chats on other machines

  workstation
  Chat      Activity
  research  Idle

  Full IDs and details:
      switchboard chats --remote --details

$ switchboard ping research
✓ workstation/research replied in 1 ms.
```

## Words used in these docs

- **Server**: one Linux machine that routes messages between machines. You run it yourself.
- **Machine**: a computer joined to the server. Each has its own key and a name such as
  `laptop`.
- **Bridge**: the background service on each machine that connects its chats to the server.
- **Chat**: a Claude Code session (one conversation in a terminal or the desktop app).
- **Invite**: a one-time token, made on the server, that lets one new machine join.
- **Join**: a machine joins the server once, with an invite. A chat joins Switchboard when you
  run `/switchboard join` in it.

## Requirements

- **Machines that join:** macOS (Apple silicon or Intel) or Linux (x86_64 or arm64), with Claude
  Code installed and run at least once. The installer needs `curl` (or `wget`) and one of
  `sha256sum`, `shasum`, or `openssl`. No root access is needed.
- **The server:** Linux with systemd, and `sudo` once for the install. It can be the same machine
  as one of your clients. For a public domain it needs a DNS name and inbound TCP port 443; on a
  private network (Tailscale or a LAN) it listens on port 8743.
- **Claude Code:** Switchboard uses Claude Code's built-in messaging between chats (`ListAgents`
  and `SendMessage`). The optional plugin installs with `claude plugin install ... --marketplace`,
  which needs Claude Code 2.1.292 or later.

## Quickstart

Switchboard is one program, `switchboard`. The same install command sets up every machine; a
setup wizard (`switchboard init`) then asks what this machine should do.

### 1. Run a server

On a Linux machine with systemd:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
```

The script downloads the release for this machine, checks it against the release's checksums,
installs it under your home directory, and starts `switchboard init`. Choose **Run the server**
(or **Run the server and join it** if you also use Claude Code on this machine), then where
machines will connect from:

- **A private network (Tailscale or LAN).** The server makes its own certificate. Each invite
  carries the certificate's fingerprint, so joining machines can check it.
- **Anywhere, with a public domain name.** Needs a DNS record pointing at this machine and port
  443 open to the internet. The certificate comes from Let's Encrypt.

The wizard uses `sudo` once to install the server as a system service, waits until it answers,
and prints an invite for the first machine:

```text
  First invite
  Single use, expires in 1 hour, at 05:22 PDT. Copy all of it; line breaks are
  fine.
      sbi1.eyJ1cmwiOiJodHRwczovL2xvY2FsaG9zdDo1MjYxNiIsInBpbiI6IjJlY2UxNmMxNzg
      ...
```

[docs/server.md](docs/server.md) covers both modes in detail, running behind a proxy you already
have, and installing without the wizard.

### 2. Join a machine

Use the invite the wizard printed for the first machine. For each machine after that, make a new
one on the server:

```sh
sudo switchboard server invite
```

An invite works once and expires after an hour. Treat it like a password until it is used. On
the machine you are adding, run the same install command and paste the invite when it asks (it
stays hidden; line breaks from a chat app are fine):

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
```

The wizard checks the server's certificate, asks for this machine's name (the host name by
default), makes the machine's key, joins, starts the bridge, and offers to install the Claude Code
plugin. When it finishes, `switchboard status` shows the connection; it starts with
`✓ Connected to <server> as <machine>`.

If your shell can't find `switchboard`, the wizard offers to add `~/bin` to your `PATH` in the
startup file your shell reads; until then, run `~/bin/switchboard`. It also offers Tab completion
for commands, options, and chat names (bash, zsh, and fish; `switchboard completion --help` says
how to add it yourself).

### 3. Join a chat

In each Claude Code chat you want connected, run:

```text
/switchboard join
```

That is the plugin's command. Without the plugin, run `!switchboard join` (the `!` runs a shell
command from the chat). To choose the name other machines see, add it:
`/switchboard join research`. The reply comes once the bridge has published the chat:

```text
✓ Joined as research.
  Reachable by 1 chat on 1 other machine.
  Chat ID: 837bd3b0-634e-423b-9451-27aeb7e3c595
```

Do the same in a chat on another machine.

### 4. Send a message

Ask Claude, in plain words, to message the other chat, as in the example above. Claude finds it
with `ListAgents` (remote chats are named `switchboard/<machine>/<name>-<id>`) and sends with
`SendMessage`. To check the path without sending anything, run `switchboard ping <chat>` from a
terminal.

A message that reaches the other chat has been handed to it. That does not mean the other Claude
has read it or acted on it.

## Everyday use

```sh
switchboard status            # connected or not, and whether this chat is joined
switchboard chats             # chats here and on other machines
switchboard chats --details research   # with full IDs, projects, and branches
switchboard ping research     # check the path to a remote chat, without waking it
switchboard doctor            # check everything, with the fix for each problem
switchboard logs --follow     # what the bridge is doing
switchboard down              # take this machine offline, even after a restart
switchboard up                # bring it back online
switchboard upgrade           # install the latest release
switchboard uninstall         # remove Switchboard from this machine
```

`status` shows one more line when a newer release is out (it asks GitHub at most once a day, or
once an hour while GitHub can't be reached; `SWITCHBOARD_NO_UPDATE_CHECK=1` turns that off). `down` keeps this machine's configuration, key,
and joined chats; only `up` brings it back, not a login, an install, or an upgrade.

In a chat, `/switchboard leave` (or `!switchboard leave`) disconnects it. A joined chat also
leaves on its own after 7 days without activity: working, or sending or receiving a Switchboard
message. Joining belongs to the conversation, so `/clear`, branching, and `/resume` move you to a
conversation that is not joined unless you join it too. With the plugin, joined chats show
`switchboard: <name>` in the footer, and `/switchboard status`, `chats`, `ping`, `doctor`, `up`,
and `down` show the same answers as in a terminal.

Every command accepts `--json` and has its own `--help`.

## On the server

```sh
sudo switchboard server status      # is the server healthy? problems first, with fixes
sudo switchboard server logs        # its events over time (--follow to keep watching)
sudo switchboard server invite      # an invite for a new machine
sudo switchboard server machines    # the machines on the server, and when each was last seen
sudo switchboard server invites     # the open invites
sudo switchboard server revoke NAME # remove a machine; its key stops working
sudo switchboard server uninstall   # remove the server, keeping its data (--purge deletes it)
```

[docs/server.md](docs/server.md) covers each of them.

## Telling agents how to treat peer messages

A message from another chat is input from another agent, not an instruction from you.
[CLIENT_INSTRUCTIONS.md](CLIENT_INSTRUCTIONS.md) is a short rule for Claude on how to weigh such
messages: take them seriously, check their reasoning, and choose recipients by topic.
[INSTALL.md](INSTALL.md) shows how to install it as a Claude Code rule.

## More

- [docs/server.md](docs/server.md): choosing and running a server, invites, machines, upgrades,
  backups, logs, and removal.
- [docs/security.md](docs/security.md): what Switchboard protects and what it does not.
- [docs/troubleshooting.md](docs/troubleshooting.md): error messages and their fixes.
- [INSTALL.md](INSTALL.md): instructions an agent can follow to install Switchboard for you.
- [REFERENCE.md](REFERENCE.md): every command, option, file, and setting.
- [ARCHITECTURE.md](ARCHITECTURE.md) and [PROTOCOL.md](PROTOCOL.md): how it works, for
  contributors.
- [CONTRIBUTING.md](CONTRIBUTING.md): building, testing, and releases.
- [claude-plugin/README.md](claude-plugin/README.md): the Claude Code plugin.
