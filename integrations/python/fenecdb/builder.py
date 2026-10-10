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
import math
import re
import unicodedata
from datetime import datetime, timezone
from typing import Any, Iterable, Mapping, NamedTuple, Sequence, TypeVar

__all__ = [
    "AsyncQuery",
    "Computed",
    "Cond",
    "FacetCount",
    "Query",
    "Rows",
    "and_",
    "bucket",
    "collection",
    "count_distinct",
    "expr",
    "first",
    "inc",
    "last",
    "not_",
    "or_",
    "raw",
]

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
_JS_SPACE = "\t\n\v\f\r " + "".join(map(chr, (0xA0, 0x1680, *range(0x2000, 0x200B), 0x2028, 0x2029, 0x202F, 0x205F, 0x3000, 0xFEFF)))


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


# A select item spelled as FenecQL spells an aggregate of one -- `count(*)`,
# `count(distinct user)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)`,
# `first(f)`, `last(f)`, a path where a field goes. ASCII-only and its case
# folded as ASCII alone, as the JS builder's pattern is: a Unicode-aware one
# folds the Kelvin sign onto `k` and `ſ` onto `s`, where JavaScript's does
# not. `\s` there is JavaScript's space, which `_JS_SPACE` spells out.
_NAMES = r"[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*"
_SP = "[" + re.escape(_JS_SPACE) + "]"
_AGGREGATE = re.compile(
    rf"(count)\(\*?\)|count\({_SP}*distinct{_SP}+({_NAMES}){_SP}*\)|(sum|avg|min|max|first|last)\(({_NAMES})\)",
    re.ASCII | re.IGNORECASE,
)
# What makes an expression an aggregate's: a call of one.
_AGGREGATE_CALL = re.compile(
    rf"(?<![A-Za-z0-9_])(count|sum|avg|min|max|first|last){_SP}*\(", re.ASCII | re.IGNORECASE
)


def _column(name: Any) -> tuple[Any, bool]:
    """A select item: a field, an aggregate spelled as FenecQL spells it,
    answering under that name, or an expression (`Computed`), answering
    under the name its `as_` gives it."""
    if isinstance(name, Computed) and name.kind == "expr":
        return name, bool(_AGGREGATE_CALL.search(name.sql))
    m = _AGGREGATE.fullmatch(name.strip(_JS_SPACE)) if isinstance(name, str) else None
    if not m:
        return _path(name), False
    if m[1]:
        return "count(*)", True
    if m[2]:
        return f"count(distinct {m[2]})", True
    return f"{m[3].lower()}({m[4]})", True


def _key(name: Any) -> str:
    """An `order` key: a field, a path, or an aggregate by its name."""
    return _column(name)[0] if isinstance(name, str) else _path(name)


def _column_text(c: Any, bind: "_Binder") -> str:
    """A select item or a group key as text, an expression's values bound."""
    if isinstance(c, str):
        return c
    text = _value("select", c, bind, "")
    return f"{text} as {c.name}" if c.name is not None else text


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


def _js_number(n: float) -> str:
    """A number as JavaScript's `String(n)` writes it, the text every other
    builder makes: a whole float without its `.0`."""
    if isinstance(n, float) and n.is_integer() and abs(n) < 1e21:
        return str(int(n))
    return repr(n)


def _whole(n: Any, what: str) -> int:
    """`limit`, `offset`, `ef` and the rest are literals in FenecQL, never
    parameters: a whole number JavaScript holds exactly."""
    if isinstance(n, bool) or not isinstance(n, int) or n < 0 or n > 2**53 - 1:
        raise _err(f"{what} must be a non-negative integer: {_quote(n) if isinstance(n, bool) else n}")
    return n


def _text(v: Any, what: str) -> str:
    if not isinstance(v, str):
        raise _err(f"{what} must be text: {_quote(v)}")
    return v


def _tags(pre: Any, post: Any, what: str) -> dict:
    """A mark's tags: both or neither, each text."""
    if pre is None and post is None:
        return {}
    if pre is None or post is None:
        raise _err(f"{what} takes both pre and post, or neither")
    return {"pre": _text(pre, f"{what} pre"), "post": _text(post, f"{what} post")}


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


