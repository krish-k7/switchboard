"""Server HTTP framing: identity headers, paths, methods, bodies, and error envelopes."""
import json
import secrets

from sbcompat.cases import ServerCase


class IdentityHeaders(ServerCase):
    def test_unix_ingress_requires_exactly_one_lowercase_fingerprint(self):
        good = self.alpha.fp
        for headers in ([], [('X-Switchboard-Client-Fingerprint', good), ('X-Switchboard-Client-Fingerprint', good)],
                        [('X-Switchboard-Client-Fingerprint', good.upper())],
                        [('X-Switchboard-Client-Fingerprint', good[:63])],
                        [('X-Switchboard-Client-Fingerprint', good + '0')]):
            with self.subTest(headers=[v[:8] for _, v in headers]):
                self.assertError(self.server.request('GET', '/health', identity_headers=headers), 401, 'invalid_proxy_identity')
                self.assertError(self.server.request('POST', '/v1/enroll', {'machine': 'x', 'owner': 'y'},
                                                     identity_headers=headers), 401, 'invalid_proxy_identity')


class Paths(ServerCase):
    def test_unknown_paths_and_wrong_methods(self):
        for method, path, body in (('GET', '/v1/nope', None), ('GET', '/v1/peers?limit=5', None), ('GET', '/health/', None),
                                   ('POST', '/v1/view', {}), ('POST', '/', {}), ('POST', '/v1/nope', {}),
                                   ('POST', '/v1/machines', {})):
            with self.subTest(method=method, path=path):
                self.assertError(self.server.request(method, path, body, machine=self.alpha), 404, 'not_found')
        # A path that exists for the other method is 405, naming the method it takes.
        for method, path, body, allow in (('GET', '/v1/register', None, 'POST'), ('GET', '/v1/poll', None, 'POST'),
                                          ('POST', '/health', {}, 'GET'), ('POST', '/v1/peers', {}, 'GET')):
            with self.subTest(method=method, path=path):
                response = self.server.request(method, path, body, machine=self.alpha)
                self.assertError(response, 405, 'method_not_allowed')
                self.assertEqual(response.header('Allow'), allow)

    def test_unsupported_methods_and_malformed_request_lines(self):
        for method in ('PUT', 'DELETE', 'HEAD', 'PATCH'):
            with self.subTest(method=method):
                response = self.server.request(method, '/health', machine=self.alpha)
                self.assertEqual((response.status, response.header('Allow')), (405, 'GET, POST'))
                if method == 'HEAD':
                    self.assertEqual(response.body, b'')
                else:
                    self.assertEqual(response.json, {'ok': False, 'error': 'method_not_allowed'})
        if self.server.mode == 'unix':
            sent = self.server.raw(f'HEAD /health HTTP/1.1\r\nHost: localhost\r\n'
                                   f'X-Switchboard-Client-Fingerprint: {self.alpha.fp}\r\n\r\n'.encode(), parse=False)
            self.assertTrue(sent.startswith(b'HTTP/1.0 405 ') and sent.endswith(b'\r\n\r\n'), sent)
        response = self.server.raw(b'GARBAGE\r\n\r\n')
        self.assertEqual((response.status, response.json), (400, {'ok': False, 'error': 'invalid_request'}))


class Bodies(ServerCase):
    def post(self, raw=None, headers=(), content_type='application/json', path='/v1/register', fp=None):
        head = f'POST {path} HTTP/1.1\r\nHost: localhost\r\nX-Switchboard-Client-Fingerprint: {fp or self.alpha.fp}\r\n'
        if content_type:
            head += f'Content-Type: {content_type}\r\n'
        head += ''.join(f'{k}: {v}\r\n' for k, v in headers)
        return self.server.raw(head.encode() + b'\r\n' + (raw or b''))

    def test_content_length_rules(self):
        body = json.dumps({'sessions': []}).encode()
        self.assertError(self.post(body), 400, 'invalid_length')
        self.assertError(self.post(b'5\r\n{"a":1}\r\n0\r\n\r\n', [('Transfer-Encoding', 'chunked')]), 400, 'invalid_length')
        self.assertError(self.post(body, [('Transfer-Encoding', 'chunked'), ('Content-Length', str(len(body)))]),
                         400, 'invalid_length')
        self.assertError(self.post(body, [('Content-Length', str(len(body)))] * 2), 400, 'invalid_length')
        self.assertError(self.post(body, [('Content-Length', 'abc')]), 400, 'invalid_length')
        self.assertError(self.post(body, [('Content-Length', '-1')]), 400, 'invalid_length')
        self.assertError(self.post(b'', [('Content-Length', '65537')]), 413, 'body_too_large')
        self.assertError(self.post(b'{"sess', [('Content-Length', '20')]), 400, 'incomplete_body')
        self.assertEqual(self.post(body, [('Content-Length', str(len(body)))]).status, 200)
        padded = json.dumps({'sessions': [], 'pad': 'x' * (65536 - 30)}).encode()
        padded = padded[:-1] + b' ' * (65536 - len(padded)) + padded[-1:]
        self.assertEqual(len(padded), 65536)
        self.assertEqual(self.post(padded, [('Content-Length', '65536')]).status, 200)

    def test_content_type_and_json_rules(self):
        body = json.dumps({'sessions': []}).encode()
        length = [('Content-Length', str(len(body)))]
        self.assertError(self.post(body, length, content_type='text/plain'), 400, 'expected_json')
        self.assertError(self.post(body, length, content_type=None), 400, 'expected_json')
        self.assertEqual(self.post(body, length, content_type='application/json; charset=utf-8').status, 200)
        self.assertEqual(self.post(body, length, content_type='Application/JSON').status, 200)
        for raw in (b'not json', b'{"sessions": [}', b'\xff\xfe{}', b'', b'{"sessions": []} trailing'):
            with self.subTest(raw=raw[:20]):
                self.assertError(self.post(raw, [('Content-Length', str(len(raw)))]), 400, 'invalid_json')

    def test_error_bodies_use_the_structured_envelope(self):
        response = self.server.request('GET', '/v1/nope', machine=self.alpha)
        self.assertEqual(response.body, b'{"ok": false, "error": "not_found"}')
        self.assertEqual(response.header('Content-Type'), 'application/json')
        self.assertEqual(response.header('Cache-Control'), 'no-store')

    def test_unknown_key_enrollment_reads_the_body_with_the_same_rules(self):
        stranger = secrets.token_hex(32)
        self.assertError(self.post(b'{}', [('Content-Length', '2')], content_type='text/plain', path='/v1/enroll',
                                   fp=stranger), 400, 'expected_json')
        self.assertError(self.post(b'{', [('Content-Length', '1')], path='/v1/enroll', fp=stranger), 400, 'invalid_json')
