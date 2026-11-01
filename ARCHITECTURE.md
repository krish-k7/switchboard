# How Switchboard works

This is for contributors. It describes what the code does today; [PROTOCOL.md](PROTOCOL.md) is the
exact contract between the parts, and [REFERENCE.md](REFERENCE.md) lists every command and file.

## The idea

Claude Code already lets chats on one machine message each other. Each running chat publishes a
small record in `~/.claude/sessions/` (its process ID, session ID, name, status, and the path of
a Unix socket it listens on) and a key file with a token. Claude's `ListAgents` tool reads those
records, and `SendMessage` connects to the target's socket, authenticates with the target's
token, and writes a message frame.

Switchboard adds chats from other machines to that same local registry. For each joined chat on
another machine, the bridge starts a **helper**: a real local process that publishes a record and
a socket exactly as a Claude Code chat does, marked as a Switchboard proxy. To Claude, a remote
chat looks like one more local chat. When a local chat writes to a helper's socket, the helper
hands the message to the bridge, the bridge sends it to the server, the server queues it for the
target machine, and that machine's bridge has its helper deliver it to the real chat's socket.
Claude Code itself is not modified, and the model gets no new tools.

```text
chat A ──native socket──▶ helper for B ──IPC──▶ bridge ──HTTPS──▶ server
                                                                    │
chat B ◀──native socket── helper for A ◀──IPC── bridge ◀──HTTPS─────┘
```

A chat takes part only if it is joined. Unjoined chats on the same machine share the registry, so
they can see the proxies, but a helper refuses their messages and the bridge doesn't advertise
them.

## One binary, several roles

Everything is the crate in `switchboard/`, built as one binary with a subcommand per role:

| Role | Entry | Source |
| --- | --- | --- |
| User commands (`init`, `join`, `chats`, ...) | `switchboard COMMAND` | `src/cli/` (`init/` is the setup wizard, `ui/` the terminal layer, `view.rs` what each command shows, shared with the plugin) |
| Bridge | `switchboard bridge --config FILE` | `src/bridge/`, `src/discovery.rs`, `src/hub_transport.rs`, `src/membership.rs` |
| Helper | `switchboard helper ...` | `src/helper.rs`, `src/helper_main.rs` |
| Server | `switchboard server --config FILE` | `src/server/` (`hub.rs` is the state and routing, `http.rs` the HTTP layer, `tls.rs` and `acme.rs` the built-in TLS) |
| Server administration | `switchboard server install`, `status`, `logs`, `uninstall`, `invite`, ... | `src/server/install.rs`, `status.rs` (with `system.rs` for systemd, the journal, and sockets, and `certinfo.rs`), `logs.rs`, `uninstall.rs`, `invites.rs`, `enroll_local.rs` |
| User install, upgrade, uninstall, up and down | `switchboard install`, `upgrade`, `uninstall`, `up`, `down` | `src/cli/install.rs`, `upgrade.rs`, `release.rs`, `service.rs` |

Shared pieces: `protocol.rs` (message and notice formats), `identity.rs` (process identity),
`invite.rs` (the invite token), `config.rs`, and `fsutil.rs` (careful file handling: no symbolic
links followed where it matters, private modes, atomic replacement). The modules `wirejson`,
`fmtutil`, `fileops`, `urlparse`, and `text` fix the exact bytes the parts write and the rules
they check text by: JSON with `", "` and `": "` separators and non-ASCII escaped as `\uXXXX`;
number and timestamp formats; temporary-file names and modes; address splitting; and the
character classes used to clean names and notices. The server holds request and response JSON
in `server/jsonval.rs`, whose strings can carry the lone UTF-16 surrogates a `\ud800` escape
produces. Older bridges compare some of these bytes exactly, so responses stay byte-identical
for them.

Everything uses threads, not an async runtime; the one exception is the ACME client in public
mode, which runs on a small single-threaded Tokio runtime of its own. TLS is rustls (with `ring`); HTTP/1.1 is written
by hand over it. The server stores messages in SQLite (bundled). The Linux release binaries are
static musl builds with no system dependencies.

## The bridge

