# Install Switchboard on this machine

These are instructions for an agent installing Switchboard for its user. A person can follow them
too; [README.md](README.md) is the shorter guide.

Install only this machine's part of Switchboard. Do not join chats, change the server, change
firewalls, copy another machine's keys, or change Claude Code settings as part of installing.
Report readiness and leave the choice of chats to the user.

A machine joins a Switchboard server with an invite. Someone with root on the server (or its
`switchboard` account) runs `sudo switchboard server invite` and gives the printed `sbi1.` string
to the user of the new machine. The invite carries the server's address and, for a server on a
private network, its certificate's fingerprint. Nothing on the server needs to change, and no SSH
access to it is needed. Treat the invite like a password until it is used: anyone holding it can
join one machine before it expires (one hour by default).

## 1. Check prerequisites

macOS or Linux (x86_64 or arm64), `curl` (or `wget`), and Claude Code already installed and run
at least once (the bridge needs `~/.claude`). No root access is needed.

```sh
claude --version
```

The version is for the report only. Switchboard does not require a specific Claude Code release;
do not upgrade or downgrade Claude Code because of it.

## 2. Install and join

Ask the user for the invite. Avoid having them paste it into the chat if you can, and never print
it, write it to a file, or put it on a command line (other users can see command lines in `ps`,
and shells keep them in history). Switchboard refuses an invite given as `--invite VALUE`.

If the user can run a command in their own terminal, have them run this; the setup wizard asks
for the invite there, without echo:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh -s -- --invite --no-plugin
```

Without a terminal, pass the invite in the environment, where `switchboard init` reads it:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | SWITCHBOARD_INVITE="$INVITE" sh -s -- --no-plugin
```

Or install first and join afterwards, with the invite on standard input:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
printf '%s\n' "$INVITE" | ~/bin/switchboard init --invite - --no-plugin
```

Options after `sh -s --` go to `switchboard init`:

- `--no-plugin` skips the Claude Code plugin, which would change Claude Code's settings. Without
  a terminal, init never installs it unless given `--plugin`.
- `--name NAME` sets the machine's name: lowercase letters, digits, and hyphens, starting with a
  letter, at most 48 characters. The default is the host name, unless the invite fixes a name.
- `--owner NAME` sets the owner shown on the server. The default is the user name.
- `--no-start` joins without installing or starting the bridge.

Without a terminal, init asks nothing. A missing answer stops it with exit status 2 and names the
option that gives it.

What happens:

1. The script downloads the release binary for this OS and architecture and checks it against the
   release's `SHA256SUMS`. It installs the program in `~/.local/share/switchboard`, the
   `switchboard` command in `~/bin` (linked from `~/.local/bin`), and the bridge's user service (a
   launchd agent on macOS, a systemd user unit on Linux). It does not change Claude Code settings.
2. `switchboard init` reads the server's address from the invite and checks the server's
   certificate, against the invite's fingerprint or the system's trusted authorities. It stops
   before making anything if the check fails.
3. It makes the machine's private key and certificate in `~/.config/switchboard/identity/`,
   writes `~/.config/switchboard/bridge.json` with no chats joined, redeems the invite, starts the
   bridge, and waits until the bridge connects.

A successful run without a terminal looks like this:

```text
✓ Server        localhost:51573, certificate matches the invite
✓ Machine name  laptop
✓ Owner         krish

✓ Joined the server as laptop
✓ Started the bridge, the background service that connects this machine's
  chats
