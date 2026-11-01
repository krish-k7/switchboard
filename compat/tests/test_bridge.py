"""Bridge end to end: a real server, a real bridge for machine-a with fake native
Claude sessions, and the test acting as machine-b directly over the protocol."""
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import time
import unittest
import uuid

from sbcompat.bridge import BridgeProcess
from sbcompat.claude import NativeEnv, envelope
from sbcompat.impl import command, slow
from sbcompat.server import Client, Poller, ServerProcess, tls_machine
from sbcompat.util import (child_env, is_hex32, is_time, native_address, private_dir, private_root, process_start,
                           wait_until, write_private)

AUTHOR = "Switchboard on this machine wrote this, not {name}; don't reply to it.\n\n"


class BridgeCase(unittest.TestCase):
    maxDiff = None

    def setUp(self):
        self.root = private_root(self)
        self.machine_a = tls_machine('machine-a', owner='alice')
        self.machine_b = tls_machine('machine-b', owner='bob')
        self.server = ServerProcess(self, self.root / 'server', 'tls', [self.machine_a, self.machine_b])
        self.server.start()
        self.native = NativeEnv(self, self.root / 'native')
        git = private_dir(self.root / 'work' / 'example' / '.git')
        (git / 'HEAD').write_text('ref: refs/heads/main\n')
        self.project = private_dir(self.root / 'work' / 'example' / 'solver')
        self.s1 = self.native.session(name='alpha chat', status='idle', cwd=str(self.project))
        self.b = Client(self.server, self.machine_b)
        self.t1 = str(uuid.uuid4())

    def bridge(self, sessions=None, start=True, **config):
        bridge = BridgeProcess(self, self.root / 'bridge', self.server, self.machine_a, self.native,
                               sessions if sessions is not None else [self.s1.sid], **config)
        if start:
            bridge.start()
        return bridge

    def alias(self, name, sid, machine='machine-b'):
        return f'switchboard/{machine}/{name}-{sid[:8]}'

    def peers_of(self, machine):
        return [p for p in self.b.call('/v1/peers')[1]['peers'] if p['machine'] == machine]

    def wait_proxy(self, sid=None, timeout=20, predicate=None):
        sid = sid or self.t1

        def find():
            for record in self.native.proxies().values():
                if record.get('name', '').endswith('-' + sid[:8]) and (predicate is None or predicate(record)):
                    return record
            return None
        return wait_until(find, timeout, message='a native proxy for ' + sid)

    def connect_b(self, name='beta chat', status='busy', capabilities=('ack', 'ping'), ack=True, pong=None, interval=0.0):
        self.b.register([dict(id=self.t1, name=name, status=status)], list(capabilities))
        poller = Poller(self.b, ack=ack, pong=pong, interval=interval).start()
        self.addCleanup(poller.stop)
        return poller

    def message_state(self, message_id):
        for message in self.server.messages():
            if message['id'] == message_id:
                return message
        return None

    def native_frame(self, session, timeout=15):
        event = session.wait_frame(timeout)
        self.assertEqual(event['event'], 'frame', event)
        self.assertTrue(event['auth_ok'])
        self.assertEqual(len(event['lines']), 2)
        return event['lines'][1]


