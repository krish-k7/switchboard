"""CLI: --json outputs, exit codes, and the files it changes, against a real server.

The CLI runs with a fake HOME (so it cannot read real Switchboard state), a fake
launchctl first on PATH (so nothing queries launchd), and a private bridge
config whose native Claude registry holds fake live sessions.
"""
import json
import os
from pathlib import Path
import secrets
import stat
import sys
import time
import unittest
import uuid

from sbcompat import pki
from sbcompat.claude import NativeEnv
from sbcompat.cli import CLIEnv
from sbcompat.impl import slow
from sbcompat.server import Client, Poller, ServerProcess, tls_machine
from sbcompat.util import is_time, private_dir, private_root, scrub, write_private

WEEK = 7 * 86400


UNREACHABLE = ("Chats on other machines are unavailable: can't connect to the server, or its certificate isn't the one "
               'this machine trusts.')


class CLICase(unittest.TestCase):
    maxDiff = None

    def setUp(self):
        self.root = private_root(self)
        self.machine_a = tls_machine('machine-a', owner='alice', admin=True)
        self.machine_b = tls_machine('machine-b', owner='bob')
        self.machine_c = tls_machine('machine-c', owner='ops')
        self.server = ServerProcess(self, self.root / 'server', 'tls', [self.machine_a, self.machine_b, self.machine_c])
        self.server.start()
        self.native = NativeEnv(self, self.root / 'native')
        repository = private_dir(self.root / 'work' / 'example' / '.git')
        (repository / 'HEAD').write_text('ref: refs/heads/main\n')
        project = private_dir(self.root / 'work' / 'example' / 'packages' / 'solver')
        plain = private_dir(self.root / 'plain')
        self.s1 = self.native.session(name='Release Planner', status='busy', cwd=str(project))
        self.s2 = self.native.session(name='idle chat', status='idle', cwd=str(plain))
        self.cli = CLIEnv(self, self.root / 'cli', self.native, self.server, self.machine_a)

    def run_json(self, *arguments, code=0, stream='stdout', **kw):
        result = self.cli.run(*arguments, '--json', **kw)
        self.assertEqual(result.code, code, result)
        return json.loads(result.stdout if stream == 'stdout' else result.stderr)

    def assertJSONError(self, arguments, code, error, **kw):
        """With --json, errors are one JSON object on stdout and nothing on stderr."""
        result = self.cli.run(*arguments, '--json', **kw)
        self.assertEqual(result.code, code, result)
        self.assertEqual(result.stderr, '', result)
        value = json.loads(result.stdout)
        if error is None:
            self.assertEqual(value['ok'], False)
            self.assertTrue(isinstance(value['error'], str) and value['error'])
        else:
            self.assertEqual(value, {'ok': False, 'error': error})
        return result

    def rows(self, value):
        return sorted(value, key=lambda row: row['id'])

    def remote(self, machine=None, sessions=(), capabilities=None):
        client = Client(self.server, machine or self.machine_b)
        client.register(list(sessions), capabilities)
        return client


