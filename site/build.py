#!/usr/bin/env python3
"""Static site generator for fenecdb.

Standard library only, on purpose: the repo's hard rule is zero dependencies,
and the site is the first thing a reader sees. A generator exists at all
because the page chrome (nav, sidebar, footer) would otherwise be duplicated
across a dozen files and drift.

    python3 site/build.py            # -> site/dist
    python3 site/build.py --serve    # build, then serve on :8788
"""

import gzip as gziplib
import hashlib
import html
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(ROOT)
OUT = os.path.join(ROOT, "dist")

REPO_URL = "https://github.com/fenecdb/fenec"

# esbuild, when it happens to be on PATH. The generator itself stays
# stdlib-only -- the repo's zero-dependency rule -- so a missing esbuild is a
# note, not an error, and the readable source ships instead. It is worth
# wiring up: the four JS/CSS assets are 38 KB gzipped as written and 23 KB
# minified, and every page waits on them.
ESBUILD = shutil.which("esbuild")
_ESBUILD_NOTED = []


def minify(body, ext):
    if ESBUILD is None:
        if not _ESBUILD_NOTED:
            print("  note: esbuild not on PATH -- shipping JS/CSS unminified")
            _ESBUILD_NOTED.append(True)
        return body
    loader = "js" if ext == ".js" else "css"
    args = [ESBUILD, f"--loader={loader}", "--minify"]
    if loader == "js":
        # Everything here is an ES module; without this esbuild is free to
        # pick a different module syntax for the output.
        args += ["--format=esm", "--target=es2022"]
    run = subprocess.run(args, input=body, capture_output=True, text=True)
    if run.returncode != 0:
        raise SystemExit(f"esbuild failed on a {loader} asset:\n{run.stderr}")
    return run.stdout

# Ordered docs navigation. Groups are rendered as the sidebar sections.
NAV = [
    ("Start", [
        ("docs/index", "Overview"),
        ("docs/quickstart", "Quickstart"),
        ("docs/concepts", "How it works"),
    ]),
    ("Query", [
        ("docs/fenecql", "FenecQL"),
        ("docs/javascript", "JavaScript"),
        ("docs/http", "HTTP endpoint"),
        ("docs/sync", "Sync"),
    ]),
    ("Operate", [
        ("docs/postgres", "PostgreSQL server"),
        ("docs/replication", "Replication"),
        ("docs/sharding", "Tenants and sharding"),
        ("docs/import", "Import"),
        ("docs/embedding", "Embedded Rust"),
    ]),
    ("Reference", [
        ("docs/benchmarks", "Benchmarks"),
        ("docs/file-format", "File format"),
        ("docs/limits", "Limits"),
    ]),
]

# ---------------------------------------------------------------- highlighting

KEYWORDS = {
    "fenecql": """create drop collection index if not exists get put set del select
        from where near order limit offset count ef exact asc desc and or in has is
        null true false collections describe compact begin commit on""".split(),
    "js": """import export from const let var async await function return new class
        extends if else for of while try catch finally throw typeof null undefined
        true false this default""".split(),
    "rust": """use pub fn let mut struct enum impl trait for in if else match return
        loop while const static crate mod self Some None Ok Err true false as where
        dyn ref move unsafe""".split(),
    "bash": """cd make cargo curl echo npx python3 docker export sudo cp mv rm set
        if then fi for do done""".split(),
    "json": "true false null".split(),
}

TYPES = """bool int float text bytes timestamp vector f16 cosine l2 dot
    Fenec FenecSync Database Collection Value Error String Vec Option Result""".split()

COMMENT = {
    "fenecql": r"--[^\n]*",
    "js": r"//[^\n]*|/\*[\s\S]*?\*/",
    "rust": r"//[^\n]*|/\*[\s\S]*?\*/",
    "bash": r"#[^\n]*",
    "http": r"#[^\n]*",
    "text": None,
    "json": None,
}


