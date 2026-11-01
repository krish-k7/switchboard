# Switchboard reference

Every command, option, file, and setting. [README.md](README.md) is the guide;
[docs/server.md](docs/server.md) explains running a server.

## The program

Switchboard is one binary, `switchboard`, with one subcommand per role:

| Command | Role |
| --- | --- |
| `switchboard COMMAND ...` | The user commands below (`switchboard cli COMMAND ...` is the same) |
| `switchboard install [--start] [--json]` | Install this binary for the current user |
| `switchboard bridge --config FILE` | The bridge, run by the user service |
| `switchboard helper ...` | One per visible remote chat, started by the bridge |
| `switchboard server ...` | The server and its administration commands |
| `switchboard --version` | Print the version, such as `switchboard 0.2.0` |

The installed command `~/bin/switchboard` is a two-line script that runs
`~/.local/share/switchboard/switchboard cli "$@"`; every row above works through it too. Run server
commands as `sudo switchboard server ...`, which uses the server's own copy in
`/usr/local/bin`.

## User commands

Every command takes these options:

| Option | Meaning |
| --- | --- |
| `--json` | Print the result as one JSON document |
| `--config PATH` | The bridge configuration (default `~/.config/switchboard/bridge.json`, or `SWITCHBOARD_CONFIG`) |
| `-h`, `--help` | Show the command's help (`switchboard help COMMAND` does the same) |
| `--display` | With `--json`, add `display`: what the command shows, as blocks (below). For the Claude Code plugin |

Options are matched exactly (no abbreviations) and may be written `--option VALUE` or
`--option=VALUE`; `--` ends the options.

### `switchboard init`

