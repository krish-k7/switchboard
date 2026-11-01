"""Server machine enrollment: invites, redemption, the local machine commands, registry."""
import base64
import hashlib
import json
import os
import secrets
import stat
import threading
import time

from sbcompat.cases import ServerCase, session, sid
from sbcompat.server import Machine, invite_fields
from sbcompat.util import is_time, write_private

GENERIC = 'invalid_invite'


class Enrollment(ServerCase):
    def enroll(self, fp, machine='new-box', owner='sam', address='198.51.100.1', invite=None, **body):
        payload = body or {'machine': machine, 'owner': owner}
        if invite is not None:
            payload['invite'] = invite
        return self.server.call(None, '/v1/enroll', payload, fp=fp, address=address)

    def store(self):
        return json.loads(self.server.invites_file.read_text())

    def write_store(self, value):
        write_private(self.server.invites_file, json.dumps(value, indent=2) + '\n')

    def test_unknown_key_may_only_redeem_an_invite(self):
        stranger = secrets.token_hex(32)
        for method, path, body in (('GET', '/health', None), ('GET', '/v1/peers', None), ('GET', '/v1/machines', None),
                                   ('GET', '/v1/view', None), ('GET', '/', None), ('GET', '/v1/enroll', None),
                                   ('POST', '/v1/send', {}), ('POST', '/v1/register', {'sessions': []}),
                                   ('POST', '/v1/poll', {}), ('POST', '/v1/approve', {'code': 'AAAA-AAAA'}),
                                   ('POST', '/v1/refuse', {}), ('POST', '/v1/revoke', {'machine': 'beta'}),
                                   ('POST', '/v1/nope', {})):
            with self.subTest(path=path, method=method):
                self.assertError(self.server.request(method, path, body, fp=stranger), 403, 'not_enrolled')
        self.assertError(self.enroll(stranger), 403, 'invite_required')

    def test_approval_and_listing_endpoints_are_gone(self):
        """Approval codes and the administrator role were removed: their endpoints answer like
        unknown paths, for every machine, including one whose registry entry says admin."""
        for client in (self.a, self.b):
            with self.subTest(machine=client.machine.name):
                self.assertError(self.server.request('GET', '/v1/machines', machine=client.machine), 404, 'not_found')
                for path, body in (('/v1/approve', {'code': 'AAAA-AAAA', 'machine': 'x', 'owner': 'y'}),
                                   ('/v1/refuse', {'code': 'AAAA-AAAA', 'machine': 'x', 'owner': 'y'}),
                                   ('/v1/revoke', {'machine': 'gamma'})):
                    self.assertError(client.call(path, body), 404, 'not_found')
        self.assertEqual(self.a.call('/health'), (200, {'ok': True, 'machine': 'alpha', 'owner': 'alice'}))
        self.assertEqual(self.c.call('/health')[0], 200)

    def test_invite_command_prints_a_token_and_stores_only_a_hash(self):
        secret, value = self.server.invite()
        self.assertEqual(sorted(value), ['expires_at', 'id', 'invite', 'machine', 'ok', 'server_pin', 'url', 'uses'])
        self.assertEqual((value['ok'], value['url'], value['server_pin'], value['uses'], value['machine']),
                         (True, 'https://switchboard.test', None, 1, None))
        self.assertTrue(abs(value['expires_at'] - (time.time() + 3600)) < 30)
        self.assertRegex(value['id'], r'^[0-9a-f]{12}$')
        self.assertRegex(value['invite'], r'^sbi1\.[A-Za-z0-9_-]+$')
        # A server without a private certificate: no pin in the token; a secret of at least 128 bits.
        fields = invite_fields(value['invite'])
        self.assertEqual(sorted(fields), ['secret', 'url'])
        raw = base64.urlsafe_b64decode(secret + '=' * (-len(secret) % 4))
        self.assertGreaterEqual(len(raw), 16)
        digest = hashlib.sha256(secret.encode()).hexdigest()
        self.assertEqual(value['id'], digest[:12])
        text = self.server.invites_file.read_text()
        self.assertNotIn(secret, text)
        self.assertEqual(self.store()[value['id']]['hash'], digest)
        info = os.lstat(self.server.invites_file)
        self.assertEqual((stat.S_IMODE(info.st_mode), info.st_nlink, info.st_uid), (0o600, 1, os.geteuid()))
        _, fixed = self.server.invite('--name', 'laptop', '--expires', '2h')
        self.assertEqual((fixed['machine'], fixed['uses']), ('laptop', 1))
        self.assertEqual(invite_fields(fixed['invite'])['machine'], 'laptop')
        self.assertTrue(abs(fixed['expires_at'] - (time.time() + 7200)) < 30)
        # A fixed name enrolls one machine, so a named invite has one use.
        for arguments in (('--expires', '30s'), ('--expires', '31d'), ('--uses', '0'), ('--name', 'Bad_Name'),
                          ('--url', 'http://plain'), ('--name', 'laptop', '--uses', '3')):
            with self.subTest(arguments=arguments):
                self.assertEqual(self.server.local('invite', *arguments), (2, None))
        # A Unix-socket server does not know its public address: --url is required.
        code, value = self.server.local('invite')
        self.assertEqual(code, 1)
        self.assertFalse(value['ok'])
        self.assertIn('--url', value['error'])

    def test_redemption_enrolls_the_key_once(self):
        stranger, second = secrets.token_hex(32), secrets.token_hex(32)
        before = self.server.registry()
        secret, value = self.server.invite()
        self.assertEqual(self.enroll(stranger, invite=secret),
                         (200, {'ok': True, 'state': 'approved', 'machine': 'new-box'}))
        raw = self.server.peers.read_text()
        registry = json.loads(raw)
        self.assertEqual(raw, json.dumps(registry, sort_keys=True, indent=2) + '\n')
        entry = registry.pop(stranger)
        self.assertTrue(is_time(entry.pop('enrolled_at')))
        self.assertEqual(entry, {'machine': 'new-box', 'owner': 'sam', 'enabled': True, 'invite': value['id']})
        self.assertEqual(registry, before)
        self.assertEqual(self.server.call(None, '/health', fp=stranger),
                         (200, {'ok': True, 'machine': 'new-box', 'owner': 'sam'}))
        # Used up: remembered as used, and another key is told so.
        self.assertEqual({id: entry['uses'] for id, entry in self.store().items()}, {value['id']: 0})
        self.assertError(self.enroll(second, machine='other-box', invite=secret), 403, 'invite_used')
        self.assertEqual(self.server.local('invites')[1]['invites'], [])
        # The enrolled key asking again is told it is enrolled, with or without an invite.
        expected = (200, {'ok': True, 'state': 'approved', 'machine': 'new-box'})
        self.assertEqual(self.enroll(stranger, machine='x', owner='y'), expected)
        self.assertEqual(self.enroll(stranger, invite='anything'), expected)

    def test_wrong_expired_used_and_mismatched_invites_are_refused(self):
        """An unknown secret or a mismatched name gets the generic error; the holder of a
        known secret is told whether it expired or was used up."""
        stranger = secrets.token_hex(32)
        secret, value = self.server.invite()
        fixed, _ = self.server.invite('--name', 'laptop')
        for wrong in (base64.urlsafe_b64encode(secrets.token_bytes(24)).decode().rstrip('='), 'short', '',
                      secret[:-2], secret + 'A'):
            with self.subTest(secret=wrong):
                self.assertError(self.enroll(stranger, invite=wrong), 403, GENERIC)
        # A name the invite fixes must be the name asked for.
        self.assertError(self.enroll(stranger, machine='new-box', invite=fixed), 403, GENERIC)
        # A taken name is refused without using the invite.
        self.assertError(self.enroll(stranger, machine='beta', invite=secret), 409, 'machine_taken')
        self.assertEqual(sorted(entry['uses'] for entry in self.store().values()), [1, 1])
        # Expired (the store's expiry passed).
        store = self.store()
        store[value['id']]['expires_at'] = int(time.time()) - 1
        self.write_store(store)
        self.assertError(self.enroll(stranger, invite=secret), 403, 'invite_expired')
        self.assertEqual(self.enroll(stranger, machine='laptop', invite=fixed)[0], 200)
        self.assertError(self.enroll(secrets.token_hex(32), machine='laptop', invite=fixed), 403, 'invite_used')
        # The name an invite fixes is checked first: a mismatch says nothing about its state.
        self.assertError(self.enroll(secrets.token_hex(32), machine='laptop2', invite=fixed), 403, GENERIC)
        # Forgotten a week after it expired.
        store = self.store()
        store[value['id']]['expires_at'] = int(time.time()) - 8 * 86400
        self.write_store(store)
        self.assertError(self.enroll(secrets.token_hex(32), invite=secret), 403, GENERIC)

    def test_use_counts_hold_under_concurrent_redemption(self):
        for uses in (1, 3):
            secret, _ = self.server.invite('--uses', str(uses))
            results = [None] * 8

            def redeem(index):
                results[index] = self.enroll(secrets.token_hex(32), machine=f'box-{uses}-{index}',
                                             address=f'192.0.2.{10 * uses + index}', invite=secret)
            threads = [threading.Thread(target=redeem, args=(index,)) for index in range(8)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(20)
            statuses = sorted(status for status, _ in results)
            with self.subTest(uses=uses):
                self.assertEqual(statuses, [200] * uses + [403] * (8 - uses), results)
                self.assertTrue(all(value == {'ok': False, 'error': 'invite_used'}
                                    for status, value in results if status == 403))
                enrolled = [e for e in self.server.registry().values() if e['machine'].startswith(f'box-{uses}-')]
                self.assertEqual(len(enrolled), uses)
                self.assertEqual([entry['uses'] for entry in self.store().values()], [0] * len(self.store()))

    def test_open_invites_are_listed_without_secrets_and_can_be_revoked(self):
        secret, first = self.server.invite('--uses', '2')
        other, second = self.server.invite('--name', 'laptop')
        code, listing = self.server.local('invites')
        self.assertEqual(code, 0)
        self.assertEqual(sorted(listing), ['invites', 'ok'])
        rows = listing['invites']
        self.assertEqual([row['id'] for row in rows], [first['id'], second['id']])
        self.assertEqual(sorted(rows[0]), ['created_at', 'expires_at', 'id', 'machine', 'uses'])
        self.assertEqual([(row['machine'], row['uses']) for row in rows], [(None, 2), ('laptop', 1)])
        self.assertTrue(is_time(rows[0]['created_at']))
        text = json.dumps(listing)
        self.assertNotIn(secret, text)
        self.assertNotIn(hashlib.sha256(secret.encode()).hexdigest(), text)
        self.assertEqual(self.server.local('invite', 'revoke', second['id']),
                         (0, {'ok': True, 'id': second['id'], 'state': 'revoked'}))
        self.assertError(self.enroll(secrets.token_hex(32), invite=other), 403, GENERIC)
        code, value = self.server.local('invite', 'revoke', second['id'])
        self.assertEqual((code, value['ok']), (1, False))
        self.assertEqual([row['id'] for row in self.server.local('invites')[1]['invites']], [first['id']])
        # A used-up or expired invite is not listed.
        self.assertEqual(self.enroll(secrets.token_hex(32), invite=secret)[0], 200)
        self.assertEqual(self.server.local('invites')[1]['invites'][0]['uses'], 1)

    def test_enrollment_request_validation(self):
        stranger = secrets.token_hex(32)
        secret, _ = self.server.invite()
        for body, status, code in [
            ({'machine': 'New-Box', 'owner': 'sam', 'invite': secret}, 400, 'invalid_machine'),
            ({'machine': '1box', 'owner': 'sam', 'invite': secret}, 400, 'invalid_machine'),
            ({'machine': 'box_1', 'owner': 'sam', 'invite': secret}, 400, 'invalid_machine'),
            ({'machine': 'a' * 49, 'owner': 'sam', 'invite': secret}, 400, 'invalid_machine'),
            ({'owner': 'sam', 'invite': secret}, 400, 'invalid_machine'),
            ({'machine': 'new-box', 'owner': '', 'invite': secret}, 400, 'invalid_label'),
            ({'machine': 'new-box', 'owner': 'o' * 65, 'invite': secret}, 400, 'invalid_label'),
            ({'machine': 'new-box', 'owner': 'a\nb', 'invite': secret}, 400, 'invalid_label'),
            ({'machine': 'new-box', 'invite': secret}, 400, 'invalid_label'),
            ({'machine': 'new-box', 'owner': 'sam', 'invite': 5}, 400, 'invalid_enrollment'),
            ({'machine': 'new-box', 'owner': 'sam', 'invite': secret, 'admin': True}, 400, 'invalid_enrollment'),
            ({'machine': 'new-box', 'owner': 'sam', 'bootstrap_token': secret}, 400, 'invalid_enrollment'),
            ({'machine': 'new-box', 'owner': 'sam'}, 403, 'invite_required'),
            ({'machine': 'beta', 'owner': 'sam', 'invite': secret}, 409, 'machine_taken'),
        ]:
            with self.subTest(code=code, body={k: v for k, v in body.items() if k != 'invite'}):
                self.assertError(self.server.call(None, '/v1/enroll', body, fp=stranger), status, code)
        self.assertError(self.server.call(None, '/v1/enroll', ['new-box'], fp=stranger), 400, 'invalid_enrollment')
        self.assertEqual(len(self.store()), 1)
        self.assertEqual(self.enroll(stranger, machine='a' + 'b' * 47, invite=secret)[0], 200)

    def test_unknown_keys_are_rate_limited_per_address(self):
        stranger = secrets.token_hex(32)
        statuses = [self.enroll(stranger, address='192.0.2.50')[0] for _ in range(20)]
        self.assertEqual(statuses, [403] * 20)
        self.assertError(self.enroll(stranger, address='192.0.2.50'), 429, 'rate_limited')
        secret, _ = self.server.invite()
        self.assertError(self.enroll(secrets.token_hex(32), machine='other', address='192.0.2.50', invite=secret),
                         429, 'rate_limited')
        self.assertEqual(len(self.store()), 1)
        self.assertEqual(self.enroll(stranger, address='192.0.2.51', invite=secret)[0], 200)

    def test_proxy_address_header_is_validated_for_unknown_keys(self):
        stranger = secrets.token_hex(32)
        identity = ('X-Switchboard-Client-Fingerprint', stranger)
        body = {'machine': 'new-box', 'owner': 'sam'}
        for headers in ([identity, ('X-Switchboard-Client-Address', 'a'), ('X-Switchboard-Client-Address', 'b')],
                        [identity, ('X-Switchboard-Client-Address', 'a' * 65)],
                        [identity, ('X-Switchboard-Client-Address', 'a\tb')]):
            with self.subTest(headers=headers[1:]):
                self.assertError(self.server.request('POST', '/v1/enroll', body, identity_headers=headers),
                                 401, 'invalid_proxy_identity')
        response = self.server.request('POST', '/v1/enroll', body, identity_headers=[identity])
        self.assertEqual((response.status, response.json), (403, {'ok': False, 'error': 'invite_required'}))
        secret, _ = self.server.invite()
        response = self.server.request('POST', '/v1/enroll', {'machine': 'b2', 'owner': 'c', 'invite': secret},
                                       identity_headers=[('X-Switchboard-Client-Fingerprint', secrets.token_hex(32)),
                                                         ('X-Switchboard-Client-Address', 'a' * 64)])
        self.assertEqual(response.status, 200)

    def test_local_revocation_is_immediate_and_frees_the_name(self):
        g1 = sid()
        self.c.register([session(g1)])
        self.c.poll(revision=False)
        waiting = self.background(self.c.call, '/v1/poll', {'registration': self.c.registration,
                                                            'peers_revision': self.c.revision})
        self.settle(0.4)
        started = time.monotonic()
        self.assertEqual(self.server.local('revoke', 'gamma'), (0, {'ok': True, 'machine': 'gamma', 'state': 'revoked'}))
        self.assertError(waiting(), 403, 'not_enrolled')
        self.assertLess(time.monotonic() - started, 3)
        self.assertError(self.c.call('/health'), 403, 'not_enrolled')
        self.assertEqual(self.b.call('/v1/peers')[1]['peers'], [])
        entry = self.server.registry()[self.gamma.fp]
        self.assertTrue(is_time(entry.pop('revoked_at')))
        self.assertTrue(entry.pop('revoked_by').endswith(' on the server'))
        self.assertEqual(entry, {'machine': 'gamma', 'owner': 'ops', 'enabled': False})
        self.assertEqual(stat.S_IMODE(os.lstat(self.server.peers).st_mode), 0o600)
        names = [row['machine'] for row in self.server.local('machines')[1]['machines']]
        self.assertEqual(names, ['alpha', 'beta'])
        for name in ('gamma', 'zeta'):
            code, value = self.server.local('revoke', name)
            self.assertEqual((code, value['ok']), (1, False))
        # The revoked key cannot redeem its way back without a new invite; a new key can take the name.
        self.assertError(self.enroll(self.gamma.fp, machine='gamma'), 403, 'invite_required')
        secret, _ = self.server.invite()
        self.assertEqual(self.enroll(secrets.token_hex(32), machine='gamma', invite=secret)[0], 200)

    def test_local_machine_listing_shows_when_each_was_last_seen(self):
        self.a.call('/health')
        code, value = self.server.local('machines')
        self.assertEqual(code, 0)
        self.assertEqual(sorted(value), ['machines', 'ok'])
        rows = value['machines']
        self.assertEqual([(row['machine'], row['owner']) for row in rows],
                         [('alpha', 'alice'), ('beta', 'bob'), ('gamma', 'ops')])
        self.assertEqual(sorted(rows[0]), ['last_seen', 'machine', 'owner'])
        self.assertTrue(is_time(rows[0]['last_seen']))
        self.assertIsNone(rows[1]['last_seen'])
        # Redeeming counts as being seen.
        secret, _ = self.server.invite()
        self.enroll(secrets.token_hex(32), invite=secret)
        rows = {row['machine']: row for row in self.server.local('machines')[1]['machines']}
        self.assertTrue(is_time(rows['new-box']['last_seen']))

    def test_invites_survive_a_server_restart(self):
        secret, _ = self.server.invite()
        self.server.stop()
        self.server.start()
        self.assertEqual(self.enroll(secrets.token_hex(32), invite=secret)[0], 200)

    def test_an_unsafe_invite_store_fails_closed(self):
        stranger = secrets.token_hex(32)
        secret, _ = self.server.invite()
        os.chmod(self.server.invites_file, 0o644)
        self.assertError(self.enroll(stranger, invite=secret), 503, 'enrollment_unavailable')
        code, value = self.server.local('invites')
        self.assertEqual((code, value['ok']), (1, False))
        os.chmod(self.server.invites_file, 0o600)
        saved = self.server.invites_file.with_name('saved.json')
        os.rename(self.server.invites_file, saved)
        os.symlink(saved, self.server.invites_file)
        self.assertError(self.enroll(stranger, invite=secret), 503, 'enrollment_unavailable')
        os.unlink(self.server.invites_file)
        os.rename(saved, self.server.invites_file)
        self.server.invites_file.write_text('{not json')
        self.assertError(self.enroll(stranger, invite=secret), 503, 'enrollment_unavailable')
        self.assertError(self.server.call(None, '/health', fp=stranger), 403, 'not_enrolled')

    def test_registry_is_hot_reloaded_and_fails_closed(self):
        newcomer = Machine('newcomer', owner='lee')
        registry = self.server.registry()
        self.server.write_registry(dict(registry, **{newcomer.fp: newcomer.entry()}))
        self.assertEqual(self.server.call(newcomer, '/health')[1]['machine'], 'newcomer')
        disabled = dict(registry, **{self.beta.fp: dict(registry[self.beta.fp], enabled=False)})
        self.server.write_registry(disabled)
        self.assertError(self.b.call('/health'), 403, 'not_enrolled')
        stranger = secrets.token_hex(32)
        broken = [
            '{not json',
            json.dumps(dict(registry, **{newcomer.fp: dict(newcomer.entry(), machine='beta')})),
            json.dumps(dict(registry, **{newcomer.fp: {'machine': 'x', 'enabled': True}})),
            json.dumps(dict(registry, **{'ZZ': dict(newcomer.entry())})),
            json.dumps([]),
        ]
        for text in broken:
            with self.subTest(registry=text[:60]):
                self.server.write_registry(text + '\n')
                self.assertError(self.a.call('/health'), 503, 'enrollment_unavailable')
                self.assertError(self.enroll(stranger), 503, 'enrollment_unavailable')
        self.server.peers.unlink()
        self.assertError(self.a.call('/health'), 503, 'enrollment_unavailable')
        # Disabled entries are not validated; entries with or without the old admin flag work alike.
        self.server.write_registry(dict(registry, **{'not-a-fingerprint': {'enabled': False}}))
        self.assertEqual(self.a.call('/health'), (200, {'ok': True, 'machine': 'alpha', 'owner': 'alice'}))
        self.assertEqual(self.b.call('/health'), (200, {'ok': True, 'machine': 'beta', 'owner': 'bob'}))