class Computed:
    """A value worked out over a row: what `inc` and `expr` make for a
    write, and what `expr`, `bucket`, `count_distinct`, `first` and `last`
    make for a select list or a group, rendered as FenecQL with its values
    as parameters."""

    __slots__ = ("kind", "by", "sql", "params", "name")

    def __init__(
        self, kind: str, by: Any = None, sql: str = "", params: tuple = (), name: str | None = None
    ):
        self.kind, self.by, self.sql, self.params, self.name = kind, by, sql, params, name

    def as_(self, name: str) -> "Computed":
        """The name the column answers under: `select ... as <name>`
        (`as` is Python's keyword, hence the underscore)."""
        return Computed(self.kind, self.by, self.sql, self.params, _ident(name, "column"))

    def __repr__(self) -> str:
        return f"Computed({self.kind})"


def inc(by: Any = 1) -> Computed:
    """`{"n": inc(1)}` in an update: the field plus `by`, counting from 0
    where it is null -- `n: coalesce(n, 0) + $1` -- worked out under the
    server's write lock, so increments from many clients all land."""
    if isinstance(by, bool) or not isinstance(by, (int, float)) or not math.isfinite(by):
        text = json.dumps(by, ensure_ascii=False, separators=(",", ":"), default=str)
        raise _err(f"inc() takes a number: {text}")
    return Computed("inc", by=by)


def expr(sql: str, *params: Any) -> Computed:
    """A value as a FenecQL expression over the row it is written into,
    each `?` bound to the next parameter: `{"at": expr("now()")}`,
    `{"total": expr("price * ?", 1.2)}`."""
    if not isinstance(sql, str):
        raise _err("expr() expects text")
    return Computed("expr", sql=sql, params=params)


# `15m`, `1h`, `1d`, `1w`, `3mo`, `1y`: what `bucket` takes, written into
# the text as a literal, since it is a part of the statement's shape.
_INTERVAL = re.compile(r"[1-9][0-9]*(ms|s|m|h|d|w|mo|y)", re.ASCII)


def bucket(field: str, interval: str) -> Computed:
    """`bucket(field, interval)`: the start of the interval a timestamp
    falls in -- `"15m"`, `"1h"`, `"1d"`, `"1w"` (from a Monday), `"1mo"`,
    `"1y"`, in UTC -- for a select list or a group:

        .select(bucket("at", "1m").as_("minute"), "count(*)").group("minute")
    """
    if not isinstance(interval, str) or not _INTERVAL.fullmatch(interval):
        raise _err(
            "bucket() takes an interval such as '15m', '1h', '1d', '1w' or '1mo': "
            + json.dumps(interval, ensure_ascii=False, default=str)
        )
    return Computed("expr", sql=f"bucket({_path(field)}, {interval})")


def count_distinct(field: str) -> Computed:
    """`count(distinct field)`: how many distinct values the rows hold."""
    return Computed("expr", sql=f"count(distinct {_path(field)})")


def first(field: str, by: str | None = None) -> Computed:
    """`first(field)`, or `first(field by key)`: the value of the row least
    by `key` -- by the order the rows were written without one -- that has
    a value; a bar's open is `first("px", "at")`."""
    return _pick("first", field, by)


def last(field: str, by: str | None = None) -> Computed:
    """`last(field [by key])`: as `first`, the row greatest by `key`."""
    return _pick("last", field, by)


def _pick(fn: str, field: str, by: str | None) -> Computed:
    return Computed("expr", sql=f"{fn}({_path(field)}{'' if by is None else f' by {_path(by)}'})")


