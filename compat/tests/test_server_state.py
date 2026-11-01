"""Server persistence: restart semantics, startup refusals, and on-disk compatibility.

The database tests seed and read the SQLite message store directly. That file is
the server's persistent state, so a replacement server must open an existing
database and leave one the current server can open: these tests pin that format.
"""
import json
import os
import sqlite3
import subprocess
import time
import uuid

from sbcompat.cases import ServerCase, session, sid
from sbcompat.impl import command
from sbcompat.server import Machine, ServerProcess
from sbcompat.util import child_env, is_time, private_root, write_private

SCHEMA = [(0, 'id', 'TEXT', 0, None, 1), (1, 'source', 'TEXT', 0, None, 0), (2, 'target', 'TEXT', 0, None, 0),
          (3, 'payload', 'TEXT', 0, None, 0), (4, 'state', 'TEXT', 0, None, 0), (5, 'created', 'REAL', 0, None, 0),
          (6, 'updated', 'REAL', 0, None, 0), (7, 'detail', 'TEXT', 0, None, 0)]


class Restart(ServerCase):
    def test_restart_marks_in_flight_messages_unknown_and_tells_senders(self):
        a1, b1 = sid(), sid()
        self.a.register([session(a1)])
        self.b.register([session(b1)], ['ack'])
        ids = {name: str(uuid.uuid4()) for name in ('routed', 'held', 'received', 'handed', 'refused')}
        for message in ids.values():
            self.a.send(message, 'alpha/' + a1, 'beta/' + b1, text=message)
        events = self.b.poll(acks=[], revision=False)['events']
        ack = next(e['ack_id'] for e in events if e['id'] == ids['received'])
        self.b.poll(acks=[ack], revision=False)
        self.b.status(ids['held'], 'held', 'native receipt: held')
        self.b.status(ids['handed'], 'handed_to_claude', 'native transport written')
        self.b.status(ids['refused'], 'refused')
        old_registration = self.b.registration
        self.server.stop()
        self.server.start()
        states = {m['id']: (m['state'], m.get('detail')) for m in self.server.messages()}
        self.assertEqual(states, {ids['routed']: ('unknown', None), ids['held']: ('unknown', 'native receipt: held'),
                                  ids['received']: ('unknown', None), ids['handed']: ('handed_to_claude', 'native transport written'),
                                  ids['refused']: ('refused', None)})
        self.assertNotEqual(self.b.register([session(b1)], ['ack'])['registration'], old_registration)
        self.assertEqual(self.b.poll(acks=[], revision=False)['events'], [])
        # No replay, but each sender's machine is told on its return, once.
        self.a.register([session(a1)])
        self.a.register([session(a1)])
        detail = 'the server restarted before delivery was confirmed'
        self.assertEqual(sorted(self.a.poll(revision=False)['events'], key=lambda e: e['id']), sorted(
            [{'type': 'status', 'id': ids[name], 'state': 'unknown', 'detail': detail, 'to': 'alpha/' + a1}
             for name in ('routed', 'held', 'received')], key=lambda e: e['id']))
        self.assertEqual(self.a.poll(revision=False)['events'], [])
        # The store survives: identical resends report the recorded state.
        self.assertEqual(self.a.send(ids['routed'], 'alpha/' + a1, 'beta/' + b1, text=ids['routed'])[1]['state'], 'unknown')

    def test_departure_of_the_recipient_tells_the_sender(self):
        a1, b1, b2 = sid(), sid(), sid()
        self.a.register([session(a1)])
        self.b.register([session(b1), session(b2, 'stays')])
        gone, kept = str(uuid.uuid4()), str(uuid.uuid4())
        self.a.send(gone, 'alpha/' + a1, 'beta/' + b1)
        self.a.send(kept, 'alpha/' + a1, 'beta/' + b2)
        self.b.register([session(b2, 'stays')])
        events = self.a.poll(revision=False)['events']
        self.assertEqual(events, [{'type': 'status', 'id': gone, 'state': 'unknown',
                                   'detail': 'recipient session left before confirming delivery', 'to': 'alpha/' + a1}])
        self.assertEqual(self.message_state(gone)['state'], 'unknown')
        self.assertEqual([e['id'] for e in self.b.poll(revision=False)['events']], [kept])


class Startup(ServerCase):
    def setUp(self):
        self.root = private_root(self)

    def failed_start(self, server):
        server.start(wait=False)
        server.process.wait(15)
        self.assertNotEqual(server.process.returncode, 0)
        self.assertFalse(server.socket_path.exists() if server.socket_path else False)

    def test_refuses_unsafe_or_conflicting_configuration(self):
        alpha = Machine('alpha')
        server = ServerProcess(self, self.root / 'mixed', 'unix', [alpha], extra_config={'port': 1})
        self.failed_start(server)
        server = ServerProcess(self, self.root / 'wide', 'unix', [alpha])
        server.peers.chmod(0o644)
        self.failed_start(server)
        server = ServerProcess(self, self.root / 'dir', 'unix', [alpha])
        server.sockets.chmod(0o755)
        self.failed_start(server)

    def test_never_replaces_an_existing_socket_path(self):
        alpha = Machine('alpha')
        server = ServerProcess(self, self.root / 'taken', 'unix', [alpha])
        stale = server.sockets / 'server-1.sock'
        stale.write_text('not a socket')
        server.start(wait=False)
        server.process.wait(15)
        self.assertNotEqual(server.process.returncode, 0)
        self.assertEqual(stale.read_text(), 'not a socket')

    def test_config_file_must_be_private(self):
        server = ServerProcess(self, self.root / 'open', 'unix', [Machine('alpha')])
        server.starts = 1
        write_private(server.config_path, server.config(), mode=0o644)
        result = subprocess.run(command('server', '--config', server.config_path), capture_output=True,
                                env=child_env(), timeout=15)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(server.socket_path.exists())


