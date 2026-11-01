"""Old clients (no capabilities, no acknowledgments) see byte-identical responses.

Expected bodies are rebuilt with the exact key order, separators, and ASCII
escaping of today's server; only values that are nondeterministic (tokens,
revisions, times) are taken from the response, after validating them.
"""
import json
import uuid

from sbcompat.cases import ServerCase, revision_of, session, sid
from sbcompat.util import is_time, is_uuid


def wire(value):
    return json.dumps(value).encode()


class OldClient(ServerCase):
    def setUp(self):
        super().setUp()
        self.a1, self.b1 = sid(), sid()

    def raw(self, client, path, body=None):
        response = self.server.request('POST' if body is not None else 'GET', path, body, machine=client.machine)
        self.assertEqual(response.status, 200, response.body)
        return response

    def peer(self, sess, machine, owner, last_seen):
        value = dict(sess, machine=machine, owner=owner)
        value['last_seen'] = last_seen
        return value

    def test_old_bridge_round_trip_is_byte_identical(self):
        a_session = session(self.a1, 'alpha chat é', 'busy')
        b_session = session(self.b1, 'beta-chat', 'idle')
        response = self.raw(self.a, '/v1/register', {'sessions': [a_session]})
        registration_a = response.json['registration']
        self.assertTrue(is_uuid(registration_a))
        self.assertEqual(response.body, wire({'ok': True, 'machine': 'alpha', 'registration': registration_a,
                                              'session_metadata_fields': ['project', 'repository', 'branch'],
                                              'capabilities': ['ack', 'ping']}))
        response = self.raw(self.b, '/v1/register', {'sessions': [b_session]})
        registration_b = response.json['registration']

        message = str(uuid.uuid4())
        send = {'id': message, 'from': 'alpha/' + self.a1, 'to': 'beta/' + self.b1,
                'text': 'héllo <λ> "quoted" / \\  ', 'mode': 'prompting', 'hop_chain': 'a' * 24}
        response = self.raw(self.a, '/v1/send', send)
        self.assertEqual(response.body, b'{"ok": true, "id": "' + message.encode() + b'", "state": "routed"}')

        response = self.raw(self.b, '/v1/poll', {})
        value = response.json
        last_seen = {p['machine']: p['last_seen'] for p in value['peers']}
        self.assertTrue(all(is_time(v) for v in last_seen.values()))
        roster = sorted([self.peer(a_session, 'alpha', 'alice', last_seen['alpha']),
                         self.peer(b_session, 'beta', 'bob', last_seen['beta'])], key=lambda p: (p['machine'], p['id']))
        expected = {'ok': True, 'events': [dict(send, type='message', sender_name='alpha chat é')], 'machine': 'beta',
                    'registration': registration_b, 'peers_revision': revision_of(roster), 'peers': roster}
        self.assertEqual(response.body, wire(expected))
        self.assertIn(b'h\\u00e9llo <\\u03bb> \\"quoted\\" / \\\\ \\u2028', response.body)

        response = self.raw(self.b, '/v1/status', {'id': message, 'state': 'handed_to_claude', 'detail': 'native transport written'})
        self.assertEqual(response.body, b'{"ok": true}')
        response = self.raw(self.a, '/v1/poll', {})
        value = response.json
        roster = [dict(p, last_seen=q['last_seen']) for p, q in zip(roster, value['peers'])]
        self.assertEqual(response.body, wire({
            'ok': True, 'events': [{'type': 'status', 'id': message, 'state': 'handed_to_claude', 'to': 'alpha/' + self.a1,
                                    'detail': 'native transport written'}],
            'machine': 'alpha', 'registration': registration_a, 'peers_revision': revision_of(roster), 'peers': roster}))

    def test_old_bridge_gets_metadata_free_roster_and_no_ping(self):
        self.raw(self.b, '/v1/register', {'sessions': [session(self.b1, 'beta-chat')]})
        response = self.server.request('POST', '/v1/ping', {'to': 'beta/' + self.b1}, machine=self.alpha)
        self.assertEqual((response.status, response.body), (409, b'{"ok": false, "error": "ping_unsupported"}'))
        # Status without detail omits the key; consume-on-poll needs no acknowledgment.
        self.raw(self.a, '/v1/register', {'sessions': [session(self.a1)]})
        message = str(uuid.uuid4())
        self.raw(self.a, '/v1/send', {'id': message, 'from': 'alpha/' + self.a1, 'to': 'beta/' + self.b1,
                                      'text': 'x', 'mode': 'bypass'})
        first = self.raw(self.b, '/v1/poll', {}).json
        self.assertEqual([e['id'] for e in first['events']], [message])
        self.assertNotIn('ack_id', first['events'][0])
        self.raw(self.b, '/v1/status', {'id': message, 'state': 'held'})
        events = self.raw(self.a, '/v1/poll', {}).json['events']
        self.assertEqual(wire(events), wire([{'type': 'status', 'id': message, 'state': 'held', 'to': 'alpha/' + self.a1}]))
        # A second legacy poll by the receiver finds nothing new; its queue was consumed.
        waiting = self.background(self.raw, self.b, '/v1/poll', {})
        self.settle(0.3)
        self.raw(self.c, '/v1/register', {'sessions': [session()]})
        self.assertEqual(waiting().json['events'], [])

    def test_health_and_error_bodies_are_byte_identical(self):
        self.assertEqual(self.raw(self.c, '/health').body, b'{"ok": true, "machine": "gamma", "owner": "ops"}')
        response = self.server.request('POST', '/v1/send', {'id': 'x'}, machine=self.gamma)
        self.assertEqual((response.status, response.body), (400, b'{"ok": false, "error": "invalid_uuid"}'))
        response = self.server.request('GET', '/health', fp='0' * 64)
        self.assertEqual((response.status, response.body), (403, b'{"ok": false, "error": "not_enrolled"}'))
