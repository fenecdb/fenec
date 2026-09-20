#!/usr/bin/env python3
"""Static site generator for fenecdb.

Standard library only, on purpose: the repo's hard rule is zero dependencies,
and the site is the first thing a reader sees. A generator exists at all
because the page chrome (nav, sidebar, footer) would otherwise be duplicated
across a dozen files and drift.

    python3 site/build.py            # -> site/dist
    python3 site/build.py --serve    # build, then serve on :8788
"""

import hashlib
import html
import os
import re
import shutil
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(ROOT)
OUT = os.path.join(ROOT, "dist")

REPO_URL = "https://github.com/fenecdb/fenec"

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
        digest = hashlib.sha256(body.encode("utf-8")).hexdigest()[:10]
        out_name = f"{stem}.{digest}{ext}"
        open(os.path.join(OUT, out_name), "w", encoding="utf-8").write(body)
        assets[name] = out_name
        return out_name

    worker = open(os.path.join(ROOT, "engine-worker.js"), encoding="utf-8").read()
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

    # The live console on the home page runs the real engine, not a recording.
    for name in ("fenec.js", "fenec.wasm"):
        src = os.path.join(REPO, "web", name)
        if os.path.exists(src):
            shutil.copy(src, os.path.join(OUT, name))
        else:
            print(f"  note: web/{name} missing — run `make wasm` for the live demo")

    # Cloudflare reads this from the asset directory; it is not served itself.
    # Hashed assets can be cached forever because a change gives a new name.
    # fenec.js and fenec.wasm keep stable names, so they revalidate instead —
    # a stale engine would silently be the wrong one.
    rules = ["/*",
             "  X-Content-Type-Options: nosniff",
             "  Referrer-Policy: strict-origin-when-cross-origin",
             "  X-Frame-Options: DENY",
             ""]
    for hashed in sorted(assets.values()):
        rules += [f"/{hashed}", "  Cache-Control: public, max-age=31536000, immutable", ""]
    for stable in ("fenec.js", "fenec.wasm"):
        rules += [f"/{stable}", "  Cache-Control: public, max-age=3600, must-revalidate", ""]
    open(os.path.join(OUT, "_headers"), "w", encoding="utf-8").write("\n".join(rules))

    print(f"built {len(pages)} pages -> {os.path.relpath(OUT, REPO)}")


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
