"""Test TLS identities made with the openssl CLI, once per test process.

The direct-TLS server mode requires client certificates issued by its client CA,
so every test machine gets a CA-signed P-256 certificate. The server certificate
names localhost and 127.0.0.1. Nothing here touches real Switchboard identities.
"""
import atexit
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import uuid

_LOCK = threading.Lock()
_ROOT = None


def _openssl(*args, cwd):
    result = subprocess.run(['openssl', *map(str, args)], cwd=cwd, capture_output=True)
    if result.returncode:
        raise RuntimeError('openssl failed: ' + result.stderr.decode(errors='replace'))
    return result.stdout


def root():
    global _ROOT
    with _LOCK:
        if _ROOT is None:
            path = Path(tempfile.mkdtemp(prefix='sbc-pki-', dir='/tmp')).resolve()
            os.chown(path, -1, os.getegid())
            path.chmod(0o700)
            atexit.register(shutil.rmtree, path, True)
            (path / 'ca.cnf').write_text(
                '[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n[dn]\nCN=Switchboard compat CA\n'
                '[v3]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n'
                'subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid:always\n')
            _openssl('genpkey', '-algorithm', 'EC', '-pkeyopt', 'ec_paramgen_curve:P-256', '-out', 'ca.key', cwd=path)
            _openssl('req', '-new', '-x509', '-key', 'ca.key', '-days', '2', '-sha256', '-config', 'ca.cnf',
                     '-out', 'ca.pem', cwd=path)
            _issue(path, 'server', 'localhost', 'extendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n')
            for name in ('ca.key', 'ca.pem'):
                (path / name).chmod(0o600)
            _ROOT = path
        return _ROOT


def _issue(path, name, cn, extensions):
    _openssl('genpkey', '-algorithm', 'EC', '-pkeyopt', 'ec_paramgen_curve:P-256', '-out', name + '.key', cwd=path)
    _openssl('req', '-new', '-key', name + '.key', '-subj', '/CN=' + cn, '-out', name + '.csr', cwd=path)
    (path / (name + '.ext')).write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n'
                                         'subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n' + extensions)
    _openssl('x509', '-req', '-in', name + '.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key', '-set_serial',
             '0x' + uuid.uuid4().hex, '-days', '2', '-sha256', '-extfile', name + '.ext', '-out', name + '.pem', cwd=path)
    for suffix in ('.key', '.pem'):
        (path / (name + suffix)).chmod(0o600)
    (path / (name + '.csr')).unlink()
    (path / (name + '.ext')).unlink()


def ca():
    return root() / 'ca.pem'


def server():
    base = root()
    return base / 'server.pem', base / 'server.key'


def fingerprint(cert):
    der = _openssl('x509', '-in', cert, '-outform', 'DER', cwd=Path(cert).parent)
    return hashlib.sha256(der).hexdigest()


def client(name, self_signed=False):
    """A client identity: (cert path, key path, SHA-256 DER fingerprint)."""
    base = root()
    label = name + '-' + uuid.uuid4().hex[:8]
    with _LOCK:
        if self_signed:
            _openssl('req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
                     '-keyout', label + '.key', '-out', label + '.pem', '-subj', '/CN=' + name, '-days', '2', cwd=base)
            for suffix in ('.key', '.pem'):
                (base / (label + suffix)).chmod(0o600)
        else:
            _issue(base, label, name, 'extendedKeyUsage=clientAuth\n')
    cert, key = base / (label + '.pem'), base / (label + '.key')
    return cert, key, fingerprint(cert)