def highlight(code, lang):
    """Tokenise `code` and return escaped HTML with <span class=t-*> wrappers.

    Deliberately small: a full parser per language would be a dependency in
    everything but name, and the pages only ever show short excerpts.
    """
    if lang in ("text", "", None):
        return html.escape(code)

    kw = set(KEYWORDS.get(lang, []))
    parts = []
    comment = COMMENT.get(lang)
    if comment:
        parts.append(f"(?P<comment>{comment})")
    parts.append(r"(?P<string>\"(?:[^\"\\\n]|\\.)*\"|'(?:[^'\\\n]|\\.)*')")
    parts.append(r"(?P<param>\$\d+)")
    parts.append(r"(?P<anno>@[A-Za-z_][\w]*)")
    parts.append(r"(?P<word>[A-Za-z_][\w]*)")
    parts.append(r"(?P<num>\b\d[\d_]*(?:\.\d+)?\b)")
    pattern = re.compile("|".join(parts))

    out, pos = [], 0
    for m in pattern.finditer(code):
        out.append(html.escape(code[pos:m.start()]))
        pos = m.end()
        kind = m.lastgroup
        raw = html.escape(m.group())
        if kind == "word":
            low = m.group()
            if low in kw:
                out.append(f'<span class="t-kw">{raw}</span>')
            elif low in TYPES:
                out.append(f'<span class="t-type">{raw}</span>')
            else:
                out.append(raw)
        else:
            out.append(f'<span class="t-{kind}">{raw}</span>')
    out.append(html.escape(code[pos:]))
    return "".join(out)


PRE_RE = re.compile(r'<pre(?P<attrs>[^>]*)>(?P<body>[\s\S]*?)</pre>')


def render_code_blocks(body):
    def sub(m):
        attrs = m.group("attrs")
        lang = re.search(r'data-lang="([^"]*)"', attrs)
        lang = lang.group(1) if lang else "text"
        label = re.search(r'data-label="([^"]*)"', attrs)
        # Code may be written with raw `<` or with entities; normalise both to
        # raw text here so highlight() does the only escaping that happens.
        code = html.unescape(m.group("body"))
        if code.startswith("\n"):
            code = code[1:]
        code = code.rstrip("\n ")
        # `data-bare` highlights in place, for type set as a design element
        # rather than as a code block (the home page headline).
        if "data-bare" in attrs:
            keep = re.sub(r'\s*data-(lang|bare)(="[^"]*")?', "", attrs)
            return f"<pre{keep}>{highlight(code, lang)}</pre>"
        head = ""
        if label:
            head = f'<div class="code-label">{html.escape(label.group(1))}</div>'
        return (f'<div class="code" data-lang="{html.escape(lang)}">{head}'
                f'<pre><code>{highlight(code, lang)}</code></pre></div>')
    return PRE_RE.sub(sub, body)


# ------------------------------------------------------------------ structure

SLUG_STRIP = re.compile(r"[^a-z0-9]+")
HEAD_RE = re.compile(r"<h(?P<lvl>[23])(?P<attrs>[^>]*)>(?P<text>[\s\S]*?)</h(?P=lvl)>")


def slug(text):
    text = re.sub(r"<[^>]+>", "", text)
    text = html.unescape(text).lower()
    return SLUG_STRIP.sub("-", text).strip("-") or "section"


def headings(body):
    """Give every h2/h3 a stable id and collect them for the table of contents."""
    toc, seen = [], {}
    def sub(m):
        lvl, attrs, text = m.group("lvl"), m.group("attrs"), m.group("text")
        found = re.search(r'id="([^"]*)"', attrs)
        if found:
            hid = found.group(1)
        else:
            hid = slug(text)
            n = seen.get(hid, 0)
            seen[hid] = n + 1
            if n:
                hid = f"{hid}-{n+1}"
            attrs = f' id="{hid}"{attrs}'
        toc.append((int(lvl), hid, re.sub(r"<[^>]+>", "", text)))
        return (f'<h{lvl}{attrs}><a class="anchor" href="#{hid}" '
                f'aria-label="Link to this section"></a>{text}</h{lvl}>')
    return HEAD_RE.sub(sub, body), toc


