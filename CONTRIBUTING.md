# Contributing

Switchboard is a Rust workspace with one crate (`switchboard/`), a black-box test suite in
Python (`compat/`), a Claude Code plugin (`claude-plugin/`), and
the installer script (`install.sh`). [ARCHITECTURE.md](ARCHITECTURE.md) explains the design and
[PROTOCOL.md](PROTOCOL.md) the contract between the parts.

## Build

```sh
cargo build --release            # target/release/switchboard
```

A local build reports the crate's version (`switchboard --version`); release builds take the
version from the tag.

## Test

What CI runs, from the repository root:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features fault-injection -- -D warnings
cargo test
cargo build --release --locked
python3 -B compat/run.py --bin target/release/switchboard
cargo test --release --locked --features fault-injection --test server_panic
.github/scripts/install-sh-test.sh target/release/switchboard
```

- `cargo test` runs the unit tests and the integration tests in `switchboard/tests/`: the
  command line with a private `HOME` and fake `launchctl` and `systemctl` first on `PATH`
  (`cli.rs`, `init.rs`), installing, upgrading against a fake release server, and uninstalling
  (`lifecycle.rs`), the helper against fake chats (`helper.rs`), the HTTPS client (`transport.rs`),
  and the server's built-in TLS modes against the real binary, including ACME against a mock ACME
  server (`server_tls.rs`). Some tests make certificates with the `openssl` command.
- The compatibility suite (`compat/`) starts the release binary's roles and observes them only
  through their public interfaces: HTTP, helper messages, Claude Code's session files and
  sockets, and command output. It needs Python 3.12 or newer and OpenSSL 3 (the LibreSSL
  `openssl` on macOS is not enough). `--quick` skips the four tests that wait out real 10 to
  30 second timers, and `--module test_helper -k receipt -v` runs a subset.
  [compat/README.md](compat/README.md) describes what it pins and how strictly.
- `server_panic` checks that a panic in one request handler ends only that request; the
  `fault-injection` feature compiles in the request paths that panic.
- `install-sh-test.sh` runs `install.sh` against a fake release server with a private `HOME`.

None of these touch your real Switchboard installation, service managers, or Claude Code
settings, and none run Claude Code itself. CI runs them on Linux x86_64 for every push, and on
macOS (Apple silicon) for `main` and pull requests. A second Linux job installs the server for
real on the disposable runner (`.github/scripts/server-install-test.sh`); never run that script
on a machine you care about. The bridge's Linux user unit is tested only against a fake
`systemctl`, not under a real systemd user manager.

What `status`, `chats`, `join`, `leave`, `ping`, `doctor`, `up`, `down`, and the help show is
defined once, in `switchboard/src/cli/view.rs`, for the terminal and the plugin alike. Its tests
write every display they check to `claude-plugin/tests/display-fixture.js` and fail when that
file is out of date; after a deliberate change, rewrite it with
`SWITCHBOARD_BLESS=1 cargo test the_plugin_fixture_matches`, then run the plugin's tests, which
check that the plugin draws every word of it.

The plugin has its own checks, run from `claude-plugin/`:

```sh
claude plugin validate --strict .
claude plugin test
```

## Trying the setup wizard

```sh
bash scripts/try-init.sh                   # join a throwaway local server
bash scripts/try-init.sh server            # the server path, with the root step simulated
bash scripts/try-init.sh server --acme-error 'order Invalid: ...'
```

Each run uses a temporary directory for `HOME`, a throwaway server on 127.0.0.1, and fake
`launchctl`, `systemctl`, `sudo`, and `claude` commands (the fake service managers really run
the bridge, as you, and the fake sudo refuses). Nothing reaches your real setup, and everything is
removed on exit. Set
`SWITCHBOARD_BINARY` to use a prebuilt binary. Anything after the mode goes to `init`.

`python3 -I scripts/capture-init.py` drives every wizard screen and error on a pseudo-terminal at
60, 80, and 120 columns, with and without color, and writes them to
`/tmp/switchboard-init-review` for review.

## Changing behavior

The compatibility suite and the integration tests pin behavior exactly: JSON shapes, error codes,
exit statuses, files, and the human lines users see. A behavior change is a deliberate decision:
change the test's expectation in the same commit as the code, and never loosen an assertion so
that both old and new behavior pass. Update the docs in the same change, too: [README.md](README.md)
and [docs/](docs/) for users, [REFERENCE.md](REFERENCE.md) for commands and files, and
PROTOCOL.md for anything on the wire or on disk.

The server's responses keep the byte-for-byte format older bridges may depend on (separators,
ASCII escaping, and field order; see the general rules in PROTOCOL.md), so changes there need
the `test_server_legacy` tests to stay green.

## Releases

1. Set the version everywhere it is written:

   ```sh
   .github/scripts/set-version.sh 1.2.3
   ```

   This updates `switchboard/Cargo.toml`, `Cargo.lock`, and the plugin's
   `claude-plugin/.claude-plugin/plugin.json`. The plugin's version must match the program's,
   because Claude Code refreshes its cached copy of the plugin only when that version changes; CI
   refuses a mismatch (`.github/scripts/check-versions.sh`).
2. Commit, merge to `main`, and push a tag on that commit: `v1.2.3` (a tag such as
   `v1.2.3-rc.1` is published as a prerelease). Tag a commit that main's CI ran on: the tip of a
   push to `main` (CI on `main` runs every platform).
3. The release workflow (`.github/workflows/release.yml`) checks that the tag names that version
   and that its commit is on `main`, and requires a successful CI run on `main` for exactly that
   commit (it reuses that run rather than testing again, waiting up to 30 minutes for one still
   running, and fails with the run's link when there is none or it didn't pass). It then builds
   four binaries, checks that each reports the tag's version, and publishes them with
   `SHA256SUMS` as a GitHub release. Run by hand, the workflow runs the tests itself and builds
   the binaries as artifacts without publishing.

| Asset | Target | Runs on |
| --- | --- | --- |
| `switchboard-darwin-arm64` | `aarch64-apple-darwin` | macOS 11 or later |
| `switchboard-darwin-x86_64` | `x86_64-apple-darwin` | macOS 10.13 or later |
| `switchboard-linux-x86_64` | `x86_64-unknown-linux-musl` | Any Linux; static |
| `switchboard-linux-arm64` | `aarch64-unknown-linux-musl` | Any Linux; static |

The Linux binaries are built with `cargo zigbuild` and need no system libraries (TLS is rustls
and SQLite is bundled). Running the release workflow by hand runs the tests and builds the
binaries as artifacts without publishing. Workflows keep no credentials in their checkouts, and
third-party actions are pinned by commit.
