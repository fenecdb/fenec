#!/usr/bin/env python3
"""A FenecQL example on the reference pages, in tabs: the statement, then the
same query written with each SDK's builder.

    {{builder "docs: the nearest articles" "docs: the nearest articles exactly"}}
    <pre data-lang="fenecql">get articles near embed $1 limit 10
    get articles near embed $1 exact limit 10</pre>

The placeholder names cases of integrations/builder-golden.json, one for each
statement of the block under it, in order. The build (`expand`, from
build.py's `filled`) holds each case to its statement -- the text the
builders make, each parameter written in as FenecQL writes the value, or a
`$n` of the page's where the case binds one -- and writes the box: the
block as it was under FenecQL, the box's first tab, then a tab for each
language with the case's chain written in that language's builder, and the
statement it sends. FenecQL's tab keeps nothing (`data-keep="false"`), so
the reader's language, where one was chosen, is what a box opens on.

A placeholder over no FenecQL block makes its FenecQL tab of the cases --
each statement with its parameters written in, broken before its clauses
where it is long (`written`) -- for a page whose prose has no statement of
its own to hold them to, as languages.html's tour of the builder:

    {{builder "docs: a page of documents with their comments" "docs: a document inserted"}}

Every tab's code is code that works, each language proven its own way:

  JavaScript   run here (`--check`): each snippet as the page shows it, in
               Node, against `connect` with a `fetch` that records.
  Python       run here (`--check`): each snippet as the page shows it,
               against a `Client` whose `query` records.
  Go, .NET,    a test file of each SDK's, written from the same renderer
  Swift,       (`--write`, `make docs-builder`): each snippet as the page
  Kotlin,      shows it, against the SDK's recording transport, the text
  Dart         and parameters it sent held to the case's. The SDK's own
               test job runs it; `--check` fails when a file is not what
               the renderer writes (whitespace aside, as `dart format` and
               `gofmt` lay it out).

Standard library only, as build.py is.

    python3 site/builder_tabs.py --write   # the SDKs' test files
    python3 site/builder_tabs.py --check   # all of the above
"""

import html
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(ROOT)
GOLDEN = os.path.join(REPO, "integrations", "builder-golden.json")
CONTENT = os.path.join(ROOT, "content")

# A chain longer than this is written a call to a line, and a call's
# arguments one to a line where the call alone is.
WIDTH = 72

_cases = None


def golden():
    """The golden file's cases by name."""
    global _cases
    if _cases is None:
        with open(GOLDEN, encoding="utf-8") as f:
            _cases = {c["name"]: c for c in json.load(f)}
    return _cases


# Cases of the check's own, never on a page and not the golden file's: a
# text holding every character a literal escapes, in a filter and in a
# document, written by every renderer and run as each language's tab is --
# each must send the text it was given. The JS builder makes these
# statements of these chains, as the golden file's cases show for any text.
SPECIALS = "".join(map(chr, (0, 1, 9, 10, 13, 31, 0x85, 0x2028, 0x2029))) + " \"'\\$`${x}$1\\(x) …"
LINES = "one two\r\nthree\u0085four "
ESCAPES = {
    "escapes: every character a literal escapes, in a filter": {
        "steps": [{"op": "from", "args": ["docs"]}, {"op": "where", "args": ["title", SPECIALS]}, {"op": "rows"}],
        "text": "get docs where title = $1", "params": [SPECIALS]},
    "escapes: line ends, in a document": {
        "steps": [{"op": "from", "args": ["docs"]}, {"op": "where", "args": ["id", 1]},
                  {"op": "update", "args": [{"title": LINES, "note": SPECIALS}]}],
        "text": "set docs {title: $1, note: $2} where id = $3", "params": [LINES, SPECIALS, 1]},
}


def case(name):
    c = golden().get(name) or ESCAPES.get(name)
    if c is None:
        raise SystemExit(f"builder tabs: no case {name!r} in integrations/builder-golden.json")
    if "error" in c:
        raise SystemExit(f"builder tabs: case {name!r} is a refusal, not a statement")
    if c["steps"][-1]["op"] not in ENDPOINTS:
        raise SystemExit(f"builder tabs: case {name!r} ends with {c['steps'][-1]['op']}, "
                         "and a tab shows a query an app runs: end it at an endpoint (rows, count, ...)")
    return c


ENDPOINTS = ("rows", "first", "count", "explain", "insert", "update", "upsert", "delete")

# ------------------------------------------------------- the page's statements


def _uncommented(line):
    """A line without its `--` comment, a string's `--` kept, an escaped
    quote no end of its string."""
    quote, escaped = None, False
    for i, ch in enumerate(line):
        if escaped:
            escaped = False
        elif quote:
            if ch == "\\":
                escaped = True
            elif ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif line.startswith("--", i):
            return line[:i]
    return line


def statements(code):
    """A block's statements: a line at the margin starts one, an indented
    line goes on with it, comments and blank lines are no part of one."""
    out = []
    for line in code.split("\n"):
        body = _uncommented(line).strip()
        if not body:
            continue
        if line[:1].isspace() and out:
            out[-1] += " " + body
        else:
            out.append(body)
    return [re.sub(r"\s+", " ", s) for s in out]


def literal(v):
    """A value as FenecQL writes it in a statement."""
    if v is None:
        return "null"
    if v is True:
        return "true"
    if v is False:
        return "false"
    if isinstance(v, (int, float)):
        return _num(v)
    if isinstance(v, str):
        return _text(v)
    if isinstance(v, list):
        return "[" + ", ".join(literal(x) for x in v) + "]"
    return "{" + ", ".join(f"{_key(k)}: {literal(x)}" for k, x in v.items()) + "}"


def _text(s):
    """A text as FenecQL writes it: between double quotes, a backslash and a
    quote escaped, and FENECQL_ESCAPES's four as their escapes."""
    return '"' + "".join("\\" + ch if ch in '\\"' else FENECQL_ESCAPES.get(ch, ch) for ch in s) + '"'


def _key(k):
    """An object's key as FenecQL writes it: a name bare, anything else --
    `a-b`, `1x`, a word FenecQL reads as a value -- quoted."""
    bare = k.isidentifier() and k.lower() not in ("true", "false", "null")
    return k if bare else _text(k)


def _num(v):
    if isinstance(v, float) and v.is_integer() and abs(v) < 1e21:
        return str(int(v))
    return repr(v) if isinstance(v, float) else str(v)


def made(c, stmt):
    """Whether the case's statement is the page's: its text, each `$k` there
    the parameter written in or a `$m` of the page's, one for one."""
    parts = re.split(r"(\$\d+)", c["text"])
    at, theirs, ours = 0, {}, {}
    for p in parts:
        if not p.startswith("$"):
            if not stmt.startswith(p, at):
                return False
            at += len(p)
            continue
        k = int(p[1:])
        m = re.match(r"\$\d+", stmt[at:])
        if m:
            if theirs.setdefault(k, m.group()) != m.group() or ours.setdefault(m.group(), k) != k:
                return False
            at += m.end()
            continue
        lit = literal(c["params"][k - 1])
        if not stmt.startswith(lit, at):
            return False
        at += len(lit)
    return at == len(stmt)


# ------------------------------------------------------------------ rendering


class Call:
    """`.name(args)`: a step of a chain, its arguments rendered."""

    def __init__(self, name, args=(), open_="(", close=")"):
        self.name, self.args, self.open, self.close = name, list(args), open_, close

    def flat(self):
        return f"{self.name}{self.open}{', '.join(self.args)}{self.close}"


# What every renderer writes as an escape, never as itself: the C0
# controls (`\r` among them), U+0085, U+2028 and U+2029, which some
# lexers take for a line's end -- C# refuses U+2028 in a string literal.
ESCAPED = frozenset(range(0x20)) | {0x85, 0x2028, 0x2029}

# FenecQL's own escapes in a string (fenec-ql's lexer): after a backslash a
# character is itself, but for these four. It has no `\u`, so the rest of
# ESCAPED has no spelling a page could show, and `written` refuses a value
# holding one.
FENECQL_ESCAPES = {"\n": "\\n", "\t": "\\t", "\r": "\\r", "\0": "\\0"}
UNWRITABLE = ESCAPED - {ord(ch) for ch in FENECQL_ESCAPES}


def quoted(s, q, also="", u="\\u%04x"):
    """`s` as a string literal between `q`s: a backslash, the quote and
    `also` (`$`, a template's start in Kotlin and Dart) escaped by a
    backslash, and each of ESCAPED as `u` spells a code point -- `\\u0085`,
    or Swift's `\\u{85}`."""
    out = [q]
    for ch in s:
        if ch == "\\" or ch == q or ch in also:
            out.append("\\" + ch)
        elif ord(ch) in ESCAPED:
            out.append(u % ord(ch))
        else:
            out.append(ch)
    return "".join(out) + q


def _ident_js(k):
    return re.fullmatch(r"[A-Za-z_$][\w$]*", k) is not None


def _special(x):
    """The `$` key of an object standing for what JSON has no word for."""
    if isinstance(x, dict) and len(x) == 1:
        k = next(iter(x))
        if k.startswith("$"):
            return k, x[k]
    return None, None


def _opt(args, at):
    return args[at] if len(args) > at and isinstance(args[at], dict) else {}


