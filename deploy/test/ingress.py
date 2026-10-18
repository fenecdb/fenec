#!/usr/bin/env python3
"""deploy/cloudflared/config.yml's rules, asked of cloudflared itself
(`cloudflared tunnel ingress rule <url>`, in its image): the tunnel's
hostname is public, so every path the global entry refuses must meet the
edge's 404 too, however it is spelled -- percent-encoded, doubled slashes,
a query -- and a tenant's paths, as the entry sends them, the router.

    python3 deploy/test/ingress.py      # make deploy-check runs it

A tenant's replication stream got through before the rule for it: the
router forwards any /t/... path to the node, whose stream the cluster-wide
replication token opens.
"""

import os
import re
import subprocess
import sys

IMAGE = os.environ.get("CLOUDFLARED_IMAGE", "cloudflare/cloudflared:2026.6.0")
DEPLOY = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOST = "https://router-1.eu.example.com"

ROUTER = "http://10.1.0.21:8090"
REFUSED = "http_status:404"

CASES = [
    # What the entry sends a region's router.
    ("/t/acme/notes", ROUTER),
    ("/t/acme", ROUTER),
    ("/t/acme/", ROUTER),
    ("/t/acme/query?x=1", ROUTER),
    ("/t/acme/notes/changes", ROUTER),
    ("//t//acme/notes", ROUTER),
    ("/t/a_b-9/_schema", ROUTER),
    # A tenant's replication stream, however it is spelled.
    ("/t/acme/_replication", REFUSED),
    ("/t/acme/_replication?since=0&history=0", REFUSED),
    ("/t/acme/_replication/status", REFUSED),
    ("/t/acme/%5Freplication", REFUSED),
    ("/t/acme/%5freplication", REFUSED),
    ("/t/acme%2F_replication", REFUSED),
    ("/t/acme%2f%5Freplication", REFUSED),
    ("//t//acme//_replication", REFUSED),
    ("/%74/acme/_replication", REFUSED),
    # Everything else the entry refuses: the router's and the node's own
    # paths, and a name no tenant can have.
    ("/_shard/nodes", REFUSED),
    ("/_replication", REFUSED),
    ("/_replication/status", REFUSED),
    ("/_metrics", REFUSED),
    ("/_studio/", REFUSED),
    ("/_admin/stats", REFUSED),
    ("/_health", REFUSED),
    ("/", REFUSED),
    ("/t/", REFUSED),
    ("/t/ACME/notes", REFUSED),
    ("/t/acme.x/notes", REFUSED),
    ("/t/" + "a" * 65 + "/notes", REFUSED),
]


def matched(path):
    """The service the rule a URL meets sends it to."""
    p = subprocess.run(
        ["docker", "run", "--rm", "-v", f"{DEPLOY}/cloudflared:/c:ro", IMAGE,
         "tunnel", "--config", "/c/config.yml", "ingress", "rule", HOST + path],
        capture_output=True, text=True, timeout=120)
    out = p.stdout + p.stderr
    m = re.search(r"service:\s*(\S+)", out)
    if p.returncode != 0 or not m:
        raise SystemExit(f"cloudflared on {path}: exit {p.returncode}\n{out}")
    return m.group(1)


def main():
    p = subprocess.run(["docker", "run", "--rm", "-v", f"{DEPLOY}/cloudflared:/c:ro", IMAGE,
                        "tunnel", "--config", "/c/config.yml", "ingress", "validate"],
                       capture_output=True, text=True, timeout=120)
    if p.returncode != 0:
        raise SystemExit(f"cloudflared refuses the config:\n{p.stdout}{p.stderr}")
    wrong = []
    for path, want in CASES:
        got = matched(path)
        print(f"  {'ok ' if got == want else 'BAD'} {path:48} -> {got}")
        if got != want:
            wrong.append((path, want, got))
    if wrong:
        print(f"== ingress: {len(wrong)} of {len(CASES)} paths meet the wrong rule ==")
        return 1
    print(f"== ingress: {len(CASES)} paths, each where it belongs ==")
    return 0


if __name__ == "__main__":
    sys.exit(main())
