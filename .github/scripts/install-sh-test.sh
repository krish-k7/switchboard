#!/usr/bin/env bash
# Test install.sh against a fake GitHub Releases server on 127.0.0.1, with a
# private HOME and fake launchctl and systemctl first on PATH, so nothing
# touches the real installation or service managers.
#
#   .github/scripts/install-sh-test.sh target/release/switchboard
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
binary=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
# Resolved: on macOS /tmp is a link, and the CLI refuses config paths through links.
work=$(cd "$(mktemp -d /tmp/sbinstall-XXXXXX)" && pwd -P)
server=
old_server=
cleanup() {
    for pid in $server $old_server; do kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; done
    if [[ -s $work/bridge-pid ]]; then kill "$(cat "$work/bridge-pid")" 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "FAILED: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

case $(uname -s) in Darwin) os=darwin ;; *) os=linux ;; esac
case $(uname -m) in x86_64 | amd64) arch=x86_64 ;; *) arch=arm64 ;; esac
asset=switchboard-$os-$arch
sha() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -c1-64; }

mkdir -p "$work/release" "$work/home" "$work/bin"
cp "$binary" "$work/release/$asset"
{
    printf '%s  %s\n' "$(sha "$work/release/$asset")" "$asset"
    printf '%s  %s\n' "$(printf other | sha /dev/stdin)" switchboard-other-arch
} > "$work/release/SHA256SUMS"
# Fake service managers. Without a mode file nothing is loaded and every call
# fails; in "working" mode the bridge's job loads and unloads (stopping the
# fake bridge); in "stuck" mode it is loaded and will not stop. Jobs under
# other names (the old service name) are never loaded.
for tool in launchctl systemctl; do
    cat > "$work/bin/$tool" <<EOF
#!/bin/sh
echo "$tool \$*" >> "$work/service-calls"
mode=\$(cat "$work/service-mode" 2>/dev/null || echo none)
[ "\$1" != --user ] || shift
case "\$*" in
    daemon-reload*) ;;
    *dev.switchboard.bridge* | *switchboard-bridge.service*) ;;
    *) exit 3 ;;
esac
case \$mode-\$1 in
    working-print | working-is-active) [ -e "$work/loaded" ] ;;
    working-bootout | working-stop) rm -f "$work/loaded"; kill "\$(cat "$work/bridge-pid")" 2>/dev/null; exit 0 ;;
    working-bootstrap | working-kickstart | working-start) touch "$work/loaded" ;;
    working-daemon-reload | working-enable) exit 0 ;;
    working-disable)
        [ "\$2" != --now ] || { rm -f "$work/loaded"; kill "\$(cat "$work/bridge-pid")" 2>/dev/null; }
        exit 0 ;;
    stuck-print | stuck-is-active | stuck-enable) exit 0 ;;
    stuck-disable) [ "\$2" != --now ] || { echo "Boot-out failed: 5: Input/output error" >&2; exit 5; } ;;
    stuck-bootout | stuck-stop) echo "Boot-out failed: 5: Input/output error" >&2; exit 5 ;;
    *) exit 3 ;;
esac
EOF
    chmod 700 "$work/bin/$tool"
done
# Nothing here may reach the real Claude Code, sudo, or clipboard. The fake
# claude lists the plugins in $work/claude-plugins (none without the file).
for tool in sudo pbcopy; do
    printf '#!/bin/sh\necho "%s is faked in this test" >&2\nexit 1\n' "$tool" > "$work/bin/$tool"
    chmod 700 "$work/bin/$tool"
done
cat > "$work/bin/claude" <<EOF
#!/bin/sh
[ "\$*" = "plugin list --json" ] || { echo "claude is faked in this test" >&2; exit 1; }
cat "$work/claude-plugins" 2>/dev/null || echo '[]'
EOF
chmod 700 "$work/bin/claude"
# curl, wrapped to record its arguments (where other users could see a token
# in ps) and the mode of any config file it is given.
real_curl=$(PATH=/usr/bin:/bin command -v curl)
cat > "$work/bin/curl" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$work/curl-argv"
previous=
for argument; do
    [ "\$previous" != --config ] || ls -ln "\$argument" | cut -c1-10 >> "$work/curl-config-modes"
    previous=\$argument
done
exec "$real_curl" "\$@"
EOF
chmod 700 "$work/bin/curl"

