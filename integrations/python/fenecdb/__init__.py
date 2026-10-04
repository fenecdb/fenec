"""fenecdb over HTTP.

A client for fenec-server's HTTP endpoint (`fenec-server --http <address>`), and on
top of it vector stores for LangChain (`fenecdb.langchain`) and LlamaIndex
(`fenecdb.llama_index`).

    from fenecdb import Client
    db = Client("http://127.0.0.1:8080", token="...")
    db.query("get articles near embed $1 limit 5", [[0.1, 0.2, 0.3]])

The client is the standard library alone, as the server is: a statement is
one `POST /query`, several are one `POST /batch` under one lock.

`collection` starts the query builder, which writes the statement for you
-- every value a parameter, every name checked -- and makes the same text
the JavaScript, Go and .NET builders make of the same chain:

    from fenecdb import or_
    rows = (db.collection("articles").select("title")
              .where("year", ">=", 2024)
              .where(or_({"lang": "tr"}, {"tags": {"has": "rust"}}))
              .near("embed", [0.1, 0.2, 0.3]).limit(5).rows())

`match` takes `highlight` and `snippet` marks in the select list, and any
read `facet` counts, which come back beside the rows as `rows.facets`:

    rows = (db.collection("products").select("title").highlight("title")
              .match("title", "phone").facet("brand", top=10).limit(20).rows())
    rows.facets["brand"]   # [FacetCount(value="acme", count=12), ...]

`AsyncClient` is the same over asyncio, for an event loop a blocking
request would stall:

    async with AsyncClient("http://127.0.0.1:8080", token="...") as db:
        rows = await db.query("get articles near embed $1 limit 5", [[0.1, 0.2, 0.3]])

`Client.follow` reads every write on the server's disk (`GET /_changes`,
`fenec-server --cdc`) for a consumer whose cursor the server keeps, and
commits each batch once the loop comes back for the next:

    for batch in db.follow("search-index"):
        for change in batch:
            index(change)          # an exception: this batch is read again

`python -m fenecdb.relay` hands the same to another program or a webhook.
"""

from __future__ import annotations

import asyncio
import http.client
import json
import ssl
import threading
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Iterable, Iterator, NamedTuple, Sequence

__all__ = [
    "AsyncClient",
    "AsyncQuery",
    "Changes",
    "Client",
    "Computed",
    "Cond",
    "FacetCount",
    "FenecError",
    "Query",
    "Rows",
    "SchemaError",
    "and_",
    "collection",
    "expr",
    "inc",
    "not_",
    "or_",
    "placeholders",
    "raw",
]


class FenecError(Exception):
    """A statement the server refused: its message, and the HTTP status --
    0 for one the query builder refused before sending anything."""

    def __init__(self, message: str, status: int):
        super().__init__(message)
        self.status = status


class SchemaError(FenecError):
    """The schema the code declares and the server's differ in what no
    open applies: each difference and how to resolve it, as the engine
    writes them, in `refusals`."""

    def __init__(self, refusals: list, status: int):
        self.refusals = refusals
        lines = [f"  - {r['message']}\n    {r['fix']}" for r in refusals]
        super().__init__("the database's schema differs from the code's:\n" + "\n".join(lines), status)


class Changes(NamedTuple):
    """Writes `GET /_changes` handed over, a dict each, and `next`: the last
    one's number, which is the `since` to read on from."""

    writes: list
    next: int