Set up this machine: join a server with an invite, run the server here, or both. On a terminal it
asks for what it needs; Esc goes back one question until something is made, and Ctrl-C stops
cleanly. Without a terminal (or with `--json`) it asks nothing and stops on a missing answer with
exit status 2, naming the option that gives it. On a machine that is already set up it shows the
server, the bridge, and the plugin, and offers the actions that fit: join again with a new
invite, start the bridge (or bring a machine that is down back up), install the plugin, join a
different server, make an invite (on a server), run the server here too, or leave the server. On
a machine taken down with `switchboard down`, init doesn't start the bridge, and its default
choice keeps the machine as it is (bringing it back up is offered, never preselected). Leaving
stops the bridge, removes its background service (so it doesn't start again at login), and moves
the setup to `~/.config/switchboard.left-<time>`; a machine that was down also has its service
allowed to start at login again, as `uninstall --purge` does, so the next setup doesn't find it
disabled. Joining a different server keeps the service, which runs the new setup.

When `switchboard` isn't on your `PATH`, init offers to add `~/bin` to your shell's startup file
(`~/.zshrc`, `~/.bashrc`, fish's `config.fish`, or `~/.profile`). Bash on macOS reads only the
first of `~/.bash_profile`, `~/.bash_login`, and `~/.profile` that exists, so the line goes in that
one; with none of them, init makes `~/.bash_profile` with a line that also reads `~/.profile`.
On a terminal, init also offers to turn on Tab completion for bash, zsh, and fish, with a line
that loads `switchboard completion SHELL` in the same file. Neither line is added twice (a file
that already has one is left alone; for completion, a line that runs `switchboard completion
SHELL` and evaluates it with `eval`, `source`, or `.`), and `--yes` never adds them.

Joining:

| Option | Meaning |
| --- | --- |
| `--invite [-]` | Join with an invite, read from standard input, or asked for without echo on a terminal. `SWITCHBOARD_INVITE` works too. The invite itself on the command line (`--invite sbi1...`) is refused: other users could see it in `ps` |
| `--name NAME` | This machine's name (default: the host name, unless the invite fixes one). `--machine` is the same |
| `--owner NAME` | Who the machine belongs to (default: your user name) |
| `--plugin` / `--no-plugin` | Install the Claude Code plugin, or don't. Without a terminal the default is not to |
| `--no-start` | Don't install or start the bridge |
| `--replace` | Join the invite's server even if this machine is set up for another; the current setup is moved to `~/.config/switchboard.before-TIME` and put back if joining fails |

On a machine taken down with `switchboard down`, `setup` installs the bridge's service but
neither starts it nor lets it start at login; it answers `"down": true`, and `switchboard up`
brings the machine back.

Running the server (Linux with systemd; the install runs once through `sudo`):

| Option | Meaning |
| --- | --- |
| `--server` | Run the server on this machine |
| `--tls private` | For a private network: a self-signed certificate that machines check through the invite (implied by `--server-name`) |
| `--tls acme` | Public: a Let's Encrypt certificate for `--domain` (implied by `--domain`) |
| `--domain NAME` | `acme`: the server's public DNS name |
| `--email ADDRESS` | `acme`: a contact for the certificate account (optional) |
| `--server-name NAME` | `private`: a name or address machines use to reach the server; repeat for more. Default: this machine's Tailscale name and IPv4 addresses, its primary address, and its host name |
| `--port PORT` | The port to listen on (default 443 for `acme`, 8743 for `private`) |
| `--join` | Also join this machine to the new server |

General:

| Option | Meaning |
| --- | --- |
| `--yes`, `-y` | Take the default answer to every question (it never edits your shell's startup file) |
| `--json` | Print the result as JSON, and ask nothing |

Machine names are lowercase letters, digits, and hyphens, start with a letter, and are at most 48
characters. Owners are one line of at most 64 characters.

### `switchboard status [--chat ID]`

Whether this machine is connected (and to which server, as which machine), whether this chat is
joined and when it expires, and how many chats here are joined:

```text
✓ Connected to sb.example.com as laptop
  This chat   Joined as research, expires in 6 days
  Chats here  2 of 8 joined
```

The `This chat` row appears inside a chat (found through `CLAUDE_CODE_MESSAGING_SOCKET`) or with
`--chat ID`. A machine taken down says `Down. Run switchboard up to reconnect.`; one that isn't
set up says `Not set up. Run switchboard init.` and exits 1. When a newer release exists, a line
says `Switchboard 0.3.1 is available. Run switchboard upgrade.` (see `SWITCHBOARD_NO_UPDATE_CHECK`).
At most one next step follows.

With `--json`: `enabled`, `server_url`, `sessions` (the chats open on this machine),
`connected`, and `machine` and `pid` while the bridge runs, or `detail` (such as
`the bridge is not running`) when it doesn't; `down: true` on a machine that is down, and
`update_available` (the version) when a newer release is known. Exits 1 only when the machine
isn't set up.

### `switchboard chats [SEARCH]`

List the Claude Code chats open on this machine and the joined chats on other machines, a table
per machine: each chat's name, what it is doing, and on this machine whether it is joined (and
as what) and when it expires. At most 50 are listed; add search words to narrow the list.

| Option | Meaning |
| --- | --- |
| `SEARCH` | Chats whose name, joined name, ID, machine, project, repository, or branch contains each word |
| `--local` / `--remote` | Only chats on this machine, or only on others |
| `--details` | Each chat's full ID, project, repository, and branch |
| `--name TEXT` | Chats whose name contains TEXT |
| `--project TEXT` | Chats whose project or repository contains TEXT |
| `--machine TEXT` | Chats on machines whose name contains TEXT |
| `--chat ID` | Mark this chat `(this chat)`; inside a chat it is found on its own |

Matching ignores case. `--local` makes no request to the server. Chats on other machines come
from one authenticated request with an 8-second deadline; if it fails, local chats are still
listed, and `--remote` exits 1. With `--json`, chats keep their older key names: `sessions`
(local rows with `id`, `name`, `status`, `enrolled`, and for joined chats `joined_name` and
`expires_at`), `remote_sessions`, `remote_available`, `machine`, and `remote_error` when the
request failed. Rows include `project`, `repository`, and `branch` when known. `sessions` is an
older name for this command.

### `switchboard join [NAME]`

Join a chat: other machines can see it and message it. Run it inside the chat
(`/switchboard join` with the plugin, `!switchboard join` without). Other machines see NAME, or the
chat's own name, normalized to letters, digits, `_`, and `-` and at most 26 characters. Joining a
joined chat again renames it. `--chat ID` joins the chat with that full ID from another terminal
(`--session` is an older name).

Join waits up to ten seconds for the bridge to publish the chat under its name (the new one,
after a rename), then reports how many chats on how many machines it can reach, or that it isn't
live yet and why, or that other machines still see the old name. A machine can have at most 32
joined chats. A joined chat leaves on its own after 7 days without activity (working, or sending
or receiving a Switchboard message).

### `switchboard leave`

Leave the current chat, or the one named with `--chat ID` (which also works for a joined chat
that has been closed). Other machines can no longer see or message it. Leaving a chat that isn't
joined changes nothing.

### `switchboard ping CHAT`

Check that a joined chat on another machine is reachable, without sending it anything: a round
trip through the server to that machine's bridge, which answers whether the chat is joined and
running. CHAT is the chat's name, `MACHINE/NAME`, its alias (with or without `switchboard/`), its
full ID, or an ID prefix of at least 4 hex digits, all ignoring case. More than one match is an
error that lists up to 8 candidates. Waits at most 5 seconds for the reply.

```text
$ switchboard ping workstation/research --json
{"ok": true, "target": "switchboard/workstation/research-837bd3b0", "machine": "workstation", "name": "research", "rtt_ms": 1, "via": "server"}
```

### `switchboard doctor`

Check the program, the configuration and key, the bridge, the server, and chat discovery, with a
fix for each problem. It changes nothing. Failures come first, then warnings, then a machine
that is down, then the checks that passed, one per line, then a summary. Exit status 1 when a
check fails; warnings alone exit 0. With `--json`: `ok` and `checks`, each with `name`
(`program`, `configuration`, `bridge`, `discovery` when something is off, `server`, `chats`),
`status` (`pass`, `warn`, or `fail`), `summary`, and `fix` when there is one. A machine that is
down has a passing `bridge` check with `down: true`. The `chats` check (how many joined chats
are open) is in the JSON only; `status` answers that.

### `switchboard up` and `switchboard down`

`down` takes this machine off Switchboard until `up`: it keeps the bridge's service from starting
at login or after a restart (`launchctl disable gui/UID/dev.switchboard.bridge` on macOS,
`systemctl --user disable --now switchboard-bridge.service` on Linux), then writes the file `down`
beside the configuration, and stops the bridge. If the service can't be disabled, nothing is
marked down. The configuration, key, and joined
chats stay. `status` and `doctor` report the machine as down, not as a failure, and `install`,
`upgrade`, `install.sh`, and `init` leave it down.

`up` checks that the machine is set up and its service installed, lets the service start at login
again (`launchctl enable`, `systemctl --user enable`), then removes `down`, starts the bridge, and
waits up to 15 seconds until it connects. If a machine that is down can't have its service
enabled, `down` stays and the error gives the command to run. When launchd refuses to start a job
that is disabled without the `down` file (disabled by hand), the error says so and points to
`switchboard up` rather than showing launchd's own message. With `--json`, `up` answers `action`, `connected`,
`server_url`, and `machine`; `down` answers `action`, `down`, and `stopped` (whether the bridge
was running). `start` and `stop`, the older names, do the same.

### `switchboard logs [--follow] [--lines N]`

The bridge's latest events from `~/.local/share/switchboard/daemon.log` (20, or N from 1 to
10000), one per line: the local time, the event (`connected`, `offline`, `delivery`,
`request failed`, `refused`, `chat left`, `helper failed`, `turned off`, `error`), and its
detail. Then the last lines the bridge printed on standard error (`daemon-error.log`), under
their own heading. `--follow` keeps printing new events until Ctrl-C, and ends quietly (status 0)
once its output is closed, as with `| head`. With `--json`, the log's own lines are printed as
they are, one JSON object per line; control characters in a line (which only a line that isn't
JSON can carry raw) are written as `\uXXXX`.

### `switchboard completion bash|zsh|fish`

Print a script that completes commands, options, chats on other machines (for `ping`), and chat
IDs (for `--chat`) with Tab. Chats come from local files only (`switchboard completion --words
chats` and `--words ids`), so Tab never waits on the server. To load it, add to the shell's
startup file `eval "$(switchboard completion zsh)"` (or `bash`), or for fish
`switchboard completion fish | source`; `init` offers to.

### `switchboard install [--start] [--json]`

Install the running binary for the current user:

- the program at `~/.local/share/switchboard/switchboard`;
- the command `~/bin/switchboard`, linked from `~/.local/bin/switchboard`;
- the bridge's user service, running `switchboard bridge --config ~/.config/switchboard/bridge.json`
  with umask 077, restarted 10 seconds after it exits while that configuration exists (without
  one it stays stopped), and given 30 seconds to stop: on macOS the
  launch agent `~/Library/LaunchAgents/dev.switchboard.bridge.plist` (label
  `dev.switchboard.bridge`), on Linux the systemd user unit `switchboard-bridge.service` in
  `$XDG_CONFIG_HOME/systemd/user` (default `~/.config/systemd/user`), enabled through a
  `default.target.wants` link.

It doesn't start the bridge unless given `--start`, and never joins chats or changes Claude Code
settings. On a machine that is down it neither enables the service nor starts it, even with
`--start`, and its JSON adds `down: true`. Running it again upgrades in place; files it replaces are backed up under
`~/.local/share/switchboard/backups/`. It replaces only files it wrote (each carries a marker),
and moves a bridge service installed by an earlier version under the old name
(`com.vorelios.switchboard` on macOS, `switchboard.service` on Linux), restarting it if it was
running.

### `switchboard upgrade [VERSION] [--force]`

Download the latest release, or VERSION (such as `1.2.3`), check it against the release's
`SHA256SUMS` and that it reports the expected version, run its own `install`, and restart the
bridge if it was running. A machine that is down stays down (the JSON adds `down: true`). The same version does nothing, and an older one is refused unless
`--force` is given. Downloads use HTTPS only.

### `switchboard uninstall [--purge]`

Stop the bridge and remove its service, the `switchboard` command and its link (only if they are
Switchboard's), the program, and its backups. Keeps `~/.config/switchboard` (configuration, key,
and chat membership) and the logs in `~/.local/share/switchboard`, unless `--purge` is given,
which deletes both directories (and lets launchd start the bridge again after a `down`). A
configuration elsewhere (`SWITCHBOARD_CONFIG`) is never
touched. The machine stays on the server until `sudo switchboard server revoke NAME` there.

### `switchboard setup`

The older, non-interactive way to join; `init` does the same and more. It is kept for scripts and
for one recovery case:

| Option | Meaning |
| --- | --- |
| `--invite -` | Read the invite from standard input. `SWITCHBOARD_INVITE` works too. The invite itself on the command line is refused |
| `--machine NAME`, `--owner NAME` | As for `init` |
| `--server-url URL` | Must match the invite's address (`--hub-url` is an older name) |
| `--server-pin SHA256` | The server certificate's fingerprint, when the invite doesn't carry it |
| `--no-start` | Don't install or start the bridge |

On a machine that is already set up, `switchboard setup --server-pin SHA256` trusts a server's new
private certificate: it accepts the pin only when the server no longer presents the certificate
this machine trusts and presents exactly the pinned one. The machine's key and enrollment are
unchanged, so this avoids a new invite after a private server's certificate was issued again.

### Output and exit status

Commands print readable output by default, with color on a terminal unless `NO_COLOR` is set or
`TERM` is `dumb`. With `--json`, every result and every error is one JSON document on standard
output; errors are `{"ok": false, "error": "..."}`. Without it, errors go to standard error as a
line starting with `✗`, sometimes a detail line, and the fix: `To fix, ...` for one step, or a
`To fix` heading over numbered steps, with each command to run on a line of its own. Usage errors
exit 2, other failures 1.

### What commands show

`status`, `chats`, `join`, `leave`, `ping`, `doctor`, `up`, `down`, and `help` build what they
show once (`switchboard/src/cli/view.rs`); the terminal draws it, and with `--json --display`
the result adds it as `display`, which the Claude Code plugin draws. `display` is a list of
blocks, each with a `kind`: `title` (`text`, optional `tone`: `success`, `warning`, or `error`),
`heading`, `fact` (`label`, `value`), `text` (optional `tone`, `strong`), `next` (`text` and a
`command` to run), `code`, `item` (a check: `text`, `tone`, optional `fix`), and `table`
(`columns`, `rows` of `cells` with an optional `note`); any block may have `space: true`.
Commands in text are in backticks.

## Environment

| Variable | Read by | Meaning |
| --- | --- | --- |
| `SWITCHBOARD_CONFIG` | All user commands | The bridge configuration's path |
| `SWITCHBOARD_INVITE` | `init`, `setup`, `install.sh` | An invite, so it stays off the command line |
| `SWITCHBOARD_VERSION` | `install.sh` | The release to install, such as `0.2.0` (default: the latest) |
| `SWITCHBOARD_REPO` | `install.sh`, `upgrade`, `init` | The GitHub repository releases and the plugin come from (default `krish-k7/switchboard`) |
| `SWITCHBOARD_GITHUB`, `SWITCHBOARD_GITHUB_API` | `install.sh`, `upgrade` | The GitHub web and API addresses, for mirrors and tests |
| `GITHUB_TOKEN` | `install.sh`, `upgrade` | A token for a private repository; sent only to the host it is meant for |
| `NO_COLOR` | All | Plain output without color |
| `SWITCHBOARD_NO_UPDATE_CHECK` | `status` | Set (to anything but `0`) to never ask GitHub for a newer release. The check is also off when `CI` is set (to anything but `false` or `0`) |
| `CLAUDE_CODE_MESSAGING_SOCKET` | `join`, `leave`, `status`, `chats` | Set by Claude Code inside a chat; identifies the current chat |
| `SWITCHBOARD_INIT_STUB_SERVER` | `init` | For trying the server path without root: a directory where a private test server runs as you |
| `SWITCHBOARD_INIT_STUB_ACME_ERROR` | `init` | With the stub, simulates a Let's Encrypt failure with this text |

## Files on a machine

| Path | What it is |
| --- | --- |
| `~/bin/switchboard` | The command; `~/.local/bin/switchboard` links to it |
| `~/.local/share/switchboard/switchboard` | The installed program |
| `~/.local/share/switchboard/backups/` | Files replaced by earlier installs |
| `~/.local/share/switchboard/daemon.log`, `daemon-error.log` | The bridge's logs: one JSON object per line, starting with `at`, the UTC time |
| `~/.config/switchboard/bridge.json` | The bridge configuration (below) |
| `~/.config/switchboard/membership.json` | The joined chats and the names other machines see; the plugin's footer label reads it |
| `~/.config/switchboard/down` | Present while this machine is down (`switchboard down`); `up` removes it |
| `~/.config/switchboard/server-cert.pem` | A private server's certificate, pinned |
| `~/.config/switchboard/identity/key.pem`, `cert.pem` | This machine's private key and certificate |
| `~/.config/switchboard/identity/identity.json` | The machine name, owner, and server it joined |
| `~/.config/switchboard/state/` | The bridge's private state: `runtime.json` (its process, connection, and published chats), `activity.json` (when each joined chat was last active), `delivered-ids.json` (messages already handed to a chat, kept an hour), and `update-check.json` (the latest release and when GitHub was last asked, or that asking failed) |

The directories are mode 0700 and the files 0600.

### Bridge configuration

`bridge.json` is written by `init`:

```json
{
  "enabled": true,
  "server_url": "https://switchboard.example.com",
  "ca": null,
  "cert": "/home/you/.config/switchboard/identity/cert.pem",
  "key": "/home/you/.config/switchboard/identity/key.pem",
  "config_dir": "/home/you/.claude",
  "state_dir": "/home/you/.config/switchboard/state",
  "sessions": []
}
```

| Key | Meaning |
| --- | --- |
| `enabled` | `false` keeps the bridge offline |
| `server_url` | The server's address (`hub_url`, the older name, is read when this is absent) |
| `ca` | `null` to trust the system's certificate authorities, or the path of a pinned certificate (a private server's) |
| `cert`, `key` | This machine's certificate and key |
| `config_dir` | Claude Code's configuration directory. `init` writes `~/.claude`; change it if Claude Code uses another directory |
| `state_dir` | The bridge's private state directory |
| `sessions` | The IDs of joined chats |
| `names`, `joined_at` | Per joined chat: the name other machines see, and when it was joined |

