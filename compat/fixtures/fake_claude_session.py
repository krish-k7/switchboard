#!/usr/bin/env python3
"""A stand-in for one live native Claude Code session, for compatibility tests.

It publishes the records Claude Code publishes (a registry record, a peer key
file, and a messaging socket owned by this real process), records every frame
written to its socket, and sends frames on request, so the kernel identifies it
as the sending process. Standard library only; it never imports Switchboard.

Control: one JSON command per stdin line; one JSON event per stdout line.
  {"op":"send","ref":R,"socket":PATH,"data":TEXT}  write TEXT, close the write
      side, read to EOF; reply {"event":"sent","ref":R,"ok":bool,"error"?}
  {"op":"update","fields":{...}}  replace the registry record atomically
  {"op":"exit"}  remove the published files and exit
Events: {"event":"ready",...}, {"event":"frame","lines":[...],"auth_ok":bool,
"raw":TEXT}, {"event":"probe"} for an empty connection.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import threading
from urllib.parse import quote

LOCK = threading.Lock()


def emit(value):
    with LOCK:
        sys.stdout.write(json.dumps(value) + "\n")
        sys.stdout.flush()


def process_start(pid):
    if sys.platform == "linux":
        data = Path(f"/proc/{pid}/stat").read_text()
        return data[data.rindex(")") + 2:].split()[19]
    return subprocess.check_output(["ps", "-o", "lstart=", "-p", str(pid)],
                                   env=dict(os.environ, LC_ALL="C", TZ="UTC"), text=True).strip()


def pid_domain():
    if sys.platform == "darwin":
        return "darwin"
    machine = Path("/etc/machine-id").read_text().strip()
    return "linux:" + machine + ":" + os.readlink("/proc/self/ns/pid")


def write_private(path, value, mode=0o600):
    temporary = path.with_name("." + path.name + ".tmp")
    fd = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY, mode)
    with os.fdopen(fd, "w") as stream:
        json.dump(value, stream)
        stream.write("\n")
    os.replace(temporary, path)


def serve(listener, token):
    while True:
        try:
            conn, _ = listener.accept()
        except OSError:
            return
        with conn:
            conn.settimeout(5)
            chunks = []
            try:
                while True:
                    data = conn.recv(65536)
                    if not data:
                        break
                    chunks.append(data)
            except OSError:
                pass
            raw = b"".join(chunks)
            if not raw:
                emit({"event": "probe"})
                continue
            lines = []
            for line in raw.split(b"\n"):
                if line:
                    try:
                        lines.append(json.loads(line))
                    except ValueError:
                        lines.append({"unparsed": line.decode(errors="replace")})
            auth = lines[0] if lines else {}
            emit({"event": "frame", "lines": lines, "raw": raw.decode(errors="replace"),
                  "auth_ok": isinstance(auth, dict) and auth.get("type") == "auth" and auth.get("token") == token})


def send(command):
    try:
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(5)
            conn.connect(command["socket"])
            conn.sendall(command["data"].encode())
            conn.shutdown(socket.SHUT_WR)
            while conn.recv(4096):
                pass
        emit({"event": "sent", "ref": command.get("ref"), "ok": True})
    except OSError as error:
        emit({"event": "sent", "ref": command.get("ref"), "ok": False, "error": str(error)})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config-dir", required=True)
    parser.add_argument("--socket-dir", required=True)
    parser.add_argument("--session-id", required=True)
    parser.add_argument("--fields", default="{}", help="extra or overriding registry fields (JSON)")
    args = parser.parse_args()
    pid = os.getpid()
    sessions = Path(args.config_dir) / "sessions"
    path = Path(args.socket_dir) / f"{pid}.sock"
    token = secrets.token_hex(16)
    start, domain = process_start(pid), pid_domain()
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(path))
    listener.listen(16)
    record = dict(pid=pid, sessionId=args.session_id, procStart=start, pidDomain=domain, peerProtocol=1,
                  kind="interactive", startedAt=0, messagingSocketPath=str(path))
    record.update(json.loads(args.fields))
    registry = sessions / f"{pid}.json"
    key = sessions / f"{pid}.{hashlib.sha256(str(path).encode()).hexdigest()}.key"
    try:
        write_private(key, dict(peerToken=token, procStart=start, pidDomain=domain))
        write_private(registry, record)
        threading.Thread(target=serve, args=(listener, token), daemon=True).start()
        emit({"event": "ready", "pid": pid, "token": token, "socket": str(path),
              "address": "uds:" + quote(str(path), safe="/:._-\\"), "record": record})
        for line in sys.stdin:
            command = json.loads(line)
            if command.get("op") == "send":
                send(command)
            elif command.get("op") == "update":
                record = dict(record, **command["fields"])
                for name in command.get("remove", []):
                    record.pop(name, None)
                write_private(registry, record)
                emit({"event": "updated", "ref": command.get("ref")})
            elif command.get("op") == "exit":
                break
    finally:
        listener.close()
        for item in (registry, key, path):
            try:
                item.unlink()
            except FileNotFoundError:
                pass


if __name__ == "__main__":
    main()
