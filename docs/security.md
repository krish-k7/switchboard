# Security

Switchboard is built for a small group of machines that trust each other: your own computers, or
a small team's, connected through a server you run. This page describes what it protects, what it
deliberately does not, and how to report a problem.

## The pieces and who trusts whom

- **The server** routes messages and decides which machines may connect. Whoever has root on
  the server (or can act as its `switchboard` account) controls it completely.
- **Each machine** has its own private key and runs a bridge as your user. The machine trusts
  the server it joined, and nothing else on the network.
- **Each chat** is a Claude Code session. Only chats you join take part, and a chat can only send
  through Switchboard from the machine it runs on.

## What Switchboard protects

**The connection to the server.** Every request goes over TLS 1.2 or later. A machine checks the
server's certificate in one of two ways. With a public domain, it uses the system's trusted
certificate authorities and checks the name, like a browser. On a private network, it trusts
exactly one certificate, whose SHA-256 fingerprint came inside the invite. Init checks that
fingerprint before it makes any key, and stops if the server presents a different one.

**Machine identity.** Each machine makes its own P-256 key when it joins. The private key never
leaves the machine; it lives in `~/.config/switchboard/identity/` with mode 0600, in a directory
only you can read. The server stores the SHA-256 fingerprint of each machine's certificate in its
registry, with the machine's name and owner. The TLS handshake proves the machine holds the key,
and the server looks the fingerprint up on every request. That lookup is the only authorization:
certificate dates and issuers are ignored, so nothing expires and nothing needs renewing.

**Joining.** A key the server doesn't know can make exactly one kind of request: redeeming an
invite. Those requests are rate limited per address (a burst of 20, then one every 3 seconds).
An invite's secret is 24 random bytes, and the server stores only its SHA-256 hash. By default
an invite works once and expires after an hour (at most 30 days and 1,000 uses; at most 64 open
at once). Uses are taken under a lock, so two machines racing for a single-use invite cannot both
join. An invite made with `--name` can only be used under that name, and only once. Init and
setup take the invite on the command line (`--invite VALUE`), from standard input,
`SWITCHBOARD_INVITE`, or a prompt without echo; one given on the command line is visible to
other users of that machine (in `ps`) while the command runs, and stays in the shell's history.

**Revocation.** `sudo switchboard server revoke NAME` disables a machine's key. The server
rereads the registry on every request, so the key stops working on its next request; a waiting
poll notices within a second. `sudo switchboard server invite revoke ID` cancels an unused invite.