def _value(k: str, v: Any, bind: "_Binder", write: str) -> str:
    """A document's value: `inc`'s and `expr`'s text, or a parameter."""
    if not isinstance(v, Computed):
        return bind(v, k)
    if v.kind == "inc":
        if write == "insert":
            raise _err(f"inc() reads the row it changes: use it in update (field: {k})")
        return f"coalesce({k}, 0) + {bind(v.by, k)}"
    out, i = [], 0
    parts = v.sql.split("?")
    for part in parts[:-1]:
        if i >= len(v.params):
            raise _err("expr(): more `?` placeholders than parameters")
        out.append(part + bind(v.params[i], k))
        i += 1
    if i != len(v.params):
        raise _err("expr(): too many parameters given")
    out.append(parts[-1])
    return "".join(out)


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
            "marks": [],
            "facets": [],
            "require": "",
        }

    def _with(self: Q, **patch: Any) -> Q:
        return type(self)("", _state={**self._s, **patch})

    @property
    def collection(self) -> str:
        return self._s["collection"]

    def select(self: Q, *cols: Any) -> Q:
        """`select a, b`; none, or `"*"`, is every field. Aggregates go in the
        same list as FenecQL spells them and answer under that name:
        `select("status", "count(*)", "sum(total)").group("status")`; an
        expression -- `expr()`, `bucket()`, `count_distinct()`, `first()`,
        `last()` -- under the name its `as_` gives it:
        `select("sym", expr("sum(px * qty) / sum(qty)").as_("vwap")).group("sym")`."""
        flat = _flat(cols)
        if not flat or any(isinstance(c, str) and c == "*" for c in flat):
            return self._with(project=None, aggregate=False)
        cs = [_column(c) for c in flat]
        return self._with(project=[t for t, _ in cs], aggregate=any(a for _, a in cs))

    def highlight(self: Q, field: str, *, pre: str | None = None, post: str | None = None) -> Q:
        """`highlight(field)` in the select list: where the terms `match`
        found stand in the field's text -- `[start, end]` pairs of UTF-16
        offsets, a JavaScript string's own, as the server counts them -- or,
        given `pre` and `post`, the text with each mark between them. The
        text is not escaped: a page that renders it as HTML escapes it
        first, or builds it from the offsets. Answers under
        `highlight(field)`, after the fields `select` named."""
        return self._mark({"field": _ident(field), "words": None, **_tags(pre, post, "highlight")})

    def snippet(
        self: Q,
        field: str,
        words: int,
        *,
        ellipsis: str | None = None,
        pre: str | None = None,
        post: str | None = None,
    ) -> Q:
        """`snippet(field, words)`: the window of `words` words around the
        densest marks, `{"text", "marks"}` -- or the marked text, given
        `pre` and `post` -- with `ellipsis` where it leaves text out.
        Answers under `snippet(field)`."""
        mark = {"field": _ident(field), "words": _whole(words, "snippet words"), **_tags(pre, post, "snippet")}
        if mark["words"] == 0:
            raise _err("snippet shows at least one word")
        if ellipsis is not None:
            mark["ellipsis"] = _text(ellipsis, "snippet ellipsis")
        return self._mark(mark)

    def _mark(self: Q, mark: dict) -> Q:
        # Each answers under its label, and a row holds a name once.
        def kind(m: dict) -> str:
            return "highlight" if m["words"] is None else "snippet"

        if any(kind(m) == kind(mark) and m["field"] == mark["field"] for m in self._s["marks"]):
            raise _err(f"{kind(mark)}({mark['field']}) is asked twice")
        return self._with(marks=[*self._s["marks"], mark])

    def facet(
        self: Q,
        field: str,
        *,
        top: int | None = None,
        ranges: Sequence[float] | None = None,
        disjunctive: bool = False,
    ) -> Q:
        """`facet field [top N]`: each value the field -- or a path into a
        json field -- holds over every row the query matches, not only the
        page, and how many rows hold it, most first; `top` keeps the
        commonest. A list counts once a row for each value. The counts come
        back beside the rows, as `rows().facets`.

            (db.collection("products").match("title", "phone").where("price", "<", 500)
               .facet("brand", top=10).facet("color").limit(20).rows().facets)

        `ranges=[0, 25, 50]` counts the rows in each range of numbers from
        one bound up to the next, every range in order, its value
        `[from, to]`; `disjunctive=True` counts as if the filter's own
        conditions on the field were not there, so the other values a
        shopper could add are counted too.
        """
        name = _path(field)
        top = None if top is None else _whole(top, "facet top")
        if top == 0:
            raise _err(f"facet {name} top 0 answers nothing")
        if any(g[0] == name for g in self._s["facets"]):
            raise _err(f"facet {name} is asked twice")
        bounds = None
        if ranges is not None:
            # The engine's rule, refused before anything is sent.
            ok = (
                top is None
                and isinstance(ranges, (list, tuple))
                and len(ranges) >= 2
                and all(
                    isinstance(b, (int, float))
                    and not isinstance(b, bool)
                    and math.isfinite(b)
                    and (i == 0 or b > ranges[i - 1])
                    for i, b in enumerate(ranges)
                )
            )
            if not ok:
                raise _err(
                    f"facet {name} ranges takes 2 to 10 001 numbers, each above the one before, "
                    "and no top: every range answers, in order"
                )
            bounds = [_js_number(b) for b in ranges]
        if not isinstance(disjunctive, bool):
            raise _err(f"facet {name} disjunctive is true or false")
        return self._with(facets=[*self._s["facets"], (name, top, bounds, disjunctive)])

    def group(self: Q, *keys: Any) -> Q:
        """`group a, b`: a row per distinct set of the keys' values, for a
        select list that aggregates. A key is a field or a path, a name the
        list gives a column with `as_`, or an expression: `bucket("at", "1h")`."""
        flat = _flat(keys)
        if not flat:
            raise _err("group takes at least one key")
        return self._with(
            group=[k if isinstance(k, Computed) and k.kind == "expr" else _path(k) for k in flat]
        )

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
            # Over groups a key may be an aggregate of the list, by its name.
            order=[*self._s["order"], (_key(field), _direction(direction), _collation(collate))]
        )

    def limit(self: Q, n: int) -> Q:
        return self._with(limit=_whole(n, "limit"))

    def offset(self: Q, n: int) -> Q:
        return self._with(offset=_whole(n, "offset"))

    def require(self: Q, n: int) -> Q:
        """`require n`: the rows the query answers, after `limit`, must
        number `n`, or it is refused (412, `unmet`) and the batch it is in
        put back, as a write's `require=n` is -- a checkout's guard on a
        read: `.where(sku=sku, price=price).require(1)`."""
        return self._with(require=_require_clause(n))

    # ---------------------------------------------------------- the text

    def to_fenecql(self) -> tuple[str, list]:
        """The statement and its parameters, as they would be sent."""
        s = self._s
        near, match, rerank, fuse = s["near"], s["match"], s["rerank"], s["fuse"]
        lookups, count, order = s["lookups"], s["count"], s["order"]
        marks, facets = s["marks"], s["facets"]
        # The engine refuses each of these too; failing here sends nothing.
        if s["group"] and not s["aggregate"]:
            keys = ", ".join(k if isinstance(k, str) else k.sql for k in s["group"])
            raise _err(f"group {keys} needs an aggregate in select: 'count(*)'")
        if s["aggregate"]:
            clash = (
                "near" if near else "match" if match else "lookup" if lookups else "count" if count else None
            )
            if clash:
                raise _err(f"aggregates cannot be combined with {clash}")
            if not s["group"] and (order or s["limit"] is not None or s["offset"]):
                raise _err("aggregates answer one row; group makes a row per value")
            if not s["group"] and s["require"]:
                raise _err("require counts the rows a query answers, and an aggregate answers one")
        if marks:
            what = "highlight" if marks[0]["words"] is None else "snippet"
            if not match:
                raise _err(f"{what} needs match: it marks the terms match found")
            if s["aggregate"]:
                raise _err(f"{what} marks a row's text; aggregates answer groups")
        if facets:
            if near:
                raise _err(
                    "facet counts the rows a filter or match selects, and near ranks every "
                    "row: ask the facets without near"
                )
            if s["aggregate"]:
                raise _err("facet cannot be combined with aggregates: group counts by value")
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
            extra = self._extra_clause() or ("require" if s["require"] else None)
            if extra:
                raise _err(f"count cannot be used with `{extra}`")
        bind = _Binder()
        sql = f"get {s['collection']}"
        # The marks after the fields `select` named, or after every field.
        items = []
        for m in marks:
            item = f"{'highlight' if m['words'] is None else 'snippet'}({m['field']}"
            if m["words"] is not None:
                item += f", {m['words']}"
            # A snippet's tags come after its ellipsis, so tags without one
            # bind the empty one.
            if "ellipsis" in m or (m["words"] is not None and "pre" in m):
                item += f", {bind(m.get('ellipsis', ''), 'snippet')}"
            if "pre" in m:
                item += f", {bind(m['pre'], m['field'])}, {bind(m['post'], m['field'])}"
            items.append(item + ")")
        # The list's values are bound first: they come first in the text.
        cols = None if s["project"] is None else [_column_text(c, bind) for c in s["project"]]
        if cols or items:
            sql += f" select {', '.join([*(cols or ['*']), *items])}"
        where = self._where(bind)
        if where:
            sql += f" where {where}"
        if s["group"]:
            sql += f" group {', '.join(_column_text(k, bind) for k in s['group'])}"
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
        # Before a lookup, whose clauses are the children's.
        sql += s["require"]
        if count:
            sql += " count"
        if facets:
            sql += " facet " + ", ".join(
                f
                + ("" if top is None else f" top {top}")
                + ("" if bounds is None else f" ranges [{', '.join(bounds)}]")
                + (" disjunctive" if disjunctive else "")
                for f, top, bounds, disjunctive in facets
            )
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

    def to_insert(
        self,
        docs: Mapping | Sequence[Mapping],
        *,
        if_absent: bool = False,
        require: int | None = None,
    ) -> tuple[str, list]:
        """The `put` of a document or a list of them, not sent. With
        `if_absent=True`, `put ... if absent`: a document whose id or
        `@unique` value is held is passed over, and not counted. With
        `require=n` on any write, `... require n`: unless it wrote exactly
        `n` rows it is refused (412) and its batch put back."""
        self._assert_plain("insert")
        items = list(docs) if isinstance(docs, (list, tuple)) else [docs]
        if not items:
            raise _err("cannot write an empty document list")
        bind = _Binder()
        body = ", ".join(_render_doc(d, bind, "insert") for d in items)
        absent = " if absent" if if_absent is True else ""
        required = _require_clause(require)
        many = body if len(items) == 1 else f"[{body}]"
        return f"put {self.collection} {many}{absent}{required}", bind.params

    def to_update(
        self,
        patch: Mapping,
        *,
        all: bool = False,
        require: int | None = None,
        returning: Any = None,
    ) -> tuple[str, list]:
        """The `set` of the rows the filter names, not sent. `order` and
        `limit` before it pick the rows it writes -- the page a `get` with
        them answers -- and `returning=True`, or a list of fields, answers
        them as written: a job queue's claim,
        `q.where(raw("run_at <= now()")).order("run_at").limit(10)
        .update({...}, returning=True)`."""
        self._assert_plain("update")
        bind = _Binder()
        body = _render_doc(patch, bind, "update")
        where = self._require_filter("update", all, bind)
        tail = f"{self._pick(returning)}{_require_clause(require)}"
        return f"set {self.collection} {body}{where}{tail}", bind.params

    def to_upsert(
        self,
        docs: Mapping | Sequence[Mapping],
        patch: Mapping,
        *,
        require: int | None = None,
    ) -> tuple[str, list]:
        """The upsert, `put ... if absent else set {patch}`, not sent: a
        document whose id or first `@unique` value a row holds sets that row
        by the patch -- an update's, `inc(n)` and `expr(...)` with it, and
        in an expression `new.f` the document's own `f` -- and every other
        one is inserted. The documents' parameters come first, then the
        patch's."""
        self._assert_plain("upsert")
        items = list(docs) if isinstance(docs, (list, tuple)) else [docs]
        if not items:
            raise _err("cannot write an empty document list")
        bind = _Binder()
        body = ", ".join(_render_doc(d, bind, "insert") for d in items)
        patched = _render_doc(patch, bind, "update")
        required = _require_clause(require)
        many = body if len(items) == 1 else f"[{body}]"
        return f"put {self.collection} {many} if absent else set {patched}{required}", bind.params

    def to_delete(
        self, *, all: bool = False, require: int | None = None, returning: Any = None
    ) -> tuple[str, list]:
        """The `del` of the rows the filter names, not sent; `order`,
        `limit` and `returning` as `to_update`'s, the rows answered as they
        were: a pop."""
        self._assert_plain("delete")
        bind = _Binder()
        where = self._require_filter("delete", all, bind)
        tail = f"{self._pick(returning)}{_require_clause(require)}"
        return f"del {self.collection}{where}{tail}", bind.params

    # `near`, `offset`, `select` mean something only to a read; dropped from
    # a write, `.offset(1).delete()` would delete from the first row. `order`
    # and `limit` pick the rows an update or a delete writes.
    def _assert_plain(self, verb: str) -> None:
        if self._s["require"]:
            raise _err(f"{verb} takes require as its option: {verb}(..., {{ require: n }})")
        extra = self._extra_clause(verb in ("update", "delete"))
        if extra:
            raise _err(f"{verb} cannot be used with `{extra}`")
        if self._s["lookups"]:
            raise _err(f"{verb} cannot be used with `lookup`")
        if self._s["facets"]:
            raise _err(f"{verb} cannot be used with `facet`")
        if verb in ("insert", "upsert") and self._s["cond"]:
            raise _err(f"{verb} cannot be used with `where`")

    def _extra_clause(self, picks: bool = False) -> str | None:
        s = self._s
        return (
            "near" if s["near"]
            else "match" if s["match"]
            else "rerank" if s["rerank"]
            else "order" if s["order"] and not picks
            else "limit" if s["limit"] is not None and not picks
            else "offset" if s["offset"]
            else "select" if s["project"]
            else None
        )  # fmt: skip

    # An update's or a delete's ` order ... limit n returning ...`.
    def _pick(self, returning: Any) -> str:
        sql = ""
        for i, k in enumerate(self._s["order"]):
            sql += f"{' order ' if i == 0 else ', '}{_sort_key(k)}"
        if self._s["limit"] is not None:
            sql += f" limit {self._s['limit']}"
        return sql + _returning_clause(returning)

    # An update or delete of every row is too easy to do by accident and
    # cannot be undone: it has to be asked for. A `limit` bounds it.
    def _require_filter(self, verb: str, everything: bool, bind: _Binder) -> str:
        where = self._where(bind)
        if where:
            return f" where {where}"
        if everything is True or self._s["limit"] is not None:
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


