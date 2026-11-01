# Running a Switchboard server

The server is one Linux machine that every other machine connects to. It routes messages
between machines, keeps the list of machines allowed to connect (the registry), and makes the
invites new machines join with. It runs as a hardened systemd service under its own account,
`switchboard`, from the same `switchboard` program the clients use.

The server is small: idle, it uses a few megabytes of memory. A cheap virtual machine, a home
server, or one of your own Linux workstations is enough.

## Choosing a mode

| Mode | Use it when | Port | Certificate |
| --- | --- | --- | --- |
| Private network | Every machine reaches the server over Tailscale, a VPN, or a LAN | 8743 | Self-signed; each invite carries its fingerprint, and machines check it |
| Public domain | Machines connect from anywhere over the internet | 443 | From Let's Encrypt, renewed automatically |
| Behind Caddy | You already run Caddy on the server and want it to terminate TLS | Caddy's | Caddy's |

The private mode needs no DNS and no open internet port, so it is the default when nothing else
is chosen. The public mode needs a DNS name and port 443 free on the server and open to the
internet. Both are set up by the wizard. The Caddy mode is for people who already run Caddy; it
is set up with `switchboard server install` and a Caddyfile you add yourself.

In every mode, machines prove who they are with their own key on every request (see
[security.md](security.md)).

## Before you start

- Linux with systemd, and a user that can run `sudo`. The install is the only step that needs
  root.
- The port for your mode must be free. The wizard stops if something already listens on it.
- The install never changes firewalls. Open the port yourself if a firewall is active (for
  example `sudo ufw allow 8743/tcp`, or `443/tcp` for a public domain), and in your cloud
  provider's security group if there is one.

## Setting up with the wizard

On the server, run the install command:

```sh
curl -fsSL https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh | sh
```

It installs `switchboard` for your user and starts `switchboard init`. (If Switchboard is already
installed, run `switchboard init`.) Choose **Run the server**, or **Run the server and join it**
to also connect this machine's Claude Code chats. Then:

1. **Where will machines connect from?** A private network, or anywhere with a public domain.
2. **Private:** the addresses machines use to reach the server. The default list is this
   machine's Tailscale name and Tailscale IPv4 addresses (when Tailscale is running), its primary
   address, and its host name. Invites point at the first entry; the certificate covers them all.
   **Public:** the domain name. The wizard looks it up first. A name that doesn't resolve stops
   it; a name that resolves somewhere else only warns, since that is expected behind NAT or a
   cloud load balancer.
3. **Install the server now?** The install runs once through `sudo`, which may ask for your
   password. The wizard waits until the server answers. In public mode it also waits (up to two
   and a half minutes) for the first certificate and turns a failure from Let's Encrypt into a
   specific fix (see [troubleshooting.md](troubleshooting.md)).

It finishes with the first invite. Pressing Enter at every question gives a working private
server.

Every question has an option, so the wizard can run without a terminal:

```sh
switchboard init --server --tls private                              # names from this machine
switchboard init --server --tls private --server-name sb.tailnet.ts.net --server-name 100.64.1.2
switchboard init --server --tls acme --domain switchboard.example.com --email you@example.com
switchboard init --server --tls private --join                        # and join this machine
```

`--port` changes the port (default 443 for `acme`, 8743 for `private`). `--yes` accepts the
default answer to every question. Run as root (under `sudo`), init sets up only the server; run
it again afterwards without `sudo` to connect your own chats, so the bridge runs as you.

## Setting up by hand

Instead of the wizard, download the binary for the server's architecture
(`switchboard-linux-x86_64` or `switchboard-linux-arm64`, both statically linked) and the
release's checksums, and run the server installer as root:

```sh
curl -fsSLO https://github.com/krish-k7/switchboard/releases/latest/download/switchboard-linux-x86_64
curl -fsSLO https://github.com/krish-k7/switchboard/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS && chmod +x switchboard-linux-x86_64

sudo ./switchboard-linux-x86_64 server install --tls private --name sb.lan --name 10.0.0.5
sudo ./switchboard-linux-x86_64 server install --tls acme --domain switchboard.example.com
```

The installer creates or checks:

| Path | What it is |
| --- | --- |
| `switchboard` system account | Runs the server; home `/var/lib/switchboard` |
| `/var/lib/switchboard/` (mode 0700) | The server's state: registry, invites, messages, certificates |
| `/etc/switchboard/server.json` (0600) | The server's configuration (its mode, names or domain, port) |
| `/usr/local/lib/switchboard/switchboard` | The program, linked from `/usr/local/bin/switchboard` |
| `/etc/systemd/system/switchboard-server.service` | The service, enabled and started |

It then waits until the server answers and, while no machine has joined, prints an invite for
the first one (single use, valid for an hour). Each such run replaces the invite an earlier run
printed. `--no-invite` skips it, for scripts and agents that make their own invites with
`switchboard server invite`.

The command is linked from `/usr/local/bin/switchboard`. On distributions whose sudo replaces
`PATH` with a `secure_path` without `/usr/local/bin` (`sudo switchboard` then says `command not
found`), the installer notices, and the server's hints name the full path,
`sudo /usr/local/bin/switchboard ...`; use that form there.

Running the installer again is safe. It rewrites only what differs and restarts the service only
when the program, the configuration, the unit, or the private certificate changed. It refuses to
replace a unit, configuration directory, or program directory it did not write, and `--no-start`
writes everything without starting the service. `switchboard server install --help` lists the
options, and [REFERENCE.md](../REFERENCE.md) has the full details.

## Private network mode

The server makes a self-signed certificate once, in `/var/lib/switchboard/tls/private-cert.pem`,
and keeps it. Its SHA-256 fingerprint (the "pin") goes into every invite, and a joining machine
compares it with what the server presents before it makes any key. Machines then trust exactly
that certificate.

The certificate covers the names you gave (`--server-name` in the wizard, `--name` for the
installer). Without names, the installer keeps the names of the previous install, or uses the
host name and primary address on a first install. Invites point at the first name, on port 8743
unless you chose another.