python3 -I "$root/.github/scripts/fake_release_server.py" "$work/release" owner/repo 1.2.3 test-token "$work/port" &
server=$!
for _ in $(seq 300); do
    [[ -s $work/port ]] && break
    kill -0 "$server" 2>/dev/null || fail "the fake release server exited"
    sleep 0.1
done
[[ -s $work/port ]] || fail "the fake release server did not start within 30 seconds"
port=$(cat "$work/port")

run() {  # run SHELL [VAR=VALUE...] [-- INIT-OPTIONS]: install.sh as `curl | sh` does, in a clean environment
    local shell=$1
    shift
    local variables=()
    while [[ $# -gt 0 && $1 != -- ]]; do variables+=("$1"); shift; done
    [[ ${1:-} != -- ]] || shift
    env -i HOME="$work/home" PATH="$work/bin:/usr/bin:/bin" SWITCHBOARD_REPO=owner/repo \
        SWITCHBOARD_GITHUB="http://127.0.0.1:$port" SWITCHBOARD_GITHUB_API="http://127.0.0.1:$port/api" \
        ${variables[@]+"${variables[@]}"} "$shell" -s -- "$@" < "$root/install.sh"
}
installed=$work/home/.local/bin/switchboard
if [[ $os == darwin ]]; then service=$work/home/Library/LaunchAgents/dev.switchboard.bridge.plist
else service=$work/home/.config/systemd/user/switchboard-bridge.service; fi

shells=(sh)
command -v dash >/dev/null && shells+=(dash)
for shell in "${shells[@]}"; do
    step "$shell: public download of the latest release"
    rm -rf "$work/home" && mkdir "$work/home"
    run "$shell" | tee "$work/out"
    cmp "$installed" "$binary" || fail "installed binary differs"
    [[ -x $installed && ! -L $installed && -f $service ]] || fail "command or service missing"
    [[ ! -e $work/home/bin && ! -e $work/home/.local/share/switchboard/switchboard ]] || fail "a wrapper or second copy was installed"
    grep -q "$installed" "$service" || fail "the service doesn't run the command"
    grep -q '^✓ Installed the switchboard command in ~/.local/bin$' "$work/out" || { cat "$work/out"; fail "no install line"; }
    grep -q 'switchboard init' "$work/out" || { cat "$work/out"; fail "no next steps"; }
    # Not a terminal: no title, no spinner, no color or cursor codes.
    if grep -q "$(printf '[\033\r]')" "$work/out"; then cat -v "$work/out"; fail "control codes without a terminal"; fi
    if grep -q 'Connects Claude Code' "$work/out"; then fail "a title without a terminal"; fi
    grep -q "^✓ Downloaded Switchboard [0-9]" "$work/out" || { cat "$work/out"; fail "no download line"; }

    step "$shell: a second run upgrades in place"
    run "$shell" > "$work/out"
    cmp "$installed" "$binary" || fail "installed binary differs after rerun"
done

for shell in "${shells[@]}"; do
    step "$shell: a pinned version from a private repository, through the API with a token"
    rm -rf "$work/home" "$work/curl-argv" "$work/curl-config-modes" && mkdir "$work/home"
    run "$shell" SWITCHBOARD_VERSION=1.2.3 GITHUB_TOKEN=test-token > "$work/out"
    cmp "$installed" "$binary" || fail "installed binary differs (token path)"
    run "$shell" SWITCHBOARD_VERSION=v1.2.3 GITHUB_TOKEN=test-token > "$work/out"
    # The token reaches curl only through a private config file, never its arguments.
    grep -q -- '--config' "$work/curl-argv" || { cat "$work/curl-argv"; fail "curl got no config file"; }
    if grep -q test-token "$work/curl-argv"; then cat "$work/curl-argv"; fail "the token was on curl's command line"; fi
    [[ $(sort -u "$work/curl-config-modes") == -rw------- ]] || { cat "$work/curl-config-modes"; fail "curl's config file is not private"; }
done

step "a token that cannot read the repository is refused, without printing it"
if run sh GITHUB_TOKEN=wrong_token_7f3a 2>"$work/err"; then fail "installed with a wrong token"; fi
grep -q 'GITHUB_TOKEN cannot read it' "$work/err" || { cat "$work/err"; fail "wrong message"; }
if grep -q wrong_token_7f3a "$work/err" "$work/curl-argv"; then fail "the token was printed or on curl's command line"; fi

step "a token that could break out of curl's config is refused, without printing it"
if run sh 'GITHUB_TOKEN=bad"token_7f3a' 2>"$work/err"; then fail "installed with a malformed token"; fi
grep -q 'GITHUB_TOKEN has characters' "$work/err" || { cat "$work/err"; fail "wrong message"; }
if grep -q token_7f3a "$work/err"; then fail "the malformed token was printed"; fi

step "a missing release is refused"
if run sh SWITCHBOARD_VERSION=9.9.9 2>"$work/err"; then fail "installed a missing release"; fi
grep -q 'Could not download' "$work/err" || { cat "$work/err"; fail "wrong message"; }

step "a checksum mismatch installs nothing"
before=$(sha "$installed")
cp "$work/release/SHA256SUMS" "$work/sums"
printf '%s  %s\n' "$(printf tampered | sha /dev/stdin)" "$asset" > "$work/release/SHA256SUMS"
rm -f "$service"
if run sh 2>"$work/err"; then fail "installed despite a checksum mismatch"; fi
grep -q "does not match the release's checksum" "$work/err" || { cat "$work/err"; fail "wrong message"; }
[[ $(sha "$installed") == "$before" && ! -e $service ]] || fail "changed files despite the mismatch"
cp "$work/sums" "$work/release/SHA256SUMS"

# An installation as 0.3.0 left it (after Python releases): the program in
# ~/.local/share/switchboard, the ~/bin wrapper, the ~/.local/bin link to it,
# Python modules, and a file of the user's beside them.
write_030() {
    local share=$work/home/.local/share/switchboard
    rm -rf "$work/home" && mkdir -p "$work/home/bin" "$work/home/.local/bin" "$share/__pycache__"
    echo 1 > "$share/.switchboard-install"
    { cat "$binary"; echo 0.3.0 copy; } > "$share/switchboard"
    chmod 700 "$share/switchboard"
    printf '#!/bin/sh\n# Managed by switchboard install\nexec %s cli "$@"\n' "$share/switchboard" > "$work/home/bin/switchboard"
    chmod 700 "$work/home/bin/switchboard"
    ln -s "$work/home/bin/switchboard" "$work/home/.local/bin/switchboard"
    echo '# python' > "$share/bridge.py"
    echo pyc > "$share/__pycache__/bridge.cpython-312.pyc"
    echo mine > "$share/notes.txt"
}
for shell in "${shells[@]}"; do
    step "$shell: an installation from before 0.3.1 moves to ~/.local/bin"
    write_030
    run "$shell" > "$work/out"
    [[ -f $installed && ! -L $installed ]] || fail "$shell: the old link is still the command"
    cmp "$installed" "$binary" || fail "$shell: installed binary differs (move)"
    for old in "$work/home/bin/switchboard" "$work/home/.local/share/switchboard/switchboard" \
        "$work/home/.local/share/switchboard/bridge.py" "$work/home/.local/share/switchboard/__pycache__"; do
        [[ ! -e $old && ! -L $old ]] || fail "$shell: $old is left"
    done
    [[ $(cat "$work/home/.local/share/switchboard/notes.txt") == mine ]] || fail "$shell: removed a file of the user's"
    grep -q '^✓ Removed what earlier versions installed in ~/bin and ~/.local/share/switchboard$' "$work/out" ||
        { cat "$work/out"; fail "$shell: the move was not reported"; }
    grep -q "$installed" "$service" || fail "$shell: the service doesn't run the command"
    # It only asked whether the bridge runs (to know the old copy is unused).
    if [[ -e $work/service-calls ]] && grep -Ev ' (print|is-active) ' "$work/service-calls"; then
        fail "$shell: moving the installation did more than ask the service manager"
    fi
    rm -f "$work/service-calls"

    step "$shell: a Claude Code plugin before 0.3.1 keeps ~/bin/switchboard as a link to the command"
    write_030
    echo '[{"id": "switchboard@switchboard", "version": "0.3.0"}]' > "$work/claude-plugins"
    run "$shell" > "$work/out"
    rm "$work/claude-plugins"
    [[ $(readlink "$work/home/bin/switchboard") == "$installed" ]] || fail "$shell: no link for the older plugin"
    grep -q '^! Kept ~/bin/switchboard as a link to the command' "$work/out" ||
        { cat "$work/out"; fail "$shell: the link was not explained"; }
    grep -q 'claude plugin marketplace update switchboard' "$work/out" ||
        { cat "$work/out"; fail "$shell: no plugin update hint"; }
    run "$shell" > "$work/out"
    [[ ! -e $work/home/bin/switchboard && ! -L $work/home/bin/switchboard ]] || fail "$shell: the link outlived the old plugin"

    step "$shell: a ~/bin/switchboard Switchboard didn't write is left alone"
    write_030
    echo 'echo mine' > "$work/home/bin/switchboard"
    run "$shell" > "$work/out"
    [[ $(cat "$work/home/bin/switchboard") == 'echo mine' ]] || fail "$shell: changed a foreign ~/bin/switchboard"
    grep -q "^! Left ~/bin/switchboard alone: Switchboard didn't write it$" "$work/out" ||
        { cat "$work/out"; fail "$shell: not told about the foreign command"; }
done
rm -rf "$work/home" "$work/service-calls" && mkdir "$work/home"

step "an invite in the environment runs init without a terminal, with the options given"
rm -rf "$work/home" && mkdir "$work/home"
token=$(python3 - <<'PY'
import base64, json, secrets
body = json.dumps({"url": "https://127.0.0.1:1", "secret": base64.urlsafe_b64encode(secrets.token_bytes(24)).decode().rstrip("=")})
print("sbi1." + base64.urlsafe_b64encode(body.encode()).decode().rstrip("="))
PY
)
if run sh "SWITCHBOARD_INVITE=$token" -- --no-plugin --name test-box > "$work/out" 2> "$work/err"; then
    fail "init succeeded against an unreachable server"
fi
cmp "$installed" "$binary" || fail "installed binary differs (invite path)"
grep -q "Can't reach the server at 127.0.0.1:1" "$work/err" || { cat "$work/out" "$work/err"; fail "init did not run with the invite"; }
grep -q 'Machine name  test-box' "$work/err" && fail "init asked for names before checking the server"
# The server is checked before any key exists: nothing is left behind.
[[ ! -e $work/home/.config/switchboard/bridge.json ]] || fail "init wrote a configuration for an unreachable server"
# The last step of the fix: run init again, the command alone on its line.
grep -Eq '^ +switchboard init$' "$work/err" || { cat "$work/err"; fail "no rerun hint"; }
if grep -q "$token" "$work/out" "$work/err" "$work/curl-argv" 2>/dev/null; then fail "the invite was printed or on a command line"; fi
if ! (exec < /dev/tty) 2>/dev/null; then
    step "--invite without a terminal says to use SWITCHBOARD_INVITE"
    if env -i HOME="$work/home" PATH="$work/bin:/usr/bin:/bin" sh -s -- --invite < "$root/install.sh" 2>"$work/err"; then
        fail "--invite without a terminal succeeded"
    fi
    grep -q 'or set SWITCHBOARD_INVITE' "$work/err" || { cat "$work/err"; fail "wrong message"; }
fi
if env -i HOME="$work/home" PATH="$work/bin:/usr/bin:/bin" sh -s -- --bogus < "$root/install.sh" 2>"$work/err"; then
    fail "an unknown option was accepted"
fi
grep -q 'Unknown option: --bogus' "$work/err" || { cat "$work/err"; fail "wrong message"; }
if env -i HOME="$work/home" PATH="$work/bin:/usr/bin:/bin" sh -s -- --server --tls < "$root/install.sh" 2>"$work/err"; then
    fail "an option without its value was accepted"
fi
grep -q -- '--tls needs a value' "$work/err" || { cat "$work/err"; fail "wrong message"; }
step "--invite takes the invite on the command line too, as pasted"
# Wrapped and indented, as a chat app hands it over; init cleans it.
pasted="  ${token:0:30}
${token:30}  "
for shell in "${shells[@]}"; do
    for form in space equals pasted; do
        rm -rf "$work/home" && mkdir "$work/home"
        case $form in
            space) set -- --invite "$token" ;;
            equals) set -- "--invite=$token" ;;
            pasted) set -- --invite "$pasted" ;;
        esac
        if run "$shell" -- "$@" --no-plugin --name test-box > "$work/out" 2> "$work/err"; then
            fail "$shell: init succeeded against an unreachable server ($form)"
        fi
        grep -q "Can't reach the server at 127.0.0.1:1" "$work/err" || { cat "$work/out" "$work/err"; fail "$shell: init did not use the invite ($form)"; }
        if grep -q "$token" "$work/out" "$work/err"; then fail "$shell: the invite was printed ($form)"; fi
    done