class Help(CLICase):
    def test_help_and_parser_errors(self):
        plain = self.cli.run('help')
        self.assertEqual(plain.code, 0)
        self.assertTrue(plain.stdout.startswith('Usage: switchboard COMMAND [options]\n'), plain.stdout)
        self.assertIn('\n  join [NAME]        Join this chat, so chats on other machines can message it', plain.stdout)
        self.assertIn('every command takes --json', plain.stdout)
        self.assertNotIn('setup', plain.stdout)
        self.assertNotIn('hub', plain.stdout)
        self.assertEqual(self.cli.run().stdout, plain.stdout)
        value = self.run_json('help')
        self.assertEqual(value, {'ok': True, 'help': plain.stdout})
        cases = [
            (['status', '--local'], '--local, --remote, --name, and --project only work with chats'),
            (['status', '--name', 'x'], '--local, --remote, --name, and --project only work with chats'),
            (['status', '--machine', 'x'], '--machine only works with chats'),
            (['status', '--owner', 'x'], '--owner, --no-start, and --invite only work with init'),
            (['status', '--no-start'], '--owner, --no-start, and --invite only work with init'),
            (['status', '--invite', 'x'], '--owner, --no-start, and --invite only work with init'),
            (['status', '--server-url', 'x'], '--server-url and --server-pin only work with setup'),
            (['doctor', '--session', 'x'], '--chat only works with join, leave, status, and chats'),
            (['ping', '--chat', 'x'], '--chat only works with join, leave, status, and chats'),
            (['status', 'extra'], 'status takes no argument (unexpected extra)'),
            (['status', '--bogus'], 'unknown option --bogus'),
            (['status', '--conf'], 'unknown option --conf'),
            (['status', '--config'], '--config needs a value'),
            (['status', '--json=yes'], '--json takes no value'),
            (['status', 'a', 'b'], 'unexpected argument b'),
            (['statsu'], "unknown command 'statsu'; did you mean status? Run switchboard help for the commands"),
            (['join', '!!!'], 'the name needs at least one letter or digit'),
            (['xyzzy'], "unknown command 'xyzzy'; run switchboard help for the commands"),
            (['sessions', '--local', '--remote'], "--local and --remote can't be used together"),
            (['chats', '--local', '--remote'], "--local and --remote can't be used together"),
            (['sesions'], "unknown command 'sesions'; did you mean chats? Run switchboard help for the commands"),
        ]
        for arguments, error in cases:
            with self.subTest(arguments=arguments):
                self.assertJSONError(arguments, 2, error)
        result = self.cli.run('status', '--local')
        self.assertEqual((result.code, result.stdout),
                         (2, ''))
        self.assertEqual(result.stderr, '✗ --local, --remote, --name, and --project only work with chats.\n'
                                        '  To fix, see switchboard status --help for the options.\n')
        # Each command has its own help; sessions and setup still work, out of the overview.
        self.assertTrue(self.cli.run('uninstall', '--help').stdout.startswith('Usage: switchboard uninstall'))
        self.assertEqual(self.cli.run('sessions', '--help').stdout, self.cli.run('chats', '--help').stdout)
        self.assertEqual(self.run_json('chats', '--local'), self.run_json('sessions', '--local'))


