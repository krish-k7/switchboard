"""Base test cases shared by the component suites."""
import hashlib
import json
import threading
import time
import unittest
import uuid

from .server import Client, Machine, ServerProcess, tls_machine
from .util import private_root


def sid():
    return str(uuid.uuid4())


def session(identifier=None, name='alpha', status='idle', **extra):
    return dict(id=identifier or sid(), name=name, status=status, **extra)


def revision_of(roster):
    """The documented peers revision: SHA-256 of the sorted roster without last_seen."""
    peers = sorted(({k: v for k, v in peer.items() if k != 'last_seen'} for peer in roster),
                   key=lambda peer: (peer['machine'], peer['id']))
    return hashlib.sha256(json.dumps(peers, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


class ServerCase(unittest.TestCase):
    """A fresh server per test, Unix ingress by default, with three enrolled machines:
    alpha, beta, and gamma. alpha's registry entry still carries the removed
    administrator flag, which the server ignores."""
    mode = 'unix'
    maxDiff = None

    def setUp(self):
        self.root = private_root(self)
        make = tls_machine if self.mode == 'tls' else Machine
        self.alpha = make('alpha', owner='alice', admin=True)
        self.beta = make('beta', owner='bob')
        self.gamma = make('gamma', owner='ops')
        self.server = ServerProcess(self, self.root / 'server', self.mode, [self.alpha, self.beta, self.gamma])
        self.server.start()
        self.a, self.b, self.c = (Client(self.server, m) for m in (self.alpha, self.beta, self.gamma))

    def background(self, function, *args, **kw):
        """Run function in a thread; return a callable that joins and returns its result."""
        box = {}

        def run():
            try:
                box['value'] = function(*args, **kw)
            except BaseException as error:  # Re-raised in the test thread.
                box['error'] = error
        thread = threading.Thread(target=run, daemon=True)
        thread.start()

        def join(timeout=20):
            thread.join(timeout)
            if thread.is_alive():
                raise AssertionError('background call did not finish')
            if 'error' in box:
                raise box['error']
            return box['value']
        return join

    def message_state(self, message_id):
        for message in self.server.messages():
            if message['id'] == message_id:
                return message
        return None

    def assertError(self, response_or_pair, status, code):
        if isinstance(response_or_pair, tuple):
            actual_status, value = response_or_pair
        else:
            actual_status, value = response_or_pair.status, response_or_pair.json
        self.assertEqual((actual_status, value), (status, {'ok': False, 'error': code}))

    def settle(self, seconds=0.3):
        time.sleep(seconds)
