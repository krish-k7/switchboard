"""Shared test utilities: private temp trees, process identity, waiting, normalization."""
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from urllib.parse import quote

UUID_RE = re.compile(r'[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}')
HEX32_RE = re.compile(r'[0-9a-f]{32}')
HEX64_RE = re.compile(r'[0-9a-f]{64}')
BASE_ENV_DROP = ('CLAUDE_', 'ANTHROPIC_', 'SWITCHBOARD_')


def private_root(test, prefix='sbc-'):
    """A fresh 0700 directory under /tmp (short, for socket paths), symlinks resolved,
    owned by this user's primary group, removed when the test ends."""
    root = Path(tempfile.mkdtemp(prefix=prefix, dir='/tmp')).resolve()
    os.chown(root, -1, os.getegid())
    root.chmod(0o700)
    test.addCleanup(shutil.rmtree, root, ignore_errors=True)
    return root


def private_dir(path, mode=0o700):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    os.chown(path, -1, os.getegid())
    path.chmod(mode)
    return path


def write_private(path, value, mode=0o600):
    """Write JSON (or bytes/str) atomically with the given mode."""
    path = Path(path)
    data = value if isinstance(value, bytes) else (value if isinstance(value, str) else json.dumps(value, indent=2) + '\n').encode()
    temporary = path.with_name('.' + path.name + '.' + uuid.uuid4().hex[:8])
    fd = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, mode)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(data)
    os.chmod(temporary, mode)
    os.replace(temporary, path)
    return path


def read_json(path):
    return json.loads(Path(path).read_text())


def process_start(pid):
    """The process-start token native Claude records (macOS ps lstart in UTC/C locale)."""
    if sys.platform == 'linux':
        data = Path(f'/proc/{pid}/stat').read_text()
        return data[data.rindex(')') + 2:].split()[19]
    return subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(pid)],
                                   env=dict(os.environ, LC_ALL='C', TZ='UTC'), text=True).strip()


def pid_domain():
    if sys.platform == 'darwin':
        return 'darwin'
    return 'linux:' + Path('/etc/machine-id').read_text().strip() + ':' + os.readlink('/proc/self/ns/pid')


def native_address(path):
    return 'uds:' + quote(str(path), safe='/:._-\\')


def wait_until(check, timeout=10.0, interval=0.05, message='condition'):
    """Poll check() until it returns a truthy value; return that value."""
    deadline = time.monotonic() + timeout
    last_error = None
    while True:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, KeyError) as error:
            last_error = error
        if time.monotonic() >= deadline:
            raise AssertionError(f'timed out after {timeout}s waiting for {message}'
                                 + (f' (last error: {last_error})' if last_error else ''))
        time.sleep(interval)


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def child_env(**extra):
    """Environment for components: no inherited Claude/Switchboard state, no bytecode."""
    env = {key: value for key, value in os.environ.items() if not key.startswith(BASE_ENV_DROP)}
    env['PYTHONDONTWRITEBYTECODE'] = '1'
    # Never ask GitHub for a newer release from a test.
    env['SWITCHBOARD_NO_UPDATE_CHECK'] = '1'
    env.update({key: value for key, value in extra.items() if value is not None})
    for key, value in extra.items():
        if value is None:
            env.pop(key, None)
    return env


def is_time(value, slack=120):
    """A plausible current Unix time (float or int seconds)."""
    return isinstance(value, (int, float)) and not isinstance(value, bool) and abs(value - time.time()) < slack


def is_uuid(value):
    return isinstance(value, str) and UUID_RE.fullmatch(value) is not None


def is_hex32(value):
    return isinstance(value, str) and HEX32_RE.fullmatch(value) is not None


def is_hex64(value):
    return isinstance(value, str) and HEX64_RE.fullmatch(value) is not None


def scrub(test, value, **rules):
    """Validate and replace nondeterministic fields, recursively, by key name.

    rules maps a key to a validator; each matching value must pass it and is then
    replaced by '<key>'. Everything else is returned unchanged for exact comparison.
    """
    if isinstance(value, dict):
        result = {}
        for key, item in value.items():
            if key in rules:
                test.assertTrue(rules[key](item), f'{key}={item!r} failed its check')
                result[key] = '<' + key + '>'
            else:
                result[key] = scrub(test, item, **rules)
        return result
    if isinstance(value, list):
        return [scrub(test, item, **rules) for item in value]
    return value
