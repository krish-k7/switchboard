# Switchboard protocol

The contract between the parts of Switchboard: the server's HTTPS API, the registry and invite
formats, the bridge's configuration and runtime files, the bridge-to-helper messages, and how
Claude Code's native messages are carried. [ARCHITECTURE.md](ARCHITECTURE.md) explains the design;
this page is the detail a change must keep. The compatibility suite in [compat/](compat/README.md)
pins much of it. All configuration files are private JSON, and no chat is ever joined
automatically.

## Server listeners and identity

The server's configuration selects exactly one listener ([REFERENCE.md](REFERENCE.md#server-configuration)
lists the keys):

| Configuration | Listener | Where the client's identity comes from |
| --- | --- | --- |
| `{"tls": "acme", "hostname": ...}` | Built-in TLS with a Let's Encrypt certificate (TLS-ALPN-01 on the server's own port; port 80 unused) | The client certificate presented in the handshake |
| `{"tls": "private", "names": [...]}` | Built-in TLS with a self-signed certificate that clients pin | The client certificate presented in the handshake |
| `{"unix_socket": ...}` | A Unix socket behind Caddy | The `X-Switchboard-Client-Fingerprint` and `X-Switchboard-Client-Address` headers Caddy sets |
| `{"listen", "port", "cert", "key", "client_ca", ...}` | Direct mutual TLS against a client CA, used only by tests | The client certificate, which must chain to `client_ca` |

There is no plaintext TCP listener. The built-in modes and Caddy require a client certificate but
trust no issuer: any key that completes the handshake (proving possession) gets through, and the
registry is the only authorization. Certificate dates and issuers are not checked. The built-in
modes use TLS 1.2 or later and serve only the configured names (an ACME server refuses handshakes
for any other name, or none).

On the Unix socket, the server accepts exactly one fingerprint header (64 lowercase hex digits)
and at most one printable address header of at most 64 characters, and Caddy must overwrite both.
The socket's directory must be owned by the service account and its primary group with mode 0750,
and the socket is 0660; existing paths are never replaced. Membership of the `caddy` group is a
trusted boundary: anything in it can claim any identity. Over TCP, identity always comes from the
actual certificate, and caller-supplied headers are ignored.

## The registry

`peers_file` (on an installed server, `/var/lib/switchboard/peers.json`, owned by the service
account, mode 0600) maps the SHA-256 of each client certificate's DER encoding, in lowercase hex,
to an entry:

```json
{"<fingerprint>": {"machine": "laptop", "owner": "krish", "enabled": true, "invite": "09f019947dc9", "enrolled_at": 1793445620}}
```

Entries enrolled through an invite carry `invite` (the invite's ID) and `enrolled_at`. Revoked
entries have `enabled: false` with `revoked_by` (`<account> on the server`) and `revoked_at`.
Keys that earlier versions wrote (`admin`, `view`, `approved_by`, `approved_at`) are ignored.
Two enabled entries may not share a machine name.

The server checks the registry on every request. It caches the parsed file while the file's
metadata is unchanged; a changed file is read through `O_NOFOLLOW`, and its descriptor and path
must still match afterwards. A missing, unreadable, malformed, or changing file invalidates
authorization (fail closed). Long polls recheck revocation at most one second apart. The server
rewrites the registry atomically, keeping its owner and group, under `<peers_file>.lock` (0600);
the administration commands take the same lock.

## Invites

An invite is `sbi1.` followed by the unpadded base64url encoding of a JSON object:

| Field | Meaning |
| --- | --- |
| `url` | The address clients use: from the configuration's `hostname` and `port` (ACME), its first `names` entry and `port` (private), or `--url` (required behind Caddy) |
| `pin` | Private mode only: the server certificate's SHA-256, 64 lowercase hex digits |
| `secret` | 24 bytes from the kernel's random source, base64url (clients accept 16 to 64 bytes) |
| `machine` | Only when the invite fixes the machine's name |

Defaults are one use and one hour; expiry may be one minute to 30 days and uses 1 to 1000, with at
most 64 open invites. The server keeps them in `invites.json` beside its database, keyed by the
invite's ID (the first 12 hex digits of the secret's SHA-256), each with the full hash,
`created_at`, `expires_at`, `uses` left, the fixed `machine` or null, and the `source` (`install`,
`init`, or `command`). The secret itself is never stored. Used-up and expired invites are kept for
seven days (at most 128) so redeeming one gets a specific answer. The file has the registry's
owner and mode, is read only through `O_NOFOLLOW` as a singly linked regular file, is replaced
atomically, and changes only under the registry's lock, whether the server or a command changes
it. `switchboard server install` makes the first invite (one use, one hour) while no machine is
enabled, replacing the one an earlier run made.

