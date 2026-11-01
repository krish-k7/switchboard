"""Native helper: its published native identity, line-JSON IPC, and native frames.

Each test runs a real helper process from the configured command, next to fake
native Claude sessions (real processes with registry records and sockets), and
checks only what crosses the IPC pipes, the native sockets, and the registry.
"""
import json
import os
from pathlib import Path
import stat
import subprocess
import time
import unittest
import uuid

from sbcompat.claude import NativeEnv, envelope
from sbcompat.helper import HelperProcess
from sbcompat.util import is_hex32, is_uuid, native_address, pid_domain, private_root, process_start, wait_until

NAME = 'switchboard/remote-box/beta-chat-1a2b3c4d'
AUTHOR = "Switchboard on this machine wrote this, not {name}; don't reply to it.\n\n"
CHAIN = '0' * 24 + ',' + 'f' * 24


def recent_ms(value):
    return type(value) is int and abs(value / 1000 - time.time()) < 120


class HelperCase(unittest.TestCase):
    maxDiff = None

    def setUp(self):
        self.root = private_root(self)
        self.env = NativeEnv(self, self.root)
        self.s1 = self.env.session(name='alpha', status='idle', version='2.1.0', cwd='/tmp')

    def helper(self, sessions=None, **kw):
        helper = HelperProcess(self, self.env, sessions if sessions is not None else [self.s1.sid], name=kw.pop('name', NAME), **kw)
        helper.wait_ready()
        return helper

    def native_frame(self, session, timeout=10):
        event = session.wait_frame(timeout)
        self.assertEqual(event['event'], 'frame')
        self.assertTrue(event['auth_ok'], event)
        self.assertEqual(len(event['lines']), 2, event)
        self.assertTrue(event['raw'].endswith('\n'))
        return event['lines'][1]

    def proxy_attrs(self, helper, mode='prompting'):
        return {'from': native_address(helper.record['messagingSocketPath']), 'from-session': helper.record['sessionId'],
                'from-name': helper.name, 'from-mode': mode}

    def assertUserFrame(self, frame, helper, session, body, mode='prompting'):
        self.assertTrue(is_uuid(frame.get('msg_id')), frame)
        address = native_address(helper.record['messagingSocketPath'])
        self.assertEqual(frame, {'msgV': 1, 'msg_id': frame['msg_id'], 'type': 'user', 'session_id': session.sid,
                                 'message': {'role': 'user', 'content': envelope(self.proxy_attrs(helper, mode), body)},
                                 'priority': 'next', 'from': address})

    def deliver_and_handoff(self, helper, session=None, **kw):
        session = session or self.s1
        message = helper.deliver(session.sid, **kw)
        frame = self.native_frame(session)
        self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'handed_to_claude',
                                          'detail': 'native transport written'})
        return message, frame['msg_id']

    def outbound(self, helper, session=None, **kw):
        session = session or self.s1
        frame = session.user_frame(helper.record, **kw)
        session.send_frame(helper.record, frame)
        event = helper.event()
        self.assertEqual(event['event'], 'outbound', event)
        return frame['msg_id']

    def rejected(self, helper, detail):
        self.assertEqual(helper.event(), {'event': 'rejected', 'detail': detail})


