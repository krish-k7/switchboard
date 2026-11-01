"""Server HTTP API: presence, routing, delivery state, acknowledgments, ping."""
import time
import uuid

from sbcompat.cases import ServerCase, revision_of, session, sid
from sbcompat.server import CSP, Client, Machine
from sbcompat.util import is_hex32, is_time, is_uuid, scrub


class Health(ServerCase):
    def test_health_reports_identity_with_fixed_security_headers(self):
        for client, owner in ((self.a, 'alice'), (self.b, 'bob'), (self.c, 'ops')):
            response = self.server.request('GET', '/health', machine=client.machine)
            self.assertEqual(response.status, 200)
            self.assertEqual(response.version, 10)  # HTTP/1.0: every response closes its connection.
            self.assertEqual(response.json, {'ok': True, 'machine': client.machine.name, 'owner': owner})
            self.assertEqual(response.header_map(), {
                'content-type': 'application/json', 'content-length': str(len(response.body)),
                'cache-control': 'no-store', 'x-content-type-options': 'nosniff', 'content-security-policy': CSP})


class Registration(ServerCase):
    def test_register_response_capabilities_and_stable_token(self):
        s1 = sid()
        value = self.a.register([session(s1, 'alpha-chat', 'busy', project='solver')], ['ack', 'ping', 'future-thing'])
        self.assertEqual(scrub(self, value, registration=is_uuid), {
            'ok': True, 'machine': 'alpha', 'registration': '<registration>',
            'session_metadata_fields': ['project', 'repository', 'branch'], 'capabilities': ['ack', 'ping']})
        token = value['registration']
        # Stable across identical and changed rosters, and across capability changes.
        self.assertEqual(self.a.register([session(s1, 'alpha-chat', 'busy', project='solver')], ['ack'])['registration'], token)
        self.assertEqual(self.a.register([session(s1, 'renamed', 'idle')])['registration'], token)
        self.assertEqual(self.a.register([])['registration'], token)
        # Capabilities are echoed as the server's full list even when none are offered.
        self.assertEqual(self.a.register([])['capabilities'], ['ack', 'ping'])

    def test_peers_roster_shape_order_owner_and_revision(self):
        a1, b1, b2 = sid(), sid(), sid()
        self.a.register([session(a1, 'alpha-chat', 'busy', project='solver', repository='example', branch='main')])
        self.b.register([session(b2, 'second', 'waiting'), session(b1, 'first', 'shell')])
        status, value = self.c.call('/v1/peers')
        self.assertEqual(status, 200)
        self.assertEqual(set(value), {'ok', 'machine', 'peers', 'peers_revision'})
        self.assertEqual((value['ok'], value['machine']), (True, 'gamma'))
        expected = sorted([
            dict(id=a1, name='alpha-chat', status='busy', project='solver', repository='example', branch='main',
                 machine='alpha', owner='alice'),
            dict(id=b2, name='second', status='waiting', machine='beta', owner='bob'),
            dict(id=b1, name='first', status='shell', machine='beta', owner='bob')],
            key=lambda peer: (peer['machine'], peer['id']))
        self.assertEqual(scrub(self, value['peers'], last_seen=is_time),
                         [dict(peer, last_seen='<last_seen>') for peer in expected])
        self.assertEqual(value['peers_revision'], revision_of(expected))
        # Heartbeats change last_seen but never the revision.
        self.settle(1.1)
        self.a.poll(revision=False)
        again = self.c.call('/v1/peers')[1]
        self.assertEqual(again['peers_revision'], value['peers_revision'])
        # A roster change does.
        self.b.register([session(b1, 'first', 'idle')])
        changed = self.c.call('/v1/peers')[1]
        self.assertNotEqual(changed['peers_revision'], value['peers_revision'])
        self.assertEqual(changed['peers_revision'], revision_of(changed['peers']))

    def test_register_replaces_the_machine_roster(self):
        a1, a2 = sid(), sid()
        self.a.register([session(a1), session(a2, 'two')])
        self.a.register([session(a2, 'two')])
        peers = self.c.call('/v1/peers')[1]['peers']
        self.assertEqual([p['id'] for p in peers], [a2])
        self.a.register([])
        self.assertEqual(self.c.call('/v1/peers')[1]['peers'], [])

    def test_register_validation_codes(self):
        good = session()
        cases = [
            ({'sessions': 'x'}, 'invalid_sessions'),
            ({}, 'invalid_sessions'),
            ({'sessions': [session() for _ in range(33)]}, 'invalid_sessions'),
            ({'sessions': [], 'capabilities': 'ack'}, 'invalid_capabilities'),
            ({'sessions': [], 'capabilities': ['x'] * 17}, 'invalid_capabilities'),
            ({'sessions': [], 'capabilities': [1]}, 'invalid_capabilities'),
            ({'sessions': [], 'capabilities': ['a' * 33]}, 'invalid_capabilities'),
            ({'sessions': ['x']}, 'invalid_session'),
            ({'sessions': [dict(good, cwd='/home')]}, 'invalid_session'),
            ({'sessions': [dict(good, id='not-a-uuid')]}, 'invalid_uuid'),
            ({'sessions': [dict(good, id=good['id'].upper())]}, 'invalid_uuid'),
            ({'sessions': [dict(good, id=7)]}, 'invalid_uuid'),
            ({'sessions': [good, dict(good, name='dup')]}, 'invalid_session'),
            ({'sessions': [dict(good, status='asleep')]}, 'invalid_session'),
            ({'sessions': [{k: v for k, v in good.items() if k != 'status'}]}, 'invalid_session'),
            ({'sessions': [dict(good, name='')]}, 'invalid_label'),
            ({'sessions': [dict(good, name='x' * 65)]}, 'invalid_label'),
            ({'sessions': [dict(good, name='two\nlines')]}, 'invalid_label'),
            ({'sessions': [dict(good, name='del\x7f')]}, 'invalid_label'),
            ({'sessions': [dict(good, project='p' * 65)]}, 'invalid_label'),
            ({'sessions': [dict(good, repository='r' * 257)]}, 'invalid_label'),
            ({'sessions': [dict(good, branch='')]}, 'invalid_label'),
            ({'sessions': [dict(good, branch=5)]}, 'invalid_label'),
        ]
        for body, code in cases:
            with self.subTest(code=code, body=str(body)[:80]):
                self.assertError(self.a.call('/v1/register', body), 400, code)
        self.assertError(self.server.call(self.alpha, '/v1/register', ['not', 'an', 'object']), 400, 'invalid_body')
        # Boundary values are accepted, and failed requests changed nothing.
        self.assertEqual(self.c.call('/v1/peers')[1]['peers'], [])
        ok = self.a.register([dict(session(name='x' * 64), project='p' * 64, repository='r' * 256, branch='b' * 256)]
                             + [session() for _ in range(31)], ['c'] * 16)
        self.assertTrue(ok['ok'])


