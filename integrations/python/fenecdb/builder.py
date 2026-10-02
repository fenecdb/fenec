"""The query builder: FenecQL text and its parameters from a chain of calls.

    rows = (db.collection("docs")
              .select("title")
              .where("year", ">=", 2024)
              .near("embed", vector, ef=64)
              .limit(5)
              .rows())

It makes the text web/fenec.js's builder makes of the same chain, to the
byte, and so do the Go and .NET builders: integrations/builder-golden.json
holds the chains and what each must make, and every builder's tests run
it. Every value goes in as a parameter; a name -- a collection, a field, a
path into a json field -- cannot, so names are checked against FenecQL's
own rule, and that check is the injection boundary.

A query is immutable: each call hands back a new one, so a base query can
be kept and branched from. `to_fenecql()` is the text and its parameters,
for logging or another transport; `rows()`, `first()`, `count()` and
`explain()` send it through the client the query came from, and on an
`AsyncClient` they are awaited.
"""

from __future__ import annotations

import array
import json
import unicodedata
from datetime import datetime, timezone
from typing import Any, Iterable, Mapping, Sequence, TypeVar

__all__ = ["AsyncQuery", "Cond", "Query", "and_", "collection", "not_", "or_", "raw"]

# Operator names, the symbols and the words for them.
_OPS = {
    "=": "=", "eq": "=",
    "!=": "!=", "ne": "!=", "neq": "!=",
    "<": "<", "lt": "<",
    "<=": "<=", "lte": "<=", "le": "<=",
    ">": ">", "gt": ">",
    ">=": ">=", "gte": ">=", "ge": ">=",
    "~": "~", "like": "~", "contains": "~",
    "has": "has",
    "in": "in",
}  # fmt: skip

# The collations the engine knows. The name is spliced into the text, so
# it is checked against the list rather than the name pattern.
_COLLATIONS = ("und", "tr")

# The engine refuses a deeper chain; failing here never sends a query.
MAX_LOOKUP_DEPTH = 8

# What JavaScript's String.prototype.trim takes off, which the JS builder
# trims an aggregate with: str.strip() also takes U+001C..U+001F and leaves
# U+FEFF.
_JS_SPACE = "\t\n\v\f\r                  　﻿"


def _err(message: str) -> Exception:
    # Imported late: the package's __init__ imports this module.
    from . import FenecError

    return FenecError(message, 0)


def _quote(name: Any) -> str:
    """A name in a message, as JSON.stringify writes it."""
    try:
        return json.dumps(name, ensure_ascii=False)
    except (TypeError, ValueError):
        return repr(name)


# FenecQL's identifier: a Unicode letter or `_`, then letters, digits and
# `_`, as the lexer reads it -- JavaScript's \p{Alphabetic} and \p{N}. The
# standard library has no Alphabetic property, which is the letters, the
# letter numbers and the marks of Other_Alphabetic (a Devanagari vowel
# sign); after the first character every mark is taken, a name the lexer
# would refuse being refused by the server rather than let through as text.
def _starts(ch: str) -> bool:
    cat = unicodedata.category(ch)
    return ch == "_" or cat[0] == "L" or cat == "Nl"


def _continues(ch: str) -> bool:
    cat = unicodedata.category(ch)
    return ch == "_" or cat[0] in "LN" or cat in ("Mn", "Mc")


def _is_ident(s: str) -> bool:
    return bool(s) and _starts(s[0]) and all(_continues(c) for c in s[1:])


def _ident(name: Any, what: str = "field") -> str:
    if not isinstance(name, str) or not _is_ident(name):
        raise _err(f"invalid {what} name: {_quote(name)}")
    return name


def _path(name: Any) -> str:
    """A field's name, or a path into a json field: `meta.lang`."""
    if not isinstance(name, str) or not all(_is_ident(p) for p in name.split(".")):
        raise _err(f"invalid field name: {_quote(name)}")
    return name


def _ascii_ident(s: str) -> bool:
    return (
        bool(s)
        and s.isascii()
        and (s[0].isalpha() or s[0] == "_")
        and all(c.isalnum() or c == "_" for c in s)
    )