class Discovery(BridgeCase):
    def test_registration_runtime_logs_and_membership(self):
        s3 = self.native.session(name='third chat', status='waiting')
        s2 = self.native.session(name='not joined', status='idle')
        bridge = self.bridge(sessions=[self.s1.sid, s3.sid], names={s3.sid: 'chosen-name'})
        ready = bridge.wait_log('ready')
        self.assertEqual(ready['machine'], 'machine-a')
        self.assertRegex(bridge.log_lines[0], r'^\{"at": "\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", ')
        runtime = bridge.wait_connected()
        self.assertEqual(stat.S_IMODE(bridge.runtime_path().stat().st_mode), 0o600)
        self.assertTrue(is_time(runtime.pop('updated_at')))
        self.assertIn(runtime.pop('discovery_mode'), ('events', 'polling'))
        self.assertIsInstance(runtime.pop('discovery_detail'), (str, type(None)))
        pid = bridge.process.pid
        published = sorted([dict(id=self.s1.sid, name='alpha chat'), dict(id=s3.sid, name='chosen-name')], key=lambda s: s['id'])
        runtime['published'] = sorted(runtime['published'], key=lambda s: s['id'])
        self.assertEqual(runtime, {'pid': pid, 'procStart': process_start(pid), 'platform': sys.platform,
                                   'machine': 'machine-a', 'connected': True,
                                   'published': published, 'remote': {'sessions': 0, 'machines': 0}})
        # Metadata is added after capability negotiation; the unjoined chat is never advertised.
        expected = sorted([dict(id=self.s1.sid, name='alpha chat', status='idle', project='solver', repository='example',
                                branch='main', machine='machine-a', owner='alice'),
                           dict(id=s3.sid, name='chosen-name', status='waiting', machine='machine-a', owner='alice')],
                          key=lambda p: p['id'])
        wait_until(lambda: [{k: v for k, v in p.items() if k != 'last_seen'} for p in self.peers_of('machine-a')] == expected,
                   15, message='advertised roster with metadata')
        membership = json.loads((bridge.cfg / 'membership.json').read_text())
        self.assertEqual(membership, {'version': 1, 'sessions': {self.s1.sid: {'name': 'alpha-chat'},
                                                                 s3.sid: {'name': 'chosen-name'}}})
        # Chats with no join record start their inactivity clock when the bridge first sees them.
        activity = json.loads((bridge.state / 'activity.json').read_text())
        self.assertEqual(set(activity), {self.s1.sid, s3.sid})
        self.assertTrue(all(isinstance(v, int) and is_time(v) for v in activity.values()))
        # Activity and departures reach the roster.
        self.s1.update(status='busy')
        wait_until(lambda: any(p['id'] == self.s1.sid and p['status'] == 'busy' for p in self.peers_of('machine-a')),
                   15, message='busy status')
        s3.close()
        wait_until(lambda: [p['id'] for p in self.peers_of('machine-a')] == [self.s1.sid], 40, message='departure')
        self.assertNotIn(s2.sid, json.dumps(bridge.runtime()))

    def test_remote_sessions_become_native_proxies(self):
        bridge = self.bridge()
        bridge.wait_connected()
        self.b.register([dict(id=self.t1, name='beta chat', status='busy')])
        record = self.wait_proxy()
        self.assertEqual(record['name'], self.alias('beta-chat', self.t1))
        self.assertEqual((record['status'], record['kind'], record['peerBridgeProxy']), ('busy', 'interactive', True))
        self.assertEqual(record['procStart'], process_start(record['pid']))
        wait_until(lambda: bridge.runtime()['remote'] == {'sessions': 1, 'machines': 1}, 10, message='remote summary')
        # Renames and activity reconfigure the same helper in place.
        self.b.register([dict(id=self.t1, name='renamed', status='unknown')])
        renamed = self.wait_proxy(predicate=lambda r: r['name'] == self.alias('renamed', self.t1))
        self.assertEqual((renamed['pid'], renamed['sessionId'], renamed['messagingSocketPath']),
                         (record['pid'], record['sessionId'], record['messagingSocketPath']))
        self.assertNotIn('status', renamed)
        self.b.register([])
        wait_until(lambda: not self.native.proxies(), 15, message='proxy withdrawal')
        # The proxy records go before the bridge rewrites its runtime summary.
        wait_until(lambda: bridge.runtime()['remote'] == {'sessions': 0, 'machines': 0}, 10,
                   message='remote summary after withdrawal')

    def test_disabled_bridge_exits_without_publishing(self):
        bridge = self.bridge(start=False, enabled=False)
        bridge.start()
        self.assertEqual(bridge.process.wait(15), 0)
        self.assertEqual(bridge.wait_log('disabled', 5)['event'], 'disabled')
        self.assertFalse(bridge.runtime_path().exists())

    def test_one_bridge_per_state_directory_and_clean_shutdown(self):
        bridge = self.bridge()
        bridge.wait_connected()
        self.b.register([dict(id=self.t1, name='beta chat', status='idle')])
        self.wait_proxy()
        second = subprocess.run(command('bridge', '--config', bridge.config_path), capture_output=True,
                                env=child_env(), timeout=20)
        self.assertNotEqual(second.returncode, 0)
        self.assertTrue(bridge.runtime()['connected'])
        self.assertEqual(bridge.stop(), 0)
        self.assertFalse(bridge.runtime_path().exists())
        self.assertEqual(self.native.proxies(), {})
        self.assertEqual(sorted(p.name for p in bridge.state.glob('bridge-*.json')), [])
        self.assertEqual(sorted(p.name for p in bridge.state.glob('proxy-*.json')), [])