class Polling(ServerCase):
    def test_poll_shapes_wakeups_and_revision_omission(self):
        a1, b1 = sid(), sid()
        self.a.register([session(a1)], ['ack', 'ping'])
        first = self.a.poll(revision=False)
        self.assertEqual(set(first), {'ok', 'events', 'machine', 'registration', 'peers_revision', 'peers'})
        self.assertEqual((first['events'], first['machine'], first['registration']), ([], 'alpha', self.a.registration))
        # The caller's own sessions are included; the bridge filters them.
        self.assertEqual([p['id'] for p in first['peers']], [a1])
        # A matching revision waits, and a roster change elsewhere wakes it with the new roster.
        started = time.monotonic()
        waiting = self.background(self.a.poll)
        self.settle(0.6)
        self.b.register([session(b1, 'beta-chat')])
        woke = waiting()
        self.assertTrue(0.5 < time.monotonic() - started < 5, time.monotonic() - started)
        self.assertEqual(sorted(p['id'] for p in woke['peers']), sorted([a1, b1]))
        # A queued message wakes it too; the roster is unchanged, so peers is omitted.
        waiting = self.background(self.a.poll)
        self.settle(0.4)
        message = str(uuid.uuid4())
        self.assertEqual(self.b.send(message, 'beta/' + b1, 'alpha/' + a1)[0], 200)
        woke = waiting()
        self.assertNotIn('peers', woke)
        self.assertEqual([e['id'] for e in woke['events']], [message])

    def test_unchanged_reregistration_does_not_end_a_waiting_poll(self):
        a1 = sid()
        self.a.register([session(a1)])
        self.a.poll(revision=False)
        started = time.monotonic()
        waiting = self.background(self.a.poll)
        self.settle(0.5)
        self.a.register([session(a1)])
        self.settle(1.0)
        self.b.register([session()])  # Ends the wait so the test stays fast.
        waiting()
        self.assertGreater(time.monotonic() - started, 1.4)

    def test_mismatched_or_missing_registration_returns_at_once_without_consuming(self):
        a1, b1 = sid(), sid()
        self.a.register([session(a1)])
        self.b.register([session(b1)])
        message = str(uuid.uuid4())
        self.b.send(message, 'beta/' + b1, 'alpha/' + a1)
        for body in ({'registration': str(uuid.uuid4()), 'peers_revision': None},
                     {'registration': None, 'peers_revision': None},
                     {'peers_revision': 'whatever'}):
            with self.subTest(body=body):
                started = time.monotonic()
                status, value = self.a.call('/v1/poll', body)
                self.assertEqual(status, 200)
                self.assertLess(time.monotonic() - started, 2)
                self.assertEqual(value['events'], [])
                self.assertEqual(value['registration'], self.a.registration)
        self.assertEqual([e['id'] for e in self.a.poll(revision=False)['events']], [message])

    def test_unregistered_machine_modern_poll(self):
        status, value = self.c.call('/v1/poll', {'registration': None, 'peers_revision': None})
        self.assertEqual(status, 200)
        self.assertEqual(scrub(self, value, peers_revision=lambda v: isinstance(v, str)),
                         {'ok': True, 'events': [], 'machine': 'gamma', 'registration': None,
                          'peers_revision': '<peers_revision>', 'peers': []})

    def test_legacy_poll_creates_presence_but_no_roster(self):
        waiting = self.background(self.c.poll, legacy=True)
        self.settle(0.4)
        b1 = sid()
        self.b.register([session(b1)])  # A roster change ends the legacy wait.
        value = waiting()
        self.assertEqual(value['registration'], None)
        self.assertEqual(value['events'], [])
        self.assertEqual([p['id'] for p in value['peers']], [b1])
        self.assertEqual([p['machine'] for p in self.c.call('/v1/peers')[1]['peers']], ['beta'])

    def test_poll_validation_codes(self):
        self.a.register([session()], ['ack'])
        reg = self.a.registration
        for body, code in [
            ({'registration': 'nope'}, 'invalid_uuid'),
            ({'registration': 5}, 'invalid_uuid'),
            ({'registration': reg, 'peers_revision': 5}, 'invalid_peers_revision'),
            ({'registration': reg, 'peers_revision': 'r' * 257}, 'invalid_peers_revision'),
            ({'registration': reg, 'acks': 'x'}, 'invalid_acks'),
            ({'registration': reg, 'acks': ['A' * 32]}, 'invalid_acks'),
            ({'registration': reg, 'acks': ['a' * 31]}, 'invalid_acks'),
            ({'registration': reg, 'acks': [1]}, 'invalid_acks'),
            ({'registration': reg, 'acks': ['a' * 32] * 513}, 'invalid_acks'),
        ]:
            with self.subTest(body=str(body)[:60]):
                self.assertError(self.a.call('/v1/poll', body), 400, code)
        self.assertError(self.server.call(self.alpha, '/v1/poll', [1]), 400, 'invalid_body')
        status, value = self.a.call('/v1/poll', {'registration': reg, 'peers_revision': 'r' * 256, 'acks': ['a' * 32] * 512})
        self.assertEqual(status, 200)


