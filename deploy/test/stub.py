#!/usr/bin/env python3
"""Stands in for cloudflared and rclone in the units' test, installed as
/usr/bin/cloudflared and /usr/bin/rclone: the test holds the units -- their
credentials, sandbox and the way systemd starts them -- not those programs,
which a test machine has no account or bucket for.

It says what it was given, reads every file its arguments name that it
should be able to read under the unit's sandbox, and exits 1 when one does
not read. As cloudflared (Type=notify) it then tells systemd it is up and
stays; as rclone it exits.
"""

import os
import socket
import sys
import time

name = os.path.basename(sys.argv[0])
args = sys.argv[1:]
print(f"{name} stub: {' '.join(args)}", flush=True)


def after(flag):
    return args[args.index(flag) + 1]


def readable(path):
    try:
        if os.path.isdir(path):
            entries = os.listdir(path)
            print(f"{name} stub: {path} lists {len(entries)} entries", flush=True)
        else:
            with open(path, "rb") as f:
                n = len(f.read())
            print(f"{name} stub: {path} reads, {n} bytes", flush=True)
        return True
    except OSError as e:
        print(f"{name} stub: {path} does not read: {e}", flush=True)
        return False


if name == "cloudflared":
    ok = readable(after("--config")) and readable(after("--credentials-file"))
    if not ok:
        sys.exit(1)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    where = os.environ["NOTIFY_SOCKET"]
    s.connect("\0" + where[1:] if where.startswith("@") else where)
    s.send(b"READY=1")
    print("cloudflared stub: ready", flush=True)
    while True:
        time.sleep(3600)
elif name == "rclone":
    # rclone sync --config <file> --cache-dir <dir> <source> <remote>
    source = args[-2]
    ok = readable(after("--config")) and readable(source)
    sys.exit(0 if ok else 1)
else:
    sys.exit(f"stub.py does not stand in for {name}")
