"""COPY FROM STDIN over fenec-pg's pg wire, as psycopg sends it: rows
written one at a time (`write_row`), which psycopg puts in COPY's text
format, or blocks of text as a file holds them (`write`). A COPY is one
command: a bad row, or an exception inside the block, lands none of it."""

import os
import uuid
from datetime import datetime, timezone

import psycopg
import pytest

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")

ROWS = 25_000


@pytest.fixture()
def table() -> str:
    """A collection no other test uses."""
    name = f"c_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(
            f"create collection {name} (name text, n int, score float, "
            f"tags [text], e vector<3> @hnsw(cosine), at timestamp)"
        )
    return name


def count(table: str) -> int:
    """What another session reads: the rows that landed."""
    with psycopg.connect(DSN, autocommit=True) as c:
        return c.execute(f"get {table} count").fetchone()[0]


def test_psycopg_copy_writes_rows(table):
    at = datetime(2026, 9, 28, 12, 30, tzinfo=timezone.utc)
    with psycopg.connect(DSN) as c:
        with c.cursor() as cur:
            with cur.copy(f"COPY {table} (name, n, score, tags, e, at) FROM STDIN") as copy:
                for i in range(ROWS):
                    # A vector as pgvector writes one, or a list of floats,
                    # which psycopg writes as an array.
                    e = f"[{i},1,0]" if i % 2 else [float(i), 1.0, 0.0]
                    copy.write_row((f"row\t{i}", i, i / 4, ["a", f"b {i}"], e, at))
                copy.write_row((None, None, None, None, None, None))
            assert cur.rowcount == ROWS + 1
    assert count(table) == ROWS + 1
    with psycopg.connect(DSN, autocommit=True) as c:
        row = c.execute(
            f"get {table} select name, n, score, tags, at where n = 7"
        ).fetchone()
        assert row == ("row\t7", 7, 1.75, ["a", "b 7"], at)
        # The vectors went into the graph.
        hit = c.execute(f"get {table} select n near e [0, 1, 0] limit 1").fetchone()
        assert hit[0] == 0


def test_psycopg_copy_writes_blocks_of_csv(table):
    with psycopg.connect(DSN, autocommit=True) as c:
        with c.cursor() as cur:
            with cur.copy(
                f"COPY {table} (name, n) FROM STDIN WITH (FORMAT csv, HEADER)"
            ) as copy:
                copy.write("name,n\n")
                copy.write('"a, quoted",1\nplain,')
                copy.write("2\n,3\n")
            assert cur.rowcount == 3
        rows = c.execute(f"get {table} select name, n").fetchall()
    assert rows == [("a, quoted", 1), ("plain", 2), (None, 3)]


def test_psycopg_copy_that_fails_lands_nothing(table):
    with pytest.raises(psycopg.errors.InvalidTextRepresentation):
        with psycopg.connect(DSN) as c:
            with c.cursor().copy(f"COPY {table} (name, n) FROM STDIN") as copy:
                for i in range(ROWS):
                    copy.write_row((f"r{i}", i))
                copy.write_row(("bad", "not a number"))
    assert count(table) == 0

    # An exception inside the block is the client's CopyFail.
    with pytest.raises(RuntimeError):
        with psycopg.connect(DSN, autocommit=True) as c:
            with c.cursor().copy(f"COPY {table} (name, n) FROM STDIN") as copy:
                for i in range(ROWS):
                    copy.write_row((f"r{i}", i))
                raise RuntimeError("the source went away")
    assert count(table) == 0
