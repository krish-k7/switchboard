"""A stand-in for GitHub Releases on 127.0.0.1, for testing install.sh.

Serves the files in RELEASE_DIR as release VERSION of REPO, both ways
install.sh downloads them:
- public: /REPO/releases/latest/download/NAME and /REPO/releases/download/vVERSION/NAME
- private (API, only with "Authorization: Bearer TOKEN", else 404 as GitHub
  answers for private repositories): /api/repos/REPO/releases/latest,
  /api/repos/REPO/releases/tags/vVERSION, and the assets they list at
  /api/repos/REPO/releases/assets/ID (with "Accept: application/octet-stream").

Usage: fake_release_server.py RELEASE_DIR REPO VERSION TOKEN PORT_FILE
Writes the chosen port to PORT_FILE, then serves until killed.
"""
import http.server
import json
import os
import socketserver
import sys

release_dir, repo, version, token, port_file = sys.argv[1:]
tag = 'v' + version
names = sorted(os.listdir(release_dir))


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, body=b'', kind='application/octet-stream'):
        self.send_response(status)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def file(self, name):
        if name not in names:
            return self.send(404, b'Not Found')
        with open(os.path.join(release_dir, name), 'rb') as handle:
            self.send(200, handle.read())

    def do_GET(self):
        path = self.path
        for prefix in (f'/{repo}/releases/latest/download/', f'/{repo}/releases/download/{tag}/'):
            if path.startswith(prefix):
                return self.file(path[len(prefix):])
        api = f'/api/repos/{repo}/releases/'
        if path.startswith(api):
            if self.headers.get('Authorization') != f'Bearer {token}':
                return self.send(404, b'{"message": "Not Found"}', 'application/json')
            rest = path[len(api):]
            if rest in ('latest', f'tags/{tag}'):
                base = f'http://127.0.0.1:{self.server.server_port}{api}'
                assets = [{'url': f'{base}assets/{100 + i}', 'id': 100 + i, 'node_id': 'x',
                           'name': name, 'label': '', 'uploader': {'login': 'ci', 'id': 1},
                           'content_type': 'application/octet-stream',
                           'browser_download_url': f'https://github.invalid/{name}'}
                          for i, name in enumerate(names)]
                body = json.dumps({'url': f'{base}1', 'tag_name': tag, 'name': tag,
                                   'author': {'login': 'ci'}, 'assets': assets}, indent=2)
                return self.send(200, body.encode(), 'application/json')
            if rest.startswith('assets/') and rest[7:].isdigit():
                if self.headers.get('Accept') != 'application/octet-stream':
                    return self.send(415, b'expected Accept: application/octet-stream')
                index = int(rest[7:]) - 100
                if 0 <= index < len(names):
                    return self.file(names[index])
        self.send(404, b'Not Found')


class Server(http.server.ThreadingHTTPServer):
    def server_bind(self):
        # HTTPServer.server_bind looks up the host's fully qualified name,
        # a reverse DNS query that takes seconds on some CI machines.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = '127.0.0.1', self.server_address[1]


server = Server(('127.0.0.1', 0), Handler)
with open(port_file + '.tmp', 'w') as handle:
    handle.write(str(server.server_port))
os.replace(port_file + '.tmp', port_file)
server.serve_forever()