def _column(name: Any) -> tuple[str, bool]:
    """A select item: a field, or an aggregate spelled as FenecQL spells it --
    `count(*)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)` -- answering
    under that name. Read by hand rather than by a case-blind pattern: one
    folds the Kelvin sign onto `k`, where the JS builder's does not."""
    if isinstance(name, str):
        s = name.strip(_JS_SPACE)
        open_, close = s.find("("), s.endswith(")")
        if open_ > 0 and close:
            fn, arg = s[:open_], s[open_ + 1 : -1]
            low = fn.lower() if fn.isascii() and fn.isalpha() else None
            if low == "count" and arg in ("", "*"):
                return "count(*)", True
            if low in ("sum", "avg", "min", "max") and _ascii_ident(arg):
                return f"{low}({arg})", True
    return _path(name), False


def _direction(d: Any) -> bool:
    low = str(d).lower()
    if low not in ("asc", "desc"):
        raise _err(f"order direction must be 'asc' or 'desc': {d}")
    return low == "asc"


def _collation(name: Any) -> str | None:
    if name is None:
        return None
    if name not in _COLLATIONS:
        raise _err(f"unknown collation: {_quote(name)}; there are 'und' and 'tr'")
    return name


def _whole(n: Any, what: str) -> int:
    """`limit`, `offset`, `ef` and the rest are literals in FenecQL, never
    parameters: a whole number JavaScript holds exactly."""
    if isinstance(n, bool) or not isinstance(n, int) or n < 0 or n > 2**53 - 1:
        raise _err(f"{what} must be a non-negative integer: {_quote(n) if isinstance(n, bool) else n}")
    return n


def _js_date(d: datetime) -> str:
    """A datetime as JavaScript's toISOString writes it, which the JS
    builder sends a Date as: UTC, to the millisecond. A naive one is taken
    for UTC."""
    if d.tzinfo is not None:
        d = d.astimezone(timezone.utc)
    return (
        f"{d.year:04d}-{d.month:02d}-{d.day:02d}T{d.hour:02d}:{d.minute:02d}:"
        f"{d.second:02d}.{d.microsecond // 1000:03d}Z"
    )


def _normalize(v: Any, what: str = "value") -> Any:
    """A value as it goes into the parameters' JSON: a datetime as its ISO
    text, an `array.array` or anything with `tolist()` (a NumPy array) as
    a list, a mapping as a json field's object."""
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, datetime):
        return _js_date(v)
    if isinstance(v, (list, tuple, array.array)):
        return [_normalize(x, what) for x in v]
    if isinstance(v, Mapping):
        return {k: _normalize(x, what) for k, x in v.items()}
    tolist = getattr(v, "tolist", None)
    if callable(tolist):
        return _normalize(tolist(), what)
    raise _err(f"this object cannot be used as a fenecdb value ({what})")


# ---------------------------------------------------------- the condition tree


class Cond:
    """A condition `or_`, `and_`, `not_` or `raw` made, or one the builder
    made of a field's. A dict is a condition too -- `{"year": {"gte": 2024}}`
    -- and stays one: only this class is taken for a node."""

    __slots__ = ("t", "items", "field", "op", "value", "negated", "sql", "params")

    def __init__(self, t: str, **kw: Any):
        self.t = t
        self.items: list[Cond] = kw.get("items", [])
        self.field: str = kw.get("field", "")
        self.op: str = kw.get("op", "")
        self.value: Any = kw.get("value")
        self.negated: bool = kw.get("negated", False)
        self.sql: str = kw.get("sql", "")
        self.params: tuple = kw.get("params", ())

    def __repr__(self) -> str:
        return f"Cond({self.t})"


def _flat(conds: Iterable[Any]) -> list:
    out = []
    for c in conds:
        out.extend(c) if isinstance(c, (list, tuple)) else out.append(c)
    return out


def or_(*conds: Any) -> Cond:
    """`or_(a, b)` / `or_([a, b])`: joins conditions with `or`."""
    return Cond("or", items=[_to_cond(c) for c in _flat(conds)])


def and_(*conds: Any) -> Cond:
    """`and_(a, b)`: `where` already ands; this is only needed inside `or_`."""
    return Cond("and", items=[_to_cond(c) for c in _flat(conds)])


def not_(cond: Any) -> Cond:
    return Cond("not", items=[_to_cond(cond)])


