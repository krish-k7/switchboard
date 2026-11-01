#!/usr/bin/env python3
"""Capture every screen of `switchboard init` (and the restyled status,
sessions, and doctor) for review, at 60, 80, and 120 columns and without
color.

    python3 -I scripts/capture-init.py [--binary PATH] [--out DIR] [--only NAME]

Each scenario runs the real binary on a pseudo-terminal in a sandbox like
scripts/try-init.sh builds (a private HOME; fake launchctl, systemctl, sudo, and
claude first on PATH; throwaway servers on 127.0.0.1), types the keys a
person would, and records the screen at chosen moments through a small
terminal emulator: NAME.txt is the plain text, NAME.ansi the same screen with
its colors (view it with `cat` or `less -R`). index.txt lists them. Nothing
outside the sandbox and the output directory is touched; both the sandbox
and the servers are removed on exit.
"""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UP, DOWN, ENTER, CTRL_C, ESC = '\x1b[A', '\x1b[B', '\r', '\x03', '\x1b'


def backspace(count):
    return '\x7f' * count


def paste(text):
    return '\x1b[200~' + text + '\x1b[201~'


class Screen:
    """Just enough of a VT100 for what the CLI writes: text with wrapping,
    CR/LF, cursor up/down/right/left, erase in line and below, and SGR."""

    def __init__(self, cols):
        self.cols = cols
        self.lines = [[]]
        self.row = self.col = 0
        self.style = ''
        self.pending = ''

    def _line(self, row):
        while len(self.lines) <= row:
            self.lines.append([])
        return self.lines[row]

    def _put(self, ch):
        if self.col >= self.cols:
            self.row += 1
            self.col = 0
        line = self._line(self.row)
        while len(line) < self.col:
            line.append((' ', ''))
        if len(line) == self.col:
            line.append((ch, self.style))
        else:
            line[self.col] = (ch, self.style)
        self.col += 1

    def feed(self, data):
        data = self.pending + data
        self.pending = ''
        i = 0
        while i < len(data):
            ch = data[i]
            if ch == '\x1b':
                match = re.match(r'\x1b\[([?0-9;]*)([A-Za-z~])', data[i:])
                if not match:
                    if len(data) - i < 16:
                        self.pending = data[i:]
                        return
                    i += 2
                    continue
                self._csi(match.group(1), match.group(2))
                i += len(match.group(0))
                continue
            if ch == '\r':
                self.col = 0
            elif ch == '\n':
                self.row += 1
                self._line(self.row)
            elif ch == '\b':
                self.col = max(0, self.col - 1)
            elif ch >= ' ':
                self._put(ch)
            i += 1

    def _csi(self, params, final):
        if params.startswith('?'):
            return
        numbers = [int(p) if p else 0 for p in params.split(';')] if params else []
        n = numbers[0] if numbers and numbers[0] else 1
        if final == 'A':
            self.row = max(0, self.row - n)
        elif final == 'B':
            self.row += n
        elif final == 'C':
            self.col += n
        elif final == 'D':
            self.col = max(0, self.col - n)
        elif final == 'K':
            line = self._line(self.row)
            mode = numbers[0] if numbers else 0
            if mode == 0:
                del line[self.col:]
            elif mode == 1:
                for c in range(min(self.col + 1, len(line))):
                    line[c] = (' ', '')
            else:
                line.clear()
        elif final == 'J':
            line = self._line(self.row)
            del line[self.col:]
            del self.lines[self.row + 1:]
        elif final == 'm':
            if not numbers or numbers == [0]:
                self.style = ''
            else:
                codes = [c for c in params.split(';') if c]
                if '0' in codes:
                    codes = codes[len(codes) - codes[::-1].index('0'):]
                    self.style = ';'.join(codes)
                else:
                    self.style = ';'.join(filter(None, [self.style] + codes))

    def text(self):
        out = [''.join(c for c, _ in line).rstrip() for line in self.lines]
        while out and not out[-1]:
            out.pop()
        return '\n'.join(out) + '\n'

    def ansi(self):
        out = []
        for line in self.lines:
            text, style = '', ''
            for ch, cell in line:
                if cell != style:
                    text += '\x1b[0m' + (f'\x1b[{cell}m' if cell else '')
                    style = cell
                text += ch
            if style:
                text += '\x1b[0m'
            out.append(text.rstrip())
        while out and not out[-1]:
            out.pop()
        return '\n'.join(out) + '\n'


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class Sandbox:
    def __init__(self, binary, base):
        self.binary = binary
        self.base = base
        self.servers = []

    def fresh(self, name):
        """A new HOME with fake service managers, sudo, and claude."""
        work = os.path.join(self.base, name)
        if os.path.exists(work):
            self.stop_bridge(work)
            shutil.rmtree(work)
        os.makedirs(os.path.join(work, 'home', '.claude', 'sessions'), mode=0o700)
        os.chmod(os.path.join(work, 'home', '.claude'), 0o700)
        os.chmod(os.path.join(work, 'home'), 0o700)
        os.makedirs(os.path.join(work, 'bin'), mode=0o700)
        manager = os.path.join(ROOT, 'scripts', 'try-init.sh')
        source = open(manager).read()
        for marker, filename in (('cat > "$work/bin/service-manager" <<EOF\n', 'service-manager'),
                                 ('cat > "$work/bin/claude" <<EOF\n', 'claude'),
                                 ('cat > "$work/bin/sudo" <<EOF\n', 'sudo')):
            body = source.split(marker, 1)[1].split('\nEOF\n', 1)[0] + '\n'
            # The heredocs in try-init.sh expand $work and escape the rest.
            body = body.replace('$work', work).replace('\\$', '$').replace('\\\\', '\\')
            path = os.path.join(work, 'bin', filename)
            with open(path, 'w') as f:
                f.write(body)
            os.chmod(path, 0o700)
        os.symlink('service-manager', os.path.join(work, 'bin', 'launchctl'))
        os.symlink('service-manager', os.path.join(work, 'bin', 'systemctl'))
        return work

    def stop_bridge(self, work):
        pidfile = os.path.join(work, 'bridge.pid')
        if os.path.exists(pidfile):
            try:
                os.kill(int(open(pidfile).read()), signal.SIGTERM)
            except (OSError, ValueError):
                pass

    def env(self, work, cols, color, extra=None):
        env = {
            'HOME': os.path.join(work, 'home'),
            'PATH': f"{work}/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            'TERM': 'xterm-256color',
            'LANG': 'en_US.UTF-8',
            'USER': 'krish',
            'LOGNAME': 'krish',
            'SHELL': '/bin/zsh',
            'COLUMNS': str(cols),
        }
        if not color:
            env['NO_COLOR'] = '1'
        env.update(extra or {})
        return env

    def server(self):
        """A throwaway private-mode server; returns its directory and port."""
        directory = tempfile.mkdtemp(prefix='server-', dir=self.base)
        os.chmod(directory, 0o700)
        os.makedirs(os.path.join(directory, 'state'), mode=0o700)
        port = free_port()
        config = os.path.join(directory, 'server.json')
        with open(os.open(config, os.O_WRONLY | os.O_CREAT, 0o600), 'w') as f:
            json.dump({'tls': 'private', 'listen': '127.0.0.1', 'port': port,
                       'names': ['localhost', '127.0.0.1'], 'db': 'state/messages.sqlite3',
                       'peers_file': 'peers.json'}, f)
        with open(os.open(os.path.join(directory, 'peers.json'), os.O_WRONLY | os.O_CREAT, 0o600), 'w') as f:
            f.write('{}\n')
        log = open(os.path.join(directory, 'server.log'), 'w')
        process = subprocess.Popen([self.binary, 'server', '--config', config], stderr=log,
                                   stdout=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
        self.servers.append(process)
        for _ in range(100):
            if os.path.exists(os.path.join(directory, 'state', 'tls', 'private-cert.pem')):
                break
            time.sleep(0.1)
        time.sleep(0.3)
        return directory, port

    def invite(self, directory, **options):
        argv = [self.binary, 'server', 'invite', '--config', os.path.join(directory, 'server.json'), '--json']
        for key, value in options.items():
            argv += [f'--{key}', str(value)]
        return json.loads(subprocess.check_output(argv))['invite']

    def close(self):
        for process in self.servers:
            process.terminate()
        for process in self.servers:
            process.wait()
        for name in os.listdir(self.base):
            self.stop_bridge(os.path.join(self.base, name))
        stub = os.path.join(self.base)
        for root, _, files in os.walk(stub):
            if 'server.pid' in files:
                try:
                    os.kill(int(open(os.path.join(root, 'server.pid')).read()), signal.SIGTERM)
                except (OSError, ValueError):
                    pass


class Session:
    """One run of a command on a pseudo-terminal."""

    def __init__(self, argv, env, cols, stdin_tty=True):
        self.screen = Screen(cols)
        self.raw = b''
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 60, cols, 0, 0))

        def controlling():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(argv, stdin=slave if stdin_tty else subprocess.DEVNULL,
                                        stdout=slave, stderr=slave, env=env, cwd=env['HOME'],
                                        preexec_fn=controlling if stdin_tty else os.setsid, close_fds=True)
        os.close(slave)
        self.master = master

    def pump(self, seconds):
        end = time.time() + seconds
        while True:
            left = end - time.time()
            if left <= 0:
                return
            ready, _, _ = select.select([self.master], [], [], min(left, 0.05))
            if ready:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    return
                if not data:
                    return
                self.raw += data
                self.screen.feed(data.decode('utf-8', 'replace'))

    def wait_for(self, text, timeout=40):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.1)
            if re.search(text, self.screen.text()):
                self.pump(0.25)
                return
        raise TimeoutError(f'never saw {text!r}; screen:\n{self.screen.text()}')

    def send(self, keys, settle=0.25):
        os.write(self.master, keys.encode())
        self.pump(settle)

    def finish(self, timeout=60):
        end = time.time() + timeout
        while self.process.poll() is None and time.time() < end:
            self.pump(0.1)
        self.pump(0.3)
        if self.process.poll() is None:
            self.process.kill()
        return self.process.wait()


