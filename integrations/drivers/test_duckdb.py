"""DuckDB over fenec-pg's pg wire, through its own postgres extension: a
collection attached as a table and read whole, aggregated, filtered -- the
conditions pushed down into the COPY it reads with -- and written out as
Parquet, which is how fenecdb's rows reach the tools that read Parquet."""

import os
import uuid

import duckdb
import psycopg
from psycopg.conninfo import conninfo_to_dict

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")


def attached():
    d = conninfo_to_dict(DSN)
    con = duckdb.connect()
    con.execute("INSTALL postgres; LOAD postgres;")
    dsn = " ".join(f"{k}={d[k]}" for k in ("host", "port", "user", "password", "dbname") if k in d)
    con.execute(f"ATTACH '{dsn}' AS f (TYPE postgres, READ_ONLY)")
    return con


def test_duckdb_reads_a_collection_as_a_table(tmp_path):
    t = f"dk_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"create collection {t} (n int @hash, g text, x float, tags [text], e vector<3>)")
        for start in range(0, 20_000, 5_000):
            rows = ", ".join(
                f'{{n: {i}, g: "g{i % 7}", x: {i / 4}, tags: ["a", "{i % 3}"], e: [{i}, 1, 0]}}'
                for i in range(start, start + 5_000)
            )
            c.execute(f"put {t} [{rows}]")
    con = attached()
    table = f"f.public.{t}"
    assert con.execute(f"SELECT count(*), sum(n), max(x) FROM {table}").fetchone() == (20_000, 199_990_000, 4999.75)
    groups = con.execute(f"SELECT g, count(*) FROM {table} GROUP BY g ORDER BY g").fetchall()
    assert len(groups) == 7 and sum(n for _, n in groups) == 20_000
    # Pushed down: `"n" < '10'` inside the COPY, `'10'` read as an int.
    assert con.execute(f"SELECT count(*) FROM {table} WHERE n < 10").fetchone() == (10,)
    assert con.execute(f"SELECT n FROM {table} WHERE g = 'g3' ORDER BY n LIMIT 2").fetchall() == [(3,), (10,)]
    # A list is DuckDB's list; a vector, whose type it has no reader for,
    # its text.
    assert con.execute(f"SELECT tags, e FROM {table} WHERE n = 4").fetchone() == (["a", "1"], "[4,1,0]")

    out = tmp_path / "rows.parquet"
    con.execute(f"COPY (SELECT n, g, x FROM {table}) TO '{out}' (FORMAT parquet)")
    assert duckdb.connect().execute(f"SELECT count(*), sum(n) FROM '{out}'").fetchone() == (20_000, 199_990_000)