def raw(sql: str, *params: Any) -> Cond:
    """Everything the builder cannot express (a function call): each `?` is
    bound to the next parameter -- a literal `?` goes in as one too.

        .where(raw("cosine(embed, ?) > ?", vector, 0.5))
    """
    if not isinstance(sql, str):
        raise _err("raw() expects text")
    return Cond("raw", sql=sql, params=params)


def _to_cond(x: Any) -> Cond:
    if isinstance(x, Cond):
        return x
    if isinstance(x, Mapping):
        return _object_cond(x)
    raise _err(f"expected an object as a condition: {_quote(x)}")


def _object_cond(obj: Mapping) -> Cond:
    items = [_field_cond(_path(f), spec) for f, spec in obj.items()]
    return items[0] if len(items) == 1 else Cond("and", items=items)


def _field_cond(field: str, spec: Any) -> Cond:
    """A raw value is equality, a mapping an operator map."""
    if spec is None:
        return Cond("null", field=field)
    if not isinstance(spec, Mapping):
        return _cmp(field, "=", spec)
    items = []
    for k, v in spec.items():
        if k == "not":
            items.append(
                Cond("null", field=field, negated=True)
                if v is None
                else Cond("not", items=[_field_cond(field, v)])
            )
            continue
        op = _OPS.get(k) if isinstance(k, str) else None
        if not op:
            raise _err(f"unknown operator `{k}` (field: {field})")
        items.append(_in_cond(field, v) if op == "in" else _cmp(field, op, v))
    if not items:
        raise _err(f"empty condition object (field: {field})")
    return items[0] if len(items) == 1 else Cond("and", items=items)


def _in_cond(field: str, values: Any) -> Cond:
    if not isinstance(values, (list, tuple)):
        raise _err(f"`in` expects an array (field: {field})")
    if not values:
        raise _err(f"`in` does not accept an empty array (field: {field})")
    return Cond("in", field=field, items=list(values))


def _cmp(field: str, op: str, value: Any) -> Cond:
    # `= null` is never true in FenecQL; what is meant is `is null`.
    if value is None:
        if op == "=":
            return Cond("null", field=field)
        if op == "!=":
            return Cond("null", field=field, negated=True)
        raise _err(f"`{op}` cannot be used with null (field: {field})")
    return Cond("cmp", field=field, op=op, value=value)


def _cond_of(args: tuple) -> Cond:
    if len(args) == 1:
        return _to_cond(args[0])
    if len(args) == 2:
        return _field_cond(_path(args[0]), args[1])
    if len(args) == 3:
        op = _OPS.get(args[1]) if isinstance(args[1], str) else None
        if not op:
            raise _err(f"unknown operator `{args[1]}`")
        field = _path(args[0])
        return _in_cond(field, args[2]) if op == "in" else _cmp(field, op, args[2])
    raise _err("where(field, op, value) | where(field, value) | where(object)")


def _prune(c: Cond) -> Cond | None:
    """Flattens empty and single-child junctions before rendering: the
    parentheses depend on the child count, and rendering binds parameters."""
    if c.t in ("and", "or"):
        items = [x for x in (_prune(i) for i in c.items) if x is not None]
        if not items:
            return None
        return items[0] if len(items) == 1 else Cond(c.t, items=items)
    if c.t == "not":
        item = _prune(c.items[0])
        return Cond("not", items=[item]) if item is not None else None
    return c


class _Binder:
    def __init__(self) -> None:
        self.params: list = []

    def __call__(self, v: Any, what: str) -> str:
        self.params.append(_normalize(v, what))
        return f"${len(self.params)}"


def _render(c: Cond, bind: _Binder, parent: str | None = None) -> str:
    if c.t in ("and", "or"):
        s = f" {c.t} ".join(_render(x, bind, c.t) for x in c.items)
        # `and` binds tighter than `or`: one inside the other needs parens.
        return f"({s})" if parent and parent != c.t else s
    if c.t == "not":
        return f"not ({_render(c.items[0], bind)})"
    if c.t == "null":
        return f"{c.field} is {'not ' if c.negated else ''}null"
    if c.t == "in":
        return f"{c.field} in [{', '.join(bind(v, c.field) for v in c.items)}]"
    if c.t == "cmp":
        return f"{c.field} {c.op} {bind(c.value, c.field)}"
    # raw
    out, i = [], 0
    for part in c.sql.split("?")[:-1]:
        if i >= len(c.params):
            raise _err("raw(): more `?` placeholders than parameters")
        out.append(part + bind(c.params[i], "raw"))
        i += 1
    if i != len(c.params):
        raise _err("raw(): too many parameters given")
    out.append(c.sql.split("?")[-1])
    return "".join(out)