class Recorder:
    def __init__(self, out):
        self.out = out
        self.index = []

    def snap(self, session, name, description, cols, color):
        suffix = f'{cols}' + ('' if color else '-nocolor')
        base = f'{name}-{suffix}'
        with open(os.path.join(self.out, base + '.txt'), 'w') as f:
            f.write(session.screen.text())
        with open(os.path.join(self.out, base + '.ansi'), 'w') as f:
            f.write(session.screen.ansi())
        self.index.append((base, description))
        width = max((len(line) for line in session.screen.text().splitlines()), default=0)
        if width > cols:
            raise AssertionError(f'{base}: a line is {width} cells wide on a {cols}-column screen')

    def write_index(self):
        with open(os.path.join(self.out, 'index.txt'), 'w') as f:
            f.write('switchboard init review captures\n\n')
            f.write('NAME-COLUMNS.txt is plain text; NAME-COLUMNS.ansi has the colors (cat or less -R).\n')
            f.write('-nocolor files are with NO_COLOR set. Screens show everything printed so far.\n\n')
            for base, description in self.index:
                f.write(f'{base:<52} {description}\n')


# Scenarios ------------------------------------------------------------------

def join_happy(box, rec, cols, color):
    """The join path where the server can't run (a Mac): no role question.
    Text is typed the way a person types it: over the dim default, with
    backspace to correct, never a key that clears the field first."""
    directory, _ = box.server()
    invite = box.invite(directory)
    work = box.fresh(f'join-{cols}-{color}')
    s = Session([box.binary, 'init'], box.env(work, cols, color), cols)
    if sys.platform != 'darwin':
        s.wait_for('What should this machine do')
        s.send(ENTER)
    s.wait_for('Paste the invite')
    rec.snap(s, '01-join-start', 'First screen where the server cannot run: straight to the invite', cols, color)
    rec.snap(s, '02-join-invite-empty', 'Invite prompt, nothing pasted: where invites come from', cols, color)
    s.send(paste('hello there'))
    rec.snap(s, '03-join-invite-wrong', 'Invite prompt, something that is not an invite pasted', cols, color)
    s.send(backspace(len('hello there')))
    half = len(invite) // 2
    s.send(paste(invite[:half] + '\n  ' + invite[half:] + '\n'))
    rec.snap(s, '04-join-invite-pasted', 'Invite pasted with a line break (as chat apps do)', cols, color)
    s.send(ENTER)
    s.wait_for(r'\? Machine name')
    rec.snap(s, '05-join-name', 'Server checked; machine name with its default as a dim placeholder', cols, color)
    s.send('My Box', settle=0.3)
    rec.snap(s, '06-join-name-typed', 'Typing over the default: "My Box" becomes my-box as it is typed', cols, color)
    s.send(backspace(len('My Box')) + '2box', settle=0.3)
    rec.snap(s, '06b-join-name-invalid', 'A name that cannot work (a leading digit): the warning mark', cols, color)
    s.send(backspace(len('2box')) + 'review-box')
    s.send(ENTER)
    s.wait_for(r'\? Owner')
    rec.snap(s, '07-join-owner', 'Owner prompt', cols, color)
    s.send(ENTER)
    s.wait_for('Install the Claude Code plugin', timeout=60)
    rec.snap(s, '08-join-steps-plugin', 'Joined and the bridge started; plugin offer', cols, color)
    s.send(ENTER)
    s.wait_for('Add ~/bin to your PATH', timeout=30)
    rec.snap(s, '09-join-path', 'Offer to put ~/bin on PATH, with the exact line', cols, color)
    s.send(ENTER)
    s.wait_for('In each Claude Code chat you want connected', timeout=30)
    s.finish()
    rec.snap(s, '09b-join-done', 'Finished: what to do next', cols, color)
    # Rerun on the set-up machine.
    s = Session([box.binary, 'init'], box.env(work, cols, color), cols)
    s.wait_for('What would you like to do')
    rec.snap(s, '10-configured', 'Rerun on a set-up machine: state and actions', cols, color)
    s.send(ENTER)
    s.finish()
    rec.snap(s, '11-configured-nothing', 'Rerun, leaving it as it is', cols, color)
    box.stop_bridge(work)
    time.sleep(0.5)
    os.remove(os.path.join(work, 'bridge.pid')) if os.path.exists(os.path.join(work, 'bridge.pid')) else None
    s = Session([box.binary, 'init'], box.env(work, cols, color), cols)
    s.wait_for('What would you like to do')
    rec.snap(s, '12-configured-stopped', 'Rerun with the bridge stopped: Start the bridge offered first', cols, color)
    s.send(ENTER)
    s.finish()
    rec.snap(s, '13-configured-started', 'Rerun, starting the bridge', cols, color)
    box.stop_bridge(work)


