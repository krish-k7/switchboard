# Switchboard

Lets Claude Code chats on different machines message each other.

Claude Code chats on one machine can already message each other: Claude lists them with its
`ListAgents` tool and writes to them with `SendMessage`. Switchboard extends that across machines
through a small server you run. Claude uses the same two tools, so there is nothing new for it to
learn. Only the chats you join take part.

## Requirements

- **Each machine:** macOS or Linux (x86_64 or arm64), with Claude Code installed and run at least
  once. No root access is needed.
- **The server:** one Linux machine with systemd, and `sudo` for the install. It can be one of
  your own machines. On a private network (Tailscale or LAN) it listens on port 8743; with a
  public domain it needs a DNS name and port 443 open to the internet.
- **The optional Claude Code plugin** (`/switchboard` commands): Claude Code 2.1.292 or later.

## Install

Run this on every machine, the server included:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
```

It downloads the release for the machine, checks it against the release's checksums, installs it
under your home directory, and starts the setup wizard, `switchboard init`. The command is
`~/bin/switchboard`; the wizard offers to add `~/bin` to your `PATH`.

### Have your agent set it up

Paste this into Claude Code on your machine:

```text
Set up Switchboard (https://github.com/krish-k7/switchboard) on this machine by following
https://raw.githubusercontent.com/krish-k7/switchboard/main/INSTALL.md. If I don't have a
Switchboard server yet, set one up on a Linux machine I choose. Ask me before every choice
INSTALL.md leaves to me. Follow its rules for invites: never ask me to paste one into this chat,
never put one on a command line, and if I paste one anyway, don't use it. When you're done, tell
me what you changed on each machine and how to check that it works.
```

With no server yet, the agent asks which machine should run it and how to reach it over SSH,
installs the server there, makes an invite, and joins this machine with it. The invite travels
from the server to this machine through SSH, never through the chat. With a server already
running, the agent makes an invite over SSH, or has you enter one in your own terminal. Run the
same prompt on each machine you add. [INSTALL.md](INSTALL.md) has the steps.

## Quick start

1. **Run the server.** On the Linux machine, run the install command and choose **Run the
   server** (or **Run the server and join it** to use Claude Code there too). Choose a private
   network or a public domain. The wizard installs the server with `sudo` and prints the first
   invite. [docs/server.md](docs/server.md) explains the choices.
2. **Add a machine.** On each other machine, run the install command, choose **Join a server** if
   it asks, and paste the invite. An invite works for one machine and is valid for one hour.
   For each machine after the first, make a new one on the server:

   ```sh
   sudo switchboard server invite
   ```

3. **Join chats.** In each Claude Code chat that should take part, on each machine, run
   `/switchboard join` (with the plugin, which the wizard offers) or `!switchboard join` (without
   it). Add a name to choose what other machines see: `/switchboard join research`. Chats that
   were open before the plugin was installed need `/reload-plugins` first.
4. **Send a message.** Ask Claude in plain words, for example: "Ask the research chat on the
   workstation which branch has the new retry logic." Claude finds the chat with `ListAgents`,
   where remote chats are named `switchboard/<machine>/<name>-<id>`, and sends with
   `SendMessage`.

To check the setup from a terminal, `switchboard status` should start with
`✓ Connected to <server> as <machine>`. `switchboard ping research` checks the path to a joined
chat named `research` on another machine, without sending it anything, and answers like
`✓ workstation/research replied in 1 ms.` If something is wrong, `switchboard doctor` says what
and how to fix it.

## Usage

| Command | What it does |
| --- | --- |
| `switchboard init` | Set up this machine, or show its setup and what you can change |
| `switchboard join [NAME]` | Join the current chat, so other machines can see and message it |
| `switchboard leave` | Leave the current chat |
| `switchboard chats` | List chats on this machine and joined chats on others |
| `switchboard status` | Whether this machine is connected, and whether this chat is joined |
| `switchboard doctor` | Check everything, with the fix for each problem |
| `switchboard ping CHAT` | Check that a remote chat is reachable, without messaging it |
| `switchboard down` | Take this machine offline, across restarts, until `switchboard up` |
| `switchboard up` | Bring this machine back online |
| `switchboard logs` | Show this machine's connection events and errors (`--follow` to keep watching) |
| `switchboard upgrade` | Install the latest release |
| `switchboard uninstall` | Remove Switchboard from this machine (`--purge` also deletes its key, settings, and logs) |
| `switchboard completion SHELL` | Print Tab completion for bash, zsh, or fish |

In a chat, put `!` in front to run a command in the chat's shell: `!switchboard join`. The
plugin's `/switchboard` covers `status`, `chats`, `join`, `leave`, `ping`, `doctor`, `up`, and
`down`, and shows `switchboard: <name>` in the footer of a joined chat.

A joined chat leaves on its own after 7 days without work or messages. Joining belongs to the
conversation, so `/clear`, branching, and `/resume` move you to a conversation that isn't joined
unless you join it too. A message that reaches another chat has been handed to it; that doesn't
mean the other Claude has read it.

On the server:

| Command | What it does |
| --- | --- |
| `sudo switchboard server status` | Whether the server is healthy; problems first, with fixes |
| `sudo switchboard server invite` | Make an invite (`--uses` and `--expires` change its limits; `--name` fixes the machine's name) |
| `sudo switchboard server invites` | List the open invites |
| `sudo switchboard server machines` | List the machines and when each was last seen |
| `sudo switchboard server revoke NAME` | Remove a machine; its key stops working |
| `sudo switchboard server logs` | Show the server's events (`--follow`, `--since 2h`) |
| `sudo switchboard server uninstall` | Remove the server, keeping its data (`--purge` deletes it) |

Every command has `--help`, and most take `--json` for output scripts can read.

## How it works

The server routes messages between machines and keeps the list of machines allowed to connect.
Each machine runs the **bridge**, a background service that connects its joined chats to the
server. For each joined chat on another machine, the bridge starts a small local helper process
that Claude Code sees as one more chat, so `SendMessage` to it travels through the server to the
real chat. A machine joins once with an invite, a single-use token made on the server. The
machine then makes its own key, which the server checks on every request.
[ARCHITECTURE.md](ARCHITECTURE.md) has the details.

## Security

Switchboard is for machines that trust each other, connected through a server you control.
Connections use TLS, and each machine proves itself with its own key. Messages are not end-to-end
encrypted: whoever has root on the server can read them. Every joined machine can see and message
every joined chat. A message from another chat is input from another agent, not an instruction
from you; [CLIENT_INSTRUCTIONS.md](CLIENT_INSTRUCTIONS.md) is a Claude Code rule for weighing such
messages. [docs/security.md](docs/security.md) covers what is protected and what is not.

## Documentation

- [docs/server.md](docs/server.md): choosing and running a server, invites, upgrades, backups,
  and removal.
- [docs/troubleshooting.md](docs/troubleshooting.md): error messages and their fixes. Start with
  `switchboard doctor`.
- [docs/security.md](docs/security.md): the security model.
- [INSTALL.md](INSTALL.md): step-by-step setup for an agent, server included.
- [REFERENCE.md](REFERENCE.md): every command, option, file, and setting.
- [claude-plugin/README.md](claude-plugin/README.md): the Claude Code plugin.
- [ARCHITECTURE.md](ARCHITECTURE.md), [PROTOCOL.md](PROTOCOL.md), and
  [CONTRIBUTING.md](CONTRIBUTING.md): for contributors.
