"""Start the server under test and speak HTTP to it.

Two ingress modes exist today, both covered:
- 'unix': the production Caddy ingress. The server listens on a private Unix
  socket and takes the client identity from X-Switchboard-Client-Fingerprint
  (and the address from X-Switchboard-Client-Address). Tests can invent keys.
- 'tls': direct mutual TLS on 127.0.0.1. Identity comes from the presented
  client certificate, which must be issued by the configured client CA.
"""
import base64
from dataclasses import dataclass, field
import errno
import http.client
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import sqlite3
import ssl
import subprocess
import threading
import time

from . import pki
from .impl import command
from .util import child_env, free_port, private_dir, wait_until, write_private

ADDRESS = '203.0.113.7'
CSP = ("default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; "
       "base-uri 'none'; frame-ancestors 'none'; form-action 'none'")


@dataclass
class Machine:
    name: str
    owner: str = 'tester'
    admin: bool = False
    enabled: bool = True
    fp: str = field(default_factory=lambda: secrets.token_hex(32))
    cert: Path = None
    key: Path = None

    def entry(self):
        value = dict(machine=self.name, owner=self.owner, enabled=self.enabled)
        if self.admin:
            value['admin'] = True
        return value


def invite_fields(token):
    """The fields of an invite token: `sbi1.` and unpadded base64url of a JSON object."""
    if not token.startswith('sbi1.'):
        raise AssertionError(f'not an invite token: {token!r}')
    body = token[len('sbi1.'):]
    return json.loads(base64.urlsafe_b64decode(body + '=' * (-len(body) % 4)))


def tls_machine(name, **kw):
    cert, key, fp = pki.client(name)
    return Machine(name, fp=fp, cert=cert, key=key, **kw)


@dataclass
class Response:
    status: int
    reason: str
    version: int
    headers: list
    body: bytes

    @property
    def json(self):
        return json.loads(self.body)

    def header(self, name):
        values = [value for key, value in self.headers if key.lower() == name.lower()]
        return values[0] if len(values) == 1 else (values or None)

    def header_map(self, exclude=('server', 'date')):
        return {key.lower(): value for key, value in self.headers if key.lower() not in exclude}


def _read_response(sock, method=None):
    response = http.client.HTTPResponse(sock, method=method)
    response.begin()
    body = response.read()
    return Response(response.status, response.reason, response.version, response.getheaders(), body)


