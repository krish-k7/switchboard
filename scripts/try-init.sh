#!/usr/bin/env bash
# Try `switchboard init` without touching your real setup.
#
#   scripts/try-init.sh                 the join path: a throwaway server and an
#                                       invite, then init in a sandboxed HOME
#   scripts/try-init.sh server          the server path, with the root step
#                                       simulated (a private test server runs as
#                                       you on 127.0.0.1 instead of the system
#                                       service)
#   scripts/try-init.sh server --acme-error 'TEXT'
#                                       the same, with Let's Encrypt failing with
#                                       TEXT (for example 'order Invalid: API
#                                       error: Timeout during connect (likely
#                                       firewall problem)
#                                       (urn:ietf:params:acme:error:connection)')
#
# Anything after the mode (and --acme-error) goes to init, for example
# `scripts/try-init.sh join --no-plugin`. Set SWITCHBOARD_BINARY to use a
# prebuilt binary; otherwise target/release/switchboard is built.
#
# Everything lives in one temporary directory: HOME, the server, and fake
# launchctl, systemctl, sudo, and claude commands first on PATH. The fake
# service managers really run the bridge (as you, from the sandboxed HOME), so
# init sees it connect; the fake claude only records the plugin install, and
# the fake sudo refuses. Nothing
# reaches the real launchd or systemd, your ~/.config or ~/.local/bin, your Claude
# Code settings, or any real server. Everything is stopped and removed on
# exit.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
mode=join
acme_error=
case ${1:-} in
    join | server) mode=$1; shift ;;
esac
if [[ ${1:-} == --acme-error ]]; then
    acme_error=${2:?--acme-error needs the error text}
    shift 2
fi

binary=${SWITCHBOARD_BINARY:-}
if [[ -z $binary ]]; then
    echo "Building switchboard (release)..." >&2
    (cd "$root" && cargo build --release --quiet)
    binary=$root/target/release/switchboard
fi
binary=$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")

# Not resolved: on macOS /tmp is a link, so HOME goes through one, as it can
# on real machines.
work=$(mktemp -d /tmp/sbtry-XXXXXX)
chmod 700 "$work"
cleanup() {
    for pidfile in "$work"/bridge.pid "$work"/server/server.pid; do
        if [[ -f $pidfile ]]; then
            kill "$(cat "$pidfile")" 2>/dev/null || true
        fi
    done
    sleep 0.2
    rm -rf "${work:?}"
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

home=$work/home
# ~/.claude stands for a machine where Claude Code has run (the bridge needs it).
mkdir -p "$home/.claude/sessions" "$work/bin" "$work/server"
chmod 700 "$home" "$home/.claude" "$home/.claude/sessions" "$work/server"

# A fake launchctl and systemctl that run the bridge for real, as you.
cat > "$work/bin/service-manager" <<EOF
#!/bin/sh
# Stands in for launchctl and systemctl inside the sandbox.
pidfile="$work/bridge.pid"
running() { [ -f "\$pidfile" ] && kill -0 "\$(cat "\$pidfile")" 2>/dev/null; }
start() {
    running && return 0
    umask 077
    mkdir -p "\$HOME/.local/share/switchboard"
    nohup "\$HOME/.local/bin/switchboard" bridge --config "\$HOME/.config/switchboard/bridge.json" \\
        >> "\$HOME/.local/share/switchboard/daemon.log" 2>> "\$HOME/.local/share/switchboard/daemon-error.log" &
    echo \$! > "\$pidfile"
}
stop() { running && kill "\$(cat "\$pidfile")" 2>/dev/null; rm -f "\$pidfile"; return 0; }
echo "\$(basename "\$0") \$*" >> "$work/service-calls"
case "\$*" in
    *com.vorelios.switchboard* | *" switchboard.service"*) exit 3 ;;
    *switchboard-server*) exit 3 ;;
esac
case "\$(basename "\$0") \$1 \$2" in
    "launchctl print "*) running && exit 0 || exit 113 ;;
    "launchctl bootstrap "* | "launchctl kickstart "*) start ;;
    "launchctl bootout "*) stop ;;
    "launchctl disable "* | "launchctl enable "*) exit 0 ;;
    "systemctl --user is-active") running && exit 0 || exit 3 ;;
    "systemctl --user start") start ;;
    "systemctl --user stop") stop ;;
    "systemctl --user "*) exit 0 ;;
    *) exit 3 ;;
