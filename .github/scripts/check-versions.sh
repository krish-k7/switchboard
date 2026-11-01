#!/usr/bin/env bash
# Check that the Claude Code plugin's version is the release version, so that
# Claude Code refreshes its cached copy of the plugin when Switchboard is
# upgraded. The version lives in switchboard/Cargo.toml; set both with
# .github/scripts/set-version.sh.
#
#   .github/scripts/check-versions.sh [TAG]   # with a tag (v1.2.3), check it too
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/switchboard/Cargo.toml" | head -n 1)
plugin=$(python3 -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' \
    "$root/claude-plugin/.claude-plugin/plugin.json")
status=0
if [[ $plugin != "$cargo" ]]; then
    echo "::error::claude-plugin/.claude-plugin/plugin.json has version $plugin, but switchboard/Cargo.toml has $cargo (run .github/scripts/set-version.sh $cargo)"
    status=1
fi
if [[ $# -gt 0 && ${1#v} != "$cargo" ]]; then
    echo "::error::tag $1 does not match version $cargo in switchboard/Cargo.toml"
    status=1
fi
[[ $status != 0 ]] || echo "versions agree: $cargo"
exit "$status"