class Sending(ServerCase):
    def setUp(self):
        super().setUp()
        self.a1, self.b1 = sid(), sid()
        self.a.register([session(self.a1, 'alpha-chat')])
        self.b.register([session(self.b1, 'beta-chat')])
        self.src, self.dst = 'alpha/' + self.a1, 'beta/' + self.b1

    def test_send_routes_one_message_event_with_server_sender_name(self):
        message = str(uuid.uuid4())
        status, value = self.a.send(message, self.src, self.dst, 'héllo λ\n<b>x</b>', 'bypass', hop_chain='free-form')
        self.assertEqual((status, value), (200, {'ok': True, 'id': message, 'state': 'routed'}))
        events = self.b.poll(revision=False)['events']
        self.assertEqual(events, [{'id': message, 'from': self.src, 'to': self.dst, 'text': 'héllo λ\n<b>x</b>',
                                   'mode': 'bypass', 'hop_chain': 'free-form', 'type': 'message',
                                   'sender_name': 'alpha-chat'}])
        self.assertEqual(self.b.poll(revision=False)['events'], [])  # Consumed: no acknowledgments negotiated.
        stored = self.message_state(message)
        self.assertEqual(scrub(self, stored, created_at=is_time, updated_at=is_time), {
            'id': message, 'from': self.src, 'to': self.dst, 'text': 'héllo λ\n<b>x</b>', 'state': 'routed',
            'created_at': '<created_at>', 'updated_at': '<updated_at>'})

    def test_identical_resend_returns_recorded_state_and_conflicts_are_refused(self):
        message = str(uuid.uuid4())
        body = {'id': message, 'from': self.src, 'to': self.dst, 'text': 'once', 'mode': 'prompting'}
        self.assertEqual(self.a.call('/v1/send', body)[1]['state'], 'routed')
        reordered = {key: body[key] for key in reversed(list(body))}
        self.assertEqual(self.a.call('/v1/send', reordered), (200, {'ok': True, 'id': message, 'state': 'routed'}))
        self.assertError(self.a.call('/v1/send', dict(body, text='twice')), 409, 'id_conflict')
        self.assertError(self.a.call('/v1/send', dict(body, hop_chain='x')), 409, 'id_conflict')
        self.assertEqual(len(self.b.poll(revision=False)['events']), 1)
        self.b.status(message, 'handed_to_claude', 'native transport written')
        self.assertEqual(self.a.call('/v1/send', body), (200, {'ok': True, 'id': message, 'state': 'handed_to_claude'}))
        # A resend is answered from the record even after the recipient left, and is never queued again.
        self.b.register([])
        self.assertEqual(self.a.call('/v1/send', body)[1]['state'], 'handed_to_claude')
        self.b.register([session(self.b1, 'beta-chat')])
        self.assertEqual(self.b.poll(revision=False)['events'], [])

    def test_send_validation_codes(self):
        mid = str(uuid.uuid4())
        good = {'id': mid, 'from': self.src, 'to': self.dst, 'text': 'x', 'mode': 'prompting'}
        cases = [
            (dict(good, id='nope'), 400, 'invalid_uuid'),
            ({k: v for k, v in good.items() if k != 'id'}, 400, 'invalid_uuid'),
            (dict(good, extra=1), 400, 'invalid_message'),
            ({k: v for k, v in good.items() if k != 'from'}, 400, 'invalid_route'),
            (dict(good, **{'from': 'alpha'}), 400, 'invalid_route'),
            (dict(good, **{'from': 'alpha/x/y'}), 400, 'invalid_route'),
            (dict(good, **{'from': 'al pha/' + self.a1}), 400, 'invalid_route'),
            (dict(good, **{'from': 'alpha/nope'}), 400, 'invalid_uuid'),
            (dict(good, to='beta'), 400, 'invalid_route'),
            (dict(good, to='beta/' + self.b1.upper()), 400, 'invalid_uuid'),
            (dict(good, **{'from': 'beta/' + self.b1}), 403, 'source_forbidden'),
            (dict(good, **{'from': 'alpha/' + sid()}), 403, 'source_forbidden'),
            (dict(good, text=5), 400, 'invalid_message'),
            (dict(good, text='é' * 4097), 400, 'invalid_message'),
            (dict(good, mode='auto'), 400, 'invalid_message'),
            ({k: v for k, v in good.items() if k != 'mode'}, 400, 'invalid_message'),
            (dict(good, hop_chain=5), 400, 'invalid_hop_chain'),
            (dict(good, hop_chain='h' * 8193), 400, 'invalid_hop_chain'),
            (dict(good, to='zeta/' + sid()), 404, 'target_offline'),
            (dict(good, to='beta/' + sid()), 404, 'target_offline'),
            (dict(good, to='gamma/' + sid()), 404, 'target_offline'),
            (dict(good, text='\ud800'), 400, 'invalid_unicode'),
        ]
        for body, status, code in cases:
            with self.subTest(code=code, body=str(body)[:90]):
                self.assertError(self.a.call('/v1/send', body), status, code)
        self.assertError(self.server.call(self.alpha, '/v1/send', 'text'), 400, 'invalid_body')
        # Nothing was stored or queued; boundary sizes pass.
        self.assertEqual(self.server.messages(), [])
        self.assertEqual(self.a.call('/v1/send', dict(good, text='é' * 4096, hop_chain='h' * 8192))[0], 200)

    def test_status_updates_reach_the_sender_and_terminal_states_hold(self):
        m1, m2, m3 = (str(uuid.uuid4()) for _ in range(3))
        for message in (m1, m2, m3):
            self.a.send(message, self.src, self.dst)
        self.assertEqual(self.b.status(m1, 'handed_to_claude', 'native transport written'), (200, {'ok': True}))
        self.assertEqual(self.b.status(m2, 'held'), (200, {'ok': True}))
        events = self.a.poll(revision=False)['events']
        self.assertEqual(events, [
            {'type': 'status', 'id': m1, 'state': 'handed_to_claude', 'to': self.src, 'detail': 'native transport written'},
            {'type': 'status', 'id': m2, 'state': 'held', 'to': self.src}])
        stored = self.message_state(m1)
        self.assertEqual((stored['state'], stored['detail']), ('handed_to_claude', 'native transport written'))
        self.assertNotIn('detail', self.message_state(m2))
        # Held advances to handed_to_claude only with the native release marker.
        self.assertEqual(self.b.status(m2, 'handed_to_claude', 'native transport written'), (200, {'ok': True}))
        self.assertEqual(self.message_state(m2)['state'], 'held')
        self.assertEqual(self.b.status(m2, 'handed_to_claude', 'delivered'), (200, {'ok': True}))
        self.assertEqual(self.message_state(m2)['state'], 'handed_to_claude')
        # Terminal refused/failed are never overwritten by a weaker observation, silently.
        self.b.status(m3, 'refused', 'native receipt: refused')
        for weaker in ('handed_to_claude', 'held', 'unknown'):
            self.assertEqual(self.b.status(m3, weaker), (200, {'ok': True}))
        self.assertEqual(self.message_state(m3)['state'], 'refused')
        self.b.status(m3, 'failed', 'later')
        self.assertEqual((self.message_state(m3)['state'], self.message_state(m3)['detail']), ('failed', 'later'))
        events = self.a.poll(revision=False)['events']
        self.assertEqual([(e['id'], e['state']) for e in events],
                         [(m2, 'handed_to_claude'), (m3, 'refused'), (m3, 'failed')])

    def test_status_validation_codes(self):
        message = str(uuid.uuid4())
        self.a.send(message, self.src, self.dst)
        for body, status, code in [
            ({'id': 'x', 'state': 'held'}, 400, 'invalid_uuid'),
            ({'id': message, 'state': 'read'}, 400, 'invalid_status'),
            ({'id': message}, 400, 'invalid_status'),
            ({'id': message, 'state': 'held', 'detail': 5}, 400, 'invalid_detail'),
            ({'id': message, 'state': 'held', 'detail': 'd' * 2049}, 400, 'invalid_detail'),
            ({'id': str(uuid.uuid4()), 'state': 'held'}, 404, 'message_not_found'),
        ]:
            with self.subTest(code=code):
                self.assertError(self.b.call('/v1/status', body), status, code)
        self.assertError(self.a.status(message, 'held'), 403, 'status_forbidden')
        self.assertError(self.c.status(message, 'held'), 403, 'status_forbidden')
        self.assertError(self.server.call(self.beta, '/v1/status', [message]), 400, 'invalid_body')
        self.assertEqual(self.b.status(message, 'held', 'd' * 2048), (200, {'ok': True}))
        self.assertEqual(self.b.call('/v1/status', {'id': message, 'state': 'held', 'detail': None}), (200, {'ok': True}))