class Client:
    """fenec-server's HTTP endpoint, one statement a request.

    A statement goes over a connection kept alive, one a thread: through
    `urlopen`, which opens one a request, a read by id took 152 us against
    57 over the one connection, the server starting a thread for each.
    """

    def __init__(
        self,
        url: str = "http://127.0.0.1:8080",
        token: str | None = None,
        timeout: float = 30.0,
    ):
        parts = urllib.parse.urlsplit(url)
        if parts.scheme not in ("http", "https"):
            raise ValueError(f"not an http(s) URL: {url!r}")
        self.url = url.rstrip("/")
        self.token = token
        self.timeout = timeout
        # The change the last write left the database at (`Fenec-Seq`): a
        # read on a replica passed it as `after` waits for that write.
        self.seq: int | None = None
        self._host = parts.hostname or "127.0.0.1"
        self._port = parts.port or (443 if parts.scheme == "https" else 80)
        self._tls = parts.scheme == "https"
        self._base = parts.path.rstrip("/")
        self._local = threading.local()

    def collection(self, name: str) -> Query:
        """The query builder over collection `name`: chain `select`, `where`,
        `near`, `order`, `limit` ... and end with `rows()`, `first()`,
        `count()`, or a write -- `insert`, `update`, `delete`."""
        return Query(name, self)

    def query(
        self, fenecql: str, params: Sequence[Any] | None = None, *, after: int | None = None
    ) -> Any:
        """Runs one FenecQL statement. Values go in as `$1`, `$2`, ... and
        never into the text. With `after` -- a primary's `seq` -- a replica
        answers once it holds that write. A read answers its rows, a list;
        one with a `facet` clause a `Rows`, the counts as its `facets`."""
        body = {"query": fenecql, "params": list(params or [])}
        return _faceted(self._post("/query", json.dumps(body).encode(), "application/json", after))

    def batch(self, statements: Iterable[tuple[str, Sequence[Any]]]) -> Any:
        """Runs statements in order under one write lock, as one block:
        their writes -- a create, a drop or a `create index` among them --
        all land or, at the first error, none of them do. A batch holding a
        `compact` runs each statement on its own, and what ran before an
        error stays."""
        lines = [json.dumps({"query": q, "params": list(p)}) for q, p in statements]
        return self._post("/batch", "\n".join(lines).encode(), "application/x-ndjson")

    def schema(
        self, fenecql: str, migrations: Sequence[Any] | None = None, *, migrate: bool = False
    ) -> dict:
        """Checks the server's schema against `fenecql` -- `create
        collection` and `create index` statements, a `schema.fenecql` file
        -- and returns the engine's plan. The server owns its schema: it is
        compared, and nothing applied, unless `migrate` -- with the server's
        token -- runs the migrations it has not recorded, in order, and adds
        what only adds, all one block. Raises `SchemaError` naming every
        difference that would lose data or could mean two things."""
        body = {"format": 1, "fenecql": fenecql, "migrations": list(migrations or [])}
        path = "/_schema/apply" if migrate else "/_schema/plan?mode=follow"
        out = self._post(path, json.dumps(body).encode(), "application/json", accept=409)
        if out["refusals"]:
            raise SchemaError(out["refusals"], 409)
        return out

    def changes(
        self,
        since: int | None = None,
        *,
        consumer: str | None = None,
        limit: int | None = None,
        wait: float | None = None,
    ) -> Changes:
        """The writes on the server's disk after `since` -- or after where
        `consumer` stands, or from now with neither -- at most `limit`, each
        `{"seq", "at", "collection", "op", "id", "doc"}`. With `wait`, an
        answer with nothing in it waits that many seconds for a write."""
        query = {"since": since, "consumer": consumer, "limit": limit}
        if wait is not None:
            query["wait"] = int(wait * 1000)
        q = urllib.parse.urlencode({k: v for k, v in query.items() if v is not None})
        req = urllib.request.Request(f"{self.url}/_changes?{q}", method="GET")
        timeout = self.timeout + (wait or 0)
        with self._open(req, timeout) as resp:
            raw = resp.read()
            nxt = int(resp.headers.get("Fenec-Next", since or 0))
        return Changes([json.loads(line) for line in raw.splitlines() if line], nxt)

    def consumer(self, name: str, since: int | None = None) -> dict:
        """Makes `name` a consumer at the last write on disk, or at `since`,
        or moves it there: once it has done with every write up to it."""
        body = json.dumps({} if since is None else {"since": since}).encode()
        return self._post(f"/_changes/consumers/{_seg(name)}", body, "application/json")

    def consumers(self) -> list:
        """Each consumer: its `name`, `since`, and how many writes `behind`."""
        req = urllib.request.Request(f"{self.url}/_changes/consumers", method="GET")
        with self._open(req, self.timeout) as resp:
            return json.load(resp)

    def forget_consumer(self, name: str) -> None:
        req = urllib.request.Request(f"{self.url}/_changes/consumers/{_seg(name)}", method="DELETE")
        with self._open(req, self.timeout):
            pass

    def follow(self, consumer: str, *, limit: int = 1000, wait: float = 10.0) -> Iterator[list]:
        """Each batch of writes for `consumer`, made if it is new, committed
        once the loop asks for the next: a batch the loop did not finish is
        read again, so each write comes at least once."""
        if not any(c["name"] == consumer for c in self.consumers()):
            self.consumer(consumer)
        while True:
            got = self.changes(consumer=consumer, limit=limit, wait=wait)
            # Nothing read, nothing committed: a commit is a write, which
            # the stream passes over, and committing past it every time
            # would write once a wait.
            if got.writes:
                yield got.writes
                self.consumer(consumer, got.next)

    def _open(self, req: urllib.request.Request, timeout: float):
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        try:
            return urllib.request.urlopen(req, timeout=timeout)
        except urllib.error.HTTPError as e:
            _answer(e.code, e.read())
            raise

    def _post(
        self, path: str, body: bytes, content_type: str, after: int | None = None, accept: int = 0
    ) -> Any:
        headers = {"Content-Type": content_type}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if after is not None:
            headers["Fenec-After"] = str(after)
        status, seq, raw = self._send("POST", self._base + path, body, headers)
        if seq is not None and 200 <= status < 300:
            self.seq = int(seq)
        # An answer that is no error: a schema's refusals (409).
        if status == accept:
            return json.loads(raw)
        return _answer(status, raw)

    def _send(self, method: str, path: str, body: bytes, headers: dict) -> tuple:
        """A request over this thread's connection: its status, `Fenec-Seq`
        and body. A connection the server closed while it was idle is found
        closed by the request sent over it, which never reached a statement;
        that one request goes again, over a new connection."""
        conn = getattr(self._local, "conn", None)
        for fresh in (conn is None, True):
            if fresh:
                conn = self._connect()
            try:
                conn.request(method, path, body, headers)
                resp = conn.getresponse()
                raw = resp.read()
            except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError):
                conn.close()
                self._local.conn = None
                if fresh:
                    raise
                continue
            except BaseException:
                conn.close()
                self._local.conn = None
                raise
            if resp.will_close:
                conn.close()
                self._local.conn = None
            else:
                self._local.conn = conn
            return resp.status, resp.getheader("Fenec-Seq"), raw
        raise AssertionError("unreachable")

    def _connect(self) -> http.client.HTTPConnection:
        if self._tls:
            return http.client.HTTPSConnection(
                self._host, self._port, timeout=self.timeout, context=ssl.create_default_context()
            )
        return http.client.HTTPConnection(self._host, self._port, timeout=self.timeout)

    def close(self) -> None:
        """Closes this thread's connection; the next request opens another."""
        conn = getattr(self._local, "conn", None)
        if conn is not None:
            conn.close()
            self._local.conn = None


