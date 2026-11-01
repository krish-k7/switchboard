# Set up Switchboard

These are instructions for an agent setting up Switchboard for its user, working from the user's
own machine ("this machine"). They cover joining this machine to a Switchboard server and, when
the user has no server yet, setting one up over SSH on a Linux machine the user chooses. A person
can follow them too; [README.md](README.md) is the shorter guide.

## Rules

- Do only what these steps say. Don't join chats, change firewalls, copy keys between machines,
  or change Claude Code settings. Leave the choice of chats to the user.
- Never choose a machine, a mode, or a domain yourself. Ask, and confirm what you found.
- An invite lets one machine join until it is used or expires (one hour by default). If the user
  gives you one, use it (step 5 says how to pass it).
- When a step fails, report the error exactly as printed and the step that failed.

## 1. Check this machine

```sh
uname -sm
claude --version
~/.local/bin/switchboard status || ~/bin/switchboard status
```

Switchboard runs on macOS and Linux, x86_64 or arm64. It needs `curl` (or `wget`) and Claude
Code installed and run at least once (the bridge needs `~/.claude`). If `claude` isn't found, ask
the user to install Claude Code and run it once. Its version is for the report only; don't upgrade
or downgrade Claude Code because of it. No root access is needed on this machine.

Neither command exists before the first install (`~/bin/switchboard` is where releases before
0.3.1 put it; installing the current release moves it). If one does, this machine was set up
before, and `server_url` in `~/.config/switchboard/bridge.json` is its server:

- `status` starts with `✓ Connected to`: report that and stop, unless the user wants a change.
- `status` says the bridge isn't running, or that the machine is down: it is set up but offline.
  Run `~/.local/bin/switchboard doctor` and report its fixes; run `switchboard up` only if the
  user asks.
- The user wants this machine on a different server: join that server in step 5 with `--replace`
  added to `init`. The current setup is moved aside, and put back if joining fails.

## 2. Find the server

Look before you ask. The `Host` entries in `~/.ssh/config` are the SSH aliases the user already
has; read the file, don't change it. Then ask the user, with what you found:

- **They already have a server:** which SSH host reaches it. Without SSH access to it, they get an
  invite from whoever runs it; skip to step 5.