class Acknowledgments(ServerCase):
    def setUp(self):
        super().setUp()
        self.a1, self.b1 = sid(), sid()
        self.a.register([session(self.a1, 'alpha-chat')], ['ack'])
        self.b.register([session(self.b1, 'beta-chat')], ['ack'])
        self.src, self.dst = 'alpha/' + self.a1, 'beta/' + self.b1

    def poll_b(self, acks):
        return self.b.poll(acks=acks, revision=False)['events']

    def test_unacknowledged_events_are_redelivered_with_the_same_ack_id(self):
        message = str(uuid.uuid4())
        self.a.send(message, self.src, self.dst, 'hello')
        first = self.poll_b([])
        self.assertEqual(len(first), 1)
        self.assertTrue(is_hex32(first[0]['ack_id']))
        self.assertEqual({k: v for k, v in first[0].items() if k != 'ack_id'},
                         {'id': message, 'from': self.src, 'to': self.dst, 'text': 'hello', 'mode': 'prompting',
                          'type': 'message', 'sender_name': 'alpha-chat'})
        again = self.poll_b([])
        self.assertEqual(again, first)
        self.assertEqual(self.message_state(message)['state'], 'routed')
        self.assertEqual(self.poll_b([first[0]['ack_id'], 'f' * 32]), [])
        self.assertEqual(self.message_state(message)['state'], 'received')
        self.assertEqual(self.poll_b([]), [])
        self.b.status(message, 'handed_to_claude', 'native transport written')
        status = self.a.poll(acks=[], revision=False)['events']
        self.assertEqual(len(status), 1)
        self.assertTrue(is_hex32(status[0]['ack_id']))
        self.assertEqual({k: v for k, v in status[0].items() if k != 'ack_id'},
                         {'type': 'status', 'id': message, 'state': 'handed_to_claude', 'to': self.src,
                          'detail': 'native transport written'})
        self.assertEqual(self.a.poll(acks=[], revision=False)['events'], status)
        self.assertEqual(self.a.poll(acks=[status[0]['ack_id']], revision=False)['events'], [])

    def test_redelivery_skips_messages_that_are_no_longer_routed(self):
        message = str(uuid.uuid4())
        self.a.send(message, self.src, self.dst)
        self.assertEqual(len(self.poll_b([])), 1)
        self.b.status(message, 'held')
        self.assertEqual(self.poll_b([]), [])

    def test_reregistering_without_ack_hands_back_in_flight_events_the_original_way(self):
        message = str(uuid.uuid4())
        self.a.send(message, self.src, self.dst, 'hello')
        self.assertEqual(len(self.poll_b([])), 1)
        self.b.register([session(self.b1, 'beta-chat')])
        events = self.b.poll(revision=False)['events']
        self.assertEqual(events, [{'id': message, 'from': self.src, 'to': self.dst, 'text': 'hello',
                                   'mode': 'prompting', 'type': 'message', 'sender_name': 'alpha-chat'}])
        self.assertEqual(self.b.poll(revision=False)['events'], [])
        self.assertEqual(self.message_state(message)['state'], 'routed')

    def test_acks_are_ignored_without_the_capability_or_with_a_stale_registration(self):
        message = str(uuid.uuid4())
        self.a.send(message, self.src, self.dst)
        stale = {'registration': str(uuid.uuid4()), 'peers_revision': None, 'acks': ['a' * 32]}
        self.assertEqual(self.b.call('/v1/poll', stale)[1]['events'], [])
        events = self.poll_b([])
        self.b.register([session(self.b1, 'beta-chat')])  # Now without ack.
        consumed = self.b.poll(acks=[events[0]['ack_id']], revision=False)['events']
        self.assertEqual([e['id'] for e in consumed], [message])
        self.assertNotIn('ack_id', consumed[0])
        self.assertEqual(self.message_state(message)['state'], 'routed')


