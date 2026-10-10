"""Notes over fenec-server's HTTP endpoint, with the fenecdb client.

    python notes.py                      seeds if empty, then lists
    python notes.py add <title> <body> [tag ...]
    python notes.py list [--tag T] [--open]
    python notes.py search <words>
    python notes.py done <id>
    python notes.py watch                needs fenec-server --cdc
    python notes.py smoke                what CI runs
"""

import math
import os
import pathlib
import sys
import time
from datetime import datetime, timezone

from fenecdb import Client

URL = os.environ.get("FENEC_URL", "http://127.0.0.1:8080")
TOKEN = os.environ.get("FENEC_TOKEN", "secret")  # a dev default; never ship one
SCHEMA = (pathlib.Path(__file__).parent / "schema.fenecql").read_text()

SEEDS = [
    ("Groceries", "Buy milk, eggs and fresh bread for the weekend.", ["home", "shopping"], False, "2026-09-28T09:00:00Z"),
    ("Release checklist", "Tag the release, publish the packages and update the docs.", ["work"], False, "2026-09-29T09:00:00Z"),
    ("Book flights", "Find cheap flights to Istanbul for the conference in spring.", ["travel", "work"], True, "2026-09-30T09:00:00Z"),
    ("Book club", "Finish the novel about the desert fox before Thursday.", ["home", "reading"], False, "2026-10-01T09:00:00Z"),
]


def embed(text: str) -> list[float]:
    """A TOY embedding, a placeholder for a real model: hashed character
    trigrams (FNV-1a over the UTF-8 bytes) into 64 dimensions. It matches
    spelling, not meaning. A real one is an API call or a local model
    (sentence-transformers) with the field's dimension changed to match."""
    data = (" " + text.translate(str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")) + " ").encode()
    v = [0.0] * 64
    for i in range(len(data) - 2):
        h = 0x811C9DC5
        for b in data[i : i + 3]:
            h = ((h ^ b) * 0x01000193) & 0xFFFFFFFF
        v[h % 64] += 1
    norm = math.sqrt(sum(x * x for x in v))
    return [x / norm for x in v] if norm else v


def connect() -> Client:
    db = Client(URL, token=TOKEN)
    db.schema(SCHEMA, migrate=True)  # makes what is missing; refuses what would lose data
    return db


def add(db, title, body, tags, done=False, at=None):
    at = at or datetime.now(timezone.utc).isoformat()
    db.collection("notes").insert(
        {"title": title, "body": body, "tags": tags, "done": done, "at": at, "embed": embed(f"{title} {body}")}
    )


def seed(db):
    if db.collection("notes").count() == 0:
        for note in SEEDS:
            add(db, *note)


def listed(db, tag=None, open_only=False):
    q = db.collection("notes").select("id", "title", "tags", "done", "at").order("at", "desc").limit(20)
    if tag:
        q = q.where("tags", "has", tag)
    if open_only:
        q = q.where("done", False)
    return q.rows()


def search(db, words):
    notes = db.collection("notes").select("id", "title")
    return {
        "match": notes.match("body", words).limit(5).rows(),
        "fuse": notes.match("body", words).near("embed", embed(words)).fuse().limit(5).rows(),
    }


def show(rows):
    for r in rows:
        mark = "x" if r.get("done") else " "
        print(f"[{mark}] {r['id']:>3}  {r['title']:<20} {', '.join(r.get('tags') or [])}")
    sys.stdout.flush()  # watch's output, piped, goes out as it comes


def watch(db):
    """Lists the open notes again after every write to them. The client has
    no live queries; the server's change stream (--cdc) stands in."""
    since = db.changes().next
    show(listed(db, open_only=True))
    while True:
        got = db.changes(since, wait=10)
        since = got.next
        if any(w["collection"] == "notes" for w in got.writes):
            print("--")
            show(listed(db, open_only=True))


def smoke(db):
    def check(step, ok):
        print(("ok   " if ok else "FAIL ") + step)
        if not ok:
            sys.exit(1)

    notes = db.collection("notes")
    seed(db)
    check("seeded 4 notes", notes.count() == 4)
    e = embed("hello")
    check("toy embedding", [i for i, x in enumerate(e) if x] == [24, 36, 46, 48, 62])
    check("newest first", listed(db)[0]["title"] == "Book club")
    check("by tag", [r["title"] for r in listed(db, tag="work")] == ["Book flights", "Release checklist"])
    check("open", len(listed(db, open_only=True)) == 3)
    check("match", notes.match("body", "release docs").first()["title"] == "Release checklist")
    check("near", notes.near("embed", embed("flights to Istanbul")).first()["title"] == "Book flights")
    check("fuse", search(db, "desert fox")["fuse"][0]["title"] == "Book club")
    # "From now" is the last write on disk, and the seeds may not be there
    # yet: an answer can hold those and not the note, so read on, from where
    # each answer ended, until the note comes or 5 s have passed.
    since = db.changes().next
    add(db, "Call mom", "Ask about the weekend.", ["home"])
    deadline, seen = time.monotonic() + 5, False
    while not seen and time.monotonic() < deadline:
        got = db.changes(since, wait=max(0.1, deadline - time.monotonic()))
        since = got.next
        seen = any((w.get("doc") or {}).get("title") == "Call mom" for w in got.writes)
    check("change stream", seen)
    notes.where("title", "Groceries").update({"done": True})
    check("done", len(listed(db, open_only=True)) == 3)


def main(args):
    db = connect()
    cmd = args[0] if args else None
    if cmd == "add":
        add(db, args[1], args[2], args[3:])
    elif cmd == "list":
        show(listed(db, tag=args[args.index("--tag") + 1] if "--tag" in args else None, open_only="--open" in args))
    elif cmd == "search":
        found = search(db, " ".join(args[1:]))
        print("match:", ", ".join(r["title"] for r in found["match"]))
        print("fuse: ", ", ".join(r["title"] for r in found["fuse"]))
    elif cmd == "done":
        db.collection("notes").where("id", int(args[1])).update({"done": True})
    elif cmd == "watch":
        watch(db)
    elif cmd == "smoke":
        smoke(db)
    else:
        seed(db)
        show(listed(db))


if __name__ == "__main__":
    main(sys.argv[1:])