class Status(CLICase):
    def setUp(self):
        super().setUp()
        self.joined = round(time.time()) - 3600
        config = dict(self.cli.config, sessions=[self.s1.sid], joined_at={self.s1.sid: self.joined},
                      names={self.s1.sid: 'custom'})
        self.cli.write_config(config)

    def expected_rows(self, expires=None, metadata=False):
        s1 = dict(id=self.s1.sid, name='Release Planner', status='busy', enrolled=True, joined_name='custom',
                  expires_at=expires if expires is not None else self.joined + WEEK)
        s2 = dict(id=self.s2.sid, name='idle chat', status='idle', enrolled=False)
        if metadata:
            s1.update(project='solver', repository='example', branch='main')
            s2.update(project='plain')
        return self.rows([s1, s2])

    def test_status_without_a_running_bridge(self):
        # Proxy records written by helpers are never listed as local chats.
        self.native.session(name='switchboard/x/y-12345678', peerBridgeProxy=True)
        value = self.run_json('status')
        self.assertEqual(value.pop('detail'), 'the bridge is not running')
        self.assertEqual(dict(value, sessions=self.rows(value['sessions'])),
                         {'enabled': True, 'sessions': self.expected_rows(), 'connected': False,
                          'server_url': self.cli.config['hub_url']})

    def test_status_with_a_live_bridge_runtime(self):
        runtime = self.cli.runtime(connected=True, machine='machine-a')
        value = self.run_json('status')
        self.assertEqual(dict(value, sessions=self.rows(value['sessions'])),
                         {'enabled': True, 'sessions': self.expected_rows(), 'connected': True,
                          'machine': 'machine-a', 'pid': runtime['pid'], 'server_url': self.cli.config['hub_url']})
        human = self.cli.run('status')
        self.assertEqual(human.stdout, f'✓ Connected to {self.cli.config["hub_url"].removeprefix("https://")} as '
                                       'machine-a\n  Chats here  1 of 2 joined\n')
        # Given a chat (or run inside one), status answers for it too.
        human = self.cli.run('status', '--chat', self.s1.sid)
        self.assertIn('\n  This chat   Joined as custom, expires in 7 days\n', human.stdout)
        self.assertEqual(self.run_json('status', '--chat', self.s1.sid), value)
        write_private(self.cli.state / 'activity.json', {self.s1.sid: self.joined + 600})
        value = self.run_json('status')
        self.assertEqual(self.rows(value['sessions']), self.expected_rows(expires=self.joined + 600 + WEEK))
        self.cli.write_config(dict(self.cli.read_config(), enabled=False))
        value = self.run_json('status')
        self.assertEqual((value['enabled'], value['connected']), (False, False))

    def test_status_rejects_stale_or_unsafe_runtime(self):
        self.cli.runtime(procStart='Thu Jan  1 00:00:00 1970')
        self.assertEqual(self.run_json('status')['detail'], 'the runtime file is out of date (its bridge process has exited)')
        self.cli.runtime()
        os.chmod(self.cli.state / 'runtime.json', 0o644)
        self.assertEqual(self.run_json('status')['detail'], 'runtime file must have mode 0600')
        self.cli.runtime(pid=1)
        self.assertEqual(self.run_json('status')['detail'], 'the runtime file has an invalid bridge PID')

    def test_missing_config_is_a_json_error(self):
        absent = self.root / 'absent.json'
        self.assertJSONError(['status', '--config', str(absent)], 1,
                             "Switchboard isn't set up on this machine; run switchboard init")
        human = self.cli.run('status', '--config', str(absent))
        self.assertEqual((human.code, human.stdout, human.stderr), (1, '  Not set up. Run switchboard init.\n', ''))

    def test_server_url_is_read_and_hub_url_still_works(self):
        """Configs written by current versions name the server `server_url`; older ones `hub_url`."""
        b1 = str(uuid.uuid4())
        self.remote(self.machine_b, [dict(id=b1, name='beta-chat', status='idle')])
        config = dict(self.cli.config)
        config['server_url'] = config.pop('hub_url')
        self.cli.write_config(config)
        value = self.run_json('sessions', '--remote')
        self.assertEqual((value['remote_available'], [r['id'] for r in value['remote_sessions']]), (True, [b1]))
        # server_url wins over a stale hub_url.
        self.cli.write_config(dict(config, hub_url='https://127.0.0.1:1'))
        self.assertEqual(self.run_json('sessions', '--remote')['remote_available'], True)

    def test_local_sessions_with_metadata_and_filters(self):
        value = self.run_json('sessions', '--local')
        self.assertEqual(set(value), {'enabled', 'sessions'})
        self.assertEqual(self.rows(value['sessions']), self.expected_rows(metadata=True))
        self.assertEqual([r['id'] for r in self.run_json('sessions', '--local', '--name', 'RELEASE')['sessions']], [self.s1.sid])
        self.assertEqual([r['id'] for r in self.run_json('sessions', '--local', '--project', 'examp')['sessions']], [self.s1.sid])
        self.assertEqual(len(self.run_json('sessions', '--local', '--machine', 'loc')['sessions']), 2)
        self.assertEqual(self.run_json('sessions', '--local', '--name', 'nothing'), {'enabled': True, 'sessions': []})
        # Search words match names, projects, and the like; --details adds full IDs.
        self.assertEqual([r['id'] for r in self.run_json('chats', '--local', 'RELEASE main')['sessions']], [self.s1.sid])
        self.assertEqual(self.run_json('chats', '--local', 'release nothing')['sessions'], [])
        human = self.cli.run('chats', '--local', '--details')
        self.assertIn(f'  ID          {self.s1.sid}\n', human.stdout)
        self.assertNotIn(self.s1.sid, self.cli.run('chats', '--local').stdout)

    def test_sessions_with_the_remote_roster(self):
        b1, b2, a1 = str(uuid.uuid4()), str(uuid.uuid4()), str(uuid.uuid4())
        self.remote(self.machine_b, [dict(id=b1, name='beta-chat', status='busy', project='solver', branch='dev'),
                                     dict(id=b2, name='second', status='waiting')])
        self.remote(self.machine_a, [dict(id=a1, name='mine', status='idle')])
        value = self.run_json('sessions')
        remote = sorted(value.pop('remote_sessions'), key=lambda row: row['id'])
        self.assertEqual(remote, sorted([
            dict(id=b1, machine='machine-b', name='beta-chat', status='busy', owner='bob', route='machine-b/' + b1,
                 project='solver', branch='dev'),
            dict(id=b2, machine='machine-b', name='second', status='waiting', owner='bob', route='machine-b/' + b2)],
            key=lambda row: row['id']))
        self.assertEqual(dict(value, sessions=self.rows(value['sessions'])),
                         {'enabled': True, 'sessions': self.expected_rows(metadata=True), 'remote_available': True,
                          'machine': 'machine-a'})
        value = self.run_json('sessions', '--remote', '--name', 'beta')
        self.assertEqual([r['id'] for r in value['remote_sessions']], [b1])
        self.assertEqual(value['sessions'], [])
        value = self.run_json('sessions', '--machine', 'machine-b')
        self.assertEqual((len(value['remote_sessions']), value['sessions']), (2, []))

    def test_sessions_when_the_server_is_unreachable_or_refuses(self):
        self.server.stop()
        error = UNREACHABLE
        value = self.run_json('sessions', '--remote', code=1)
        self.assertEqual(value, {'enabled': True, 'sessions': [], 'remote_available': False, 'remote_sessions': [],
                                 'remote_error': error})
        value = self.run_json('sessions')
        self.assertEqual((value['remote_available'], value['remote_error'], len(value['sessions'])), (False, error, 2))
        self.server.start()
        self.server.write_registry({self.machine_b.fp: self.machine_b.entry()})
        value = self.run_json('sessions', '--remote', code=1)
        self.assertEqual(value['remote_error'],
                         "Chats on other machines are unavailable: the server doesn't accept this machine's key.")
        self.cli.write_config(dict(self.cli.read_config(), cert=str(self.root / 'missing.pem')))
        value = self.run_json('sessions', '--remote', code=1)
        self.assertEqual(value['remote_error'],
                         "Chats on other machines are unavailable: this machine's configuration or key files are "
                         'missing or unsafe.')