Edit membership with `join` and `leave`, not by hand: both take the configuration's lock.

## Server commands

Run these on the server as root (or as the `switchboard` account; `install`, `uninstall`, and
`logs` need root). Each reads the server's configuration, `/etc/switchboard/server.json` unless
`--config FILE` names another, and its state directly; nothing restarts.

### `switchboard server install`

```sh
switchboard server install --tls private [--name HOST]... [--port 8743]
switchboard server install --tls acme --domain NAME [--email ADDRESS] [--port 443]
switchboard server install --tls caddy [--domain NAME]
```

| Option | Meaning |
| --- | --- |
| `--tls MODE` | `acme`, `private`, or `caddy` (required) |
| `--domain NAME` | `acme`: the public DNS name. `caddy`: the name clients use, only for the printed invite |
| `--email ADDRESS` | `acme`: the contact for the certificate account |
| `--name NAME` | `private`: a host name or address machines connect to; repeat for more. Default: the previous install's names, else the host name and primary address |
| `--port PORT` | `acme` and `private`: the port to listen on |
| `--no-start` | Write everything, but don't enable, start, or restart the service |
| `--json` | Print the result as JSON: `server_url`, `server_pin`, `certificate_reissued`, `invite`, `started`, `changed` |

It creates the `switchboard` system account (home `/var/lib/switchboard`), the state directory, an
empty registry, the configuration, the program at `/usr/local/lib/switchboard/switchboard` with a
`/usr/local/bin/switchboard` link, and the unit `switchboard-server.service`. It starts the
service (restarting it when the program, configuration, unit, or private certificate changed),
waits up to 15 seconds for it to answer, and prints the first invite (single use, one hour) while
no machine has joined, replacing the one an earlier run printed. It refuses to replace a unit,
configuration directory, or program directory it didn't write, and never touches firewalls.

