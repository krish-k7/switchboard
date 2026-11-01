# Troubleshooting

Start with these two commands on the machine that has the problem:

```sh
switchboard status     # connected or not, and whether this chat is joined
switchboard doctor     # every check, with the fix for each problem
switchboard logs       # what the bridge has been doing
```

`doctor` only reads; it never repairs, restarts, or changes anything. It exits with status 1 when
a check fails (warnings alone leave it at 0), and `switchboard doctor --json` gives the same
checks for scripts. Anything that needs attention comes first, each failure or warning with its
fix, then the checks that passed, one per line. A healthy machine looks like this:

```text
✓ Switchboard 0.3.0, installed in ~/.local/bin.
✓ Configuration and key are private and valid.
✓ The bridge is running and connected.
✓ The server knows this machine as laptop (owner krish).

  All checks passed.
```

With the bridge stopped:

```text
✗ The bridge isn't running.
  To fix, run
      switchboard up

✓ Switchboard 0.3.0, installed in ~/.local/bin.
✓ Configuration and key are private and valid.
✓ The server knows this machine as laptop (owner krish).

  1 problem to fix.
```

**A machine that is down.** `switchboard down` takes a machine off Switchboard on purpose: its
bridge stops and stays stopped after logins and restarts, and installs, upgrades, and `init`
leave it that way. `status` and `doctor` say so instead of reporting a failure:

```text
  Down. Run switchboard up to reconnect.
```

`switchboard up` brings it back. The file `down` beside the configuration
(`~/.config/switchboard/down`) records the state.

Running `switchboard init` again is also safe on a machine that is set up: it shows the server,
the bridge, and the plugin, and offers the fixes that apply (join again with a new invite, start
the bridge or bring a machine that is down back up, install the plugin).

**Logs.** `switchboard logs` shows the bridge's latest events (20 unless `--lines N`), one per
line with its local time, and `--follow` keeps showing new ones:

```text
  Oct 31 04:21:27  request failed  register: server HTTP 403 (not_enrolled)
  Oct 31 04:22:10  connected       to the server as laptop
```

What the bridge printed on standard error follows under its own heading. The files are
`~/.local/share/switchboard/daemon.log` (one JSON object per line, starting with the UTC time;
`switchboard logs --json` prints those lines as they are, with any control characters escaped)
and `daemon-error.log`.

If `daemon-error.log` says `there is no configuration at ...`, this machine left its server (or
its configuration was removed). The service starts the bridge only while the configuration
exists, so the line appears once; run `switchboard init` to join a server again.