class Messages(BridgeCase):
    def test_outbound_message_and_receipts(self):
        poller = self.connect_b()
        self.bridge().wait_connected()
        proxy = self.wait_proxy()
        frame = self.s1.user_frame(proxy, text='hello beta', mode='prompting', hop_chain='a' * 24)
        self.s1.send_frame(proxy, frame)
        native_id = frame['msg_id']
        event = poller.wait(lambda e: e.get('id') == native_id and e['type'] == 'message')
        self.assertTrue(is_hex32(event['ack_id']))
        self.assertEqual({k: v for k, v in event.items() if k != 'ack_id'}, {
            'id': native_id, 'from': 'machine-a/' + self.s1.sid, 'to': 'machine-b/' + self.t1, 'text': 'hello beta',
            'mode': 'prompting', 'hop_chain': 'a' * 24, 'type': 'message', 'sender_name': 'alpha chat'})
        wait_until(lambda: self.message_state(native_id)['state'] == 'received', 15, message='acknowledged')
        self.b.status(native_id, 'held', 'native receipt: held')
        address = native_address(proxy['messagingSocketPath'])
        receipt = self.native_frame(self.s1)
        self.assertEqual({k: v for k, v in receipt.items() if k != 'msg_id'}, {
            'msgV': 1, 'type': 'control', 'action': 'peer_message_status', 'status': 'held', 'orig_msg_id': native_id,
            'session_id': self.s1.sid, 'reason': 'Switchboard remote receiver observation', 'from': address})
        self.b.status(native_id, 'handed_to_claude', 'delivered')
        self.assertEqual(self.native_frame(self.s1)['status'], 'delivered')
        # A failed delivery comes back as one plain notice from the proxy.
        second = self.s1.user_frame(proxy, text='are you there?', mode='bypass')
        self.s1.send_frame(proxy, second)
        poller.wait(lambda e: e.get('id') == second['msg_id'])
        self.b.status(second['msg_id'], 'failed', 'local session is no longer enrolled/live')
        notice = self.native_frame(self.s1)
        name = proxy['name']
        attrs = {'from': address, 'from-session': proxy['sessionId'], 'from-name': name, 'from-mode': 'bypass'}
        body = (AUTHOR.format(name=name) + f'Your message to {name} was not delivered (local session is no longer '
                'enrolled/live). Your message began: "are you there?"\n\nResend it later or tell the user.')
        self.assertEqual(notice['message'], {'role': 'user', 'content': envelope(attrs, body)})
        self.assertEqual((notice['type'], notice['session_id'], notice['priority']), ('user', self.s1.sid, 'next'))

    def test_inbound_delivery_acknowledgment_and_persisted_ids(self):
        poller = self.connect_b()
        bridge = self.bridge()
        bridge.wait_connected()
        proxy = self.wait_proxy()
        message = str(uuid.uuid4())
        self.assertEqual(self.b.send(message, 'machine-b/' + self.t1, 'machine-a/' + self.s1.sid, 'hi alpha λ', 'bypass')[0], 200)
        frame = self.native_frame(self.s1)
        attrs = {'from': native_address(proxy['messagingSocketPath']), 'from-session': proxy['sessionId'],
                 'from-name': proxy['name'], 'from-mode': 'bypass'}
        self.assertEqual({k: v for k, v in frame.items() if k != 'msg_id'}, {
            'msgV': 1, 'type': 'user', 'session_id': self.s1.sid,
            'message': {'role': 'user', 'content': envelope(attrs, 'hi alpha λ')}, 'priority': 'next', 'from': attrs['from']})
        status = poller.wait(lambda e: e['type'] == 'status' and e['id'] == message)
        self.assertEqual({k: v for k, v in status.items() if k != 'ack_id'}, {
            'type': 'status', 'id': message, 'state': 'handed_to_claude', 'to': 'machine-b/' + self.t1,
            'detail': 'native transport written'})
        self.assertEqual(self.message_state(message)['state'], 'handed_to_claude')
        delivered = bridge.state / 'delivered-ids.json'
        self.assertIn(message, json.loads(delivered.read_text())['ids'])
        self.assertEqual(stat.S_IMODE(delivered.stat().st_mode), 0o600)
        self.s1.send_frame(proxy, self.s1.receipt_frame(frame['msg_id'], 'held'))
        held = poller.wait(lambda e: e['type'] == 'status' and e['id'] == message and e['state'] == 'held')
        self.assertEqual(held['detail'], 'native receipt: held')
        # A message to a chat that is not joined there is refused by the server.
        self.assertEqual(self.b.send(str(uuid.uuid4()), 'machine-b/' + self.t1, 'machine-a/' + str(uuid.uuid4()))[1]['error'],
                         'target_offline')

    def test_message_ids_already_handed_over_are_never_delivered_again(self):
        poller = self.connect_b()
        message = str(uuid.uuid4())
        state = private_dir(self.root / 'bridge' / 'state')
        write_private(state / 'delivered-ids.json', {'ids': {message: time.time()}})
        self.bridge().wait_connected()
        self.wait_proxy()
        self.b.send(message, 'machine-b/' + self.t1, 'machine-a/' + self.s1.sid, 'replayed')
        wait_until(lambda: self.message_state(message)['state'] == 'received', 15, message='acknowledged')
        self.s1.no_frame(2.0)
        self.assertEqual(self.message_state(message)['state'], 'received')
        self.assertFalse(any(e['type'] == 'status' and e['id'] == message for e in poller.events))

    def test_ping_is_answered_without_waking_the_chat(self):
        self.b.register([dict(id=self.t1, name='beta chat', status='idle')])
        self.bridge().wait_connected()
        wait_until(lambda: self.peers_of('machine-a'), 15, message='machine-a roster')
        status, value = self.b.call('/v1/ping', {'to': 'machine-a/' + self.s1.sid})
        self.assertEqual(status, 200, value)
        self.assertEqual((value['ok'], value['live'], type(value['rtt_ms'])), (True, True, int))
        self.s1.no_frame(0.5)

    def test_unjoined_chat_is_told_and_nothing_is_sent(self):
        poller = self.connect_b()
        other = self.native.session(name='not joined')
        bridge = self.bridge()
        bridge.wait_connected()
        proxy = self.wait_proxy()
        other.send_frame(proxy, other.user_frame(proxy, text='hello?'))
        notice = self.native_frame(other)
        self.assertIn("this chat hasn't joined Switchboard.", notice['message']['content'])
        rejected = bridge.wait_log('native_rejected')
        self.assertEqual(rejected['detail'], 'sender has not joined Switchboard; told it how to join')
        time.sleep(1)
        self.assertFalse(any(e['type'] == 'message' for e in poller.events))

    def test_uncertain_send_is_resent_after_the_connection_returns(self):
        poller = self.connect_b()
        self.bridge().wait_connected()
        proxy = self.wait_proxy()
        self.server.pause()  # The send below is written but never answered.
        frame = self.s1.user_frame(proxy, text='survives a server crash')
        self.s1.send_frame(proxy, frame)
        time.sleep(1.5)
        self.server.kill()
        self.server.start()
        self.b.register([dict(id=self.t1, name='beta chat', status='busy')], ['ack', 'ping'])
        event = poller.wait(lambda e: e.get('id') == frame['msg_id'], timeout=30)
        self.assertEqual((event['text'], event['from']), ('survives a server crash', 'machine-a/' + self.s1.sid))
        self.s1.no_frame(0.5)
        # The replacement helper adopted the message, so its outcome still reaches the sender.
        self.b.status(frame['msg_id'], 'refused', 'native receipt: denied')
        receipt = self.native_frame(self.s1)
        self.assertEqual((receipt['type'], receipt['status'], receipt['orig_msg_id']), ('control', 'denied', frame['msg_id']))

    def test_a_server_restart_tells_the_sender(self):
        poller = self.connect_b()
        self.bridge().wait_connected()
        proxy = self.wait_proxy()
        frame = self.s1.user_frame(proxy, text='in flight at the restart')
        self.s1.send_frame(proxy, frame)
        poller.wait(lambda e: e.get('id') == frame['msg_id'])
        self.server.stop()
        self.server.start()
        self.b.register([dict(id=self.t1, name='beta chat', status='busy')], ['ack', 'ping'])
        # The bridge reconnects with a new helper for the route, which adopts
        # the message so the server's notice still reaches the chat.
        notice = self.native_frame(self.s1, timeout=30)
        new_proxy = self.wait_proxy()
        name = new_proxy['name']
        body = (AUTHOR.format(name=name) + f"Switchboard couldn't confirm that your message to {name} arrived "
                '(the server restarted before delivery was confirmed); it may or may not have reached that chat. '
                'Your message began: "in flight at the restart"\n\nIf it matters, resend it once or tell the user.')
        attrs = {'from': native_address(new_proxy['messagingSocketPath']), 'from-session': new_proxy['sessionId'],
                 'from-name': name, 'from-mode': 'prompting'}
        self.assertEqual(notice['message']['content'], envelope(attrs, body))
        self.assertEqual(self.message_state(frame['msg_id'])['state'], 'unknown')
        self.s1.no_frame(1.0)

    @slow
    def test_unconfirmed_delivery_tells_the_sender(self):
        poller = self.connect_b(ack=False, interval=2.0)  # Polls but never acknowledges, so the message stays unconfirmed.
        self.bridge().wait_connected()
        proxy = self.wait_proxy()
        frame = self.s1.user_frame(proxy, text='anyone home?')
        self.s1.send_frame(proxy, frame)
        poller.wait(lambda e: e.get('id') == frame['msg_id'])
        notice = self.native_frame(self.s1, timeout=45)
        name = proxy['name']
        body = (AUTHOR.format(name=name) + f"Switchboard couldn't confirm that your message to {name} arrived "
                '(delivery not confirmed within 30 seconds); it may or may not have reached that chat. '
                'Your message began: "anyone home?"\n\nIf it matters, resend it once or tell the user.')
        attrs = {'from': native_address(proxy['messagingSocketPath']), 'from-session': proxy['sessionId'],
                 'from-name': name, 'from-mode': 'prompting'}
        self.assertEqual(notice['message']['content'], envelope(attrs, body))