class JoinLeave(CLICase):
    NOT_RUNNING = 'the bridge is not running (switchboard up)'

    def membership(self):
        path = self.cli.config_path.parent / 'membership.json'
        raw = path.read_text()
        value = json.loads(raw)
        self.assertEqual(raw, json.dumps(value, indent=2, sort_keys=True) + '\n')
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        return value

    def test_join_with_a_name_then_leave(self):
        before = self.cli.read_config()
        value = self.run_json('join', '--session', self.s1.sid, 'release-planner')
        self.assertEqual(value, {'ok': True, 'action': 'join', 'session': self.s1.sid, 'name': 'release-planner',
                                 'confirmed': False, 'detail': self.NOT_RUNNING})
        config = self.cli.read_config()
        self.assertTrue(is_time(config['joined_at'][self.s1.sid]))
        self.assertIsInstance(config['joined_at'][self.s1.sid], int)
        self.assertEqual(config, dict(before, sessions=[self.s1.sid], joined_at=config['joined_at'],
                                      names={self.s1.sid: 'release-planner'}))
        self.assertEqual(stat.S_IMODE(self.cli.config_path.stat().st_mode), 0o600)
        self.assertEqual(self.membership(), {'version': 1, 'sessions': {self.s1.sid: {'name': 'release-planner'}}})
        value = self.run_json('join', '--session', self.s2.sid, 'Héllo World!!')
        self.assertEqual(value['name'], 'H-llo-World')
        self.assertEqual(self.cli.read_config()['sessions'], sorted([self.s1.sid, self.s2.sid]))
        # Joining again without a name drops the chosen name; peers see the chat's own name.
        value = self.run_json('join', '--session', self.s1.sid)
        self.assertEqual(value['name'], 'Release-Planner')
        self.assertEqual(self.cli.read_config()['names'], {self.s2.sid: 'H-llo-World'})
        self.assertEqual(self.membership()['sessions'], {self.s1.sid: {'name': 'Release-Planner'},
                                                         self.s2.sid: {'name': 'H-llo-World'}})
        # Leaving reports the name peers saw: the chosen one here.
        value = self.run_json('leave', '--session', self.s2.sid)
        self.assertEqual(value, {'ok': True, 'action': 'leave', 'session': self.s2.sid, 'name': 'H-llo-World',
                                 'confirmed': False, 'detail': self.NOT_RUNNING})
        config = self.cli.read_config()
        self.assertEqual(config['sessions'], [self.s1.sid])
        self.assertNotIn('names', config)
        self.assertEqual(set(config['joined_at']), {self.s1.sid})
        self.assertEqual(self.membership()['sessions'], {self.s1.sid: {'name': 'Release-Planner'}})
        # A chat that is no longer live can be left by UUID; it has no name to report.
        # A closed chat that is still joined can be left by UUID; it has no name to report.
        closed = str(uuid.uuid4())
        self.cli.write_config(dict(self.cli.read_config(), sessions=sorted([self.s1.sid, closed])))
        value = self.run_json('leave', '--session', closed)
        self.assertEqual((value['session'], value['name']), (closed, ''))
        self.assertEqual(self.cli.read_config()['sessions'], [self.s1.sid])
        # An ID no chat here has is an error, not a confirmation.
        ghost = str(uuid.uuid4())
        self.assertJSONError(['leave', '--session', ghost], 1,
                             f'no chat is joined on this machine with the ID {ghost}; run switchboard chats to find its ID')
        human = self.cli.run('leave', '--chat', ghost)
        self.assertEqual((human.code, human.stderr),
                         (1, f'✗ No chat with the ID {ghost} is joined on this\n  machine.\n'
                             '  To fix, run switchboard chats to find its ID.\n'))

    def test_join_identifies_the_chat_from_its_socket(self):
        value = self.run_json('join', env={'CLAUDE_CODE_MESSAGING_SOCKET': 'uds:' + self.s2.socket})
        self.assertEqual((value['session'], value['name']), (self.s2.sid, 'idle-chat'))
        before = self.cli.read_config()
        self.assertJSONError(['join'], 1, "this isn't a Claude Code chat; run it inside the chat, or pass --chat with "
                                          "a chat's ID (from switchboard chats)")
        human = self.cli.run('join')
        self.assertEqual(human.stderr, '✗ switchboard join connects the chat it runs in, and this is a terminal.\n'
                                       '  To fix, run /switchboard join inside the chat, or pick a chat from\n'
                                       '  switchboard chats and run\n'
                                       '      switchboard join --chat ID\n')
        for chat in (str(uuid.uuid4()), 'nope'):
            for option in ('--session', '--chat'):
                with self.subTest(chat=chat, option=option):
                    self.assertJSONError(['join', option, chat], 1,
                                         f'no open chat has the ID {chat}; run switchboard chats to find its full ID')
        self.assertEqual(self.cli.read_config(), before)

    def test_the_join_cap_ignores_expired_chats(self):
        now = round(time.time())
        others = sorted(str(uuid.uuid4()) for _ in range(32))
        self.cli.write_config(dict(self.cli.config, sessions=others, joined_at={sid: now for sid in others}))
        self.assertJSONError(['join', '--session', self.s1.sid], 1, 'at most 32 chats may be joined; leave another chat first')
        self.assertEqual(self.cli.read_config()['sessions'], others)
        stale = dict.fromkeys(others, now)
        stale[others[0]] = now - WEEK - 60
        self.cli.write_config(dict(self.cli.config, sessions=others, joined_at=stale, names={others[0]: 'old'}))
        self.assertTrue(self.run_json('join', '--session', self.s1.sid)['ok'])
        config = self.cli.read_config()
        self.assertEqual(config['sessions'], sorted(others[1:] + [self.s1.sid]))
        self.assertNotIn('names', config)
        self.assertNotIn(others[0], config['joined_at'])

    def test_join_and_leave_confirmed_by_the_running_bridge(self):
        # Confirmed only once the bridge publishes the chat under the name just set.
        self.cli.runtime(published=[dict(id=self.s1.sid, name='renamed')], remote=dict(sessions=3, machines=2))
        value = self.run_json('join', '--session', self.s1.sid, 'renamed')
        self.assertEqual(value, {'ok': True, 'action': 'join', 'session': self.s1.sid, 'name': 'renamed',
                                 'confirmed': True, 'remote_sessions': 3, 'remote_machines': 2})
        human = self.cli.run('join', '--session', self.s1.sid, 'renamed')
        self.assertEqual((human.code, human.stdout), (0, '✓ Joined as renamed.\n  Reachable by 3 chats on 2 other '
                                                         'machines.\n'))
        # Leaving a live chat that isn't joined changes nothing, and says so.
        value = self.run_json('leave', '--session', self.s2.sid)
        self.assertEqual(value, {'ok': True, 'action': 'leave', 'session': self.s2.sid, 'name': 'idle-chat',
                                 'changed': False})
        human = self.cli.run('leave', '--session', self.s2.sid)
        self.assertEqual(human.stdout, "  This chat wasn't joined; nothing changed.\n")
        # Without a chosen name, leave reports the chat's own name as other machines saw it.
        self.run_json('join', '--session', self.s2.sid)
        value = self.run_json('leave', '--session', self.s2.sid)
        self.assertEqual(value, {'ok': True, 'action': 'leave', 'session': self.s2.sid, 'name': 'idle-chat',
                                 'confirmed': True, 'remote_sessions': 3, 'remote_machines': 2})
        self.run_json('join', '--session', self.s2.sid)
        human = self.cli.run('leave', '--session', self.s2.sid)
        self.assertEqual(human.stdout, '✓ This chat left.\n  Other machines can no longer see it.\n')
        # A rename the bridge still publishes under the old name is not confirmed, and says what
        # other machines see.
        value = self.run_json('join', '--session', self.s1.sid, 'second')
        self.assertEqual(value, {'ok': True, 'action': 'join', 'session': self.s1.sid, 'name': 'second',
                                 'confirmed': False, 'published_name': 'renamed',
                                 'detail': 'the bridge has not published the new name within 10 seconds'})

    def test_join_reports_why_the_bridge_did_not_confirm(self):
        self.cli.runtime(connected=False)
        started = time.monotonic()
        value = self.run_json('join', '--session', self.s1.sid)
        self.assertTrue(2.5 < time.monotonic() - started < 8)
        self.assertEqual(value['detail'], 'the bridge is offline, so other machines cannot see changes until it reconnects')
        runtime = self.cli.runtime()
        del runtime['published']
        write_private(self.cli.state / 'runtime.json', runtime)
        self.assertEqual(self.run_json('join', '--session', self.s1.sid)['detail'],
                         'the running bridge is too old to confirm; restart it with switchboard down, then switchboard up')
        self.cli.write_config(dict(self.cli.read_config(), enabled=False))
        value = self.run_json('join', '--session', self.s1.sid)
        self.assertEqual((value['confirmed'], value['detail']), (False, 'the bridge is disabled in its configuration'))
        human = self.cli.run('join', '--session', self.s1.sid)
        self.assertEqual(human.stdout, '! Joined as Release-Planner, but not live yet.\n  The bridge is disabled in its '
                                       'configuration.\n\n  If it stays this way, run\n      switchboard doctor\n')

    @slow
    def test_join_gives_up_after_ten_seconds(self):
        self.cli.runtime(published=[])
        started = time.monotonic()
        value = self.run_json('join', '--session', self.s1.sid)
        self.assertTrue(9.5 < time.monotonic() - started < 15)
        self.assertEqual(value['detail'], 'the bridge has not published this change within 10 seconds')