```

Keep `~/.config/switchboard/` at mode 0700 and its files at 0600. Never print the private key or
commit these files. Keys don't expire, so nothing needs renewing.

`~/bin` may not be on the `PATH`. Use `~/bin/switchboard` directly, or add
`export PATH="$HOME/bin:$PATH"` to the user's shell startup file only if they authorize that
edit. For bash on macOS that is the first of `~/.bash_profile`, `~/.bash_login`, and `~/.profile`
that exists: making a new `~/.bash_profile` would stop bash from reading `~/.profile`.

## 3. Check the connection

```sh
~/bin/switchboard status
```

It should start with `✓ Connected to <server> as <machine>`. Report the machine name and that it
is connected. No joined chats is the expected state after installing. Do not send a test message
or join the user's chats to demonstrate the installation.

## 4. Install the rule for peer messages

Read [CLIENT_INSTRUCTIONS.md](CLIENT_INSTRUCTIONS.md) and install it as the regular file
`~/.claude/rules/switchboard.md` (with a custom Claude Code configuration directory, its
`rules/switchboard.md`). It tells agents how to weigh messages from other chats: use independent
judgment, carry out sound requests within their authority, and choose recipients by topic. It is
a persistent instruction for every chat, not text added to messages.

```sh
mkdir -p "$HOME/.claude/rules"
```

Copy `CLIENT_INSTRUCTIONS.md` to that file. Keep the user's other rules. If `switchboard.md`
already exists, compare it first and keep the user's own additions. Do not add path-scoping
frontmatter: the rule is about messages across projects. Claude Code's documentation describes
[user-level rules](https://code.claude.com/docs/en/memory#user-level-rules).

New chats load the rule automatically. A chat that is already running should read the file once
before using Switchboard. Include the installed path in your report. The rule guides judgment; it
does not enforce anything.

## 5. Let the user choose chats

In each chat the user wants connected, the user runs:

```text
!switchboard join
```

(or `/switchboard join` with the plugin). `!switchboard join research` sets the name other
machines see; without a name, they see the chat's own name. If the chat's shell doesn't have
`~/bin` on its `PATH`, use `!~/bin/switchboard join`. The command answers once the bridge has
published the chat. Claude can then use its own `ListAgents` and `SendMessage` tools to reach
chats on other machines. `!switchboard leave` disconnects the chat, and a joined chat leaves on
its own after 7 days without activity.

From outside a chat, `switchboard chats` lists chats with their IDs, and
`switchboard join --chat ID` or `switchboard leave --chat ID` acts on one. Use these only when the
user has explicitly chosen that chat.

Switchboard has no command for reading messages. A message reported as handed off reached the
other chat; that does not show it was read or acted on.

## Upgrades, removal, and recovery

- `switchboard upgrade` installs the latest release (checksum checked) and restarts the bridge if
  it was running. `switchboard upgrade 1.2.3` installs that release; moving to an older one needs
  `--force`. Rerunning the install command without options also upgrades in place. After
  upgrading, refresh `~/.claude/rules/switchboard.md` from the new `CLIENT_INSTRUCTIONS.md`.
- `switchboard down` takes the machine offline until `switchboard up`, across logins and
  restarts; upgrades and reinstalls leave it down. Run either only when the user asks.
  `switchboard logs` shows what the bridge has been doing.
- `switchboard uninstall` stops the bridge and removes its service, the `switchboard` command and
  its link, and the program and its installer backups. It keeps `~/.config/switchboard` (the
  configuration, this machine's key, and chat membership) and the logs in
  `~/.local/share/switchboard`. `--purge` deletes both directories as well, but not a
  configuration kept elsewhere through `SWITCHBOARD_CONFIG`. Neither removes the machine from the
  server; that takes `sudo switchboard server revoke NAME` on the server.
- `switchboard init` without an invite, on a machine that is set up, shows the server, the
  bridge, and the plugin, and offers what fits. It reuses the existing key and configuration and
  never overwrites them. After a refused invite, run it again with a new one.
- Do not delete identity files to force a fresh start. A new key is a machine the server has
  never seen.

If installing fails, report the error exactly as printed and the step that failed. Then run
`switchboard doctor`, which checks the installation, the configuration and file permissions, the
bridge, the server's acceptance of this machine's key, and chat discovery, and gives a fix for
each problem. It changes nothing. It exits 1 when a check fails; warnings alone exit 0.
`switchboard doctor --json` gives the same checks as JSON.

Common causes:

- **The invite:** used, expired, or not accepted means asking the user for a new one. A name
  already on the server means choosing another `--name` (or an invite without a fixed name).
- **The certificate:** a certificate that doesn't match the invite means the server changed or
  something intercepts the connection; ask for a new invite. Never turn certificate checking
  off.
- **The bridge doesn't connect:** read `~/.local/share/switchboard/daemon.log` and
  `daemon-error.log`, and check DNS and that the server is reachable.
- **Chats:** `switchboard chats` lists the chats it can see. Do not invent chat IDs or join every
  chat.

[docs/troubleshooting.md](docs/troubleshooting.md) lists the messages and their fixes.