# ---------------------------------------------------------------- the query


def _order_keys(spec: Any) -> list[tuple[str, bool, str | None]]:
    """A lookup's `order`: `"created"`, or `[("created", "desc"), ...]`, a
    pair taking `{"collate": ...}` third; a bare name in the list is one
    ascending key."""
    if spec is None:
        return []
    if isinstance(spec, str):
        return [(_path(spec), True, None)]
    keys = []
    for k in spec:
        parts = [k] if isinstance(k, str) else list(k)
        field = parts[0] if parts else None
        d = parts[1] if len(parts) > 1 else "asc"
        opts = parts[2] if len(parts) > 2 else {}
        keys.append((_path(field), _direction(d), _collation((opts or {}).get("collate"))))
    return keys


def _sort_key(k: tuple[str, bool, str | None]) -> str:
    field, asc, collate = k
    return f"{field}{f' collate {collate}' if collate else ''} {'asc' if asc else 'desc'}"


Q = TypeVar("Q", bound="_Builder")


class _Builder:
    """The chain, shared by `Query` and `AsyncQuery`: everything but sending."""

    __slots__ = ("_s",)

    def __init__(self, collection: str, client: Any = None, _state: dict | None = None):
        if _state is not None:
            self._s = _state
            return
        self._s = {
            "collection": _ident(collection, "collection"),
            "client": client,
            "project": None,
            "aggregate": False,
            "group": None,
            "cond": [],
            "near": None,
            "match": None,
            "rerank": None,
            "fuse": None,
            "order": [],
            "limit": None,
            "offset": 0,
            "count": False,
            "lookups": [],
        }

    def _with(self: Q, **patch: Any) -> Q:
        return type(self)("", _state={**self._s, **patch})

    @property
    def collection(self) -> str:
        return self._s["collection"]

    def select(self: Q, *cols: Any) -> Q:
        """`select a, b`; none, or `"*"`, is every field. Aggregates go in the
        same list as FenecQL spells them and answer under that name:
        `select("status", "count(*)", "sum(total)").group("status")`."""
        flat = _flat(cols)
        if not flat or "*" in flat:
            return self._with(project=None, aggregate=False)
        cs = [_column(c) for c in flat]
        return self._with(project=[t for t, _ in cs], aggregate=any(a for _, a in cs))

    def group(self: Q, field: str) -> Q:
        """`group field`: a row per value, for a select list that aggregates."""
        return self._with(group=_ident(field))

    def where(self: Q, *args: Any) -> Q:
        """`where(field, op, value)`, `where(field, value)` -- a value is
        equality, a dict an operator map -- or `where(condition)`: a dict of
        fields, or what `or_`, `and_`, `not_` and `raw` make. Successive
        calls join with `and`."""
        return self._with(cond=[*self._s["cond"], _cond_of(args)])

    def or_where(self: Q, *args: Any) -> Q:
        """Joins everything conditioned so far to a new condition with `or`."""
        right = _cond_of(args)
        left = self._s["cond"]
        if not left:
            return self._with(cond=[right])
        return self._with(cond=[Cond("or", items=[Cond("and", items=list(left)), right])])

    def near(self: Q, field: str, vector: Any, *, ef: int | None = None, exact: bool = False) -> Q:
        """`near field $n [ef N] [exact]`."""
        return self._with(
            near=(_ident(field), vector, None if ef is None else _whole(ef, "ef"), bool(exact))
        )

    def match(self: Q, field: str, query: str) -> Q:
        """`match field $n`: BM25 over a `@text` index."""
        return self._with(match=(_ident(field), query))

    def fuse(self: Q, *, k: int | None = None, candidates: int | None = None) -> Q:
        """`fuse [k N] [candidates N]`: with both `match` and `near`, ranks by
        both -- a document scores `1 / (k + rank)` from each list it is on."""
        return self._with(
            fuse=(
                None if k is None else _whole(k, "k"),
                None if candidates is None else _whole(candidates, "candidates"),
            )
        )

    def rerank(self: Q, field: str, vector: Any, *, candidates: int | None = None) -> Q:
        """`rerank field $n [candidates N]`: reorders what `match` found by
        exact distance, the vectors read out of the store."""
        return self._with(
            rerank=(
                _ident(field),
                vector,
                None if candidates is None else _whole(candidates, "candidates"),
            )
        )

    def lookup(
        self: Q,
        name: str,
        *,
        on: str | None = None,
        parent_key: str | None = None,
        select: str | Sequence[str] | None = None,
        where: Any = None,
        required: bool = False,
        order: Any = None,
        limit: int | None = None,
        offset: int | None = None,
    ) -> Q:
        """`lookup name on child [= parent] ...`: each row's children,
        attached to it. Everything here binds to the looked-up collection;
        `limit` counts children per parent. `required` drops a parent no
        child matches. Called again, it chains onto the collection the call
        before named."""
        if not on:
            raise _err("lookup needs `on`: the child field holding the key")
        cols = None if select is None else ([select] if isinstance(select, str) else list(select))
        level = {
            "collection": _ident(name, "collection"),
            "on": _ident(on),
            "parent": None if parent_key is None else _ident(parent_key),
            "project": None if cols is None or "*" in cols else [_path(c) for c in cols],
            "cond": [] if where is None else [_cond_of((where,))],
            "required": bool(required),
            "order": _order_keys(order),
            "limit": None if limit is None else _whole(limit, "limit"),
            "offset": 0 if offset is None else _whole(offset, "offset"),
        }
        return self._with(lookups=[*self._s["lookups"], level])

    def order(self: Q, field: str, direction: str = "asc", *, collate: str | None = None) -> Q:
        """`order field asc|desc`; each call adds a key. `collate="tr"` puts
        text in Turkish order, `"und"` in Unicode's root order."""
        return self._with(
            order=[*self._s["order"], (_column(field)[0], _direction(direction), _collation(collate))]
        )

    def limit(self: Q, n: int) -> Q:
        return self._with(limit=_whole(n, "limit"))

    def offset(self: Q, n: int) -> Q:
        return self._with(offset=_whole(n, "offset"))

    # ---------------------------------------------------------- the text

    def to_fenecql(self) -> tuple[str, list]:
        """The statement and its parameters, as they would be sent."""
        s = self._s
        near, match, rerank, fuse = s["near"], s["match"], s["rerank"], s["fuse"]
        lookups, count, order = s["lookups"], s["count"], s["order"]
        # The engine refuses each of these too; failing here sends nothing.
        if s["group"] and not s["aggregate"]:
            raise _err(f"group {s['group']} needs an aggregate in select: 'count(*)'")
        if s["aggregate"]:
            clash = (
                "near" if near else "match" if match else "lookup" if lookups else "count" if count else None
            )
            if clash:
                raise _err(f"aggregates cannot be combined with {clash}")
            if not s["group"] and (order or s["limit"] is not None or s["offset"]):
                raise _err("aggregates answer one row; group makes a row per value")
        if rerank and not match:
            raise _err("rerank needs match: it reorders what match found")
        if match and near and not fuse:
            raise _err("match and near cannot be combined: both order the result; fuse() ranks by both")
        if fuse and not (match and near):
            raise _err("fuse combines match and near: the query needs both")
        if fuse and rerank:
            raise _err("fuse and rerank are two ways to use a vector with match: pick one")
        if lookups:
            clash = "near" if near else "match" if match else "rerank" if rerank else None
            if clash:
                raise _err(f"lookup cannot be combined with {clash}")
            if count and not lookups[0]["required"]:
                raise _err(
                    "count cannot be used with lookup unless it is required: there is "
                    "nothing to attach children to"
                )
            if len(lookups) > MAX_LOOKUP_DEPTH:
                raise _err(f"lookup chained too deep: at most {MAX_LOOKUP_DEPTH} levels")
            seen = [s["collection"]]
            for level in lookups:
                if level["collection"] in seen:
                    raise _err(
                        f"{level['collection']} cannot look itself up: both sides would "
                        "answer to the same name"
                    )
                seen.append(level["collection"])
        if count:
            extra = self._extra_clause()
            if extra:
                raise _err(f"count cannot be used with `{extra}`")
        bind = _Binder()
        sql = f"get {s['collection']}"
        if s["project"]:
            sql += f" select {', '.join(s['project'])}"
        where = self._where(bind)
        if where:
            sql += f" where {where}"
        if s["group"]:
            sql += f" group {s['group']}"
        if near:
            sql += f" near {near[0]} {bind(near[1], near[0])}"
            if near[2] is not None:
                sql += f" ef {near[2]}"
            if near[3]:
                sql += " exact"
        if match:
            sql += f" match {match[0]} {bind(match[1], match[0])}"
        if rerank:
            sql += f" rerank {rerank[0]} {bind(rerank[1], rerank[0])}"
            if rerank[2] is not None:
                sql += f" candidates {rerank[2]}"
        if fuse:
            sql += " fuse"
            if fuse[0] is not None:
                sql += f" k {fuse[0]}"
            if fuse[1] is not None:
                sql += f" candidates {fuse[1]}"
        for i, k in enumerate(order):
            sql += f"{' order ' if i == 0 else ', '}{_sort_key(k)}"
        if s["limit"] is not None:
            sql += f" limit {s['limit']}"
        if s["offset"]:
            sql += f" offset {s['offset']}"
        if count:
            sql += " count"
        # Terminal, so every clause after it is the child's -- and last, so
        # its parameters come after the parent's.
        for level in lookups:
            sql += f" lookup {level['collection']} on {level['on']}"
            if level["parent"]:
                sql += f" = {level['parent']}"
            if level["required"]:
                sql += " required"
            if level["project"]:
                sql += f" select {', '.join(level['project'])}"
            root = _prune(Cond("and", items=level["cond"]))
            if root is not None:
                sql += f" where {_render(root, bind)}"
            for i, k in enumerate(level["order"]):
                sql += f"{' order ' if i == 0 else ', '}{_sort_key(k)}"
            if level["limit"] is not None:
                sql += f" limit {level['limit']}"
            if level["offset"]:
                sql += f" offset {level['offset']}"
        return sql, bind.params

    def to_insert(self, docs: Mapping | Sequence[Mapping]) -> tuple[str, list]:
        """The `put` of a document or a list of them, not sent."""
        self._assert_plain("insert")
        items = list(docs) if isinstance(docs, (list, tuple)) else [docs]
        if not items:
            raise _err("cannot write an empty document list")
        bind = _Binder()
        body = ", ".join(_render_doc(d, bind) for d in items)
        return f"put {self.collection} {body if len(items) == 1 else f'[{body}]'}", bind.params

    def to_update(self, patch: Mapping, *, all: bool = False) -> tuple[str, list]:
        """The `set` of the rows the filter names, not sent."""
        self._assert_plain("update")
        bind = _Binder()
        body = _render_doc(patch, bind)
        return f"set {self.collection} {body}{self._require_filter('update', all, bind)}", bind.params

    def to_delete(self, *, all: bool = False) -> tuple[str, list]:
        """The `del` of the rows the filter names, not sent."""
        self._assert_plain("delete")
        bind = _Binder()
        return f"del {self.collection}{self._require_filter('delete', all, bind)}", bind.params

    # `near`, `order`, `limit` mean something only to a read; dropped from a
    # write, `.limit(1).delete()` would delete every row.
    def _assert_plain(self, verb: str) -> None:
        extra = self._extra_clause()
        if extra:
            raise _err(f"{verb} cannot be used with `{extra}`")
        if self._s["lookups"]:
            raise _err(f"{verb} cannot be used with `lookup`")
        if verb == "insert" and self._s["cond"]:
            raise _err("insert cannot be used with `where`")

    def _extra_clause(self) -> str | None:
        s = self._s
        return (
            "near" if s["near"]
            else "match" if s["match"]
            else "rerank" if s["rerank"]
            else "order" if s["order"]
            else "limit" if s["limit"] is not None
            else "offset" if s["offset"]
            else "select" if s["project"]
            else None
        )  # fmt: skip

    # An update or delete of every row is too easy to do by accident and
    # cannot be undone: it has to be asked for.
    def _require_filter(self, verb: str, everything: bool, bind: _Binder) -> str:
        where = self._where(bind)
        if where:
            return f" where {where}"
        if everything is True:
            return ""
        raise _err(
            f"an unfiltered {verb} covers the whole collection; if you mean it, "
            f"{verb}({{ all: true }})"
        )

    def _where(self, bind: _Binder) -> str | None:
        root = _prune(Cond("and", items=self._s["cond"]))
        return None if root is None else _render(root, bind)

    def _client(self) -> Any:
        client = self._s["client"]
        if client is None:
            raise _err(
                "query is not bound to a connection: use db.collection(...) "
                "(to_fenecql() if you only want the text)"
            )
        return client

    def _counted(self: Q) -> Q:
        return self._with(count=True)