One bridge runs per machine, as a launchd agent or systemd user service. `switchboard down`
disables that service (so neither a login nor a restart starts it), stops it, and writes a
`down` file beside the configuration; installs, upgrades, and `init` read that file and leave
the machine down until `switchboard up` removes it and enables the service again. A single-instance lock
in its state directory prevents two bridges from running. One main loop owns all state; other
threads only queue events to it: the long poll, three request workers, the connection reaper, the
discovery watcher, the network-change watcher, and a reader and a writer thread per helper. The main loop sleeps until there
is work or a real deadline; there is no periodic tick.

### Discovering chats

The bridge watches Claude Code's session records, its own configuration, and the processes of
joined chats: kqueue on macOS, inotify and process descriptors (pidfd) on Linux. Notifications are
only hints. Bursts are coalesced for about 100 ms, then the bridge checks the current records:
the process ID and start time from the kernel, the socket's owner and type, and that the chat is
joined. A full check every 30 seconds recovers anything a notification missed. If the watchers are
unavailable or overflow (more than 256 tracked records, or 32 watched processes), the bridge falls
back to scanning every 5 seconds and keeps trying to restore the watchers; it never silently drops
chats. `doctor` reports the fallback.

### Staying idle

The bridge wakes for events (a poll reply, a file or process change, a helper's message, a
signal), not on a clock. While connected, its only timers are the 30-second check above and the
server's long poll, which answers within 10 seconds. Helpers keep no timer except the single
deadline of their oldest unanswered receipt (24 hours). The services keep the default priority:
macOS's background process type stretches the 150 ms pause after each delivery to a chat to
about 250 ms, and on Linux `Nice=` or the idle scheduling and I/O classes could delay messages on
a busy machine, while there are no frequent timers left for either to save.

Offline, the poll thread sleeps until the bridge registers again, and the bridge retries the server
with exponential backoff: the first retry after 1 to 2 seconds, then double the wait each time, at
most 30 to 60 seconds (a random factor spreads machines that lost the server together). An answered
poll resets the backoff. A change in the machine's network configuration (the system
configuration's `network_change` notification on macOS, netlink link and address changes on Linux)
retries within a second, so a returning network is noticed at once; when only the server comes
back, the bridge notices within 60 seconds. Local discovery keeps running meanwhile. `runtime.json`
is rewritten only when what it says changes.