class Ping(CLICase):
    def setUp(self):
        super().setUp()
        self.b1 = str(uuid.uuid4())

    def answer(self, live=True, capabilities=('ping', 'ack'), sessions=None):
        client = self.remote(self.machine_b, sessions or [dict(id=self.b1, name='beta-chat', status='idle')],
                             list(capabilities))
        poller = Poller(client, ack='ack' in capabilities, pong=live).start()
        self.addCleanup(poller.stop)
        return client

    def test_ping_success_by_every_target_form(self):
        self.answer()
        alias = f'switchboard/machine-b/beta-chat-{self.b1[:8]}'
        for target in ('beta-chat', 'BETA-CHAT', alias, alias.removeprefix('switchboard/'), 'machine-b/beta-chat',
                       'machine-b/' + self.b1, self.b1[:6]):
            with self.subTest(target=target):
                value = self.run_json('ping', target)
                self.assertEqual(scrub(self, value, rtt_ms=lambda v: type(v) is int and v >= 0),
                                 {'ok': True, 'target': alias, 'machine': 'machine-b', 'name': 'beta-chat',
                                  'rtt_ms': '<rtt_ms>', 'via': 'server'})
        human = self.cli.run('ping', 'beta-chat')
        self.assertEqual(human.code, 0)
        self.assertRegex(human.stdout, r'^✓ machine-b/beta-chat replied in \d+ ms\.\n$')

    def test_ping_refusals(self):
        twin1, twin2 = sorted(str(uuid.uuid4()) for _ in range(2))
        self.answer(live=False, sessions=[dict(id=self.b1, name='beta-chat', status='idle'),
                                          dict(id=twin1, name='twin', status='idle'),
                                          dict(id=twin2, name='twin', status='busy')])
        cases = [
            (['ping'], 'name a chat on another machine to ping, for example switchboard ping research'),
            (['ping', 'zzz'], "no chat on another machine matches 'zzz'; run switchboard chats --remote"),
            (['ping', 'abc'], "no chat on another machine matches 'abc'; run switchboard chats --remote"),
            (['ping', 'twin'], f"'twin' matches 2 chats (switchboard/machine-b/twin-{twin1[:8]}, "
                               f"switchboard/machine-b/twin-{twin2[:8]}); use a full alias or UUID"),
            (['ping', 'beta-chat'], 'machine-b answered, but that chat is no longer joined there'),
        ]
        for arguments, error in cases:
            with self.subTest(arguments=arguments):
                self.assertJSONError(arguments, 1, error)
        human = self.cli.run('ping', 'zzz')
        self.assertEqual((human.code, human.stdout), (1, ''))
        self.assertEqual(human.stderr, "✗ No reply from zzz.\n"
                                       "  No chat on another machine matches 'zzz'.\n"
                                       '  To fix, run\n'
                                       '      switchboard chats --remote\n')

    def test_ping_to_an_old_bridge_or_without_a_server(self):
        self.remote(self.machine_b, [dict(id=self.b1, name='beta-chat', status='idle')])
        self.assertJSONError(['ping', 'beta-chat'], 1, 'the remote Switchboard is too old to answer ping')
        self.server.stop()
        self.assertJSONError(['ping', 'beta-chat'], 1, UNREACHABLE)

    def test_ping_timeout(self):
        self.answer(live=None)  # Polls but never answers.
        self.assertJSONError(['ping', 'beta-chat'], 1, 'no reply within 5 seconds')


