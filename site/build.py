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
import urllib.parse

import builder_tabs

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(ROOT)
OUT = os.path.join(ROOT, "dist")

REPO_URL = "https://github.com/fenecdb/fenecdb"

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

# Ordered docs navigation. Groups are rendered as the sidebar sections, in the
# order of what a reader is doing: Start gets a first query running, Clients
# is how an app talks to the database, Guides are what one builds with it,
# Reference is where one looks a thing up, Operate is for whoever runs a
# server, Design and comparisons is why it is built as it is and how it sits
# beside others, and Benchmarks is what was measured. A page that fits two groups sits with the task that
# brings the reader to it: HTTP endpoint is in Clients, for the reader with no
# SDK for their language, and the reference of that client (every route, the
# statuses, the tokens and policies) is in Reference, where the Limits page
# points; File format is Reference, the place one looks a byte up, though it
# is also why the file is as it is; Studio is Operate, an admin page the
# server serves.
NAV = [
    # The Quickstart is one path, the server's, and no step of it stops to
    # choose between installs: every way is on Install. The browser's path is
    # beside it, not under Clients, since a reader who wants the database in a
    # page would not look there to begin.
    ("Start", [
        ("docs/index", "Overview"),
        ("docs/quickstart", "Quickstart"),
        ("docs/quickstart-browser", "Quickstart: in the browser"),
        ("docs/install", "Install"),
    ]),
    ("Clients", [
        ("docs/languages", "Your language"),
        ("docs/javascript", "JavaScript and TypeScript"),
        ("docs/mobile", "Mobile and native apps"),
        ("docs/http", "HTTP endpoint"),
        ("docs/embedding", "Embedded Rust"),
        # A way to talk to a file, which the FenecQL tour is run in.
        ("docs/shell", "The shell"),
        ("docs/integrations", "Integrations"),
        ("docs/mcp", "Agents and MCP"),
    ]),
    ("Guides", [
        ("docs/sync", "Sync"),
        # What an app adds Redis for, a page a family of patterns: this one
        # says which page holds each, and is named for what it replaces now
        # that no group is.
        ("docs/redis", "Instead of Redis"),
        ("docs/queues", "Job queues"),
        ("docs/geo", "Points and distances"),
        ("docs/analytics", "Analytics and market data"),
        ("docs/microservices", "Services and events"),
        ("docs/serverless", "Serverless and Cloudflare"),
        ("docs/import", "Import"),
    ]),
    ("Reference", [
        ("docs/fenecql", "FenecQL"),
        # The reference this page once held, a page a family of statements,
        # the general-database ones first and vectors last (the tour on
        # docs/fenecql lists them); labelled as the pages are titled, since
        # the group says what they are. A fragment of the old page is
        # followed to its new one by moved.json.
        ("docs/fenecql-reading", "Reading data"),
        ("docs/fenecql-writing", "Writing data"),
        ("docs/fenecql-schema", "Collections and types"),
        ("docs/fenecql-indexes", "Indexes"),
        ("docs/fenecql-lookup", "Lookup and subqueries"),
        ("docs/fenecql-aggregates", "Aggregates and grouping"),
        ("docs/fenecql-search", "Text search"),
        ("docs/fenecql-vector", "Vector search"),
        ("docs/sql", "From SQL"),
        # What `docs/http` once held beyond the way in (which is HTTP
        # endpoint, in Clients): the routes and everything a request can
        # carry or be answered with, and who may ask. A section of the old
        # page is followed to its new one by moved.json.
        ("docs/http-reference", "HTTP reference"),
        ("docs/http-access", "HTTP security and scoped access"),
        ("docs/limits", "Limits"),
        ("docs/file-format", "File format"),
    ]),
    ("Operate", [
        ("docs/server", "Server"),
        ("docs/studio", "Studio"),
        ("docs/replication", "Replication"),
        ("docs/monitoring", "Monitoring"),
        ("docs/datadog", "Datadog and others"),
        ("docs/grafana", "Grafana"),
    ]),
    # What `docs/sharding` once held, in a group of its own for what a reader
    # is doing with it: the tutorial, a guide to each part, the reference to
    # look a thing up in and the design. A section of the old page is followed
    # to its new one by moved.json. The labels of the last two keep "Sharding":
    # the previous and next links and llms.txt show a label with no group.
    ("Tenants and sharding", [
        ("docs/sharding", "Tenants and sharding"),
        ("docs/sharding-tenants", "Tenants"),
        ("docs/sharding-failover", "Replicas and failover"),
        ("docs/sharding-regions", "Regions"),
        ("docs/sharding-entry", "The global entry"),
        ("docs/sharding-read-replicas", "Read replicas"),
        ("docs/sharding-reference", "Sharding reference"),
        ("docs/sharding-design", "Sharding design"),
    ]),
    # The comparisons are labelled "vs ...", since the group does not say
    # what the three are beside.
    ("Design and comparisons", [
        ("docs/concepts", "How it works"),
        ("docs/compare", "Compare"),
        ("docs/vs-postgres", "vs PostgreSQL"),
        ("docs/vs-sqlite", "vs SQLite"),
        ("docs/vs-pglite", "vs PGlite"),
    ]),
    # What `docs/benchmarks` once held, in a group of its own, as the
    # sharding pages are: the overview, then a page for each family, the
    # general database and text search before vectors. A section of the old
    # page is followed to its new one by moved.json. The labels of the family
    # pages keep "benchmarks": the previous and next links and llms.txt show
    # a label with no group, and "Text search" and "Vector search" are the
    # Reference group's.
    ("Benchmarks", [
        ("docs/benchmarks", "Benchmarks"),
        ("docs/benchmarks-ycsb", "YCSB benchmarks"),
        ("docs/benchmarks-requests", "Concurrency and request benchmarks"),
        ("docs/benchmarks-text", "Text search benchmarks"),
        ("docs/benchmarks-vector", "Vector search benchmarks"),
        ("docs/benchmarks-size", "Size and memory benchmarks"),
    ]),
    # Last, after the reader has what they came for: Install installs a
    # release, and what is left for a clone is here.
    ("Contribute", [
        ("docs/build", "Build from source"),
    ]),
]


def is_benchmark(key):
    """Whether `key` (as NAV keys it) is the benchmarks overview or one of
    its family pages: the header's Benchmarks link is current on all of
    them, and the search files them under one kind."""
    return key == "docs/benchmarks" or key.startswith("docs/benchmarks-")


# ---------------------------------------------------------------- highlighting

