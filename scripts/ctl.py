#!/usr/bin/env python3
"""minimal client for the blkrevertd control socket."""
import json
import socket
import sys


def call(sock_path, op, **kw):
    req = dict(op=op, **kw)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(30)
    s.connect(sock_path)
    s.sendall((json.dumps(req) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    s.close()
    return json.loads(buf.decode())


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print("usage: ctl.py <socket> <op> [k=v ...]", file=sys.stderr)
        raise SystemExit(2)
    kwargs = {}
    for arg in sys.argv[3:]:
        k, _, v = arg.partition("=")
        try:
            kwargs[k] = json.loads(v)
        except json.JSONDecodeError:
            kwargs[k] = v
    resp = call(sys.argv[1], sys.argv[2], **kwargs)
    print(json.dumps(resp, indent=2))
    raise SystemExit(0 if resp.get("ok") else 1)