The unit runs `switchboard server --config /etc/switchboard/server.json` as `switchboard` with
`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`,
`MemoryMax=256M`, and no capabilities, except `CAP_NET_BIND_SERVICE` for a port below 1024. In
Caddy mode its group is `caddy` and it may open Unix sockets only.

In private mode, rerunning with names the certificate doesn't cover issues a new certificate and
prints its new fingerprint; machines that joined before must trust it again (a new invite and
`switchboard init`, or `switchboard setup --server-pin NEW`).

### `switchboard server status`

```text
switchboard server status [--config FILE] [--json]
```

Whether the server is healthy, read-only. Problems come first, each with its fix, then one row per
fact: the service (running or not, since when), the installed program's version, the mode and the
address machines use, the certificate (names, issuer, and expiry; the pin in private mode), the
listening addresses and whether they take IPv6 and IPv4, how many machines are enrolled and how
many were seen in the last 10 minutes, how many invites are open, the database's size, and the
errors in the journal over the last day (the newest three, with times). It never lists machines
or invites.

These need attention, and make it exit 1:

| Check | Failing | Warning |
| --- | --- | --- |
| `service` | Not installed, stopped, failed, or restarting in a loop; no systemd | Not enabled at boot; starting or stopping |
| `answer` | Running but not answering on its port or socket, or another program holds the port | |
| `certificate` | Missing, unreadable, or expired; public mode without a certificate yet | Under 7/30 of its lifetime left in public mode (21 days of 90), under 21 days in private mode; the server presents a different certificate than the file |
| `machines` | The registry can't be read | |
| `invites` | | The invites can't be read |
| `errors` | | Errors logged since the service last started |

