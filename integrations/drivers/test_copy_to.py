"""COPY TO STDOUT over fenec-pg's pg wire, as psycopg and asyncpg read it:
a collection's rows in text, CSV and binary, typed by the client as it
types PostgreSQL's, a query's rows, and a table's COPY out loaded into
another's COPY in."""

import asyncio
import io
import os
import uuid
from datetime import datetime, timedelta, timezone

import asyncpg
import psycopg
import pytest
from psycopg.conninfo import conninfo_to_dict

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")

ROWS = 2_500
COLUMNS = "name, n, score, tags, at"
TYPES = ["text", "int8", "float8", "text[]", "timestamptz"]
AT = datetime(2026, 9, 28, 12, 30, tzinfo=timezone.utc)


def schema(name: str) -> str:
    return f"create collection {name} (name text, n int, score float, tags [text], at timestamp)"


@pytest.fixture()
def table() -> str:
    """A collection of ROWS rows, some with awkward text or a NULL, and
    holes in its ids across the first page of a COPY TO."""
    name = f"o_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(schema(name))
        awkward = ["tab\there", "line\nend", "back\\slash", 'q"uote, comma', "", "\\N"]
        rows = []
        for i in range(ROWS):
            rows.append(
                (
                    awkward[i % len(awkward)] if i % 3 == 0 else f"row {i}",
                    i,
                    None if i % 7 == 0 else i / 4,
                    ["a b", str(i), ""],
                    AT + timedelta(seconds=i),
                )
            )
        with c.cursor() as cur:
            cur.executemany(
                f"put {name} {{name: %s, n: %s, score: %s, tags: %s, at: %s}}",
                rows,
            )
        gone = [str(i) for i in range(ROWS) if i % 11 == 5 or 990 <= i < 1010]
        c.execute(f"del {name} where n in [{', '.join(gone)}]")
    return name


def expected(table: str):
    with psycopg.connect(DSN, autocommit=True) as c:
        return c.execute(f"get {table} select {COLUMNS} order n").fetchall()


@pytest.mark.parametrize("fmt", ["text", "binary"])
def test_psycopg_reads_rows_typed(table, fmt):
    want = expected(table)
    with psycopg.connect(DSN, autocommit=True) as c:
        with c.cursor() as cur:
            with cur.copy(f"COPY {table} ({COLUMNS}) TO STDOUT (FORMAT {fmt})") as copy:
                copy.set_types(TYPES)
                got = list(copy.rows())
            assert cur.rowcount == len(want)
    assert got == want


def test_psycopg_copies_a_table_into_another(table):
    other = f"i_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(schema(other))
    with psycopg.connect(DSN, autocommit=True) as src, psycopg.connect(DSN, autocommit=True) as dst:
        with src.cursor().copy(f"COPY {table} ({COLUMNS}) TO STDOUT (FORMAT csv)") as copy_out:
            with dst.cursor().copy(f"COPY {other} ({COLUMNS}) FROM STDIN (FORMAT csv)") as copy_in:
                for block in copy_out:
                    copy_in.write(block)
    assert expected(other) == expected(table)


def test_psycopg_reads_a_querys_rows(table):
    with psycopg.connect(DSN, autocommit=True) as c:
        want = c.execute(f"get {table} select name, n where n >= 100 order n desc limit 10").fetchall()
        with c.cursor().copy(
            f"COPY (get {table} select name, n where n >= 100 order n desc limit 10) TO STDOUT"
        ) as copy:
            copy.set_types(["text", "int8"])
            assert list(copy.rows()) == want


def test_asyncpg_copies_a_table_and_a_query_out(table):
    want = expected(table)

    async def go():
        d = conninfo_to_dict(DSN)
        c = await asyncpg.connect(
            host=d["host"],
            port=int(d["port"]),
            user=d["user"],
            password=d.get("password"),
            database=d["dbname"],
        )
        try:
            buf = io.BytesIO()
            res = await c.copy_from_table(
                table, columns=["name", "n"], output=buf, format="csv", header=True
            )
            assert res == f"COPY {len(want)}"
            lines = buf.getvalue().decode()
            assert lines.startswith("name,n\n")
            # A query's rows in binary, which asyncpg wraps in COPY (...).
            buf = io.BytesIO()
            res = await c.copy_from_query(
                f"get {table} select n where n < 50 order n", output=buf, format="binary"
            )
            n = sum(1 for r in want if r[1] < 50)
            assert res == f"COPY {n}"
            assert buf.getvalue().startswith(b"PGCOPY\n\xff\r\n\0")
        finally:
            await c.close()

    asyncio.run(go())
