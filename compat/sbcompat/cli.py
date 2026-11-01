"""Run the CLI under test in an isolated home with a private bridge config."""
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import subprocess

from . import pki
from .impl import command
from .util import child_env, private_dir, write_private


@dataclass
class Result:
    code: int
    stdout: str
    stderr: str

    @property
    def json(self):
        return json.loads(self.stdout)

    @property
    def error_json(self):
        return json.loads(self.stderr)


class CLIEnv:
    """A fake home (so nothing reads the real ~/.config or ~/.local), fake
    launchctl and systemctl first on PATH (so nothing queries launchd or a
    systemd user manager), and a bridge config."""

    def __init__(self, test, root, native_env, server=None, machine=None, sessions=(), **config):
        self.test = test
        self.root = private_dir(root)
        self.home = private_dir(self.root / 'home')
        self.bin = private_dir(self.root / 'bin')
        launchctl = self.bin / 'launchctl'
        launchctl.write_text('#!/bin/sh\nexit 113\n')
        launchctl.chmod(0o700)
        systemctl = self.bin / 'systemctl'
        systemctl.write_text('#!/bin/sh\nexit 3\n')
        systemctl.chmod(0o700)
        self.cfg = private_dir(self.root / 'cfg')
        self.state = private_dir(self.root / 'state')
        self.config_path = self.cfg / 'bridge.json'
        self.native = native_env
        identity = private_dir(self.cfg / 'identity')
        if machine is not None:
            for source, name in ((machine.cert, 'cert.pem'), (machine.key, 'key.pem'), (pki.ca(), 'ca.pem')):
                shutil.copyfile(source, identity / name)
                (identity / name).chmod(0o600)
        self.config = dict(enabled=True, hub_url=server.url if server else 'https://localhost:1',
                           ca=str(identity / 'ca.pem'), cert=str(identity / 'cert.pem'), key=str(identity / 'key.pem'),
                           config_dir=str(native_env.config_dir), state_dir=str(self.state), sessions=list(sessions))
        self.config.update(config)
        if self.config is not None:
            self.write_config(self.config)

    def write_config(self, value):
        write_private(self.config_path, value)

    def read_config(self):
        return json.loads(self.config_path.read_text())

    def run(self, *arguments, config=True, env=None, timeout=60, stdin=''):
        argv = (['--config', str(self.config_path)] if config else []) + [str(a) for a in arguments]
        environment = child_env(HOME=str(self.home), PATH=f'{self.bin}:{os.environ.get("PATH", "/usr/bin:/bin")}',
                                NO_COLOR='1', COLUMNS='100', TERM='dumb')
        for key, value in (env or {}).items():
            if value is None:
                environment.pop(key, None)
            else:
                environment[key] = value
        result = subprocess.run(command('cli', *argv), input=stdin, capture_output=True, text=True,
                                env=environment, timeout=timeout, cwd=str(self.root))
        return Result(result.returncode, result.stdout, result.stderr)

    def runtime(self, **fields):
        """Publish a bridge runtime record owned by a live stand-in daemon process."""
        from .util import process_start
        if not hasattr(self, 'daemon'):
            self.daemon = subprocess.Popen(['sleep', '600'])
            self.test.addCleanup(self._stop_daemon)
        value = dict(pid=self.daemon.pid, procStart=process_start(self.daemon.pid), platform='darwin',
                     machine='machine-a', connected=True, discovery_mode='events',
                     discovery_detail=None, published=[], remote=dict(sessions=0, machines=0), updated_at=0)
        value.update(fields)
        write_private(self.state / 'runtime.json', value)
        return value

    def _stop_daemon(self):
        self.daemon.kill()
        self.daemon.wait(5)