def going_back(box, rec, cols, color):
    """Esc goes back one question and erases the answer."""
    work = box.fresh(f'back-{cols}-{color}')
    stub = os.path.join(work, 'server')
    os.makedirs(stub, mode=0o700)
    s = Session([box.binary, 'init'], box.env(work, cols, color, {'SWITCHBOARD_INIT_STUB_SERVER': stub}), cols)
    s.wait_for('What should this machine do')
    rec.snap(s, '14-role', 'Where the server can run: what should this machine do', cols, color)
    s.send(ENTER)
    s.wait_for('Paste the invite')
    rec.snap(s, '15-back-invite', 'After choosing Join: the invite prompt, Esc to go back', cols, color)
    s.send(ESC, settle=0.5)
    s.wait_for('What should this machine do')
    rec.snap(s, '16-back-role', 'Esc: back at the first question, its answer erased', cols, color)
    s.send(CTRL_C)
    s.finish()
    rec.snap(s, '17-stopped', 'Ctrl-C before anything was made', cols, color)


def join_errors(box, rec, cols, color):
    directory, port = box.server()
    # A wrong pin.
    invite = box.invite(directory)
    body = json.loads(__import__('base64').urlsafe_b64decode(invite[5:] + '=='))
    body['pin'] = 'ab' * 32
    forged = 'sbi1.' + __import__('base64').urlsafe_b64encode(json.dumps(body).encode()).decode().rstrip('=')
    work = box.fresh(f'pin-{cols}-{color}')
    s = Session([box.binary, 'init', '--invite'], box.env(work, cols, color), cols)
    s.wait_for('Paste the invite')
    s.send(paste(forged))
    s.send(ENTER)
    s.finish()
    rec.snap(s, '20-error-pin', "Server's certificate does not match the invite", cols, color)
    # An unreachable server.
    body['pin'] = json.loads(__import__('base64').urlsafe_b64decode(invite[5:] + '=='))['pin']
    body['url'] = f'https://127.0.0.1:{free_port()}'
    dead = 'sbi1.' + __import__('base64').urlsafe_b64encode(json.dumps(body).encode()).decode().rstrip('=')
    work = box.fresh(f'dead-{cols}-{color}')
    s = Session([box.binary, 'init', '--invite'], box.env(work, cols, color), cols)
    s.wait_for('Paste the invite')
    s.send(paste(dead))
    s.send(ENTER)
    s.finish()
    rec.snap(s, '21-error-unreachable', 'Server cannot be reached', cols, color)
    # A used invite, then a fresh one.
    used = box.invite(directory)
    subprocess.run([box.binary, 'init', '--name', f'taker-{cols}-{int(color)}', '--no-start', '--no-plugin', '--json'],
                   env=dict(box.env(box.fresh(f'taker-{cols}-{color}'), cols, color), SWITCHBOARD_INVITE=used),
                   capture_output=True, check=True)
    work = box.fresh(f'used-{cols}-{color}')
    s = Session([box.binary, 'init', '--invite', '--no-start'], box.env(work, cols, color), cols)
    s.wait_for('Paste the invite')
    s.send(paste(used))
    s.send(ENTER)
    s.wait_for(r'\? Machine name')
    s.send(ENTER)
    s.wait_for(r'\? Owner')
    s.send(ENTER)
    s.wait_for('Paste a new invite', timeout=30)
    rec.snap(s, '22-error-used-invite', 'Invite already used: asks for a new one', cols, color)
    s.send(paste(box.invite(directory)))
    s.send(ENTER)
    s.wait_for('Machine|Install the Claude|Joined as', timeout=30)
    s.send(CTRL_C)
    s.finish()
    # A taken name.
    taken = f'taker-{cols}-{int(color)}'
    work = box.fresh(f'taken-{cols}-{color}')
    s = Session([box.binary, 'init', '--invite', '--no-start', '--name', taken], box.env(work, cols, color), cols)
    s.wait_for('Paste the invite')
    s.send(paste(box.invite(directory)))
    s.send(ENTER)
    s.wait_for(r'\? Owner')
    s.send(ENTER)
    s.wait_for(r'\? Machine name', timeout=30)
    rec.snap(s, '23-error-name-taken', 'Name already taken: asks for another', cols, color)
    s.send(CTRL_C)
    s.finish()
    rec.snap(s, '24-cancelled', 'Ctrl-C at a prompt', cols, color)