class Database(ServerCase):
    def setUp(self):
        self.root = private_root(self)
        self.alpha = Machine('alpha', admin=True)
        self.server = ServerProcess(self, self.root / 'server', 'unix', [self.alpha])

    def seed(self, rows):
        db = sqlite3.connect(self.server.db)
        db.execute('CREATE TABLE messages (id TEXT PRIMARY KEY, source TEXT, target TEXT, payload TEXT, state TEXT, '
                   'created REAL, updated REAL, detail TEXT)')
        db.executemany('INSERT INTO messages VALUES (?,?,?,?,?,?,?,?)', rows)
        db.commit()
        db.close()

    @staticmethod
    def row(state, created, text='x', detail=None):
        message = str(uuid.uuid4())
        body = {'id': message, 'from': 'beta/' + str(uuid.uuid4()), 'to': 'alpha/' + str(uuid.uuid4()),
                'text': text, 'mode': 'prompting'}
        return (message, body['from'], body['to'], json.dumps(body, sort_keys=True, ensure_ascii=True), state,
                created, created, detail)

    def test_existing_database_is_adopted_pruned_and_expired(self):
        now = time.time()
        old = self.row('handed_to_claude', now - 8 * 86400, 'old')
        routed = self.row('routed', now - 60, 'routed')
        held = self.row('held', now - 50, 'held', 'native receipt: held')
        kept = self.row('handed_to_claude', now - 40, 'kept é', 'native transport written')
        self.seed([old, routed, held, kept])
        self.server.start()
        messages = self.server.messages()
        self.assertEqual([m['id'] for m in messages], [kept[0], held[0], routed[0]])
        by_id = {m['id']: m for m in messages}
        self.assertEqual(by_id[kept[0]], {'id': kept[0], 'from': kept[1], 'to': kept[2], 'text': 'kept é',
                                          'state': 'handed_to_claude', 'created_at': kept[5], 'updated_at': kept[6],
                                          'detail': 'native transport written'})
        self.assertEqual((by_id[routed[0]]['state'], by_id[held[0]]['state']), ('unknown', 'unknown'))
        self.assertEqual(by_id[held[0]]['detail'], 'native receipt: held')
        self.assertTrue(is_time(by_id[routed[0]]['updated_at']) and by_id[routed[0]]['updated_at'] > routed[6])
        self.server.stop()
        db = sqlite3.connect(self.server.db)
        self.assertEqual(db.execute('PRAGMA table_info(messages)').fetchall(), SCHEMA)
        self.assertEqual(db.execute('SELECT count(*) FROM messages').fetchone()[0], 3)
        db.close()

    def test_new_database_has_the_current_schema_and_message_rows(self):
        self.server.start()
        from sbcompat.server import Client
        beta = Machine('beta')
        self.server.write_registry({self.alpha.fp: self.alpha.entry(), beta.fp: beta.entry()})
        a, b = Client(self.server, self.alpha), Client(self.server, beta)
        a1, b1 = sid(), sid()
        a.register([session(a1)])
        b.register([session(b1)])
        message = str(uuid.uuid4())
        body = {'id': message, 'from': 'alpha/' + a1, 'to': 'beta/' + b1, 'text': 'héllo', 'mode': 'prompting',
                'hop_chain': 'h'}
        self.assertEqual(a.call('/v1/send', body)[0], 200)
        b.status(message, 'held', 'native receipt: held')
        self.server.stop()
        db = sqlite3.connect(self.server.db)
        self.assertEqual(db.execute('PRAGMA table_info(messages)').fetchall(), SCHEMA)
        indexes = {row[1] for row in db.execute('PRAGMA index_list(messages)').fetchall()}
        self.assertTrue({'messages_created', 'messages_state_created'} <= indexes, indexes)
        row = db.execute('SELECT * FROM messages').fetchone()
        db.close()
        self.assertEqual(row[:5], (message, 'alpha/' + a1, 'beta/' + b1,
                                   json.dumps(body, sort_keys=True, ensure_ascii=True), 'held'))
        self.assertEqual(row[7], 'native receipt: held')

    def test_message_cap_is_enforced(self):
        now = time.time()
        rows = [self.row('handed_to_claude', now - 3600 + index * 0.1, f'n{index}') for index in range(10005)]
        self.seed(rows)
        self.server.start()
        messages = self.server.messages()
        self.assertEqual([m['text'] for m in messages], [f'n{index}' for index in range(10004, 4, -1)])
        self.server.stop()
        db = sqlite3.connect(self.server.db)
        self.assertEqual(db.execute('SELECT count(*) FROM messages').fetchone()[0], 10000)
        self.assertEqual(db.execute('SELECT min(created) FROM messages').fetchone()[0], rows[5][5])
        db.close()