Changing the names (rerunning with names the certificate doesn't cover) issues a new certificate
and prints its new fingerprint. Machines that joined before cannot connect to the new
certificate. Make a new invite for each of them; on each, run `switchboard init`, choose to join
again, and paste it. Rerunning without names keeps the certificate.

## Public domain mode

The server gets its certificate from Let's Encrypt on its own, using the TLS-ALPN-01 challenge on
port 443. Port 80 is not used. Before installing:

- Create an A record (and AAAA for IPv6) for the domain, pointing at the server's public
  address.
- Let inbound TCP on port 443 reach the server, in its firewall and in any cloud security group
  or router.
- If the domain is on Cloudflare, make the record DNS-only. A proxy in front of the server breaks
  both the certificate check and the machines' own keys.
- If the domain has CAA records, allow `letsencrypt.org`.

The server answers only for the configured domain. It keeps its Let's Encrypt account,
certificate, and key in `/var/lib/switchboard/tls/` and serves a stored certificate at once on
restart. It renews when a third of the certificate's lifetime remains and retries failures after
5 minutes, doubling up to every 6 hours. It logs each outcome as a `certificate_obtained` or
`certificate_error` line (see [Logs](#logs)).

## Behind Caddy

If Caddy already runs on the server, Switchboard can listen on a private Unix socket behind it:

```sh
sudo ./switchboard-linux-x86_64 server install --tls caddy --domain switchboard.example.com
```

This needs the `caddy` group (install Caddy first). The server listens on
`/run/switchboard/hub.sock`, readable only by the `caddy` group. Caddy must require a client
certificate without a trust pool and pass the client's key fingerprint to the server in a header;
[REFERENCE.md](../REFERENCE.md#behind-caddy) has the exact Caddyfile. Keep the `caddy` group to
Caddy alone: anything in it can claim to be any machine.

`--domain` is used only to print the first invite. Without it, make invites with
`sudo switchboard server invite --url https://YOUR-DOMAIN`, since the server's configuration
doesn't record the address machines use.

## Invites

A machine joins with an invite made on the server:

```sh
sudo switchboard server invite                               # one machine, within an hour
sudo switchboard server invite --name build-box              # one machine, as build-box
sudo switchboard server invite --uses 3 --expires 2h         # three machines, within two hours
```

The output is the invite and how to use it:

```text
  Invite
  Single use, expires in 1 hour, at 05:20 PDT. Copy all of it; line breaks are
  fine.
      sbi1.eyJ1cmwiOiJodHRwczovL2xvY2FsaG9zdDo1MTU3MyIsInBpbiI6IjlmZTNjMjk5ZTE
      ...
  Treat it like a password until it's used: anyone who has it can join one
  machine.

  Next
  On the machine you're adding, run this, and paste the invite when it asks:
      curl -fsSL \
      https://raw.githubusercontent.com/krish-k7/switchboard/main/install.sh \
      | sh
```

An invite carries the server's address, the certificate's fingerprint in private mode, a random
secret, and the fixed machine name when `--name` was given. The server keeps only a hash of the
secret. `--expires` takes `30m`, `2h`, `7d`, or seconds, from one minute to 30 days; `--uses`
takes 1 to 1000. An invite with `--name` has one use (only one machine can have that name), so
`--uses` above 1 with `--name` is refused. At most 64 invites can be open at once.

```text
$ sudo switchboard server invites
  Open invites  2

  4d202e95446c  any name   1 use left   expires in 57 minutes
  7005b0616e6e  build-box  3 uses left  expires in 2 hours

  To revoke one, run
      switchboard server invite revoke ID

$ sudo switchboard server invite revoke 4d202e95446c
✓ Revoked invite 4d202e95446c; it no longer works.
```

The server remembers used-up and expired invites for a week, so a machine that tries one is told
plainly that it was used or has expired.

## Machines

```text
$ sudo switchboard server machines
  Machines  2

  laptop       krish  seen just now
  workstation  dana   seen 8 min ago

$ sudo switchboard server revoke laptop
✓ Revoked laptop. Its key stops working on its next request.
```

"Seen" is the machine's last request, recorded at most once a minute. A revoked machine's name
can go to a new machine. To bring a revoked machine back, make a new invite and run
`switchboard init` on it. Uninstalling Switchboard from a machine does not remove it from the
server; revoke it here.

There are no administrator roles. Whoever can run these commands on the server (root, or the
`switchboard` account) decides who joins, and every joined machine has the same rights.

## Checking the server

```sh
sudo switchboard server status
```

`server status` answers one question: is the server healthy? It only reads. Anything that needs
attention comes first, each problem with its fix, then one line per fact:

```text
✓ The server is healthy.

  Service      running since Oct 29 09:14 PDT
  Version      0.3.0
  Mode         private network
  Address      https://sb.lan:8743
  Certificate  self-signed for sb.lan and 10.0.0.5, valid until Jan 1, 2125
  Pin          9fe3cade5e4a90c1549e93d87a8b8abcc16ac3a0ef553826c55614869b2533ce
  Listening    [::]:8743, IPv6 and IPv4
  Machines     2 machines, both seen in the last 10 minutes
  Invites      1 open invite
  Database     1.2 MB
```

It checks that the service is running and enabled, that the server answers on its port (or Caddy
mode's socket) with the certificate on disk, and how long that certificate has left. For a public
domain it warns when less than 7/30 of the certificate's lifetime is left (21 days of Let's
Encrypt's 90): the server renews at a third, so by then renewal has been failing for over a week.
Errors the server logged in the last day are listed under **Recent errors** with their times;
those since the server last started count as a problem. It shows counts only: `server machines`
and `server invites` list them.

It exits with status 1 when anything needs attention, so a monitor or cron job can run it, and
`--json` gives the same report for scripts. Run it as root or as the `switchboard` account; the
journal (for recent errors) needs root.

## Upgrading the server

The service runs its own copy of the program, `/usr/local/lib/switchboard/switchboard`. To
upgrade it, download the new release as in [Setting up by hand](#setting-up-by-hand) and run its
installer with the same options you installed with:

```sh
sudo ./switchboard-linux-x86_64 server install --tls private
sudo ./switchboard-linux-x86_64 server install --tls acme --domain switchboard.example.com
```

The installer copies the running program, so run the new binary, not the installed one. It
replaces the program and restarts the service; messages in flight at that moment are reported
to their senders as unconfirmed. Pass the same `--tls`, `--domain`, `--port`, and `--email` as
before, or the installer rewrites the configuration to match what you passed. Private mode keeps
its names when `--name` is left out.

`switchboard upgrade` upgrades only the client copy in your home directory. If the server
machine is also joined, run both.

## Backup and restore

Everything the server knows is in two directories:

| Path | Contents |
| --- | --- |
| `/etc/switchboard/` | `server.json`, the configuration |
| `/var/lib/switchboard/peers.json` | The registry: every machine's key fingerprint, name, and owner. The one file that matters most |
| `/var/lib/switchboard/invites.json` | Open invites (hashes only) |
| `/var/lib/switchboard/messages.sqlite3` | Messages from the last 7 days (at most 10,000), and when each machine was last seen |
| `/var/lib/switchboard/tls/` | Private mode: the certificate and key machines have pinned. Public mode: the Let's Encrypt account, certificate, and key |

Back up with the service stopped, so the database is consistent:

```sh
sudo systemctl stop switchboard-server
sudo tar -C / -czpf switchboard-server-backup.tgz etc/switchboard var/lib/switchboard
sudo systemctl start switchboard-server
```

The backup holds the server's private keys and the message text: store it as carefully as the
server itself.

To restore, on the same machine or a new one: install the server with the same options (this
creates the `switchboard` account and enables the service), stop it, unpack the backup over it,
and start it again:

```sh
sudo ./switchboard-linux-x86_64 server install --tls private
sudo systemctl stop switchboard-server
sudo tar -C / -xzpf switchboard-server-backup.tgz
sudo systemctl start switchboard-server
```

Machines keep working without rejoining, because the registry and (in private mode) the pinned
certificate come back unchanged. The new machine must be reachable at the same names or domain.
In private mode, if the addresses changed, rerun the installer with the new names; that issues a
new certificate, and each machine joins again with a new invite.

## Logs

```sh
sudo switchboard server logs                  # the last 50 events
sudo switchboard server logs --since 2h       # everything from the last two hours
sudo switchboard server logs --follow         # keep printing new events
```

Each line is one event from the system journal: the time, what happened, and the detail. Errors
are marked with a cross and warnings with `!`. The service's own starts, stops, exits, and
restarts appear beside the server's events:

```text
  Server events  4, 1 error, times in PDT

  Oct 31 09:14:02  started
  Oct 31 09:14:02  private_certificate  pin 9fe3cade5e4a…
  Oct 31 09:20:41  invite_redeemed      laptop joined with invite 09f019947dc9
✗ Oct 31 10:02:13  request_error        database is locked
```

`--lines N` changes how many (default 50), and `--since` takes `30m`, `2h`, `7d`, or a date and
time such as `"2026-10-31 09:00"`. `--json` prints one JSON object per event, with the line as the
server logged it, so `--follow --json` streams.

The server writes one JSON object per line, for example:

```text
{"event": "private_certificate", "sha256": "9fe3cade5e4a90c1549e93d87a8b8abcc16ac3a0ef553826c55614869b2533ce", "certificate": "/var/lib/switchboard/tls/private-cert.pem", "created": false}
{"event": "invite_redeemed", "invite": "09f019947dc9", "machine": "laptop"}
```

`private_certificate` appears at every start in private mode, with the fingerprint machines pin.
`certificate_obtained` and `certificate_error` report Let's Encrypt in public mode. The log never
includes message text. `journalctl -u switchboard-server` shows the same lines unchanged.

## Uninstalling the server

```sh
sudo switchboard server uninstall             # keep the registry, messages, and certificates
sudo switchboard server uninstall --purge     # delete everything
```

`server uninstall` stops and disables the service and removes the unit, the program, and the
`/usr/local/bin/switchboard` link. It keeps the configuration (`/etc/switchboard`), the state
(`/var/lib/switchboard`: the registry, invites, messages, and certificates), and the `switchboard`
account, and lists each of them with its path. Installing again with the same options picks them
up, so machines reconnect without joining again.

`--purge` also deletes the configuration, the state, and the account. Machines that were joined
stop connecting; on each, `switchboard uninstall` removes the client.

Like the installer, it refuses a unit or directory the installer didn't write, before changing
anything, and it removes only the files the installer and the server make: anything else in those
directories stays, with its directory, and is listed. Running it again is safe; it says what is
left. A `/usr/local/bin/switchboard` that isn't the installer's link is left alone.
