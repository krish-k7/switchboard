#!/usr/bin/env bash
# Set the release version everywhere it is written: the crate
# (switchboard/Cargo.toml and its Cargo.lock entry) and the Claude Code
# plugin (claude-plugin/.claude-plugin/plugin.json). Run it, commit, then tag
# the commit v<VERSION>; CI and the release workflow refuse versions that
# disagree (check-versions.sh).
#
#   .github/scripts/set-version.sh 1.2.3
set -euo pipefail
version=${1:?usage: set-version.sh VERSION}
version=${version#v}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "not a version: $version" >&2; exit 1; }
root=$(cd "$(dirname "$0")/../.." && pwd)
python3 -I - "$root" "$version" <<'PY'
import json, re, sys
root, version = sys.argv[1:]

def rewrite(path, pattern, replacement, count=1):
    with open(path) as handle:
        text = handle.read()
    updated, found = re.subn(pattern, replacement, text, count=count, flags=re.M)
    if found != count:
        sys.exit(f'{path}: version not found')
    with open(path, 'w') as handle:
        handle.write(updated)

rewrite(f'{root}/switchboard/Cargo.toml', r'^version = ".*"$', f'version = "{version}"')
rewrite(f'{root}/Cargo.lock', r'(^name = "switchboard"\nversion = )".*"$', rf'\g<1>"{version}"')
path = f'{root}/claude-plugin/.claude-plugin/plugin.json'
with open(path) as handle:
    plugin = json.load(handle)
plugin['version'] = version
with open(path, 'w') as handle:
    handle.write(json.dumps(plugin, indent=2) + '\n')
PY
echo "set version $version"