Lookups of a single chat (for each message's sender and target) fully check only the records that
claim that chat. Which records claim which chat is remembered while a record file's device, inode,
size, times, and mode are unchanged.

### Joining and membership

`bridge.json` holds the selection: `sessions` (joined chat IDs), and per chat `names` (the name
other machines see) and `joined_at`. `join` and `leave` edit it under its lock. Names are
normalized to letters, digits, `_`, and `-`, at most 26 characters.

The bridge records activity in its private `activity.json`: a joined chat whose status is busy or
waiting, or that sends or receives a Switchboard message, is active (written at most every five
minutes). A chat expires when the later of its join and its last activity is more than seven days
old; the bridge then removes it from the configuration under the lock and logs `session_expired`.
An idle open chat, closing a chat, and resuming it are not activity.

`membership.json`, beside the configuration, lists the joined chats and their names. Every change
(join, leave, expiry, or a joined chat's own name changing) rewrites it under the configuration's
lock. The Claude Code plugin watches it through Claude Code's file watching to show the footer
label, so no chat runs a process of its own.

The bridge's `runtime.json` (private, replaced atomically) records its process identity, whether
it is connected, how it discovers chats, the chats the server has accepted (`published`, with the
names sent), and how many remote chats and machines are visible. `status` checks the process
identity before trusting it, and `join` and `leave` wait on `published` (up to ten seconds, and
for a join, under the chat's current name) to report whether the change is live. While the server
refuses this machine's key, `server_refused` names the refusal, so `doctor` can say so.

Membership belongs to a chat's session ID. Claude Code's `/clear`, branching, and `/resume` move to
a different session ID, which is not joined until joined.

### Presence

The bridge registers its joined chats with the server (`POST /v1/register`) when it connects and
whenever the advertised list changes (a chat joins or leaves, or a name or status changes).
Otherwise it holds a long poll (`POST /v1/poll`), which waits up to 10 seconds and doubles as the
heartbeat: each poll renews the machine's 30-second lease. The server sends the roster of remote
chats only when its revision changes. A changed roster makes the bridge start, rename, or stop
helpers.

When the connection fails, or the server stops accepting the machine's key, the bridge withdraws
every proxy and registers again once it can. A rejected individual request (a full queue, a
recipient that went offline) affects only that message.

### Helpers

A helper is `switchboard helper`, started by the bridge from its own binary, one per visible
remote chat (at most 128). It owns everything Claude Code expects of a peer: a real process ID and
start time, a socket in the same directory as the local chats' sockets, a session ID, a registry
record, and a key file with its token. Its record mirrors the remote chat's activity (idle, busy,
waiting, or shell), omits the status when unknown, and copies version metadata from a real local
chat.

The bridge talks to each helper over its standard input and output, one JSON object per line.
`configure` commands carry a strictly increasing revision and update the helper's name, status,
and the joined chats allowed to use it, in place: renaming a remote chat or changing local joins
keeps the helper's identity and its pending receipts. A helper is replaced only when its route
leaves, the socket namespace changes, no joined local chat remains, it fails, or the connection is
lost. Every helper event carries the route, a random helper instance ID, and the bridge's
generation, so a late event from a replaced helper can't act as the new one.

Helper commands are written by one writer thread per helper, each within three seconds, with at
most 64 commands (256 KiB) waiting. A helper that stops reading is terminated and replaced; the
commands it never took are failed. Closing a helper waits at most 0.2 seconds on the main loop;
the rest of its shutdown (SIGTERM, then SIGKILL after 8 seconds) happens in the background, and
bridge shutdown gives all helpers one shared deadline.

## Sending a message

1. A joined local chat writes a native message frame to a helper's socket.
2. The helper identifies the sending process through the kernel, checks that it is the live
   process of a joined chat, rechecks the selection after reading the complete frame, and emits an
   `outbound` event to the bridge.
3. The bridge checks the sender against its current selection and the helper's acknowledged
   configuration, remembers which chat sent which message, and submits `POST /v1/send` with a
   message ID (a UUID) of its own.
4. The server checks that the sender is one of the calling machine's registered chats and that
   the target is live, stores the message, and queues it for the target machine.
5. The target machine's bridge receives it on its poll, records the message ID in
   `delivered-ids.json` (so a redelivery or restart never hands it to a chat twice), and tells the
   helper for the sending chat to deliver it to the target chat's socket.
6. The helper writes the native frame. The target bridge reports the outcome with
   `POST /v1/status`; the server queues it for the sender's machine, and that bridge passes it to
   the sending chat's helper.

The sender's name the target sees is built from the server's records (machine and registered
chat name), never from the message.

A local chat that isn't joined and writes to a proxy gets a single reply from that proxy, at most
once a minute, saying the message was not delivered and how to join.

### Outcomes and notices

The receiving side reports one of `handed_to_claude`, `held` (Claude Code is holding it for
approval), `refused`, `failed`, or `unknown`. Claude Code's own receipts (held, denied, expired,
refused, dropped at the inbox with a reason, and released after approval) are carried back as
canonical markers in the status detail and turned into the same native receipts for the sender,
so Claude reports them as it would for a local chat. Ordinary handoff produces no extra positive
receipt.

Transport problems have no native equivalent, and Switchboard never disguises them as one. For a
failed, refused, or unknown outcome without a native marker, the sender's helper sends one
ordinary message to the sending chat, from the remote chat's proxy and in the sender's own
permission mode so it isn't held for approval. It says Switchboard wrote it, states the outcome
and reason, quotes the first 120 characters of the message, and asks the agent not to reply (the
exact text is in `src/protocol.rs`). At most one notice is sent per message.

An outcome for a message whose helper was replaced still reaches the sender: the replacement first
receives a `track` command for the message. Attribution follows the joined chat's session ID, not
the exact process, so a chat resumed under a new process still gets the outcome of what it sent.
Native receipt controls stay with the exact process they belong to.

### Acknowledgments, resends, and timers

- **Acknowledged polls.** Each event a poll hands out stays in flight on the server until the
  bridge lists its acknowledgment ID on its next poll. Anything not acknowledged is queued again,
  because the reply that carried it was lost. The receiver drops message IDs it already handed to
  a chat.
- **Uncertain sends.** If a send's outcome is uncertain (the connection failed mid-request), the
  bridge resends the identical request for up to 25 seconds once it is back online; the server
  treats an identical resend as the same message. If the outcome is still uncertain, the sender is
  told delivery couldn't be confirmed.
- **Unconfirmed delivery.** A message not confirmed within 30 seconds becomes `unknown`, and its
  sender is told. A message whose recipient chat leaves first is reported the same way. Held
  messages don't time out this way.
- **Server restarts.** In-flight state is in memory. A restart marks in-flight messages `unknown`
  and stores a status event for each sender's machine, delivered when it next connects.
- **Stored status events.** When the sender's machine can't take a status event (offline, queue
  full, or its lease lapsed), the server stores it, one per message, until it can.

No other request is retried, and nothing is replayed after a restart.

### Request workers and connections

Registration, sends, and status reports go to three fixed worker threads: one reserved for
registration and two for messages, chosen by message ID so each message's requests stay in order.
Each lane queues at most 32 requests (registration, one). Leaving a chat or losing the connection
cancels its queued requests; requests already started may have reached the server.

HTTPS connections are reused within each thread, at most eight at once. Idle connections close
after 30 seconds, and a connection is replaced after 60 seconds of age once its current request
finishes. Each socket operation times out after 15 seconds. Every request is authenticated by the
server again, so reusing a connection never skips a revocation check.

### Chat metadata

For each joined chat the bridge publishes optional labels: the project (the name of the chat's
working directory), the repository, and the branch, read from Git's files in that directory or
above it. It never runs Git, reads Git configuration or remotes, or publishes absolute paths.
Changes are picked up by the 30-second check. Claude's `ListAgents` doesn't show these labels;
`switchboard chats` does.

## The server

The server is a thread per connection, at most 128, of which long polls and pings may hold at most
96 so short requests always find a thread. It answers every request with HTTP/1.0 and closes the
connection. All state is behind one lock: an in-memory view of machines, leases, and queues, and
an SQLite database of messages, stored status events, and last-seen times. Messages are kept 7
days and at most 10,000; pruning runs at most once a minute. Each machine's event queue holds 256
events, and a send to a full queue is refused rather than dropped.

The registry (`peers.json`) maps each machine's certificate fingerprint to its name, owner, and
whether it is enabled. The server checks it on every request, caching the parsed file while its
metadata is unchanged and failing closed if it is missing, unreadable, or changing. Registry and
invite changes, by the server or by the local administration commands, take the same lock file.

A panic in one request handler ends only that handler: the release build unwinds, the thread
closes its connection and releases its slot and counters, and the next request rolls back any
database transaction the handler left open. A panic on any other thread, in the server or in any
other role, aborts the process (`src/panics.rs`), so a half-working bridge never keeps running.

### Identity and joining

Each machine makes its own P-256 key and a self-signed certificate when it joins. The server's TLS
listener (built in, or Caddy in front) requires a client certificate but trusts no issuer: the
handshake proves the client holds the key, and the key's fingerprint is looked up in the registry.
That lookup is the only authorization, so certificate dates and issuers don't matter, and adding
or revoking a machine is a registry change with no restart. An unknown key may only redeem an
invite (`POST /v1/enroll`). [docs/security.md](docs/security.md) describes the trust model, and
PROTOCOL.md the invite format.

## Known limitations

| Area | Today | Possible next step |
| --- | --- | --- |
| Bridge restart | Which chat sent which message, and the resend queue, are in memory. Outcomes for messages sent before a bridge restart are only logged, and a send whose outcome was uncertain at shutdown is neither resent nor reported. | Persist a small, short-lived record of outbound messages beside `delivered-ids.json` and resend or report on startup. |
| Server restart | Status events already queued for settled messages (handed off, refused, failed) live only in memory and are lost at a restart. | Store every status event until acknowledged, at one database write per outcome. |
| Clients without acknowledgments | A bridge that doesn't register the `ack` capability has poll events consumed before the server knows the reply arrived. | None while such bridges are supported. |
| Server connections | The built-in TLS modes make a new TLS connection per request (HTTP/1.0, no keep-alive). | Open. |
| Roles | Every enrolled machine has the same rights; there are no administrators or per-machine permissions. | Open. |

## Tests

`cargo test` runs the unit tests and the integration tests in `switchboard/tests/`. The
compatibility suite in `compat/` drives the release binary from outside, with fake Claude Code
chats (real processes that publish records and sockets). Neither runs Claude Code itself.
[CONTRIBUTING.md](CONTRIBUTING.md) has the commands.
