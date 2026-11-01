# Switchboard slash commands

This plugin adds `/switchboard` commands to Claude Code and shows a footer label in joined chats.
`switchboard init` offers to install it. To install it yourself for every Claude Code chat
(Claude Code 2.1.292 or later; the marketplace is this repository's root):

```sh
claude plugin install switchboard --marketplace krish-k7/switchboard
```

From a checkout, use the checkout's path instead of `krish-k7/switchboard`. To pick up a newer
version, run `claude plugin marketplace update switchboard`, then `/reload-plugins` in chats that
were already open.

The plugin's version is the release version (`.github/scripts/set-version.sh` sets both, and CI
refuses a mismatch), so Claude Code refreshes its cached copy when a new release is pulled.

Or load it for one chat only:

```sh
claude --plugin-dir /path/to/switchboard/claude-plugin
```

The plugin calls the installed CLI at `~/bin/switchboard`; it does not install or start the
bridge, and it never reads or sends messages. To install the CLI, run this in a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
```

then set the machine up with `switchboard init`.

- `/switchboard` (or `/switchboard status`): whether this machine is connected, whether this
  chat is joined and when it expires, and how many chats here are joined.
- `/switchboard chats [--local|--remote] [--details] [search words]`: a table of chats per
  machine. Joined chats show the name other machines see and when they expire. Search matches
  names, IDs, machines, projects, repositories, and branches. `sessions` is an older name for
  the same command and still works.
- `/switchboard join [name]`: join this chat. Other machines see the name, or the chat's own
  name.
  The reply comes once the bridge has published the chat.
- `/switchboard leave`: leave this chat. Leaving a chat that isn't joined changes nothing.
- `/switchboard ping <chat>`: check that a chat on another machine is reachable, without waking it.
- `/switchboard up`: bring this machine back online after `down`, and wait for the connection.
- `/switchboard down`: take this machine offline, across logins and restarts, until `up`.
- `/switchboard doctor`: problems first, each with how to fix it, then the checks that passed.
- `/switchboard help`: usage.

Machines are added, listed, and removed on the server, not from a chat: `sudo switchboard
server invite` there makes an invite, `switchboard init` on the new machine redeems it, and
`sudo switchboard server machines` and `sudo switchboard server revoke NAME` list and remove
machines.
`/switchboard machines`, `approve`, `refuse`, and `revoke` (removed) say so and run nothing.

A joined chat shows `switchboard: <name>` among the footer labels at the right of the prompt
(terminal and desktop). The label follows joins and leaves made anywhere, including from a
terminal or by an agent. The bridge and CLI keep `membership.json` current beside the bridge
config the CLI uses (`SWITCHBOARD_CONFIG` when set, else `~/.config/switchboard/bridge.json`);
the plugin reads the same variable, asks Claude Code to watch that file, and rereads it when it
changes and on each prompt; join, leave, and status set the label directly. A file that cannot
be read or parsed clears the label rather than leaving a stale one. A relative
`SWITCHBOARD_CONFIG` depends on the directory the CLI runs in, so the plugin then watches
nothing and the label follows this chat's commands. It starts no process and sets no timer.

What each command shows comes from the CLI: the plugin runs it with `--json --display`, and
the CLI adds the same blocks its terminal output is drawn from (REFERENCE.md, "What commands
show"). The plugin checks them, removes addresses and credentials, shows `<` and `>` in any text
from the CLI, the server, or another machine as `‹` and `›` (what a command prints reaches the
model too, so a chat named `<system-reminder>` can't read as a tag), shows `switchboard X` as
`/switchboard X` for its own commands, and draws them with Claude Code's renderer
(`hooks/display.js`), so the terminal and the chat say the same thing in the same order. The
help is written here and checked against the CLI's. A CLI older than 0.3 has no `--display`;
the plugin then asks again the old way and formats the answer itself (`hooks/format.js`).

`tests/display-fixture.js` is written by the CLI's tests
(`SWITCHBOARD_BLESS=1 cargo test the_plugin_fixture_matches`) and holds every display they
check; the plugin's tests fail if it drops, reorders, or rewords any of it.

Each result is one layout document. The stored command result is its plain text, and the
transcript row is drawn from the same document, so wording changes cannot drop styling. Rows
from before a plugin reload are shown as plain text.

Requests run one at a time. Join and leave always use this chat's exact UUID; clearing or
switching the chat, or canceling, before the CLI starts stops the request, and a started join or leave
is never retried. Raw CLI output and credentials are never printed: a failed command shows the
error the CLI reported (its JSON `error` field), with addresses and credentials removed.

Checks, from this directory:

```sh
claude plugin validate --strict .
claude plugin test
```

The tests stub the CLI and host; they do not start a chat or reach a real server.
