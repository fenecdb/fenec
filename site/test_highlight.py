#!/usr/bin/env python3
"""The playground's highlighter (`highlight.js`, as `build.py` writes it)
against the docs' (`build.highlight`), over every FenecQL block the site shows
and a few statements written to reach the edges: the same HTML, byte for byte.

    python3 site/test_highlight.py      # needs node on PATH
"""

import glob
import html
import json
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, ROOT)
import build  # noqa: E402

SAMPLES = [
    'create collection notes (body text @text, n int, embed vector<3> @hnsw(cosine))',
    'put notes {body: "Night at the oasis", n: 1, embed: [0.1, 0.2, 0.3]}',
    'get notes select body\n  match body "oasis" near embed $1 fuse\n  limit 10',
    'set notes {n: n + 1} where id = 7',
    "get notes where body = 'it\\'s' and n >= -1.5e3 -- a comment\n  order n desc",
    'get notes where body ~ "şehir" and note_2 = 12_000 and x1 = 1.25',
    'get notes where body = "unterminated\n  limit 5',
    'get notes where tag = "<b>&amp;</b>" and q = \'"\'',
    'get notes select highlight(body), snippet(body, 12) match body "kuş ağacı"',
    'çok güzel İstanbul 東京 ü2 aéb',
    '',
]


def site_blocks():
    """The body of every `data-lang="fenecql"` block, as build.py reads it."""
    out = []
    for path in glob.glob(os.path.join(ROOT, "content", "**", "*.html"), recursive=True):
        text = open(path, encoding="utf-8").read()
        for m in build.PRE_RE.finditer(text):
            if 'data-lang="fenecql"' in m.group("attrs"):
                out.append(html.unescape(m.group("body")).strip("\n"))
    return out


def main():
    samples = SAMPLES + site_blocks()
    with tempfile.TemporaryDirectory() as tmp:
        mod = os.path.join(tmp, "highlight.mjs")
        open(mod, "w", encoding="utf-8").write(build.highlight_js())
        run = subprocess.run(
            ["node", "--input-type=module", "-e",
             f"import {{ highlight }} from {json.dumps('file://' + mod)};"
             "let s = ''; process.stdin.on('data', (d) => s += d);"
             "process.stdin.on('end', () => process.stdout.write(JSON.stringify("
             "JSON.parse(s).map((c) => highlight(c, 'fenecql')))));"],
            input=json.dumps(samples), capture_output=True, text=True)
    if run.returncode != 0:
        raise SystemExit(run.stderr)
    got = json.loads(run.stdout)
    bad = [(s, g, build.highlight(s, "fenecql"))
           for s, g in zip(samples, got) if g != build.highlight(s, "fenecql")]
    for s, g, want in bad[:5]:
        print(f"differs on {s!r}:\n  js: {g}\n  py: {want}")
    if bad:
        raise SystemExit(f"{len(bad)} of {len(samples)} statements highlight differently")
    # The editor colours a line at a time: no FenecQL token may span one.
    for s in samples:
        whole = build.highlight(s, "fenecql")
        by_line = "\n".join(build.highlight(ln, "fenecql") for ln in s.split("\n"))
        if whole != by_line:
            raise SystemExit(f"a token spans a line in {s!r}: the editor would colour it apart")
    # The rules are written in, not copied: a keyword the docs know is one here.
    assert re.search(r'"fuse"', build.highlight_js())
    print(f"{len(samples)} statements highlight the same in the docs and the editor")


if __name__ == "__main__":
    main()