On the server, `sudo switchboard server status` checks its health (see
[What server status reports](#what-server-status-reports)) and `sudo switchboard server logs` shows
its events.

Messages below are quoted as Switchboard prints them. Each error is a line starting with `✗`,
sometimes a line of detail, and the fix: `To fix, ...` for one step, or a `To fix` heading over
numbered steps.

## Installing

| Message | What to do |
| --- | --- |
| `Switchboard does not run on ...` or `Switchboard has no build for ... processors.` | Switchboard runs on macOS and Linux, on x86_64 or arm64. |
| `Downloading Switchboard needs curl or wget, and this machine has neither.` | Install `curl` and run the command again. |
| `The download of ... does not match the release's checksum, so nothing was installed.` | Run it again. If it fails the same way, the release itself is damaged. |
| `The --invite option needs a terminal to ask for the invite on.` | You ran the installer with `--invite` but no terminal (for example from a script). Set `SWITCHBOARD_INVITE` instead. |
| `The --invite option doesn't take the invite on the command line, where other users can see it.` | You passed the invite itself after `--invite`. Set `SWITCHBOARD_INVITE` instead, or use `--invite` alone and paste the invite when asked. |
| `Could not reach <host> (...)` | Check the machine's internet connection. |
| `unmanaged command: ~/.local/bin/switchboard` | Something Switchboard didn't write is at the command's path (a file, or a link to anything but the old `~/bin/switchboard`). Move it aside if you don't need it, then install again. |
| `! Left ~/bin/switchboard alone: Switchboard didn't write it` | A `~/bin/switchboard` of your own stays. If `~/bin` comes before `~/.local/bin` on your `PATH`, it runs instead of Switchboard; rename or remove it. |
| `! Kept ~/bin/switchboard as a link to the command: ...` | The Claude Code plugin here is older than 0.3.1 and runs that path. Run `claude plugin marketplace update switchboard` (then `/reload-plugins` in open chats); the next install or upgrade removes the link. |

Running the install command on a machine that is already set up only upgrades it. It restarts the
bridge if it was running.

**The command moved in 0.3.1.** The program is now `~/.local/bin/switchboard` itself; releases
before it had a `~/bin/switchboard` script that ran a copy in `~/.local/share/switchboard`. The
first install or upgrade to 0.3.1 moves the installation and removes what the earlier
installers wrote there (the script, the old copy once the bridge no longer runs it, and the
Python releases' files, by name), keeping the newest five installer backups. What changes for
you:

- **`switchboard: command not found`.** `~/.local/bin` isn't on your shell's `PATH` (`~/bin` may
  have been). Run `switchboard init` and accept the offer to add it, or add
  `export PATH="$HOME/.local/bin:$PATH"` to your shell's startup file yourself. Until then,
  `~/.local/bin/switchboard` works.
- **Tab completion stopped.** The line init added before 0.3.1 loads completion from
  `~/bin/switchboard` and skips itself once that's gone. Run `switchboard init` on a terminal and
  accept the offer to turn completion on again (or replace `$HOME/bin/switchboard` with
  `$HOME/.local/bin/switchboard` in that line).
- **`/switchboard` says `Switchboard isn't installed on this machine (~/bin/switchboard is
  missing)`.** That's a plugin older than 0.3.1 on a machine where Claude Code couldn't be asked
  about it during the install (for example, `claude` wasn't on `PATH`). Update the plugin with
  `claude plugin marketplace update switchboard`, then `/reload-plugins`.

## Joining a server

These come from `switchboard init` when it joins with an invite.

**The invite.**

| Message | Cause and fix |
| --- | --- |
| `The invite can't be used.` `The invite is damaged; copy it again in full.` | The text isn't a whole invite. Copy all of it again; it starts with `sbi1.`. Line breaks and spaces are fine. |
| `This invite was already used.` | Each invite works for one machine by default. Make a new one on the server with `sudo switchboard server invite`. |
| `This invite has expired.` | Invites last an hour by default. Make a new one (`--expires 1d` makes one that lasts a day). |
| `The server didn't accept this invite.` | It was revoked, or the server doesn't know it. Make a new one. |
| `The invite is for the machine name build-box, not laptop.` | The invite was made with `--name`. Leave out `--name`, or ask for an invite without a fixed name. |
| `--invite doesn't take the invite on the command line, ...` | `switchboard init --invite sbi1...` (or `setup --invite sbi1...`) is refused, because other users could see the invite in `ps`. Pipe it in (`printf '%s\n' "$INVITE" \| switchboard init --invite`), set `SWITCHBOARD_INVITE`, or run `switchboard init --invite` in a terminal and paste it. |
| `No invite on standard input.` | `--invite -` (or `--invite` alone) without a terminal reads the invite from standard input, and nothing came. Pipe it in, or set `SWITCHBOARD_INVITE`. |
| `Nothing to set up: no invite and no server options.` | Init has no terminal to ask on. Pass `--invite -` with the invite on standard input, or `--server --tls private` to run the server here. |

**Reaching the server.**

| Message | Cause and fix |
| --- | --- |
| `Can't reach the server at <host>.` followed by `Connection refused.` or `Nodename nor servname provided, or not known.` | The server isn't running, or this machine can't reach it. Check that the server is up (`systemctl status switchboard-server` there), that its port is open, and that Tailscale or your VPN is on. |
| `The server sent a different certificate than the invite names.` | The server's certificate changed after the invite was made (for example, its private-mode names changed), or something is intercepting the connection. Nothing was made on this machine. Make a new invite. |
| `This machine doesn't trust the server's certificate.` | The invite has no fingerprint, but the server's certificate isn't from an authority this system trusts. Invites from a private-mode server always carry the fingerprint, so make a new invite on the server. For a public server, check that its certificate comes from Let's Encrypt and that no proxy sits in front of it. |
| `The server at <host> didn't answer.` | The request was sent but no reply came. Check the network and run `switchboard init` again; it reuses this machine's key. |

**The name.**

| Message | Cause and fix |
| --- | --- |
| `A machine named workstation is already on this server.` | Choose another name: `switchboard init --name ANOTHER-NAME`. If the old machine is gone, revoke it on the server first. |
| `Bad_Name! can't be a machine name.` `Names use lowercase letters, digits, and hyphens. Try bad-name.` | Machine names are lowercase letters, digits, and hyphens, start with a letter, and are at most 48 characters. |

After a refusal like these, init removes the key and configuration it had just made (`Nothing was
left on this machine.`), so running it again starts clean.

**After joining.**

| Message | Cause and fix |
| --- | --- |
| `Claude Code hasn't run on this machine yet (~/.claude is missing), so the bridge wasn't started.` | Start Claude Code once, then run `switchboard up`. |
| `This machine is down (taken offline with switchboard down), so the bridge wasn't started.` | Init leaves a machine that is down as it is. Run `switchboard up` when you want it online. |
| `The bridge's service is turned off in launchd (launchctl disable), though this machine wasn't taken down with switchboard down.` | Something disabled the launchd job without `switchboard down` (by hand, or an earlier version). Run `switchboard up`, which enables it again and starts the bridge. |
| `The bridge's service couldn't be allowed to start again (...), so this machine is still down.` | `switchboard up` couldn't enable the service, so it left the machine marked down. Run the command the message gives (`launchctl enable gui/UID/dev.switchboard.bridge`, or `systemctl --user enable switchboard-bridge.service`), then `switchboard up`. |
| `Started the bridge ..., but it hasn't connected to the server yet` | Usually the network. Run `switchboard doctor`. |
| `This machine is set up for the server at <url>, not <url>.` | The machine already belongs to another server. Run `switchboard init` in a terminal and choose to join a different server, or pass `--replace`. Either way the current setup is moved aside to `~/.config/switchboard.before-<time>` and put back if joining fails. |
| `The Claude Code plugin wasn't installed.` | The machine is joined either way. Update Claude Code (`claude update`) and run `claude plugin install switchboard --marketplace krish-k7/switchboard`. Without a terminal, init installs the plugin only with `--plugin`. |

## Running the server with init

| Message | Cause and fix |
| --- | --- |
| `Port 443 is already in use on this machine.` | Another program listens there. Find it with `sudo ss -ltnp 'sport = :443'` and stop it, or pass `--port` with a free port. |
| `switchboard.example.com doesn't resolve to an address.` | Add an A record (or AAAA) for the domain pointing at this machine's public address, wait for it to spread, and check again. |
| `switchboard.example.com points to 203.0.113.7, which isn't this machine's address.` | A warning, not a stop. It is expected behind NAT or a cloud load balancer that forwards port 443. Otherwise fix the DNS record, or Let's Encrypt will fail. |
| `Let's Encrypt couldn't reach this machine on port 443.` | Allow inbound TCP on that port in the machine's firewall and in your cloud security group or router. If the domain has an AAAA record, that address must reach the machine too. |
| `Something else answered for <domain> on port 443.` | A proxy, CDN, or other web server answered. Point the domain straight at this machine (for Cloudflare, make the record DNS-only). |
| `Let's Encrypt couldn't find <domain> in DNS.` | Create the A or AAAA record and wait a few minutes. |
| `A CAA record for <domain> doesn't allow Let's Encrypt.` | Add `0 issue "letsencrypt.org"` to the domain's CAA records, or remove them. |
| `Let's Encrypt is rate limiting certificates for <domain>.` | Wait as long as it says. The server keeps retrying on its own. |
| `This machine can't reach Let's Encrypt.` | The server needs outgoing HTTPS to `acme-v02.api.letsencrypt.org`. |
| `No certificate for <domain> after two and a half minutes.` | The server keeps trying in the background. Watch `sudo switchboard server logs --follow`; once it shows `certificate_obtained`, run `switchboard init` again for the invite. |
| `The server started but didn't answer within 15 seconds.` | See `sudo switchboard server logs`. Often another program already uses the port. |
| `/etc/switchboard is in the way.` (or another path) | That path wasn't written by Switchboard's installer, so it isn't replaced. Move it aside if it isn't needed and run init again. |
| `Installing the server needs root, and sudo isn't installed.` | Run `switchboard init` as root, or install `sudo`. |

## What doctor reports

| Check | Message | What to do |
| --- | --- | --- |
| program | `Switchboard ... is running, but its installed copy is missing or unsafe.` | `switchboard install` installs it again. |
| configuration | `This machine isn't set up yet.` | Run `switchboard init`. |
| configuration | `The configuration is invalid or unsafe.` | Check that you own `~/.config/switchboard/bridge.json`, that it is mode 0600, and that no directory above it is a symbolic link. |
| configuration | `This machine's key or certificate is unsafe or invalid, or the two don't match.` | Check that you own the files in `~/.config/switchboard/identity/` and that they are mode 0600. Keep them; a new key is a new machine to the server. |
| bridge | `Down. Run switchboard up to reconnect.` | Not a problem: the machine was taken down with `switchboard down`. `switchboard up` brings it back. |
| bridge | `The bridge isn't running.` | `switchboard up`. |
| bridge | `The bridge isn't running, and its background service isn't installed.` | `switchboard install --start`. |
| bridge | `The bridge is running, but not connected to the server.` | Check the network and that the server is up. |
| bridge | `The bridge is running, but the server doesn't accept this machine's key: ...` | The machine was revoked, or never added. Make a new invite on the server (`sudo switchboard server invite`), then run `switchboard init` here and choose to join again. |
| bridge | `The bridge's runtime file is unsafe or out of date.` | `switchboard down`, then `switchboard up`. |
| server | `The server doesn't accept this machine's key.` | The machine was revoked, or an earlier setup stopped before the server accepted it. Make a new invite on the server and run `switchboard init` here; choose to join again. |
| server | `Can't connect to the server, or its certificate isn't the one this machine trusts.` | Check the network and the server. If the server's certificate changed, join again with a new invite. |
| server | `The server didn't answer within 8 seconds.` | Check the network and the server. |
| discovery | `Chats are found by polling, so new chats can take a few seconds to appear.` | A warning only. The bridge couldn't use file and process notifications and falls back to scanning every five seconds. |
| chats | `Joined chats can't be checked safely.` | Check that you own Claude Code's session files in `~/.claude/sessions` and that they are private. |

## What server status reports

Run `sudo switchboard server status` on the server. It exits with status 1 when anything below
appears; a healthy server shows `✓ The server is healthy.` above its facts.

| Message | What to do |
| --- | --- |
| `The server's service isn't installed.` | Install it again with the command shown (`sudo switchboard server install --tls ...`, with your options). |
| `The server isn't running.` | `sudo systemctl start switchboard-server`. |
| `The server stopped after a failure.` | See why with `sudo switchboard server logs`, then start it. A `server_error` line names the cause, such as a port in use or an unsafe file. |
| `The server keeps failing and restarting.` | See why with `sudo switchboard server logs`. |
| `The server isn't running, and another program is listening on port <port>.` | Find it with `sudo ss -ltnp 'sport = :<port>'`, stop it, and restart the server. |
| `The server doesn't answer on port <port>.` | It runs but doesn't respond. See `sudo switchboard server logs`. |
| `The server won't start when this machine boots.` | `sudo systemctl enable switchboard-server`. |
| `The server has no certificate from Let's Encrypt yet, so machines can't connect.` | Look for `certificate_error` lines in `sudo switchboard server logs`; the [table above](#running-the-server-with-init) explains each Let's Encrypt refusal. |
| `The certificate expires in <n> days and should have been renewed by now.` | Renewal has been failing. Check the `certificate_error` lines the same way. |
| `The server presents a different certificate than <path>.` | `sudo systemctl restart switchboard-server`. |
| `The server's certificate is missing (<path>).` | Restore it from a backup. Otherwise a restart makes a new one, and every machine needs a new invite. |
| `The registry can't be read: ...` | Check that `/var/lib/switchboard/peers.json` is owned by `switchboard`, mode 0600, and valid JSON. |
| `The server has logged <n> errors since it started.` | The newest are listed under **Recent errors**; see them in context with `sudo switchboard server logs`. |
| `You can't read the server's configuration as this user.` | Run it with `sudo`. |

**`sudo: switchboard: command not found` on the server.** The server's command is
`/usr/local/bin/switchboard`, and some distributions' sudo replaces your `PATH` with a
`secure_path` that leaves out `/usr/local/bin` (`sudo sudo -V | grep 'override user'` shows it).
Run the commands by their full path, `sudo /usr/local/bin/switchboard server ...`. The server
installer checks this when it runs: where sudo doesn't find `switchboard`, every hint the server
prints names the full path. To make `sudo switchboard` work instead, add `/usr/local/bin` to
`secure_path` with `sudo visudo`. Hints printed on other machines (for example `init`'s "make an
invite on the server") can't know the server's sudo and say `sudo switchboard`.

## Certificate problems

**A private-mode server changed its certificate.** Rerunning the server installer with different
names makes a new certificate, and machines that joined before can no longer connect. `doctor`
reports `Can't connect to the server, or its certificate isn't the one this machine trusts.` Make
a new invite on the server for each machine; on each, run `switchboard init`, choose to join
again, and paste it. The machine keeps its name and key.

**A public server's certificate isn't trusted.** The server serves its Let's Encrypt certificate
only once it has one. Until then (or if a proxy sits in front), machines can't connect. Check
`sudo switchboard server status` on the server, and `sudo switchboard server logs` for
`certificate_error` lines.

Never turn certificate checking off; there is no option for it.

## A chat doesn't appear

- **On this machine.** `switchboard chats --local` lists the Claude Code chats open here. A chat
  appears while its Claude Code process is running. If `doctor` warns that chats are found by
  polling, give it a few seconds.
- **`switchboard join` from a terminal** answers
  `switchboard join connects the chat it runs in, and this is a terminal.` Run `/switchboard join`
  (with the plugin) or `!switchboard join` inside the chat (the message names the one that fits
  this machine), or pass the chat's ID:
  `switchboard join --chat ID`, with the ID from `switchboard chats`.
- **`Joined as <name>, but not live yet.`** The chat is joined, but the bridge hasn't published it
  yet, usually because the bridge isn't running or isn't connected. The line below says which.
  Run `switchboard up`, or `switchboard doctor`.
- **`Joined as <name>, but other machines still see it as <old name>.`** The chat was renamed, but
  the bridge still publishes its earlier name after ten seconds. Give it a moment; if it stays,
  run `switchboard doctor`.
- **On other machines.** `switchboard chats --remote` lists only joined chats on machines whose
  bridge is connected right now. If it says
  `Chats on other machines are unavailable: can't connect to the server, or its certificate isn't the one this machine trusts.`,
  this machine can't reach the server; see `doctor`.
- **A chat that was joined has disappeared.** A joined chat leaves on its own after 7 days with no
  activity (working, or sending or receiving a Switchboard message). `/clear`, branching, and
  `/resume` move to a different conversation, which isn't joined until you join it; the
  original stays joined and comes back when you return to it.
- **Join refuses a 33rd chat.** A machine can have at most 32 joined chats at once; leave another
  chat first.
- **A Linux machine's bridge stops when you log out.** The bridge is a systemd user service, and
  systemd stops a user's services at logout unless lingering is on for that user
  (`loginctl enable-linger $USER`).

## Messages don't arrive

**Check the path.** `switchboard ping <chat>` asks the other machine's bridge, through the server,
whether the chat is joined and running, without sending it anything:

```text
$ switchboard ping research
✓ workstation/research replied in 1 ms.
```

Failures say why, for example `No reply from zzz.` and `No chat on another machine matches
'zzz'.` Other reasons include
that the chat is offline or no longer joined, and that no reply came within 5 seconds.

**Read the note Switchboard sends back.** When a message can't be delivered, the sending chat gets
one message written by Switchboard on its own machine, not by the other chat. It starts
`Switchboard on this machine wrote this, not <chat>; don't reply to it.` and then says what
happened:

- `Your message to <chat> was not delivered: this chat hasn't joined Switchboard.` The sending
  chat isn't joined. Run `/switchboard join` (or, without the plugin, `!switchboard join`) in it and
  send again.
- `Your message to <chat> was not delivered (<reason>).` The server or the other machine refused
  it, for example because that chat left or its machine went offline.
- `Switchboard couldn't confirm that your message to <chat> arrived (<reason>); it may or may not
  have reached that chat.` The outcome is unknown, for example because the connection dropped
  after sending. Resend once if it matters.

**Handed off is not read.** A message that reached the other chat has only been handed to it. The
other Claude may be busy, may hold it for your approval where Claude Code does that, or may not
act on it.

**Look in the logs.** On the sending machine, `switchboard logs` shows each message's outcome as a
`delivery` line, and a send it refused as a `refused` line with the reason.

## Upgrades

| Message | Cause and fix |
| --- | --- |
| `0.1.1 is older than the installed 0.1.2.` | `switchboard upgrade VERSION` refuses to move to an older release unless you pass `--force`. |
| `Could not reach <host>: ...` | The download failed. Check the connection and run it again. |

`switchboard upgrade` restarts the bridge if it was running, and leaves a machine that is down
down. If `doctor` warns that the bridge's
service is still installed under an old name, `switchboard install` moves it.

The Claude Code plugin updates separately from the program. If `/switchboard` commands behave
like an older version, run `claude plugin marketplace update switchboard`, then
`/reload-plugins` in chats that were already open.

## Removing a machine

Leaving a server from `switchboard init` (on a machine that is set up) stops the bridge, removes
its background service, and moves the setup to `~/.config/switchboard.left-<time>`. A machine
that was down also has its service allowed to start at login again, so a later setup starts
normally.

`switchboard uninstall` removes the bridge service, the command, and the program, and keeps the
configuration, key, chat membership, and logs (`--purge` deletes those too). Neither removes the
machine from the server: it stays listed until you run `sudo switchboard server revoke NAME`
there. Uninstall's last lines say so:

```text
  This machine is still on the server as workstation. To remove it there, run
  this on the server:
    sudo switchboard server revoke workstation
```