class Lang:
    """A language's builder: how a chain is written in it. Each step's
    calls and arguments follow that SDK's API as its golden test hands the
    file's steps to it."""

    name = code = ""
    indent = "    "
    # Where the code's names come from, its block's label: the import, with
    # the helpers called where a language imports each by name.
    IMPORT, HELPERS = "", ()

    def needs(self, code):
        used = [h for h in self.HELPERS if re.search(rf"(?<![\w.]){h}\(", code)]
        return self.IMPORT.format(", ".join(used) and ", " + ", ".join(used))

    # -- the chain, laid out

    def render(self, c):
        steps = c["steps"]
        calls = [self.start(steps[0]["args"][0])]
        for s in steps[1:]:
            a = s.get("args", [])
            if s["op"] in ("select", "group"):
                # A list of columns is its columns: every builder flattens it.
                a = [x for e in a for x in (e if isinstance(e, list) else [e])]
            calls.append(self.step(s["op"], a))
        return self.layout(calls)

    def layout(self, calls):
        """One line if it fits; else a call a line, and a call too long for
        its line its arguments one a line (no trailing comma: Swift's and
        C#'s argument lists take none)."""
        head, rest = calls[0], calls[1:]
        one = self.wrap(head.flat() + "".join("." + c.flat() for c in rest))
        if len(one) <= WIDTH and "\n" not in one:
            return one
        lines = [head.flat()]
        for c in rest:
            text = self.indent + "." + c.flat()
            if len(text) > WIDTH + 8 and (len(c.args) > 1 or c.open.endswith("[")):
                body = ",\n".join(self.indent * 2 + a for a in c.args)
                text = f"{self.indent}.{c.name}{c.open}\n{body}\n{self.indent}{c.close}"
            lines.append(text)
        return self.wrap_lines(lines)

    WRITES = {"insert": 1, "update": 1, "upsert": 2, "delete": 0}

    def write(self, op, a, labels, names):
        """A write: its documents and options, and, where it answers its
        rows, `<op>Returning` -- Swift's, Kotlin's and Dart's -- the fields
        named unless every one, the call's default."""
        at = self.WRITES[op]
        o = dict(_opt(a, at))
        r = o.pop("returning", False)
        name, ret = op, []
        if r not in (False, None):
            name = op + "Returning"
            if r is not True and r != ["*"]:
                ret = [names(r)]
        return Call(name, [self.v(x) for x in a[:at]] + ret + labels(op, o))

    def wrap(self, chain):
        return chain

    def wrap_lines(self, lines):
        return self.wrap("\n".join(lines))

    # -- what each language spells its own way

    def start(self, collection):
        raise NotImplementedError

    def step(self, op, a):
        raise NotImplementedError