done
set --

step "options without a terminal go to init, which names a missing answer"
rm -rf "$work/home" && mkdir "$work/home"
if run sh -- --join --no-plugin > "$work/out" 2> "$work/err"; then fail "init joined without an invite"; fi
grep -q 'Missing --invite' "$work/err" || { cat "$work/out" "$work/err"; fail "init did not name the missing option"; }

step "errors say what went wrong and how to fix it, plainly without a terminal"
check_error() {  # check_error PHRASE: $work/err is one failure in the command's shape
    local first
    first=$(head -n 1 "$work/err")
    [[ $first == "✗ "*"$1"*. ]] || { cat "$work/err"; fail "not a failure line naming '$1'"; }
    [[ $(sed -n 2p "$work/err") == "  To fix, "?* ]] || { cat "$work/err"; fail "no fix under the failure"; }
    if grep -q "$(printf '[\033\r]')" "$work/err"; then cat -v "$work/err"; fail "control codes without a terminal"; fi
}
for shell in "${shells[@]}"; do
    if run "$shell" -- --bogus > "$work/out" 2> "$work/err"; then fail "$shell: an unknown option was accepted"; fi
    check_error 'Unknown option: --bogus'
    [[ ! -s $work/out ]] || { cat "$work/out"; fail "$shell: output before an option error"; }
    if run "$shell" SWITCHBOARD_GITHUB=http://127.0.0.1:1 > "$work/out" 2> "$work/err"; then fail "$shell: installed from nowhere"; fi
    check_error 'Could not reach 127.0.0.1:1'
    grep -q 'internet connection' "$work/err" || { cat "$work/err"; fail "$shell: no network fix"; }
    if run "$shell" SWITCHBOARD_VERSION=9.9.9 > "$work/out" 2> "$work/err"; then fail "$shell: installed a missing release"; fi
    check_error 'the server answered 404'