With `--json` the result has stable fields: `ok` (no problems); `problems` (each `check`, `level`
`fail` or `warn`, `summary`, `fix`); `service` (`unit`, `state`: `running`, `starting`,
`restarting`, `stopping`, `stopped`, `failed`, `not_installed`, or `unknown`; `since`, `enabled`,
`pid`); `version` and `command_version`; `mode` (`private`, `acme`, `caddy`); `address`;
`socket`; `port`; `answering`; `certificate` (`path`, `subject`, `names`, `issuer`,
`self_signed`, `not_after`, `days_left`, `sha256`, `served`) or null in Caddy mode; `pin`;
`listening`, `ipv4`, `ipv6`; `machines` (`enrolled`, `seen_last_10_minutes`); `invites`
(`open`); `database` (`path`, `bytes`); and `errors` (`readable`, `last_day`, `since_start`,
`recent`, each like a `server logs --json` line). Times are seconds since the epoch. When the
status can't be read at all (no server here, or another user), the result is `ok: false` with
`error`, exit status 1.

### `switchboard server logs`

```text
switchboard server logs [--follow] [--lines N] [--since TIME] [--json]
```

The server's events from the systemd journal (`journalctl -u switchboard-server`), oldest first:
the local time, the event, and the detail, with errors marked `✗` and warnings `!`. The events
are the server's own JSON lines (`private_certificate`, `invite_redeemed`, `certificate_obtained`,
`certificate_error`, `invite_store_unavailable`, `registry_group_not_kept`), its other output
(`server_error` for a startup failure, `request_error`, `panic`), and systemd's lines about the
service (`started`, `stopping`, `stopped`, `exited`, `failed`, `restarting`).