## HTTPS API

Version 1. Every request must come from an enrolled key, except `POST /v1/enroll`. The machine's
identity is always the one the registry gives its key; nothing in a request can override it.

### General rules

- Request and response bodies are JSON objects; bodies are at most 64 KiB, and message text and
  hop chains at most 8 KiB each.
- Errors are `{"ok": false, "error": "<code>"}` with a 4xx or 5xx status. There are no redirects
  and no HTML.
- A method other than GET or POST is `405 method_not_allowed` with `Allow: GET, POST`; a path
  used with the other method is `405` with `Allow` naming the right one; an unknown path is
  `404 not_found`. A query string is part of the path.
- Response bodies use `", "` and `": "` as separators and escape every non-ASCII character as
  `\uXXXX`; message events keep the sender's field order. Older bridges may depend on these exact
  bytes, so they stay fixed (`test_server_legacy` pins them). A lone UTF-16 surrogate escape
  in a request parses; a field the server measures that holds one answers `400 invalid_unicode`.
- Message IDs and chat IDs are UUIDs. Remote routes are `machine/sessionUUID`.
- Chat entries are `{id, name, status, project?, repository?, branch?}`: name at most 64
  characters, status one of `idle`, `busy`, `waiting`, `shell`, `unknown`; project at most 64
  characters and repository and branch at most 256 (labels, not paths).

### Limits

| Limit | Value |
| --- | --- |
| Connection handlers | 128, of which long polls and pings together hold at most 96 (beyond that, `429 rate_limited`) |
| Per machine | 2 concurrent polls, 16 active requests, 120 requests a minute with bursts |
| Unknown keys (enroll only) | Per address: a burst of 20, then one every 3 seconds |
| Event queue per machine | 256 events; a send to a full queue is refused with `503 queue_full`, never dropped |
| Header and body reads | 10 seconds each |
| Outstanding pings | 256 server-wide |

### Endpoints

| Request | Purpose |
| --- | --- |
| `POST /v1/enroll` | Redeem an invite (the only request an unknown key may make) |
| `POST /v1/register` | Replace this machine's list of joined chats, and negotiate capabilities |
| `POST /v1/poll` | Long poll for events; renews the machine's lease |
| `GET /v1/peers` | Read the roster without polling or renewing presence |
| `POST /v1/send` | Send a message to a remote chat |
| `POST /v1/status` | Report a delivery outcome for a message this machine received |
| `POST /v1/ping` | Ask a remote machine's bridge whether a chat is reachable |
| `POST /v1/pong` | Answer a ping |
| `GET /health` | `{"ok": true, "machine": ..., "owner": ...}` |

#### `POST /v1/enroll`

Body `{"machine": NAME, "owner": LABEL, "invite": SECRET}`. Machine names are lowercase letters,
digits, and hyphens, start with a letter, and have at most 48 characters; owners are one line of
at most 64 characters. Under the registry's lock the server finds an open invite whose hash
matches, refuses a name other than one the invite fixes, takes one use (removing the invite at
zero), and adds `{machine, owner, enabled: true, invite, enrolled_at}`. If writing the registry
fails, the use is given back. Reply: `{"ok": true, "state": "approved", "machine": NAME}`.

