#!/usr/bin/env bash
# End-to-end test of `switchboard server install`, its first invite, the
# local invite and machine commands, `server status`, `server logs`, and
# `server uninstall`, run by CI on a disposable Linux runner with systemd and
# passwordless sudo. Never run it on a machine you care about: it installs and starts the
# server system-wide, creates the switchboard account, and changes modes.
#
#   .github/scripts/server-install-test.sh target/release/switchboard
set -euo pipefail

binary=$(realpath "${1:?usage: server-install-test.sh BINARY}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
json() { python3 -c 'import json,sys; v=json.load(sys.stdin)
for k in sys.argv[1:]: v=v[int(k) if k.isdigit() else k]
print(v if not isinstance(v,(dict,list,bool)) and v is not None else json.dumps(v))' "$@"; }
step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }

# The client side runs as this (unprivileged) user with a private HOME and
# fake service managers first on PATH.
export HOME="$work/home"
mkdir -p "$HOME" "$work/bin"
printf '#!/bin/sh\nexit 113\n' > "$work/bin/launchctl"
printf '#!/bin/sh\nexit 3\n' > "$work/bin/systemctl"
chmod 700 "$work/bin/launchctl" "$work/bin/systemctl"
client() { PATH="$work/bin:/usr/bin:/bin" "$binary" "$@"; }
check_mode() { [[ $(sudo stat -c '%a:%U:%G' "$1") == "$2" ]] || fail "$1 is $(sudo stat -c '%a:%U:%G' "$1"), want $2"; }

step "refuses an unmanaged unit and changes nothing"
printf '[Service]\nExecStart=/bin/true\n' | sudo tee /etc/systemd/system/switchboard-server.service >/dev/null
if sudo "$binary" server install --tls private 2>"$work/err"; then fail "installed over an unmanaged unit"; fi
grep -qi 'refusing unmanaged unit' "$work/err" || { cat "$work/err"; fail "wrong refusal"; }
! getent passwd switchboard >/dev/null || fail "created the account before refusing"
sudo rm /etc/systemd/system/switchboard-server.service

step "refuses an unmanaged /etc/switchboard and changes nothing"
sudo mkdir -m 755 /etc/switchboard
echo keep | sudo tee /etc/switchboard/other.conf >/dev/null
if sudo "$binary" server install --tls private 2>"$work/err"; then fail "took over an unmanaged /etc/switchboard"; fi
grep -qi 'refusing unmanaged directory: /etc/switchboard' "$work/err" || { cat "$work/err"; fail "wrong refusal"; }
check_mode /etc/switchboard 755:root:root
! getent passwd switchboard >/dev/null || fail "created the account before refusing"
sudo rm -r /etc/switchboard

step "installs in private mode"
sudo "$binary" server install --tls private --name localhost --name 127.0.0.1 --json | tee "$work/install.json"
first_invite=$(json invite < "$work/install.json")
pin=$(json server_pin < "$work/install.json")
url=$(json server_url < "$work/install.json")
[[ $url == https://localhost:8743 ]] || fail "server_url $url"
[[ $first_invite == sbi1.* && ${#pin} == 64 ]] || fail "invite or pin missing"
systemctl is-active --quiet switchboard-server.service || fail "service not active"
check_mode /var/lib/switchboard 700:switchboard:switchboard
check_mode /var/lib/switchboard/peers.json 600:switchboard:switchboard
check_mode /var/lib/switchboard/invites.json 600:switchboard:switchboard
check_mode /var/lib/switchboard/peers.json.lock 600:switchboard:switchboard
secret=$(python3 -c 'import base64,json,sys; b=sys.argv[1][5:]; print(json.loads(base64.urlsafe_b64decode(b+"="*(-len(b)%4)))["secret"])' "$first_invite")
[[ ${#secret} -ge 22 ]] || fail "the invite has no secret"
! sudo grep -q -- "$secret" /var/lib/switchboard/invites.json || fail "the invite's secret is stored in the clear"
check_mode /var/lib/switchboard/tls 700:switchboard:switchboard
check_mode /var/lib/switchboard/tls/private-key.pem 600:switchboard:switchboard
check_mode /etc/switchboard 700:switchboard:switchboard
check_mode /etc/switchboard/server.json 600:switchboard:switchboard
check_mode /etc/systemd/system/switchboard-server.service 644:root:root
check_mode /usr/local/lib/switchboard/switchboard 755:root:root
[[ $(readlink /usr/local/bin/switchboard) == /usr/local/lib/switchboard/switchboard ]] || fail "command link"
[[ $(ps -o user= -p "$(systemctl show -p MainPID --value switchboard-server.service)") == switchboard ]] || fail "not running as switchboard"
systemd-analyze security switchboard-server.service --no-pager | tail -1

step "a rerun changes nothing and does not restart the server"
before=$(systemctl show -p MainPID --value switchboard-server.service)
sudo "$binary" server install --tls private --name localhost --name 127.0.0.1 --json > "$work/rerun.json"
[[ $(json changed < "$work/rerun.json") == '[]' ]] || fail "rerun changed $(json changed < "$work/rerun.json")"
[[ $(systemctl show -p MainPID --value switchboard-server.service) == "$before" ]] || fail "rerun restarted the server"

step "the summary prints the first machine's invite and how to join with it"
# Each run while no machine is enrolled replaces the invite an earlier run printed.
sudo "$binary" server install --tls private --name localhost --name 127.0.0.1 > "$work/human.txt"
cat "$work/human.txt"
grep -q -- "/install.sh" "$work/human.txt" || fail "no install command for the new machine"
if grep -q -- "switchboard setup" "$work/human.txt"; then fail "the summary points at setup, not init"; fi
# The invite alone on its line, at the copy indent (one line without a terminal).
token=$(sed -n 's/^ *\(sbi1\.[A-Za-z0-9_-]*\)$/\1/p' "$work/human.txt")
[[ $token == sbi1.* ]] || fail "no invite in the summary"
if grep -q -- "--invite $token" "$work/human.txt"; then fail "the invite is printed on a command line"; fi

step "the invite, on standard input, pins the server and enrolls the first machine"
printf '%s\n' "$token" |
    client setup --invite - --machine ci-first --owner ci --no-start --json | tee "$work/setup.json"
[[ $(json enrolled < "$work/setup.json") == true ]] || fail "not enrolled"
[[ $(json ca < "$HOME/.config/switchboard/bridge.json") == "$HOME/.config/switchboard/server-cert.pem" ]] || fail "server not pinned"
sudo "$binary" server machines --json | tee "$work/machines.json"
[[ $(json machines 0 machine < "$work/machines.json") == ci-first ]] || fail "server machines does not list ci-first"
[[ $(json machines 0 last_seen < "$work/machines.json") != null ]] || fail "no last_seen for ci-first"
[[ $(sudo "$binary" server invites --json | json invites) == '[]' ]] || fail "the used invite is still open"
# The invite the first run printed was replaced by later runs.
replaced="$work/replaced/bridge.json"
printf '%s\n' "$first_invite" | client --config "$replaced" setup --invite - --machine ci-old --owner ci --no-start --json > "$work/old.json" || true
[[ $(json error < "$work/old.json") == "the server didn't accept the invite"* ]] || fail "a replaced invite worked: $(cat "$work/old.json")"

step "with a machine enrolled, a rerun prints no invite"
sudo "$binary" server install --tls private --name localhost --name 127.0.0.1 --json > "$work/third.json"
[[ $(json invite < "$work/third.json") == null ]] || fail "invite printed with a machine enrolled"

step "server status as root: healthy, with counts, the pin, and both address families"
sudo "$binary" server status --json | tee "$work/status.json" || { sudo "$binary" server status; fail "status found a problem"; }
[[ $(json ok < "$work/status.json") == true && $(json problems < "$work/status.json") == '[]' ]] || fail "status not healthy"
[[ $(json service state < "$work/status.json") == running && $(json service enabled < "$work/status.json") == true ]] || fail "service state"
[[ $(json version < "$work/status.json") == "$(json command_version < "$work/status.json")" ]] || fail "installed version"
[[ $(json mode < "$work/status.json") == private && $(json address < "$work/status.json") == "$url" ]] || fail "mode or address"
[[ $(json pin < "$work/status.json") == "$pin" && $(json certificate served < "$work/status.json") == true ]] || fail "pin"
[[ $(json answering < "$work/status.json") == true ]] || fail "not answering"
[[ $(json listening < "$work/status.json") == '["[::]:8743"]' ]] || fail "listening $(json listening < "$work/status.json")"
[[ $(json ipv4 < "$work/status.json") == true && $(json ipv6 < "$work/status.json") == true ]] || fail "address families"
[[ $(json machines enrolled < "$work/status.json") == 1 && $(json machines seen_last_10_minutes < "$work/status.json") == 1 ]] || fail "machine counts"
[[ $(json invites open < "$work/status.json") == 0 ]] || fail "invite count"
[[ $(json database bytes < "$work/status.json") -gt 0 && $(json errors readable < "$work/status.json") == true ]] || fail "database or journal"
sudo "$binary" server status | tee "$work/status.txt"
grep -q ' The server is healthy.$' "$work/status.txt" || fail "no healthy line"
grep -q '^  Machines     1 machine, seen in the last 10 minutes$' "$work/status.txt" || fail "machines row"
grep -q "^  Pin          $pin\$" "$work/status.txt" || fail "pin row"
! grep -q ci-first "$work/status.txt" || fail "status lists machines"

step "server status as the service account works; other users are told to use sudo"
sudo -u switchboard /usr/local/bin/switchboard server status --json > "$work/status-service.json" || fail "status as the service account"
[[ $(json errors readable < "$work/status-service.json") == false && $(json machines enrolled < "$work/status-service.json") == 1 ]] || fail "service account status"
if "$binary" server status > "$work/status-user.txt" 2>&1; then fail "an unprivileged user read the status"; fi
grep -q "sudo switchboard server status" "$work/status-user.txt" || { cat "$work/status-user.txt"; fail "no sudo hint"; }

step "server logs shows the service's and the server's events, as text and JSON"
sudo "$binary" server logs --lines 200 | tee "$work/logs.txt" | tail -5
grep -q '^  Server events  ' "$work/logs.txt" || fail "no heading"
grep -q '  started' "$work/logs.txt" || fail "no started event"
tr -s ' \n' ' ' < "$work/logs.txt" | grep -E ' invite_redeemed ci-first joined with invite [0-9a-f]{12}' >/dev/null || fail "no invite_redeemed event"
sudo "$binary" server logs --lines 200 --since 1h --json > "$work/logs.jsonl"
python3 - "$work/logs.jsonl" "$pin" <<'PY' || fail "server logs --json"
import json, sys
events = [json.loads(line) for line in open(sys.argv[1])]
assert events and all(set(e) == {"time", "source", "level", "event", "detail", "message"} for e in events), events[:2]
assert any(e["event"] == "private_certificate" and sys.argv[2] in e["message"] for e in events)
assert any(e["source"] == "systemd" and e["event"] == "started" for e in events)
PY
timeout 5 sudo "$binary" server logs --follow --lines 3 --json > "$work/follow.jsonl" || [[ $? == 124 ]] || fail "server logs --follow"
[[ $(wc -l < "$work/follow.jsonl") -ge 1 ]] || fail "follow printed nothing"
sleep 1
! pgrep -f 'journalctl --unit=switchboard-server' >/dev/null || fail "journalctl outlived server logs --follow"
if "$binary" server logs --json > "$work/logs-user.json" 2>&1 && ! id -nG | grep -qwE 'adm|systemd-journal|wheel'; then
    fail "an unprivileged user read the journal"
fi

step "init as root, without a terminal, keeps the server as it is and makes an invite a user's init joins with"
sudo "$binary" init --server --tls private --server-name localhost --server-name 127.0.0.1 --json | tee "$work/init-server.json"
[[ $(json ok < "$work/init-server.json") == true ]] || fail "init --server failed"
[[ $(json server_url < "$work/init-server.json") == "$url" && $(json server_pin < "$work/init-server.json") == "$pin" ]] ||
    fail "init names the wrong server"
init_invite=$(json invite < "$work/init-server.json")
[[ $init_invite == sbi1.* ]] || fail "init made no invite"
mkdir -p "$work/init-home/.claude"
printf '%s\n' "$init_invite" | HOME="$work/init-home" client init --invite --name ci-init --owner ci --no-start --no-plugin --json | tee "$work/init-join.json"
[[ $(json machine < "$work/init-join.json") == ci-init ]] || fail "init did not join with the invite"
[[ $(json ca < "$work/init-home/.config/switchboard/bridge.json") == "$work/init-home/.config/switchboard/server-cert.pem" ]] ||
    fail "init did not pin the server"

step "invites made on the server by root or the service account, never by other users"
if "$binary" server invite --json > "$work/denied.json" 2>&1; then fail "an unprivileged user made an invite"; fi
sudo -u switchboard /usr/local/bin/switchboard server invite --json | tee "$work/service-invite.json"
[[ $(json server_pin < "$work/service-invite.json") == "$pin" && $(json url < "$work/service-invite.json") == "$url" ]] ||
    fail "the service account's invite names the wrong server"
sudo "$binary" server invite --name ci-member --expires 1h --json > "$work/member-invite.json"
member="$work/member/bridge.json"
SWITCHBOARD_INVITE=$(json invite < "$work/member-invite.json") client --config "$member" setup --owner ci --no-start --json | tee "$work/member.json"
[[ $(json machine < "$work/member.json") == ci-member ]] || fail "the invite's fixed name was not used"
check_mode /var/lib/switchboard/invites.json 600:switchboard:switchboard
id=$(json id < "$work/service-invite.json")
[[ $(sudo "$binary" server invites --json | json invites 0 id) == "$id" ]] || fail "server invites does not list the open invite"
sudo "$binary" server invite revoke "$id" --json | grep -q '"state": "revoked"' || fail "invite revoke"
[[ $(sudo "$binary" server invites --json | json invites) == '[]' ]] || fail "a revoked invite is still listed"
sudo "$binary" server revoke ci-member --json | grep -q '"state": "revoked"' || fail "server revoke"
check_mode /var/lib/switchboard/peers.json 600:switchboard:switchboard
client --config "$member" setup --no-start --json > "$work/revoked.json" || true
[[ $(json error < "$work/revoked.json") == "this machine isn't on the server"* ]] || fail "a revoked machine still connects: $(cat "$work/revoked.json")"

step "new names issue the certificate again, and a machine re-pins with setup --server-pin"
sudo "$binary" server install --tls private --name localhost --name 127.0.0.1 --name switchboard.internal --json | tee "$work/renamed.json"
newpin=$(json server_pin < "$work/renamed.json")
[[ $(json certificate_reissued < "$work/renamed.json") == true && $newpin != "$pin" ]] || fail "the certificate was not issued again"
if client setup --no-start --json >/dev/null 2>&1; then fail "the old pin still connects"; fi
client setup --server-pin "$newpin" --no-start --json | tee "$work/repin.json"
[[ $(json ok < "$work/repin.json") == true ]] || fail "setup did not re-pin"
client setup --no-start --json >/dev/null || fail "setup fails after re-pinning"
pin=$newpin
sudo "$binary" server install --tls private --json > "$work/kept.json"
[[ $(json server_pin < "$work/kept.json") == "$pin" && $(json changed < "$work/kept.json") == '[]' ]] ||
    fail "a rerun without --name changed something: $(cat "$work/kept.json")"

step "links the service account puts in its TLS directory are refused, their targets untouched"
sudo install -d -m 755 /root/sb-target
sudo install -m 640 /etc/hostname /root/sb-secret
sudo -u switchboard mv /var/lib/switchboard/tls /var/lib/switchboard/tls.saved
sudo -u switchboard ln -s /root/sb-target /var/lib/switchboard/tls
if sudo "$binary" server install --tls private --no-start 2>"$work/err"; then fail "followed a linked TLS directory"; fi
grep -qi 'refusing non-directory: /var/lib/switchboard/tls' "$work/err" || { cat "$work/err"; fail "wrong refusal"; }
check_mode /root/sb-target 755:root:root
sudo -u switchboard rm /var/lib/switchboard/tls
sudo -u switchboard mv /var/lib/switchboard/tls.saved /var/lib/switchboard/tls
sudo -u switchboard mv /var/lib/switchboard/tls/private-key.pem /var/lib/switchboard/tls/key.saved
sudo -u switchboard ln -s /root/sb-secret /var/lib/switchboard/tls/private-key.pem
if sudo "$binary" server install --tls private --no-start 2>"$work/err"; then fail "followed a linked key"; fi
grep -qi 'refusing unsafe file: /var/lib/switchboard/tls/private-key.pem' "$work/err" || { cat "$work/err"; fail "wrong refusal"; }
check_mode /root/sb-secret 640:root:root
sudo -u switchboard rm /var/lib/switchboard/tls/private-key.pem
sudo -u switchboard mv /var/lib/switchboard/tls/key.saved /var/lib/switchboard/tls/private-key.pem
sudo rm -r /root/sb-target /root/sb-secret

step "enroll-local works without Caddy in front of a built-in TLS server"
local="$work/local/bridge.json"
# An invite for an unreachable server just makes the key and config.
unreachable=$(python3 - <<'PY'
import base64, json, secrets
body = json.dumps({"url": "https://127.0.0.1:1", "secret": base64.urlsafe_b64encode(secrets.token_bytes(24)).decode().rstrip("=")})
print("sbi1." + base64.urlsafe_b64encode(body.encode()).decode().rstrip("="))
PY
)
printf '%s\n' "$unreachable" | client --config "$local" setup --invite - --machine ci-local --owner ci --no-start --json || true
python3 - "$work/local/identity/cert.pem" > "$work/request.json" <<'PY'
import json, sys
print(json.dumps({"machine": "ci-local", "owner": "ci",
                  "certificate_pem": open(sys.argv[1]).read()}))
PY
sudo "$binary" server enroll-local < "$work/request.json" | tee "$work/enrolled.json"
[[ $(json ok < "$work/enrolled.json") == true ]] || fail "enroll-local refused"
python3 - "$local" "$url" "$HOME/.config/switchboard/server-cert.pem" <<'PY'
import json, sys
path, url, ca = sys.argv[1:]
config = json.load(open(path)); config.update(server_url=url, ca=ca)
open(path, "w").write(json.dumps(config, indent=2) + "\n")
PY
client --config "$local" setup --no-start --json | tee "$work/local-setup.json"
[[ $(json machine < "$work/local-setup.json") == ci-local ]] || fail "the machine enroll-local added cannot reach the server"

step "acme mode: the unit gets the port-443 capability; the server binds 443 unprivileged"
sudo "$binary" server install --tls acme --domain switchboard.example.com --no-start --json > "$work/acme.json"
grep -qx 'AmbientCapabilities=CAP_NET_BIND_SERVICE' /etc/systemd/system/switchboard-server.service || fail "no capability"
# Point ACME at an unreachable directory so no certificate authority is contacted.
sudo python3 - <<'PY'
import json
path = "/etc/switchboard/server.json"
config = json.load(open(path)); config["acme_directory"] = "https://127.0.0.1:9/directory"
open(path, "w").write(json.dumps(config, indent=2) + "\n")
PY
sudo systemctl daemon-reload
sudo systemctl restart switchboard-server.service
for _ in $(seq 50); do (exec 3<>/dev/tcp/127.0.0.1/443) 2>/dev/null && break; sleep 0.2; done
(exec 3<>/dev/tcp/127.0.0.1/443) 2>/dev/null || { sudo journalctl -u switchboard-server --no-pager | tail -20; fail "nothing listens on 443"; }
sleep 1
sudo journalctl -u switchboard-server --no-pager | grep -q certificate_error || fail "no certificate_error logged"
sudo "$binary" server status --json > "$work/status-acme.json" && fail "status passed without a certificate"
python3 - "$work/status-acme.json" <<'PY' || { cat "$work/status-acme.json"; fail "acme status"; }
import json, sys
s = json.load(open(sys.argv[1]))
assert s["mode"] == "acme" and s["service"]["state"] == "running", s
assert s["certificate"] is None and any(p["check"] == "certificate" and p["level"] == "fail" for p in s["problems"]), s
assert s["errors"]["since_start"] >= 1 and s["errors"]["recent"][-1]["event"] == "certificate_error", s
PY
sudo "$binary" server status || true

step "caddy mode: the Unix socket for Caddy, owned switchboard:caddy 0660"
sudo groupadd --system caddy
sudo "$binary" server install --tls caddy --domain switchboard.example.com --json | tee "$work/caddy.json"
check_mode /run/switchboard 750:switchboard:caddy
check_mode /run/switchboard/hub.sock 660:switchboard:caddy
fingerprint=$(openssl x509 -in "$HOME/.config/switchboard/identity/cert.pem" -outform DER | sha256sum | cut -c1-64)
sudo curl -sS --unix-socket /run/switchboard/hub.sock -H "X-Switchboard-Client-Fingerprint: $fingerprint" http://localhost/health | tee "$work/health.json"
echo
[[ $(json machine < "$work/health.json") == ci-first ]] || fail "the first machine is not known over the socket"
[[ $(json invite < "$work/caddy.json") == null ]] || fail "caddy mode printed an invite with a machine enrolled"
sudo "$binary" server status --json > "$work/status-caddy.json" || { sudo "$binary" server status; fail "caddy status found a problem"; }
[[ $(json mode < "$work/status-caddy.json") == caddy && $(json socket < "$work/status-caddy.json") == /run/switchboard/hub.sock ]] || fail "caddy mode"
[[ $(json answering < "$work/status-caddy.json") == true && $(json certificate < "$work/status-caddy.json") == null ]] || fail "caddy answer"

step "the binary refuses to install beside an older server's service (switchboard-hub)"
printf '[Service]\nExecStart=/bin/sleep infinity\n[Install]\nWantedBy=multi-user.target\n' | sudo tee /etc/systemd/system/switchboard-hub.service >/dev/null
sudo systemctl daemon-reload
sudo systemctl enable --quiet switchboard-hub.service
if sudo "$binary" server install --tls caddy 2>"$work/err"; then fail "installed beside switchboard-hub"; fi
grep -q 'switchboard-hub.service' "$work/err" || { cat "$work/err"; fail "wrong refusal"; }

sudo systemctl disable --now --quiet switchboard-hub.service
sudo rm /etc/systemd/system/switchboard-hub.service
sudo systemctl daemon-reload

step "uninstall refuses a unit it didn't write and changes nothing"
sudo cp /etc/systemd/system/switchboard-server.service "$work/unit.saved"
printf '[Service]\nExecStart=/bin/true\n' | sudo tee /etc/systemd/system/switchboard-server.service >/dev/null
if sudo "$binary" server uninstall 2>"$work/err"; then fail "uninstalled over an unmanaged unit"; fi
tr -s ' \n' ' ' < "$work/err" | grep "switchboard-server.service wasn't written by the server installer, so nothing was removed." >/dev/null || { cat "$work/err"; fail "wrong refusal"; }
[[ -e /usr/local/lib/switchboard/switchboard && -L /usr/local/bin/switchboard ]] || fail "removed something after refusing"
systemctl is-active --quiet switchboard-server.service || fail "stopped the service after refusing"
sudo cp "$work/unit.saved" /etc/systemd/system/switchboard-server.service
sudo systemctl daemon-reload

step "uninstall (with the installed command) stops the service and removes only what install wrote"
echo mine | sudo tee /usr/local/lib/switchboard/notes.txt >/dev/null
sudo /usr/local/bin/switchboard server uninstall --json | tee "$work/uninstall.json"
python3 - "$work/uninstall.json" <<'PY' || fail "uninstall result"
import json, sys
u = json.load(open(sys.argv[1]))
assert u["ok"] and u["stopped"] and not u["account_removed"] and u["account_kept"], u
assert u["removed"] == ["/etc/systemd/system/switchboard-server.service",
                        "/usr/local/lib/switchboard/switchboard",
                        "/usr/local/bin/switchboard"], u["removed"]
kept = {k["kind"]: k["path"] for k in u["kept"]}
assert kept == {"configuration": "/etc/switchboard/server.json",
                "registry": "/var/lib/switchboard/peers.json",
                "invites": "/var/lib/switchboard/invites.json",
                "messages": "/var/lib/switchboard/messages.sqlite3",
                "certificates": "/var/lib/switchboard/tls",
                "other": "/usr/local/lib/switchboard/notes.txt"}, kept
# ci-first, ci-init, and ci-local; ci-member was revoked.
assert [k.get("machines") for k in u["kept"] if k["kind"] == "registry"] == [3], u["kept"]
PY
! systemctl is-active --quiet switchboard-server.service || fail "service still active"
! systemctl is-enabled --quiet switchboard-server.service 2>/dev/null || fail "service still enabled"
[[ ! -e /etc/systemd/system/switchboard-server.service && ! -e /usr/local/bin/switchboard ]] || fail "unit or link left"
[[ $(sudo cat /usr/local/lib/switchboard/notes.txt) == mine ]] || fail "removed a file install didn't write"
sudo test -f /var/lib/switchboard/peers.json && getent passwd switchboard >/dev/null || fail "removed the state or the account"
sudo rm /usr/local/lib/switchboard/notes.txt

step "uninstall again: nothing left to do, the same things kept, and the summary says where"
sudo "$binary" server uninstall --json > "$work/uninstall2.json"
[[ $(json removed < "$work/uninstall2.json") == '["/usr/local/lib/switchboard"]' && $(json stopped < "$work/uninstall2.json") == false ]] ||
    fail "second uninstall: $(cat "$work/uninstall2.json")"
sudo "$binary" server uninstall | tee "$work/uninstall3.txt"
tr -s ' \n' ' ' < "$work/uninstall3.txt" | grep " The Switchboard server isn't installed here; there was nothing to remove. " >/dev/null || fail "not idempotent"
grep -q '^  Registry       /var/lib/switchboard/peers.json, 3 machines$' "$work/uninstall3.txt" || fail "kept registry row"
grep -q 'sudo switchboard server uninstall --purge' "$work/uninstall3.txt" || fail "no purge hint"

step "installing again picks up the kept state: same pin, same machines"
sudo "$binary" server install --tls private --json > "$work/reinstall.json"
[[ $(json server_pin < "$work/reinstall.json") == "$pin" && $(json invite < "$work/reinstall.json") == null ]] || fail "reinstall lost the state"
sudo "$binary" server machines --json | grep -q '"machine": "ci-first"' || fail "reinstall lost the machines"
client setup --no-start --json >/dev/null || fail "a joined machine can't reach the reinstalled server"

step "purge removes the configuration, state, and account, leaving files install didn't write"
echo mine | sudo tee /var/lib/switchboard/operator-notes.txt >/dev/null
sudo "$binary" server uninstall --purge --json | tee "$work/purge.json"
[[ $(json account_removed < "$work/purge.json") == false && $(json account_kept < "$work/purge.json") == true ]] || fail "account removed with its home in use"
[[ $(json kept < "$work/purge.json") == '[{"kind": "other", "path": "/var/lib/switchboard/operator-notes.txt"}]' ]] || fail "purge kept $(json kept < "$work/purge.json")"
[[ ! -e /etc/switchboard && $(sudo ls -A /var/lib/switchboard) == operator-notes.txt ]] || fail "purge left the installer's files"
sudo rm /var/lib/switchboard/operator-notes.txt
sudo "$binary" server uninstall --purge | tee "$work/purge2.txt"
grep -q ' Removed the switchboard account.$' "$work/purge2.txt" || fail "account not removed"
! getent passwd switchboard >/dev/null && ! getent group switchboard >/dev/null || fail "account or group left"
for path in /etc/switchboard /var/lib/switchboard /usr/local/lib/switchboard /usr/local/bin/switchboard /etc/systemd/system/switchboard-server.service; do
    [[ ! -e $path ]] || fail "$path left after purge"
done
sudo "$binary" server uninstall --purge --json > "$work/purge3.json"
[[ $(json removed < "$work/purge3.json") == '[]' && $(json kept < "$work/purge3.json") == '[]' ]] || fail "purge not idempotent"

echo
echo "server install: all checks passed"