class Identity(HelperCase):
    def test_ready_event_and_published_native_identity(self):
        helper = self.helper(status='busy')
        pid = helper.process.pid
        self.assertEqual(helper.ready, {'event': 'ready', 'pid': pid, 'sessionId': helper.ready['sessionId'],
                                        'route': helper.route})
        self.assertTrue(is_uuid(helper.ready['sessionId']))
        record_path = self.env.sessions / f'{pid}.json'
        record = json.loads(record_path.read_text())
        socket_path = self.env.sockets / f'{pid}.sock'
        for key in ('nameSince', 'startedAt'):
            self.assertTrue(recent_ms(record[key]), (key, record[key]))
        self.assertEqual({k: v for k, v in record.items() if k not in ('nameSince', 'startedAt')}, {
            'pid': pid, 'sessionId': helper.ready['sessionId'], 'procStart': process_start(pid), 'pidDomain': pid_domain(),
            'peerProtocol': 1, 'peerFeatures': [], 'kind': 'interactive', 'name': NAME, 'nameSource': 'user',
            'cwd': str(helper.state_dir.resolve()), 'messagingSocketPath': str(socket_path), 'peerBridgeProxy': True,
            'status': 'busy', 'version': '2.1.0'})
        key_path = self.env.key_for(record)
        key = json.loads(key_path.read_text())
        self.assertTrue(is_hex32(key['peerToken']))
        self.assertEqual(key, {'peerToken': key['peerToken'], 'procStart': record['procStart'], 'pidDomain': pid_domain()})
        ledger_path = helper.state_dir / f"proxy-{pid}-{record['sessionId']}.json"
        self.assertEqual(json.loads(ledger_path.read_text()), {
            'route': helper.route, 'registry': str(record_path), 'socket': str(socket_path), 'pid': pid,
            'procStart': record['procStart'], 'sessionId': record['sessionId']})
        for path in (record_path, key_path, ledger_path):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600, path)
        self.assertTrue(stat.S_ISSOCK(socket_path.stat().st_mode))
        helper.no_event(0.3)
        self.assertEqual(helper.terminate(), 0)
        for path in (record_path, key_path, ledger_path, socket_path):
            self.assertFalse(path.exists(), path)

    def test_status_argument_and_version_source(self):
        second = self.env.session(name='other', version='9.9.9')
        sessions = sorted([self.s1, second], key=lambda s: s.sid)
        helper = self.helper(sessions=[self.s1.sid, second.sid])
        self.assertEqual(helper.record['status'], 'idle')  # The default activity.
        self.assertEqual(helper.record['version'], sessions[0].record['version'])
        unknown = self.helper(status='unknown')
        self.assertNotIn('status', unknown.record)
        unversioned = self.env.session(name='plain')
        bare = self.helper(sessions=[unversioned.sid])
        self.assertNotIn('version', bare.record)

    def test_end_of_input_stops_the_helper_and_removes_its_files(self):
        helper = self.helper()
        pid = helper.process.pid
        helper.process.stdin.close()
        helper.process.wait(10)
        wait_until(lambda: not (self.env.sessions / f'{pid}.json').exists(), 5, message='registry removal')
        self.assertFalse((self.env.sockets / f'{pid}.sock').exists())
        self.assertEqual(list(helper.state_dir.glob('proxy-*.json')), [])

    def test_startup_refusals_leave_nothing_behind(self):
        before = sorted(p.name for p in self.env.sessions.iterdir())
        sockets = sorted(p.name for p in self.env.sockets.iterdir())
        cases = [dict(sessions=[str(uuid.uuid4())]), dict(enabled=False), dict(route='remote-box/not-a-uuid'),
                 dict(route='bad host!/' + str(uuid.uuid4())), dict(name='"<>'), dict(status='sleepy')]
        for case in cases:
            with self.subTest(case=case):
                sessions = case.pop('sessions', [self.s1.sid])
                helper = HelperProcess(self, self.env, sessions, name=case.pop('name', NAME), **case)
                self.assertNotEqual(helper.process.wait(15), 0)
                self.assertEqual([line for line in helper.output if b'"ready"' in line], [])
                self.assertEqual(sorted(p.name for p in self.env.sessions.iterdir()), before)
                self.assertEqual(sorted(p.name for p in self.env.sockets.iterdir()), sockets)


