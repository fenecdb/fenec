"""The change stream: every write on the server's disk, read as a consumer
the server keeps the cursor of (`fenec-server --cdc`)."""

import os
import subprocess
import sys

from conftest import TOKEN, URL, fresh


def mine(writes, name):
    return [w for w in writes if w["collection"] == name]


def test_follow_has_every_write_and_commits_what_the_loop_finished(client):
    name, consumer = fresh("cdc"), fresh("consumer")
    client.consumer(consumer)
    client.query(f"create collection {name} (t text)")
    client.query(f'put {name} [{{t: "a"}}, {{t: "b"}}]')
    client.query(f'set {name} {{t: "c"}} where t = "a"')
    client.query(f'del {name} where t = "b"')
    try:
        seen = []
        for batch in client.follow(consumer, wait=5):
            seen += mine(batch, name)
            if len(seen) >= 5:
                break
        assert [(w["op"], (w.get("doc") or {}).get("t")) for w in seen] == [
            ("create", None),
            ("put", "a"),
            ("put", "b"),
            ("put", "c"),
            ("del", None),
        ]
        seqs = [w["seq"] for w in seen]
        assert seqs == sorted(seqs)
        # The last batch was not committed -- the loop left before asking
        # for the next -- so it is read again.
        again = client.changes(consumer=consumer, wait=1)
        assert mine(again.writes, name)
    finally:
        client.forget_consumer(consumer)
        client.query(f"drop collection if exists {name}")


def test_a_cursor_moves_only_when_committed(client):
    name, consumer = fresh("cdc"), fresh("consumer")
    client.consumer(consumer)
    client.query(f"create collection {name} (t text)")
    try:
        first = client.changes(consumer=consumer, wait=5)
        assert first.writes
        # Where the writes end: at or past the page's last.
        assert first.seq >= first.next
        assert client.changes(consumer=consumer, wait=1).writes[0] == first.writes[0]
        client.consumer(consumer, first.next)
        listed = {c["name"]: c for c in client.consumers()}
        assert listed[consumer]["since"] == first.next
        # From a cursor it names itself, the stream goes on from there.
        assert client.changes(since=first.next - 1, limit=1).writes[0]["seq"] == first.next
    finally:
        client.forget_consumer(consumer)
        client.query(f"drop collection if exists {name}")


def test_the_relay_hands_writes_on_and_moves_the_cursor_after(client):
    name, consumer = fresh("cdc"), fresh("relay")
    client.query(f"create collection {name} (t text)")
    env = dict(os.environ, FENEC_TOKEN=TOKEN or "")
    relay = [sys.executable, "-m", "fenecdb.relay", URL, "--consumer", consumer, "--once"]
    try:
        # Made at the last write on disk: what comes after is relayed.
        subprocess.run(relay, env=env, check=True, capture_output=True, timeout=60)
        client.query(f'put {name} {{t: "relayed"}}')
        out = ""
        for _ in range(20):
            out += subprocess.run(relay, env=env, check=True, capture_output=True, text=True, timeout=60).stdout
            if "relayed" in out:
                break
        assert '"relayed"' in out
        # Handed on, then committed: not relayed twice.
        again = subprocess.run(relay, env=env, check=True, capture_output=True, text=True, timeout=60).stdout
        assert "relayed" not in again
    finally:
        client.forget_consumer(consumer)
        client.query(f"drop collection if exists {name}")