- **They don't have one:** which Linux machine should run it, as an SSH host or alias, or this
  machine if it runs Linux. And where machines will connect from:
  - **A private network** (Tailscale, a VPN, or a LAN). The server makes its own certificate,
    and each invite carries its fingerprint. It listens on port 8743. This is the simpler choice.
  - **Anywhere, with a public domain.** It needs a DNS name pointing at the server, port 443 open
    to the internet, and gets a certificate from Let's Encrypt. Ask for the domain, and
    optionally an email address for the certificate account. Check the prerequisites in
    [docs/server.md](docs/server.md#public-domain-mode) with the user first.

In the commands below, `SERVER` is the server's SSH host or alias. `-o BatchMode=yes` makes `ssh`
fail rather than wait for a password or host-key prompt you can't answer.

If this machine is to be the server, skip steps 3 and 4. Install as in step 5, then set up the
server and join it in one command (with `--tls acme --domain DOMAIN` in place of
`--tls private --server-name ADDRESS` for a public domain):

```sh
~/.local/bin/switchboard init --server --join --tls private --server-name ADDRESS --no-plugin
```

It runs `sudo -n` once for the install. If sudo needs a password, have the user run
`~/.local/bin/switchboard init` in their own terminal and choose **Run the server and join it**. Then go
to step 6.

## 3. Check the server machine

```sh
ssh -o BatchMode=yes SERVER 'uname -sm
test -d /run/systemd/system || echo no-systemd
test "$(id -u)" = 0 && echo root
sudo -n true 2>/dev/null && echo sudo-ok || echo sudo-needs-password
test -e /etc/systemd/system/switchboard-server.service && echo server-installed || echo no-server'
```

- If `ssh` fails, ask the user to fix SSH access, for example by running `ssh SERVER` once in
  their own terminal to accept the host key.
- The server needs Linux, x86_64 or arm64, running systemd: `no-systemd` rules the machine out.
- `root` means you log in as root: leave `sudo -n` out of every command on the server.
- `sudo-ok` means you can run `sudo -n` there. The commands here call the server's program by its
  full path, `/usr/local/bin/switchboard`, because some distributions' sudo doesn't search
  `/usr/local/bin`. With `sudo-needs-password`, sudo asks for a password
  you can't type. The user then runs step 4's commands in their own terminal, with `ssh -t` in
  place of `ssh -o BatchMode=yes` and `sudo` in place of `sudo -n` (for example
  `ssh -t SERVER 'sudo /usr/local/bin/switchboard server status'`), and joins this machine the
  second way in step 5.
- `server-installed` means a Switchboard server is already installed there. Skip step 4; don't
  reinstall it.

## 4. Install the server

Only when the user has no server. For a private network, first find the address this machine
uses to reach the server:

```sh
ssh -G SERVER | awk '/^hostname /{print $2}'
```

Confirm it with the user; if it prints the alias itself, ask for the real host name or address.
Invites point at this address, so every machine that joins must be able to reach it (a Tailscale
name or address works from anywhere on the tailnet). Then download the release on the server,
check it against the release's checksums, and install it. Replace `OPTIONS` with
`--tls private --name ADDRESS` for a private network, or with
`--tls acme --domain DOMAIN --email EMAIL` for a public domain (`--email` is optional):

```sh
ssh -o BatchMode=yes SERVER 'set -e
case $(uname -m) in x86_64 | amd64) arch=x86_64 ;; aarch64 | arm64) arch=arm64 ;; esac
dir=$(mktemp -d)
trap "rm -rf $dir" EXIT
cd "$dir"
base=https://github.com/krish-k7/switchboard/releases/latest/download
curl -fsSLO "$base/switchboard-linux-$arch"
curl -fsSLO "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS >&2
chmod +x "switchboard-linux-$arch"
sudo -n "./switchboard-linux-$arch" server install OPTIONS --no-invite --json'
```

The installer creates the `switchboard` system account, `/etc/switchboard/server.json`,
`/var/lib/switchboard/`, the program in `/usr/local/lib/switchboard` (linked from
`/usr/local/bin/switchboard`), and the `switchboard-server` systemd service, then starts it and
waits until it answers. Success is one JSON line with `"ok": true` (shortened here), and a failure
is an error message or `{"ok": false, "error": ...}` with a nonzero exit status.

```text
{"ok": true, "server_url": "https://ADDRESS:8743", "server_pin": "9fe3cade…", "certificate_reissued": false, "invite": null, "started": true, "changed": [...], "setup": "switchboard init"}
```

`--no-invite` keeps the installer from making the first machine's invite, which it would
otherwise print; step 5 makes one and pipes it straight to `init`. The installer never changes
firewalls. If a firewall or cloud
security group protects the server, port 8743 (or 443) must be open to the machines that join;
ask the user before changing one.

Then check the server:

```sh
ssh -o BatchMode=yes SERVER 'sudo -n /usr/local/bin/switchboard server status'
```

It starts with `✓ The server is healthy.` and exits 0. In public mode it reports a problem until
the first certificate arrives from Let's Encrypt; wait a minute and run it again, and don't go on
to step 5 until it is healthy. Certificate errors and their fixes are in
[docs/troubleshooting.md](docs/troubleshooting.md).

## 5. Install and join this machine

Install the program for this user:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
```

It downloads the release binary for this machine, checks it against the release's `SHA256SUMS`,
and installs it as the command `~/.local/bin/switchboard`, with the bridge's user service (a
launchd agent on macOS, a systemd user unit on Linux, logging to `~/.local/share/switchboard`),
without starting it. On a machine set up before 0.3.1 it moves the installation from
`~/bin/switchboard`. Without a terminal it stops there, ending with `Next` and
`switchboard init`.

Then join with an invite, in one of two ways:

- **With SSH and `sudo -n` on the server**, make the invite there and pipe it straight into
  `init`:

  ```sh
  ssh -o BatchMode=yes SERVER 'sudo -n /usr/local/bin/switchboard server invite --json' |
      grep -o 'sbi1\.[A-Za-z0-9_-]*' | ~/.local/bin/switchboard init --invite - --no-plugin
  ```

  If `init` says `No invite on standard input`, the command on the server failed and printed its
  error above. A server behind Caddy needs `--url https://DOMAIN` added to `server invite`; a
  server installed some other way may keep its program or configuration elsewhere, so ask the
  user.

- **With an invite the user gives you** (made on the server with
  `sudo switchboard server invite`), pass it to `init` on standard input. `init` takes the invite
  only that way, from `SWITCHBOARD_INVITE`, or typed at its prompt; it refuses `--invite VALUE`.
  Put the invite in the shell variable `INVITE` (keep it whole: it starts with `sbi1.`; line
  breaks and spaces a chat added are fine), then:

  ```sh
  printf '%s\n' "$INVITE" | ~/.local/bin/switchboard init --invite - --no-plugin
  ```

  The user can also run `~/.local/bin/switchboard init` in their own terminal and paste it when
  asked.

`init` checks the server's certificate against the invite (or, for a public domain, the system's
trusted authorities) and stops before making anything if the check fails. It then makes this
machine's key in `~/.config/switchboard/identity/`, writes `~/.config/switchboard/bridge.json`
with no chats joined, redeems the invite, starts the bridge, and waits until it connects:

```text
✓ Server        sb.lan:8743, certificate matches the invite
✓ Machine name  laptop
✓ Owner         krish

✓ Joined the server as laptop
✓ Started the bridge, the background service that connects this machine's
  chats
```

Options for `init`:

- `--no-plugin` skips the Claude Code plugin, which would change Claude Code's settings. Without
  a terminal, init installs it only when given `--plugin`; pass that only if the user asks.
- `--name NAME` sets the machine's name: lowercase letters, digits, and hyphens, starting with a
  letter, at most 48 characters. The default is the host name, unless the invite fixes a name.
- `--owner NAME` sets the owner shown on the server. The default is the user name.
- `--no-start` joins without installing or starting the bridge.

Without a terminal, init asks nothing; a missing answer stops it with exit status 2 and names the
option that gives it. `init` also reads the invite from `SWITCHBOARD_INVITE`.

Keep `~/.config/switchboard/` at mode 0700 and its files at 0600. Never print the private key or
commit these files. Keys don't expire, so nothing needs renewing.

`~/.local/bin` may not be on the `PATH` (Claude Code's own installer uses it too, so it often is).
Use `~/.local/bin/switchboard` directly, or add `export PATH="$HOME/.local/bin:$PATH"` to the
user's shell startup file only if they authorize that edit. For bash on macOS that is the first of `~/.bash_profile`, `~/.bash_login`, and `~/.profile`
that exists: making a new `~/.bash_profile` would stop bash from reading `~/.profile`.

## 6. Check the connection

```sh
~/.local/bin/switchboard status
~/.local/bin/switchboard doctor
```

`status` should start with `✓ Connected to <server> as <machine>`. `doctor` should exit 0; on a
healthy machine it ends with `All checks passed.` Report any warnings it lists, with their fixes.
No joined chats is the expected state. Don't send a test message or join the user's chats to
demonstrate the installation.

## 7. Install the rule for peer messages

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
before using Switchboard. The rule guides judgment; it does not enforce anything.

## 8. Report, and let the user choose chats

Report:

- this machine's name, the server's address, and that it is connected;
- what you installed on each machine: on the server (if you set it up), the mode, the address and
  port, and the paths and service from step 4; on this machine, `~/.local/bin/switchboard`,
  `~/.local/share/switchboard`, `~/.config/switchboard`, the bridge's service, and the rule file;
- anything the user must still do, such as opening a firewall port;
- how to add another machine: run the same setup there, with a new invite.

Then tell the user how to connect chats. In each chat they want connected, they run:

```text
!switchboard join
```

(or `/switchboard join` with the plugin). `!switchboard join research` sets the name other
machines see; without a name, they see the chat's own name. If the chat's shell doesn't have
`~/.local/bin` on its `PATH`, use `!~/.local/bin/switchboard join`. Claude can then use its own `ListAgents` and
`SendMessage` tools to reach chats on other machines. `!switchboard leave` disconnects the chat,
and a joined chat leaves on its own after 7 days without activity.

From outside a chat, `switchboard chats --details` lists chats with their IDs, and
`switchboard join --chat ID` or `switchboard leave --chat ID` acts on one. Use these only when the
user has explicitly chosen that chat.

Switchboard has no command for reading messages. A message reported as handed off reached the
other chat; that does not show it was read or acted on.

## Upgrades, removal, and recovery

- `switchboard upgrade` installs the latest release (checksum checked) and restarts the bridge if
  it was running. `switchboard upgrade 1.2.3` installs that release; moving to an older one needs
  `--force`. Rerunning the install command without options also upgrades in place. After
  upgrading, refresh `~/.claude/rules/switchboard.md` from the new `CLIENT_INSTRUCTIONS.md`. The
  server upgrades separately: [docs/server.md](docs/server.md#upgrading-the-server).
- `switchboard down` takes the machine offline until `switchboard up`, across logins and
  restarts; upgrades and reinstalls leave it down. Run either only when the user asks.
  `switchboard logs` shows what the bridge has been doing.
- `switchboard uninstall` stops the bridge and removes its service, the program
  (`~/.local/bin/switchboard`), and its installer backups. It keeps `~/.config/switchboard` (the
  configuration, this machine's key, and chat membership) and the logs in
  `~/.local/share/switchboard`. `--purge` deletes both directories as well, but not a
  configuration kept elsewhere through `SWITCHBOARD_CONFIG`. Neither removes the machine from the
  server; that takes `sudo switchboard server revoke NAME` on the server.
- `switchboard init` without an invite, on a machine that is set up, shows the server, the
  bridge, and the plugin, and offers what fits. It reuses the existing key and configuration and
  never overwrites them. After a refused invite, run it again with a new one.
- Do not delete identity files to force a fresh start. A new key is a machine the server has
  never seen.

If something fails, run `switchboard doctor`. It checks the installation, the configuration and
file permissions, the bridge, the server's acceptance of this machine's key, and chat discovery,
and gives a fix for each problem. It changes nothing. It exits 1 when a check fails; warnings
alone exit 0. `switchboard doctor --json` gives the same checks as JSON. On the server,
`sudo switchboard server status` does the same for the server.

Common causes:

- **The invite:** used, expired, or not accepted means making a new one. A name already on the
  server means choosing another `--name` (or an invite without a fixed name).
- **The certificate:** a certificate that doesn't match the invite means the server changed or
  something intercepts the connection; make a new invite. Never turn certificate checking off.
- **The server can't be reached:** check that this machine reaches the invite's address and port
  (8743 private, 443 public), and that no firewall or security group blocks it.
- **The bridge doesn't connect:** read `~/.local/share/switchboard/daemon.log` and
  `daemon-error.log`, and check DNS and that the server is reachable.
- **Chats:** `switchboard chats` lists the chats it can see. Do not invent chat IDs or join every
  chat.

[docs/troubleshooting.md](docs/troubleshooting.md) lists the messages and their fixes.
