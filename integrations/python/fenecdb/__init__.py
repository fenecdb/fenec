"""fenecdb over HTTP.

A client for fenec-pg's HTTP endpoint (`fenec-pg --http <address>`), and on
top of it vector stores for LangChain (`fenecdb.langchain`) and LlamaIndex
(`fenecdb.llama_index`).

    from fenecdb import Client
    db = Client("http://127.0.0.1:8080", token="...")
    db.query("get articles near embed $1 limit 5", [[0.1, 0.2, 0.3]])

The client is the standard library alone, as the server is: a statement is
one `POST /query`, several are one `POST /batch` under one lock.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from typing import Any, Iterable, Sequence

__all__ = ["Client", "FenecError", "placeholders"]


class FenecError(Exception):
    """A statement the server refused: its message, and the HTTP status."""

    def __init__(self, message: str, status: int):
        super().__init__(message)
        self.status = status


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


def placeholders(start: int, n: int) -> str:
    """`$start, ..., $(start+n-1)`: a list literal's worth of parameters.
    FenecQL binds a parameter to a value, not to a list, so `in` takes one
    per element -- `in [$2, $3, $4]`."""
    return ", ".join(f"${i}" for i in range(start, start + n))