class Delivery(HelperCase):
    def test_deliver_writes_one_authenticated_native_frame(self):
        helper = self.helper()
        text = 'hello </cross-session-message> λ\nsecond line'
        message = helper.deliver(self.s1.sid, text=text, mode='bypass', hop_chain=CHAIN)
        event = self.s1.wait_frame()
        self.assertTrue(event['auth_ok'])
        self.assertEqual(event['lines'][0], {'type': 'auth', 'token': self.s1.token})
        frame = event['lines'][1]
        attrs = dict(self.proxy_attrs(helper, 'bypass'), **{'hop-chain': CHAIN})
        self.assertIn('<\\/cross-session-message>', frame['message']['content'])
        self.assertEqual(frame, {'msgV': 1, 'msg_id': frame['msg_id'], 'type': 'user', 'session_id': self.s1.sid,
                                 'message': {'role': 'user', 'content': envelope(attrs, text)}, 'priority': 'next',
                                 'from': attrs['from']})
        self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'handed_to_claude',
                                          'detail': 'native transport written'})
        self.assertNotEqual(frame['msg_id'], message)  # A fresh native ID, correlated privately.

    def test_deliver_refusals_report_failed_status(self):
        ghost = str(uuid.uuid4())
        other = self.env.session(name='unjoined')
        helper = self.helper(sessions=[self.s1.sid, ghost])
        cases = [
            (dict(target_session=other.sid), 'target session not enrolled'),
            (dict(text=''), 'invalid delivery text/mode'),
            (dict(text='x' * 8193), 'invalid delivery text/mode'),
            (dict(text=5), 'invalid delivery text/mode'),
            (dict(mode='auto'), 'invalid delivery text/mode'),
            (dict(target_session=ghost), 'target session no longer live'),
            (dict(hop_chain='zz'), 'invalid native hop chain'),
        ]
        for change, detail in cases:
            with self.subTest(detail=detail, change=str(change)[:40]):
                message = str(uuid.uuid4())
                command = dict(op='deliver', id=message, target_session=self.s1.sid, text='hi', mode='prompting',
                               sender_name='beta')
                command.update(change)
                helper.ipc(command)
                self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'failed', 'detail': detail})
        self.s1.no_frame(0.5)
        # The largest text is accepted; a repeated ID is never handed over twice.
        message = helper.deliver(self.s1.sid, text='é' * 4096)
        self.native_frame(self.s1)
        self.assertEqual(helper.event()['state'], 'handed_to_claude')
        helper.deliver(self.s1.sid, message_id=message)
        self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'failed',
                                          'detail': 'duplicate delivery; no replay'})
        self.s1.no_frame(0.5)
        # A malformed ID string still fails that ID; anything else is a rejected command.
        helper.ipc(dict(op='deliver', id='not-a-uuid', target_session=self.s1.sid, text='x', mode='prompting'))
        event = helper.event()
        self.assertEqual((event['event'], event['id'], event['state']), ('status', 'not-a-uuid', 'failed'))
        self.assertTrue(isinstance(event.get('detail'), str) and event['detail'])
        for raw in (json.dumps(dict(op='deliver', id=5)).encode() + b'\n', b'{"op": "launch"}\n', b'not json\n',
                    b'[1, 2]\n', b'{}\n', json.dumps(dict(op='receipt', id='x')).encode() + b'\n'):
            with self.subTest(raw=raw):
                helper.ipc_raw(raw)
                self.rejected(helper, 'invalid IPC command')

    def test_native_receipts_from_the_target_are_relayed_as_status(self):
        helper = self.helper()
        cases = [
            ('denied', {}, 'refused', 'native receipt: denied'),
            ('expired', {}, 'failed', 'native receipt: expired'),
            ('expired', {'status_detail': 'refused'}, 'refused', 'native receipt: refused'),
            ('dropped', {'drop_reason': 'duplicate'}, 'failed', 'native receipt: dropped; drop_reason=duplicate'),
            ('dropped', {'drop_reason': 'cosmic-rays'}, 'failed', 'native receipt: dropped'),
            ('dropped', {'drop_reason': 'rate-limited', 'dropped_msg_ids': []}, 'failed',
             'native receipt: dropped; drop_reason=rate-limited'),
        ]
        for status, extra, state, detail in cases:
            with self.subTest(status=status, extra=extra):
                message, native_id = self.deliver_and_handoff(helper)
                self.s1.send_frame(helper.record, self.s1.receipt_frame(native_id, status, **extra))
                self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': state, 'detail': detail})
                self.s1.send_frame(helper.record, self.s1.receipt_frame(native_id, status, **extra))
                self.rejected(helper, 'unmatched receipt identity')

    def test_held_then_released_and_batched_drops(self):
        helper = self.helper()
        message, native_id = self.deliver_and_handoff(helper)
        self.s1.send_frame(helper.record, self.s1.receipt_frame(native_id, 'held'))
        self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'held', 'detail': 'native receipt: held'})
        self.s1.send_frame(helper.record, self.s1.receipt_frame(native_id, 'delivered'))
        self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'handed_to_claude', 'detail': 'delivered'})
        first, second = self.deliver_and_handoff(helper), self.deliver_and_handoff(helper)
        frame = self.s1.receipt_frame(None, 'dropped', dropped_msg_ids=[second[1], first[1], second[1]],
                                      drop_reason='queue-full')
        del frame['orig_msg_id']
        self.s1.send_frame(helper.record, frame)
        detail = 'native receipt: dropped; drop_reason=queue-full'
        self.assertEqual([helper.event(), helper.event()], [
            {'event': 'status', 'id': second[0], 'state': 'failed', 'detail': detail},
            {'event': 'status', 'id': first[0], 'state': 'failed', 'detail': detail}])

    def test_malformed_or_foreign_receipts_are_rejected(self):
        helper = self.helper()
        message, native_id = self.deliver_and_handoff(helper)
        stranger = self.env.session(name='stranger')
        for frame, detail in [
            (self.s1.receipt_frame(native_id, 'read'), 'unsupported receipt status'),
            (dict(self.s1.receipt_frame(native_id, 'held'), **{'from': stranger.address}), 'receipt source mismatch'),
            (self.s1.receipt_frame(native_id, 'held', dropped_msg_ids=[native_id]), 'invalid native dropped message batch'),
            (self.s1.receipt_frame(str(uuid.uuid4()), 'held'), 'unmatched receipt identity'),
        ]:
            with self.subTest(detail=detail):
                self.s1.send_frame(helper.record, frame)
                self.rejected(helper, detail)
        self.s1.send_frame(helper.record, self.s1.receipt_frame(native_id, 'held'), token='0' * 32)
        self.rejected(helper, 'peer auth required')
        # Still correlated after all of that.
        self.s1.send_frame(helper.record, self.s1.receipt_frame(native_id, 'held'))
        self.assertEqual(helper.event()['state'], 'held')