| Error | Status | Meaning |
| --- | --- | --- |
| `invite_used` | 403 | The invite was used up (remembered for a week) |
| `invite_expired` | 403 | The invite expired (remembered for a week) |
| `invalid_invite` | 403 | Unknown secret, or a name other than the one the invite fixes |
| `invite_required` | 403 | No `invite` field |
| `machine_taken` | 409 | An enabled machine has that name; the invite is not used |
| `invalid_machine`, `invalid_label`, `invalid_enrollment` | 400 | A bad name, owner, or body |
| `rate_limited` | 429 | Too many requests from this address |
| `enrollment_unavailable` | 503 | The registry, its lock, or the invite store is unsafe or damaged |

A key that is already enrolled gets `approved` whatever the body. A revoked key counts as unknown
and needs a new invite. Every other request from an unknown key is `403 not_enrolled`.

#### `POST /v1/register`

Body `{"sessions": [chat, ...], "capabilities": ["ack", "ping"]}`, at most 32 chats and at most 16
capability strings (unknown ones are ignored). It replaces this machine's joined chats and renews
its presence. Reply:

```json
{"ok": true, "machine": "laptop", "registration": "<uuid>", "session_metadata_fields": ["project", "repository", "branch"], "capabilities": ["ack", "ping"]}
```

`registration` identifies the server's current in-memory registration for this machine. It stays
the same across roster changes and changes after a lease lapses or the server restarts; it is not
a credential. Only a real change to the roster wakes waiting polls. A bridge first registers the
basic fields and adds the metadata fields the reply lists. A machine that doesn't register `ack`
has its events consumed when polled; one that doesn't register `ping` never receives pings.
Re-registering without `ack` hands back any unacknowledged events the old way.

#### `POST /v1/poll`

Body `{"registration": UUID, "peers_revision": REVISION or null, "acks": [ACK_ID, ...]}` (at
most 512 acknowledgment IDs of 32 hex digits). Waits up to 10 seconds for events or a roster
change. Reply:

```json
{"ok": true, "events": [...], "peers": [...], "machine": "laptop", "registration": "<uuid>", "peers_revision": "<sha256>"}
```