| Option | Meaning |
| --- | --- |
| `-f`, `--follow` | Keep printing new events until interrupted |
| `-n`, `--lines N` | How many of the latest events (default 50) |
| `--since TIME` | `30m`, `2h`, `7d`, or `1w` ago, or any time `journalctl --since` reads (`"2026-10-31 09:00"`, `today`) |
| `--json` | One JSON object per line: `time`, `source` (`server` or `systemd`), `level` (`info`, `warning`, `error`), `event`, `detail`, and `message`, the line as logged |

It needs root or membership in a group that may read the journal (`systemd-journal`, `adm`).

### `switchboard server uninstall`

```text
switchboard server uninstall [--purge] [--json]
```

Run as root. Stops and disables `switchboard-server.service` and removes the unit, the program
directory `/usr/local/lib/switchboard`, and the `/usr/local/bin/switchboard` link (only when it
points at that program). Keeps `/etc/switchboard/server.json`, the registry, invites, messages,
and certificates in `/var/lib/switchboard`, and the `switchboard` account, listing each with its
path; installing again with the same options uses them. `--purge` also deletes the configuration,
the state, and then the account and its group.

Before changing anything it refuses a unit or program directory the installer didn't write (and,
with `--purge`, an unmanaged `/etc/switchboard` or a link in place of the state or certificate
directory). Inside the installer's directories it removes only the files the installer and the
server write, by name; anything else stays, with its directory, and is listed. Reruns are safe.
With `--json`: `ok`, `stopped` (it was running), `disabled`, `removed` (paths), `account_removed`, `kept` (each `kind`:
`configuration`, `registry`, `invites`, `messages`, `certificates`, or `other`; `path`; and
`machines` for the registry), and `account_kept`.