class ServerProcess:
    """One server process with its own config, registry, and database."""

    def __init__(self, test, root, mode='unix', machines=(), registry=None, extra_config=None):
        self.test, self.mode = test, mode
        self.root = private_dir(root)
        self.machines = list(machines)
        self.peers = self.root / 'peers.json'
        self.state = private_dir(self.root / 'state')
        self.db = self.state / 'messages.db'
        self.sockets = private_dir(self.root / 'sock', 0o750)
        self.config_path = self.root / 'server.json'
        self.extra_config = extra_config or {}
        self.process = None
        self.port = free_port() if mode == 'tls' else None
        self.socket_path = None
        self.starts = 0
        self.stderr_path = self.root / 'stderr.log'
        self.write_registry(registry if registry is not None else {m.fp: m.entry() for m in self.machines})

    # Registry -----------------------------------------------------------
    def write_registry(self, value):
        write_private(self.peers, value if isinstance(value, (str, bytes)) else json.dumps(value, indent=2) + '\n')

    def registry(self):
        return json.loads(self.peers.read_text())

    # Local machine commands -----------------------------------------------
    def local(self, *arguments):
        """`server COMMAND ... --config CONFIG --json` (invite, invites, machines, revoke),
        run as the user that owns the server's files, as an operator on the server would;
        returns (exit status, parsed JSON, or None when nothing was printed: usage errors)."""
        result = subprocess.run(command('server', *arguments, '--config', self.config_path, '--json'),
                                stdin=subprocess.DEVNULL, capture_output=True, env=child_env(), timeout=30)
        return result.returncode, (json.loads(result.stdout) if result.stdout.strip() else None)

    def invite(self, *arguments, url='https://switchboard.test'):
        """A new invite: (its secret, the command's JSON)."""
        code, value = self.local('invite', '--url', url, *arguments)
        if code != 0:
            raise AssertionError(f'server invite failed: {value}')
        return invite_fields(value['invite'])['secret'], value

    @property
    def invites_file(self):
        return self.state / 'invites.json'

    # Message store --------------------------------------------------------
    def messages(self):
        """Every stored message, newest first, read from the database (opened read-only).

        The database is part of the server contract (see test_server_state), so the suite
        observes delivery states there. Fields: id, from, to, text, state, created_at,
        updated_at, and detail when one is recorded."""
        db = sqlite3.connect(f'file:{self.db}?mode=ro', uri=True, timeout=10)
        try:
            rows = db.execute('SELECT id,source,target,payload,state,created,updated,detail FROM messages '
                              'ORDER BY created DESC').fetchall()
        finally:
            db.close()
        result = []
        for identifier, source, target, payload, state, created, updated, detail in rows:
            message = {'id': identifier, 'from': source, 'to': target, 'text': json.loads(payload)['text'],
                       'state': state, 'created_at': created, 'updated_at': updated}
            if detail:
                message['detail'] = detail
            result.append(message)
        return result

    # Lifecycle ----------------------------------------------------------
    def config(self):
        if self.mode == 'unix':
            self.socket_path = self.sockets / f'server-{self.starts}.sock'
            value = dict(unix_socket=str(self.socket_path), db=str(self.db), peers_file=str(self.peers))
        else:
            cert, key = pki.server()
            value = dict(listen='127.0.0.1', port=self.port, cert=str(cert), key=str(key),
                         client_ca=str(pki.ca()), db=str(self.db), peers_file=str(self.peers))
        value.update(self.extra_config)
        return value

    def start(self, wait=True):
        self.starts += 1
        write_private(self.config_path, self.config())
        self.stderr = open(self.stderr_path, 'ab')
        self.process = subprocess.Popen(command('server', '--config', self.config_path), stdin=subprocess.DEVNULL,
                                        stdout=subprocess.DEVNULL, stderr=self.stderr, env=child_env(),
                                        start_new_session=True)
        self.test.addCleanup(self._cleanup, self.process, self.stderr)
        if wait:
            wait_until(self.ready, 15, message='server to listen')
        return self

    def ready(self):
        if self.process.poll() is not None:
            raise AssertionError('server exited with %s: %s' % (self.process.returncode, self.stderr_path.read_text()[-2000:]))
        try:
            if self.mode == 'unix':
                with socket.socket(socket.AF_UNIX) as probe:
                    probe.connect(str(self.socket_path))
            else:
                with socket.create_connection(('127.0.0.1', self.port), timeout=1):
                    pass
            return True
        except OSError:
            return False

    def stop(self, sig=signal.SIGINT, timeout=8):
        """Graceful stop (SIGINT, as an operator or supervisor would send)."""
        if self.process and self.process.poll() is None:
            self.process.send_signal(sig)
            try:
                self.process.wait(timeout)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(5)

    def kill(self):
        if self.process and self.process.poll() is None:
            self.process.kill()
            self.process.wait(5)

    def pause(self):
        self.process.send_signal(signal.SIGSTOP)

    def resume(self):
        self.process.send_signal(signal.SIGCONT)

    @staticmethod
    def _cleanup(process, stderr):
        if process.poll() is None:
            try:
                process.send_signal(signal.SIGCONT)
                process.kill()
            except ProcessLookupError:
                pass
            process.wait(5)
        stderr.close()

    # HTTP ---------------------------------------------------------------
    def request(self, method, path, body=None, *, machine=None, fp=None, address=ADDRESS, raw_body=None,
                content_type='application/json', headers=None, identity_headers=None, timeout=20):
        """One HTTP request. body is JSON-encoded unless raw_body (bytes) is given."""
        payload = raw_body if raw_body is not None else (None if body is None else json.dumps(body).encode())
        extra = list(headers or [])
        if payload is not None and content_type is not None:
            extra.append(('Content-Type', content_type))
        if payload is not None and not any(k.lower() in ('content-length', 'transfer-encoding') for k, _ in extra):
            extra.append(('Content-Length', str(len(payload))))
        if self.mode == 'unix':
            if identity_headers is None:
                fingerprint = fp if fp is not None else machine.fp
                identity_headers = [('X-Switchboard-Client-Fingerprint', fingerprint)]
                if address is not None:
                    identity_headers.append(('X-Switchboard-Client-Address', address))
            head = f'{method} {path} HTTP/1.1\r\nHost: localhost\r\n'
            head += ''.join(f'{k}: {v}\r\n' for k, v in [*identity_headers, *extra])
            return self.raw(head.encode() + b'\r\n' + (payload or b''), timeout=timeout, method=method)
        connection = self.https(machine, timeout)
        try:
            connection.putrequest(method, path, skip_accept_encoding=True)
            for key, value in [*(identity_headers or []), *extra]:
                connection.putheader(key, value)
            connection.endheaders(payload)
            response = connection.getresponse()
            return Response(response.status, response.reason, response.version, response.getheaders(), response.read())
        finally:
            connection.close()

    def raw(self, data, timeout=20, shutdown=True, method=None, parse=True):
        """Write raw bytes to the Unix ingress and parse one HTTP response (the
        reply to `method`, so a HEAD reply has no body), or with parse=False
        return every byte the server sent."""
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(timeout)
            client.connect(str(self.socket_path))
            client.sendall(data)
            if shutdown:
                try:
                    client.shutdown(socket.SHUT_WR)
                except OSError as error:
                    # The server may answer and close before this half-close when the
                    # request needs no body; macOS then fails the shutdown with ENOTCONN,
                    # but the reply is already buffered for reading.
                    if error.errno != errno.ENOTCONN:
                        raise
            if not parse:
                chunks = []
                while chunk := client.recv(65536):
                    chunks.append(chunk)
                return b''.join(chunks)
            return _read_response(client, method)

    def tls_context(self, machine):
        context = ssl.create_default_context(cafile=str(pki.ca()))
        if machine is not None:
            context.load_cert_chain(str(machine.cert), str(machine.key))
        return context

    def https(self, machine, timeout=20):
        return http.client.HTTPSConnection('localhost', self.port, context=self.tls_context(machine), timeout=timeout)

    @property
    def url(self):
        return f'https://localhost:{self.port}'

    def call(self, machine, path, body=None, **kw):
        """POST when body is given, else GET; return (status, parsed JSON)."""
        response = self.request('POST' if body is not None else 'GET', path, body, machine=machine, **kw)
        return response.status, response.json


