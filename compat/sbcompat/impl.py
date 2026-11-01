"""The binary under test, and how to start each component from it."""
import json
import os
from pathlib import Path
import shlex
import unittest

COMPAT = Path(__file__).resolve().parents[1]
REPOSITORY = COMPAT.parent
DEFAULT_BINARY = REPOSITORY / 'target' / 'release' / 'switchboard'
COMPONENTS = ('server', 'bridge', 'helper', 'cli')


def resolve(binary=None, overrides=None):
    """Return {binary, server, bridge, helper, cli}: each component is `<binary> <component>`
    unless an override (shell words) replaces its command."""
    path = Path(binary or DEFAULT_BINARY).resolve()
    result = {'binary': str(path)}
    for component in COMPONENTS:
        command = (overrides or {}).get(component)
        result[component] = shlex.split(command) if command else [str(path), component]
    return result


def current():
    raw = os.environ.get('SBCOMPAT_IMPL')
    return json.loads(raw) if raw else resolve()


IMPL = current()


def command(component, *arguments):
    return [*IMPL[component], *map(str, arguments)]


def slow(test):
    """Tests that wait on real protocol timers (30-second expiry and similar)."""
    return unittest.skipIf(os.environ.get('SBCOMPAT_QUICK') == '1', 'slow test skipped by --quick')(test)
