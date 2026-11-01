"""Direct mutual-TLS mode: identity from the certificate, never from headers."""
import socket
import ssl
import uuid

from sbcompat import pki
from sbcompat.cases import ServerCase, session, sid
from sbcompat.server import Client, Machine, tls_machine


class DirectTLS(ServerCase):
    mode = 'tls'

    def test_identity_comes_from_the_certificate_and_headers_are_ignored(self):
        response = self.server.request('GET', '/health', machine=self.gamma,
                                       identity_headers=[('X-Switchboard-Client-Fingerprint', self.alpha.fp)])
        self.assertEqual((response.status, response.json), (200, {'ok': True, 'machine': 'gamma', 'owner': 'ops'}))
        self.assertEqual(response.version, 10)
        response = self.server.request('GET', '/v1/peers', machine=self.gamma,
                                       identity_headers=[('X-Switchboard-Client-Fingerprint', 'zz')])
        self.assertEqual((response.status, response.json['machine']), (200, 'gamma'))

    def test_handshake_requires_a_client_certificate_from_the_client_ca(self):
        cert, key, _ = pki.client('rogue', self_signed=True)
        rogue = Machine('rogue', cert=cert, key=key)
        for machine in (None, rogue):
            with self.subTest(client='none' if machine is None else 'self-signed'):
                with self.assertRaises((ssl.SSLError, ConnectionError, OSError)):
                    self.server.request('GET', '/health', machine=machine, timeout=5)
        self.assertEqual(self.server.request('GET', '/health', machine=self.alpha).status, 200)

    def test_unenrolled_certificate_can_redeem_an_invite_over_tls(self):
        newcomer = tls_machine('newcomer', owner='lee')
        self.assertError(self.server.request('GET', '/health', machine=newcomer), 403, 'not_enrolled')
        self.assertError(self.server.call(newcomer, '/v1/enroll', {'machine': 'newcomer', 'owner': 'lee'}),
                         403, 'invite_required')
        secret, _ = self.server.invite()
        status, value = self.server.call(newcomer, '/v1/enroll', {'machine': 'newcomer', 'owner': 'lee', 'invite': secret})
        self.assertEqual((status, value), (200, {'ok': True, 'state': 'approved', 'machine': 'newcomer'}))
        self.assertEqual(self.server.call(newcomer, '/health')[1]['machine'], 'newcomer')

    def test_unknown_key_rate_limit_uses_the_connection_address(self):
        # Direct mode takes the client address from the connection, ignoring headers.
        newcomer = tls_machine('newcomer')
        for index in range(20):
            self.assertError(self.server.call(newcomer, '/v1/enroll', {'machine': 'newcomer', 'owner': 'o'},
                                              headers=[('X-Switchboard-Client-Address', f'192.0.2.{index}')]),
                             403, 'invite_required')
        self.assertError(self.server.call(newcomer, '/v1/enroll', {'machine': 'newcomer', 'owner': 'o'},
                                          headers=[('X-Switchboard-Client-Address', '192.0.2.99')]), 429, 'rate_limited')

    def test_message_round_trip_over_tls(self):
        a1, b1 = sid(), sid()
        self.a.register([session(a1, 'alpha-chat')], ['ack'])
        self.b.register([session(b1)], ['ack'])
        message = str(uuid.uuid4())
        self.assertEqual(self.a.send(message, 'alpha/' + a1, 'beta/' + b1, 'over tls'),
                         (200, {'ok': True, 'id': message, 'state': 'routed'}))
        event = self.b.poll(acks=[], revision=False)['events'][0]
        self.assertEqual((event['text'], event['sender_name']), ('over tls', 'alpha-chat'))
        self.b.poll(acks=[event['ack_id']], revision=False)
        self.assertEqual(self.message_state(message)['state'], 'received')
