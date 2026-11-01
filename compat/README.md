# Switchboard compatibility suite

Black-box tests of the `switchboard` binary. They pin its observable behavior
from outside, so a change to the code cannot change what clients, bridges, and
files see without a test changing too. Each component is started from
the binary and observed only through its public interfaces: HTTP for the
server, line JSON on stdin/stdout for the native helper, native Claude registry
files and sockets, and process arguments, files, exit codes, and output for the
bridge and CLI. Nothing here imports the code under test. Standard library
only; Python 3.12 or newer, plus the `openssl` command (OpenSSL 3 or later; the
LibreSSL `/usr/bin/openssl` on macOS is not enough) for test certificates.

```sh
cargo build --release
python3 -B compat/run.py                       # tests target/release/switchboard
python3 -B compat/run.py --bin path/to/switchboard
python3 -B compat/run.py --quick               # skip the four tests that wait on 10-30 s timers
python3 -B compat/run.py --module test_helper -k receipt -v
```

A full run takes about a minute with the default four parallel modules (`-j`).
Everything a test creates lives under a fresh `/tmp/sbc-*` directory that is
removed when the test ends. The CLI runs with a fake `HOME` and fake
`launchctl` and `systemctl` first on `PATH`, so no test reads real Switchboard
state, contacts a real server, or queries launchd or systemd.

`test_bridge.TwoBridges` runs two bridges on two fake machines, each with its
own native Claude namespace, through one server, and checks messages,
receipts, pings, and departures in both directions.

## How components are started

Each component is `<binary> server`, `bridge`, `helper`, or `cli`. A single
component can be replaced with a command of its own (shell words), for
example to run an older release's server against the current bridge:

```sh
python3 -B compat/run.py --server-cmd "/path/to/older/switchboard server"
```

The suite appends these arguments, so each command must accept them:

| Component | Arguments appended | Contract the tests rely on |
| --- | --- | --- |
| server | `--config PATH` | Config is the JSON in PROTOCOL.md: Unix form `{unix_socket, db, peers_file}` or direct-TLS form `{listen, port, cert, key, client_ca, db, peers_file}`. SIGINT stops it cleanly (removing its Unix socket). Unsafe or conflicting configs exit nonzero without listening. `db` is an SQLite file in the current schema. |
| helper | `--config PATH --route M/UUID --name NAME --socket-dir DIR [--status S]` | The helper IPC and native protocol in PROTOCOL.md. Prints `{"event":"ready",...}` first; SIGTERM or end of stdin removes its files and exits 0. |
| bridge | `--config PATH` | Bridge config from PROTOCOL.md. One JSON log object per stdout line, `at` first. SIGTERM exits 0 after removing `runtime.json` and every proxy. The bridge starts its own helpers; the suite does not choose them. |
| cli | `[--config PATH] COMMAND ...` | The commands, `--json` shapes, and exit codes in REFERENCE.md; with `--json`, results and errors are JSON on stdout. Reads `HOME`, `PATH` (for `launchctl` and `systemctl`), and `CLAUDE_CODE_MESSAGING_SOCKET`. |

## What the tests pin, and how strictly

- **Exact**: status codes, error codes, JSON values and key sets, protocol
  strings written to native sessions (envelopes, receipt frames, Switchboard
  notices), registry/runtime/membership file contents, CLI `--json` output, exit
  codes, and the handful of human-readable lines quoted in the README.
- **Byte-for-byte** (`test_server_legacy.py`): what an old bridge (no
  capabilities, no acknowledgments) receives: key order, `", "`/`": "`
  separators, and ASCII escaping of non-ASCII text. Message events keep the
  sender's field order, then `type`, `sender_name`; the tests send fields in
  the order the bridge uses.
- **Computed, not copied**: the peers revision (SHA-256 of the sorted roster
  without `last_seen`) and an invite's ID (the first 12 hex digits of its
  secret's SHA-256) are recomputed from their documented definitions.
- **Validated, then normalized**: UUIDs, ack IDs, tokens, timestamps, `rtt_ms`,
  and process IDs are checked for format and plausibility, then replaced before
  comparison. Nothing else is masked.
- **Format only**: error details that come from the language runtime rather than
  from Switchboard (for example, the text of a malformed-UUID or missing-file
  error) are checked as non-empty strings, and their status and code exactly.
- **On-disk state** (`test_server_state.py`, `test_bridge.py`): the SQLite
  message schema, `delivered-ids.json`, and `activity.json` are read and seeded
  directly, because a replacement must take over the existing files on a live
  machine. Delivery states are observed in the message database (opened
  read-only), since the server has no message-history endpoint.

Real time: the server has no setting to shorten its 30-second delivery and
lease cutoffs or its 10-second long poll, so four tests (marked `@slow`) wait them
out once each; `--quick` skips them. Everything else uses events, not sleeps.

A behavior change is a deliberate decision: change the test's expectation in
the same commit as the code, never loosen an assertion to let both pass.

## Layout

- `run.py`: runner (parallel by module, one summary line per module).
- `sbcompat/`: harness. `server.py` starts the server and speaks HTTP to it (Unix
  ingress with the fingerprint header, or direct mTLS with CA-signed test
  certificates from `pki.py`); `claude.py` runs fake native Claude sessions
  (`fixtures/fake_claude_session.py`: real processes that publish a registry
  record, peer key, and socket, record every frame written to them, and send
  frames so the kernel identifies them as the sender); `helper.py`, `bridge.py`,
  and `cli.py` drive the other components.
- `tests/`: `test_server_*` (API, enrollment, HTTP framing, old clients, direct
  TLS, persistence, timers), `test_helper`, `test_cli`, `test_bridge`.

## Not covered, and why

- **Server**: the 128-handler global cap and the more-than-256-outstanding-pings
  limit (the handler cap makes the ping limit unreachable over HTTP); the
  10-second header and body read deadlines (timers with no setting; slow); retention's once-a-minute cadence (the
  7-day prune and 10,000 cap are checked at startup from a seeded database);
  `invalid_registry_change` (no API request can produce it); SQLite `unavailable` (need breaking the implementation's own files);
  registry owner/group preservation (needs root); the TLS minimum version.
- **Helper**: the 24-hour receipt-correlation expiry; capacity eviction at 256
  pending and 10,000 remembered IDs; partial or dripping native frames during
  reconfiguration and signal wakeup internals (covered by the unit tests).
- **Bridge**: discovery watcher internals (only the reported mode is checked);
  helper crash recovery; the 20-second fallback registration for servers without
  registration tokens (needs an old server); the 10-minute resend notice window;
  `delivered-ids.json` pruning; worker-lane and IPC-deadline limits.
- **CLI**: `setup` (it makes a self-signed key, which the direct-TLS test server
  cannot accept, and then installs the launchd service); `start`/`stop`
  (launchd); `install`, `upgrade`, and `uninstall` (covered by
  `switchboard/tests/lifecycle.rs` with fake service managers and a fake
  release server); terminal table layouts and colors. `setup` with invites is
  covered by the crate's tests (`switchboard/tests/cli.rs`) against a
  private-certificate server.
- The Claude Code plugin in `claude-plugin/`.
- Platforms: CI runs the suite on Linux x86_64 and macOS on Apple silicon.