def noninteractive(box, rec, cols, color):
    directory, _ = box.server()
    work = box.fresh(f'scripted-{cols}-{color}')
    env = box.env(work, cols, color)

    def run(name, description, argv, stdin=None, extra=None):
        result = subprocess.run([box.binary] + argv, env=dict(env, **(extra or {})), input=stdin,
                                capture_output=True, text=True)
        text = result.stdout + result.stderr + f'[exit status {result.returncode}]\n'
        s = Screen(cols)
        s.feed(text.replace('\n', '\r\n'))
        fake = type('S', (), {'screen': s})()
        rec.snap(fake, name, description, cols, color)

    run('30-scripted-missing', 'No terminal, no flags: says which flag is missing', ['init'])
    run('31-scripted-empty-stdin', 'No terminal, --invite with nothing on standard input',
        ['init', '--invite'], stdin='')
    run('32-scripted-join', 'No terminal: invite on standard input, every step as a settled line',
        ['init', '--invite', '-', '--name', f'scripted-{cols}-{int(color)}', '--plugin'],
        stdin=box.invite(directory) + '\n')
    run('33-scripted-json', '--json on a set-up machine', ['init', '--json'])
    box.stop_bridge(work)


def server_path(box, rec, cols, color):
    work = box.fresh(f'server-{cols}-{color}')
    stub = os.path.join(work, 'server')
    os.makedirs(stub, mode=0o700)
    s = Session([box.binary, 'init'], box.env(work, cols, color, {'SWITCHBOARD_INIT_STUB_SERVER': stub}), cols)
    s.wait_for('What should this machine do')
    s.send(DOWN)
    rec.snap(s, '40-server-role', 'Run the server highlighted (the sandbox makes it available on a Mac)', cols, color)
    s.send(ENTER)
    s.wait_for('Where will machines connect from')
    rec.snap(s, '41-server-mode', 'Private network or public domain', cols, color)
    s.send(ENTER)
    s.wait_for('Addresses machines use')
    rec.snap(s, '42-server-names', 'Private: addresses, editable', cols, color)
    s.send(ENTER)
    s.wait_for('Install the server now')
    rec.snap(s, '43-server-confirm', 'Before installing: what it does, and that it uses sudo once', cols, color)
    s.send(ENTER)
    s.wait_for('Also connect this machine', timeout=40)
    rec.snap(s, '44-server-installed', 'Installed (simulated); offer to join this machine too', cols, color)
    s.send(DOWN + ENTER)
    s.finish()
    rec.snap(s, '45-server-invite', 'Finished: the first invite, how to use it, how to make more', cols, color)
    # Both.
    work = box.fresh(f'both-{cols}-{color}')
    stub = os.path.join(work, 'server')
    os.makedirs(stub, mode=0o700)
    s = Session([box.binary, 'init', '--server', '--join', '--tls', 'private', '--yes'],
                box.env(work, cols, color, {'SWITCHBOARD_INIT_STUB_SERVER': stub}), cols)
    s.finish(timeout=90)
    rec.snap(s, '46-both-done', 'Both, from flags with --yes: server, then this machine joined', cols, color)
    box.stop_bridge(work)


