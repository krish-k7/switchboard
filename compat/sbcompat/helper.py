"""Drive one native helper process over its line-JSON IPC."""
import json
from pathlib import Path
import queue
import subprocess
import threading
import uuid

from .impl import command
from .util import child_env, private_dir, write_private


class HelperProcess:
    def __init__(self, test, env, sessions, route=None, name='switchboard/remote/beta-12345678', status=None,
                 state_dir=None, enabled=True):
        self.test, self.env = test, env
        self.route = route or 'remote-box/' + str(uuid.uuid4())
        self.name = name
        self.state_dir = private_dir(state_dir or env.root / 'state')
        self.config_path = env.root / f'helper-{uuid.uuid4().hex[:8]}.json'
        write_private(self.config_path, dict(enabled=enabled, config_dir=str(env.config_dir),
                                             state_dir=str(self.state_dir), sessions=list(sessions)))
        arguments = ['--config', self.config_path, '--route', self.route, '--name', name, '--socket-dir', env.sockets]
        if status is not None:
            arguments += ['--status', status]
        self.stderr_path = env.root / f'helper-{uuid.uuid4().hex[:8]}.stderr'
        self.stderr = open(self.stderr_path, 'wb')
        self.process = subprocess.Popen(command('helper', *arguments), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.stderr, env=child_env())
        self.events = queue.Queue()
        self.output = []
        threading.Thread(target=self._read, daemon=True).start()
        test.addCleanup(self.close)
        self.ready = None
        self.record = None
        self.token = None

    def _read(self):
        for line in self.process.stdout:
            self.output.append(line)
            self.events.put(json.loads(line))
        self.events.put(None)

    def wait_ready(self, timeout=10):
        self.ready = self.event(timeout)
        self.test.assertEqual(self.ready['event'], 'ready', self.ready)
        self.record = self.proxy_record()
        self.token = self.env.token_for(self.record)
        return self.ready

    def proxy_record(self):
        return json.loads((self.env.sessions / f"{self.ready['pid']}.json").read_text())

    def event(self, timeout=10):
        try:
            value = self.events.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError(f'helper emitted nothing within {timeout}s') from None
        if value is None:
            raise AssertionError('helper exited: ' + self.stderr_path.read_text()[-1500:])
        return value

    def no_event(self, timeout=1.0):
        try:
            value = self.events.get(timeout=timeout)
        except queue.Empty:
            return True
        raise AssertionError(f'unexpected helper event: {value}')

    def ipc(self, value):
        self.ipc_raw(json.dumps(value).encode() + b'\n')

    def ipc_raw(self, data):
        self.process.stdin.write(data)
        self.process.stdin.flush()

    def deliver(self, target, text='hello from beta', mode='prompting', message_id=None, sender_name='beta', **extra):
        message_id = message_id or str(uuid.uuid4())
        self.ipc(dict(op='deliver', id=message_id, target_session=target, text=text, mode=mode,
                      sender_name=sender_name, **extra))
        return message_id

    def terminate(self, timeout=10):
        self.process.terminate()
        return self.process.wait(timeout)

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(5)
        for stream in (self.process.stdin, self.process.stdout):
            try:
                stream.close()
            except OSError:
                pass
        self.stderr.close()