def _render_doc(doc: Any, bind: _Binder) -> str:
    if not isinstance(doc, Mapping):
        raise _err("expected a document object")
    pairs = [f"{_path(k)}: {bind(v, k)}" for k, v in doc.items()]
    if not pairs:
        raise _err("cannot write an empty document")
    return "{" + ", ".join(pairs) + "}"


def _rows(answer: Any) -> list:
    return answer if isinstance(answer, list) else []


def _affected(answer: Any) -> int:
    return answer.get("affected", 0) if isinstance(answer, dict) else 0


def _count(answer: Any) -> int:
    rows = _rows(answer)
    return rows[0].get("count", 0) if rows and isinstance(rows[0], dict) else 0


class Query(_Builder):
    """A query over a `Client`: immutable, its endpoints sending it."""

    __slots__ = ()

    def rows(self) -> list[dict]:
        """Every row the query answers, a dict each."""
        return _rows(self._client().query(*self.to_fenecql()))

    def first(self) -> dict | None:
        """The first row, or None: the query with `limit 1`."""
        rows = self.limit(1).rows()
        return rows[0] if rows else None

    def count(self) -> int:
        """How many rows match: `get ... count`, no row decoded."""
        q = self._counted()
        return _count(q._client().query(*q.to_fenecql()))

    def explain(self) -> list[str]:
        """The path the query took, a line a step; the query runs to tell."""
        sql, params = self.to_fenecql()
        return [r.get("plan") for r in _rows(self._client().query(f"explain {sql}", params))]

    def insert(self, docs: Mapping | Sequence[Mapping]) -> int:
        """`put` of a document or a list of them: how many were written."""
        if isinstance(docs, (list, tuple)) and not docs:
            return 0
        return _affected(self._client().query(*self.to_insert(docs)))

    def update(self, patch: Mapping, *, all: bool = False) -> int:
        """`set` over the rows the filter names: how many it changed. With
        no filter it is refused unless `all=True`."""
        return _affected(self._client().query(*self.to_update(patch, all=all)))

    def delete(self, *, all: bool = False) -> int:
        """`del` of the rows the filter names: how many it deleted. With no
        filter it is refused unless `all=True`."""
        return _affected(self._client().query(*self.to_delete(all=all)))


class AsyncQuery(_Builder):
    """A query over an `AsyncClient`: `Query`'s endpoints, awaited."""

    __slots__ = ()

    async def rows(self) -> list[dict]:
        return _rows(await self._client().query(*self.to_fenecql()))

    async def first(self) -> dict | None:
        rows = await self.limit(1).rows()
        return rows[0] if rows else None

    async def count(self) -> int:
        q = self._counted()
        return _count(await q._client().query(*q.to_fenecql()))

    async def explain(self) -> list[str]:
        sql, params = self.to_fenecql()
        return [r.get("plan") for r in _rows(await self._client().query(f"explain {sql}", params))]

    async def insert(self, docs: Mapping | Sequence[Mapping]) -> int:
        if isinstance(docs, (list, tuple)) and not docs:
            return 0
        return _affected(await self._client().query(*self.to_insert(docs)))

    async def update(self, patch: Mapping, *, all: bool = False) -> int:
        return _affected(await self._client().query(*self.to_update(patch, all=all)))

    async def delete(self, *, all: bool = False) -> int:
        return _affected(await self._client().query(*self.to_delete(all=all)))


def collection(name: str) -> Query:
    """A query bound to no client: for its text alone, `to_fenecql()`."""
    return Query(name)