### `switchboard server invite`

```text
switchboard server invite [--expires DURATION] [--uses N] [--name MACHINE] [--url URL] [--json]
switchboard server invite revoke ID
```

| Option | Meaning |
| --- | --- |
| `--expires DURATION` | `30m`, `2h`, `7d`, or seconds; at least a minute, at most 30 days (default `1h`) |
| `--uses N` | How many machines may join with it, 1 to 1000 (default 1) |
| `--name MACHINE` | The name the machine must join under; such an invite has one use, so `--uses` above 1 is refused |
| `--url URL` | The address machines use (default: from the configuration; required in Caddy mode) |

At most 64 invites are open at once. With `--json` the result is `ok`, `id`, `invite`, `url`,
`server_pin`, `expires_at`, `uses`, and `machine`.

An invite is `sbi1.` followed by unpadded base64url JSON: `url`; `pin`, the certificate's SHA-256
in private mode; `secret`, 24 random bytes in base64url; and `machine` when the name is fixed. The
server stores only the secret's SHA-256; an invite's ID is the first 12 hex digits of that hash.

### `switchboard server invites`, `machines`, and `revoke`

| Command | Effect |
| --- | --- |
| `switchboard server invites [--json]` | List open invites: ID, fixed name, uses left, expiry. Never their secrets |
| `switchboard server invite revoke ID` | Cancel an open invite |
| `switchboard server machines [--json]` | List enabled machines with owner and when each was last seen (recorded at most once a minute) |
| `switchboard server revoke MACHINE` | Disable a machine's key; it stops working on its next request, and the name is free again |

### `switchboard server enroll-local`