def _render_doc(doc: Any, bind: _Binder, write: str) -> str:
    if not isinstance(doc, Mapping):
        raise _err("expected a document object")
    pairs = [f"{_path(k)}: {_value(k, v, bind, write)}" for k, v in doc.items()]
    if not pairs:
        raise _err("cannot write an empty document")
    return "{" + ", ".join(pairs) + "}"


class FacetCount(NamedTuple):
    """A value a facet counted -- any JSON value, `None` for the rows whose
    field is null -- and how many rows hold it."""

    value: Any
    count: int


class Rows(list):
    """A query's rows, a dict each -- a list, and equal to one -- with what
    `facet` counted beside them as `facets`: each field asked, in the order
    asked, to its values most first. Empty when the query asked none."""

    facets: dict[str, list[FacetCount]]

    def __init__(self, rows: Iterable = (), facets: Mapping | None = None):
        super().__init__(rows)
        self.facets = {
            k: [FacetCount(c.get("value"), c.get("count", 0)) for c in v]
            for k, v in (facets or {}).items()
        }


def _rows(answer: Any) -> Rows:
    """The rows of a `/query` answer: the bare array, or -- for a query that
    asked facets -- `{"rows": [...], "facets": {...}}`, the counts beside
    the rows since they answer for every row matched, not one."""
    if isinstance(answer, Rows):
        return answer
    if isinstance(answer, list):
        return Rows(answer)
    if isinstance(answer, Mapping) and isinstance(answer.get("rows"), list):
        return Rows(answer["rows"], answer.get("facets"))
    return Rows()