class PingPong(ServerCase):
    def setUp(self):
        super().setUp()
        self.b1 = sid()

    def ping(self, client, target):
        return self.background(client.call, '/v1/ping', {'to': target})

    def test_ping_is_answered_by_the_target_bridge(self):
        self.b.register([session(self.b1)], ['ping'])
        pending = self.ping(self.c, 'beta/' + self.b1)
        self.settle(0.4)
        events = self.b.poll(revision=False)['events']
        self.assertEqual(len(events), 1)
        event = events[0]
        self.assertTrue(is_uuid(event['id']))
        self.assertEqual(event, {'type': 'ping', 'id': event['id'], 'to': 'beta/' + self.b1})
        self.settle(0.05)
        self.assertError(self.a.call('/v1/pong', {'id': event['id'], 'live': True}), 403, 'pong_forbidden')
        self.assertEqual(self.b.call('/v1/pong', {'id': event['id'], 'live': True}), (200, {'ok': True}))
        self.assertEqual(self.b.call('/v1/pong', {'id': event['id'], 'live': False}), (200, {'ok': True}))
        status, value = pending()
        self.assertEqual(status, 200)
        self.assertEqual(scrub(self, value, rtt_ms=lambda v: type(v) is int and 0 <= v < 5000),
                         {'ok': True, 'rtt_ms': '<rtt_ms>', 'live': True})
        # Late answers are ignored; pings are not messages.
        self.assertEqual(self.b.call('/v1/pong', {'id': event['id'], 'live': True}), (200, {'ok': True}))
        self.assertEqual(self.server.messages(), [])

    def test_ping_with_acknowledgments_and_live_false(self):
        self.b.register([session(self.b1)], ['ping', 'ack'])
        pending = self.ping(self.a, 'beta/' + self.b1)
        self.settle(0.4)
        event = self.b.poll(acks=[], revision=False)['events'][0]
        self.assertTrue(is_hex32(event.pop('ack_id')))
        self.assertEqual(set(event), {'type', 'id', 'to'})
        self.b.call('/v1/pong', {'id': event['id'], 'live': False})
        self.assertEqual(pending()[1]['live'], False)

    def test_ping_refusals(self):
        self.b.register([session(self.b1)])
        self.a.register([session()], ['ping'])
        for body, status, code in [
            ({'to': 'beta/' + self.b1}, 409, 'ping_unsupported'),
            ({'to': 'beta/' + sid()}, 404, 'target_offline'),
            ({'to': 'zeta/' + sid()}, 404, 'target_offline'),
            ({'to': 'gamma/' + sid()}, 404, 'target_offline'),
            ({'to': 'nope'}, 400, 'invalid_route'),
            ({'to': 'beta/nope'}, 400, 'invalid_uuid'),
            ({}, 400, 'invalid_route'),
            ({'to': 'beta/' + self.b1, 'x': 1}, 400, 'invalid_ping'),
        ]:
            with self.subTest(code=code, body=body):
                self.assertError(self.c.call('/v1/ping', body), status, code)
        for body, code in [({'id': 'x', 'live': True}, 'invalid_uuid'), ({'id': str(uuid.uuid4()), 'live': 1}, 'invalid_pong'),
                           ({'id': str(uuid.uuid4())}, 'invalid_pong'),
                           ({'id': str(uuid.uuid4()), 'live': True, 'x': 1}, 'invalid_pong')]:
            with self.subTest(code=code, body=body):
                self.assertError(self.c.call('/v1/pong', body), 400, code)
        self.assertEqual(self.c.call('/v1/pong', {'id': str(uuid.uuid4()), 'live': True}), (200, {'ok': True}))

    def test_ping_timeout_and_dropped_ping_after_downgrade(self):
        self.b.register([session(self.b1)], ['ping'])
        started = time.monotonic()
        pending = self.ping(self.c, 'beta/' + self.b1)
        self.settle(0.3)
        self.b.register([session(self.b1)])  # The bridge no longer answers pings.
        self.assertEqual(self.b.poll(revision=False)['events'], [])
        self.assertError(pending(), 504, 'ping_timeout')
        self.assertTrue(4.5 < time.monotonic() - started < 8, time.monotonic() - started)