def _seg(name: str) -> str:
    return urllib.parse.quote(name, safe="")


def _faceted(answer: Any) -> Any:
    """A `/query` answer as it came, but for a query that asked facets:
    the server sends `{"rows": [...], "facets": {...}}` for it rather than
    the bare array, and that becomes `Rows` -- a list of the rows, as every
    other read answers, with the counts as its `facets`. A `/batch` answer
    is left as it came, each read's result `{"rows": ..., "facets": ...}`."""
    if isinstance(answer, dict) and "facets" in answer and isinstance(answer.get("rows"), list):
        return _rows(answer)
    return answer


def _answer(status: int, raw: bytes) -> Any:
    """A response's JSON, or the error it says, as `Client` raises it."""
    if 200 <= status < 300:
        return json.loads(raw) if raw else None
    try:
        message = json.loads(raw).get("error", raw.decode(errors="replace"))
    except (ValueError, AttributeError):
        message = raw.decode(errors="replace")
    raise FenecError(message, status)


class AsyncClient:
    """fenec-server's HTTP endpoint over asyncio: `Client`'s calls, awaited.

    One connection, kept alive and opened again when the server closed it;
    calls made at once on one client go one after another over it, as they
    would over one `Client` -- a client each for requests side by side.
    `https://` for a server behind a TLS terminator. The standard library
    alone: asyncio's streams and a request written out by hand, the
    endpoint's answers being a status, a length and JSON.
    """

    def __init__(
        self,
        url: str = "http://127.0.0.1:8080",
        token: str | None = None,
        timeout: float = 30.0,
    ):
        parts = urllib.parse.urlsplit(url)
        if parts.scheme not in ("http", "https"):
            raise ValueError(f"not an http(s) URL: {url!r}")
        self.url = url.rstrip("/")
        self.token = token
        self.timeout = timeout
        self._host = parts.hostname or "127.0.0.1"
        self._port = parts.port or (443 if parts.scheme == "https" else 80)
        self._tls = ssl.create_default_context() if parts.scheme == "https" else None
        self._base = parts.path.rstrip("/")
        self._conn: tuple[asyncio.StreamReader, asyncio.StreamWriter] | None = None
        self._lock = asyncio.Lock()

    def collection(self, name: str) -> AsyncQuery:
        """The query builder, as `Client.collection`, its endpoints awaited."""
        return AsyncQuery(name, self)

    async def query(self, fenecql: str, params: Sequence[Any] | None = None) -> Any:
        """One FenecQL statement, as `Client.query`."""
        body = {"query": fenecql, "params": list(params or [])}
        return _faceted(await self._post("/query", json.dumps(body).encode(), "application/json"))

    async def batch(self, statements: Iterable[tuple[str, Sequence[Any]]]) -> Any:
        """Statements under one write lock, as one block, as `Client.batch`."""
        lines = [json.dumps({"query": q, "params": list(p)}) for q, p in statements]
        return await self._post("/batch", "\n".join(lines).encode(), "application/x-ndjson")

    async def close(self) -> None:
        if self._conn:
            _, writer = self._conn
            self._conn = None
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()

    async def _post(self, path: str, body: bytes, content_type: str) -> Any:
        async with self._lock:
            # A kept connection the server closed meanwhile fails on its first
            # use: the request goes once more over a fresh one. A request
            # that failed over a fresh one says why.
            for fresh in (self._conn is None, True):
                try:
                    status, raw = await asyncio.wait_for(
                        self._exchange(path, body, content_type), self.timeout
                    )
                    return _answer(status, raw)
                except (ConnectionError, asyncio.IncompleteReadError, OSError):
                    await self.close()
                    if fresh:
                        raise
                except asyncio.TimeoutError:
                    await self.close()
                    raise
            raise AssertionError("unreachable")

    async def _exchange(self, path: str, body: bytes, content_type: str) -> tuple[int, bytes]:
        if self._conn is None:
            self._conn = await asyncio.open_connection(
                self._host, self._port, ssl=self._tls, server_hostname=self._host if self._tls else None
            )
        reader, writer = self._conn
        head = [
            f"POST {self._base}{path} HTTP/1.1",
            f"Host: {self._host}:{self._port}",
            f"Content-Type: {content_type}",
            f"Content-Length: {len(body)}",
            "Connection: keep-alive",
        ]
        if self.token:
            head.append(f"Authorization: Bearer {self.token}")
        writer.write(("\r\n".join(head) + "\r\n\r\n").encode() + body)
        await writer.drain()

        status_line = await reader.readuntil(b"\r\n")
        status = int(status_line.split(b" ", 2)[1])
        headers: dict[str, str] = {}
        while (line := await reader.readuntil(b"\r\n")) != b"\r\n":
            name, _, value = line.decode("latin-1").partition(":")
            headers[name.strip().lower()] = value.strip()
        if "content-length" in headers:
            raw = await reader.readexactly(int(headers["content-length"]))
        elif headers.get("transfer-encoding", "").lower() == "chunked":
            raw = b""
            while (size := int((await reader.readuntil(b"\r\n")).split(b";")[0], 16)) > 0:
                raw += await reader.readexactly(size)
                await reader.readexactly(2)
            await reader.readuntil(b"\r\n")
        else:
            raw = await reader.read()
            self._conn = None
        if headers.get("connection", "").lower() == "close" and self._conn:
            await self.close()
        return status, raw


def placeholders(start: int, n: int) -> str:
    """`$start, ..., $(start+n-1)`: a list literal's worth of parameters.
    FenecQL binds a parameter to a value, not to a list, so `in` takes one
    per element -- `in [$2, $3, $4]`."""
    return ", ".join(f"${i}" for i in range(start, start + n))


# Below FenecError, which the builder raises.
from .builder import (  # noqa: E402
    AsyncQuery,
    Computed,
    Cond,
    FacetCount,
    Query,
    Rows,
    _rows,
    and_,
    collection,
    expr,
    inc,
    not_,
    or_,
    raw,
)