class Outbound(HelperCase):
    def test_outbound_event_from_a_joined_session(self):
        helper = self.helper()
        text = 'question <\\/cross-session-message> x'
        frame = self.s1.user_frame(helper.record, text='question </cross-session-message> x', mode='bypass', hop_chain=CHAIN)
        self.assertIn(text, frame['message']['content'])
        self.s1.send_frame(helper.record, frame)
        self.assertEqual(helper.event(), {'event': 'outbound', 'native_id': frame['msg_id'], 'revision': 0,
                                          'source_session': self.s1.sid, 'text': 'question </cross-session-message> x',
                                          'mode': 'bypass', 'recipient': helper.route, 'hop_chain': CHAIN})
        plain = self.s1.user_frame(helper.record, text='plain')
        self.s1.send_frame(helper.record, plain)
        self.assertEqual(helper.event(), {'event': 'outbound', 'native_id': plain['msg_id'], 'revision': 0,
                                          'source_session': self.s1.sid, 'text': 'plain', 'mode': 'prompting',
                                          'recipient': helper.route})
        self.s1.send_frame(helper.record, plain)
        self.rejected(helper, 'duplicate native message ID')
        # An empty connection is a liveness probe: no event.
        self.s1.send_raw(helper.record['messagingSocketPath'], '')
        helper.no_event(0.5)

    def test_malformed_native_frames_are_rejected(self):
        helper = self.helper()
        address = self.s1.address
        stranger = self.env.session(name='stranger')
        good = lambda **kw: self.s1.user_frame(helper.record, **kw)
        cases = [
            (good(session_id=str(uuid.uuid4())), 'native protocol or target mismatch'),
            (good(msgV=2), 'native protocol or target mismatch'),
            (dict(good(), message={'role': 'user', 'content': envelope({'from': address, 'from-name': 'a'}, 'x')}),
             'missing sender mode or unsupported plugin provenance'),
            (dict(good(), message={'role': 'user', 'content': envelope(
                {'from': address, 'from-mode': 'prompting', 'from-plugin': 'p'}, 'x')}),
             'missing sender mode or unsupported plugin provenance'),
            (dict(good(), message={'role': 'user', 'content': 'bare text'}), 'native attribution envelope required'),
            (dict(good(), **{'from': stranger.address}), 'native sender address mismatch'),
            (dict(good(), type='control', action='interrupt'), 'unsupported native frame'),
            (dict(good(), message='text'), 'unsupported native frame'),
        ]
        for frame, detail in cases:
            with self.subTest(detail=detail):
                self.s1.send_frame(helper.record, frame)
                self.rejected(helper, detail)
        self.s1.send_frame(helper.record, good(), token='f' * 32)
        self.rejected(helper, 'peer auth required')
        self.assertEqual(self.s1.send_frame(helper.record, good())['ok'], True)
        self.assertEqual(helper.event()['event'], 'outbound')

    def test_relayed_receipts_reach_the_original_sender(self):
        helper = self.helper()
        address = native_address(helper.record['messagingSocketPath'])
        cases = [
            ('held', 'native receipt: held', {'status': 'held'}),
            ('handed_to_claude', 'delivered', {'status': 'delivered'}),
            ('refused', 'native receipt: refused', {'status': 'expired', 'status_detail': 'refused'}),
            ('refused', 'native receipt: denied', {'status': 'denied'}),
            ('failed', 'native receipt: expired', {'status': 'expired'}),
            ('failed', 'native receipt: dropped; drop_reason=hop-loop', {'status': 'dropped', 'drop_reason': 'hop-loop'}),
            ('failed', 'native receipt: dropped', {'status': 'dropped'}),
        ]
        for state, detail, expected in cases:
            with self.subTest(detail=detail):
                native_id = self.outbound(helper)
                helper.ipc(dict(op='receipt', id=native_id, state=state, detail=detail))
                frame = self.native_frame(self.s1)
                self.assertTrue(is_uuid(frame['msg_id']))
                value = {'msgV': 1, 'msg_id': frame['msg_id'], 'type': 'control', 'action': 'peer_message_status',
                         'orig_msg_id': native_id, 'session_id': self.s1.sid,
                         'reason': 'Switchboard remote receiver observation', 'from': address, **expected}
                if expected['status'] == 'dropped':
                    value['dropped_msg_ids'] = [native_id]
                self.assertEqual(frame, value)
                helper.no_event(0.2)
                if state != 'held':
                    helper.ipc(dict(op='receipt', id=native_id, state=state, detail=detail))
                    self.rejected(helper, 'invalid IPC command')
        native_id = self.outbound(helper)
        helper.ipc(dict(op='receipt', id=native_id, state='failed', detail='native receipt: held'))
        self.rejected(helper, 'invalid IPC command')
        helper.ipc(dict(op='receipt', id=str(uuid.uuid4()), state='held', detail='native receipt: held'))
        self.rejected(helper, 'invalid IPC command')
        self.s1.no_frame(0.5)


