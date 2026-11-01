"""Fake native Claude Code sessions: real processes publishing native records and sockets."""
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import uuid

from .impl import COMPAT
from .util import child_env, native_address, private_dir

FIXTURE = COMPAT / 'fixtures' / 'fake_claude_session.py'
ATTRS = ('from', 'from-session', 'hop-chain', 'from-name', 'from-mode', 'from-plugin')


def envelope(attrs, body):
    """Native cross-session envelope, attributes in Claude's canonical order."""
    values = ''.join(f' {key}="{attrs[key]}"' for key in ATTRS if key in attrs)
    return ('<cross-session-message' + values + '>\n'
            + body.replace('</cross-session-message', '<\\/cross-session-message') + '\n</cross-session-message>')


class NativeEnv:
    """One OS account's native Claude namespace: config dir, registry, socket dir."""

    def __init__(self, test, root):
        self.test = test
        self.root = private_dir(root)
        self.config_dir = private_dir(self.root / 'claude')
        self.sessions = private_dir(self.config_dir / 'sessions')
        self.sockets = private_dir(self.root / 'socks')

    def session(self, sid=None, name='alpha', status='idle', **fields):
        session = FakeSession(self, sid or str(uuid.uuid4()), dict(name=name, status=status, **fields)
                              if status is not None else dict(name=name, **fields))
        self.test.addCleanup(session.close)
        return session

    def records(self):
        """Every registry record currently published, by PID."""
        result = {}
        for path in self.sessions.glob('*.json'):
            if path.stem.isdigit():
                try:
                    result[int(path.stem)] = json.loads(path.read_text())
                except (OSError, ValueError):
                    pass
        return result

    def proxies(self):
        return {pid: record for pid, record in self.records().items() if record.get('peerBridgeProxy')}

    def key_for(self, record):
        digest = hashlib.sha256(record['messagingSocketPath'].encode()).hexdigest()
        return self.sessions / f"{record['pid']}.{digest}.key"

    def token_for(self, record):
        return json.loads(self.key_for(record).read_text())['peerToken']


class FakeSession:
    def __init__(self, env, sid, fields):
        self.env, self.sid = env, sid
        self.process = subprocess.Popen(
            [sys.executable, '-B', str(FIXTURE), '--config-dir', str(env.config_dir), '--socket-dir', str(env.sockets),
             '--session-id', sid, '--fields', json.dumps(fields)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=child_env())
        self.events = queue.Queue()
        self.frames = queue.Queue()
        self.sent = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        ready = self.events.get(timeout=10)
        assert ready['event'] == 'ready', ready
        self.pid, self.token, self.socket = ready['pid'], ready['token'], ready['socket']
        self.address, self.record = ready['address'], ready['record']
        self.name = fields.get('name')

    def _read(self):
        for line in self.process.stdout:
            event = json.loads(line)
            if event['event'] in ('frame', 'probe'):
                self.frames.put(event)
            elif event['event'] in ('sent', 'updated'):
                self.sent.put(event)
            else:
                self.events.put(event)

    def command(self, value):
        self.process.stdin.write(json.dumps(value) + '\n')
        self.process.stdin.flush()

    # Writing to another native socket, as this session -------------------
    def send_raw(self, socket_path, data, timeout=10):
        ref = uuid.uuid4().hex
        self.command(dict(op='send', ref=ref, socket=str(socket_path), data=data))
        event = self.sent.get(timeout=timeout)
        assert event.get('ref') == ref, event
        return event

    def send_frame(self, target_record, frame, token=None, env=None):
        token = token if token is not None else (env or self.env).token_for(target_record)
        data = json.dumps(dict(type='auth', token=token)) + '\n' + json.dumps(frame) + '\n'
        return self.send_raw(target_record['messagingSocketPath'], data)

    def user_frame(self, target_record, text='hello from alpha', mode='prompting', msg_id=None, hop_chain=None,
                   name=None, **overrides):
        """A native SendMessage frame from this session to a peer (e.g. a proxy)."""
        attrs = {'from': self.address, 'from-name': name or self.name or 'alpha', 'from-mode': mode}
        if hop_chain:
            attrs['hop-chain'] = hop_chain
        frame = dict(msgV=1, msg_id=msg_id or str(uuid.uuid4()), type='user', session_id=target_record['sessionId'],
                     message=dict(role='user', content=envelope(attrs, text)), **{'from': self.address})
        frame.update(overrides)
        return frame

    def receipt_frame(self, orig_msg_id, status, **extra):
        return dict(msgV=1, type='control', action='peer_message_status', status=status,
                    orig_msg_id=orig_msg_id, **{'from': self.address}, **extra)

    # Frames written to this session --------------------------------------
    def wait_frame(self, timeout=10):
        try:
            return self.frames.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError(f'session {self.sid} received no frame within {timeout}s') from None

    def no_frame(self, timeout=1.5):
        try:
            event = self.frames.get(timeout=timeout)
        except queue.Empty:
            return True
        raise AssertionError(f'unexpected frame for session {self.sid}: {event}')

    def update(self, remove=(), **fields):
        self.command(dict(op='update', fields=fields, remove=list(remove)))
        self.sent.get(timeout=10)

    def close(self):
        if self.process.poll() is None:
            try:
                self.command(dict(op='exit'))
                self.process.wait(5)
            except (OSError, subprocess.TimeoutExpired):
                self.process.kill()
                self.process.wait(5)
        for stream in (self.process.stdin, self.process.stdout):
            try:
                stream.close()
            except OSError:
                pass
