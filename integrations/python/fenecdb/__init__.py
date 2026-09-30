"""fenecdb over HTTP.

A client for fenec-pg's HTTP endpoint (`fenec-pg --http <address>`), and on
top of it vector stores for LangChain (`fenecdb.langchain`) and LlamaIndex
(`fenecdb.llama_index`).

    from fenecdb import Client
    db = Client("http://127.0.0.1:8080", token="...")
    db.query("get articles near embed $1 limit 5", [[0.1, 0.2, 0.3]])

The client is the standard library alone, as the server is: a statement is
one `POST /query`, several are one `POST /batch` under one lock.
`AsyncClient` is the same over asyncio, for an event loop a blocking
request would stall:

    async with AsyncClient("http://127.0.0.1:8080", token="...") as db:
        rows = await db.query("get articles near embed $1 limit 5", [[0.1, 0.2, 0.3]])

`Client.follow` reads every write on the server's disk (`GET /_changes`,
`fenec-pg --cdc`) for a consumer whose cursor the server keeps, and
commits each batch once the loop comes back for the next:

    for batch in db.follow("search-index"):
        for change in batch:
            index(change)          # an exception: this batch is read again

`python -m fenecdb.relay` hands the same to another program or a webhook.
"""

from __future__ import annotations

import asyncio
import json
import ssl
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Iterable, Iterator, NamedTuple, Sequence

__all__ = ["AsyncClient", "Changes", "Client", "FenecError", "placeholders"]


class FenecError(Exception):
    """A statement the server refused: its message, and the HTTP status."""

    def __init__(self, message: str, status: int):
        super().__init__(message)
        self.status = status


class Changes(NamedTuple):
    """Writes `GET /_changes` handed over, a dict each, and `next`: the last
    one's number, which is the `since` to read on from."""

    writes: list
    next: int


class Client:
    """fenec-pg's HTTP endpoint, one statement a request."""

    def __init__(
        self,
        url: str = "http://127.0.0.1:8080",
        token: str | None = None,
        timeout: float = 30.0,
    ):
        self.url = url.rstrip("/")
        self.token = token
        self.timeout = timeout

    def query(self, fenecql: str, params: Sequence[Any] | None = None) -> Any:
        """Runs one FenecQL statement. Values go in as `$1`, `$2`, ... and
        never into the text."""
        body = {"query": fenecql, "params": list(params or [])}
        return self._post("/query", json.dumps(body).encode(), "application/json")

    def batch(self, statements: Iterable[tuple[str, Sequence[Any]]]) -> Any:
        """Runs statements in order under one write lock, as one block:
        their writes -- a create, a drop or a `create index` among them --
        all land or, at the first error, none of them do. A batch holding a
        `compact` runs each statement on its own, and what ran before an
        error stays."""
        lines = [json.dumps({"query": q, "params": list(p)}) for q, p in statements]
        return self._post("/batch", "\n".join(lines).encode(), "application/x-ndjson")

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

    def _post(self, path: str, body: bytes, content_type: str) -> Any:
        req = urllib.request.Request(self.url + path, data=body, method="POST")
        req.add_header("Content-Type", content_type)
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                message = json.loads(raw).get("error", raw.decode(errors="replace"))
            except (ValueError, AttributeError):
                message = raw.decode(errors="replace")
            raise FenecError(message, e.code) from None


def _seg(name: str) -> str:
    return urllib.parse.quote(name, safe="")


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
    """fenec-pg's HTTP endpoint over asyncio: `Client`'s calls, awaited.

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

    async def query(self, fenecql: str, params: Sequence[Any] | None = None) -> Any:
        """One FenecQL statement, as `Client.query`."""
        body = {"query": fenecql, "params": list(params or [])}
        return await self._post("/query", json.dumps(body).encode(), "application/json")

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