**Who can send.** Only joined chats can send or receive across machines, and the server only
lists joined chats. On the sending machine, each remote chat is represented by a local helper
process that speaks Claude Code's own messaging protocol, including its token check. Before
passing a message on, the helper asks the kernel which process is on the other end of the
connection and accepts the message only from the live Claude Code process of a joined chat
(checked by process ID, process start time, and the chat's published record and socket). A chat
that isn't joined gets a note saying the message was not delivered. The server then checks that the message's sender
belongs to the machine that sent it, so one machine cannot speak for another machine's chats. The
sender's name the recipient sees comes from the server's records, not from the message.

**Files and the service.** The client keeps its configuration in `~/.config/switchboard` (mode
0700, files 0600), and the bridge runs with umask 077. The server's state directory is mode 0700
and owned by the `switchboard` account. Its systemd unit runs with `NoNewPrivileges`,
`ProtectSystem=strict`, `ProtectHome`, a private `/tmp`, a 256 MB memory limit, and no Linux
capabilities except `CAP_NET_BIND_SERVICE` when it listens on a port below 1024.

**Downloads.** `install.sh` and `switchboard upgrade` check each download against the
`SHA256SUMS` file of the same release, and `upgrade` downloads only over HTTPS. A GitHub token,
when one is set, is sent only to GitHub, never to a host a redirect leads to. The checksum file
comes from the same release as the binary, so it catches a damaged download, not a release that
was tampered with at the source.

## What Switchboard does not protect

**The server operator can read every message.** Messages are not end-to-end encrypted. They are
encrypted on the way to and from the server, and the server stores their text in its database
(`/var/lib/switchboard/messages.sqlite3`) for up to 7 days, at most 10,000 messages. Anyone with
root on the server can read them, and can also add machines, by making invites or by editing the
registry. Run the server on a machine you control, and treat its backups the same way.

**Every joined machine is equal.** There are no administrator roles and no per-machine
permissions yet. Any joined machine can see every joined chat (its name, activity, and the
project, repository, and branch it reports) and can message any of them.

**Everything running as your user is trusted.** The bridge, the helpers, and Claude Code all run
as your operating system user. Any program running as you can read your machine's key, join or
leave chats, read Claude Code's own session records, and talk to your chats. Switchboard does not
defend against other software on your account. Chats that aren't joined can still see the remote
chats in `ListAgents`; they just can't message them.

**A peer message is not a message from you.** Claude Code presents a Switchboard message as one
from another chat, and its own rules for messages between chats apply. Switchboard does not add
approvals, and it does not judge what a message asks for. The receiving agent may act on a
message within whatever permissions that chat already has. Give agents a rule for weighing peer
messages, such as [CLIENT_INSTRUCTIONS.md](../CLIENT_INSTRUCTIONS.md), and join only the chats
that should take part.

**Delivery is not reading.** A message reported as handed off reached the other chat. That does
not mean the other Claude read it or acted on it.

**A stolen key works until you revoke it.** Keys don't expire. If a machine is lost, revoke it on
the server right away.

**What an invite grants.** An invite lets one machine join until it is used or expires (one hour
by default). In private mode it also carries the fingerprint that tells the new machine which
server to trust. An unused invite can be cancelled on the server (`sudo switchboard server
invite revoke ID`).

**What leaves your machine.** For each joined chat, the server and every joined machine see its
name, its activity (idle, busy, waiting, or shell), and labels for its project (the working
directory's name), repository, and branch, read from local Git files. Absolute paths, Git
remotes, and transcripts are never sent. Message text goes to the server and to the recipient.

## How joining works

1. On the server, `switchboard server invite` makes a random secret, stores its hash, and prints
   an invite: the server's address, the certificate's fingerprint in private mode, and the
   secret.
2. On the new machine, `switchboard init` reads the invite and connects to the server. It checks
   the certificate against the fingerprint (private) or the system's trusted authorities
   (public), and stops before making anything if the check fails.
3. It makes the machine's key and a self-signed certificate, valid for ten years (the dates are
   never checked), and writes the bridge's configuration.
4. It sends one enrollment request with the invite's secret. The server checks the hash and
   takes a use under the registry lock, then adds the certificate's fingerprint, the machine's
   name, and its owner to the registry.
5. From then on, the machine authenticates every request with its key.

If the server refuses the invite (wrong, used up, expired, or the name is taken), init removes the
key and configuration it just made. If the server doesn't answer, the key stays, and the next run
reuses it.

## Revoking and rotating

- **A machine:** `sudo switchboard server revoke NAME`. To let it back in, make a new invite and
  run `switchboard init` on it.
- **An invite:** `sudo switchboard server invites` lists open invites by ID;
  `sudo switchboard server invite revoke ID` cancels one.
- **A private-mode certificate:** stop the server, delete
  `/var/lib/switchboard/tls/private-cert.pem` and `private-key.pem`, and rerun
  `sudo switchboard server install --tls private` (with the new binary, if upgrading). It makes a
  new certificate and prints its fingerprint. Every machine then joins again with a new invite:
  run `switchboard init`, choose to join again, and paste it.
- **A machine's own key** (for example, if it may have been copied): revoke the machine on the
  server, then on the machine run `switchboard uninstall --purge`, which deletes its key and
  configuration, and set it up again with a new invite. That makes a new key. Rejoining without
  `--purge` keeps the old key.
