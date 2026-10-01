"""Hands every write on a fenec-server's disk to another program or a webhook.

    python -m fenecdb.relay http://127.0.0.1:8080 --consumer kafka | kcat -P -b broker -t fenec
    python -m fenecdb.relay http://127.0.0.1:8080 --consumer search --to https://indexer/hook

The writes are read as a consumer the server keeps the cursor of
(`GET /_changes?consumer=`, `fenec-server --cdc`), one JSON object a line, and
the cursor moves only once they are out: flushed to standard output, or
answered 2xx by the webhook, which is sent them as `application/x-ndjson`
and sent them again until it takes them. So each write comes at least once
-- again after a crash between the two -- and whatever takes them should
be keyed by `seq`, or by the collection and `id`.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

from . import Client, FenecError


def deliver(to: str | None, writes: list, timeout: float) -> None:
    body = "".join(json.dumps(w, separators=(",", ":")) + "\n" for w in writes)
    if to is None:
        sys.stdout.write(body)
        sys.stdout.flush()
        return
    req = urllib.request.Request(to, data=body.encode(), method="POST")
    req.add_header("Content-Type", "application/x-ndjson")
    with urllib.request.urlopen(req, timeout=timeout):
        pass


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="python -m fenecdb.relay", description=__doc__.split("\n\n")[0])
    p.add_argument("url", help="the server's HTTP endpoint")
    p.add_argument("--consumer", required=True, help="the name the server keeps the cursor under")
    p.add_argument("--to", help="a webhook to POST each batch to; standard output without")
    p.add_argument("--token", default=os.environ.get("FENEC_TOKEN"), help="the data token (or FENEC_TOKEN)")
    p.add_argument("--limit", type=int, default=1000, help="writes a batch holds at most")
    p.add_argument("--once", action="store_true", help="relay what there is, then stop")
    a = p.parse_args(argv)
    db = Client(a.url, token=a.token)
    if not any(c["name"] == a.consumer for c in db.consumers()):
        db.consumer(a.consumer)
    pause = 0.5
    batches = db.follow(a.consumer, limit=a.limit, wait=0 if a.once else 10.0)
    while True:
        try:
            if a.once:
                got = db.changes(consumer=a.consumer, limit=a.limit)
                if not got.writes:
                    return 0
                deliver(a.to, got.writes, 30.0)
                db.consumer(a.consumer, got.next)
                continue
            writes = next(batches)
            while True:
                try:
                    deliver(a.to, writes, 30.0)
                    break
                except (OSError, urllib.error.URLError) as e:
                    # Not taken: sent again, the cursor where it was.
                    print(f"relay: {a.to} did not take the batch ({e}); again in {pause:g} s", file=sys.stderr)
                    time.sleep(pause)
                    pause = min(pause * 2, 30.0)
            pause = 0.5
        except FenecError as e:
            print(f"relay: {e} ({e.status})", file=sys.stderr)
            return 1
        except BrokenPipeError:
            return 0


if __name__ == "__main__":
    sys.exit(main())