class Client:
    """A test-driven machine speaking the protocol directly (no bridge)."""

    def __init__(self, server, machine):
        self.server, self.machine = server, machine
        self.registration = None
        self.revision = None
        self.capabilities = None

    def call(self, path, body=None, **kw):
        return self.server.call(self.machine, path, body, **kw)

    def ok(self, path, body=None, **kw):
        status, value = self.call(path, body, **kw)
        if status != 200:
            raise AssertionError(f'{self.machine.name} {path} -> {status} {value}')
        return value

    def register(self, sessions, capabilities=None):
        body = {'sessions': sessions}
        if capabilities is not None:
            body['capabilities'] = capabilities
        value = self.ok('/v1/register', body)
        self.registration = value['registration']
        self.capabilities = capabilities
        return value

    def poll(self, acks=None, legacy=False, revision=True, **kw):
        if legacy:
            body = {}
        else:
            body = {'registration': self.registration, 'peers_revision': self.revision if revision else None}
            if acks is not None:
                body['acks'] = acks
        value = self.ok('/v1/poll', body, **kw)
        self.revision = value.get('peers_revision')
        return value

    def send(self, message_id, source, target, text='hello', mode='prompting', **extra):
        return self.call('/v1/send', {'id': message_id, 'from': source, 'to': target, 'text': text, 'mode': mode, **extra})

    def status(self, message_id, state, detail=None):
        body = {'id': message_id, 'state': state}
        if detail is not None:
            body['detail'] = detail
        return self.call('/v1/status', body)


class Poller:
    """Background long-poller for a Client: collects events, acknowledges them on
    the next poll when ack is on, and optionally answers pings."""

    def __init__(self, client, ack=True, pong=None, interval=0.0):
        self.client, self.ack, self.pong, self.interval = client, ack, pong, interval
        self.events = []
        self.condition = threading.Condition()
        self.stopped = False
        self.error = None
        self.thread = threading.Thread(target=self.run, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def run(self):
        acks = []
        while not self.stopped:
            try:
                value = self.client.poll(acks=acks if self.ack else None, timeout=15)
            except Exception as error:  # Reported to the test through .error.
                self.error = error
                time.sleep(0.2)
                continue
            acks = []
            if value.get('registration') != self.client.registration:
                time.sleep(0.5)  # Lost registration (e.g. server restart): wait for the test to register again.
            elif self.interval:
                time.sleep(self.interval)
            for event in value['events']:
                if self.ack and 'ack_id' in event:
                    acks.append(event['ack_id'])
                if event.get('type') == 'ping' and self.pong is not None:
                    live = self.pong(event) if callable(self.pong) else self.pong
                    if live is not None:
                        self.client.call('/v1/pong', {'id': event['id'], 'live': live})
            with self.condition:
                self.events.extend(value['events'])
                self.condition.notify_all()

    def wait(self, predicate, timeout=15):
        deadline = time.monotonic() + timeout
        with self.condition:
            while True:
                for event in self.events:
                    if predicate(event):
                        return event
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f'no matching event within {timeout}s; saw {self.events}')
                self.condition.wait(remaining)

    def stop(self):
        self.stopped = True