- A poll with the current `registration` renews the machine's 30-second lease.
- `peers` is the full live roster (`{machine, owner, id, name, status, project?, repository?,
  branch?, last_seen}`, including the caller's own chats, which the bridge filters out). It is
  omitted when the supplied revision matches. The revision is the SHA-256 of the sorted roster
  without `last_seen`, so heartbeats don't change it.
- A missing or stale `registration` returns at once without handing out events; the bridge
  withdraws its proxies and registers again. Polling alone never creates a roster.
- For a machine registered with `ack`, every event carries an `ack_id` and stays in flight until
  acknowledged on a later poll. Each poll drops the acknowledged events (marking acknowledged
  messages `received`) and queues every other in-flight event again.

Events:

```text
{"type": "message", "id", "from", "to", "text", "mode", "hop_chain"?, "sender_name", "ack_id"?}
{"type": "status", "id", "state", "detail"?, "to", "ack_id"?}
{"type": "ping", "id", "to", "ack_id"}
```

Redelivered messages are skipped once they are no longer `routed`. Redelivered status events
older than 30 seconds are dropped.

#### `GET /v1/peers`

Reply `{"ok": true, "machine": ..., "peers": [...], "peers_revision": ...}`. Reads presence only:
it never creates or renews a lease, consumes events, registers chats, or returns messages.
`switchboard chats` uses it, with an 8-second deadline.

#### `POST /v1/send`

Body `{"id": UUID, "from": "machine/uuid", "to": "machine/uuid", "text": TEXT, "mode":
"prompting" | "bypass", "hop_chain"?: TEXT}`. Reply `{"ok": true, "id": ..., "state": "routed"}`.
`from` must be one of the caller's currently registered chats (`403 source_forbidden`) and `to` a
live chat (`404 target_offline`). The ID is bound to its sender and the whole body: an identical
resend is the same message and returns its recorded state; a different body with the same ID is
`409 id_conflict`. The sender's display name comes from the server's registration.

#### `POST /v1/status`

Body `{"id": UUID, "state": "handed_to_claude" | "held" | "refused" | "failed" | "unknown",
"detail"?: TEXT}`. Reply `{"ok": true}`. Only the machine the message was sent to may report
(`403 status_forbidden`; an unknown ID is `404 message_not_found`). A final `refused` or `failed`
is never overwritten by a weaker state. The server queues `{"type": "status", "id", "state",
"detail"?, "to": <original sender route>}` for the sender's machine, or stores it (below). A
status is an observation by the receiving machine, not a claim that the message was read or acted
on.

#### `POST /v1/ping` and `POST /v1/pong`

`/v1/ping` body `{"to": "machine/uuid"}`. The server queues a ping event for the target machine
and waits up to 5 seconds for its pong. Reply `{"ok": true, "rtt_ms": INT, "live": BOOL}`, where
`rtt_ms` is the server's time from queuing to the pong. Pings are never stored as messages and
never reach a chat.

| Error | Status |
| --- | --- |
| `target_offline` (machine or chat not registered) | 404 |
| `ping_unsupported` (the target bridge didn't register `ping`) | 409 |
| `ping_timeout` | 504 |
| `queue_full` | 503 |
| `rate_limited` (more than 256 pings outstanding) | 429 |
| `invalid_route`, `invalid_ping` | 400 |

`/v1/pong` body `{"id": PING_ID, "live": BOOL}`, reply `{"ok": true}`. Only the pinged machine
may answer (`403 pong_forbidden`); late answers are ignored. `live` says whether the chat is
currently joined and running on that machine; the bridge answers without involving any helper or
chat.

### Delivery states and timers

| State | Meaning |
| --- | --- |
| `routed` | Accepted by the server and queued for the target machine |
| `received` | Acknowledged by the target machine's bridge |
| `handed_to_claude` | Written to the target chat's socket (or released by Claude Code after approval) |
| `held` | Claude Code is holding it for approval |
| `refused`, `failed` | Not delivered |
| `unknown` | The outcome can't be confirmed |

A message still `routed` 30 seconds after sending becomes `unknown` with the detail
`delivery not confirmed within 30 seconds`, as does a routed message whose recipient chat leaves
(`recipient session left before confirming delivery`). A `received` message with no further
status within 30 seconds of its acknowledgment also becomes `unknown`. Held messages don't time
out. Each of these sends the sender's machine a status event.

In-flight state lives in memory. A server restart marks `routed`, `held`, and `received` messages
`unknown` without replaying them, and stores for each a status event with the detail
`the server restarted before delivery was confirmed`.

**Stored status events.** When the sender's machine can't take a status event (offline, its queue
full, its lease lapsed with events unacknowledged, or in flight at a restart), the server stores it
in the `notices` table, at most one per message (a newer one replaces one not yet queued). Stored
events are queued when that machine registers, and on its polls while its queue has room, only
while the message's recorded state still matches. An acknowledgment deletes one; a machine without
`ack` consumes it on poll. They live no longer than their message and are exempt from the
30-second redelivery cutoff.

Messages are kept seven days, and at most 10,000; expiry checks run on a one-second deadline and
pruning at most once a minute. There is no endpoint that returns message history.

## Bridge configuration and files

`bridge.json`:

```json
{"enabled": true, "server_url": "https://host", "ca": null, "cert": "...", "key": "...", "config_dir": "...", "state_dir": "...", "sessions": ["<uuid>", ...]}
```

plus optional `names` and `joined_at`, keyed by chat ID. `hub_url` is read when `server_url` is
absent; `viewer_port` is ignored. A null `ca` verifies the server against the system's trusted
authorities, including its host name; a path pins that certificate. The machine's name is the one
the server returns at registration, never the configuration's. `state_dir` is private (0700). The
bridge discovers only joined chats that are live.

`runtime.json` in `state_dir` (private, replaced atomically, deleted on a clean exit):
`{pid, procStart, platform, machine, connected, discovery_mode, discovery_detail, published,
remote, updated_at}`. `discovery_mode` is `events`, `polling`, or null before the first scan;
`published` lists the chats the server accepted with the names sent; `remote` counts visible
remote `sessions` and `machines`. A connection failure clears `connected`. While the server
refuses this machine itself (HTTP 401 or 403 with `not_enrolled` or `invalid_proxy_identity`), an
extra `server_refused` field holds that code; `doctor` reads it.

`delivered-ids.json` in `state_dir` (0600) holds the IDs of messages already handed to a chat, for
one hour and at most 2,048, written before each handoff.

## Native proxies

Each helper publishes a record in Claude Code's session registry (`config_dir/sessions/`) and owns
a socket in the same directory as the joined local chats' sockets, with its own real process ID,
start time, session ID, and token. Its record carries `peerBridgeProxy: true` and
`peerFeatures: []` (so Claude Code doesn't subscribe it to features it doesn't implement),
`kind: "interactive"`, the helper's own start time, and the remote chat's activity; an unknown
activity is omitted rather than shown as idle. Version metadata comes from a real joined local
chat; Claude Code release numbers are never compared or required. Helper records are never
re-exported as real chats, and existing files are never overwritten.

Aliases are `switchboard/<machine>/<name>-<first 8 of the chat ID>`: the machine part at most 16
characters and the name at most 26, both limited to ASCII letters, digits, `_`, and `-`. The whole
alias is at most 64 characters, including any suffix that tells apart colliding names (which
shortens only the name). The alias is for display; routing uses `machine/uuid`.

The helper fails closed on an unsupported `peerProtocol`, a malformed frame, the wrong target, or
a sender that isn't a joined chat. It accepts only the token it published, from a socket
directory owned by the user, and never logs private keys. Identity of a peer never authorizes
commands or settings changes. An empty connection is a probe.

## Bridge and helper messages

One JSON object per line on the helper's standard input and output; there is no network listener.

From the bridge:

| Command | Meaning |
| --- | --- |
| `{"op": "configure", "revision": N, "name": ..., "sessions": [UUID, ...], "status"?: ...}` | Update the name, status, and the joined chats (at most 32, in the current socket namespace) allowed to use this helper. `revision` must increase. An omitted `status` keeps the current one; `unknown` removes it |
| `{"op": "deliver", "id", "target_session", "text", "mode", "hop_chain"?, "sender_name"}` | Deliver a message to a joined local chat |
| `{"op": "track", "id", "source_session", "text", "mode"}` | Adopt an outbound message from a replaced helper, so its outcome reaches the sender |

From the helper:

| Event | Meaning |
| --- | --- |
| `{"event": "ready", ...}` | First line, once its record and socket are published |
| `{"event": "configured", "revision"}` / `{"event": "configure_rejected", "revision", "detail"}` | The outcome of a `configure` |
| `{"event": "outbound", "native_id", "source_session", "text", "mode", "hop_chain"?, "recipient"}` | A joined chat sent a message to this proxy; carries the configuration revision used to authorize it |
| `{"event": "status", "id", "state", "detail"?}` | The outcome of a delivery |

Every event is bound to the route, a random helper instance ID, and the bridge's generation.
Outbound frames are checked against the current selection again after a complete frame is read.
SIGTERM, SIGINT, or end of input makes the helper remove its files and exit 0; its signal handler
only records the signal and wakes the blocked waits.

A configure keeps the helper's process ID, start time, socket, session ID, token, and pending
receipts for chats that stay joined (or rejoin); a removed chat can't send while removed. The
registry record is replaced atomically.

## Native receipts

A native `SendMessage` succeeding means only that the local proxy took the message. Claude Code's
receipts from the receiving chat are carried in the status `detail` as canonical markers, at most
96 characters, each allowed only with its state:

| Marker | State |
| --- | --- |
| `native receipt: held` | `held` |
| `native receipt: denied`, `native receipt: refused` | `refused` |
| `native receipt: expired`, `native receipt: dropped` | `failed` |
| `delivered` | `handed_to_claude` (released after approval) |

A dropped marker may add `; drop_reason=` with `rate-limited`, `duplicate`, `hop-loop`,
`hop-runaway`, or `queue-full`; unknown reasons are omitted without discarding the receipt. A
recognized marker with the wrong state is rejected; a malformed one produces no receipt. The
sender's helper turns markers back into native receipts for the original sender process. Native
`refused` is relayed as `expired` with `status_detail: refused`; `denied` and `expired` keep their
own statuses.

Native drop batches (`dropped_msg_ids`) of 0 to 256 IDs, including the single-drop form with an
empty list and `orig_msg_id`, are validated as a whole and deduplicated. Only entries belonging to
the authenticated original process and chat are processed, and each produces its own status event
and native receipt. Receipt correlation is bounded (256 pending, 10,000 remembered IDs, 24 hours),
so late receipts can be lost after eviction or a restart.

Generic failures, uncertain outcomes, and plain handoff have no native receipt and are never
relabeled as one. For an unmarked `failed`, `refused`, or `unknown`, the sender instead gets the
Switchboard-written message described in [ARCHITECTURE.md](ARCHITECTURE.md#outcomes-and-notices),
at most once per message. An `unknown` after a confirmed handoff sends none, and an `unknown`
keeps its correlation so a later native receipt can still be relayed.

## Bridge internals that the contract depends on

- **Request workers.** Registration, send, status, and pong requests run on three fixed workers:
  one reserved for registration (one queued slot) and two for messages (32 queued slots each),
  chosen by message or ping ID. Request bodies are copied at submission. Leaving a chat or
  withdrawing cancels its queued requests under the same lock used to start them; started
  requests can't be recalled.
- **Uncertain sends.** A send whose outcome is uncertain (transport failure, a new connection
  generation, or cancellation by withdrawal) is kept for up to 25 seconds and resent with the
  identical body once the bridge is online and the route's helper exists. If that window passes,
  the sender is told delivery couldn't be confirmed; if the helper never returns within ten more
  minutes, the uncertainty is only logged.
- **Acknowledgments.** With a server that supports `ack`, the bridge acknowledges every processed
  event, including ones it rejected. Events discarded because of a stale generation or a lost
  registration are not acknowledged, so they are redelivered.
- **Reconnecting.** After a lost connection or a failed registration the bridge registers again
  after 1 to 2 seconds, doubling the wait after each failure in a row up to 30 to 60 seconds (each
  wait is scaled by a random factor between one half and one). An answered poll resets it; a
  network configuration change retries within a second. A poll the server rejects without ending
  the connection is retried on the same schedule.
- **Connections.** At most eight cached or active HTTPS connections, idle expiry after 30 seconds,
  replacement after 60 seconds of age, a 15-second timeout per socket operation, and no automatic
  retry of a failed request.
- **Helper writes.** One writer thread per helper, each command within three seconds, at most 64
  commands (256 KiB) queued. When a write fails, the helper gets SIGTERM; a `deliver` being
  written becomes `unknown` (`native helper IPC outcome uncertain; no replay`) and queued ones
  `failed` (`native helper IPC failed before write`).
- **Outbound memory.** For each message a local chat sent (at most 10,000), the bridge remembers
  its route, helper, source chat, first 240 characters, and mode, in memory only.