# Cloudflare's static-asset router canonicalises to extensionless URLs, so a
# link written as `foo.html` is answered with a 307 to `foo`. Links are emitted
# without the suffix and the file on disk keeps it.
LINK_RE = re.compile(r'(href=")(?!https?:|mailto:|data:|#)([^"#]*?)\.html(#[^"]*)?(")')


def clean_links(html):
    def sub(m):
        open_, path, frag, close = m.group(1), m.group(2), m.group(3) or "", m.group(4)
        if path.endswith("index") or path == "index":
            path = path[: -len("index")]  # foo/index -> foo/ ; index -> ""
            if not path and not frag:
                path = "./"
        return f"{open_}{path}{frag}{close}"
    return LINK_RE.sub(sub, html)


META_RE = re.compile(r"^<!--\s*(?P<meta>[\s\S]*?)-->\s*")


def read_page(path):
    raw = open(path, encoding="utf-8").read()
    meta = {}
    m = META_RE.match(raw)
    if m:
        for line in m.group("meta").strip().splitlines():
            if ":" in line:
                k, v = line.split(":", 1)
                meta[k.strip()] = v.strip()
        raw = raw[m.end():]
    return meta, raw


def nav_html(active, base):
    rows = []
    for group, items in NAV:
        rows.append(f'<p class="side-group">{group}</p><ul class="side-list">')
        for key, label in items:
            href = f"{base}{key}.html"
            cls = ' class="here"' if key == active else ""
            rows.append(f'<li><a{cls} href="{href}">{label}</a></li>')
        rows.append("</ul>")
    return "".join(rows)


def toc_html(toc):
    if len(toc) < 3:
        return ""
    items = "".join(
        f'<li class="lvl{lvl}"><a href="#{hid}">{html.escape(text)}</a></li>'
        for lvl, hid, text in toc)
    return f'<nav class="toc" aria-label="On this page"><p>On this page</p><ul>{items}</ul></nav>'


def prev_next(active, base):
    flat = [(k, l) for _, items in NAV for k, l in items]
    keys = [k for k, _ in flat]
    if active not in keys:
        return ""
    i = keys.index(active)
    out = []
    for label, j in (("Previous", i - 1), ("Next", i + 1)):
        if not 0 <= j < len(flat):
            continue
        key, text = flat[j]
        href = f"{base}{key}.html"
        side = "prev" if label == "Previous" else "next"
        out.append(f'<a class="pn {side}" href="{href}"><span>{label}</span>{text}</a>')
    return f'<nav class="pagenav">{"".join(out)}</nav>' if out else ""


