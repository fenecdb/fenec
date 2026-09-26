"""psycopg and SQLAlchemy over fenec-pg's pg wire. Both make a nested
transaction a savepoint -- psycopg quoting its name, `SAVEPOINT "_pg3_1"`,
SQLAlchemy not -- and take a failure inside one back to it with
`ROLLBACK TO`, the transaction going on."""

import os
import uuid

import psycopg
import pytest
import sqlalchemy
from psycopg import pq
from psycopg.conninfo import conninfo_to_dict
from sqlalchemy import create_engine, text
from sqlalchemy.orm import Session

DSN = os.environ.get("FENEC_PG", "host=127.0.0.1 port=5433 user=fenec dbname=fenec")


@pytest.fixture()
def table() -> str:
    """A collection no other test uses."""
    name = f"t_{uuid.uuid4().hex[:12]}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"create collection {name} (name text)")
    return name


@pytest.fixture()
def engine() -> sqlalchemy.Engine:
    d = conninfo_to_dict(DSN)
    url = sqlalchemy.URL.create(
        "postgresql+psycopg",
        username=d["user"],
        password=d.get("password"),
        host=d["host"],
        port=int(d["port"]),
        database=d["dbname"],
    )
    return create_engine(url)


def landed(table: str) -> list[str]:
    """What another session reads: the writes that landed."""
    with psycopg.connect(DSN, autocommit=True) as c:
        return [r[0] for r in c.execute(f"get {table} select name").fetchall()]


def test_psycopg_nested_transactions_are_savepoints(table):
    with psycopg.connect(DSN) as c:
        with c.transaction():
            c.execute(f"put {table} {{name: 'outer'}}")
            with pytest.raises(RuntimeError):
                with c.transaction():
                    c.execute(f"put {table} {{name: 'inner'}}")
                    raise RuntimeError("put back")
            with c.transaction():
                c.execute(f"put {table} {{name: 'kept'}}")
            with pytest.raises(psycopg.errors.DatatypeMismatch):
                with c.transaction():
                    c.execute(f"put {table} {{name: 'half'}}")
                    c.execute(f"put {table} {{name: 1}}")
            # Taken back to its savepoint, the failed transaction goes on.
            assert c.info.transaction_status == pq.TransactionStatus.INTRANS
            with c.transaction():
                c.execute(f"put {table} {{name: 'deep'}}")
                with c.transaction():
                    c.execute(f"put {table} {{name: 'deeper'}}")
                    raise psycopg.Rollback()
            c.execute(f"put {table} {{name: 'after'}}")
    assert landed(table) == ["outer", "kept", "deep", "after"]


def test_psycopg_pipeline_is_taken_back_to_a_savepoint(table):
    with psycopg.connect(DSN, autocommit=True) as c:
        with c.pipeline():
            with c.transaction():
                c.execute(f"put {table} {{name: 'outer'}}")
                with pytest.raises(psycopg.errors.DatatypeMismatch):
                    with c.transaction():
                        c.execute(f"put {table} {{name: 'inner'}}")
                        c.execute(f"put {table} {{name: 2}}")
                        c.execute(f"get {table} count").fetchone()
                c.execute(f"put {table} {{name: 'after'}}")
    assert landed(table) == ["outer", "after"]


def test_sqlalchemy_begin_nested_is_a_savepoint(table, engine):
    with engine.connect() as conn:
        with conn.begin():
            conn.exec_driver_sql(f"put {table} {{name: 'outer'}}")
            with pytest.raises(RuntimeError):
                with conn.begin_nested():
                    conn.exec_driver_sql(f"put {table} {{name: 'inner'}}")
                    raise RuntimeError("put back")
            nested = conn.begin_nested()
            conn.exec_driver_sql(f"put {table} {{name: 'kept'}}")
            nested.commit()
            with pytest.raises(sqlalchemy.exc.DBAPIError):
                with conn.begin_nested():
                    conn.exec_driver_sql(f"put {table} {{name: 3}}")
            conn.exec_driver_sql(f"put {table} {{name: 'after'}}")
    assert landed(table) == ["outer", "kept", "after"]


def test_sqlalchemy_session_begin_nested(table, engine):
    with Session(engine) as s:
        with s.begin():
            s.execute(text(f"put {table} {{name: 'outer'}}"))
            savepoint = s.begin_nested()
            s.execute(text(f"put {table} {{name: 'inner'}}"))
            savepoint.rollback()
            with s.begin_nested():
                s.execute(text(f"put {table} {{name: 'kept'}}"))
            with pytest.raises(sqlalchemy.exc.DBAPIError):
                with s.begin_nested():
                    s.execute(text(f"put {table} {{name: 4}}"))
            s.execute(text(f"put {table} {{name: 'after'}}"))
    assert landed(table) == ["outer", "kept", "after"]