done
step "without a UTF-8 locale the marks are ASCII"
if run sh LANG=C -- --bogus 2> "$work/err"; then fail "an unknown option was accepted"; fi
[[ $(head -n 1 "$work/err") == "x Unknown option: --bogus." ]] || { cat "$work/err"; fail "not an ASCII failure mark"; }
run sh LC_ALL=C > "$work/out"
grep -q '^+ Downloaded Switchboard' "$work/out" || { cat "$work/out"; fail "not an ASCII done mark"; }

step "an already set-up machine whose bridge state is unclear is told how to restart it"
mkdir -p "$work/home/.config/switchboard"
# Readable by others, so the CLI refuses to read it: status cannot answer.
echo '{}' > "$work/home/.config/switchboard/bridge.json"
chmod 644 "$work/home/.config/switchboard/bridge.json"
run sh > "$work/out"
grep -q '^! Could not tell whether the bridge is running' "$work/out" || { cat "$work/out"; fail "no warning"; }
# The two commands, each alone on its line, stop first.
grep -A2 'restart it with$' "$work/out" | tr '\n' ' ' | grep -Eq 'switchboard down +[^ ]*switchboard up' ||
    { cat "$work/out"; fail "no restart hint"; }
grep -q 'switchboard upgrade' "$work/out" || { cat "$work/out"; fail "no upgrade hint"; }