esac
EOF
chmod 700 "$work/bin/service-manager"
ln -s service-manager "$work/bin/launchctl"
ln -s service-manager "$work/bin/systemctl"

# A fake claude: the plugin install is recorded, never done.
cat > "$work/bin/claude" <<EOF
#!/bin/sh
echo "claude \$*" >> "$work/claude-calls"
case "\$1 \$2" in
    "plugin list")
        if [ -f "$work/plugin-installed" ]; then
            echo '[{"id": "switchboard@switchboard", "version": "0.1.2", "scope": "user", "enabled": true}]'
        else
            echo '[]'
        fi ;;
    "plugin install") sleep 2; touch "$work/plugin-installed"; echo '{"ok": true}' ;;
    *) echo "fake claude: \$*" >&2; exit 1 ;;
esac
EOF
chmod 700 "$work/bin/claude"

# A fake sudo: nothing in the sandbox may become root.
cat > "$work/bin/sudo" <<EOF
#!/bin/sh
echo "sudo \$*" >> "$work/sudo-calls"
echo "sudo: not available in this sandbox" >&2
exit 1
EOF
chmod 700 "$work/bin/sudo"

sandbox_env=(env -i HOME="$home" PATH="$work/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    TERM="${TERM:-xterm-256color}" LANG="${LANG:-en_US.UTF-8}" USER="${USER:-you}" LOGNAME="${LOGNAME:-${USER:-you}}")
[[ -n ${NO_COLOR+set} ]] && sandbox_env+=(NO_COLOR="$NO_COLOR")
[[ -n ${COLUMNS:-} ]] && sandbox_env+=(COLUMNS="$COLUMNS")

say() { printf '%s\n' "$*" >&2; }

if [[ $mode == join ]]; then
    # A throwaway private-mode server on 127.0.0.1 and a free port.
    port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
    mkdir -p "$work/server/state" && chmod 700 "$work/server/state"
    (umask 077
     printf '{"tls": "private", "listen": "127.0.0.1", "port": %s, "names": ["localhost", "127.0.0.1"], "db": "state/messages.sqlite3", "peers_file": "peers.json"}\n' "$port" > "$work/server/server.json"
     printf '{}\n' > "$work/server/peers.json")
    "$binary" server --config "$work/server/server.json" 2>> "$work/server/server.log" &
    echo $! > "$work/server/server.pid"
    for _ in $(seq 100); do
        [[ -s $work/server/state/tls/private-cert.pem ]] && break
        sleep 0.1
    done
    sleep 0.3
    invite=$("$binary" server invite --config "$work/server/server.json" --expires 1h --json |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["invite"])')
    say ""
    say "Sandbox: $work"
    say "A throwaway server is running on 127.0.0.1:$port. Its invite (paste it when init asks):"
    say ""
    say "$invite"
    say ""
    if command -v pbcopy > /dev/null; then
        printf '%s' "$invite" | pbcopy && say "(It is on your clipboard too.)"
    fi
    say "Press Enter to start switchboard init."
    read -r _ < /dev/tty
    "${sandbox_env[@]}" "$binary" init "$@" || true
    say ""
    say "Run init again in the same sandbox, to see a set-up machine? [y/N] "
    read -r again < /dev/tty || again=
    if [[ $again == y || $again == Y ]]; then
        "${sandbox_env[@]}" "$binary" init "$@" || true
    fi
else
    sandbox_env+=(SWITCHBOARD_INIT_STUB_SERVER="$work/server")
    [[ -n $acme_error ]] && sandbox_env+=(SWITCHBOARD_INIT_STUB_ACME_ERROR="$acme_error")
    say ""
    say "Sandbox: $work"
    say "The server path, with the root step simulated: a private test server runs as you on 127.0.0.1."
    say "Press Enter to start switchboard init."
    read -r _ < /dev/tty
    "${sandbox_env[@]}" "$binary" init "$@" || true
fi
say ""
say "Cleaning up the sandbox."