def macos_role(box, rec, cols, color):
    work = box.fresh(f'mac-{cols}-{color}')
    s = Session([box.binary, 'init'], box.env(work, cols, color), cols)
    s.wait_for('Paste the invite')
    rec.snap(s, '47-role-no-server', 'Where the server cannot run: no question, one line on why', cols, color)
    s.send(CTRL_C)
    s.finish()


def acme_path(box, rec, cols, color):
    work = box.fresh(f'acme-{cols}-{color}')
    stub = os.path.join(work, 'server')
    os.makedirs(stub, mode=0o700)
    env = box.env(work, cols, color, {'SWITCHBOARD_INIT_STUB_SERVER': stub})
    s = Session([box.binary, 'init', '--server'], env, cols)
    s.wait_for('Where will machines connect from')
    s.send(DOWN)
    rec.snap(s, '50-acme-mode', 'Public domain highlighted', cols, color)
    s.send(ENTER)
    s.wait_for('Domain name')
    s.send('https://Switchboard')
    rec.snap(s, '51-acme-domain-invalid', 'Domain typed with https:// (forgiven) and without a dot', cols, color)
    s.send(backspace(len('https://Switchboard')) + 'no-such-name.invalid')
    s.send(ENTER)
    s.wait_for('What next', timeout=30)
    rec.snap(s, '52-acme-dns-missing', 'Domain does not resolve: why, the fix, and choices', cols, color)
    s.send(DOWN + ENTER)
    s.wait_for('Domain name')
    s.send('example.com')
    s.send(ENTER)
    s.wait_for('Install the server now', timeout=30)
    rec.snap(s, '53-acme-dns-elsewhere', "Domain points elsewhere: a warning, not a stop", cols, color)
    s.send(ENTER)
    s.wait_for('Done|Got a certificate|Also connect', timeout=60)
    s.send(DOWN + ENTER)
    s.finish()
    rec.snap(s, '54-acme-done', 'Certificate (simulated) and the invite', cols, color)
    # Let's Encrypt failing: a firewall.
    work = box.fresh(f'acme-fail-{cols}-{color}')
    stub = os.path.join(work, 'server')
    os.makedirs(stub, mode=0o700)
    error = ('order Invalid: API error: 203.0.113.7: Timeout during connect (likely firewall problem) '
             '(urn:ietf:params:acme:error:connection)')
    env = box.env(work, cols, color, {'SWITCHBOARD_INIT_STUB_SERVER': stub, 'SWITCHBOARD_INIT_STUB_ACME_ERROR': error})
    s = Session([box.binary, 'init', '--server', '--tls', 'acme', '--domain', 'example.com', '--yes'], env, cols)
    s.finish(timeout=60)
    rec.snap(s, '55-acme-firewall', "Let's Encrypt cannot reach port 443: the real reason and the fix", cols, color)