step "an already set-up machine whose bridge is stopped leaves it stopped"
chmod 600 "$work/home/.config/switchboard/bridge.json"
run sh > "$work/out"
grep -q '^✓ This machine is already set up' "$work/out" || { cat "$work/out"; fail "not told it is set up"; }
grep -A1 'stays stopped' "$work/out" | grep -Eq '^ +[^ ]*switchboard up$' ||
    { cat "$work/out"; fail "not told the bridge stays stopped"; }
if grep -q 'switchboard init' "$work/out"; then cat "$work/out"; fail "told to set up a set-up machine"; fi

step "a machine taken down with switchboard down stays down"
touch "$work/home/.config/switchboard/down"
run sh > "$work/out"
grep -A1 'stays down' "$work/out" | grep -Eq '^ +[^ ]*switchboard up$' ||
    { cat "$work/out"; fail "not told the machine stays down"; }
rm "$work/home/.config/switchboard/down"

[[ ! -e $work/service-calls ]] || { cat "$work/service-calls"; fail "install.sh called a service manager"; }

step "a service an earlier version installed under the old name is moved"
if [[ $os == darwin ]]; then
    old=$work/home/Library/LaunchAgents/com.vorelios.switchboard.plist
    sed 's/dev\.switchboard\.bridge/com.vorelios.switchboard/' "$service" > "$old"