class Machines(CLICase):
    def test_approval_commands_are_gone(self):
        """Machines join with invites made on the server; the approval, listing, and
        revocation commands were removed from the client, with their options."""
        for arguments in (['approve', 'ABCD-EFGH'], ['refuse', 'ABCD-EFGH'], ['revoke', 'machine-c'], ['machines']):
            with self.subTest(arguments=arguments):
                result = self.assertJSONError(arguments, 2, None)
                self.assertTrue(json.loads(result.stdout)['error'].startswith(f"unknown command '{arguments[0]}'"),
                                result.stdout)
        for arguments, option in ((['setup', '--wait', '0'], '--wait'), (['setup', '--yes'], '--yes'),
                                  (['setup', '--token', 'x'], '--token')):
            with self.subTest(arguments=arguments):
                self.assertJSONError(arguments, 2, f'unknown option {option}')
        help_text = self.cli.run('help').stdout
        for word in ('approve', 'refuse', 'revoke', 'administrator', '--token', '--wait', '--yes'):
            self.assertNotIn(word, help_text)
        self.assertIn('--invite', self.cli.run('setup', '--help').stdout)
        self.assertIn('--invite', self.cli.run('init', '--help').stdout)
        self.assertEqual(self.server.request('GET', '/health', machine=self.machine_c).status, 200)

    def test_setup_without_an_invite_or_config_says_how_to_get_one(self):
        result = self.cli.run('setup', '--config', str(self.root / 'absent.json'), '--json')
        self.assertEqual((result.code, result.stderr), (1, ''))
        error = json.loads(result.stdout)['error']
        self.assertTrue(error.startswith('setup needs an invite: on the server run switchboard server invite'), error)
        self.assertFalse((self.root / 'absent.json').exists())