def _affected(answer: Any) -> int:
    return answer.get("affected", 0) if isinstance(answer, dict) else 0


def _written(answer: Any, returning: Any) -> Any:
    """A write's answer: its count, or with `returning` its rows."""
    if returning is None or returning is False:
        return _affected(answer)
    return _rows(answer)


def _returning_clause(returning: Any) -> str:
    """` returning ...` for a write's `returning`: `True` or `["*"]` every
    field, or the fields named, each a field or a path."""
    if returning is None or returning is False:
        return ""
    if returning is True:
        return " returning *"
    if isinstance(returning, str) or not isinstance(returning, (list, tuple)) or not returning:
        raise _err("returning names the fields it answers, or * for every one")
    if "*" in returning:
        if len(returning) > 1:
            raise _err("returning * answers every field: it takes no other")
        return " returning *"
    return " returning " + ", ".join(_path(f) for f in returning)


def _count(answer: Any) -> int:
    rows = _rows(answer)
    return rows[0].get("count", 0) if rows and isinstance(rows[0], dict) else 0


class Query(_Builder):
    """A query over a `Client`: immutable, its endpoints sending it."""

    __slots__ = ()

    def rows(self) -> Rows:
        """Every row the query answers, a dict each -- and, for a query that
        asked facets, the counts as the list's `facets`."""
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

    def insert(
        self,
        docs: Mapping | Sequence[Mapping],
        *,
        if_absent: bool = False,
        require: int | None = None,
    ) -> int:
        """`put` of a document or a list of them: how many were written --
        with `if_absent=True` those whose id or `@unique` value was held
        left out, so a lock taken answers 1 and one held 0. With
        `require=n` (on `update` and `delete` too) a write that did not
        write exactly `n` rows is refused, 412, and its batch put back."""
        if isinstance(docs, (list, tuple)) and not docs:
            return 0
        text = self.to_insert(docs, if_absent=if_absent, require=require)
        return _affected(self._client().query(*text))

    def upsert(
        self,
        docs: Mapping | Sequence[Mapping],
        patch: Mapping,
        *,
        require: int | None = None,
    ) -> int:
        """`put ... if absent else set`: each document inserted, or where a
        row holds its id or `@unique` value, that row set by the patch --
        `upsert({"key": k, "n": 1}, {"n": expr("n + new.n")})`. How many
        rows it set and made together."""
        if isinstance(docs, (list, tuple)) and not docs:
            return 0
        return _affected(self._client().query(*self.to_upsert(docs, patch, require=require)))

    def update(
        self,
        patch: Mapping,
        *,
        all: bool = False,
        require: int | None = None,
        returning: Any = None,
    ) -> Any:
        """`set` over the rows the filter names: how many it changed, or
        with `returning` the rows as written. With no filter it is refused
        unless `all=True` or a `limit` bounds it."""
        text = self.to_update(patch, all=all, require=require, returning=returning)
        return _written(self._client().query(*text), returning)

    def delete(
        self, *, all: bool = False, require: int | None = None, returning: Any = None
    ) -> Any:
        """`del` of the rows the filter names: how many it deleted, or with
        `returning` the rows as they were. With no filter it is refused
        unless `all=True` or a `limit` bounds it."""
        text = self.to_delete(all=all, require=require, returning=returning)
        return _written(self._client().query(*text), returning)


