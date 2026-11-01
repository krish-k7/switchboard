"""Run one bridge process against a server under test, with fake native sessions."""
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import subprocess
import threading
import time

from . import pki
from .impl import command
from .util import child_env, private_dir, wait_until, write_private


class BridgeProcess:
    def __init__(self, test, root, server, machine, env, sessions=(), **config):
        self.test, self.server, self.machine, self.env = test, server, machine, env
        self.root = private_dir(root)
        self.cfg = private_dir(self.root / 'cfg')
        self.state = self.root / 'state'
        self.config_path = self.cfg / 'bridge.json'
        identity = private_dir(self.cfg / 'identity')
        for source, name in ((machine.cert, 'cert.pem'), (machine.key, 'key.pem'), (pki.ca(), 'ca.pem')):
            shutil.copyfile(source, identity / name)
            (identity / name).chmod(0o600)
        self.config = dict(enabled=True, hub_url=server.url, ca=str(identity / 'ca.pem'), cert=str(identity / 'cert.pem'),
                           key=str(identity / 'key.pem'), config_dir=str(env.config_dir), state_dir=str(self.state),
                           sessions=list(sessions))
        self.config.update(config)
        self.write_config(self.config)
        self.process = None
        self.logs = queue.Queue()
        self.log_lines = []

    def write_config(self, value):
        write_private(self.config_path, value)

    def read_config(self):
        return json.loads(self.config_path.read_text())

    def start(self):
        self.stderr = open(self.root / 'stderr.log', 'ab')
        argv = command('bridge', '--config', self.config_path)
        self.process = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                        stdout=subprocess.PIPE, stderr=self.stderr, text=True, env=child_env())
        threading.Thread(target=self._read, args=(self.process,), daemon=True).start()
        self.test.addCleanup(self.close)
        return self

    def _read(self, process):
        for line in process.stdout:
            self.log_lines.append(line)
            try:
                self.logs.put(json.loads(line))
            except ValueError:
                self.logs.put({'unparsed': line})

    def wait_log(self, event, timeout=15, **match):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f'no {event} log line within {timeout}s; saw {self.log_lines[-20:]}')
            try:
                value = self.logs.get(timeout=remaining)
            except queue.Empty:
                continue
            if value.get('event') == event and all(value.get(k) == v for k, v in match.items()):
                return value

    def runtime_path(self):
        return self.state / 'runtime.json'

    def runtime(self):
        return json.loads(self.runtime_path().read_text())

    def wait_connected(self, timeout=20):
        return wait_until(lambda: self.runtime().get('connected') is True and self.runtime(), timeout,
                          message='bridge to report connected')

    def stop(self, timeout=15):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            return self.process.wait(timeout)
        return self.process.returncode if self.process else None

    def close(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(5)
        if self.process and self.process.stdout:
            self.process.stdout.close()
        if getattr(self, 'stderr', None):
            self.stderr.close()