class Membership(BridgeCase):
    def test_inactive_chats_expire_and_membership_follows(self):
        now = round(time.time())
        s3 = self.native.session(name='recent')
        s4 = self.native.session(name='working', status='busy')
        sessions = sorted([self.s1.sid, s3.sid, s4.sid])
        bridge = self.bridge(sessions=sessions, joined_at={self.s1.sid: now - 8 * 86400, s3.sid: now,
                                                           s4.sid: now - 8 * 86400},
                             names={self.s1.sid: 'old-name', s3.sid: 'fresh'})
        expired = bridge.wait_log('session_expired')
        self.assertEqual({k: v for k, v in expired.items() if k != 'at'},
                         {'event': 'session_expired', 'session': self.s1.sid, 'detail': 'no activity for 7 days'})
        bridge.wait_connected()
        config = bridge.read_config()
        self.assertEqual(config['sessions'], sorted([s3.sid, s4.sid]))
        self.assertEqual(config['names'], {s3.sid: 'fresh'})
        self.assertEqual(set(config['joined_at']), {s3.sid, s4.sid})
        wait_until(lambda: json.loads((bridge.cfg / 'membership.json').read_text()) ==
                   {'version': 1, 'sessions': {s3.sid: {'name': 'fresh'}, s4.sid: {'name': 'working'}}}, 10,
                   message='membership.json')
        wait_until(lambda: sorted(p['id'] for p in self.peers_of('machine-a')) == sorted([s3.sid, s4.sid]), 15,
                   message='roster without the expired chat')
        activity = json.loads((bridge.state / 'activity.json').read_text())
        self.assertEqual(set(activity), {s4.sid})

    def test_revocation_withdraws_every_proxy(self):
        bridge = self.bridge()
        bridge.wait_connected()
        self.b.register([dict(id=self.t1, name='beta chat', status='idle')])
        self.wait_proxy()
        self.assertEqual(self.server.local('revoke', 'machine-a')[1]['state'], 'revoked')
        wait_until(lambda: not self.native.proxies(), 15, message='proxies withdrawn')
        wait_until(lambda: bridge.runtime()['connected'] is False, 10, message='disconnected runtime')
        self.assertEqual(bridge.runtime()['published'], [])