def other_commands(box, rec, cols, color):
    directory, _ = box.server()
    work = box.fresh(f'cmds-{cols}-{color}')
    env = box.env(work, cols, color)
    subprocess.run([box.binary, 'init', '--name', f'cmds-{cols}-{int(color)}', '--no-plugin', '--json'],
                   env=dict(env, SWITCHBOARD_INVITE=box.invite(directory)), capture_output=True, check=True)
    for name, argv, description in (('60-status', ['status'], 'switchboard status'),
                                    ('61-chats', ['chats'], 'switchboard chats'),
                                    ('62-doctor', ['doctor'], 'switchboard doctor, healthy'),
                                    ('64-help', ['help'], 'switchboard help: commands only'),
                                    ('65-uninstall-help', ['uninstall', '--help'], 'A command\'s own help'),
                                    ('66-unknown-command', ['statsu'], 'An unknown command'),
                                    ('67-leave-unknown-chat', ['leave', '--chat', '00000000-0000-0000-0000-000000000000'],
                                     'Leaving a chat ID no chat has')):
        s = Session([box.binary] + argv, env, cols)
        s.finish()
        rec.snap(s, name, description, cols, color)
    box.stop_bridge(work)
    time.sleep(0.5)
    s = Session([box.binary, 'doctor'], env, cols)
    s.finish()
    rec.snap(s, '63-doctor-stopped', 'switchboard doctor with the bridge stopped', cols, color)
    s = Session([box.binary, 'status'], env, cols)
    s.finish()
    rec.snap(s, '68-status-stopped', 'switchboard status with the bridge stopped', cols, color)
    fresh = box.fresh(f'fresh-{cols}-{color}')
    for name, argv, description in (('69-start-not-set-up', ['start'], 'start before init'),
                                    ('70-nothing', [], 'switchboard alone, before init')):
        s = Session([box.binary] + argv, box.env(fresh, cols, color), cols)
        s.finish()
        rec.snap(s, name, description, cols, color)


