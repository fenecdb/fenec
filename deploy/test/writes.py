#!/usr/bin/env python3
"""Rows of 64 KB put into a tenant through a router, back to back, until a
file appears or a time runs out: the archive's newest segment grows the
whole while, which is what the bucket sync must copy. Its token read out of
an environment file, as ask.py reads one.

    writes.py URL ENVFILE:VARIABLE STOPFILE SECONDS FIRST_ID
"""

import http.client
import json
import os
import sys
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ask import token  # noqa: E402


def main():
    url, spec, stop, seconds, first = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4]), int(sys.argv[5])
    u = urllib.parse.urlsplit(url)
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=30)
    headers = {"Authorization": "Bearer " + token(spec), "Content-Type": "application/json"}
    filler = "x" * 65536
    end = time.monotonic() + seconds
    i = first
    while time.monotonic() < end and not os.path.exists(stop):
        body = json.dumps({"query": "put notes {id: $1, n: $1, title: $2}", "params": [i, filler]})
        c.request("POST", u.path, body=body, headers=headers)
        r = c.getresponse()
        r.read()
        i += 1
    print(i - first)


if __name__ == "__main__":
    main()