KEYWORDS = {
    "fenecql": """create drop collection index if not exists get put set del select
        from where near order limit offset count ef exact asc desc and or in has is
        null true false collections describe compact begin commit on match fuse
        lookup group insert set del rerank absent returning fuzzy prefix like ilike
        between""".split(),
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
        FORMAT count avg sum INSERT INTO VALUES UPDATE SET DELETE ON CONFLICT DO
        NOTHING RETURNING ORDER LIMIT OFFSET DESC ASC AND OR NOT IN BETWEEN LIKE
        ILIKE ESCAPE IS NULL DISTINCT HAVING JOIN INNER LEFT OUTER EXISTS CREATE
        TABLE INDEX UNIQUE PRIMARY KEY USING WITH""".split(),
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
# A page whose script is the point of showing it (the browser Quickstart's): the
# tags are plain words and the script's words are JavaScript's.
KEYWORDS["html"] = KEYWORDS["js"]

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
    "html": r"//[^\n]*|/\*[\s\S]*?\*/|<!--[\s\S]*?-->",
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
            # An attribute's entities read back first (a builder tab's label
            # names Go's import path in quotes), so a label is escaped once.
            head = f'<div class="code-label">{html.escape(html.unescape(label.group(1)))}</div>'
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


def docs_map():
    """Every page of NAV, a group at a time, for the docs index: written
    from the navigation, so a page added there is reachable from the index
    with nothing else to remember. Links are relative to docs/."""
    groups = []
    for group, items in NAV:
        links = "".join(
            f'<li><a href="{key[len("docs/"):]}.html">{html.escape(label)}</a></li>'
            for key, label in items if key != "docs/index")
        if links:
            groups.append(f'<div><p class="docmap-group">{html.escape(group)}</p><ul>{links}</ul></div>')
    return f'<div class="docmap">{"".join(groups)}</div>'


# The tabs of Your language that talk to a server with the HTTP client the
# language has, as the HTTP endpoint page shows them. The others embed the
# database in an app and their first example opens a file, so they stay on
# Your language; a tab in neither list is a build error, or a new HTTP
# language would be skipped here with nothing said.
HTTP_TABS = ("Python", "JavaScript", "Go", "Rust", ".NET", "Java", "PHP", "Ruby")
EMBEDDED_TABS = ("Swift", "Kotlin", "Dart")
LANG_TAB = re.compile(r'<div class="lang" data-name="([^"]+)">\n([\s\S]*?)\n</div>')
CODE_BLOCK = re.compile(r'<pre[^>]*>[\s\S]*?</pre>\n')


def first_example(name, body):
    """A tab's first example: its install line, if it has one, then the
    program, and nothing after that. A tab goes on from there with prose and
    the builder's example, and the example is cut at the prose: with the
    prose gone a third block would be taken in with no word said, so a tab
    that is not an install line (labelled `install`) and one program, or a
    program alone, is an error."""
    blocks, at = [], 0
    while (m := CODE_BLOCK.match(body + "\n", at)):
        blocks.append(m.group(0))
        at = m.end()
    installs = ['data-label="install"' in b for b in blocks]
    if installs not in ([False], [True, False]):
        raise SystemExit(
            f"docs/languages.html's `{name}` tab does not start with an install line "
            "(labelled `install`, optional) and one program, then prose: docs/http.html "
            f"takes that much of it, and found {len(blocks)} blocks in a row")
    return "".join(blocks)


def language_tabs():
    """The first example of each of Your language's tabs in `HTTP_TABS`, as
    the tabs of another page's `data-langs` box: the HTTP endpoint page's
    examples are the languages page's, taken from it when the page is read,
    so the two cannot say different things (`make languages-test` holds that
    page's statements to the programs it runs)."""
    src = open(os.path.join(ROOT, "content", "docs", "languages.html"), encoding="utf-8").read()
    tabs = {name: body for name, body in LANG_TAB.findall(src)}
    unplaced = [n for n in tabs if n not in HTTP_TABS + EMBEDDED_TABS]
    missing = [n for n in HTTP_TABS + EMBEDDED_TABS if n not in tabs]
    if unplaced or missing:
        raise SystemExit(
            "docs/languages.html's tabs are not the ones docs/http.html is made for: "
            f"{'a tab named ' + ', '.join(unplaced) if unplaced else 'no tab named ' + ', '.join(missing)}"
            " -- put it in HTTP_TABS (a language the server is reached from with the "
            "language's own HTTP client) or EMBEDDED_TABS (one that opens a file) in build.py")
    out = []
    for name in HTTP_TABS:
        out.append(f'<div class="lang" data-name="{name}">\n{first_example(name, tabs[name]).rstrip()}\n</div>\n')
    return "\n".join(out)


# Where the sections of a page that was split went (`moved.json`: the page,
# then each fragment it had and the page and fragment that hold it now), for
# a script on the old page to follow: a fragment never reaches the server, so
# no redirect rule can. `{{moved_script}}` in a page is that script with the
# page's own map, `{{moved}}` the map alone as an object literal;
# check_links.py holds every target to a page and an id that were built, and
# every page moved.json names to carry the script (`data-moved`).
MOVED = os.path.join(ROOT, "moved.json")
MOVED_SCRIPT = """<script data-moved>
(function(){
  /* A link to a section this page once held (a fragment of its address) is
     sent where the section went. */
  var moved=@@MAP@@;
  function go(){
    var h=location.hash.slice(1);
    try{h=decodeURIComponent(h)}catch(e){}
    if(h&&Object.prototype.hasOwnProperty.call(moved,h))location.replace(moved[h]);
  }
  go();
  addEventListener('hashchange',go);
})();
</script>"""


def moved_map(page):
    """The map of `page` (as NAV keys it: `docs/sharding`) in moved.json, as
    the object literal the page's script reads."""
    with open(MOVED, encoding="utf-8") as f:
        pages = json.load(f)
    if page not in pages:
        raise SystemExit(f"{page} asks for its moved sections, and moved.json has none for it")
    return json.dumps(pages[page], separators=(",", ":"), sort_keys=True)


def filled(raw, page=None):
    """A page's text with what the build writes into it: the release's
    number wherever the page says `{{version}}`, from the one place
    `make version` writes it (the workspace's Cargo.toml), so a release moves
    the install lines, the image's tag and the sample output with it; the
    docs index's map of every page; the HTTP endpoint page's tabs of
    languages; and, for a page that was split (`page` is its NAV key), the
    script that sends a reader to where a section went (`{{moved_script}}`);
    and a FenecQL example's tabs of the same query in each SDK's builder
    (`{{builder "case" ...}}` over the block, `builder_tabs.py`), each case
    held to its statement. Claims are checked against this text, as it is
    read."""
    if "{{language_tabs}}" in raw:
        raw = raw.replace("{{language_tabs}}", language_tabs())
    if "{{builder" in raw:
        raw = builder_tabs.expand(raw, f"site/content/{page}.html" if page else "a page")
    if page is not None and "{{moved" in raw:
        raw = (raw.replace("{{moved_script}}", MOVED_SCRIPT.replace("@@MAP@@", moved_map(page)))
                  .replace("{{moved}}", moved_map(page)))
    return (raw.replace("{{version}}", workspace_version())
               .replace("{{docs_map}}", docs_map()))


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
    key = os.path.relpath(path, os.path.join(ROOT, "content"))[:-len(".html")]
    return meta, filled(ycsb_redis(raw), key)


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
              "kb_lite_gz", "kb_lite_br", "kb_min_gz", "kb_min_br",
              "kb_app_br", "kb_app_gz", "kb_app_client_br", "kb_app_client_gz", "kb_studio_gz"}
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
# The benchmarks pages a claim is on: docs/benchmarks is the overview, whose
# glance holds the headline YCSB cells and the sizes a page loads, and each
# family has a page of its own (a sentence a claim holds is on the page its
# table is on; the YCSB page also holds the full grid, `ycsb_grid_claims`).
BENCH = "site/content/docs/benchmarks.html"
BENCH_YCSB = "site/content/docs/benchmarks-ycsb.html"
BENCH_SIZE = "site/content/docs/benchmarks-size.html"
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
    ("site/content/docs/concepts.html", r"(\d+) KB gzip<br><small>the wasm", "kb_gz", 0),
    (BENCH_SIZE,
     r'wasm32, browser</td><td class="n"><b>(\d+) KB</b>', "kb", 0),
    (BENCH_SIZE,
     r'<b>\d+ KB</b></td><td class="n"><b>(\d+) KB</b></td><td class="n">\d+ KB</td>',
     "kb_br", 0),
    (BENCH_SIZE,
     r'<b>\d+ KB</b></td><td class="n"><b>\d+ KB</b></td><td class="n">(\d+) KB</td>',
     "kb_gz", 0),
    (BENCH_SIZE,
     r'the client</td><td class="n">(\d+) KB</td>', "kb_client", 0),
    (BENCH_SIZE,
     r'the client</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_client_br", 0),
    (BENCH_SIZE,
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
    ("site/content/docs/javascript.html", r"make wasm +# every feature: (\d+) KB brotli", "kb_br", 0),
    ("site/content/docs/javascript.html",
     r"make wasm FEATURES=language SCHEMA=0   # no index, nor the schema check: (\d+) KB", "kb_lite_br", 0),
    ("site/content/docs/javascript.html",
     r"make wasm FEATURES=none SCHEMA=0       # no feature at all: (\d+) KB", "kb_min_br", 0),
    ("site/content/docs/javascript.html", r"modules it\s+imports, (\d+) KB brotli", "kb_client_br", 0),
    ("site/content/docs/javascript.html", r"bundles to\s+(\d+) KB brotli through it", "kb_app_client_br", 0),
    ("site/content/docs/javascript.html", r"and to (\d+) KB through <code>@fenecdb/web</code>", "kb_app_br", 0),
    ("site/content/docs/integrations.html", r"no module, (\d+) KB brotli of client", "kb_app_client_br", 0),
    ("site/content/docs/integrations.html", r"<td><code>fenec.wasm</code>, (\d+) KB brotli</td>", "kb_br", 0),
    (BENCH,
     r'<code>fenec.wasm</code></td><td class="n"><b>(\d+) KB</b></td>', "kb_br", 0),
    (BENCH,
     r'<code>fenec.wasm</code></td><td class="n"><b>\d+ KB</b></td><td class="n">(\d+) KB</td>', "kb_gz", 0),
    (BENCH,
     r'<code>fenec.js</code> and its modules</td><td class="n">(\d+) KB</td>', "kb_client_br", 0),
    (BENCH,
     r'<code>fenec.js</code> and its modules</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_client_gz", 0),
    (BENCH,
     r"in an app's bundle, <code>@fenecdb/web/client</code></td><td class=\"n\">(\d+) KB</td>", "kb_app_client_br", 0),
    (BENCH_SIZE,
     r"<code>@fenecdb/web/client</code> in an app's bundle, <code>connect</code> and a query's rows, esbuild, minified</td><td class=\"n\">(\d+) KB</td>", "kb_app_client", 0),
    (BENCH_SIZE,
     r"<code>@fenecdb/web/client</code> in an app's bundle, <code>connect</code> and a query's rows, esbuild, minified</td><td class=\"n\">\d+ KB</td><td class=\"n\">(\d+) KB</td>", "kb_app_client_br", 0),
    (BENCH_SIZE,
     r"<code>@fenecdb/web/client</code> in an app's bundle, <code>connect</code> and a query's rows, esbuild, minified</td><td class=\"n\">\d+ KB</td><td class=\"n\">\d+ KB</td><td class=\"n\">(\d+) KB</td>",
     "kb_app_client_gz", 0),
    (BENCH_SIZE,
     r'<code>make wasm FEATURES=language SCHEMA=0</code></td><td class="n">(\d+) KB</td>', "kb_lite", 0),
    (BENCH_SIZE,
     r'<code>make wasm FEATURES=language SCHEMA=0</code></td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_lite_br", 0),
    (BENCH_SIZE,
     r'<code>make wasm FEATURES=language SCHEMA=0</code></td><td class="n">\d+ KB</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_lite_gz", 0),
    (BENCH_SIZE,
     r'<code>make wasm FEATURES=none SCHEMA=0</code></td><td class="n">(\d+) KB</td>', "kb_min", 0),
    (BENCH_SIZE,
     r'<code>make wasm FEATURES=none SCHEMA=0</code></td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_min_br", 0),
    (BENCH_SIZE,
     r'<code>make wasm FEATURES=none SCHEMA=0</code></td><td class="n">\d+ KB</td><td class="n">\d+ KB</td><td class="n">(\d+) KB</td>',
     "kb_min_gz", 0),
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
    ("README.md", r"does B at 16 clients, durable, at ([\d.]+) k against", "ycsb:server-docker:durable:B:16", 0),
    ("README.md", r"durable, at [\d.]+ k against\s+PostgreSQL's ([\d.]+) k, both in Docker", "ycsb:pg:durable:B:16", 0),
    # The benchmarks page's glance, ahead of the section the cells are in.
    (BENCH, r"B, 95% reads, 16 threads, buffered</td><td class=\"n\"><b>([\d.]+) k", "ycsb:fenec:buffered:B:16", 0),
    (BENCH, r"B, 95% reads, 16 threads, buffered</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:sqlite:buffered:B:16", 0),
    (BENCH, r"C, reads, one thread, after the updates</td><td class=\"n\"><b>([\d.]+) k", "ycsb:fenec:buffered:C:1", 0),
    (BENCH, r"C, reads, one thread, after the updates</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:sqlite:buffered:C:1", 0),
    (BENCH, r"A, 50% updates, 16 threads, durable</td><td class=\"n\"><b>([\d.]+) k", "ycsb:fenec:durable:A:16", 0),
    (BENCH, r"A, 50% updates, 16 threads, durable</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:sqlite:durable:A:16", 0),
    (BENCH, r"B, 95% reads, 16 clients</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:B:16", 0),
    (BENCH, r"B, 95% reads, 16 clients</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:B:16", 0),
    (BENCH, r"B, 95% reads, 16 clients</td>(?:<td[^>]*>(?:<b>)?[\d.]+ k(?:</b>)?</td>){2}<td class=\"n\">([\d.]+) k", "ycsb:mongo:durable:B:16", 0),
    (BENCH, r"A, 50% updates, 16 clients</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:A:16", 0),
    (BENCH, r"A, 50% updates, 16 clients</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:A:16", 0),
    (BENCH, r"A, 50% updates, 16 clients</td>(?:<td[^>]*>(?:<b>)?[\d.]+ k(?:</b>)?</td>){2}<td class=\"n\">([\d.]+) k", "ycsb:mongo:durable:A:16", 0),
    (BENCH, r"A, 50% updates, one client</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:A:1", 0),
    (BENCH, r"A, 50% updates, one client</td><td[^>]*><b>[\d.]+ k</b></td><td class=\"n\">([\d.]+) k", "ycsb:pg:durable:A:1", 0),
    (BENCH, r"A, 50% updates, one client</td>(?:<td[^>]*>(?:<b>)?[\d.]+ k(?:</b>)?</td>){2}<td class=\"n\">([\d.]+) k", "ycsb:mongo:durable:A:1", 0),
    ("site/content/index.html", r"YCSB B over HTTP, 16 clients, durable</dt><dd><b>([\d.]+) k ops/s", "ycsb:server-docker:durable:B:16", 0),
    ("site/content/index.html", r"PostgreSQL ([\d.]+) k, both in Docker", "ycsb:pg:durable:B:16", 0),
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
    ("site/content/docs/vs-postgres.html", r"its median now ([\d.]+) k", "ycsb:pg:buffered:C:1", 0),
    ("site/content/docs/vs-postgres.html", r"fenec-server leads C,\s+([\d.]+) k", "ycsb:server-docker:buffered:C:16", 0),
    ("site/content/docs/vs-postgres.html", r"fenec-server leads C,\s+[\d.]+ k against ([\d.]+) k", "ycsb:pg:buffered:C:16", 0),
    (BENCH_YCSB, r"buffered C ([\d.]+) k against [\d.]+ k and F", "ycsb:server-docker:buffered:C:1", 0),
    (BENCH_YCSB, r"buffered C [\d.]+ k against ([\d.]+) k and F", "ycsb:pg:buffered:C:1", 0),
    (BENCH_YCSB, r"here it does ([\d.]+) k against\s+PostgreSQL's [\d.]+ k buffered", "ycsb:server-docker:buffered:E:1", 0),
    (BENCH_YCSB, r"here it does [\d.]+ k against\s+PostgreSQL's ([\d.]+) k buffered", "ycsb:pg:buffered:E:1", 0),
    (BENCH_YCSB, r"k buffered, ([\d.]+) k against [\d.]+ k durable", "ycsb:server-docker:durable:E:1", 0),
    (BENCH_YCSB, r"k buffered, [\d.]+ k against ([\d.]+) k durable", "ycsb:pg:durable:E:1", 0),
    (BENCH_YCSB, r"3.27 k on 2026-10-04; here ([\d.]+) k against", "ycsb:server-docker:durable:A:1", 0),
    (BENCH_YCSB, r"here [\d.]+ k against PostgreSQL's ([\d.]+) k\)", "ycsb:pg:durable:A:1", 0),
    (BENCH_YCSB, r"and F ([\d.]+) k against [\d.]+ k\. A read", "ycsb:server-docker:buffered:F:1", 0),
    (BENCH_YCSB, r"and F [\d.]+ k against ([\d.]+) k\. A read", "ycsb:pg:buffered:F:1", 0),
    (BENCH_YCSB, r"2026-10-05, ([\d.]+) k here", "ycsb:pg:buffered:C:1", 0),
    (BENCH_YCSB, r"fenec-server's [\d.]+ k and ([\d.]+) k\. E was", "ycsb:server-docker:buffered:C:1", 0),
    (BENCH_YCSB, r"A ([\d.]+) k at one client and [\d.]+ k at 16, F", "ycsb:server:durable:A:1", 0),
    (BENCH_YCSB, r"A [\d.]+ k at one client and ([\d.]+) k at 16, F", "ycsb:server:durable:A:16", 0),
    (BENCH_YCSB, r"at 16, F ([\d.]+) k and [\d.]+ k\. Half", "ycsb:server:durable:F:1", 0),
    (BENCH_YCSB, r"at 16, F [\d.]+ k and ([\d.]+) k\. Half", "ycsb:server:durable:F:16", 0),
    # A cache in front of fenec-server (concepts.html tells the architecture,
    # these pages the comparison with Redis): the cells they quote, their
    # latencies, the count of cells Redis led in and the read by key taken
    # apart, each held to what the results (or benchmarks-requests.html's
    # round-trip table) say.
    ("site/content/index.html", r"YCSB C, all reads, 16 clients, buffered</dt><dd><b>([\d.]+) k ops/s", "ycsb:server-docker:buffered:C:16", 0),
    ("site/content/index.html", r"<span>p99 ([\d.]+) ms; Redis [\d.]+ k", "ycsbms:server-docker:buffered:C:16:slowest_p99", 0),
    ("site/content/index.html", r"<span>p99 [\d.]+ ms; Redis ([\d.]+) k, p99", "ycsb:redis:buffered:C:16", 0),
    ("site/content/index.html", r"Redis [\d.]+ k, p99 ([\d.]+) ms; both in Docker", "ycsbms:redis:buffered:C:16:slowest_p99", 0),
    ("site/content/index.html", r"<span>Redis ([\d.]+) k, PostgreSQL [\d.]+ k, both in Docker", "ycsb:redis:durable:B:16", 0),
    ("site/content/index.html", r"<b>(none) of \d+</b>", "redis:lost", 0),
    ("site/content/index.html", r"<b>none of (\d+)</b>", "redis:both", 0),
    ("site/content/index.html", r"spends\s+([\d.]+) ms\s+inside\s+the\s+server\s+of\s+a\s+[\d.]+ ms\s+round\s+trip", "rt:docker:read:inside", 0),
    ("site/content/index.html", r"inside\s+the\s+server\s+of\s+a\s+([\d.]+) ms\s+round\s+trip", "rt:docker:read:round_trip", 0),
    ("site/content/docs/redis.html", r"C: 100% read, 16 clients, buffered</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:buffered:C:16", 0),
    ("site/content/docs/redis.html", r"C: 100% read, 16 clients, buffered</td><td class=\"n\"><b>[\d.]+ k</b><br><small>p99 ([\d.]+) ms", "ycsbms:server-docker:buffered:C:16:slowest_p99", 0),
    ("site/content/docs/redis.html", r"C: 100% read, 16 clients, buffered</td>(?:<td[^>]*>.*?</td>){1}<td class=\"n\">([\d.]+) k", "ycsb:redis:buffered:C:16", 0),
    ("site/content/docs/redis.html", r"C: 100% read, 16 clients, buffered</td>(?:<td[^>]*>.*?</td>){1}<td class=\"n\">[\d.]+ k<br><small>p99 ([\d.]+) ms", "ycsbms:redis:buffered:C:16:slowest_p99", 0),
    ("site/content/docs/redis.html", r"B: 95% read, 5% update, 16 clients, durable</td><td class=\"n\"><b>([\d.]+) k", "ycsb:server-docker:durable:B:16", 0),
    ("site/content/docs/redis.html", r"B: 95% read, 5% update, 16 clients, durable</td><td class=\"n\"><b>[\d.]+ k</b><br><small>p99 ([\d.]+) ms", "ycsbms:server-docker:durable:B:16:slowest_p99", 0),
    ("site/content/docs/redis.html", r"B: 95% read, 5% update, 16 clients, durable</td>(?:<td[^>]*>.*?</td>){1}<td class=\"n\">([\d.]+) k", "ycsb:redis:durable:B:16", 0),
    ("site/content/docs/redis.html", r"B: 95% read, 5% update, 16 clients, durable</td>(?:<td[^>]*>.*?</td>){1}<td class=\"n\">[\d.]+ k<br><small>p99 ([\d.]+) ms", "ycsbms:redis:durable:B:16:slowest_p99", 0),
    ("site/content/docs/redis.html", r"(none)\s+of\s+the\s+\d+\s+cells\s+measured\s+both\s+ways", "redis:lost", 0),
    ("site/content/docs/redis.html", r"none\s+of\s+the\s+(\d+)\s+cells\s+measured\s+both\s+ways", "redis:both", 0),
    ("site/content/docs/redis.html", r"behind\s+fenec-server\s+buffered\s+in\s+(none)\s+of\s+the\s+\d+\s+cells", "redis:snapshot_lost", 0),
    ("site/content/docs/redis.html", r"behind\s+fenec-server\s+buffered\s+in\s+none\s+of\s+the\s+(\d+)\s+cells", "redis:snapshot_both", 0),
    ("site/content/docs/redis.html", r"takes\s+([\d.]+) ms\s+at\s+the\s+median\s+from\s+fenec-server", "ycsbms:server-docker:buffered:C:1:READ_p50", 0),
    ("site/content/docs/redis.html", r"and\s+([\d.]+) ms\s+from\s+Redis", "ycsbms:redis:buffered:C:1:READ_p50", 0),
    ("site/content/docs/redis.html", r"spends\s+([\d.]+) ms\s+inside\s+the\s+server\s+of\s+a\s+[\d.]+ ms\s+round\s+trip", "rt:docker:read:inside", 0),
    ("site/content/docs/redis.html", r"inside\s+the\s+server\s+of\s+a\s+([\d.]+) ms\s+round\s+trip", "rt:docker:read:round_trip", 0),
    ("README.md", r"fenec-server's C at 16 clients, buffered, is ([\d.]+) k against", "ycsb:server-docker:buffered:C:16", 0),
    ("README.md", r"buffered, is [\d.]+ k against Redis's ([\d.]+) k, and its B", "ycsb:redis:buffered:C:16", 0),
    ("README.md", r"and its B, durable, ([\d.]+) k against [\d.]+ k \(one node", "ycsb:server-docker:durable:B:16", 0),
    ("README.md", r"and its B, durable, [\d.]+ k against ([\d.]+) k \(one node", "ycsb:redis:durable:B:16", 0),
    ("README.md", r"Redis does more operations a second in (none) of the \d+ cells", "redis:lost", 0),
    ("README.md", r"Redis does more operations a second in none of the (\d+) cells", "redis:both", 0),
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


YCSB_WORKLOADS = ("A", "B", "C", "D", "E", "F")
YCSB_OPS = ("READ", "UPDATE", "INSERT", "SCAN", "RMW")


def ycsb_cells():
    """Each line `ycsb` wrote -- a cell, a load, a size, a memory -- as the
    median of its runs, by `(system, mode, workload, threads)`: `ops_s`,
    `ops` (where a size or a memory line keeps its bytes) and each
    operation's `<OP>_p50` and `<OP>_p99`, each column's median on its own
    as `ycsb report` takes them, the upper of the two middle runs when there
    are an even number. A run's cell measured again (a run cut short,
    completed later under its own id) counts once, as its last line."""
    if not os.path.exists(YCSB_RESULTS):
        return {}
    lines = open(YCSB_RESULTS, encoding="utf-8").read().splitlines()
    head = lines[0].split("\t")
    col = {name: i for i, name in enumerate(head)}
    superseded = set()
    if os.path.exists(YCSB_SUPERSEDED):
        for line in open(YCSB_SUPERSEDED, encoding="utf-8").read().splitlines()[1:]:
            run, system = line.split("\t")[:2]
            superseded.add((run, system))
    latest = {}
    for line in lines[1:]:
        f = line.split("\t")
        if (f[col["run"]], f[col["system"]]) in superseded:
            continue
        key = tuple(f[col[c]] for c in ("system", "mode", "workload", "threads"))
        latest[(key, f[col["run"]])] = f
    runs = {}
    for (key, _), f in latest.items():
        runs.setdefault(key, []).append(f)
    names = ["ops_s", "ops"] + [f"{op}_{q}" for op in YCSB_OPS for q in ("p50", "p99")]
    out = {}
    for key, fs in runs.items():
        cell = {}
        for name in names:
            vals = []
            for f in fs:
                try:
                    vals.append(float(f[col[name]]))
                except (IndexError, ValueError):
                    pass  # a size line stops at the probe's column
            if vals:
                cell[name] = sorted(vals)[len(vals) // 2]
        out[key] = cell
    return out


def ycsb_facts():
    """Each YCSB cell's median ops/s, as `ycsb report` takes it."""
    return {"ycsb:" + ":".join(key): cell["ops_s"] for key, cell in ycsb_cells().items()
            if key[2] in YCSB_WORKLOADS}


def ycsb_latency_facts():
    """A latency a page quotes beside a cell, in microseconds, held to the
    last digit it shows: `ycsbms:<system>:<mode>:<workload>:<threads>:<stat>`,
    the stat being `slowest_p99` (the p99 the tables put under a figure) or
    an operation's `READ_p50` or `READ_p99`. The pages that say what a cache
    in front of fenec-server can and cannot take off a read quote these, so
    they cannot drift from the results they rest on."""
    out = {}
    for key, cell in ycsb_cells().items():
        if key[2] not in YCSB_WORKLOADS:
            continue
        base = "ycsbms:" + ":".join(key)
        if any(f"{op}_p99" in cell for op in YCSB_OPS):
            out[base + ":slowest_p99"] = slowest_p99(cell)
        for stat in ("READ_p50", "READ_p99"):
            if stat in cell:
                out[f"{base}:{stat}"] = cell[stat]
    return out


# fenec-server's mode against Redis's: the same against the same, and
# fenec-server buffered against Redis with its snapshots alone (no append-only
# file), which keeps no log and is the nearest to a cache.
SAME_MODE = (("durable", "durable"), ("buffered", "buffered"))
AGAINST_SNAPSHOTS = (("buffered", "snapshot"),)


def redis_compared(cells, modes=SAME_MODE):
    """The cells measured for fenec-server in Docker and for Redis alike, as
    (mode, workload, clients, ours, theirs), durable first: the same mode
    against the same, or whatever `modes` pairs (ours, Redis's)."""
    out = []
    for mode, redis_mode in modes:
        for wl in YCSB_WORKLOADS:
            for n in sorted({int(k[3]) for k in cells if k[0] == "redis" and k[1] == redis_mode}):
                ours = cells.get(("server-docker", mode, wl, str(n)))
                theirs = cells.get(("redis", redis_mode, wl, str(n)))
                if ours and theirs:
                    out.append((mode, wl, n, ours, theirs))
    return out


def redis_facts():
    """How many cells were measured both ways (`redis:both`) and in how many
    Redis did more operations a second (`redis:lost`); and the same against
    Redis with its snapshots alone (`redis:snapshot_both`,
    `redis:snapshot_lost`). A page may say "none of the 33 cells", and the
    build fails when one stops being so."""
    cells = ycsb_cells()
    out = {}
    for both, lost, modes in (("redis:both", "redis:lost", SAME_MODE),
                              ("redis:snapshot_both", "redis:snapshot_lost", AGAINST_SNAPSHOTS)):
        compared = redis_compared(cells, modes)
        if compared:
            out[both] = len(compared)
            out[lost] = sum(1 for *_, ours, theirs in compared if theirs["ops_s"] > ours["ops_s"])
    return out


# A read by key in Docker taken apart (`make roundtrip-bench`) has no results
# file: the table of benchmarks-requests.html is where its medians are
# recorded, so a page quoting them is held to that table
# (`rt:docker:read:<part>`, microseconds).
ROUND_TRIP_PAGE = os.path.join(ROOT, "content", "docs", "benchmarks-requests.html")
ROUND_TRIP_ROWS = (
    ("rt:docker:read:round_trip",
     r'<tr><td>round trip, p50</td><td class="n">[\d.]+ ms</td><td class="n"><b>([\d.]+) ms</b>'),
    ("rt:docker:read:inside",
     r'<tr><td>inside the server</td><td class="n">[\d.]+ ms</td><td class="n">([\d.]+) ms</td>'),
)


def round_trip_facts():
    page = open(ROUND_TRIP_PAGE, encoding="utf-8").read()
    out = {}
    for fact, pattern in ROUND_TRIP_ROWS:
        m = re.search(pattern, page)
        if m:
            out[fact] = float(m.group(1)) * 1000
    return out


# What the bench's figures give: every fact with the unit a drifted claim is
# reported in, and the prefixes that say a fact is one of them.
BENCH_FACTS = ("ycsb:", "ycsbms:", "redis:", "rt:")


def bench_facts():
    ycsb = ycsb_facts()
    latency = {**ycsb_latency_facts(), **round_trip_facts()}
    counts = redis_facts()
    # The counts are of cells; a page says `none` for how many Redis led in.
    unit = {**{k: " k ops/s" for k in ycsb}, **{k: " ms" for k in latency},
            **{k: " cells" for k in counts}}
    return {**ycsb, **latency, **counts}, unit


# ---- Redis on benchmarks-ycsb.html#ycsb
#
# Redis's part of the YCSB section is in the page only once the results hold
# a Redis cell: until then there is no Redis column at all, rather than an
# empty one. Its prose is the page's, between `<!-- ycsb redis: begin -->`
# and `<!-- ycsb redis: end -->` (a span or several, all kept or all taken
# out), and its figures are written in at `<!-- ycsb redis: tables -->`,
# `lost`, `memory` and `grid` from the medians -- so each is the median, and
# the full grid's claims, which read the page as built, hold it so.

REDIS_SPAN = re.compile(r"<!-- ycsb redis: begin -->([\s\S]*?)<!-- ycsb redis: end -->")
REDIS_MARK = re.compile(r"<!-- ycsb redis: (tables|lost|memory|grid) -->")
YCSB_LABELS = {
    "A": "A: 50% read, 50% update",
    "B": "B: 95% read, 5% update",
    "C": "C: 100% read",
    "D": "D: 95% read the latest, 5% insert",
    "E": "E: 95% scan, 5% insert",
    "F": "F: 50% read, 50% read-modify-write",
}


def kops(ops):
    """Operations a second as the YCSB tables write them: thousands, to
    three figures."""
    k = ops / 1000
    return f"{k:.2f}" if k < 10 else f"{k:.1f}" if k < 100 else f"{k:.0f}"


def ms(us):
    """A latency in microseconds as the YCSB tables write it, in ms."""
    m = us / 1000
    if m < 1:
        return f"{m:.2f}"
    if m < 100:
        return f"{m:.1f}"
    return f"{m:,.0f}".replace(",", " ")


def megabytes(n):
    return f"{n / 1e6:,.0f}".replace(",", " ") + " MB"


def slowest_p99(cell):
    """The p99 of a cell's slowest operation, which the tables put under it."""
    return max(cell[f"{op}_p99"] for op in YCSB_OPS if f"{op}_p99" in cell)


def redis_figure(cell, bold):
    if cell is None:
        return '<td class="n">&nbsp;</td>'
    figure = f"{kops(cell['ops_s'])} k"
    if bold:
        figure = f"<b>{figure}</b>"
    return f'<td class="n">{figure}<br><small>p99 {ms(slowest_p99(cell))} ms</small></td>'


def redis_tables(cells):
    """fenec-server in Docker beside Redis, mode for mode, at 1 and 16
    clients, each pair's leader in bold; Redis's snapshots alone beside the
    buffered pair, against neither."""
    get = lambda system, mode, wl, n: cells.get((system, mode, wl, str(n)))
    out = []
    for mode, caption, extra in (
        ("durable", "<p>Durable: fenec-server's <code>--sync always</code> against Redis's "
                    "<code>appendfsync always</code>, an fsync before each answer, both in "
                    "Docker's virtual machine:</p>", None),
        ("buffered", "<p>Buffered, both in Docker as well: <code>--sync 250</code> against <code>appendfsync "
                     "everysec</code>, and beside them Redis with its snapshots alone, which "
                     "nothing here is measured against:</p>", "snapshot"),
    ):
        head = []
        for n in (1, 16):
            clients = "1 client" if n == 1 else f"{n} clients"
            head.append(f'<th class="n">fenec-server, {clients}</th><th class="n">Redis, {clients}</th>')
            if extra:
                head.append(f'<th class="n">Redis, snapshots, {clients}</th>')
        rows = []
        for wl in YCSB_WORKLOADS:
            tds = []
            for n in (1, 16):
                ours, theirs = get("server-docker", mode, wl, n), get("redis", mode, wl, n)
                lead = ours and theirs and (ours["ops_s"] >= theirs["ops_s"])
                tds.append(redis_figure(ours, bool(lead)))
                tds.append(redis_figure(theirs, bool(ours and theirs and not lead)))
                if extra:
                    tds.append(redis_figure(get("redis", extra, wl, n), False))
            if all(t == '<td class="n">&nbsp;</td>' for t in tds):
                continue  # C writes nothing and is measured buffered alone
            rows.append(f"<tr><td>{YCSB_LABELS[wl]}</td>{''.join(tds)}</tr>")
        if rows:
            out.append(f'{caption}\n<div class="tbl">\n<table>\n<thead><tr><th>{mode}</th>'
                       f'{"".join(head)}</tr></thead>\n<tbody>\n' + "\n".join(rows)
                       + "\n</tbody>\n</table>\n</div>")
    return "\n".join(out)


def redis_lost(cells):
    """Every cell in which Redis does more operations a second than
    fenec-server in Docker, the same mode against the same mode."""
    compared = redis_compared(cells)
    lost = []
    for mode, wl, n, ours, theirs in compared:
        if theirs["ops_s"] > ours["ops_s"]:
            clients = "one client" if n == 1 else f"{n} clients"
            lost.append(f"<li>{mode} {wl}, {clients}: {kops(ours['ops_s'])} k against "
                        f"{kops(theirs['ops_s'])} k, p99 {ms(slowest_p99(ours))} ms against "
                        f"{ms(slowest_p99(theirs))} ms</li>")
    if not lost:
        return f"<p>None of the {len(compared)} cells measured both ways.</p>"
    return (f"<p>{len(lost)} of the {len(compared)} cells measured both ways:</p>\n<ul>\n"
            + "\n".join(lost) + "\n</ul>")


def redis_memory(cells):
    """What each server held after A, B, C, F and D, on disk and in memory
    (its container's anonymous and shared memory), mode by mode."""
    systems = (("server-docker", "fenec-server in Docker"), ("redis", "Redis"),
               ("pg", "PostgreSQL"), ("mongo", "MongoDB"))
    modes = [m for m in ("durable", "buffered", "snapshot")
             if any(k[1] == m and k[2] == "memory-ABCFD" for k in cells)]
    head = "".join(f'<th class="n">{m}, on disk</th><th class="n">{m}, in memory</th>' for m in modes)
    rows = []
    for system, name in systems:
        tds = []
        for m in modes:
            for what in ("size", "memory"):
                cell = cells.get((system, m, f"{what}-ABCFD", "1"))
                tds.append(f'<td class="n">{megabytes(cell["ops"])}</td>' if cell and "ops" in cell
                           else '<td class="n">&nbsp;</td>')
        if any("MB" in t for t in tds):
            rows.append(f"<tr><td>{name}</td>{''.join(tds)}</tr>")
    if not rows:
        return ""
    return (f'<div class="tbl">\n<table>\n<thead><tr><th>after A to F</th>{head}</tr></thead>\n'
            f'<tbody>\n' + "\n".join(rows) + "\n</tbody>\n</table>\n</div>")


def redis_grid(cells):
    """Redis's rows of the full grid, as the others' are written."""
    order = {"durable": 0, "buffered": 1, "snapshot": 2}
    keys = sorted((k for k in cells if k[0] == "redis" and k[2] in YCSB_WORKLOADS),
                  key=lambda k: (order.get(k[1], 3), k[2], int(k[3])))
    rows = []
    for k in keys:
        cell = cells[k]
        ops = "; ".join(f"{op.lower()} {ms(cell[f'{op}_p50'])} ms / {ms(cell[f'{op}_p99'])} ms"
                        for op in YCSB_OPS if f"{op}_p50" in cell and f"{op}_p99" in cell)
        rows.append(f'<tr><td>Redis</td><td>{k[1]}</td><td>{k[2]}</td><td class="n">{k[3]}</td>'
                    f'<td class="n">{kops(cell["ops_s"])} k</td><td>{ops}</td></tr>')
    return "\n".join(rows)


def ycsb_redis(body):
    """The page with Redis's part of the YCSB section written in where the
    results hold a Redis cell, and taken out where they hold none."""
    if "<!-- ycsb redis:" not in body:
        return body
    cells = ycsb_cells()
    if not any(k[0] == "redis" and k[2] in YCSB_WORKLOADS for k in cells):
        return REDIS_MARK.sub("", REDIS_SPAN.sub("", body))
    made = {"tables": redis_tables, "lost": redis_lost, "memory": redis_memory, "grid": redis_grid}
    body = REDIS_SPAN.sub(lambda m: m.group(1), body)
    return REDIS_MARK.sub(lambda m: made[m.group(1)](cells), body)


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
        bench, bench_unit = bench_facts()
        unit = {**bench_unit, "kb_studio_gz": " KB"}
        return claims_against({**bench, "kb_studio_gz": studio_gz()}, unit,
                              lambda f: f.startswith(BENCH_FACTS) or f == "kb_studio_gz")
    size = os.path.getsize(wasm)
    wasm_gz, wasm_br = compressed(wasm)
    # The client is fenec.js and the two modules it imports, as a page loads
    # them with no build step.
    client = b"".join(open(web(n), "rb").read() for n in CLIENT_MODULES if n != "client.js")
    client_gz, client_br = compressed_bytes(client)
    kb = lambda n: None if n is None else n / 1024
    # The module built without the indexes or the schema check (`make
    # wasm-lite`) and the one with no feature at all (`make wasm-min`),
    # test builds no package ships, whose sizes the docs quote as build
    # options, where they were built: a claim on one missing says so.
    other = {}
    for key, name in (("lite", "fenec-lite.wasm"), ("min", "fenec-min.wasm")):
        if os.path.exists(web(name)):
            gz, br = compressed(web(name))
            other[f"kb_{key}"] = kb(os.path.getsize(web(name)))
            other[f"kb_{key}_gz"] = kb(gz)
            other[f"kb_{key}_br"] = kb(br)
    apps = {}
    for key, entry in (("kb_app", "fenec.js"), ("kb_app_client", "client.js")):
        bundle = app_bundle(entry)
        gz, br = (None, None) if bundle is None else compressed_bytes(bundle)
        apps[key] = None if bundle is None else kb(len(bundle))
        apps[f"{key}_gz"] = kb(gz)
        apps[f"{key}_br"] = kb(br)
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
    bench, bench_unit = bench_facts()
    unit = {"glue": " lines", "version": "", "bytes": " bytes"}
    # Every size fact is in KB; without a default the report of a drifted
    # size died on a KeyError instead of saying which file drifted.
    unit = {**{k: " KB" for k in truth}, **bench_unit, **unit}
    return claims_against({**truth, **bench}, unit, lambda f: True)


# The flags of fenec-shard, which docs/sharding-reference lists in a table of
# their own: a flag added to its usage and not to the table fails the build
# (as a number that drifted does), since nothing else would say the page was
# behind the program.
SHARD_MAIN = os.path.join(REPO, "crates", "fenec-shard", "src", "main.rs")
SHARD_FLAGS_PAGE = os.path.join(ROOT, "content", "docs", "sharding-reference.html")


def check_router_flags():
    """The flags `fenec-shard --help` lists that the first column of the
    reference's table of them lacks, and the rows of it for a flag the
    usage no longer has."""
    usage = re.search(r'const USAGE: &str = "\\\n([\s\S]*?)\n";', open(SHARD_MAIN, encoding="utf-8").read())
    if usage is None:
        return [f"{os.path.relpath(SHARD_MAIN, REPO)}: no USAGE to hold sharding-reference.html's flags to"]
    flags = re.findall(r"^(?: {2}-\w, | {6})(--[a-z][a-z0-9-]*)", usage.group(1), re.M)
    if not flags:
        return ["fenec-shard's usage lists no flag: the pattern that reads it is out of date"]
    page = open(SHARD_FLAGS_PAGE, encoding="utf-8").read()
    # only its own section: the page has other tables
    page = re.search(r'<h2 id="router-flags">[\s\S]*?(?=<h2)', page)
    page = page.group(0) if page else ""
    listed = re.findall(r"<tr><td><code>(?:-\w, )?(--[a-z][a-z0-9-]*)", page)
    return ([f"docs/sharding-reference.html has no row for fenec-shard's {f}" for f in flags if f not in listed]
            + [f"docs/sharding-reference.html has a row for {f}, which fenec-shard's usage no longer has"
               for f in listed if f not in flags])


# What a fold's summary may not be: a bare word that says nothing of what is inside.
FOLD_BARE = {"details", "detail", "method", "methods", "method and details", "measured", "more", "notes"}
# A number in a summary as a page writes one: digits in groups of three and a
# decimal part, and not the end of a name (f32, int8, wasm32).
FOLD_NUMBER = re.compile(r"(?<![\w.,])\d+(?: \d{3})*(?:[.,]\d+)?(?!\w)")


def check_fold_labels():
    """The folds (`<details class="deep">`) whose summary does not say what is
    inside, one line for each, since a reader cannot tell what "Details" hides.
    A fold's first child is a plain `<summary>` of a few words: not a bare
    "Details", "Method" or "Measured", nor "Details: ..."; with no ";" or ":"
    (a phrase, not a list); and a number in it is one the fold itself holds,
    since no claim holds a label's figures to a measurement."""
    problems = []
    for dirpath, _, files in os.walk(os.path.join(ROOT, "content")):
        for name in sorted(files):
            if not name.endswith(".html"):
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, ROOT)
            text = open(path, encoding="utf-8").read()
            for m in re.finditer(r"<details\b([^>]*)>", text):
                if not re.search(r"""\bclass\s*=\s*["'][^"']*\bdeep\b""", m.group(1)):
                    continue
                where = f"{rel}:{text.count(chr(10), 0, m.start()) + 1}"
                end, depth = m.end(), 1
                while depth:
                    tag = re.compile(r"<(/?)details\b").search(text, end)
                    if tag is None:
                        break
                    depth += -1 if tag.group(1) else 1
                    end = tag.end()
                summary = re.match(r"\s*<summary(\b[^>]*)>(.*?)</summary>", text[m.end():], re.S)
                if summary is None:
                    problems.append(f"{where}: a fold's first child is no <summary>, so it names nothing")
                    continue
                if summary.group(1).strip():
                    problems.append(f"{where}: the <summary> has attributes; a fold's label is its bare text")
                    continue
                if "<" in summary.group(2):
                    problems.append(f"{where}: the <summary> holds tags; its label is plain text")
                    continue
                label = re.sub(r"\s+", " ", html.unescape(summary.group(2))).strip()
                said = label.lower().rstrip(" .")
                if not label:
                    problems.append(f"{where}: the <summary> is empty, so the fold names nothing")
                elif said in FOLD_BARE or re.match(r"(?:method and )?details?\s*[:\-–—]", said):
                    problems.append(f"{where}: {label!r} does not say what is inside; name it, as in "
                                    f"'How the lock is taken'")
                elif ";" in label or ":" in label:
                    problems.append(f"{where}: {label!r} has a ';' or ':'; write a phrase, not a list")
                else:
                    body = text[m.end() + summary.end():end]  # what the fold holds, not its label
                    held = re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", " ", body)))
                    for number in FOLD_NUMBER.findall(label):
                        if not re.search(r"(?<![\w.,])" + re.escape(number) + r"(?!\w|[.,]\d)", held):
                            problems.append(f"{where}: {label!r} has {number}, which the fold never says; "
                                            f"a figure in a label is one the fold holds")
    return problems


# The names benchmarks-ycsb.html#ycsb's full grid gives each system.
YCSB_NAMES = {"fenec": "fenecdb", "sqlite": "SQLite", "server": "fenec-server",
              "server-docker": "fenec-server in Docker", "pg": "PostgreSQL", "mongo": "MongoDB",
              "redis": "Redis"}


def ycsb_grid_claims(truth):
    """A claim for each cell of benchmarks-ycsb.html#ycsb's full grid: its row
    names the cell, so every one is held to the results, not a few."""
    out = []
    for fact in truth:
        if not fact.startswith("ycsb:"):
            continue
        _, system, mode, wl, threads = fact.split(":")
        out.append((BENCH_YCSB,
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
        # As built: Redis's YCSB rows are written into the page then, and
        # the release's number into every `{{version}}`.
        body = filled(ycsb_redis(open(path, encoding="utf-8").read()))
        found = list(re.finditer(pattern, body, re.MULTILINE))
        if not found:
            problems.append(f"{rel}: nothing matched /{pattern}/ -- reworded?")
            continue
        if truth.get(fact) is None:
            why = (
                "an app's bundle and `esbuild` is not on PATH" if fact.startswith("kb_app")
                else "a module `make wasm-min` did not build" if fact.startswith("kb_min") and fact not in truth
                else "a module `make wasm-lite` did not build" if fact not in truth
                else "a brotli size and `brotli` is not installed"
            )
            why = ("a YCSB cell crates/fenec-bench/ycsb/results.tsv does not hold"
                   if fact.startswith(("ycsb:", "ycsbms:", "redis:")) else
                   "a row of benchmarks-requests.html's round-trip table it no longer has"
                   if fact.startswith("rt:") else why)
            problems.append(f"{rel}: /{pattern}/ claims {why}, so it went unchecked")
            continue
        for m in found:
            # `written` is what the page says, as it says it, and a drift
            # reports it so; `want` is the truth as the page would write it.
            written, want = m.group(1), truth[fact]
            if fact == "version":
                drifted = written != want
            elif fact.startswith("ycsb:"):
                # Thousands a second, to the last digit written.
                places = len(written.partition(".")[2])
                room = 0.5 * 10 ** -places + tol + 1e-9
                drifted = abs(float(written) - want / 1000) > room
                want = f"{want / 1000:.{places}f}"
            elif fact.startswith(("ycsbms:", "rt:")):
                # Milliseconds, to the last digit written.
                places = len(written.partition(".")[2])
                room = 0.5 * 10 ** -places + tol + 1e-9
                drifted = abs(float(written) - want / 1000) > room
                want = f"{want / 1000:.{places}f}"
            elif fact.startswith("redis:"):
                # A count of cells; a page says `none` for zero.
                said = 0 if written == "none" else int(written)
                drifted = said != int(want)
                want = "none" if int(want) == 0 else str(int(want))
            elif fact.startswith("kb"):
                room = 0.5 + tol + (NOISE_KB if fact in COMPRESSED else 0)
                drifted = abs(int(written) - want) > room
                want = f"{want:.1f}"
            else:
                drifted = abs(int(written) - want) > tol
            if drifted:
                line = body.count("\n", 0, m.start()) + 1
                if fact.endswith("_lost") or fact.endswith(":lost"):
                    problems.append(f"{rel}:{line}: says Redis is ahead in {written} of the cells, "
                                    f"it is ahead in {want}")
                else:
                    problems.append(f"{rel}:{line}: says {written}{unit[fact]}, it is {want}{unit[fact]}")
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

> Minimal embedded document database: one file, live queries an app's
> screens follow, a replica on the device that syncs with a server, a server
> over HTTP with a file per tenant, and search built in -- BM25 over text,
> with typos, highlights and facets, and HNSW vectors for meaning. It runs in
> the browser as WebAssembly, in an app, and as a server. Its query language
> is FenecQL, which is not SQL but reads SQL's everyday spellings.

Writing FenecQL -- the reference below spells it out in full:

- It is not SQL, but it reads SQL's everyday spellings as the statements
  they mean: `CREATE TABLE` and `CREATE INDEX`, `INSERT INTO t (a, b)
  VALUES (1, 'x')`, `UPDATE t SET a = 1 WHERE ...`, `IN (1, 2)`,
  `BETWEEN`, `LIKE` and `ILIKE`, `ON CONFLICT DO NOTHING` and `DO UPDATE
  SET`, `DISTINCT`, `EXISTS`, single quotes and `||`
  (https://fenecdb.com/docs/sql). A double-quoted word is text, never a
  column: a field is written bare, `where name = 'Ada'`. Every row has an
  `id` (an int the database makes; no `rowid`, no `_id`). A JOIN on a
  key is read as `lookup`, which FenecQL writes `get orders lookup users on
  id = user_id` -- each order with its user nested; the clauses after
  `lookup` are the looked-up collection's -- and a filter by another
  collection's rows is an uncorrelated
  `in (get <collection> select <field> where ...)`, or `not in (...)` for
  the rows it does not name. There is no interactive
  transaction: a `/batch` of statements (in the page, a `run` of several)
  lands whole or not at all, and `require <n>` after a write makes its count
  a condition that puts the whole batch back.
- The aggregates -- `count(*)`, `count(f)` (the rows where `f` is not
  null), `count(distinct f)`, `sum`, `avg`, `min`, `max`, `first(f by k)`,
  `last(f by k)` -- go in the select list, over every match or per
  `group <key>`, and `having` keeps groups. Distinct values are a group:
  `select city group city`.
- Text: `like 'a%'` and `ilike` take SQL's `%` and `_`; `~` is a
  case-insensitive *contains* (`email ~ "@example.org"`), never a regular
  expression, and `match` ranks words over an `@text` field.
- Square brackets are list literals and nothing else: `tags: ["a", "b"]`,
  `tags [text]`. Nothing here marks an optional part with them; a clause you
  do not need is simply left out.
- Strings take double or single quotes. Parameters are `$1`, `$2`, ...; a vector
  parameter is a list of numbers. A timestamp is written as text,
  `"2026-09-01T10:00:00Z"`, or `"2026-01-01"` in a comparison.
- Types: `bool int float text bytes timestamp`, lists such as `[text]`,
  `json` (any JSON value; a path such as `meta.source` reads into it),
  `geo` (a point, `[lon, lat]`), `vector<N>`, `vector<N, f16>` and
  `sparse<N>`. A typed field holds no object; a `json` field does. There is
  no decimal -- money is an `int` of cents -- and no UUID type (use
  `text @hash`).
- Indexes: `@hash` for equality, `@unique` for a value only one row may hold,
  `@sorted` for ranges and for `order` with a `limit`, `@ttl(30m)` for rows
  that expire, `@text` for `match`, `@hnsw(cosine)` (or `l2`, `dot`) for
  `near`. Only an `and` chain uses an index: `=` and `in [...]` on a `@hash`
  field or on `id`, `<`, `<=`, `>`, `>=` and `=` on a `@sorted` field;
  everything else scans. `explain get ...` runs a query and returns the path
  it took.
- Text is ordered by its bytes; `order title collate tr` orders one query's
  text as Turkish does (ICU's `tr`: `ç` after `c`, `ı` before `i`). A field
  declared `name text collate tr` compares by it wherever its text is ordered
  -- `order`, `<` and `>` in `where`, `min`, `max`, its `@sorted` index --
  while `=` compares bytes.
- A text field declared `nocase` (`email text nocase @unique`) compares by
  its case fold everywhere -- `=`, `in`, its indexes, `order`, `group`,
  `facet` -- and keeps what was written: `Alice@x.com` is `alice@X.COM`.
- `match` ranks by BM25 and `near` by distance; each decides the order, so
  neither combines with `order`, and the two together need `fuse`, which
  ranks by both. `match ... fuzzy` finds a word typed wrong and
  `match ... prefix` one half typed. Each returns at most 10 000 rows
  (`limit + offset`) and adds a `_score` column.
- After `lookup`, every clause belongs to the child collection and `limit`
  counts children per parent. `on <field>` names the child's field that
  holds the parent's `id` (`get orders lookup items on order_id`); `on <its
  field> = <this one's>` runs the other way (`get items lookup orders on
  id = order_id` attaches each item's order). `required` goes right after
  `on <field>` and keeps only the parents with a matching child -- SQL's
  `exists`; `count` goes before `lookup`.

Every statement, by example:

```fenecql
create collection articles (title text @hash, views int, tags [text], published timestamp @sorted, body text @text, embed vector<4> @hnsw(cosine))
put articles {title: "Rust", views: 10, tags: ["lang"], published: "2026-09-01T10:00:00Z", body: "Ownership and borrowing", embed: [0.1, 0.2, 0.3, 0.4]}
put articles [{title: "Zig", views: 3}, {title: "Go", views: 7}]
get articles select title, views where views < 100 and tags has "lang" order views desc limit 5 offset 5
get articles select title order title collate tr, views desc limit 20
get articles where title in ["Rust", "Go"] count
get articles select title, sum(views), max(published) where tags has "lang" group title order sum(views) desc limit 5
get articles order published desc limit 20 lookup comments on article_id where score >= 4 order published desc limit 3
get articles count lookup comments on article_id required where score = 5
get comments where score >= 4 lookup articles on id = article_id select title
insert into articles (title, views) values ('Nim', 4), ('Odin', 2)
get articles where title like 'R%' and views between 1 and 100 and title not in ["Zig"]
get articles select title, highlight(body) match body "borowing" fuzzy facet tags limit 10
get articles select title where published >= "2026-01-01" near embed $1 limit 3
get articles match body "borrowing" near embed $1 fuse limit 10
get articles match body "borrowing" rerank embed $1 candidates 200 limit 10
explain get articles where views < 100 order published desc limit 5
set articles {views: views + 1} where title = "Rust" require 1
del articles where views > 100000
create index on articles (views) @hash
drop collection articles
```

From JavaScript, `db.from('articles').where('views', '<', 100).order('published', 'desc')
.limit(10).rows()` builds the same FenecQL with every value a parameter, and
`db.live(query, draw)` runs it again after every write to what it reads.
"""


def page_text(body):
    """A docs page as plain Markdown-ish text: headings, code, tables, lists."""
    # A code block may be written with entities or with a raw `<`; both are
    # escaped here and unescaped only by the last pass, or `<name>` and
    # `vector<384>` would read as tags to the pass that strips them.
    def code(m):
        # Any attributes: a block with a label (`data-label`) is code too.
        lang = (re.search(r'data-lang="([^"]*)"', m.group(1)) or [None, ""])[1]
        src = html.escape(html.unescape(m.group(2)), quote=False)
        return f"\n```{lang}\n{src.strip()}\n```\n"

    def row(m):
        cells = re.findall(r"<t[hd][^>]*>([\s\S]*?)</t[hd]>", m.group(1))
        return "| " + " | ".join(re.sub(r"\s+", " ", c).strip() for c in cells) + " |\n"

    def link(m):
        href, text = m.group(1), m.group(2)
        if not re.match(r"https?:|mailto:|#", href):
            # Relative to a docs page, as the browser reads it: `../playground.html`
            # and `/docs/http` both land on their page.
            href = re.sub(r"\.html(?=#|$)", "", urllib.parse.urljoin(f"{SITE}/docs/", href))
        return f"[{text}]({href})"

    # A page's own script (a split page follows a moved fragment) is not
    # text to read.
    t = re.sub(r"<script[\s\S]*?</script>", "", body)
    t = re.sub(r"<!--[\s\S]*?-->", "", t)
    t = re.sub(r"<pre([^>]*)>([\s\S]*?)</pre>", code, t)
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
        kind = ("benchmark" if is_benchmark(key)
                else "compare" if key == "docs/compare" or key.startswith("docs/vs-")
                else "doc")
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


def search_index(docs, wasm, dest, dest_vectors):
    """Writes the documents into a database through the module the site
    ships, twice: its image with their words to `dest`, and with their
    vectors as well to `dest_vectors`; returns the number of documents.
    Fails the build on an index missing or empty, and on a known query that
    does not find its page first (`search.test.mjs`)."""
    if not os.path.exists(wasm):
        raise SystemExit("  the search index needs web/fenec.wasm: run `make wasm`")
    node = shutil.which("node")
    if node is None:
        raise SystemExit("  the search index is written by node, which is not on PATH")
    # The sections' vectors, for the search by meaning: from the committed
    # cache, and the sections it lacks from Workers AI where the build has
    # the credentials. A section without one is found by its words alone,
    # and the build goes on (search-embed.mjs).
    vectors = dest + ".vectors.json"
    embed = subprocess.run([node, os.path.join(ROOT, "search-embed.mjs"), vectors],
                           input=json.dumps(docs), capture_output=True, text=True)
    if embed.returncode != 0:
        raise SystemExit(f"  embedding the sections failed:\n{embed.stderr}")
    said = json.loads(embed.stderr.strip().splitlines()[-1])
    print(f"  search: {said['cached']} vectors from the cache, {said['embedded']} embedded, "
          f"{said['stale']} kept from a section's text before, {said['lacking']} sections without one"
          + (f" ({said['why']})" if said["lacking"] or said["stale"] else ""))
    run = subprocess.run([node, os.path.join(ROOT, "search-index.mjs"), wasm, dest, dest_vectors, vectors],
                         input=json.dumps(docs), capture_output=True, text=True)
    os.remove(vectors)
    if run.returncode != 0:
        raise SystemExit(f"  writing the search index failed:\n{run.stderr}")
    for path in (dest, dest_vectors):
        if not os.path.exists(path) or os.path.getsize(path) == 0:
            raise SystemExit(f"  the search index {path} is missing or empty")
    held = [int(n) for n in run.stdout.split()] or [0]
    if any(n != len(docs) for n in held) or not docs:
        raise SystemExit(f"  the search index holds {held} documents of {len(docs)}")
    test = subprocess.run([node, "--test", "--test-reporter=spec",
                           os.path.join(ROOT, "search.test.mjs"), os.path.join(ROOT, "worker.test.mjs")],
                          capture_output=True, text=True,
                          env={**os.environ, "FENEC_SEARCH_INDEX": dest,
                               "FENEC_SEARCH_VECTORS": dest_vectors, "FENEC_WASM": wasm})
    if test.returncode != 0:
        raise SystemExit(f"  the search does not find what it should:\n{test.stdout}{test.stderr}")
    return held[0]


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


def install_script():
    """dist/install.sh: the installer `curl -fsSL https://fenecdb.com/install.sh
    | sh` fetches, as the repository has it. It is a file of its own and not
    a page, so nothing here rewrites it; what is held is the property that
    makes piping it to a shell safe -- everything outside a function is
    `set -eu`, a comment or an assignment, and the one thing that runs is
    `main "$@"` on the last line, so a download cut short stops before
    anything runs."""
    src = os.path.join(ROOT, "install.sh")
    text = open(src, encoding="utf-8").read()
    lines = text.splitlines()
    if not lines or lines[0] != "#!/bin/sh":
        raise SystemExit("  site/install.sh must start with #!/bin/sh")
    if [l for l in lines if l.strip()][-1] != 'main "$@"':
        raise SystemExit("  site/install.sh must end by calling main")
    if text.count('main "$@"') != 1 or re.search(r"\beval\b", text):
        raise SystemExit("  site/install.sh calls main once and evals nothing")
    # A function runs from its `name() {` at the margin to the `}` at the
    # margin (a heredoc in one has its text at the margin and no `}`). Outside
    # one, a command before main would run on a download cut short at it.
    inside = False
    for n, line in enumerate(lines, 1):
        if inside:
            inside = line != "}"
        elif not line.strip() or line.startswith("#") or line == "set -eu" or line == 'main "$@"':
            continue
        elif re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*\(\) \{", line):
            inside = True
        elif not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*=[^ ;&|<>()$`\\\"']*", line):
            raise SystemExit(
                f"  site/install.sh:{n}: outside a function, only set -eu, comments and "
                f"plain assignments come before main: {line!r}"
            )
    if inside:
        raise SystemExit("  site/install.sh ends inside a function")
    shutil.copy(src, os.path.join(OUT, "install.sh"))


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
    vectors_path = os.path.join(OUT, "search-vectors.fenec")
    held = search_index(docs, os.path.join(REPO, "web", "fenec.wasm"), index_path, vectors_path)
    # Shipped gzipped, and opened in the page with DecompressionStream: a
    # file of no type the edge knows is served as it lies, and as it lies the
    # image is three times what it is gzipped.
    for key, path in (("search.fenec", index_path), ("search-vectors.fenec", vectors_path)):
        image = open(path, "rb").read()
        os.remove(path)
        packed = gziplib.compress(image, 9, mtime=0)
        stem = key[: -len(".fenec")]
        name = f"{stem}.{hashlib.sha256(image).hexdigest()[:10]}.fenec.gz"
        open(os.path.join(OUT, name), "wb").write(packed)
        engine[key] = name
        _, br = compressed_bytes(image)
        print(f"  {stem}: {held} sections, an image of {len(image) / 1024:.1f} KB, "
              f"{len(packed) / 1024:.1f} KB gzipped as served"
              + (f" ({br / 1024:.1f} KB brotli)" if br else ""))
    # The search's modules, each named by its hash and each importing the
    # others by theirs, inside out.
    def module(name, imports):
        body = open(os.path.join(ROOT, name), encoding="utf-8").read()
        for plain, hashed in imports.items():
            body = body.replace(f"'{plain}'", f"'./{hashed}'")
        return emit(name, body)
    search_vectors = module("search-vectors.js", {})
    search_query = module("search-query.js", {"./search-vectors.js": search_vectors})
    search_meaning = module("search-meaning.js", {"./search-vectors.js": search_vectors})
    search_css = emit("search.css", open(os.path.join(ROOT, "search.css"), encoding="utf-8").read())
    search_js = module("search.js", {
        "./search-query.js": search_query, "./search-meaning.js": search_meaning,
        "./search-vectors.js": search_vectors,
        "./search.css": search_css, "./search.fenec": engine["search.fenec"],
        "./search-vectors.fenec": engine["search-vectors.fenec"],
        "./fenec.js": engine.get("fenec.js", "fenec.js"),
        "./fenec.wasm": engine.get("fenec.wasm", "fenec.wasm"),
    })

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
        # One header link is current: Benchmarks for its pages, Compare for the
        # comparison pages, Docs for the rest of the docs, Playground for its own.
        here = ' aria-current="page"'
        compare = key == "docs/compare" or key.startswith("docs/vs-")
        page = page.replace("{{nav_bench}}", here if is_benchmark(key) else "")
        page = page.replace("{{nav_compare}}", here if compare else "")
        page = page.replace("{{nav_docs}}", here if is_docs and not is_benchmark(key)
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
    # The installer, served as plain text: a browser shows it, which is how
    # someone reads it before `curl | sh`, where a .sh would be saved. Five
    # minutes, so a fix reaches the next install and not a day's.
    rules += ["/install.sh",
              "  Content-Type: text/plain; charset=utf-8",
              "  Cache-Control: public, max-age=300, must-revalidate",
              ""]
    open(os.path.join(OUT, "_headers"), "w", encoding="utf-8").write("\n".join(rules))

    # Every module a script imports by name is a file of the build: a name
    # left unhashed is a 404 that only the page that loads it would show --
    # the search's dialog, once, failed to open over one.
    for name in os.listdir(OUT):
        if not name.endswith(".js"):
            continue
        body = open(os.path.join(OUT, name), encoding="utf-8").read()
        for dep in re.findall(r"""(?:from\s*|import\s*\(\s*)["'](\./[^"']+)["']""", body):
            if not os.path.exists(os.path.join(OUT, dep[2:])):
                raise SystemExit(f"  {name} imports {dep}, which the build did not write")

    install_script()

    brief, full = llms_texts()
    open(os.path.join(OUT, "llms.txt"), "w", encoding="utf-8").write(brief)
    open(os.path.join(OUT, "llms-full.txt"), "w", encoding="utf-8").write(full)

    print(f"built {len(pages)} pages -> {os.path.relpath(OUT, REPO)}")

    labels = check_fold_labels()
    if labels:
        print("\n  a fold's summary has to say what is inside it (site/README.md, Conventions):")
        for p in labels:
            print(f"    {p}")
        raise SystemExit("\n  refusing to build: name the folds above")

    problems = check_claims() + check_router_flags()
    if problems:
        print("\n  the docs disagree with what was just built:")
        for p in problems:
            print(f"    {p}")
        if os.environ.get("CI"):
            raise SystemExit("\n  refusing to ship stale numbers (CI)")
        print("  (a warning here, an error under CI)")


STUDIO_HIGHLIGHT = os.path.join(REPO, "studio", "highlight.js")
# The Grafana data source's query editor colours FenecQL by the same rules
# (integrations/grafana): its Monaco tokenizer reads `tokens` from a copy.
GRAFANA_HIGHLIGHT = os.path.join(REPO, "integrations", "grafana", "src", "fenecql", "highlight.js")

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
    for path in (STUDIO_HIGHLIGHT, GRAFANA_HIGHLIGHT):
        open(path, "w", encoding="utf-8").write(highlight_js())
        print(f"wrote {os.path.relpath(path, REPO)}")


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