class Machines(ServerCase):
    def test_removed_viewer_paths_and_legacy_view_keys(self):
        """The browser viewer is gone: its paths answer like any unknown path, and a registry
        that still carries the old per-machine view flag loads with the flag ignored."""
        self.server.write_registry({m.fp: dict(m.entry(), view=m is not self.gamma)
                                    for m in (self.alpha, self.beta, self.gamma)})
        for client in (self.a, self.b, self.c):
            for path in ('/v1/view', '/'):
                with self.subTest(machine=client.machine.name, path=path):
                    self.assertError(self.server.request('GET', path, machine=client.machine), 404, 'not_found')
        self.assertEqual(self.b.call('/health'), (200, {'ok': True, 'machine': 'beta', 'owner': 'bob'}))


class Limits(ServerCase):
    def test_per_machine_request_rate_limit(self):
        statuses = [self.server.request('GET', '/health', machine=self.gamma).status for _ in range(130)]
        self.assertEqual(statuses[:120], [200] * 120)
        self.assertIn(429, statuses[120:])
        self.assertError(self.server.request('GET', '/health', machine=self.gamma), 429, 'rate_limited')
        self.assertEqual(self.server.request('GET', '/health', machine=self.beta).status, 200)

    def test_two_concurrent_polls_per_machine(self):
        b1 = sid()
        self.b.register([session(b1)])
        self.b.poll(revision=False)
        first, second = self.background(self.b.poll), self.background(self.b.poll)
        self.settle(0.6)
        self.assertError(self.b.call('/v1/poll', {'registration': self.b.registration, 'peers_revision': self.b.revision}),
                         429, 'rate_limited')
        self.assertEqual(self.b.call('/health')[0], 200)
        self.a.register([session()])  # Wakes both waiting polls.
        first(), second()
        self.assertEqual(self.b.call('/v1/poll', {'registration': self.b.registration, 'peers_revision': None})[0], 200)

    def test_sixteen_active_requests_per_machine(self):
        b1 = sid()
        self.b.register([session(b1)], ['ping'])  # Never answers: each ping waits five seconds.
        pings = [self.background(self.c.call, '/v1/ping', {'to': 'beta/' + b1}) for _ in range(16)]
        self.settle(1.0)
        self.assertError(self.c.call('/health'), 429, 'rate_limited')
        for pending in pings:
            self.assertError(pending(), 504, 'ping_timeout')
        self.assertEqual(self.c.call('/health')[0], 200)

    def test_recipient_queue_and_sender_status_queue_are_bounded(self):
        # Every machine has its own request budget, so extra senders fill queues quickly.
        senders = [Machine(f'sender-{index}') for index in range(6)]
        self.server.write_registry({m.fp: m.entry() for m in [self.alpha, self.beta, self.gamma, *senders]})
        clients = [Client(self.server, m) for m in senders]
        routes = []
        for client in clients:
            identifier = sid()
            client.register([session(identifier)])
            routes.append((client, client.machine.name + '/' + identifier))
        target, sender = sid(), sid()
        self.b.register([session(target)])
        self.a.register([session(sender)])

        def fill(destination, count):
            for index in range(count):
                client, source = routes[index % len(routes)]
                self.assertEqual(client.send(str(uuid.uuid4()), source, destination)[0], 200, index)
        # 256 queued events fill beta's queue; the next send is refused.
        fill('beta/' + target, 256)
        self.assertError(routes[0][0].send(str(uuid.uuid4()), routes[0][1], 'beta/' + target), 503, 'queue_full')
        self.assertEqual(self.b.poll(revision=False)['events'][0]['type'], 'message')  # Drains beta.
        # A status update for a sender whose queue is full is recorded and kept
        # for the sender's machine until its queue has room.
        message = str(uuid.uuid4())
        self.a.send(message, 'alpha/' + sender, 'beta/' + target)
        fill('alpha/' + sender, 256)
        self.assertEqual(self.b.status(message, 'held', 'native receipt: held'), (200, {'ok': True}))
        # An identical resend reports the recorded state.
        self.assertEqual(self.a.send(message, 'alpha/' + sender, 'beta/' + target),
                         (200, {'ok': True, 'id': message, 'state': 'held'}))
        self.assertEqual({e['type'] for e in self.a.poll(revision=False)['events']}, {'message'})
        self.assertEqual(self.a.poll(revision=False)['events'],
                         [{'type': 'status', 'id': message, 'state': 'held', 'detail': 'native receipt: held',
                           'to': 'alpha/' + sender}])