else
    old=$work/home/.config/systemd/user/switchboard.service
    sed 's/# Managed by switchboard install/# Managed by Switchboard install_local.py/' "$service" > "$old"
    ln -s "$old" "$work/home/.config/systemd/user/default.target.wants/switchboard.service"
fi
run sh > "$work/out"
[[ ! -e $old && -f $service ]] || fail "the old service was not moved"
[[ ! -L $work/home/.config/systemd/user/default.target.wants/switchboard.service ]] || fail "the old unit's link remains"
# The old service was not running (the fake managers report nothing loaded),
# so it was only checked: nothing was stopped or started.
if grep -Eq 'bootout|bootstrap|kickstart| stop | start ' "$work/service-calls"; then
    cat "$work/service-calls"; fail "a service was stopped or started"
fi
# A bridge running as the fake service manager's job (mode $1): a stand-in
# process the manager stops, with the runtime record the bridge writes.
start_bridge() {
    local pid start
    rm -f "$work/service-calls" "$work/bridge-pid"
    # A parent that reaps it, so once stopped it is gone, not a zombie.
    sh -c 'sleep 600 & echo $! > "$1"; wait' sh "$work/bridge-pid" > /dev/null 2>&1 &
    for _ in $(seq 100); do [[ -s $work/bridge-pid ]] && break; sleep 0.05; done
    pid=$(cat "$work/bridge-pid")
    if [[ $os == darwin ]]; then start=$(LC_ALL=C TZ=UTC ps -o lstart= -p "$pid" | sed 's/^ *//; s/ *$//')
    else start=$(sed 's/.*) //' "/proc/$pid/stat" | awk '{print $20}'); fi
    mkdir -p -m 700 "$work/state"
    (umask 077
     printf '{"pid": %s, "procStart": "%s", "connected": false, "machine": "test-box"}\n' "$pid" "$start" > "$work/state/runtime.json"
     printf '{"state_dir": "%s"}\n' "$work/state" > "$work/home/.config/switchboard/bridge.json")
    echo "$1" > "$work/service-mode"
    touch "$work/loaded"
}

for shell in "${shells[@]}"; do
    step "$shell: an already set-up machine with a running bridge restarts it on the new version"
    start_bridge working
    run "$shell" > "$work/out"
    cmp "$installed" "$binary" || fail "installed binary differs (restart)"
    grep -q '^✓ Restarted the bridge on Switchboard [0-9]' "$work/out" || { cat "$work/out"; fail "the bridge was not restarted"; }
    grep -Eq 'bootout|systemctl --user stop' "$work/service-calls" || { cat "$work/service-calls"; fail "the bridge was not stopped"; }
    grep -Eq 'bootstrap|kickstart|systemctl --user start' "$work/service-calls" || { cat "$work/service-calls"; fail "the bridge was not started"; }
    if kill -0 "$(cat "$work/bridge-pid")" 2>/dev/null; then fail "the old bridge still runs"; fi
    grep -q 'switchboard upgrade' "$work/out" || { cat "$work/out"; fail "no upgrade hint"; }
    if grep -q "$(printf '[\033\r]')" "$work/out"; then cat -v "$work/out"; fail "control codes without a terminal"; fi
done

step "a bridge that will not stop is reported, with what to do"
start_bridge stuck
if run sh > "$work/out" 2> "$work/err"; then fail "a failed restart succeeded"; fi
check_error 'Could not stop the bridge to restart it on Switchboard'
grep -q 'Boot-out failed: 5' "$work/err" || { cat "$work/err"; fail "the reason was not given"; }
grep -q 'switchboard doctor' "$work/err" || { cat "$work/err"; fail "doctor not suggested"; }
grep -q '^✓ Installed the switchboard command' "$work/out" || { cat "$work/out"; fail "the install was not reported"; }
kill "$(cat "$work/bridge-pid")" 2>/dev/null || true
rm -f "$work/service-mode" "$work/loaded"