class JS(Lang):
    name, code = "JavaScript", "js"
    IMPORT = "import {{ connect{} }} from '@fenecdb/web/client'"
    HELPERS = ("or", "and", "not", "raw", "inc", "expr", "bucket", "countDistinct", "distance", "first", "last")
    indent = "  "

    def s(self, v):
        return quoted(v, "'")

    def v(self, x):
        k, w = _special(x)
        if k == "$inc":
            return f"inc({_num(w)})"
        if k == "$expr":
            return "expr(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: JavaScript has no value {k}")
        if x is None:
            return "null"
        if isinstance(x, bool):
            return "true" if x else "false"
        if isinstance(x, (int, float)):
            return _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            return "[" + ", ".join(self.v(e) for e in x) + "]"
        return self.obj({k: self.v(e) for k, e in x.items()})

    def obj(self, fields):
        if not fields:
            return "{}"
        return "{ " + ", ".join(f"{k if _ident_js(k) else self.s(k)}: {v}" for k, v in fields.items()) + " }"

    def col(self, x):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.as({self.s(w[1])})"
        if k == "$expr":
            return self.v(x)
        if k == "$bucket":
            return "bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f"countDistinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f"{k[1:]}(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$distance":
            return f"distance({self.s(w[0])}, {self.v(w[1])})"
        raise SystemExit(f"builder tabs: JavaScript has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f"{k[1:]}(" + ", ".join(self.cond(e) for e in w) + ")"
        if k == "$not":
            return f"not({self.cond(w)})"
        if k == "$raw":
            return "raw(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        return self.v(x)

    def opts(self, o):
        out = {}
        for k, x in o.items():
            if k == "order" and isinstance(x, list):
                out[k] = "[" + ", ".join(self.v(e) for e in x) + "]"
            elif k == "where":
                out[k] = self.cond(x)
            elif k in ("select", "group"):
                out[k] = "[" + ", ".join(self.col(e) for e in x) + "]" if isinstance(x, list) else self.col(x)
            else:
                out[k] = self.v(x)
        return self.obj(out)

    def start(self, collection):
        return Call("db.from", [self.s(collection)])

    def step(self, op, a):
        if op in ("select", "group"):
            return Call(op, [self.col(x) for x in a])
        if op in ("where", "orWhere"):
            if len(a) == 1:
                return Call(op, [self.cond(a[0])])
            return Call(op, [self.s(a[0])] + ([self.s(a[1])] if len(a) == 3 else []) + [self.v(a[-1])])
        if op == "order":
            args = [self.s(x) for x in a[:2]]
            return Call(op, args + ([self.opts(a[2])] if len(a) > 2 else []))
        if op in ("limit", "offset", "require"):
            return Call(op, [_num(a[0])] + ([self.opts(a[1])] if len(a) > 1 else []))
        if op in ("near", "rerank"):
            return Call(op, [self.s(a[0]), self.v(a[1])] + ([self.opts(a[2])] if len(a) > 2 else []))
        if op == "match":
            return Call(op, [self.s(a[0]), self.s(a[1])] + ([self.opts(a[2])] if len(a) > 2 else []))
        if op in ("highlight", "facet", "lookup"):
            return Call(op, [self.s(a[0])] + ([self.opts(a[1])] if len(a) > 1 else []))
        if op == "snippet":
            return Call(op, [self.s(a[0]), _num(a[1])] + ([self.opts(a[2])] if len(a) > 2 else []))
        if op == "fuse":
            return Call(op, [self.opts(a[0])] if a else [])
        if op in ("insert", "update"):
            return Call(op, [self.v(a[0])] + ([self.opts(a[1])] if len(a) > 1 else []))
        if op == "upsert":
            return Call(op, [self.v(a[0]), self.v(a[1])] + ([self.opts(a[2])] if len(a) > 2 else []))
        if op == "delete":
            return Call(op, [self.opts(a[0])] if a else [])
        if op in ENDPOINTS:
            return Call(op)
        raise SystemExit(f"builder tabs: JavaScript has no step {op}")

    def wrap(self, chain):
        return f"await {chain};"


class Python(Lang):
    name, code = "Python", "python"
    IMPORT = "from fenecdb import Client{}"
    HELPERS = ("or_", "and_", "not_", "raw", "inc", "expr", "bucket", "count_distinct", "distance", "first", "last")
    SNAKE = {"parentKey": "parent_key", "ifAbsent": "if_absent"}

    def s(self, v):
        return quoted(v, '"')

    def v(self, x):
        k, w = _special(x)
        if k == "$inc":
            return f"inc({_num(w)})"
        if k == "$expr":
            return "expr(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: Python has no value {k}")
        if x is None:
            return "None"
        if isinstance(x, bool):
            return "True" if x else "False"
        if isinstance(x, (int, float)):
            return _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            return "[" + ", ".join(self.v(e) for e in x) + "]"
        return "{" + ", ".join(f"{self.s(k)}: {self.v(e)}" for k, e in x.items()) + "}"

    def col(self, x):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.as_({self.s(w[1])})"
        if k == "$expr":
            return self.v(x)
        if k == "$bucket":
            return "bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f"count_distinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f"{k[1:]}(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$distance":
            return f"distance({self.s(w[0])}, {self.v(w[1])})"
        raise SystemExit(f"builder tabs: Python has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f"{k[1:]}_(" + ", ".join(self.cond(e) for e in w) + ")"
        if k == "$not":
            return f"not_({self.cond(w)})"
        if k == "$raw":
            return "raw(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        return self.v(x)

    def kw(self, o):
        out = []
        for k, x in o.items():
            if k == "order" and isinstance(x, list):
                x = "[" + ", ".join(self.s(e) if isinstance(e, str) else "(" + ", ".join(self.v(p) for p in e) + ")"
                                    for e in x) + "]"
            elif k == "where":
                x = self.cond(x)
            elif k in ("select", "group"):
                x = "[" + ", ".join(self.col(e) for e in x) + "]" if isinstance(x, list) else self.col(x)
            else:
                x = self.v(x)
            out.append(f"{self.SNAKE.get(k, k)}={x}")
        return out

    def start(self, collection):
        return Call("db.collection", [self.s(collection)])

    def step(self, op, a):
        if op in ("select", "group"):
            return Call(op, [self.col(x) for x in a])
        if op in ("where", "orWhere"):
            name = "or_where" if op == "orWhere" else op
            if len(a) == 1:
                return Call(name, [self.cond(a[0])])
            return Call(name, [self.s(a[0])] + ([self.s(a[1])] if len(a) == 3 else []) + [self.v(a[-1])])
        if op == "order":
            return Call(op, [self.s(x) for x in a[:2]] + self.kw(_opt(a, 2)))
        if op in ("limit", "offset", "require"):
            return Call(op, [_num(a[0])] + self.kw(_opt(a, 1)))
        if op in ("near", "rerank"):
            return Call(op, [self.s(a[0]), self.v(a[1])] + self.kw(_opt(a, 2)))
        if op == "match":
            return Call(op, [self.s(a[0]), self.s(a[1])] + self.kw(_opt(a, 2)))
        if op in ("highlight", "facet", "lookup"):
            return Call(op, [self.s(a[0])] + self.kw(_opt(a, 1)))
        if op == "snippet":
            return Call(op, [self.s(a[0]), _num(a[1])] + self.kw(_opt(a, 2)))
        if op == "fuse":
            return Call(op, self.kw(_opt(a, 0)))
        if op in ("insert", "update"):
            return Call(op, [self.v(a[0])] + self.kw(_opt(a, 1)))
        if op == "upsert":
            return Call(op, [self.v(a[0]), self.v(a[1])] + self.kw(_opt(a, 2)))
        if op == "delete":
            return Call(op, self.kw(_opt(a, 0)))
        if op in ENDPOINTS:
            return Call(op)
        raise SystemExit(f"builder tabs: Python has no step {op}")

    def wrap_lines(self, lines):
        # A chain over lines is one expression in parentheses.
        return "(" + "\n".join(lines) + ")"


class Go(Lang):
    name, code = "Go", "go"
    IMPORT = 'import fenecdb "github.com/fenecdb/fenecdb/integrations/go"'
    indent = "\t"

    def s(self, v):
        return quoted(v, '"')

    def v(self, x, vector=False):
        k, w = _special(x)
        if k == "$inc":
            return f"fenecdb.Inc({_num(w)})"
        if k == "$expr":
            return "fenecdb.Expr(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: Go has no value {k}")
        if x is None:
            return "nil"
        if isinstance(x, bool):
            return "true" if x else "false"
        if isinstance(x, (int, float)):
            return _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            numbers = x and all(isinstance(e, (int, float)) and not isinstance(e, bool) for e in x)
            kind = "float64" if vector and numbers else "any"
            return f"[]{kind}{{" + ", ".join(self.v(e) for e in x) + "}"
        return self.doc(x)

    def doc(self, x):
        # A document is a D: a map has no order, and the order is the text's.
        return "fenecdb.D(" + ", ".join(f"{self.s(k)}, {self.v(e)}" for k, e in x.items()) + ")"

    def col(self, x):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.As({self.s(w[1])})"
        if k == "$expr":
            return self.v(x)
        if k == "$bucket":
            return "fenecdb.Bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f"fenecdb.CountDistinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f"fenecdb.{k[1:].title()}(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$distance":
            return f"fenecdb.Distance({self.s(w[0])}, {self.v(w[1], vector=True)})"
        raise SystemExit(f"builder tabs: Go has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f"fenecdb.{k[1:].title()}(" + ", ".join(self.cond(e) for e in w) + ")"
        if k == "$not":
            return f"fenecdb.Not({self.cond(w)})"
        if k == "$raw":
            return "fenecdb.Raw(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        # One field held to one operator is a comparison; else its fields.
        if len(x) == 1:
            (f, spec), = x.items()
            if isinstance(spec, dict) and len(spec) == 1 and _special(spec)[0] is None:
                (op, val), = spec.items()
                if op not in ("not", "distance", "within"):
                    return f"fenecdb.Cmp({self.s(f)}, {self.s(op)}, {self.v(val)})"
        return "fenecdb.Fields(" + ", ".join(f"{self.s(f)}, {self.spec(spec)}" for f, spec in x.items()) + ")"

    def spec(self, x):
        """What a field is held to: a value, or its operators as Ops, a
        `not`'s and a distance's comparisons Ops of their own."""
        if not isinstance(x, dict) or _special(x)[0] is not None:
            return self.v(x)
        return "fenecdb.Ops(" + ", ".join(
            f"{self.s(o)}, {self.spec(e) if o in ('not', 'distance') else self.v(e)}" for o, e in x.items()) + ")"

    def opts(self, o):
        out = []
        for k, x in o.items():
            if k == "ef":
                out.append(f"fenecdb.Ef({x})")
            elif k == "exact":
                out += ["fenecdb.Exact()"] if x else []
            elif k == "fuzzy":
                out += [f"fenecdb.Fuzzy({2 if x is True else x})"] if x is not False else []
            elif k == "prefix":
                out += ["fenecdb.Prefix()"] if x else []
            elif k in ("k", "candidates", "top", "limit", "offset"):
                out.append(f"fenecdb.{k.title()}({x})")
            elif k == "collate":
                out.append(f"fenecdb.Collate({self.s(x)})")
            elif k == "ranges":
                out.append("fenecdb.Ranges(" + ", ".join(_num(e) for e in x) + ")")
            elif k == "disjunctive":
                out += ["fenecdb.Disjunctive()"] if x else []
            elif k == "required":
                out += ["fenecdb.Required()"] if x else []
            elif k == "all":
                out += ["fenecdb.All()"] if x else []
            elif k == "require":
                out.append(f"fenecdb.Require({x})")
            elif k == "reason":
                out.append(f"fenecdb.Reason({self.s(x)})")
            elif k == "returning":
                # JavaScript's true is every field, "*" here.
                names = ["*"] if x is True else [] if x is False else x
                out += ["fenecdb.Returning(" + ", ".join(self.s(n) for n in names) + ")"] if names else []
            elif k in ("on", "parentKey"):
                names = [x] if isinstance(x, str) else x
                out.append(f"fenecdb.{'On' if k == 'on' else 'ParentKey'}(" + ", ".join(self.s(n) for n in names) + ")")
            elif k == "where":
                out.append(f"fenecdb.Where({self.cond(x)})")
            elif k in ("select", "group"):
                cols = [x] if isinstance(x, str) else x
                out.append(f"fenecdb.{k.title()}(" + ", ".join(self.col(e) for e in cols) + ")")
            elif k == "order":
                for key in [x] if isinstance(x, str) else x:
                    key = [key] if isinstance(key, str) else key
                    more = self.opts(key[2]) if len(key) > 2 else []
                    out.append("fenecdb.Sort(" + ", ".join(
                        [self.s(key[0]), self.s(key[1] if len(key) > 1 else "asc")] + more) + ")")
            elif k in ("pre", "post", "ellipsis"):
                continue
            else:
                raise SystemExit(f"builder tabs: Go has no option {k}")
        if "pre" in o or "post" in o:
            out.append(f"fenecdb.Tags({self.s(o['pre'])}, {self.s(o['post'])})")
        if "ellipsis" in o:
            out.insert(0, f"fenecdb.Ellipsis({self.s(o['ellipsis'])})")
        return out

    def start(self, collection):
        return Call("db.From", [self.s(collection)])

    def step(self, op, a):
        if op in ("select", "group"):
            return Call(op.title(), [self.col(x) for x in a])
        if op in ("where", "orWhere"):
            name = "OrWhere" if op == "orWhere" else "Where"
            if len(a) == 1:
                return Call(name + "Cond", [self.cond(a[0])])
            if len(a) == 2:
                # `where(field, value)` is equality, which Go writes out.
                if isinstance(a[1], (dict, list)) or a[1] is None:
                    return Call(name + "Cond", [self.cond({a[0]: a[1]})])
                return Call(name, [self.s(a[0]), self.s("="), self.v(a[1])])
            return Call(name, [self.s(a[0]), self.s(a[1]), self.v(a[2])])
        if op == "order":
            return Call("Order", [self.s(a[0]), self.s(a[1] if len(a) > 1 else "asc")] + self.opts(_opt(a, 2)))
        if op in ("limit", "offset", "require"):
            return Call(op.title(), [_num(a[0])] + self.opts(_opt(a, 1)))
        if op in ("near", "rerank"):
            return Call(op.title(), [self.s(a[0]), self.v(a[1], vector=True)] + self.opts(_opt(a, 2)))
        if op == "match":
            return Call("Match", [self.s(a[0]), self.s(a[1])] + self.opts(_opt(a, 2)))
        if op in ("highlight", "facet", "lookup"):
            return Call(op.title(), [self.s(a[0])] + self.opts(_opt(a, 1)))
        if op == "snippet":
            return Call("Snippet", [self.s(a[0]), _num(a[1])] + self.opts(_opt(a, 2)))
        if op == "fuse":
            return Call("Fuse", self.opts(_opt(a, 0)))
        if op in ("rows", "first", "count", "explain"):
            return Call(op.title(), ["ctx"])
        if op == "update":
            return Call("Update", ["ctx", self.doc(a[0])] + self.opts(_opt(a, 1)))
        if op == "delete":
            return Call("Delete", ["ctx"] + self.opts(_opt(a, 0)))
        if op == "insert":
            docs = a[0] if isinstance(a[0], list) else [a[0]]
            o = _opt(a, 1)
            more = (["fenecdb.IfAbsent()"] if o.get("ifAbsent") else []) + self.opts(
                {k: x for k, x in o.items() if k != "ifAbsent"})
            return Call("Insert", ["ctx"] + [self.doc(d) for d in docs] + more)
        if op == "upsert":
            docs = a[0] if isinstance(a[0], list) else [a[0]]
            return Call("Upsert", ["ctx", "[]any{" + ", ".join(self.doc(d) for d in docs) + "}", self.doc(a[1])]
                        + self.opts(_opt(a, 2)))
        raise SystemExit(f"builder tabs: Go has no step {op}")

    def layout(self, calls):
        one = calls[0].flat() + "".join("." + c.flat() for c in calls[1:])
        if len(one) <= WIDTH:
            return one
        # Go breaks a chain after the dot.
        lines = [calls[0].flat() + "."]
        for i, c in enumerate(calls[1:]):
            end = "." if i < len(calls) - 2 else ""
            text = "\t" + c.flat() + end
            if len(text) > WIDTH + 8 and len(c.args) > 1:
                body = ",\n".join("\t\t" + a for a in c.args) + ","
                text = f"\t{c.name}(\n{body}\n\t){end}"
            lines.append(text)
        return "\n".join(lines)


class CSharp(Lang):
    name, code = ".NET", "csharp"
    IMPORT = "using FenecDb;"
    ASYNC = {"rows": "RowsAsync", "first": "FirstAsync", "count": "CountAsync", "explain": "ExplainAsync",
             "insert": "InsertAsync", "update": "UpdateAsync", "upsert": "UpsertAsync", "delete": "DeleteAsync"}

    def s(self, v):
        return quoted(v, '"')

    def v(self, x, vector=False):
        k, w = _special(x)
        if k == "$inc":
            return f"Computed.Inc({_num(w)})"
        if k == "$expr":
            return "Computed.Expr(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: .NET has no value {k}")
        if x is None:
            return "null"
        if isinstance(x, bool):
            return "true" if x else "false"
        if isinstance(x, (int, float)):
            return _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            numbers = x and all(isinstance(e, (int, float)) and not isinstance(e, bool) for e in x)
            if vector and numbers:
                return "new[] { " + ", ".join(self.v(e) for e in x) + " }"
            return "new object?[] { " + ", ".join(self.v(e) for e in x) + " }"
        return self.doc(x)

    def doc(self, x):
        # An object's public properties, in the order declared, are a
        # document; an anonymous type's property holds no bare null.
        if all(re.fullmatch(r"[A-Za-z_]\w*", k) for k in x) and None not in x.values():
            return "new { " + ", ".join(f"{k} = {self.v(e)}" for k, e in x.items()) + " }"
        return self.dict(x)

    def dict(self, x):
        # What the builder reads as an operator object is an IDictionary.
        return ("new Dictionary<string, object?> { " + ", ".join(
            f"[{self.s(k)}] = {self.dict(e) if isinstance(e, dict) and _special(e)[0] is None else self.v(e)}"
            for k, e in x.items()) + " }")

    def col(self, x):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.As({self.s(w[1])})"
        if k == "$expr":
            return self.v(x)
        if k == "$bucket":
            return "Computed.Bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f"Computed.CountDistinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f"Computed.{k[1:].title()}(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$distance":
            return f"Computed.Distance({self.s(w[0])}, {self.v(w[1], vector=True)})"
        raise SystemExit(f"builder tabs: .NET has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f"Cond.{k[1:].title()}(" + ", ".join(self.cond(e) for e in w) + ")"
        if k == "$not":
            return f"Cond.Not({self.cond(w)})"
        if k == "$raw":
            return "Cond.Raw(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        # Comparisons as Cond.Cmp, as the languages page writes them; a
        # spec holding more than comparisons as the dictionary it is.
        plain = lambda e: not isinstance(e, (dict, list)) or _special(e)[0] is not None  # noqa: E731
        if not all(plain(s) or isinstance(s, dict) and _special(s)[0] is None and all(
                o not in ("not", "distance", "within", "in") and plain(e) for o, e in s.items()) for s in x.values()):
            return f"(Cond){self.dict(x)}"
        items = []
        for f, spec in x.items():
            if isinstance(spec, dict) and _special(spec)[0] is None:
                items += [f"Cond.Cmp({self.s(f)}, {self.s(op)}, {self.v(e)})" for op, e in spec.items()]
            else:
                items.append(f"Cond.Cmp({self.s(f)}, \"=\", {self.v(spec)})")
        return items[0] if len(items) == 1 else "Cond.And(" + ", ".join(items) + ")"

    def named(self, o):
        out = []
        for k, x in o.items():
            if k in ("on", "parentKey"):
                # Fields takes a string or a string[].
                x = self.s(x) if isinstance(x, str) else "new[] { " + ", ".join(self.s(n) for n in x) + " }"
            elif k == "where":
                x = self.cond(x)
            elif k in ("select", "group"):
                x = "[" + ", ".join(self.col(e) for e in ([x] if isinstance(x, str) else x)) + "]"
            elif k == "order":
                keys = []
                for key in [x] if isinstance(x, str) else x:
                    key = [key] if isinstance(key, str) else key
                    more = [f"{self.s(key[1] if len(key) > 1 else 'asc')}"] if len(key) > 1 else []
                    if len(key) > 2 and "collate" in key[2]:
                        more.append(f"Collate: {self.s(key[2]['collate'])}")
                    keys.append("new SortKey(" + ", ".join([self.s(key[0])] + more) + ")")
                x = "[" + ", ".join(keys) + "]"
            elif k == "ranges":
                x = "[" + ", ".join(_num(e) for e in x) + "]"
            elif k == "returning":
                x = '["*"]' if x is True else "[" + ", ".join(self.s(n) for n in x) + "]"
            else:
                x = self.v(x)
            out.append(f"{k}: {x}")
        return out

    def start(self, collection):
        return Call("db.From", [self.s(collection)])

    def step(self, op, a):
        if op in ("select", "group"):
            return Call(op.title(), [self.col(x) for x in a])
        if op in ("where", "orWhere"):
            name = "OrWhere" if op == "orWhere" else "Where"
            if len(a) == 1:
                return Call(name, [self.cond(a[0])])
            if len(a) == 2 and isinstance(a[1], dict) and _special(a[1])[0] is None:
                return Call(name, [self.s(a[0]), self.dict(a[1])])
            return Call(name, [self.s(a[0])] + ([self.s(a[1])] if len(a) == 3 else []) + [self.v(a[-1])])
        if op == "order":
            return Call("Order", [self.s(x) for x in a[:2]] + self.named(_opt(a, 2)))
        if op in ("limit", "offset", "require"):
            return Call(op.title(), [_num(a[0])] + self.named(_opt(a, 1)))
        if op in ("near", "rerank"):
            return Call(op.title(), [self.s(a[0]), self.v(a[1], vector=True)] + self.named(_opt(a, 2)))
        if op == "match":
            # fuzzy is a number of edits, JavaScript's true two; false is none.
            o = {k: (2 if x is True and k == "fuzzy" else x) for k, x in _opt(a, 2).items() if x is not False}
            return Call("Match", [self.s(a[0]), self.s(a[1])] + self.named(o))
        if op in ("highlight", "facet", "lookup"):
            return Call(op.title(), [self.s(a[0])] + self.named(_opt(a, 1)))
        if op == "snippet":
            return Call("Snippet", [self.s(a[0]), _num(a[1])] + self.named(_opt(a, 2)))
        if op == "fuse":
            return Call("Fuse", self.named(_opt(a, 0)))
        if op in ("insert", "update"):
            docs = a[0]
            first = ("new object[] { " + ", ".join(self.doc(d) for d in docs) + " }") if isinstance(docs, list) else self.doc(docs)
            return Call(self.ASYNC[op], [first] + self.named(_opt(a, 1)))
        if op == "upsert":
            docs = a[0]
            first = ("new object[] { " + ", ".join(self.doc(d) for d in docs) + " }") if isinstance(docs, list) else self.doc(docs)
            return Call("UpsertAsync", [first, self.doc(a[1])] + self.named(_opt(a, 2)))
        if op == "delete":
            return Call("DeleteAsync", self.named(_opt(a, 0)))
        if op in ENDPOINTS:
            return Call(self.ASYNC[op])
        raise SystemExit(f"builder tabs: .NET has no step {op}")

    def wrap(self, chain):
        return f"await {chain};"


class Swift(Lang):
    name, code = "Swift", "swift"
    IMPORT = "import FenecDB"

    def s(self, v):
        return quoted(v, '"', u="\\u{%x}")

    def v(self, x, top=True):
        """A value: `top`, an argument standing alone (`any FenecValue`),
        else inside a `Value` literal, whose elements are Values."""
        k, w = _special(x)
        if k == "$inc":
            return f".inc({_num(w)})"
        if k == "$expr":
            return ".expr(" + ", ".join([self.s(w[0])] + [self.v(p, False) for p in w[1:]]) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: Swift has no value {k}")
        if x is None:
            return "Value.null" if top else ".null"
        if isinstance(x, bool):
            return "true" if x else "false"
        if isinstance(x, (int, float)):
            return _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            body = "[" + ", ".join(self.v(e, False) for e in x) + "]"
            kinds = {"number" if isinstance(e, (int, float)) and not isinstance(e, bool) else type(e).__name__
                     for e in x}
            # A list of one kind is Swift's own array; any other a Value.
            return body if not top or len(kinds) == 1 and kinds <= {"number", "str", "bool"} else body + " as Value"
        # A Value, so a document's fields keep their order: a Swift
        # dictionary has none.
        body = "[" + ", ".join(f"{self.s(k)}: {self.v(e, False)}" for k, e in x.items()) + "]" if x else "[:]"
        return body + " as Value" if top else body

    def col(self, x, lead=True):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.as({self.s(w[1])})"
        if k == "$expr":
            return ".expr(" + ", ".join([self.s(w[0])] + [self.v(p, False) for p in w[1:]]) + ")"
        if k == "$bucket":
            return ".bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f".countDistinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f".{k[1:]}({self.s(w[0])}" + (f", by: {self.s(w[1])}" if len(w) > 1 else "") + ")"
        if k == "$distance":
            return f".distance({self.s(w[0])}, {self.v(w[1], False)})"
        raise SystemExit(f"builder tabs: Swift has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f".{k[1:]}(" + ", ".join(self.cond(e) for e in w) + ")"
        if k == "$not":
            return f".not({self.cond(w)})"
        if k == "$raw":
            return ".raw(" + ", ".join([self.s(w[0])] + [self.v(p, False) for p in w[1:]]) + ")"
        if len(x) == 1:
            (f, spec), = x.items()
            if isinstance(spec, dict) and len(spec) == 1 and _special(spec)[0] is None:
                (op, val), = spec.items()
                if op not in ("not", "distance", "within"):
                    return f".cmp({self.s(f)}, {self.s(op)}, {self.v(val)})"
        return self.v(x, False)

    # A call's labels in the order the method declares them.
    ORDER = {
        "lookup": ("on", "parentKey", "select", "group", "where", "required", "order", "limit", "offset"),
        "near": ("ef", "exact"), "match": ("fuzzy", "prefix"), "rerank": ("candidates",),
        "fuse": ("k", "candidates"), "highlight": ("pre", "post"), "snippet": ("ellipsis", "pre", "post"),
        "facet": ("top", "ranges", "disjunctive"), "order": ("collate",),
        "insert": ("ifAbsent", "require", "reason"), "update": ("all", "require", "reason"),
        "upsert": ("require", "reason"), "delete": ("all", "require", "reason"), "require": ("reason",),
    }

    def labels(self, op, o):
        out = []
        for k in self.ORDER[op]:
            if k not in o:
                continue
            x = o[k]
            # fuzzy is a number of edits, JavaScript's true two; false is none.
            if k == "fuzzy" and x is False:
                continue
            if k == "fuzzy" and x is True:
                x = 2
            if k in ("on", "parentKey"):
                x = self.s(x) if isinstance(x, str) else "[" + ", ".join(self.s(n) for n in x) + "]"
            elif k == "where":
                x = self.cond(x)
            elif k in ("select", "group"):
                x = "[" + ", ".join(self.col(e) for e in ([x] if isinstance(x, str) else x)) + "]"
            elif k == "order":
                keys = []
                for key in [x] if isinstance(x, str) else x:
                    if isinstance(key, str):
                        keys.append(self.s(key))
                        continue
                    more = [self.s(key[1])] if len(key) > 1 else []
                    if len(key) > 2 and "collate" in key[2]:
                        more.append(f"collate: {self.s(key[2]['collate'])}")
                    keys.append("SortKey(" + ", ".join([self.s(key[0])] + more) + ")")
                x = "[" + ", ".join(keys) + "]"
            else:
                x = self.v(x, False)
            out.append(f"{k}: {x}")
        extra = set(o) - set(self.ORDER[op])
        if extra:
            raise SystemExit(f"builder tabs: Swift's {op} takes no {', '.join(sorted(extra))} here")
        return out

    def start(self, collection):
        return Call("db.from", [self.s(collection)])

    def step(self, op, a):
        if op in ("select", "group"):
            return Call(op, [self.col(x) for x in a])
        if op in ("where", "orWhere"):
            if len(a) == 1:
                return Call(op, [self.cond(a[0])])
            return Call(op, [self.s(a[0])] + ([self.s(a[1])] if len(a) == 3 else []) + [self.v(a[-1])])
        if op == "order":
            return Call(op, [self.s(x) for x in a[:2]] + self.labels(op, _opt(a, 2)))
        if op in ("limit", "offset", "require"):
            return Call(op, [_num(a[0])] + (self.labels(op, _opt(a, 1)) if op == "require" else []))
        if op in ("near", "rerank"):
            return Call(op, [self.s(a[0]), self.v(a[1])] + self.labels(op, _opt(a, 2)))
        if op == "match":
            return Call(op, [self.s(a[0]), self.s(a[1])] + self.labels(op, _opt(a, 2)))
        if op in ("highlight", "facet", "lookup"):
            return Call(op, [self.s(a[0])] + self.labels(op, _opt(a, 1)))
        if op == "snippet":
            return Call(op, [self.s(a[0]), _num(a[1])] + self.labels(op, _opt(a, 2)))
        if op == "fuse":
            return Call(op, self.labels(op, _opt(a, 0)))
        if op in self.WRITES:
            return self.write(op, a, self.labels,
                              lambda r: "returning: [" + ", ".join(self.s(n) for n in r) + "]")
        if op in ENDPOINTS:
            return Call(op)
        raise SystemExit(f"builder tabs: Swift has no step {op}")

    def wrap(self, chain):
        return f"try await {chain}"


class Kotlin(Lang):
    name, code = "Kotlin", "kotlin"
    IMPORT = "import com.fenecdb.*"

    def s(self, v):
        return quoted(v, '"', also="$")

    def v(self, x, vector=False):
        k, w = _special(x)
        if k == "$inc":
            return f"Computed.inc({_num(w)})"
        if k == "$expr":
            return "Computed.expr(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: Kotlin has no value {k}")
        if x is None:
            return "null"
        if isinstance(x, bool):
            return "true" if x else "false"
        if isinstance(x, (int, float)):
            # A vector's numbers are Doubles, 0 written 0.0.
            return f"{_num(x)}.0" if vector and "." not in _num(x) and "e" not in _num(x) else _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            return "listOf(" + ", ".join(self.v(e, vector) for e in x) + ")"
        if not x:
            return "emptyMap<String, Any?>()"
        return "mapOf(" + ", ".join(f"{self.s(k)} to {self.v(e)}" for k, e in x.items()) + ")"

    def col(self, x):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.alias({self.s(w[1])})"
        if k == "$expr":
            return self.v(x)
        if k == "$bucket":
            return "Computed.bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f"Computed.countDistinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f"Computed.{k[1:]}(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$distance":
            return f"Computed.distance({self.s(w[0])}, {self.v(w[1], True)})"
        raise SystemExit(f"builder tabs: Kotlin has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f"Cond.{k[1:]}(" + ", ".join(self.cond(e) for e in w) + ")"
        if k == "$not":
            return f"Cond.not({self.cond(w)})"
        if k == "$raw":
            return "Cond.raw(" + ", ".join([self.s(w[0])] + [self.v(p) for p in w[1:]]) + ")"
        if len(x) == 1:
            (f, spec), = x.items()
            if isinstance(spec, dict) and len(spec) == 1 and _special(spec)[0] is None:
                (op, val), = spec.items()
                if op not in ("not", "distance", "within"):
                    return f"Cond.cmp({self.s(f)}, {self.s(op)}, {self.v(val)})"
        return self.v(x)

    def named(self, o):
        out = []
        for k, x in o.items():
            # fuzzy is a number of edits, JavaScript's true two; false is none.
            if k == "fuzzy" and x is False:
                continue
            if k == "fuzzy" and x is True:
                x = 2
            if k in ("on", "parentKey"):
                x = self.s(x) if isinstance(x, str) else "listOf(" + ", ".join(self.s(n) for n in x) + ")"
            elif k == "where":
                x = self.cond(x)
            elif k in ("select", "group"):
                x = "listOf(" + ", ".join(self.col(e) for e in ([x] if isinstance(x, str) else x)) + ")"
            elif k == "order":
                keys = []
                for key in [x] if isinstance(x, str) else x:
                    key = [key] if isinstance(key, str) else key
                    more = [self.s(key[1])] if len(key) > 1 else []
                    if len(key) > 2 and "collate" in key[2]:
                        more.append(self.s(key[2]["collate"]))
                    keys.append("SortKey(" + ", ".join([self.s(key[0])] + more) + ")")
                x = "listOf(" + ", ".join(keys) + ")"
            elif k == "returning":
                x = 'listOf("*")' if x is True else "listOf(" + ", ".join(self.s(n) for n in x) + ")"
            else:
                x = self.v(x)
            out.append(f"{k} = {x}")
        return out

    def start(self, collection):
        return Call("db.from", [self.s(collection)])

    def step(self, op, a):
        if op in ("select", "group"):
            return Call(op, [self.col(x) for x in a])
        if op in ("where", "orWhere"):
            if len(a) == 1:
                return Call(op, [self.cond(a[0])])
            return Call(op, [self.s(a[0])] + ([self.s(a[1])] if len(a) == 3 else []) + [self.v(a[-1])])
        if op == "order":
            return Call(op, [self.s(x) for x in a[:2]] + self.named(_opt(a, 2)))
        if op in ("limit", "offset", "require"):
            return Call(op, [_num(a[0])] + self.named(_opt(a, 1)))
        if op in ("near", "rerank"):
            return Call(op, [self.s(a[0]), self.v(a[1], True)] + self.named(_opt(a, 2)))
        if op == "match":
            return Call(op, [self.s(a[0]), self.s(a[1])] + self.named(_opt(a, 2)))
        if op in ("highlight", "facet", "lookup"):
            return Call(op, [self.s(a[0])] + self.named(_opt(a, 1)))
        if op == "snippet":
            return Call(op, [self.s(a[0]), _num(a[1])] + self.named(_opt(a, 2)))
        if op == "fuse":
            return Call(op, self.named(_opt(a, 0)))
        if op in self.WRITES:
            return self.write(op, a, lambda _, o: self.named(o),
                              lambda r: "returning = listOf(" + ", ".join(self.s(n) for n in r) + ")")
        if op in ENDPOINTS:
            return Call(op)
        raise SystemExit(f"builder tabs: Kotlin has no step {op}")


class Dart(Lang):
    name, code = "Dart", "dart"
    IMPORT = "import 'package:fenecdb/fenecdb.dart';"

    def s(self, v):
        return quoted(v, "'", also="$")

    def v(self, x):
        k, w = _special(x)
        if k == "$inc":
            return f"Computed.inc({_num(w)})"
        if k == "$expr":
            return "Computed.expr(" + ", ".join([self.s(w[0])] + (["[" + ", ".join(self.v(p) for p in w[1:]) + "]"]
                                                                    if len(w) > 1 else [])) + ")"
        if k is not None:
            raise SystemExit(f"builder tabs: Dart has no value {k}")
        if x is None:
            return "null"
        if isinstance(x, bool):
            return "true" if x else "false"
        if isinstance(x, (int, float)):
            return _num(x)
        if isinstance(x, str):
            return self.s(x)
        if isinstance(x, list):
            return "[" + ", ".join(self.v(e) for e in x) + "]"
        return "{" + ", ".join(f"{self.s(k)}: {self.v(e)}" for k, e in x.items()) + "}"

    def col(self, x):
        k, w = _special(x)
        if k is None:
            return self.s(x)
        if k == "$as":
            return f"{self.col(w[0])}.as({self.s(w[1])})"
        if k == "$expr":
            return self.v(x)
        if k == "$bucket":
            return "Computed.bucket(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$countDistinct":
            return f"Computed.countDistinct({self.s(w)})"
        if k in ("$first", "$last"):
            return f"Computed.{k[1:]}(" + ", ".join(self.s(p) for p in w) + ")"
        if k == "$distance":
            return f"Computed.distance({self.s(w[0])}, {self.v(w[1])})"
        raise SystemExit(f"builder tabs: Dart has no column {k}")

    def cond(self, x):
        k, w = _special(x)
        if k in ("$or", "$and"):
            return f"Cond.{k[1:]}([" + ", ".join(self.cond(e) for e in w) + "])"
        if k == "$not":
            return f"Cond.not({self.cond(w)})"
        if k == "$raw":
            return "Cond.raw(" + ", ".join([self.s(w[0])] + (["[" + ", ".join(self.v(p) for p in w[1:]) + "]"]
                                                              if len(w) > 1 else [])) + ")"
        if len(x) == 1:
            (f, spec), = x.items()
            if isinstance(spec, dict) and len(spec) == 1 and _special(spec)[0] is None:
                (op, val), = spec.items()
                if op not in ("not", "distance", "within"):
                    return f"Cond.cmp({self.s(f)}, {self.s(op)}, {self.v(val)})"
        return self.v(x)

    def named(self, o):
        out = []
        for k, x in o.items():
            if k in ("on", "parentKey"):
                x = self.s(x) if isinstance(x, str) else "[" + ", ".join(self.s(n) for n in x) + "]"
            elif k == "where":
                x = self.cond(x)
            elif k == "select":
                x = "[" + ", ".join(self.col(e) for e in ([x] if isinstance(x, str) else x)) + "]"
            elif k == "group":
                x = self.col(x) if isinstance(x, str) else "[" + ", ".join(self.col(e) for e in x) + "]"
            elif k == "order":
                keys = []
                for key in [x] if isinstance(x, str) else x:
                    key = [key] if isinstance(key, str) else key
                    more = [self.s(key[1])] if len(key) > 1 else []
                    if len(key) > 2 and "collate" in key[2]:
                        more.append(self.s(key[2]["collate"]))
                    keys.append("SortKey(" + ", ".join([self.s(key[0])] + more) + ")")
                x = "[" + ", ".join(keys) + "]"
            elif k == "returning":
                x = "['*']" if x is True else "[" + ", ".join(self.s(n) for n in x) + "]"
            else:
                x = self.v(x)
            out.append(f"{k}: {x}")
        return out

    def start(self, collection):
        return Call("db.from", [self.s(collection)])

    def step(self, op, a):
        if op == "select":
            return Call(op, [self.col(x) for x in a], "([", "])")
        if op == "group":
            if len(a) == 1:
                return Call(op, [self.col(a[0])])
            return Call(op, [self.col(x) for x in a], "([", "])")
        if op in ("where", "orWhere"):
            if len(a) == 1:
                return Call(op, [self.cond(a[0])])
            return Call(op, [self.s(a[0])] + ([self.s(a[1])] if len(a) == 3 else []) + [self.v(a[-1])])
        if op == "order":
            # The direction and the collation are positional.
            args = [self.s(x) for x in a[:2]]
            if len(a) > 2 and "collate" in a[2]:
                args += [self.s("asc")] * (len(args) == 1) + [self.s(a[2]["collate"])]
            return Call(op, args)
        if op in ("limit", "offset", "require"):
            return Call(op, [_num(a[0])] + self.named(_opt(a, 1)))
        if op in ("near", "rerank"):
            return Call(op, [self.s(a[0]), self.v(a[1])] + self.named(_opt(a, 2)))
        if op == "match":
            return Call(op, [self.s(a[0]), self.s(a[1])] + self.named(_opt(a, 2)))
        if op in ("highlight", "facet", "lookup"):
            return Call(op, [self.s(a[0])] + self.named(_opt(a, 1)))
        if op == "snippet":
            return Call(op, [self.s(a[0]), _num(a[1])] + self.named(_opt(a, 2)))
        if op == "fuse":
            return Call(op, self.named(_opt(a, 0)))
        if op in self.WRITES:
            return self.write(op, a, lambda _, o: self.named(o),
                              lambda r: "returning: [" + ", ".join(self.s(n) for n in r) + "]")
        if op in ENDPOINTS:
            return Call(op)
        raise SystemExit(f"builder tabs: Dart has no step {op}")

    def wrap(self, chain):
        return f"await {chain};"


LANGS = [JS(), Python(), Go(), CSharp(), Swift(), Kotlin(), Dart()]


def render(lang, name):
    return lang.render(case(name))


# -------------------------------------------------------------------- the box

# The block under the placeholder is the box's FenecQL tab; with none, the
# tab is written from the cases (`written`).
PLACEHOLDER = re.compile(
    r'\{\{builder(?P<names>(?:\s+"[^"]+")+)\s*\}\}'
    r'(?:\s*\n(?P<pre><pre data-lang="fenecql"[^>]*>(?P<code>[\s\S]*?)</pre>))?')
NAME = re.compile(r'"([^"]+)"')

# Where a statement too long for its line is broken: before a clause, outside
# any bracket and string.
CLAUSES = ("select", "where", "group", "near", "match", "rerank", "fuse", "order", "limit", "offset",
           "require", "count", "facet", "lookup", "returning")


def _texts(v):
    """Every text in a value, its keys among them."""
    if isinstance(v, str):
        yield v
    elif isinstance(v, list):
        for x in v:
            yield from _texts(x)
    elif isinstance(v, dict):
        for k, x in v.items():
            yield k
            yield from _texts(x)


def written(c):
    """A case's statement as a page writes it: each parameter written in as
    FenecQL writes the value, and one longer than WIDTH broken before its
    clauses, a line going on indented -- before each `lookup` level, whose
    clauses are its children's."""
    out = []
    for p in re.split(r"(\$\d+)", c["text"]):
        if not p.startswith("$"):
            out.append(p)
            continue
        v = c["params"][int(p[1:]) - 1]
        if any(ord(ch) in UNWRITABLE for s in _texts(v) for ch in s):
            # FenecQL's strings take no `\u` escape: such a value is a page's
            # own block to write.
            raise SystemExit(f"builder tabs: case {c['name']!r} binds a text FenecQL cannot write in "
                             "(a C0 control but \\n, \\t, \\r and \\0, U+0085, U+2028 or U+2029)")
        out.append(literal(v))
    text = "".join(out)
    if len(text) <= WIDTH:
        return text
    cuts, depth, quote, i = [0], 0, None, 0
    while i < len(text):
        ch = text[i]
        if quote:
            if ch == "\\":
                i += 1
            elif ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == " " and not depth and re.match(rf"({'|'.join(CLAUSES)}) ", text[i + 1:]):
            cuts.append(i)
        i += 1
    parts = [text[a:b] for a, b in zip(cuts, cuts[1:] + [len(text)])]
    # The first clause stays on the statement's line: `get docs` alone
    # reads as a statement of its own.
    lines = ["".join(parts[:2])]
    for p in parts[2:]:
        if p.startswith(" lookup ") or len(lines[-1]) + len(p) > WIDTH:
            lines.append("  " + p[1:])
        else:
            lines[-1] += p
    return "\n".join(lines)


def sends(names):
    """What the code sends, a statement a line, its parameters under it."""
    lines = []
    for n in names:
        c = case(n)
        lines.append(c["text"])
        if c["params"]:
            lines.append("-- " + json.dumps(c["params"], ensure_ascii=False))
    return "\n".join(lines)


def box(names, pre, code, where):
    if pre is None:
        # No block of the page's: the statements as the cases make them, a
        # blank line between two as between the tabs' chains.
        code = html.escape("\n\n".join(written(case(n)) for n in names), quote=False)
        pre = f'<pre data-lang="fenecql">{code}</pre>'
    found = statements(html.unescape(code))
    if len(found) != len(names):
        raise SystemExit(
            f"{where}: {{{{builder}}}} names {len(names)} cases for a block of {len(found)} statements")
    for n, stmt in zip(names, found):
        c = case(n)
        if not made(c, stmt):
            raise SystemExit(
                f"{where}: the case {n!r} makes\n  {c['text']}  {json.dumps(c['params'])}\n"
                f"which is not the page's\n  {stmt}")
    out = ['<div class="langs builder" data-langs>',
           f'<div class="lang" data-name="FenecQL" data-keep="false">\n{pre}\n</div>']
    for lang in LANGS:
        snippet = "\n\n".join(render(lang, n) for n in names)
        out.append(f'<div class="lang" data-name="{lang.name}">\n'
                   f'<pre data-lang="{lang.code}" data-label="{html.escape(lang.needs(snippet))}">'
                   f'{html.escape(snippet, quote=False)}</pre>\n</div>')
    # Once for every tab, which the widget leaves showing beside the
    # panels: the same statements under seven tabs were seven copies in the
    # page's text, its search sections and llms-full.txt.
    out.append(f'<div class="builder-sends">\n<pre data-lang="fenecql" data-label="sends">'
               f'{html.escape(sends(names), quote=False)}</pre>\n</div>')
    out.append("</div>")
    return "\n".join(out)


def expand(raw, where="a page"):
    """The page with each `{{builder ...}}` and the block under it written
    as a box of tabs."""
    if "{{builder" not in raw:
        return raw

    def sub(m):
        if m.group("pre") is None and raw[m.end():].lstrip().startswith("<pre"):
            raise SystemExit(f'{where}: a {{{{builder "case"}}}} over a block that is not <pre data-lang="fenecql">')
        return box(NAME.findall(m.group("names")), m.group("pre"), m.group("code"), where)

    out = PLACEHOLDER.sub(sub, raw)
    if "{{builder" in out:
        raise SystemExit(f'{where}: a {{{{builder}}}} that names no case')
    return out


def used():
    """Every case a page's tabs show, in the order the pages show them."""
    names = []
    for dirpath, _, files in sorted(os.walk(CONTENT)):
        for f in sorted(files):
            if not f.endswith(".html"):
                continue
            text = open(os.path.join(dirpath, f), encoding="utf-8").read()
            for m in PLACEHOLDER.finditer(text):
                for n in NAME.findall(m.group("names")):
                    if n not in names:
                        names.append(n)
    return names


# ------------------------------------------------------- the SDKs' test files

WRITTEN = "Written by site/builder_tabs.py (make docs-builder): do not edit."
HEADER = ("The code of each builder tab on the site, as the page shows it, run against a "
          "transport that records and held to the statement its case of integrations/builder-golden.json "
          "makes: a tab shows code this SDK runs. make docs-builder writes the file again as the pages' "
          "tabs change, and site/builder_tabs.py --check fails while it is behind them.")


def _lines(text, width=78, lead=""):
    out, line = [], lead
    for word in text.split():
        if len(line) + len(word) + 1 > width and line.strip():
            out.append(line.rstrip())
            line = lead
        line += word + " "
    out.append(line.rstrip())
    return "\n".join(out)


def _ident(name, upper=False, taken=None):
    words = re.findall(r"[A-Za-z0-9]+", name.removeprefix("docs: "))
    ident = "".join(w.capitalize() if i or upper else w.lower() for i, w in enumerate(words))
    if taken is not None:
        base, n = ident, 2
        while ident in taken:
            ident, n = f"{base}{n}", n + 1
        taken.add(ident)
    return ident


def _indented(code, pad):
    return "\n".join(pad + line if line else line for line in code.split("\n"))


def go_test(names):
    lang = Go()
    body = []
    for n in names:
        c = case(n)
        body.append(
            f"\tt.Run({lang.s(n)}, func(t *testing.T) {{\n"
            # A tab that sent nothing finds no body of the tab before it.
            "\t\trec.body = nil\n"
            f"\t\t_, err := {_indented(render(lang, n), chr(9) * 2).lstrip()}\n"
            f"\t\tsent(t, err, {lang.s(c['text'])}, {lang.s(json.dumps(c['params'], ensure_ascii=False))})\n"
            "\t})\n")
    return f"""// Code generated by site/builder_tabs.py (make docs-builder); DO NOT EDIT.

package fenecdb_test

{_lines(HEADER, lead="// ")}

import (
\t"context"
\t"encoding/json"
\t"net/http/httptest"
\t"testing"

\tfenecdb "github.com/fenecdb/fenecdb/integrations/go"
)

func TestDocsBuilderTabs(t *testing.T) {{
\trec := &recorder{{}}
\tsrv := httptest.NewServer(rec)
\tdefer srv.Close()
\t// What the tabs call db and ctx.
\tdb := fenecdb.New(srv.URL)
\tctx := context.Background()
\tsent := func(t *testing.T, err error, text, params string) {{
\t\tt.Helper()
\t\tif err != nil {{
\t\t\tt.Fatal(err)
\t\t}}
\t\tvar body struct {{
\t\t\tQuery  string
\t\t\tParams any
\t\t}}
\t\tif err := json.Unmarshal(rec.body, &body); err != nil {{
\t\t\tt.Fatal(err)
\t\t}}
\t\tvar want any
\t\tif err := json.Unmarshal([]byte(params), &want); err != nil {{
\t\t\tt.Fatal(err)
\t\t}}
\t\tif body.Query != text || !sameJSON(body.Params, want) {{
\t\t\tt.Fatalf("sent %q %v, want %q %s", body.Query, body.Params, text, params)
\t\t}}
\t}}

{chr(10).join(body)}}}
"""


def cs_test(names):
    lang, taken = CSharp(), set()
    body = []
    for n in names:
        c = case(n)
        body.append(
            f"    [Fact(DisplayName = {lang.s(n)})]\n"
            f"    public async Task {_ident(n, True, taken)}()\n"
            "    {\n"
            "        using var db = Db(out var sent);\n"
            f"{_indented(render(lang, n), ' ' * 8)}\n"
            f"        Sends(sent, {lang.s(c['text'])}, {lang.s(json.dumps(c['params'], ensure_ascii=False))});\n"
            "    }\n")
    return f"""// {WRITTEN}

{_lines(HEADER, lead="// ")}

using System.Net;
using System.Text;
using System.Text.Json;
using Xunit;

namespace FenecDb.Tests;

public sealed class DocsBuilderTests
{{
    /// <summary>A server that answers as fenec-server would and keeps the last body it was sent.</summary>
    sealed class Recorder : HttpMessageHandler
    {{
        public string? Body;

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken ct)
        {{
            Body = await request.Content!.ReadAsStringAsync(ct);
            var query = JsonDocument.Parse(Body).RootElement.GetProperty("query").GetString()!;
            var rows = query.EndsWith(" count") || query.Contains(" count lookup ") ? \"\"\"[{{"count":0}}]\"\"\" : "[]";
            var answer = query.Split(' ')[0] is "put" or "set" or "del" ? \"\"\"{{"affected":0}}\"\"\"
                : query.Contains(" facet ") ? \"\"\"{{"rows":\"\"\" + rows + \"\"\","facets":{{}}}}\"\"\" : rows;
            return new HttpResponseMessage(HttpStatusCode.OK) {{ Content = new StringContent(answer, Encoding.UTF8, "application/json") }};
        }}
    }}

    /// <summary>What the tabs call db: a client of a server that records.</summary>
    static FenecClient Db(out Recorder sent)
    {{
        sent = new Recorder();
        return new FenecClient("http://docs.invalid", new() {{ HttpClient = new HttpClient(sent) }});
    }}

    static void Sends(Recorder sent, string text, string parameters)
    {{
        var body = JsonDocument.Parse(sent.Body!).RootElement;
        Assert.Equal(text, body.GetProperty("query").GetString());
        var got = body.GetProperty("params");
        Assert.True(Same(JsonDocument.Parse(parameters).RootElement, got), $"params {{got.GetRawText()}}, want {{parameters}}");
    }}

    // JSON compared by value: a number written 1 and 1.0 is one number to the server.
    static bool Same(JsonElement a, JsonElement b) => (a.ValueKind, b.ValueKind) switch
    {{
        (JsonValueKind.Number, JsonValueKind.Number) => a.GetDouble() == b.GetDouble(),
        (JsonValueKind.String, JsonValueKind.String) => a.GetString() == b.GetString(),
        (JsonValueKind.Array, JsonValueKind.Array) => a.GetArrayLength() == b.GetArrayLength()
            && a.EnumerateArray().Zip(b.EnumerateArray()).All(p => Same(p.First, p.Second)),
        (JsonValueKind.Object, JsonValueKind.Object) => a.EnumerateObject().Count() == b.EnumerateObject().Count()
            && a.EnumerateObject().All(p => b.TryGetProperty(p.Name, out var v) && Same(p.Value, v)),
        var (x, y) when x == y => a.GetRawText() == b.GetRawText(),
        _ => false,
    }};

{chr(10).join(body)}}}
"""


def swift_test(names):
    lang, taken = Swift(), set()
    body = []
    for n in names:
        c = case(n)
        body.append(
            f"    @Test({lang.s(n)}) func {_ident(n, False, taken)}() async throws {{\n"
            "        let db = Recording()\n"
            f"        _ = {_indented(render(lang, n), ' ' * 8).lstrip()}\n"
            f"        try await db.sends({lang.s(c['text'])}, {lang.s(json.dumps(c['params'], ensure_ascii=False))})\n"
            "    }\n")
    return f"""// {WRITTEN}

{_lines(HEADER, lead="// ")}

import Testing

@testable import FenecDB

@Suite struct DocsBuilder {{
    /// What the tabs call `db`: queries bound to a recorder, as a database's are to its engine.
    struct Recording: Sendable {{
        let sent = Sent()

        func from(_ collection: String) throws -> Query {{
            try Query.from(collection).bind {{ [sent] text, params in
                await sent.record(text, params)
                if text.hasPrefix("put ") || text.hasPrefix("set ") || text.hasPrefix("del ") {{ return .affected(0) }}
                if text.hasSuffix(" count") {{ return .rows(columns: ["count"], rows: [["count": 0]]) }}
                return .rows(columns: [], rows: [])
            }}
        }}

        func sends(_ text: String, _ params: String) async throws {{
            let got = await sent.last
            #expect(got?.0 == text)
            #expect(Golden.same(.array(got?.1 ?? []), try Value.parse(params)), "\\(Value.array(got?.1 ?? []).json)")
        }}
    }}

    actor Sent {{
        var last: (String, [Value])?
        func record(_ text: String, _ params: [Value]) {{ last = (text, params) }}
    }}

{chr(10).join(body)}}}
"""


def kotlin_test(names):
    lang, taken = Kotlin(), set()
    body = []
    for n in names:
        c = case(n)
        body.append(
            f"    /** {n} */\n"
            "    @Test\n"
            f"    fun {_ident(n, False, taken)}() = runBlocking {{\n"
            "        val db = Recording()\n"
            f"{_indented(render(lang, n), ' ' * 8)}\n"
            f"        db.sends({lang.s(c['text'])}, {lang.s(json.dumps(c['params'], ensure_ascii=False))})\n"
            "    }\n")
    return f"""// {WRITTEN}

{_lines(HEADER, lead="// ")}

package com.fenecdb

import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class DocsBuilderTest {{
    /** What the tabs call `db`: queries bound to a recorder, as a database's are to its engine. */
    private class Recording {{
        var sent: Statement? = null

        fun from(collection: String): Query = Query.from(collection).bind {{ text, params ->
            sent = Statement(text, params)
            when {{
                Regex("^(put|set|del) ").containsMatchIn(text) -> Answer.Affected(0)
                text.endsWith(" count") -> Answer.Rows(listOf("count"), listOf(Row(mapOf("count" to 0L))))
                else -> Answer.Rows(emptyList(), emptyList())
            }}
        }}

        fun sends(text: String, params: String) {{
            assertEquals(text, sent?.text)
            val got = Json.parse(Json.write(sent?.params))
            assertTrue(GoldenTest.same(got, Json.parse(params)), "params $got, want $params")
        }}
    }}

{chr(10).join(body)}}}
"""


def dart_test(names):
    lang = Dart()
    body = []
    for n in names:
        c = case(n)
        body.append(
            f"  test({lang.s(n)}, () async {{\n"
            "    final db = _Recording();\n"
            f"{_indented(render(lang, n), '    ')}\n"
            f"    db.sends({lang.s(c['text'])}, {lang.s(json.dumps(c['params'], ensure_ascii=False))});\n"
            "  });\n")
    return f"""// {WRITTEN}

{_lines(HEADER, lead="// ")}

import 'dart:convert';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

/// What the tabs call `db`: queries bound to a recorder, as a database's are to its engine.
class _Recording {{
  ({{String text, List<Object?> params}})? sent;

  Query from(String collection) => Query.from(collection).bind((text, params) async {{
        sent = (text: text, params: params);
        if (RegExp(r'^(put|set|del) ').hasMatch(text)) return Answer.of({{'kind': 'affected', 'count': 0}});
        return Answer.of({{
          'kind': 'rows',
          'result': {{
            'columns': [],
            'rows': text.endsWith(' count')
                ? [
                    {{'count': 0}}
                  ]
                : []
          }}
        }});
      }});

  void sends(String text, String params) {{
    expect(sent?.text, text);
    final got = jsonDecode(jsonEncode(sent?.params));
    expect(_same(got, jsonDecode(params)), isTrue, reason: 'params $got, want $params');
  }}
}}

/// JSON compared by value: a number written 1 and 1.0 is one number.
bool _same(Object? a, Object? b) {{
  if (a is num && b is num) return a.toDouble() == b.toDouble();
  if (a is List && b is List) {{
    return a.length == b.length && [for (var i = 0; i < a.length; i++) i].every((i) => _same(a[i], b[i]));
  }}
  if (a is Map && b is Map) return a.length == b.length && a.keys.every((k) => b.containsKey(k) && _same(a[k], b[k]));
  return a == b;
}}

void main() {{
{chr(10).join(body)}}}
"""


# Each file, and the formatter its SDK's tests hold it to, run where the file
# is: `dart format` lays a file out by its package's language version, which
# it reads off what `dart pub get` writes -- 3.4's style; from a 3.7
# constraint on it writes a comma after a broken list's last item, which the
# comparison below allows for.
TESTS = {
    "integrations/go/docs_builder_test.go": (go_test, None),
    "integrations/dotnet/FenecDb.Tests/DocsBuilderTests.cs": (cs_test, None),
    "integrations/swift/Tests/FenecDBTests/DocsBuilderTests.swift": (swift_test, None),
    "integrations/kotlin/fenecdb/src/test/kotlin/com/fenecdb/DocsBuilderTest.kt": (kotlin_test, None),
    "integrations/dart/fenecdb/test/docs_builder_test.dart": (
        dart_test, ("integrations/dart/fenecdb", [["dart", "pub", "get"], ["dart", "format", "test/docs_builder_test.dart"]])),
}


# Each file's string syntax: its quotes, which take a backslash's escape;
# its raw strings, which take none; Dart's `r'...'`.
STRINGS = {
    ".go": {"quotes": "\"'", "raw": ("`",)},
    ".cs": {"quotes": "\"'", "raw": ('"""',)},
    ".swift": {"quotes": '"', "raw": ()},
    ".kt": {"quotes": "\"'", "raw": ()},
    ".dart": {"quotes": "\"'", "raw": ("r'", 'r"')},
}


def tokens(text, ext):
    """A source file as its formatter cannot change it: each string literal
    and each `//` comment whole, every other character outside whitespace
    one token, and a comma before a closing bracket gone -- `gofmt` and
    `dart format` move whitespace, and Dart's tall style adds that comma.
    Whitespace inside a string is part of it."""
    syntax, out, i, n = STRINGS[ext], [], 0, len(text)
    while i < n:
        ch = text[i]
        raw = next((r for r in syntax["raw"] if text.startswith(r, i)
                    and not (r[0] == "r" and i and (text[i - 1].isalnum() or text[i - 1] == "_"))), None)
        if raw:
            close = raw[-1] if raw[0] == "r" else raw
            end = text.find(close, i + len(raw))
            end = n if end < 0 else end + len(close)
        elif text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            out.append(text[i:end].rstrip())
            i = end
            continue
        elif ch in syntax["quotes"]:
            end = i + 1
            while end < n and text[end] not in (ch, "\n"):
                end += 2 if text[end] == "\\" else 1
            end = min(end + 1, n)
        elif ch.isspace():
            i += 1
            continue
        else:
            end = i + 1
        out.append(text[i:end])
        i = end
    return [t for k, t in enumerate(out) if not (t == "," and k + 1 < len(out) and out[k + 1] in (")", "]", "}"))]


def _written_selftest():
    """`written` held to what it is for: values in as FenecQL writes them, a
    long statement's lines, and a text it cannot write refused."""
    def w(text, *params):
        return written({"name": "selftest", "text": text, "params": list(params)})

    assert w("put d {a: $1}", {"a-b": 1, "ok": 2, "null": 3, "año": 4}) \
        == 'put d {a: {"a-b": 1, ok: 2, "null": 3, año: 4}}', "a key that is no name is quoted"
    assert w("get d where t = $1", 'q"b\\s\n\t\r\0') == 'get d where t = "q\\"b\\\\s\\n\\t\\r\\0"', \
        "FenecQL's own escapes"
    for ch in ("\x01", "\x1f", "\x85", " ", " "):
        try:
            w("get d where t = $1", "a" + ch)
        except SystemExit:
            continue
        raise AssertionError(f"written wrote U+{ord(ch):04X}, which FenecQL's strings cannot")
    one = ("get docs where year >= $1 and (lang = $2 or tags has $3) order title collate tr asc limit $4 "
           "lookup comments on doc_id limit $5")
    lines = w(one, 2024, "x order y", "rust", 20, 3)
    assert lines == ('get docs where year >= 2024 and (lang = "x order y" or tags has "rust")\n'
                     "  order title collate tr asc limit 20\n"
                     "  lookup comments on doc_id limit 3"), f"a long statement's lines:\n{lines}"
    assert statements(lines) == ['get docs where year >= 2024 and (lang = "x order y" or tags has "rust") '
                                 "order title collate tr asc limit 20 lookup comments on doc_id limit 3"], \
        "its lines read back as the one statement"
    assert w("get d where a = $1 limit $2", "short", 5) == 'get d where a = "short" limit 5', "a short one stays one line"


def selftest():
    """The comparison and the comment cutter held to what they are for."""
    _written_selftest()
    assert tokens('f("pub lished")', ".go") != tokens('f("published")', ".go"), "a string's space"
    assert tokens("f( a ,\n  b, )", ".dart") == tokens("f(a, b)", ".dart"), "a formatter's comma and spaces"
    assert tokens("// the site's\nx('a b')", ".kt") != tokens("// the site's\nx('ab')", ".kt"), "a comment's quote"
    assert tokens("RegExp(r'^(put|set) \\d')", ".dart") != tokens("RegExp(r'^(put|set)\\d')", ".dart"), "a raw string"
    assert tokens('x = """{"a": 1}""" ', ".cs") != tokens('x = """{"a":1}"""', ".cs"), "a raw C# string"
    assert _uncommented('get a where b = "x\\" -- y" -- z') == 'get a where b = "x\\" -- y" ', "an escaped quote"
    for lang in LANGS:
        assert not set(lang.s(SPECIALS)) & set(map(chr, ESCAPED)), f"{lang.name} writes a character raw"


def tested():
    """What the SDKs' test files run: every tab a page shows, and the
    check's own cases of the characters a literal escapes."""
    return used() + list(ESCAPES)


def write_tests():
    names = tested()
    for rel, (make, fmt) in TESTS.items():
        path = os.path.join(REPO, rel)
        with open(path, "w", encoding="utf-8") as f:
            f.write(make(names))
        if rel.endswith(".go") and shutil.which("gofmt"):
            subprocess.run(["gofmt", "-w", path], check=True)
        if fmt and shutil.which(fmt[1][0][0]):
            for cmd in fmt[1]:
                subprocess.run(cmd, cwd=os.path.join(REPO, fmt[0]), check=True, capture_output=True)
        print(f"  {rel}: {len(names)} tabs")


def stale_tests():
    names, stale = tested(), []
    for rel, (make, _) in TESTS.items():
        path = os.path.join(REPO, rel)
        have = open(path, encoding="utf-8").read() if os.path.exists(path) else ""
        ext = os.path.splitext(rel)[1]
        if tokens(have, ext) != tokens(make(names), ext):
            stale.append(rel)
    return stale


# --------------------------------------------------- JavaScript and Python run


def _same(a, b):
    if isinstance(a, bool) or isinstance(b, bool):
        return a is b
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return a == b
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(_same(x, y) for x, y in zip(a, b))
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(_same(a[k], b[k]) for k in a)
    return a == b


def _answer(text):
    """What fenec-server answers a statement, as far as the clients read it."""
    if re.match(r"(put|set|del) ", text):
        return {"affected": 0}
    rows = [{"count": 0}] if text.endswith(" count") or " count lookup " in text else []
    return {"rows": rows, "facets": {}} if " facet " in text else rows


JS_RUN = """
import { connect, or, and, not, raw, inc, expr, bucket, countDistinct, distance, first, last } from '@@CLIENT@@';
const answers = @@ANSWERS@@;
let sent = [];
const fetch = async (url, init) => {
  const { query, params } = JSON.parse(init.body);
  sent.push({ text: query, params });
  return new Response(JSON.stringify(answers[query] ?? []), { status: 200, headers: { 'content-type': 'application/json' } });
};
const db = connect('http://docs.invalid', { fetch });
const out = {};
@@CASES@@
console.log(JSON.stringify(out));
"""


def run_js(names):
    """Each JavaScript tab as the page shows it, in Node: what it sent."""
    client = "file://" + os.path.join(REPO, "web", "client.js")
    blocks = []
    for n in names:
        code = render(JS(), n)
        blocks.append(f"sent = [];\ntry {{\n{code}\n  out[{json.dumps(n)}] = sent;\n}} catch (e) {{\n"
                      f"  out[{json.dumps(n)}] = {{ error: String(e) }};\n}}")
    answers = {case(n)["text"]: _answer(case(n)["text"]) for n in names}
    src = (JS_RUN.replace("@@CLIENT@@", client).replace("@@ANSWERS@@", json.dumps(answers))
           .replace("@@CASES@@", "\n".join(blocks)))
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "tabs.mjs")
        with open(path, "w", encoding="utf-8") as f:
            f.write(src)
        try:
            # A few hundred milliseconds; a tab that never settled is a failure.
            run = subprocess.run(["node", path], capture_output=True, text=True, timeout=120)
        except subprocess.TimeoutExpired:
            raise SystemExit("builder tabs: the JavaScript tabs ran past 120 s")
    if run.returncode != 0:
        raise SystemExit(f"builder tabs: the JavaScript tabs did not run:\n{run.stderr}")
    return json.loads(run.stdout)


def run_python(names):
    """Each Python tab as the page shows it: what it sent."""
    sys.path.insert(0, os.path.join(REPO, "integrations", "python"))
    import fenecdb

    class Recording(fenecdb.Client):
        def __init__(self):
            super().__init__("http://docs.invalid")
            self.sent = []

        def query(self, fenecql, params=None, **kw):
            self.sent.append({"text": fenecql, "params": json.loads(json.dumps(params or []))})
            return _answer(fenecql)

    helpers = {k: getattr(fenecdb, k) for k in
               ("or_", "and_", "not_", "raw", "inc", "expr", "bucket", "count_distinct", "distance", "first", "last")}
    out = {}
    for n in names:
        db = Recording()
        try:
            exec(compile(render(Python(), n), n, "exec"), {**helpers, "db": db})
            out[n] = db.sent
        except Exception as e:  # noqa: BLE001 -- a tab that fails says how
            out[n] = {"error": f"{type(e).__name__}: {e}"}
    return out


def check():
    selftest()
    if not used():
        raise SystemExit("builder tabs: no page shows one")
    names = tested()
    # The build's own check, every page's statements against their cases.
    for dirpath, _, files in os.walk(CONTENT):
        for f in files:
            if f.endswith(".html"):
                path = os.path.join(dirpath, f)
                expand(open(path, encoding="utf-8").read(), os.path.relpath(path, REPO))
    failed = []
    for lang, ran in (("JavaScript", run_js(names)), ("Python", run_python(names))):
        for n in names:
            c, got = case(n), ran.get(n)
            want = [{"text": c["text"], "params": c["params"]}]
            if not isinstance(got, list) or len(got) != 1 or got[0]["text"] != c["text"] \
                    or not _same(got[0]["params"], c["params"]):
                failed.append(f"{lang} tab of {n!r}: sent {json.dumps(got, ensure_ascii=False)}, "
                              f"want {json.dumps(want, ensure_ascii=False)}")
    stale = stale_tests()
    for rel in stale:
        failed.append(f"{rel} is not what the tabs make: make docs-builder")
    if failed:
        raise SystemExit("builder tabs:\n  " + "\n  ".join(failed))
    print(f"builder tabs: {len(used())} cases on the pages and {len(ESCAPES)} of escapes; the JavaScript and "
          "Python tabs ran and sent their statements; the Go, .NET, Swift, Kotlin and Dart tests are the tabs'")


if __name__ == "__main__":
    if "--write" in sys.argv:
        write_tests()
    elif "--check" in sys.argv:
        check()
    else:
        raise SystemExit(__doc__)