class Doctor(CLICase):
    NOT_RUNNING = dict(name='bridge', status='fail',
                       summary="The bridge isn't running, and its background service isn't installed.",
                       fix='Run switchboard install --start to install it and start the bridge.')

    def common(self):
        return [dict(name='program', status='warn',
                     summary='Switchboard <version> is running, but its installed copy is missing or unsafe.',
                     fix='Run switchboard install to install it again.')]

    def run_json(self, *arguments, **kw):
        """Doctor's output, with the running version (checked for form) masked."""
        value = super().run_json(*arguments, **kw)
        if arguments[:1] == ('doctor',):
            self.scrub_version(value)
        return value

    def scrub_version(self, value):
        program = value['checks'][0]
        self.assertRegex(program['summary'], r'^Switchboard \d+\.\d+\.\d+\S* is running, but')
        program['summary'] = program['summary'].replace(program['summary'].split()[1], '<version>', 1)
        return value

    def test_healthy_setup(self):
        self.cli.write_config(dict(self.cli.config, sessions=[self.s1.sid]))
        self.cli.runtime(connected=True, discovery_mode='events')
        value = self.run_json('doctor')
        self.assertEqual(value, {'ok': True, 'checks': self.common() + [
            dict(name='configuration', status='pass', summary='Configuration and key are private and valid.'),
            dict(name='bridge', status='pass', summary='The bridge is running and connected.'),
            dict(name='server', status='pass', summary='The server knows this machine as machine-a (owner alice).'),
            dict(name='chats', status='pass', summary='1 chat joined, open now.')]})
        # No chats joined yet is how a new machine starts, not a warning.
        self.cli.write_config(dict(self.cli.config, sessions=[]))
        self.assertEqual(self.run_json('doctor')['checks'][-1],
                         dict(name='chats', status='pass',
                              summary='No chats joined yet. To connect one, run /switchboard join in it.'))
        human = self.cli.run('doctor')
        self.assertTrue(human.stdout.endswith('\n  1 warning.\n'), human.stdout)

    def test_missing_configuration(self):
        result = self.cli.run('doctor', '--json', '--config', str(self.root / 'absent.json'))
        self.assertEqual(result.code, 1)
        self.assertEqual(self.scrub_version(json.loads(result.stdout)), {'ok': False, 'checks': self.common() + [
            dict(name='configuration', status='fail', summary="This machine isn't set up yet.",
                 fix='Run switchboard init.')]})
        os.chmod(self.cli.config_path, 0o644)
        value = self.run_json('doctor', code=1)
        self.assertEqual(value['checks'][1], dict(name='configuration', status='fail',
                                                  summary='The configuration is invalid or unsafe.',
                                                  fix='Check its fields, that you own it, its private mode (0600), and '
                                                      'that no directory above it is a symbolic link.'))

    def test_stopped_bridge_unready_sessions_and_refused_key(self):
        self.cli.write_config(dict(self.cli.config, sessions=[self.s1.sid, str(uuid.uuid4())]))
        self.server.write_registry({self.machine_b.fp: self.machine_b.entry()})
        value = self.run_json('doctor', code=1)
        self.assertEqual(value, {'ok': False, 'checks': self.common() + [
            dict(name='configuration', status='pass', summary='Configuration and key are private and valid.'),
            self.NOT_RUNNING,
            dict(name='server', status='fail', summary="The server doesn't accept this machine's key.",
                 fix='Make a new invite on the server (sudo switchboard server invite), then run switchboard init '
                     'here and choose to join again.'),
            dict(name='chats', status='pass', summary='2 chats joined; 1 open now.')]})

    def test_disabled_bridge_polling_discovery_and_mismatched_key(self):
        other_cert, other_key, _ = pki.client('other')
        identity = Path(self.cli.config['key']).parent
        (identity / 'key.pem').write_bytes(Path(other_key).read_bytes())
        self.cli.write_config(dict(self.cli.config, enabled=False))
        self.cli.runtime(connected=False, discovery_mode='polling')
        value = self.run_json('doctor', code=1)
        names = {check['name']: check for check in value['checks']}
        self.assertEqual(value['ok'], False)
        self.assertEqual(names['bridge'], dict(name='bridge', status='warn',
                                               summary='The bridge is running, but turned off in its configuration.',
                                               fix='Set "enabled" to true in the configuration, then run switchboard '
                                                   'down and switchboard up.'))
        self.assertEqual(names['discovery'], dict(name='discovery', status='warn',
                                                  summary='Chats are found by polling, so new chats can take a few '
                                                          'seconds to appear.'))
        self.assertEqual(names['chats'], dict(name='chats', status='pass',
                                              summary='No chats joined yet. To connect one, run /switchboard join in it.'))
        self.assertEqual(names['configuration'], dict(name='configuration', status='fail',
                                                      summary="This machine's key or certificate is unsafe or invalid, "
                                                              "or the two don't match.",
                                                      fix='Check that you own them and that they are private (mode '
                                                          '0600); keep the identity files.'))
        self.assertNotIn('server', names)


class RemovedViewer(CLICase):
    def test_view_command_is_gone(self):
        """The browser viewer was removed: `view` and `--open` are parser errors, the help
        never mentions them, and a runtime file an older bridge wrote with a viewer URL
        still reads."""
        help_text = self.cli.run('help').stdout
        self.assertNotIn('view', help_text)
        self.assertNotIn('--open', help_text)
        self.assertJSONError(['view'], 2, None)
        self.assertJSONError(['status', '--open'], 2, None)
        self.cli.runtime(viewer_url='http://127.0.0.1:43210/' + 'A' * 32 + '/')
        value = self.run_json('status')
        self.assertEqual((value['enabled'], value['connected'], value['machine']), (True, True, 'machine-a'))
        self.assertNotIn('detail', value)
        self.assertNotIn('viewer_url', value)