class AsyncQuery(_Builder):
    """A query over an `AsyncClient`: `Query`'s endpoints, awaited."""

    __slots__ = ()

    async def rows(self) -> Rows:
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

    async def insert(
        self,
        docs: Mapping | Sequence[Mapping],
        *,
        if_absent: bool = False,
        require: int | None = None,
    ) -> int:
        if isinstance(docs, (list, tuple)) and not docs:
            return 0
        text = self.to_insert(docs, if_absent=if_absent, require=require)
        return _affected(await self._client().query(*text))

    async def upsert(
        self,
        docs: Mapping | Sequence[Mapping],
        patch: Mapping,
        *,
        require: int | None = None,
    ) -> int:
        if isinstance(docs, (list, tuple)) and not docs:
            return 0
        return _affected(await self._client().query(*self.to_upsert(docs, patch, require=require)))

    async def update(
        self,
        patch: Mapping,
        *,
        all: bool = False,
        require: int | None = None,
        returning: Any = None,
    ) -> Any:
        text = self.to_update(patch, all=all, require=require, returning=returning)
        return _written(await self._client().query(*text), returning)

    async def delete(
        self, *, all: bool = False, require: int | None = None, returning: Any = None
    ) -> Any:
        text = self.to_delete(all=all, require=require, returning=returning)
        return _written(await self._client().query(*text), returning)


# ` require n` for a write's `require=n`: the rows it must write, a whole
# number from 0 -- not a parameter, as `limit` is not, so a statement keeps
# its shape. A bool is an int to Python and is refused, as JavaScript's
# builder refuses `true`.
def _require_clause(n: Any) -> str:
    if n is None:
        return ""
    if isinstance(n, bool) or not isinstance(n, int) or n < 0:
        raise _err(f"require takes a count of rows, a whole number from 0 (got {n})")
    return f" require {n}"


def collection(name: str) -> Query:
    """A query bound to no client: for its text alone, `to_fenecql()`."""
    return Query(name)