# A release before 0.3: `up --help` succeeds (0.2 printed its general help),
# but it has no up or down, only start and stop. Pinning it on a machine whose
# bridge runs must restart the bridge with the old binary's own commands.
mkdir -p "$work/old-release"
old_asset=$work/old-release/$asset
cat > "$old_asset" <<EOF
#!/bin/sh
echo "\$*" >> "$work/old-calls"
case "\$1" in
    --version) echo "switchboard 0.2.1" ;;
    status) printf '{\n  "ok": true,\n  "pid": 4242\n}\n' ;;
    install)
        # As earlier releases refuse a command they didn't write.
        if [ -e "$work/old-install-fails" ]; then echo "switchboard install: unmanaged command: \$HOME/bin/switchboard" >&2; exit 1; fi
        mkdir -p "\$HOME/bin" && cp "\$0" "\$HOME/bin/switchboard" && chmod 700 "\$HOME/bin/switchboard" ;;
    start | stop) printf '{"ok": true}\n' ;;
    up | down)
        [ "\$2" != --help ] || exit 0
        printf '{"ok": false, "error": "unknown command %s"}\n' "\$1"; exit 2 ;;
esac
EOF
chmod 700 "$old_asset"
printf '%s  %s\n' "$(sha "$old_asset")" "$asset" > "$work/old-release/SHA256SUMS"
python3 -I "$root/.github/scripts/fake_release_server.py" "$work/old-release" owner/repo 0.2.1 test-token "$work/old-port" &
old_server=$!
for _ in $(seq 300); do
    [[ -s $work/old-port ]] && break
    kill -0 "$old_server" 2>/dev/null || fail "the second fake release server exited"
    sleep 0.1
done
[[ -s $work/old-port ]] || fail "the second fake release server did not start within 30 seconds"
old_port=$(cat "$work/old-port")
for shell in "${shells[@]}"; do
    step "$shell: pinning a release before 0.3 restarts the bridge with its own stop and start"
    rm -f "$work/old-calls"
    run "$shell" SWITCHBOARD_VERSION=0.2.1 SWITCHBOARD_GITHUB="http://127.0.0.1:$old_port" \
        SWITCHBOARD_GITHUB_API="http://127.0.0.1:$old_port/api" > "$work/out" 2> "$work/err" ||
        { cat "$work/out" "$work/err"; fail "$shell: installing 0.2.1 failed"; }
    grep -q '^✓ Restarted the bridge on Switchboard 0.2.1' "$work/out" || { cat "$work/out"; fail "$shell: the bridge was not restarted"; }
    { grep -qx 'stop --json' "$work/old-calls" && grep -qx 'start --json' "$work/old-calls"; } ||
        { cat "$work/old-calls"; fail "$shell: not restarted with stop and start"; }
    if grep -Eq '^(up|down)( |$)' "$work/old-calls"; then cat "$work/old-calls"; fail "$shell: asked 0.2.1 for up or down"; fi
done

for shell in "${shells[@]}"; do
    step "$shell: an older release whose installer fails leaves the installed command in place"
    rm -rf "$work/home" "$work/service-mode" "$work/loaded" && mkdir "$work/home"
    echo '[{"id": "switchboard@switchboard", "version": "0.3.0"}]' > "$work/claude-plugins"
    write_030
    run "$shell" > "$work/out"
    rm "$work/claude-plugins"
    [[ -f $installed && $(readlink "$work/home/bin/switchboard") == "$installed" ]] || fail "$shell: setup for the failing downgrade"
    touch "$work/old-install-fails"
    if run "$shell" SWITCHBOARD_VERSION=0.2.1 SWITCHBOARD_GITHUB="http://127.0.0.1:$old_port" \
        SWITCHBOARD_GITHUB_API="http://127.0.0.1:$old_port/api" > "$work/out" 2> "$work/err"; then
        fail "$shell: a failing older installer succeeded"
    fi
    rm "$work/old-install-fails"
    grep -q 'so the installed version was left as it was' "$work/err" || { cat "$work/err"; fail "$shell: wrong message"; }
    cmp "$installed" "$binary" || fail "$shell: the command was not put back"
    [[ $(readlink "$work/home/bin/switchboard") == "$installed" ]] || fail "$shell: the plugin's link was not put back"
done

echo
echo "install.sh: all checks passed"