SCENARIOS = {
    'join': join_happy,
    'back': going_back,
    'errors': join_errors,
    'scripted': noninteractive,
    'server': server_path,
    'mac': macos_role,
    'acme': acme_path,
    'commands': other_commands,
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default=os.path.join(ROOT, 'target', 'release', 'switchboard'))
    parser.add_argument('--out', default='/tmp/switchboard-init-review')
    parser.add_argument('--only', action='append')
    parser.add_argument('--widths', default='60,80,120')
    arguments = parser.parse_args()
    os.makedirs(arguments.out, exist_ok=True)
    # Not resolved: on macOS /tmp is a link, so every HOME goes through one.
    base = tempfile.mkdtemp(prefix='sbcap-', dir='/tmp')
    box = Sandbox(os.path.realpath(arguments.binary), base)
    recorder = Recorder(arguments.out)
    variants = [(int(w), True) for w in arguments.widths.split(',')] + [(80, False)]
    failures = []
    try:
        for name, scenario in SCENARIOS.items():
            if arguments.only and name not in arguments.only:
                continue
            if name == 'mac' and sys.platform != 'darwin':
                continue
            for cols, color in variants:
                try:
                    scenario(box, recorder, cols, color)
                except Exception as error:  # report and go on
                    failures.append(f'{name} {cols} {"color" if color else "no color"}: {error}')
                    print(failures[-1], file=sys.stderr)
    finally:
        box.close()
        shutil.rmtree(base, ignore_errors=True)
        recorder.write_index()
    print(f'{len(recorder.index)} captures in {arguments.out}')
    if failures:
        print(f'{len(failures)} scenario runs failed', file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