class Notices(HelperCase):
    def test_failed_and_refused_outcomes_send_one_plain_notice(self):
        helper = self.helper()
        long_text = 'word  ' * 30 + '\nend'
        native_id = self.outbound(helper, text=long_text, mode='bypass')
        helper.ipc(dict(op='receipt', id=native_id, state='failed', detail='the recipient chat is offline\tor  gone'))
        quoted = ' '.join(long_text.split())[:120] + '...'
        body = (AUTHOR.format(name=NAME) + f'Your message to {NAME} was not delivered (the recipient chat is offline or gone). '
                f'Your message began: "{quoted}"\n\nResend it later or tell the user.')
        self.assertUserFrame(self.native_frame(self.s1), helper, self.s1, body, mode='bypass')
        helper.ipc(dict(op='receipt', id=native_id, state='failed', detail='again'))
        self.rejected(helper, 'invalid IPC command')
        self.s1.no_frame(0.5)
        native_id = self.outbound(helper, text='short one')
        helper.ipc(dict(op='receipt', id=native_id, state='refused'))
        body = (AUTHOR.format(name=NAME) + f'Your message to {NAME} was not delivered (no reason was reported). '
                'Your message began: "short one"\n\nResend it later or tell the user.')
        self.assertUserFrame(self.native_frame(self.s1), helper, self.s1, body)

    def test_unknown_outcome_notice_is_sent_once_and_keeps_the_correlation(self):
        helper = self.helper()
        native_id = self.outbound(helper, text='is anyone there?')
        helper.ipc(dict(op='receipt', id=native_id, state='unknown', detail=''))
        body = (AUTHOR.format(name=NAME) + f"Switchboard couldn't confirm that your message to {NAME} arrived "
                '(no outcome was reported); it may or may not have reached that chat. Your message began: '
                '"is anyone there?"\n\nIf it matters, resend it once or tell the user.')
        self.assertUserFrame(self.native_frame(self.s1), helper, self.s1, body)
        helper.ipc(dict(op='receipt', id=native_id, state='unknown', detail='delivery not confirmed within 30 seconds'))
        helper.no_event(0.5)
        self.s1.no_frame(0.3)
        helper.ipc(dict(op='receipt', id=native_id, state='held', detail='native receipt: held'))
        self.assertEqual(self.native_frame(self.s1)['status'], 'held')

    def test_handoff_then_unknown_sends_nothing(self):
        helper = self.helper()
        native_id = self.outbound(helper)
        helper.ipc(dict(op='receipt', id=native_id, state='handed_to_claude', detail='native transport written'))
        helper.ipc(dict(op='receipt', id=native_id, state='unknown',
                        detail='native receipt correlation expired after 24 hours'))
        helper.ipc(dict(op='receipt', id=native_id, state='received'))
        helper.no_event(0.5)
        self.s1.no_frame(0.5)

    def test_track_adopts_a_message_sent_through_an_earlier_helper(self):
        other = self.env.session(name='unjoined')
        helper = self.helper()
        tracked = str(uuid.uuid4())
        helper.ipc(dict(op='track', id=tracked, source_session=self.s1.sid, text='sent before the restart', mode='bypass'))
        helper.no_event(0.3)
        helper.ipc(dict(op='receipt', id=tracked, state='unknown',
                        detail='Switchboard lost its hub connection during the send and could not resend it in time'))
        body = (AUTHOR.format(name=NAME) + f"Switchboard couldn't confirm that your message to {NAME} arrived "
                '(Switchboard lost its hub connection during the send and could not resend it in time); it may or '
                'may not have reached that chat. Your message began: "sent before the restart"\n\n'
                'If it matters, resend it once or tell the user.')
        self.assertUserFrame(self.native_frame(self.s1), helper, self.s1, body, mode='bypass')
        for command in (dict(op='track', id=str(uuid.uuid4()), source_session=other.sid, text='x', mode='bypass'),
                        dict(op='track', id=str(uuid.uuid4()), source_session=self.s1.sid, text='x', mode='auto'),
                        dict(op='track', id=str(uuid.uuid4()), source_session=self.s1.sid, text=5, mode='bypass'),
                        dict(op='track', id='nope', source_session=self.s1.sid, text='x', mode='bypass')):
            with self.subTest(command=command):
                helper.ipc(command)
                self.rejected(helper, 'invalid IPC command')


