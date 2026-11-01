"""Server timers that only real time can exercise: the 30-second delivery and lease
cutoffs and the 10-second long-poll wait. Skipped by --quick.

The server exposes no setting to shorten these timers, so each test waits them
out once and checks every behavior that depends on the same deadline.
"""
import time
import uuid

from sbcompat.cases import ServerCase, session, sid
from sbcompat.impl import slow
from sbcompat.util import is_hex32


class ThirtySecondDeadlines(ServerCase):
    @slow
    def test_unconfirmed_deliveries_expire_and_tell_the_sender(self):
        a1, b1, g1 = sid(), sid(), sid()
        self.a.register([session(a1)], ['ack'])
        self.b.register([session(b1)], ['ack'])
        self.c.register([session(g1)])
        old_gamma = self.c.registration
        src, dst = 'alpha/' + a1, 'beta/' + b1
        routed, received, held = (str(uuid.uuid4()) for _ in range(3))
        for message in (routed, received, held):
            self.a.send(message, src, dst, text=message)
        events = {e['id']: e for e in self.b.poll(acks=[], revision=False)['events']}
        self.assertEqual(set(events), {routed, received, held})
        self.b.status(held, 'held', 'native receipt: held')
        acked_at = time.monotonic()
        self.b.poll(acks=[events[received]['ack_id']], revision=False)
        self.assertEqual(self.message_state(received)['state'], 'received')
        # The held status event reaches alpha but is never acknowledged.
        first = self.a.poll(acks=[], revision=False)['events']
        self.assertEqual([(e['id'], e['state']) for e in first], [(held, 'held')])
        started = time.monotonic()
        # Alpha and beta keep their leases (without acknowledging anything); gamma goes silent.
        while time.monotonic() - started < 25:
            again = self.a.poll(acks=[], revision=False)['events']
            self.assertEqual([(e['id'], e['state']) for e in again], [(held, 'held')])
            self.assertEqual([e['id'] for e in self.b.poll(acks=[], revision=False)['events']], [routed])
            time.sleep(3)
        while time.monotonic() - acked_at < 31.5:
            self.a.poll(acks=[], revision=False)
            self.b.poll(acks=[], revision=False)
            time.sleep(1)
        collected = []
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            collected += self.a.poll(acks=[], revision=False)['events']
            if {routed, received} <= {e['id'] for e in collected if e['state'] == 'unknown'}:
                break
            time.sleep(0.5)
        unknown = {e['id']: e for e in collected if e['state'] == 'unknown'}
        for message, detail in ((routed, 'delivery not confirmed within 30 seconds'),
                                (received, 'received by the recipient machine but handoff not confirmed within 30 seconds')):
            event = dict(unknown[message])
            self.assertTrue(is_hex32(event.pop('ack_id')))
            self.assertEqual(event, {'type': 'status', 'id': message, 'state': 'unknown', 'detail': detail, 'to': src})
            self.assertEqual((self.message_state(message)['state'], self.message_state(message)['detail']), ('unknown', detail))
        # Redelivered status observations older than 30 seconds are dropped.
        self.assertNotIn((held, 'held'), [(e['id'], e['state']) for e in collected])
        # Held messages do not age out; the expired routed message is no longer redelivered.
        self.assertEqual(self.message_state(held)['state'], 'held')
        self.assertEqual(self.b.poll(acks=[], revision=False)['events'], [])
        # Gamma's lease lapsed: its sessions left the roster and its registration was replaced.
        self.assertNotIn('gamma', {p['machine'] for p in self.b.call('/v1/peers')[1]['peers']})
        self.assertNotEqual(self.c.register([session(g1)])['registration'], old_gamma)


class TenSecondWait(ServerCase):
    @slow
    def test_long_polls_wait_ten_seconds_then_return_empty(self):
        self.b.register([session()])
        self.b.poll(revision=False)
        modern = self.background(self.b.poll)
        started = time.monotonic()
        legacy = self.c.poll(legacy=True)
        self.assertTrue(9.5 < time.monotonic() - started < 13, time.monotonic() - started)
        self.assertEqual(legacy['events'], [])
        self.assertIn('peers', legacy)
        value = modern()
        self.assertEqual(value['events'], [])
        self.assertNotIn('peers', value)