class TwoBridges(unittest.TestCase):
    """Two bridges, on machine-a and machine-b, each with its own native Claude
    namespace, through one server: messages, receipts, notices, and pings in
    both directions."""
    maxDiff = None

    def setUp(self):
        self.root = private_root(self)
        self.machine_a = tls_machine('machine-a', owner='alice')
        self.machine_b = tls_machine('machine-b', owner='bob')
        self.server = ServerProcess(self, self.root / 'server', 'tls', [self.machine_a, self.machine_b])
        self.server.start()
        self.native_a = NativeEnv(self, self.root / 'native-a')
        self.native_b = NativeEnv(self, self.root / 'native-b')
        self.s1 = self.native_a.session(name='alpha chat', status='idle')
        self.t1 = self.native_b.session(name='beta chat', status='busy')
        self.bridge_a = BridgeProcess(self, self.root / 'bridge-a', self.server, self.machine_a, self.native_a,
                                      [self.s1.sid]).start()
        self.bridge_b = BridgeProcess(self, self.root / 'bridge-b', self.server, self.machine_b, self.native_b,
                                      [self.t1.sid]).start()
        self.bridge_a.wait_connected()
        self.bridge_b.wait_connected()

    def proxy(self, env, sid, timeout=20):
        def find():
            for record in env.proxies().values():
                if record.get('name', '').endswith('-' + sid[:8]):
                    return record
            return None
        return wait_until(find, timeout, message='a native proxy for ' + sid)

    def frame(self, session, timeout=15):
        event = session.wait_frame(timeout)
        self.assertEqual(event['event'], 'frame', event)
        self.assertTrue(event['auth_ok'])
        return event['lines'][1]

    def test_messages_receipts_and_pings_cross_both_bridges(self):
        to_beta = self.proxy(self.native_a, self.t1.sid)
        to_alpha = self.proxy(self.native_b, self.s1.sid)
        self.assertEqual(to_beta['name'], f'switchboard/machine-b/beta-chat-{self.t1.sid[:8]}')
        self.assertEqual(to_alpha['name'], f'switchboard/machine-a/alpha-chat-{self.s1.sid[:8]}')
        self.assertEqual(to_beta.get('status'), 'busy')
        self.assertEqual(to_alpha.get('status'), 'idle')
        # machine-a to machine-b, with a held receipt back.
        outbound = self.s1.user_frame(to_beta, text='hello beta λ', mode='prompting', hop_chain='b' * 24)
        self.s1.send_frame(to_beta, outbound)
        received = self.frame(self.t1)
        attrs = {'from': native_address(to_alpha['messagingSocketPath']), 'from-session': to_alpha['sessionId'],
                 'hop-chain': 'b' * 24, 'from-name': to_alpha['name'], 'from-mode': 'prompting'}
        self.assertEqual({k: v for k, v in received.items() if k != 'msg_id'}, {
            'msgV': 1, 'type': 'user', 'session_id': self.t1.sid,
            'message': {'role': 'user', 'content': envelope(attrs, 'hello beta λ')}, 'priority': 'next',
            'from': attrs['from']})
        self.t1.send_frame(to_alpha, self.t1.receipt_frame(received['msg_id'], 'held'))
        receipt = self.frame(self.s1)
        self.assertEqual({k: v for k, v in receipt.items() if k != 'msg_id'}, {
            'msgV': 1, 'type': 'control', 'action': 'peer_message_status', 'status': 'held',
            'orig_msg_id': outbound['msg_id'], 'session_id': self.s1.sid,
            'reason': 'Switchboard remote receiver observation', 'from': native_address(to_beta['messagingSocketPath'])})
        # machine-b to machine-a, and a denial back.
        reply = self.t1.user_frame(to_alpha, text='hi alpha', mode='bypass')
        self.t1.send_frame(to_alpha, reply)
        inbound = self.frame(self.s1)
        attrs = {'from': native_address(to_beta['messagingSocketPath']), 'from-session': to_beta['sessionId'],
                 'from-name': to_beta['name'], 'from-mode': 'bypass'}
        self.assertEqual(inbound['message'], {'role': 'user', 'content': envelope(attrs, 'hi alpha')})
        self.s1.send_frame(to_beta, self.s1.receipt_frame(inbound['msg_id'], 'denied'))
        denied = self.frame(self.t1)
        self.assertEqual((denied['type'], denied['status'], denied['orig_msg_id']), ('control', 'denied', reply['msg_id']))
        states = {m['id']: m['state'] for m in self.server.messages()}
        self.assertEqual(states[outbound['msg_id']], 'held')
        self.assertEqual(states[reply['msg_id']], 'refused')
        # Pings are answered by each bridge, never by a chat.
        for machine, target in ((self.machine_b, 'machine-a/' + self.s1.sid), (self.machine_a, 'machine-b/' + self.t1.sid)):
            status, value = self.server.call(machine, '/v1/ping', {'to': target})
            self.assertEqual((status, value['ok'], value['live']), (200, True, True), value)
        self.s1.no_frame(0.5)
        self.t1.no_frame(0.5)
        # A chat that leaves on one side disappears on the other.
        self.bridge_b.write_config(dict(self.bridge_b.read_config(), sessions=[]))
        wait_until(lambda: not self.native_a.proxies(), 40, message='machine-a proxies withdrawn')
        self.assertEqual(self.bridge_a.stop(), 0)
        self.assertEqual(self.bridge_b.stop(), 0)
        self.assertEqual(self.native_a.proxies(), {})
        self.assertEqual(self.native_b.proxies(), {})