class Configuration(HelperCase):
    def test_configure_updates_name_status_and_selection_in_place(self):
        second = self.env.session(name='second')
        helper = self.helper()
        before = helper.proxy_record()
        key_before = self.env.key_for(before).read_bytes()
        time.sleep(0.01)
        helper.ipc(dict(op='configure', revision=1, name='switchboard/remote-box/renamed-1a2b3c4d',
                        sessions=[self.s1.sid, second.sid], status='waiting'))
        self.assertEqual(helper.event(), {'event': 'configured', 'revision': 1})
        after = helper.proxy_record()
        self.assertEqual(after['name'], 'switchboard/remote-box/renamed-1a2b3c4d')
        self.assertEqual(after['status'], 'waiting')
        self.assertGreaterEqual(after['nameSince'], before['nameSince'])
        unchanged = {k: v for k, v in before.items() if k not in ('name', 'status', 'nameSince')}
        self.assertEqual({k: v for k, v in after.items() if k not in ('name', 'status', 'nameSince')}, unchanged)
        self.assertEqual(self.env.key_for(after).read_bytes(), key_before)
        helper.name = after['name']
        # An omitted status keeps the current one; unknown removes it.
        helper.ipc(dict(op='configure', revision=2, name=helper.name, sessions=[self.s1.sid, second.sid]))
        self.assertEqual(helper.event(), {'event': 'configured', 'revision': 2})
        self.assertEqual(helper.proxy_record()['status'], 'waiting')
        helper.ipc(dict(op='configure', revision=3, name=helper.name, sessions=[self.s1.sid, second.sid], status='unknown'))
        self.assertEqual(helper.event(), {'event': 'configured', 'revision': 3})
        self.assertNotIn('status', helper.proxy_record())
        # Outbound events carry the revision they were authorized under; the new session may send.
        frame = second.user_frame(helper.record, text='from second')
        second.send_frame(helper.record, frame)
        event = helper.event()
        self.assertEqual((event['event'], event['revision'], event['source_session']), ('outbound', 3, second.sid))
        message, _ = self.deliver_and_handoff(helper, session=second)
        self.assertTrue(is_uuid(message))

    def test_configure_rejections_do_not_consume_the_revision(self):
        ghost = str(uuid.uuid4())
        helper = self.helper()
        original = (self.env.sessions / f"{helper.ready['pid']}.json").read_bytes()
        good = dict(op='configure', revision=1, name=NAME, sessions=[self.s1.sid])
        cases = [
            (dict(good, revision=0), 'invalid configuration revision'),
            (dict(good, revision='1'), 'invalid configuration revision'),
            (dict(good, revision=True), 'invalid configuration revision'),
            (dict(good, name='bad "name"'), 'invalid native name'),
            (dict(good, name=''), 'invalid native name'),
            (dict(good, name=' padded'), 'invalid native name'),
            (dict(good, status='sleepy'), 'invalid remote activity'),
            (dict(good, status=None), 'invalid remote activity'),
            (dict(good, sessions='all'), 'invalid selection list'),
            (dict(good, sessions=[str(uuid.uuid4()) for _ in range(33)]), 'invalid selection list'),
            (dict(good, sessions=[self.s1.sid, self.s1.sid]), 'duplicate selected UUID'),
            (dict(good, sessions=[ghost]), 'selection requires helper replacement'),
            (dict(good, sessions=[]), 'selection requires helper replacement'),
        ]
        for command, detail in cases:
            with self.subTest(detail=detail, command=str(command)[:70]):
                helper.ipc(command)
                self.assertEqual(helper.event(), {'event': 'configure_rejected', 'revision': command['revision'],
                                                  'detail': detail})
        helper.ipc(dict(good, sessions=['not-a-uuid']))
        event = helper.event()
        self.assertEqual((event['event'], event['revision']), ('configure_rejected', 1))
        self.assertTrue(event['detail'])
        self.assertEqual((self.env.sessions / f"{helper.ready['pid']}.json").read_bytes(), original)
        helper.ipc(good)
        self.assertEqual(helper.event(), {'event': 'configured', 'revision': 1})
        helper.ipc(good)
        self.assertEqual(helper.event(), {'event': 'configure_rejected', 'revision': 1,
                                          'detail': 'stale configuration revision'})

    def test_removed_sessions_lose_access_and_get_the_join_notice(self):
        second = self.env.session(name='second')
        helper = self.helper(sessions=[self.s1.sid, second.sid])
        helper.ipc(dict(op='configure', revision=1, name=NAME, sessions=[second.sid]))
        self.assertEqual(helper.event(), {'event': 'configured', 'revision': 1})
        self.s1.send_frame(helper.record, self.s1.user_frame(helper.record, text='still here?'))
        self.rejected(helper, 'sender has not joined Switchboard; told it how to join')
        self.assertUserFrame(self.native_frame(self.s1), helper, self.s1, AUTHOR.format(name=NAME) +
                             f"Your message to {NAME} was not delivered: this chat hasn't joined Switchboard.\n\n"
                             'Ask the user to run /switchboard join (or !switchboard join), then resend it.')
        message = helper.deliver(self.s1.sid)
        self.assertEqual(helper.event(), {'event': 'status', 'id': message, 'state': 'failed',
                                          'detail': 'target session not enrolled'})


class Unjoined(HelperCase):
    def test_unjoined_sender_gets_one_join_notice_per_minute(self):
        other = self.env.session(name='unjoined')
        helper = self.helper()
        other.send_frame(helper.record, other.user_frame(helper.record, text='hi remote', mode='bypass'))
        self.rejected(helper, 'sender has not joined Switchboard; told it how to join')
        body = (AUTHOR.format(name=NAME) + f"Your message to {NAME} was not delivered: this chat hasn't joined "
                'Switchboard.\n\nAsk the user to run /switchboard join (or !switchboard join), then resend it.')
        self.assertUserFrame(self.native_frame(other), helper, other, body, mode='bypass')
        other.send_frame(helper.record, other.user_frame(helper.record, text='hello?'))
        self.rejected(helper, 'sender has not joined Switchboard; told it how to join')
        other.no_frame(1.0)
        # A receipt from an unjoined session is not a message and gets no notice.
        other.send_frame(helper.record, other.receipt_frame(str(uuid.uuid4()), 'held'))
        self.rejected(helper, 'session not enrolled')
        self.s1.no_frame(0.2)