A fallback over SSH for when no invite can be made: run as root on the server, it reads one JSON
request on standard input, `{"machine": ..., "owner": ..., "certificate_pem": ...}` (the
certificate from the new machine's `~/.config/switchboard/identity/cert.pem`), checks the
certificate, adds its fingerprint to the registry under the registry's lock, and prints one JSON
line. It requires a listener that accepts any client key and leaves authorization to the
registry: the built-in TLS modes, or Caddy configured as below. It restarts nothing. Exact reruns
change nothing; a revoked key stays revoked. `--help` describes it.

### `switchboard server --config FILE`

Run the server in the foreground with that configuration (the service runs it this way). SIGINT
stops it cleanly and removes its socket; SIGTERM ends it at once. Exit status 2 for usage errors
and unsafe files, 1 for other failures.

## Files on the server

| Path | What it is |
| --- | --- |
| `/etc/switchboard/server.json` | The configuration (mode 0600, owned by `switchboard`) |
| `/var/lib/switchboard/peers.json` | The registry: certificate fingerprint to `{machine, owner, enabled, ...}`. Reread on every request |
| `/var/lib/switchboard/peers.json.lock` | The lock every registry and invite change takes |
| `/var/lib/switchboard/invites.json` | Open invites, and used or expired ones for a week; hashes only |
| `/var/lib/switchboard/messages.sqlite3` | Messages (kept 7 days, at most 10,000), stored delivery notices, and last-seen times |
| `/var/lib/switchboard/tls/` | `private-cert.pem` and `private-key.pem` (private mode), or the Let's Encrypt account, certificate, and key (public mode) |
| `/usr/local/lib/switchboard/switchboard` | The program; `/usr/local/bin/switchboard` links to it |
| `/etc/systemd/system/switchboard-server.service` | The unit |
| `/run/switchboard/hub.sock` | Caddy mode: the socket Caddy forwards to |

### Server configuration

`server.json` selects one listener:

```json
{"tls": "private", "names": ["sb.lan", "10.0.0.5"], "db": "/var/lib/switchboard/messages.sqlite3", "peers_file": "/var/lib/switchboard/peers.json"}
{"tls": "acme", "hostname": "switchboard.example.com", "db": "/var/lib/switchboard/messages.sqlite3", "peers_file": "/var/lib/switchboard/peers.json"}
{"unix_socket": "/run/switchboard/hub.sock", "db": "/var/lib/switchboard/messages.sqlite3", "peers_file": "/var/lib/switchboard/peers.json"}
```

| Key | Modes | Meaning |
| --- | --- | --- |
| `tls` | built-in | `"private"` or `"acme"` |
| `names` | private | Names and addresses the certificate covers (default `localhost`, `127.0.0.1`, `::1`) |
| `hostname` | acme | The only name served |
| `acme_email` | acme | Account contact |
| `acme_directory` | acme | The ACME directory URL (default: Let's Encrypt production) |
| `acme_ca` | acme | PEM roots to trust for a test ACME server |
| `listen` | built-in | Address to listen on (default `::`, which serves IPv6 and IPv4, falling back to IPv4 where IPv6 is unavailable) |
| `port` | built-in | Default 443 (`acme`) or 8743 (`private`) |
| `unix_socket` | Caddy | The socket to listen on |
| `db` | all | The SQLite database; its directory is the state directory (mode 0700) |
| `peers_file` | all | The registry (mode 0600) |

Relative paths are relative to the configuration file. The file must be mode 0600. Built-in TLS
cannot be combined with `unix_socket`, `cert`, `key`, or `client_ca` (the last three belong to a
direct mutual-TLS listener used only by tests). In private mode the certificate and key are made
once and kept; every start logs the fingerprint as a `private_certificate` line.

### Behind Caddy

In Caddy mode the server listens on `/run/switchboard/hub.sock` (directory 0750, socket 0660,
group `caddy`) and trusts two headers that only Caddy may set. Caddy must require a client
certificate without a trust pool, so any key that proves possession gets through and the registry
decides, and must overwrite both headers. Run Caddy as the only member of the `caddy` group, with
this configuration (use your own host name, and make invites with
`switchboard server invite --url https://HOST`):

```caddyfile
{
	admin off
	persist_config off
	auto_https disable_redirects
	servers {
		protocols h1 h2
		strict_sni_host on
	}
}

https://switchboard.example.com {
	tls {
		issuer acme {
			disable_http_challenge
		}
		client_auth {
			mode require
		}
	}
	reverse_proxy unix//run/switchboard/hub.sock {
		header_up X-Switchboard-Client-Fingerprint {http.request.tls.client.fingerprint}
		header_up X-Switchboard-Client-Address {http.request.remote.host}
	}
}
```

`switchboard server enroll-local` reads this file from `/etc/switchboard-caddy/Caddyfile` (owned
`root:caddy`, mode 0640, in a `root:caddy` 0750 directory) and refuses to enroll unless it
requires client certificates this way.

## Names and limits

| Thing | Rule |
| --- | --- |
| Machine name | Lowercase letters, digits, hyphens; starts with a letter; at most 48 characters |
| Owner | One line, at most 64 characters |
| Name other machines see for a chat | Letters, digits, `_`, `-`; at most 26 characters |
| Remote chat alias in `ListAgents` | `switchboard/<machine>/<name>-<first 8 of the chat ID>`: machine shortened to 16 characters, at most 64 in all |
| Joined chats per machine | 32 |
| Joined chat inactivity limit | 7 days |
| Invite | Default one use and one hour; at most 1000 uses and 30 days; 64 open at once |
| Message text | 8 KiB |
| Messages kept on the server | 7 days, at most 10,000 |
| Unconfirmed delivery | Reported to the sender as unknown after 30 seconds |
