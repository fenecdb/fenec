#!/usr/bin/env python3
"""One HTTP request from inside a test machine, the token read out of an
environment file the units read -- so no token is on this command line
either, and the check that `ps` shows none holds while the test runs.

    ask.py METHOD URL [ENVFILE:VARIABLE [BODY]]

Prints the status, a line, then the body; exit status 0 whatever the
status, 1 when the server was not reached.
"""

import http.client
import sys
import urllib.parse


def token(spec):
    path, name = spec.rsplit(":", 1)
    for line in open(path):
        line = line.strip()
        if line.startswith(name + "="):
            return line.split("=", 1)[1]
    raise SystemExit(f"no {name} in {path}")


def main():
    method, url = sys.argv[1], sys.argv[2]
    headers = {}
    if len(sys.argv) > 3 and sys.argv[3]:
        headers["Authorization"] = "Bearer " + token(sys.argv[3])
    body = sys.argv[4] if len(sys.argv) > 4 else None
    if body is not None:
        headers["Content-Type"] = "application/json"
    u = urllib.parse.urlsplit(url)
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=30)
    try:
        c.request(method, u.path + (f"?{u.query}" if u.query else ""), body=body, headers=headers)
        r = c.getresponse()
        print(r.status)
        print(r.read().decode("utf-8", "replace"))
    except OSError as e:
        print(f"unreached: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