# Measured numbers end up copied into prose, and prose does not recompile. Every
# place one is written down is listed here, with the pattern that finds it: the
# module's size alone had drifted across eight files by 13 KB before this
# existed. `tol` is for the figures written as approximate ("~175 lines").
#
# A miss is a warning locally -- a rebuild that moves the module by forty bytes
# should not stop you working -- and an error under CI, which is the build that
# ships the number.
#
# Not everything measured is in here. The container image's size is written
# into four files and cannot be checked from a site build, which has neither a
# daemon nor the registry; it drifted from 1.25 MB to a claimed 1.55 before
# anyone noticed. `docker image inspect ghcr.io/fenecdb/fenec-pg:<v>` is the
# way to settle it by hand.
# Not in here, and deliberately: the `fenec` and `fenec-pg` binary sizes. They
# are quoted for an Apple M-series and CI is Linux, so a check would compare
# two different numbers and fail honest builds. They are re-measured by hand at
# each release, next to the version bump -- 0.1.4 moved them 636/717/863 KB ->
# 684/765/927 KB when the text index went in.
CLAIMS = [
    ("README.md", r"\*\*Runtime size\*\* \| (\d+) KB wasm", "kb", 0),
    ("README.md", r"fenec-pg:(\d+\.\d+\.\d+)", "version", 0),
    ("README.md", r"(\d+) KB of WebAssembly —", "kb", 0),
    ("site/content/index.html", r"compiles to (\d+) KB of WebAssembly", "kb", 0),
    ("site/content/index.html", r"(\d+) KB of WebAssembly with no", "kb", 0),
    ("site/content/index.html", r"WebAssembly output is (\d+) KB", "kb", 0),
    ("site/content/playground.html", r"the same (\d+) KB WebAssembly module", "kb", 0),
    ("site/content/docs/index.html", r"(\d+) KB wasm", "kb", 0),
    ("site/content/docs/index.html", r"(\d+) KB brotli, with the client", "kb_br_all", 0),
    ("site/content/docs/benchmarks.html",
     r'wasm32, browser</td><td class="n"><b>(\d+) KB</b>', "kb", 0),
    ("site/content/docs/benchmarks.html",
     r'<b>\d+ KB</b></td><td class="n"><b>(\d+) KB</b></td><td class="n">\d+ KB</td>',
     "kb_br", 0),
    ("site/content/docs/benchmarks.html",
     r'<b>\d+ KB</b></td><td class="n"><b>\d+ KB</b></td><td class="n">(\d+) KB</td>',
     "kb_gz", 0),
    ("site/content/docs/benchmarks.html",
     r'the client</td><td class="n">(\d+) KB</td>', "kb_client", 0),
    ("site/content/docs/benchmarks.html",
     r'the client</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_client_br", 0),
    ("site/content/docs/benchmarks.html",
     r"browser pays is\s*\n?\s*<b>(\d+) KB brotli</b>", "kb_br_all", 0),
    ("README.md", r"(\d+) KB brotli \(`-q 11`\) over the wire", "kb_br_all", 0),
    ("CLAUDE.md", r"WASM glue \(~(\d+) lines\)", "glue", 8),
    ("AGENTS.md", r"WASM glue \(~(\d+) lines\)", "glue", 8),
    ("site/content/docs/concepts.html", r"glue is about (\d+) lines", "glue", 8),
]


def glue_lines():
    """Lines in the `Fenec` class -- the wasm glue the docs put a number on."""
    src = open(os.path.join(REPO, "web", "fenec.js"), encoding="utf-8").read().splitlines()
    start = next(i for i, ln in enumerate(src) if ln.startswith("export class Fenec {"))
    end = next(i for i in range(start + 1, len(src)) if src[i] == "}")
    return end - start + 1


def workspace_version():
    """`version` under [workspace.package] in the root Cargo.toml."""
    body = open(os.path.join(REPO, "Cargo.toml"), encoding="utf-8").read()
    return re.search(r'^version = "([^"]+)"', body, re.MULTILINE).group(1)


def compressed(path):
    """`(gzip, brotli)` bytes for a file that is served over HTTP.

    What a reader downloads is the compressed form, so that is the number the
    site quotes -- and a quoted number has to be checkable or it rots, which
    is the whole point of this file. gzip comes from the standard library.
    Brotli does not exist there, so it comes from the `brotli` CLI; when that
    is missing the caller is told, and the brotli claims are reported as
    unverified rather than quietly passed.
    """
    raw = open(path, "rb").read()
    gz = len(gziplib.compress(raw, 9, mtime=0))
    exe = shutil.which("brotli")
    if not exe:
        return gz, None
    out = subprocess.run(
        [exe, "-c", "-q", "11"], input=raw, stdout=subprocess.PIPE, check=True
    ).stdout
    return gz, len(out)


def check_claims():
    """Compares every number in CLAIMS against the thing it describes."""
    wasm = os.path.join(REPO, "web", "fenec.wasm")
    client = os.path.join(REPO, "web", "fenec.js")
    if not os.path.exists(wasm):
        return []  # the copy step above already said so
    size = os.path.getsize(wasm)
    wasm_gz, wasm_br = compressed(wasm)
    client_size = os.path.getsize(client)
    client_gz, client_br = compressed(client)
    truth = {
        "bytes": size,
        "kb": round(size / 1024),
        "kb_gz": round(wasm_gz / 1024),
        "kb_br": round(wasm_br / 1024) if wasm_br else None,
        "kb_client": round(client_size / 1024),
        "kb_client_gz": round(client_gz / 1024),
        "kb_client_br": round(client_br / 1024) if client_br else None,
        # What the browser actually pays: the module and the client together.
        "kb_br_all": round((wasm_br + client_br) / 1024) if wasm_br else None,
        "glue": glue_lines(),
        # The tag the docs tell people to pull. It follows the workspace
        # version rather than the last release, so a version bump that
        # forgets the README is caught at the bump rather than after it
        # has shipped.
        "version": workspace_version(),
    }
    unit = {"glue": " lines", "version": "", "bytes": " bytes"}
    # Every size fact is in KB; without a default the report of a drifted
    # size died on a KeyError instead of saying which file drifted.
    unit = {**{k: " KB" for k in truth}, **unit}

    problems = []
    for rel, pattern, fact, tol in CLAIMS:
        path = os.path.join(REPO, rel)
        if not os.path.exists(path):
            continue  # AGENTS.md is optional
        body = open(path, encoding="utf-8").read()
        found = list(re.finditer(pattern, body, re.MULTILINE))
        if not found:
            problems.append(f"{rel}: nothing matched /{pattern}/ -- reworded?")
            continue
        if truth.get(fact) is None:
            problems.append(
                f"{rel}: /{pattern}/ claims a brotli size and `brotli` is not "
                f"installed, so it went unchecked"
            )
            continue
        for m in found:
            said, want = m.group(1), truth[fact]
            if fact == "version":
                drifted = said != want
            else:
                said, drifted = int(said), abs(int(said) - want) > tol
            if drifted:
                line = body.count("\n", 0, m.start()) + 1
                problems.append(
                    f"{rel}:{line}: says {said}{unit[fact]}, "
                    f"it is {want}{unit[fact]}"
                )
    return problems


# ------------------------------------------------------------------ llms.txt
#
# Most new databases are now created by coding agents, and no model has seen
# FenecQL: left alone, an agent writes SQL at it. /llms.txt is the short brief
# an agent reads first (https://llmstxt.org), /llms-full.txt the whole
# reference as one text file. Both are rendered from the pages on every build,
# so they cannot drift from the docs the way a hand-kept copy would.

SITE = "https://fenecdb.com"

LLMS_BRIEF = """\
# fenecdb

> Minimal, vector-native embedded database: one file, HNSW, BM25 and hash
> indexes, runs in the browser as WebAssembly and speaks the PostgreSQL wire
> protocol. Its query language is FenecQL, which is not SQL.

Writing FenecQL -- the reference below spells it out in full:

- It is not SQL. There is no JOIN, subquery, GROUP BY, sum/avg or
  transaction; `count` is the one aggregate, and related rows come from
  `lookup`.
- Square brackets are list literals and nothing else: `tags: ["a", "b"]`,
  `tags [text]`. Nothing here marks an optional part with them; a clause you
  do not need is simply left out.
- Strings take double quotes. Parameters are `$1`, `$2`, ...; a vector
  parameter is a list of numbers. A timestamp is written as text,
  `"2026-09-01T10:00:00Z"`, or `"2026-01-01"` in a comparison.
- Types: `bool int float text bytes timestamp vector<N> vector<N, f16>` and
  lists such as `[text]`. There is no decimal -- money is an `int` of cents --
  no UUID type (use `text @hash`) and no nested objects.
- Indexes: `@hash` for equality, `@sorted` for ranges and for `order` with a
  `limit`, `@hnsw(cosine)` (or `l2`, `dot`) for `near`, `@text` for `match`.
  Only an `and` chain uses an index: `=` and `in [...]` on a `@hash` field or
  on `id`, `<`, `<=`, `>`, `>=` and `=` on a `@sorted` field; everything else
  scans. `explain get ...` runs a query and returns the path it took.
- `near` and `match` decide the order: neither combines with `order` or with
  the other, and each returns at most 10 000 rows (`limit + offset`). Both add
  a `_score` column.
- After `lookup`, every clause belongs to the child collection and `limit`
  counts children per parent. `required` goes right after `on <field>` and
  keeps only the parents with a matching child; `count` goes before `lookup`.

Every statement, by example:

```fenecql
create collection articles (title text @hash, views int, tags [text], published timestamp @sorted, embed vector<4> @hnsw(cosine), body text @text)
put articles {title: "Rust", views: 10, tags: ["lang"], published: "2026-09-01T10:00:00Z", embed: [0.1, 0.2, 0.3, 0.4], body: "Ownership and borrowing"}
put articles [{title: "Zig", views: 3}, {title: "Go", views: 7}]
get articles select title, views where views < 100 and tags has "lang" order views desc limit 5 offset 5
get articles where title in ["Rust", "Go"] count
get articles select title where published >= "2026-01-01" near embed $1 limit 3
get articles match body "borrowing" rerank embed $1 candidates 200 limit 10
get articles order published desc limit 20 lookup comments on article_id where score >= 4 order published desc limit 3
get articles count lookup comments on article_id required where score = 5
explain get articles where views < 100 order published desc limit 5
set articles {views: 11} where id = 7
del articles where views > 100000
create index on articles (views) @hash
drop collection articles
```

From JavaScript, `db.from('articles').where('views', '<', 100).near('embed', v)
.limit(10).rows()` builds the same FenecQL with every value a parameter.
"""


def page_text(body):
    """A docs page as plain Markdown-ish text: headings, code, tables, lists."""
    # A code block may be written with entities or with a raw `<`; both are
    # escaped here and unescaped only by the last pass, or `<name>` and
    # `vector<384>` would read as tags to the pass that strips them.
    def code(m):
        lang = m.group(1) or ""
        src = html.escape(html.unescape(m.group(2)), quote=False)
        return f"\n```{lang}\n{src.strip()}\n```\n"

    def row(m):
        cells = re.findall(r"<t[hd][^>]*>([\s\S]*?)</t[hd]>", m.group(1))
        return "| " + " | ".join(re.sub(r"\s+", " ", c).strip() for c in cells) + " |\n"

    def link(m):
        href, text = m.group(1), m.group(2)
        if not re.match(r"https?:|mailto:|#", href):
            href = f"{SITE}/docs/" + re.sub(r"\.html(?=#|$)", "", href)
        return f"[{text}]({href})"

    t = re.sub(r"<!--[\s\S]*?-->", "", body)
    t = re.sub(r'<pre(?: data-lang="([^"]*)")?>([\s\S]*?)</pre>', code, t)
    t = re.sub(r"<tr[^>]*>([\s\S]*?)</tr>", row, t)
    t = re.sub(r"<h1[^>]*>([\s\S]*?)</h1>", r"\n# \1\n", t)
    t = re.sub(r"<h2[^>]*>([\s\S]*?)</h2>", r"\n## \1\n", t)
    t = re.sub(r"<h3[^>]*>([\s\S]*?)</h3>", r"\n### \1\n", t)
    t = re.sub(r"<li[^>]*>", "\n- ", t)
    t = re.sub(r"<code>([\s\S]*?)</code>", r"`\1`", t)
    t = re.sub(r"<b>([\s\S]*?)</b>", r"**\1**", t)
    t = re.sub(r'<a [^>]*?href="([^"]*)"[^>]*>([\s\S]*?)</a>', link, t)
    t = re.sub(r"</?(p|div|ul|ol|table|thead|tbody|br)[^>]*>", "\n", t)
    t = html.unescape(re.sub(r"<[^>]+>", "", t))
    t = re.sub(r"[ \t]+\n", "\n", t)
    return re.sub(r"\n{3,}", "\n\n", t).strip() + "\n"


def llms_texts():
    """(`llms.txt`, `llms-full.txt`) from the docs pages, in navigation order."""
    index = [LLMS_BRIEF, "\n## Docs\n\n"]
    full = [LLMS_BRIEF]
    for _, items in NAV:
        for key, label in items:
            meta, body = read_page(os.path.join(ROOT, "content", key + ".html"))
            url = f"{SITE}/{key}".replace("/docs/index", "/docs/")
            index.append(f"- [{label}]({url}): {meta.get('description', '')}\n")
            full.append(f"\n\n---\n\nSource: {url}\n\n{page_text(body)}")
    index.append(f"\n## Optional\n\n- [Every page above as one text file]({SITE}/llms-full.txt)\n")
    return "".join(index), "".join(full)


def build():
    if os.path.isdir(OUT):
        shutil.rmtree(OUT)
    os.makedirs(os.path.join(OUT, "docs"), exist_ok=True)

    template = open(os.path.join(ROOT, "template.html"), encoding="utf-8").read()

    # Content-hashed asset names. Without them a deploy serves new HTML beside
    # whatever CSS and JS the visitor already had cached — not merely stale but
    # broken, since the two no longer agree. Hashed names make a deploy atomic
    # and let the files be cached forever.
    assets = {}

    def emit(name, body):
        stem, ext = os.path.splitext(name)
        if ext in (".js", ".css"):
            body = minify(body, ext)
        digest = hashlib.sha256(body.encode("utf-8")).hexdigest()[:10]
        out_name = f"{stem}.{digest}{ext}"
        open(os.path.join(OUT, out_name), "w", encoding="utf-8").write(body)
        assets[name] = out_name
        return out_name

    # The engine ships twice. The stable names are what the docs tell people
    # to import, so they have to keep resolving; the hashed copies are what
    # the site itself loads, and those can be cached forever. Only the hashed
    # pair is minified -- whoever follows a link from the docs gets readable
    # source. They are deliberately kept out of `assets`: that dict drives a
    # page-wide replace and the docs are full of `./fenec.js` inside code
    # examples, which must not be rewritten.
    engine = {}
    for name in ("fenec.js", "fenec.wasm"):
        src = os.path.join(REPO, "web", name)
        if not os.path.exists(src):
            print(f"  note: web/{name} missing -- run `make wasm` for the live demo")
            continue
        shutil.copy(src, os.path.join(OUT, name))
        if name.endswith(".js"):
            blob = minify(open(src, encoding="utf-8").read(), ".js").encode("utf-8")
        else:
            blob = open(src, "rb").read()
        stem, ext = os.path.splitext(name)
        hashed = f"{stem}.{hashlib.sha256(blob).hexdigest()[:10]}{ext}"
        open(os.path.join(OUT, hashed), "wb").write(blob)
        engine[name] = hashed

    worker = open(os.path.join(ROOT, "engine-worker.js"), encoding="utf-8").read()
    # Point the worker at the immutable copies. The exact quoted paths are
    # matched, so the `booting fenec.wasm` log line is left alone.
    if "fenec.js" in engine:
        worker = worker.replace("from './fenec.js'", f"from './{engine['fenec.js']}'")
    if "fenec.wasm" in engine:
        worker = worker.replace("Fenec.open('./fenec.wasm')",
                                f"Fenec.open('./{engine['fenec.wasm']}')")
    worker_name = emit("engine-worker.js", worker)

    script = open(os.path.join(ROOT, "site.js"), encoding="utf-8").read()
    script = script.replace("./engine-worker.js", "./" + worker_name)
    emit("site.js", script)

    emit("styles.css", open(os.path.join(ROOT, "styles.css"), encoding="utf-8").read())

    pages = []
    for dirpath, _, files in os.walk(os.path.join(ROOT, "content")):
        for name in sorted(files):
            if name.endswith(".html"):
                pages.append(os.path.join(dirpath, name))

    for path in sorted(pages):
        rel = os.path.relpath(path, os.path.join(ROOT, "content"))
        key = rel[:-5].replace(os.sep, "/")
        depth = key.count("/")
        base = "../" * depth or "./"
        meta, body = read_page(path)
        body = render_code_blocks(body)
        body, toc = headings(body)

        is_docs = key.startswith("docs/")
        page = template
        page = page.replace("{{lang_class}}", "docs" if is_docs else "home")
        page = page.replace("{{title}}", html.escape(meta.get("title", "fenecdb")))
        page = page.replace("{{description}}", html.escape(meta.get("description", "")))
        page = page.replace("{{base}}", base)
        page = page.replace("{{repo}}", REPO_URL)
        page = page.replace("{{nav_docs}}", ' aria-current="page"' if is_docs else "")
        if is_docs:
            # The dune field carries over the top of every docs page, so the
            # reference does not read as a different site from the front door.
            scarp = ('<div class="hero-scarp" aria-hidden="true">'
                     '<svg viewBox="0 0 1440 120" preserveAspectRatio="none">'
                     '<path d="M0 78 C 220 44 340 96 520 70 C 720 40 840 92 1030 64 '
                     'C 1200 40 1320 80 1440 58 L1440 120 L0 120 Z" fill="#1B1233"/>'
                     '<path d="M0 96 C 240 70 360 112 560 92 C 760 72 880 110 1060 88 '
                     'C 1230 68 1330 100 1440 84 L1440 120 L0 120 Z" fill="#150F26"/>'
                     '</svg></div>')
            shell = (f'{scarp}<div class="shell"><aside class="side" id="side">'
                     f'<div class="side-inner">{nav_html(key, base)}</div></aside>'
                     f'<main class="doc" id="content"><article>{body}'
                     f'{prev_next(key, base)}</article></main>'
                     f'{toc_html(toc)}</div>')
        else:
            shell = f'<main id="content">{body}</main>'
        page = page.replace("{{content}}", shell)

        for plain, hashed in assets.items():
            page = page.replace(plain, hashed)
        page = clean_links(page)

        dest = os.path.join(OUT, key + ".html")
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        open(dest, "w", encoding="utf-8").write(page)

    # Cloudflare reads this from the asset directory; it is not served itself.
    # Hashed assets can be cached forever because a change gives a new name --
    # that now includes the engine the live console and the playground load.
    # The stable `fenec.js` / `fenec.wasm` names exist for the docs links, and
    # those revalidate: a stale engine would silently be the wrong one.
    rules = ["/*",
             "  X-Content-Type-Options: nosniff",
             "  Referrer-Policy: strict-origin-when-cross-origin",
             "  X-Frame-Options: DENY",
             ""]
    for hashed in sorted(list(assets.values()) + list(engine.values())):
        rules += [f"/{hashed}", "  Cache-Control: public, max-age=31536000, immutable", ""]
    for stable in ("fenec.js", "fenec.wasm"):
        rules += [f"/{stable}", "  Cache-Control: public, max-age=3600, must-revalidate", ""]
    open(os.path.join(OUT, "_headers"), "w", encoding="utf-8").write("\n".join(rules))

    brief, full = llms_texts()
    open(os.path.join(OUT, "llms.txt"), "w", encoding="utf-8").write(brief)
    open(os.path.join(OUT, "llms-full.txt"), "w", encoding="utf-8").write(full)

    print(f"built {len(pages)} pages -> {os.path.relpath(OUT, REPO)}")

    problems = check_claims()
    if problems:
        print("\n  the docs disagree with what was just built:")
        for p in problems:
            print(f"    {p}")
        if os.environ.get("CI"):
            raise SystemExit("\n  refusing to ship stale numbers (CI)")
        print("  (a warning here, an error under CI)")


if __name__ == "__main__":
    build()
    if "--serve" in sys.argv:
        import http.server, socketserver, functools
        port = 8788
        for i, a in enumerate(sys.argv):
            if a == "--port" and i + 1 < len(sys.argv):
                port = int(sys.argv[i + 1])
        class Handler(http.server.SimpleHTTPRequestHandler):
            """Resolve `/docs/fenecql` to `docs/fenecql.html`, the way the
            Cloudflare asset router does, so preview and production agree."""

            def translate_path(self, path):
                full = super().translate_path(path)
                if not os.path.exists(full) and not path.endswith("/"):
                    if os.path.exists(full + ".html"):
                        return full + ".html"
                return full

        handler = functools.partial(Handler, directory=OUT)
        socketserver.TCPServer.allow_reuse_address = True
        with socketserver.TCPServer(("", port), handler) as httpd:
            print(f"serving http://localhost:{port}")
            httpd.serve_forever()
