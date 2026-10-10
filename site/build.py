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
import html.parser
import json
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
        ("docs/languages", "Your language"),
    ]),
    ("Query", [
        ("docs/fenecql", "FenecQL"),
        ("docs/javascript", "JavaScript and TypeScript"),
        ("docs/mobile", "Mobile and native apps"),
        ("docs/http", "HTTP endpoint"),
        ("docs/sync", "Sync"),
        ("docs/redis", "Instead of Redis"),
        ("docs/analytics", "Analytics and market data"),
        ("docs/integrations", "Integrations"),
    ]),
    ("Operate", [
        ("docs/server", "Server"),
        ("docs/replication", "Replication"),
        ("docs/monitoring", "Monitoring"),
        ("docs/datadog", "Datadog and others"),
        ("docs/studio", "Studio"),
        ("docs/sharding", "Tenants and sharding"),
        ("docs/serverless", "Serverless and Cloudflare"),
        ("docs/microservices", "Services and events"),
        ("docs/import", "Import"),
        ("docs/embedding", "Embedded Rust"),
    ]),
    ("Compare", [
        ("docs/compare", "Overview"),
        ("docs/vs-postgres", "PostgreSQL + pgvector"),
        ("docs/vs-sqlite", "SQLite"),
        ("docs/vs-pglite", "PGlite"),
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
        null true false collections describe compact begin commit on match fuse
        lookup group insert set del rerank absent""".split(),
    "js": """import export from const let var async await function return new class
        extends if else for of while try catch finally throw typeof null undefined
        true false this default""".split(),
    # TypeScript is JavaScript's words and its own for types.
    "ts": """import export from const let var async await function return new class
        extends if else for of while try catch finally throw typeof null undefined
        true false this default type interface declare as satisfies keyof
        readonly implements private public protected""".split(),
    "rust": """use pub fn let mut struct enum impl trait for in if else match return
        loop while const static crate mod self Some None Ok Err true false as where
        dyn ref move unsafe""".split(),
    "bash": """cd make cargo curl echo npx python3 docker export sudo cp mv rm set
        if then fi for do done""".split(),
    "json": "true false null".split(),
    "python": """import from as def return if else for in while with try except
        True False None and or not lambda class await async""".split(),
    "go": """package import func return if else for range var const defer go
        nil true false type struct map""".split(),
    "sql": """INSTALL LOAD ATTACH AS SELECT FROM WHERE GROUP BY COPY TO TYPE
        FORMAT count avg sum""".split(),
    "swift": """import let var func return if else for in while do try await async
        throws catch struct class enum init self some any guard case switch
        nil true false as is static private public""".split(),
    "kotlin": """import package val var fun return if else for in while try catch
        class object companion override suspend null true false is as by
        when this private""".split(),
    "dart": """import final const var return if else for in while try on catch
        async await class extends super this null true false void required
        late static""".split(),
    # The home page's sessions in the languages the docs show as text.
    "csharp": """using var new await async return if else for foreach in while try
        catch throw class static public private void null true false""".split(),
    "java": """static final var new return if else for while try catch throw throws
        class public private void null true false""".split(),
    "php": """function return if else foreach as new throw array null true
        false""".split(),
    "ruby": """def end return if else elsif unless do require nil true false
        raise""".split(),
}

TYPES = """bool int float text bytes timestamp vector f16 cosine l2 dot
    Fenec FenecSync Database Collection Value Error String Vec Option Result""".split()

COMMENT = {
    "fenecql": r"--[^\n]*",
    "js": r"//[^\n]*|/\*[\s\S]*?\*/",
    "ts": r"//[^\n]*|/\*[\s\S]*?\*/",
    "rust": r"//[^\n]*|/\*[\s\S]*?\*/",
    "bash": r"#[^\n]*",
    "http": r"#[^\n]*",
    "text": None,
    "json": None,
    "python": r"#[^\n]*",
    "go": r"//[^\n]*",
    "sql": r"--[^\n]*",
    "swift": r"//[^\n]*|/\*[\s\S]*?\*/",
    "kotlin": r"//[^\n]*|/\*[\s\S]*?\*/",
    "dart": r"//[^\n]*|/\*[\s\S]*?\*/",
    "csharp": r"//[^\n]*|/\*[\s\S]*?\*/",
    "java": r"//[^\n]*|/\*[\s\S]*?\*/",
    "php": r"//[^\n]*|/\*[\s\S]*?\*/",
    "ruby": r"#[^\n]*",
}


def token_pattern(lang):
    """The one alternation a language is tokenised by, in the syntax Python
    and JavaScript share and ASCII-only (`re.ASCII` here, a JS regex without
    the `u` flag there), so the playground's highlighter (`highlight_js`)
    compiles this same text and splits a statement where the docs do."""
    parts = []
    comment = COMMENT.get(lang)
    if comment:
        parts.append(f"(?P<comment>{comment})")
    parts.append(r"(?P<string>\"(?:[^\"\\\n]|\\.)*\"|'(?:[^'\\\n]|\\.)*')")
    parts.append(r"(?P<param>\$\d+)")
    parts.append(r"(?P<anno>@[A-Za-z_][\w]*)")
    parts.append(r"(?P<word>[A-Za-z_][\w]*)")
    parts.append(r"(?P<num>\b\d[\d_]*(?:\.\d+)?\b)")
    return "|".join(parts)


def highlight(code, lang):
    """Tokenise `code` and return escaped HTML with <span class=t-*> wrappers.

    Deliberately small: a full parser per language would be a dependency in
    everything but name, and the pages only ever show short excerpts.
    """
    if lang in ("text", "", None):
        return html.escape(code)

    kw = set(KEYWORDS.get(lang, []))
    pattern = re.compile(token_pattern(lang), re.ASCII)

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


# The languages a page highlights as they are typed: the playground's. Every
# other code on the site is highlighted above, as it is built.
JS_LANGS = ("fenecql",)


def highlight_js(langs=JS_LANGS):
    """`highlight.js` with the rules above written into it, so the editor and
    the docs cannot colour a statement two ways; `site/test_highlight.py`
    holds the two to the same HTML over sample statements."""
    rules = {"types": TYPES, "langs": {
        lang: {"pattern": token_pattern(lang).replace("(?P<", "(?<"),
               "kw": KEYWORDS.get(lang, [])} for lang in langs}}
    src = open(os.path.join(ROOT, "highlight.js"), encoding="utf-8").read()
    marker = "/* rules: build.py */ null"
    if marker not in src:
        raise SystemExit("site/highlight.js lost its rules marker")
    return src.replace(marker, json.dumps(rules, separators=(",", ":")))


PRE_RE =re.compile(r'<pre(?P<attrs>[^>]*)>(?P<body>[\s\S]*?)</pre>')


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
# ships the number. A size is held to its claim to the half KB either way, and
# a compressed one with NOISE_KB more: gzip and brotli move by up to 0.3 KB
# with nothing but the layout of the bytes changed -- two lines of comment at
# the top of engine.rs moved brotli 46 bytes, through the line numbers the
# panics carry -- and held to the whole KB alone, noise that carried the
# module across a half would fail CI with nothing wrong in the docs.
#
# Not everything measured is in here. The container image's size is written
# into four files and cannot be checked from a site build, which has neither a
# daemon nor the registry; it drifted from 1.25 MB to a claimed 1.55 before
# anyone noticed. `docker image inspect ghcr.io/fenecdb/fenec-server:<v>` is the
# way to settle it by hand.
# Not in here, and deliberately: the `fenec` and `fenec-server` binary sizes. They
# are quoted for an Apple M-series and CI is Linux, so a check would compare
# two different numbers and fail honest builds. They are re-measured by hand at
# each release, next to the version bump -- 0.1.4 moved them 636/717/863 KB ->
# 684/765/927 KB when the text index went in.
NOISE_KB = 0.3
COMPRESSED = {"kb_gz", "kb_br", "kb_client_gz", "kb_client_br", "kb_br_all",
              "kb_lite_gz", "kb_lite_br",
              "kb_app_br", "kb_app_client_br", "kb_studio_gz"}
# The browser client's modules, each after those it imports: what
# `@fenecdb/web` ships of JavaScript. client.js is `@fenecdb/web/client`,
# builder.js and http.js without the engine.
CLIENT_MODULES = ("builder.js", "http.js", "client.js", "fenec.js")
# An app that runs its queries on a server, bundled as an app is (esbuild,
# minified): `connect`, a builder query and its rows, through the package
# and through its client entry.
APP = (
    "import {{ connect }} from '{entry}';\n"
    "const db = connect('https://db.example.com', {{ token: 't' }});\n"
    "console.log(await db.from('docs').where('year', '>=', 2024).limit(10).rows());\n"
)
CLAIMS = [
    # fenec studio's first load, which studio/test/statements.test.mjs holds
    # under 120 KB.
    ("site/content/docs/studio.html", r"first load is (\d+) KB of JavaScript and CSS, gzipped", "kb_studio_gz", 0),
    ("README.md", r"\*\*Runtime size\*\* \| (\d+) KB gzip wasm", "kb_gz", 0),
    ("README.md", r"gzip wasm \+ (\d+) KB gzip client", "kb_client_gz", 0),
    ("README.md", r"fenec-server:(\d+\.\d+\.\d+)", "version", 0),
    ("README.md", r"(\d+) KB of gzipped WebAssembly and", "kb_gz", 0),
    ("README.md", r"as (\d+) KB of\s+gzipped WebAssembly", "kb_gz", 0),
    ("README.md", r"and a (\d+) KB gzipped client", "kb_client_gz", 0),
    # The site quotes what a visitor downloads: the module gzipped.
    ("site/content/index.html", r"compiles to (\d+) KB of gzipped WebAssembly", "kb_gz", 0),
    ("site/content/index.html", r"(\d+) KB of gzipped WebAssembly with no", "kb_gz", 0),
    ("site/content/playground.html", r"\((\d+) KB\s+gzipped\)", "kb_gz", 0),
    ("site/content/docs/index.html", r"(\d+) KB gzip<br><small>the wasm", "kb_gz", 0),
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
    # The comparison pages set the module beside another engine's files.
    ("site/content/docs/vs-sqlite.html",
     r'<code>fenec\.wasm</code> [\d.]+</td><td class="n"><b>(\d+) KB</b>', "kb_br", 0),
    ("site/content/docs/vs-sqlite.html",
     r'<code>fenec\.wasm</code> [\d.]+</td><td class="n"><b>\d+ KB</b></td><td class="n"><b>(\d+) KB</b>',
     "kb_gz", 0),
    ("site/content/docs/vs-sqlite.html",
     r"<tr><td>Browser module, brotli</td>\s*<td>(\d+) KB", "kb_br", 0),
    ("site/content/docs/vs-pglite.html",
     r'<code>fenec\.wasm</code> [\d.]+</td><td class="n"><b>(\d+) KB</b>', "kb_br", 0),
    ("site/content/docs/vs-pglite.html",
     r'<code>fenec\.wasm</code> [\d.]+</td><td class="n"><b>\d+ KB</b></td><td class="n"><b>(\d+) KB</b>',
     "kb_gz", 0),
    ("site/content/docs/vs-pglite.html",
     r"<tr><td>Download, brotli</td>\s*<td>(\d+) KB", "kb_br", 0),
    # The packages a page picks between (javascript.html#packages), and
    # wherever else their sizes are quoted.
    ("site/content/docs/javascript.html",
     r'<code>@fenecdb/web/client</code>: <code>connect</code>, no module</td><td class="n">(\d+) KB</td>',
     "kb_app_client_br", 0),
    ("site/content/docs/javascript.html",
     r"<code>sync\(\)</code> loads it too</td><td class=\"n\">(\d+) KB</td>", "kb_br", 0),
    ("site/content/docs/javascript.html",
     r"make wasm FEATURES=none SCHEMA=0   # nor the schema check: (\d+) KB", "kb_lite_br", 0),
    ("site/content/docs/javascript.html", r"modules it\s+imports, (\d+) KB brotli", "kb_client_br", 0),
    ("site/content/docs/javascript.html", r"bundles to\s+(\d+) KB brotli through it", "kb_app_client_br", 0),
    ("site/content/docs/javascript.html", r"and to (\d+) KB through <code>@fenecdb/web</code>", "kb_app_br", 0),
    ("site/content/docs/integrations.html", r"no module, (\d+) KB brotli of client", "kb_app_client_br", 0),
    ("site/content/docs/integrations.html", r"<td><code>fenec.wasm</code>, (\d+) KB brotli</td>", "kb_br", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>fenec.wasm</code></td><td class="n"><b>(\d+) KB</b></td>', "kb_br", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>fenec.wasm</code></td><td class="n"><b>\d+ KB</b></td><td class="n">(\d+) KB</td>', "kb_gz", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>fenec.js</code> and its modules</td><td class="n">(\d+) KB</td>', "kb_client_br", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>fenec.js</code> and its modules</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_client_gz", 0),
    ("site/content/docs/benchmarks.html",
     r"in an app's bundle, <code>@fenecdb/web/client</code></td><td class=\"n\">(\d+) KB</td>", "kb_app_client_br", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>make wasm FEATURES=none SCHEMA=0</code></td><td class="n">(\d+) KB</td>', "kb_lite", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>make wasm FEATURES=none SCHEMA=0</code></td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_lite_br", 0),
    ("site/content/docs/benchmarks.html",
     r'<code>make wasm FEATURES=none SCHEMA=0</code></td><td class="n">\d+ KB</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_lite_gz", 0),
    ("README.md", r"`@fenecdb/web/client`, (\d+) KB brotli\s+in its bundle", "kb_app_client_br", 0),
    ("web/README.md", r"`fenec.wasm` \|[^\n]*: (\d+) KB brotli", "kb_br", 0),
    ("web/README.md", r"`@fenecdb/web/client`, (\d+) KB brotli in an app's bundle", "kb_app_client_br", 0),
    ("CLAUDE.md", r"WASM glue \(~(\d+) lines\)", "glue", 8),
    ("AGENTS.md", r"WASM glue \(~(\d+) lines\)", "glue", 8),
    ("site/content/docs/concepts.html", r"glue is about (\d+) lines", "glue", 8),
    # YCSB, in thousands of operations a second (`ycsb_facts`).
    ("README.md", r"fenecdb does B with 16 threads at ([\d.]+) k operations", "ycsb:fenec:buffered:B:16", 0),
    ("README.md", r"second against SQLite's ([\d.]+) k, and C on one thread", "ycsb:sqlite:buffered:B:16", 0),
    ("README.md", r"and C on one thread at ([\d.]+) k against", "ycsb:fenec:buffered:C:1", 0),
    ("README.md", r"k against\s+([\d.]+) k, its file compacted", "ycsb:sqlite:buffered:C:1", 0),
    ("site/content/index.html", r"YCSB B over HTTP, 16 clients, durable</dt><dd><b>([\d.]+) k ops/s", "ycsb:server-docker:durable:B:16", 0),
    ("site/content/index.html", r"<span>PostgreSQL ([\d.]+) k, both in Docker", "ycsb:pg:durable:B:16", 0),
    ("site/content/docs/vs-sqlite.html", r"B, 95% reads, 16 threads, buffered</td><td class=\"n\"><b>([\d.]+) k", "ycsb:fenec:buffered:B:16", 0),
    ("site/content/docs/vs-sqlite.html", r"B, 95% reads, 16 threads, buffered</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:sqlite:buffered:B:16", 0),
    ("site/content/docs/vs-sqlite.html", r"A, 50% updates, 16 threads, durable</td><td class=\"n\"><b>([\d.]+) k", "ycsb:fenec:durable:A:16", 0),
    ("site/content/docs/vs-sqlite.html", r"A, 50% updates, 16 threads, durable</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:sqlite:durable:A:16", 0),
    ("site/content/docs/vs-sqlite.html", r"C, reads, one thread, after the updates</td><td class=\"n\"><b>([\d.]+) k", "ycsb:fenec:buffered:C:1", 0),
    ("site/content/docs/vs-sqlite.html", r"C, reads, one thread, after the updates</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:sqlite:buffered:C:1", 0),
    ("site/content/docs/vs-postgres.html", r"B, 95% reads, 16 clients</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:B:16", 0),
    ("site/content/docs/vs-postgres.html", r"B, 95% reads, 16 clients</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:B:16", 0),
    ("site/content/docs/vs-postgres.html", r"A, 50% updates, 16 clients</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:A:16", 0),
    ("site/content/docs/vs-postgres.html", r"A, 50% updates, 16 clients</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:A:16", 0),
    ("site/content/docs/vs-postgres.html", r"A, 50% updates, one client</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:A:1", 0),
    ("site/content/docs/vs-postgres.html", r"A, 50% updates, one client</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:A:1", 0),
    ("site/content/docs/vs-postgres.html", r"B, 95% reads, one client</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:B:1", 0),
    ("site/content/docs/vs-postgres.html", r"B, 95% reads, one client</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:B:1", 0),
    ("site/content/docs/vs-postgres.html", r"E, scans of 1 to 100 rows, one client</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:E:1", 0),
    ("site/content/docs/vs-postgres.html", r"E, scans of 1 to 100 rows, one client</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:E:1", 0),
    ("site/content/docs/vs-postgres.html", r"C, reads alone, one client, buffered</td><td class=\"n\">([\d.]+) k", "ycsb:server-docker:buffered:C:1", 0),
    ("site/content/docs/vs-postgres.html", r"C, reads alone, one client, buffered</td><td class=\"n\">[\d.]+ k</td><td class=\"n\"><b>([\d.]+) k", "ycsb:pg:buffered:C:1", 0),
    ("site/content/docs/vs-postgres.html", r"PostgreSQL's own B\s+doing ([\d.]+) k", "ycsb:pg:buffered:B:1", 0),
    ("site/content/docs/vs-postgres.html", r"beside its C's ([\d.]+) k", "ycsb:pg:buffered:C:1", 0),
    ("site/content/docs/vs-postgres.html", r"fenec-server leads C,\s+([\d.]+) k", "ycsb:server-docker:buffered:C:16", 0),
    ("site/content/docs/vs-postgres.html", r"fenec-server leads C,\s+[\d.]+ k against ([\d.]+) k", "ycsb:pg:buffered:C:16", 0),
    ("site/content/docs/benchmarks.html", r"buffered C ([\d.]+) k against [\d.]+ k and F", "ycsb:server-docker:buffered:C:1", 0),
    ("site/content/docs/benchmarks.html", r"buffered C [\d.]+ k against ([\d.]+) k and F", "ycsb:pg:buffered:C:1", 0),
    ("site/content/docs/benchmarks.html", r"and E went 1.03 k -> ([\d.]+) k buffered", "ycsb:server-docker:buffered:E:1", 0),
    ("site/content/docs/benchmarks.html", r"buffered against\s+([\d.]+) k, 1.06 k", "ycsb:pg:buffered:E:1", 0),
    ("site/content/docs/benchmarks.html", r"1.06 k -> ([\d.]+) k durable", "ycsb:server-docker:durable:E:1", 0),
    ("site/content/docs/benchmarks.html", r"durable against ([\d.]+) k\. Durable A", "ycsb:pg:durable:E:1", 0),
]


# A YCSB figure the site quotes is the median of the runs of its cell in the
# bench's results, which are committed with the pages that quote them: a
# number copied by hand from a terminal is how the sizes above drifted.
# Its fact is `ycsb:<system>:<mode>:<workload>:<threads>`, written in
# thousands of operations a second, held to the last digit it shows.
YCSB_RESULTS = os.path.join(REPO, "crates", "fenec-bench", "ycsb", "results.tsv")
# The runs a later one replaced, by run and system: their lines stay in the
# results, which are only appended to, and count in no median.
YCSB_SUPERSEDED = os.path.join(REPO, "crates", "fenec-bench", "ycsb", "superseded.tsv")


def ycsb_facts():
    """Each YCSB cell's median ops/s, as `ycsb report` takes it: the upper
    of the two middle runs when there are an even number."""
    if not os.path.exists(YCSB_RESULTS):
        return {}
    lines = open(YCSB_RESULTS, encoding="utf-8").read().splitlines()
    head = lines[0].split("\t")
    col = {name: i for i, name in enumerate(head)}
    # A run's cell measured again (a run cut short, completed later under
    # its own id) counts once, as its last line, as `ycsb report` takes it.
    superseded = set()
    if os.path.exists(YCSB_SUPERSEDED):
        for line in open(YCSB_SUPERSEDED, encoding="utf-8").read().splitlines()[1:]:
            run, system = line.split("\t")[:2]
            superseded.add((run, system))
    latest = {}
    for line in lines[1:]:
        f = line.split("\t")
        if f[col["workload"]] not in ("A", "B", "C", "D", "E", "F"):
            continue
        if (f[col["run"]], f[col["system"]]) in superseded:
            continue
        key = "ycsb:" + ":".join(f[col[c]] for c in ("system", "mode", "workload", "threads"))
        latest[(key, f[col["run"]])] = float(f[col["ops_s"]])
    runs = {}
    for (key, _), ops in latest.items():
        runs.setdefault(key, []).append(ops)
    return {k: sorted(v)[len(v) // 2] for k, v in runs.items()}


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
    return compressed_bytes(open(path, "rb").read())


def compressed_bytes(raw):
    """[`compressed`] of bytes in hand."""
    gz = len(gziplib.compress(raw, 9, mtime=0))
    exe = shutil.which("brotli")
    if not exe:
        return gz, None
    out = subprocess.run(
        [exe, "-c", "-q", "11"], input=raw, stdout=subprocess.PIPE, check=True
    ).stdout
    return gz, len(out)


def app_bundle(entry):
    """An app that connects to a server, bundled and minified by esbuild
    through `entry` (`APP`): its bytes, or None without esbuild -- the
    claims on it are then reported unverified, as brotli's are."""
    if ESBUILD is None:
        return None
    run = subprocess.run(
        [ESBUILD, "--bundle", "--minify", "--format=esm", "--target=es2022", "--log-level=error"],
        input=APP.format(entry=f"./{entry}"), capture_output=True, text=True,
        # The text on stdin resolves its imports from here.
        cwd=os.path.join(REPO, "web"),
    )
    if run.returncode != 0:
        raise SystemExit(f"esbuild failed on the app through {entry}:\n{run.stderr}")
    return run.stdout.encode("utf-8")


def studio_gz():
    """fenec studio's first load, in KB: its page, and each stylesheet and
    module the page names (`client.js`, `builder.js` and `http.js` are
    web/'s, which the server serves beside the studio's own), each gzipped
    as a proxy in front would send it."""
    page = open(os.path.join(REPO, "studio", "index.html"), encoding="utf-8").read()
    names = re.findall(r'<link rel="(?:stylesheet|modulepreload)" href="([^"]+)"', page)
    total = len(gziplib.compress(page.encode("utf-8"), 9, mtime=0))
    for name in names:
        where = "web" if name in CLIENT_MODULES else "studio"
        total += len(gziplib.compress(open(os.path.join(REPO, where, name), "rb").read(), 9, mtime=0))
    return total / 1024


def check_claims():
    """Compares every number in CLAIMS against the thing it describes."""
    web = lambda name: os.path.join(REPO, "web", name)
    wasm = web("fenec.wasm")
    if not os.path.exists(wasm):
        # The copy step above already said so. The bench's figures and the
        # studio's need no module, so they are still held to their truth.
        ycsb = ycsb_facts()
        unit = {**{k: " k ops/s" for k in ycsb}, "kb_studio_gz": " KB"}
        return claims_against({**ycsb, "kb_studio_gz": studio_gz()}, unit,
                              lambda f: f.startswith("ycsb:") or f == "kb_studio_gz")
    size = os.path.getsize(wasm)
    wasm_gz, wasm_br = compressed(wasm)
    # The client is fenec.js and the two modules it imports, as a page loads
    # them with no build step.
    client = b"".join(open(web(n), "rb").read() for n in CLIENT_MODULES if n != "client.js")
    client_gz, client_br = compressed_bytes(client)
    kb = lambda n: None if n is None else n / 1024
    # The module built without the indexes or the schema check (`make
    # wasm-lite`, a test build no package ships, whose size the docs quote
    # as a build option), where it was built: a claim on it when it is
    # missing says so.
    other = {}
    for key, name in (("lite", "fenec-lite.wasm"),):
        if os.path.exists(web(name)):
            gz, br = compressed(web(name))
            other[f"kb_{key}"] = kb(os.path.getsize(web(name)))
            other[f"kb_{key}_gz"] = kb(gz)
            other[f"kb_{key}_br"] = kb(br)
    apps = {}
    for key, entry in (("kb_app_br", "fenec.js"), ("kb_app_client_br", "client.js")):
        bundle = app_bundle(entry)
        apps[key] = None if bundle is None else kb(compressed_bytes(bundle)[1])
    truth = {
        "bytes": size,
        "kb": kb(size),
        "kb_gz": kb(wasm_gz),
        "kb_br": kb(wasm_br),
        "kb_client": kb(len(client)),
        "kb_client_gz": kb(client_gz),
        "kb_client_br": kb(client_br),
        # What the browser actually pays: the module and the client together.
        "kb_br_all": kb(wasm_br + client_br) if wasm_br else None,
        **other,
        **apps,
        "glue": glue_lines(),
        # The tag the docs tell people to pull. It follows the workspace
        # version rather than the last release, so a version bump that
        # forgets the README is caught at the bump rather than after it
        # has shipped.
        "version": workspace_version(),
        "kb_studio_gz": studio_gz(),
    }
    ycsb = ycsb_facts()
    unit = {"glue": " lines", "version": "", "bytes": " bytes"}
    # Every size fact is in KB; without a default the report of a drifted
    # size died on a KeyError instead of saying which file drifted.
    unit = {**{k: " KB" for k in truth}, **{k: " k ops/s" for k in ycsb}, **unit}
    return claims_against({**truth, **ycsb}, unit, lambda f: True)


# The names benchmarks.html#ycsb's full grid gives each system.
YCSB_NAMES = {"fenec": "fenecdb", "sqlite": "SQLite", "server": "fenec-server",
              "server-docker": "fenec-server in Docker", "pg": "PostgreSQL", "mongo": "MongoDB"}


def ycsb_grid_claims(truth):
    """A claim for each cell of benchmarks.html#ycsb's full grid: its row
    names the cell, so every one is held to the results, not a few."""
    out = []
    for fact in truth:
        if not fact.startswith("ycsb:"):
            continue
        _, system, mode, wl, threads = fact.split(":")
        out.append(("site/content/docs/benchmarks.html",
                    rf'<tr><td>{re.escape(YCSB_NAMES[system])}</td><td>{mode}</td><td>{wl}</td>'
                    rf'<td class="n">{threads}</td><td class="n">([\d.]+) k</td>', fact, 0))
    return out


def claims_against(truth, unit, wanted):
    """The CLAIMS whose fact `wanted` takes, each held to `truth`."""
    problems = []
    for rel, pattern, fact, tol in CLAIMS + ycsb_grid_claims(truth):
        if not wanted(fact):
            continue
        path = os.path.join(REPO, rel)
        if not os.path.exists(path):
            continue  # AGENTS.md is optional
        body = open(path, encoding="utf-8").read()
        found = list(re.finditer(pattern, body, re.MULTILINE))
        if not found:
            problems.append(f"{rel}: nothing matched /{pattern}/ -- reworded?")
            continue
        if truth.get(fact) is None:
            why = (
                "an app's bundle and `esbuild` is not on PATH" if fact.startswith("kb_app")
                else "a module `make wasm-lite` did not build" if fact not in truth
                else "a brotli size and `brotli` is not installed"
            )
            why = "a YCSB cell crates/fenec-bench/ycsb/results.tsv does not hold" if fact.startswith("ycsb:") else why
            problems.append(f"{rel}: /{pattern}/ claims {why}, so it went unchecked")
            continue
        for m in found:
            said, want = m.group(1), truth[fact]
            if fact == "version":
                drifted = said != want
            elif fact.startswith("ycsb:"):
                # Thousands a second, to the last digit written.
                places = len(said.partition(".")[2])
                room = 0.5 * 10 ** -places + tol + 1e-9
                said, drifted = float(said), abs(float(said) - want / 1000) > room
                want = f"{want / 1000:.{places}f}"
            elif fact.startswith("kb"):
                room = 0.5 + tol + (NOISE_KB if fact in COMPRESSED else 0)
                said, drifted = int(said), abs(int(said) - want) > room
                want = f"{want:.1f}"
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
> indexes, runs in the browser as WebAssembly and as a server over HTTP.
> Its query language is FenecQL, which is not SQL.

Writing FenecQL -- the reference below spells it out in full:

- It is not SQL. There is no JOIN, subquery or transaction; related rows
  come from `lookup`. The aggregates -- `count(*)`, `sum`, `avg`, `min`,
  `max` -- go in the select list, over every match or per `group <field>`.
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
- Text is ordered by its bytes; `order title collate tr` orders it as
  Turkish does (ICU's `tr`: `ç` after `c`, `ı` before `i`). The collation
  is for `order` alone -- `where` compares bytes.
- `near` and `match` decide the order: neither combines with `order`, and
  the two together need `fuse`, which ranks by both. Each returns at most
  10 000 rows (`limit + offset`) and adds a `_score` column.
- After `lookup`, every clause belongs to the child collection and `limit`
  counts children per parent. `required` goes right after `on <field>` and
  keeps only the parents with a matching child; `count` goes before `lookup`.

Every statement, by example:

```fenecql
create collection articles (title text @hash, views int, tags [text], published timestamp @sorted, embed vector<4> @hnsw(cosine), body text @text)
put articles {title: "Rust", views: 10, tags: ["lang"], published: "2026-09-01T10:00:00Z", embed: [0.1, 0.2, 0.3, 0.4], body: "Ownership and borrowing"}
put articles [{title: "Zig", views: 3}, {title: "Go", views: 7}]
get articles select title, views where views < 100 and tags has "lang" order views desc limit 5 offset 5
get articles select title order title collate tr, views desc limit 20
get articles where title in ["Rust", "Go"] count
get articles select title where published >= "2026-01-01" near embed $1 limit 3
get articles match body "borrowing" rerank embed $1 candidates 200 limit 10
get articles match body "borrowing" near embed $1 fuse limit 10
get articles select title, sum(views), max(published) where tags has "lang" group title order sum(views) desc limit 5
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


# ------------------------------------------------------------------ search
#
# The site's search is fenecdb itself, in the visitor's browser: every page cut
# into its sections, written into a database by the module the site ships,
# and its image served beside the pages. Nothing is asked of a server and
# nothing is loaded until the search is opened. The documents are built here
# from the pages as rendered, so a section's anchor is the id its heading got.

# The pages that are not docs, and the group each is found under. 404 is not
# content.
SEARCH_PAGES = {"index": "Home", "playground": "Playground"}
# What a section's text does not hold: drawings, scripts, and what the page
# hides from a screen reader too.
SEARCH_SKIP = {"svg", "script", "style", "canvas", "template", "noscript", "button"}
SEARCH_VOID = {"br", "img", "input", "meta", "link", "hr", "wbr", "source", "col", "area"}
SEARCH_BLOCK = {"p", "li", "div", "pre", "tr", "td", "th", "table", "ul", "ol", "dl",
                "dt", "dd", "details", "summary", "figure", "figcaption", "section",
                "blockquote", "h1", "h2", "h3", "h4", "aside", "nav", "header", "footer",
                "main", "article", "br"}


class Sections(html.parser.HTMLParser):
    """A rendered page's text, a section for each h2 and h3: `(id, heading,
    text)`, the first under the page's h1 and no id."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.sections = [[None, "", []]]
        self.skip = None  # (tag, depth) of the element being passed over
        self.heading = None  # the text of the heading being read

    def handle_starttag(self, tag, attrs):
        if self.skip:
            if tag == self.skip[0]:
                self.skip[1] += 1
            return
        attrs = dict(attrs)
        if tag in SEARCH_SKIP or attrs.get("aria-hidden") == "true":
            if tag not in SEARCH_VOID:
                self.skip = [tag, 1]
            return
        if tag in ("h2", "h3"):
            self.sections.append([attrs.get("id"), "", []])
        if tag in ("h1", "h2", "h3"):
            self.heading = []
        elif tag in SEARCH_BLOCK:
            self.sections[-1][2].append(" ")

    def handle_startendtag(self, tag, attrs):
        if not self.skip and tag in SEARCH_BLOCK:
            self.sections[-1][2].append(" ")

    def handle_endtag(self, tag):
        if self.skip:
            if tag == self.skip[0]:
                self.skip[1] -= 1
                if self.skip[1] == 0:
                    self.skip = None
            return
        if tag in ("h1", "h2", "h3") and self.heading is not None:
            text = squash("".join(self.heading))
            if tag == "h1" and len(self.sections) == 1:
                self.sections[0][1] = text
            elif tag != "h1":
                self.sections[-1][1] = text
            self.heading = None
        elif tag in SEARCH_BLOCK:
            self.sections[-1][2].append(" ")

    def handle_data(self, data):
        if self.skip:
            return
        if self.heading is not None:
            self.heading.append(data)
        else:
            self.sections[-1][2].append(data)


def squash(text):
    return re.sub(r"\s+", " ", text).strip()


def search_documents(rendered):
    """`rendered` is `[(key, meta, body)]`, each body as the page shows it;
    the documents of the search index, a section each."""
    groups = {key: group for group, items in NAV for key, _ in items}
    docs = []
    for key, meta, body in rendered:
        group = groups.get(key) or SEARCH_PAGES.get(key)
        if group is None:
            continue
        kind = ("benchmark" if key == "docs/benchmarks"
                else "compare" if group == "Compare" else "doc")
        title = re.sub(r"\s+—\s+fenecdb$", "", meta.get("title", "")).strip()
        if key == "index":
            title = "fenecdb"
        # The URL as the pages link it (`clean_links`): no `.html`, and an
        # index is its directory.
        url = key[: -len("index")] if key.endswith("index") else key
        parser = Sections()
        parser.feed(body)
        parser.close()
        for anchor, heading, text in parser.sections:
            text = squash("".join(text))
            heading = heading or title
            if not text and anchor is None:
                continue
            docs.append({
                "title": title,
                "heading": heading,
                "url": url + (f"#{anchor}" if anchor else ""),
                "body": text,
                "section": group,
                "kind": kind,
            })
    return docs


def search_index(docs, wasm, dest):
    """Writes the documents into a database through the module the site
    ships, and its image to `dest`; returns the number of documents. Fails
    the build on an index missing or empty, and on a known query that does
    not find its page first (`search.test.mjs`)."""
    if not os.path.exists(wasm):
        raise SystemExit("  the search index needs web/fenec.wasm: run `make wasm`")
    node = shutil.which("node")
    if node is None:
        raise SystemExit("  the search index is written by node, which is not on PATH")
    run = subprocess.run([node, os.path.join(ROOT, "search-index.mjs"), wasm, dest],
                         input=json.dumps(docs), capture_output=True, text=True)
    if run.returncode != 0:
        raise SystemExit(f"  writing the search index failed:\n{run.stderr}")
    if not os.path.exists(dest) or os.path.getsize(dest) == 0:
        raise SystemExit(f"  the search index {dest} is missing or empty")
    held = int(run.stdout.strip() or 0)
    if held != len(docs) or held == 0:
        raise SystemExit(f"  the search index holds {held} documents of {len(docs)}")
    test = subprocess.run([node, "--test", os.path.join(ROOT, "search.test.mjs")],
                          capture_output=True, text=True,
                          env={**os.environ, "FENEC_SEARCH_INDEX": dest, "FENEC_WASM": wasm})
    if test.returncode != 0:
        raise SystemExit(f"  the search does not find what it should:\n{test.stdout}{test.stderr}")
    return held


# The fennec, written out of `fennec.js` by `node site/fennec.js`. Inlined,
# so the header draws it with the first paint rather than after a request.
def _mark(name, cls):
    body = open(os.path.join(ROOT, name), encoding="utf-8").read().strip()
    return body.replace("<svg ", f'<svg class="{cls}" ', 1)


MARK = _mark("mark-detail.svg", "mark")
# The mark's lines alone, for the home page's hero to draw at its own size
# and in its own line weight: the same table, so the two cannot drift.
MARK_LINES = re.search(r'<path d="([^"]+)"', open(os.path.join(ROOT, "mark.svg"), encoding="utf-8").read()).group(1)
MARK_DETAIL = _mark("mark-detail.svg", "mark mark-detail")
# The light that runs through the mark's edges, each with its delay, for the
# hero to run again every cycle as the header runs it once.
MARK_LIGHT = re.search(r'<g class="mark-light".*?</g>', open(os.path.join(ROOT, "mark-detail.svg"), encoding="utf-8").read(), re.S).group(0)

# The band under the header on every page but the home page: the home hero's
# four ridges, in its colours and its ridge light, cut to the dunes alone
# (the viewBox starts where the farthest ridge does). The nearest ridge ends
# in #150F26, the colour the band's floor (`.scarp::after`) starts from and
# fades out of, as the home hero's floor does and the footer fades in --
# two navy ridges ending on a hard edge read as another site. Each ridge is
# the home hero's two groups (`.ridge` > `.ridge-in`), so the band rises in
# and moves on scroll by the hero's own keyframes and `site.js`'s parallax.
SCARP = (
    '<div class="scarp" aria-hidden="true">'
    '<svg viewBox="0 60 1440 360" preserveAspectRatio="none" focusable="false">'
    '<defs>'
    '<linearGradient id="sc4" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#5E2F57"/><stop offset="1" stop-color="#37192F"/></linearGradient>'
    '<linearGradient id="sc3" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#8B3C48"/><stop offset="1" stop-color="#4E2130"/></linearGradient>'
    '<linearGradient id="sc2" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#B0522C"/><stop offset=".55" stop-color="#7C3626"/><stop offset="1" stop-color="#48201F"/></linearGradient>'
    '<linearGradient id="sc1" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#2B1742"/><stop offset="1" stop-color="#150F26"/></linearGradient>'
    '<radialGradient id="sckiss" cx=".64" cy="0" r=".55"><stop offset="0" stop-color="#FFCE73" stop-opacity=".75"/><stop offset="1" stop-color="#FFCE73" stop-opacity="0"/></radialGradient>'
    '</defs>'
    '<g class="ridge r4"><g class="ridge-in"><path d="M0 170 C 160 120 320 195 480 150 C 640 105 800 180 980 140 C 1140 105 1300 165 1440 130 L1440 420 L0 420 Z" fill="url(#sc4)"/></g></g>'
    '<g class="ridge r3"><g class="ridge-in"><path d="M0 235 C 180 195 300 265 470 225 C 660 180 790 255 960 215 C 1150 170 1290 240 1440 205 L1440 420 L0 420 Z" fill="url(#sc3)"/></g></g>'
    '<g class="ridge r2"><g class="ridge-in">'
    '<path d="M0 300 C 150 265 340 330 520 292 C 700 254 830 320 1010 285 C 1190 250 1310 305 1440 275 L1440 420 L0 420 Z" fill="url(#sc2)"/>'
    '<path d="M0 300 C 150 265 340 330 520 292 C 700 254 830 320 1010 285 C 1190 250 1310 305 1440 275" fill="none" stroke="url(#sckiss)" stroke-width="3" vector-effect="non-scaling-stroke"/>'
    '</g></g>'
    '<g class="ridge r1"><g class="ridge-in">'
    '<path d="M0 368 C 200 340 330 392 540 362 C 760 330 880 386 1080 356 C 1260 330 1350 372 1440 350 L1440 420 L0 420 Z" fill="url(#sc1)"/>'
    '<path d="M0 368 C 200 340 330 392 540 362 C 760 330 880 386 1080 356 C 1260 330 1350 372 1440 350" fill="none" stroke="url(#sckiss)" stroke-width="2" vector-effect="non-scaling-stroke"/>'
    '</g></g>'
    '</svg></div>')

# Put back where the reader left the sidebar on the page before, before the
# first paint: a sidebar link loads a page, and a sidebar starting at its top
# again had the link just followed out of sight in a long nav. Where nothing
# was kept, or what was kept leaves the current page out of view (a link from
# the page body, a narrower window), the current page is centred in it --
# by the sidebar's own scrollTop, so the page itself never moves.
SIDE_RESTORE = (
    "<script>(function(){var s=document.getElementById('side'),h=s.querySelector('a.here'),k=null;"
    "try{k=sessionStorage.getItem('fenec-side')}catch(e){}"
    "if(k!==null)s.scrollTop=+k;"
    # What of the sidebar shows: at the top of a page it starts below the
    # dunes, so its foot is under the fold until the page is scrolled.
    "var v=Math.min(s.clientHeight,innerHeight-s.getBoundingClientRect().top);"
    "if(h&&v>0&&(h.offsetTop<s.scrollTop||h.offsetTop+h.offsetHeight>s.scrollTop+v))"
    "s.scrollTop=h.offsetTop-(v-h.offsetHeight)/2})()</script>")


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
    #
    # fenec.js imports builder.js and http.js, and client.js the two of
    # them: each module's hashed copy imports the others' hashed copies,
    # so the imported are made first.
    engine = {}
    for name in CLIENT_MODULES + ("fenec.wasm",):
        src = os.path.join(REPO, "web", name)
        if not os.path.exists(src):
            print(f"  note: web/{name} missing -- run `make wasm` for the playground")
            continue
        shutil.copy(src, os.path.join(OUT, name))
        if name.endswith(".js"):
            body = open(src, encoding="utf-8").read()
            for dep, hashed_dep in engine.items():
                body = body.replace(f"from './{dep}'", f"from './{hashed_dep}'")
            blob = minify(body, ".js").encode("utf-8")
        else:
            blob = open(src, "rb").read()
        stem, ext = os.path.splitext(name)
        hashed = f"{stem}.{hashlib.sha256(blob).hexdigest()[:10]}{ext}"
        open(os.path.join(OUT, hashed), "wb").write(blob)
        engine[name] = hashed

    # The collation data the module is handed as a page needs it: beside the
    # stable module under `collate/`, and for the site's own under a name
    # that changes with the data -- a chunk of other tables is refused, and
    # a cached one would be handed over until the cache let it go.
    chunks = os.path.join(REPO, "web", "collate")
    if os.path.isdir(chunks):
        names = sorted(n for n in os.listdir(chunks) if n.endswith(".bin"))
        digest = hashlib.sha256()
        for n in names:
            digest.update(n.encode("utf-8"))
            digest.update(open(os.path.join(chunks, n), "rb").read())
        engine["collate/"] = f"collate.{digest.hexdigest()[:10]}/"
        for d in ("collate/", engine["collate/"]):
            os.makedirs(os.path.join(OUT, d), exist_ok=True)
            for n in names:
                shutil.copy(os.path.join(chunks, n), os.path.join(OUT, d, n))

    # The mark's table, then the scenes that import it, then the page
    # script that imports them: each named by its hash, inside out.
    mark_js = emit("fennec.js", open(os.path.join(ROOT, "fennec.js"), encoding="utf-8").read())
    motion = open(os.path.join(ROOT, "motion.js"), encoding="utf-8").read()
    motion_js = emit("motion.js", motion.replace("'./fennec.js'", f"'./{mark_js}'"))

    pages = []
    for dirpath, _, files in os.walk(os.path.join(ROOT, "content")):
        for name in sorted(files):
            if name.endswith(".html"):
                pages.append(os.path.join(dirpath, name))

    # The search: its index first, since the script that opens it names the
    # image by its hash. The pages are rendered for it as they are below, so
    # each section's anchor is the id its heading is given there.
    rendered = []
    for path in sorted(pages):
        key = os.path.relpath(path, os.path.join(ROOT, "content"))[:-5].replace(os.sep, "/")
        meta, body = read_page(path)
        rendered.append((key, meta, headings(render_code_blocks(body))[0]))
    docs = search_documents(rendered)
    index_path = os.path.join(OUT, "search.fenec")
    held = search_index(docs, os.path.join(REPO, "web", "fenec.wasm"), index_path)
    image = open(index_path, "rb").read()
    os.remove(index_path)
    # Shipped gzipped, and opened in the page with DecompressionStream: a
    # file of no type the edge knows is served as it lies, and as it lies the
    # image is three times what it is gzipped.
    packed = gziplib.compress(image, 9, mtime=0)
    index_name = f"search.{hashlib.sha256(image).hexdigest()[:10]}.fenec.gz"
    open(os.path.join(OUT, index_name), "wb").write(packed)
    engine["search.fenec"] = index_name
    _, br = compressed_bytes(image)
    print(f"  search: {held} sections, an image of {len(image) / 1024:.1f} KB, "
          f"{len(packed) / 1024:.1f} KB gzipped as served"
          + (f" ({br / 1024:.1f} KB brotli)" if br else ""))
    search_css = emit("search.css", open(os.path.join(ROOT, "search.css"), encoding="utf-8").read())
    search_query = emit("search-query.js", open(os.path.join(ROOT, "search-query.js"), encoding="utf-8").read())
    search = open(os.path.join(ROOT, "search.js"), encoding="utf-8").read()
    for plain, hashed in (("./search-query.js", search_query), ("./search.css", search_css),
                          ("./search.fenec", index_name),
                          ("./fenec.js", engine.get("fenec.js", "fenec.js")),
                          ("./fenec.wasm", engine.get("fenec.wasm", "fenec.wasm"))):
        search = search.replace(f"'{plain}'", f"'./{hashed}'")
    search_js = emit("search.js", search)

    script = open(os.path.join(ROOT, "site.js"), encoding="utf-8").read()
    script = script.replace("'./motion.js'", f"'./{motion_js}'")
    script = script.replace("'./search.js'", f"'./{search_js}'")
    emit("site.js", script)

    shutil.copy(os.path.join(ROOT, "mark.svg"), os.path.join(OUT, "favicon.svg"))

    # The web fonts, named by their hash like the rest: styles.css names
    # them and the template preloads one, so both are rewritten to the
    # hashed names, and _headers caches them for good.
    os.makedirs(os.path.join(OUT, "fonts"), exist_ok=True)
    styles = open(os.path.join(ROOT, "styles.css"), encoding="utf-8").read()
    for name in sorted(os.listdir(os.path.join(ROOT, "fonts"))):
        if not name.endswith(".woff2"):
            continue
        blob = open(os.path.join(ROOT, "fonts", name), "rb").read()
        hashed = f"fonts/{name[:-6]}.{hashlib.sha256(blob).hexdigest()[:10]}.woff2"
        open(os.path.join(OUT, hashed), "wb").write(blob)
        assets[f"fonts/{name}"] = hashed
        styles = styles.replace(f"url(fonts/{name})", f"url({hashed})")
    emit("styles.css", styles)

    studio = build_studio(engine, assets)

    # Screenshots the docs show (images/), named by their hash as the fonts
    # are: a page names `images/<name>`, rewritten below to the hashed name.
    images = os.path.join(ROOT, "images")
    if os.path.isdir(images):
        os.makedirs(os.path.join(OUT, "images"), exist_ok=True)
        for name in sorted(os.listdir(images)):
            stem, ext = os.path.splitext(name)
            if ext not in (".webp", ".png", ".svg"):
                continue
            blob = open(os.path.join(images, name), "rb").read()
            hashed = f"images/{stem}.{hashlib.sha256(blob).hexdigest()[:10]}{ext}"
            open(os.path.join(OUT, hashed), "wb").write(blob)
            assets[f"images/{name}"] = hashed

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
        # One header link is current: Benchmarks for its page, Compare for the
        # comparison pages, Docs for the rest of the docs, Playground for its own.
        here = ' aria-current="page"'
        compare = key == "docs/compare" or key.startswith("docs/vs-")
        page = page.replace("{{nav_bench}}", here if key == "docs/benchmarks" else "")
        page = page.replace("{{nav_compare}}", here if compare else "")
        page = page.replace("{{nav_docs}}", here if is_docs and key != "docs/benchmarks"
                            and not compare else "")
        page = page.replace("{{nav_playground}}", here if key == "playground" else "")
        if is_docs:
            # The dune field carries over the top of every docs page, so the
            # reference does not read as a different site from the front door.
            shell = (f'{SCARP}<div class="shell">'
                     f'<aside class="side" id="side" aria-label="Documentation">'
                     f'<div class="side-inner">{nav_html(key, base)}</div></aside>{SIDE_RESTORE}'
                     f'<main class="doc" id="content"><article>{body}'
                     f'{prev_next(key, base)}</article></main>'
                     f'{toc_html(toc)}</div>')
        else:
            shell = f'<main id="content">{body}</main>'
        page = page.replace("{{content}}", shell)
        page = page.replace("{{scarp}}", SCARP)
        page = page.replace("{{mark}}", MARK).replace("{{mark_detail}}", MARK_DETAIL)
        page = page.replace("{{mark_lines}}", MARK_LINES).replace("{{mark_light}}", MARK_LIGHT)

        for plain, hashed in assets.items():
            page = page.replace(plain, hashed)
        page = clean_links(page)

        dest = os.path.join(OUT, key + ".html")
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        open(dest, "w", encoding="utf-8").write(page)

    # Cloudflare reads this from the asset directory; it is not served itself.
    # Hashed assets can be cached forever because a change gives a new name --
    # that now includes the engine the playground loads.
    # The stable `fenec.js` / `fenec.wasm` names exist for the docs links, and
    # those revalidate: a stale engine would silently be the wrong one.
    rules = ["/*",
             "  X-Content-Type-Options: nosniff",
             "  Referrer-Policy: strict-origin-when-cross-origin",
             "  X-Frame-Options: DENY",
             ""]
    # The playground frames the studio's page, which every other page's
    # DENY would refuse: framed by the site alone.
    rules += ["/studio/*",
              "  ! X-Frame-Options",
              "  Content-Security-Policy: frame-ancestors 'self'",
              ""]
    for hashed in sorted(list(assets.values()) + list(engine.values()) + studio):
        if hashed.endswith("/"):
            hashed += "*"
        rules += [f"/{hashed}", "  Cache-Control: public, max-age=31536000, immutable", ""]
    for stable in CLIENT_MODULES + ("fenec.wasm", "collate/*"):
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


STUDIO_HIGHLIGHT = os.path.join(REPO, "studio", "highlight.js")

# ------------------------------------------------------------------ the studio
#
# The playground is fenec studio -- the files fenec-server embeds with
# `--studio`, not a copy of them -- over a database in the page. Its page is
# `dist/studio/`, which the playground frames between the site's header and
# footer: the studio's stylesheet and the site's name the same classes and
# tokens (`.top`, `.side`, `.btn`, `--sun`), and a frame keeps each to its
# own. Every module is copied, minified and named by its hash, as the site's
# are, each import rewritten to the name it got, so the page can be cached
# for good and a deploy never pairs a new module with an old one. The
# playground's own modules come from site/: its boot and its data.
STUDIO = os.path.join(REPO, "studio")
STUDIO_SITE = ("playground.js", "playground-data.js")
# A name a module, a stylesheet or the page writes in quotes: `'./grid.js'`,
# `import('./editor.js')`, `new URL('./worker.js', ...)`, `'views.css'`.
STUDIO_REF = re.compile(r"""(['"])(?:\./)?([\w-]+\.(?:js|css))\1""")


def build_studio(engine, assets):
    """dist/studio/: the studio's page in local mode and every module and
    stylesheet it reaches, each named by its hash. Returns the hashed paths
    (`studio/<name>`), which `_headers` caches for good."""
    out = os.path.join(OUT, "studio")
    os.makedirs(out, exist_ok=True)
    sources = {n: os.path.join(STUDIO, n) for n in os.listdir(STUDIO) if n.endswith((".js", ".css"))}
    sources.update({n: os.path.join(ROOT, n) for n in STUDIO_SITE})
    # The web client and the module are the site's own hashed copies, a
    # directory up; a font and the mark too.
    up = {n: f"../{engine[n]}" for n in CLIENT_MODULES + ("fenec.wasm", "collate/") if n in engine}
    for weight in ("400", "500"):
        up[f"plex-mono-{weight}.woff2"] = f"../{assets[f'fonts/plex-mono-{weight}-latin.woff2']}"
    named = {}

    def hashed(name, through=()):
        if name in named:
            return named[name]
        if name in through:
            raise SystemExit(f"  the studio's modules import each other in a circle: {' -> '.join(through + (name,))}")
        body = open(sources[name], encoding="utf-8").read()

        def ref(m):
            q, n = m.group(1), m.group(2)
            if n in sources and n != name:
                return f"{q}./{hashed(n, through + (name,))}{q}"
            if n in up:
                return f"{q}{up[n]}{q}"
            return m.group(0)

        body = STUDIO_REF.sub(ref, body)
        for plain in ("../fenec.wasm", "../collate/"):
            target = plain[3:]
            if f"'{plain}'" in body:
                if target not in up:
                    raise SystemExit(f"  studio/{name} names {plain}, which the build has not got: make wasm")
                body = body.replace(f"'{plain}'", f"'{up[target]}'")
        for font, target in up.items():
            if font.endswith(".woff2"):
                body = body.replace(f"url({font})", f"url({target})")
        stem, ext = os.path.splitext(name)
        blob = minify(body, ext).encode("utf-8")
        named[name] = f"{stem}.{hashlib.sha256(blob).hexdigest()[:10]}{ext}"
        open(os.path.join(out, named[name]), "wb").write(blob)
        return named[name]

    entry = hashed("playground.js")
    # Every module app.js may fetch later, and the views' stylesheet, made
    # whether or not the entry reached them: an unused one is a few bytes.
    for n in sorted(sources):
        hashed(n)

    page = open(os.path.join(STUDIO, "index.html"), encoding="utf-8").read()

    def must(old, new):
        nonlocal page
        if old not in page:
            raise SystemExit(f"  studio/index.html lost `{old}`: the playground's page is made from it")
        page = page.replace(old, new)

    must('<script type="module" src="app.js"></script>',
         "\n".join(f'<link rel="modulepreload" href="{n}">' for n in ("local.js", "playground-data.js"))
         + f'\n<link rel="preload" href="{up["fenec.wasm"]}" as="fetch" crossorigin>'
         + '\n<script type="module" src="playground.js"></script>')
    must('<div id="app" class="booting">', '<div id="app" class="booting" data-mode="local">')
    must('<title>fenec studio</title>',
         '<title>fenec studio, in this tab</title>\n<meta name="robots" content="noindex">\n'
         '<meta name="description" content="fenec studio over a fenecdb database in this tab: the playground.">')
    must('href="mark.svg"', 'href="../favicon.svg"')
    must('<link rel="stylesheet" href="app.css">', '<link rel="stylesheet" href="app.css">\n<link rel="stylesheet" href="local.css">')

    def attr(m):
        n = m.group(2)
        if n in sources:
            return f'{m.group(1)}{hashed(n)}"'
        return f'{m.group(1)}{up.get(n, n)}"'

    page = re.sub(r'((?:href|src)=")([\w.-]+)"', attr, page)
    open(os.path.join(out, "index.html"), "w", encoding="utf-8").write(page)
    total = sum(os.path.getsize(os.path.join(out, f)) for f in named.values())
    print(f"  studio: {len(named)} modules and stylesheets, {total / 1024:.1f} KB, entry {entry}")
    return [f"studio/{n}" for n in named.values()]


def studio_highlight():
    """fenec studio's copy of the editor's highlighter, the rules written in
    as for the site: the server embeds the studio's files as they are in the
    repository, with no Python at `cargo build`, so the generated file is
    kept there, and `site/test_highlight.py` refuses one that is not what
    this writes (`make studio-highlight`)."""
    open(STUDIO_HIGHLIGHT, "w", encoding="utf-8").write(highlight_js())
    print(f"wrote {os.path.relpath(STUDIO_HIGHLIGHT, REPO)}")


if __name__ == "__main__":
    if "--studio-highlight" in sys.argv:
        studio_highlight()
        sys.exit(0)
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
