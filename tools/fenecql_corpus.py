#!/usr/bin/env python3
"""Every FenecQL text the repository holds, parsed by this tree and by
another: `make fenecql-corpus BASE=origin/main`.

The rule it holds (docs/design/fenecql.md, *SQL spellings*): a change to the
grammar reads no text that parsed before as another tree. A named statement
is stored as its text and read again at every open, its tree's digest held
to the one stored (docs/design/named-statements.md), so a text read
otherwise would be an API that changed under its callers.

The texts are harvested from the files git tracks:

- the docs' `<pre data-lang="fenecql">` and `"sql"` blocks, and every inline
  `<code>` that begins a statement (site/content);
- Markdown's ```fenecql and ```sql blocks, and every inline code span that
  begins a statement (docs/, README.md, CLAUDE.md, each integration's);
- every string of the golden files and the sync's scenarios that begins a
  statement (integrations/*.json);
- every string literal of the Rust, JS, TS, Python, Go, C#, Swift, Kotlin
  and Dart sources that begins a statement, and every inline code span of
  their comments that does.

A block is a text whole, and each of its lines that begins a statement a
text of its own. Each text is parsed with `parse` and `parse_exact` by
fenec-ql's `corpus` example, built in this tree and in BASE's (a worktree
in a temporary directory, the example copied in); the trees are compared
as their `Debug` forms. The report counts the texts, those whose tree
changed -- which must be none -- those that parse now and were refused,
those refused now that parsed, and those refused in other words.

    python3 tools/fenecql_corpus.py origin/main       # compare
    python3 tools/fenecql_corpus.py --list            # the texts, as JSON lines
"""

import html
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
HOME_CARGO = os.path.expanduser("~/.cargo/bin/cargo")
CARGO = os.environ.get("CARGO") or (HOME_CARGO if os.path.exists(HOME_CARGO) else "cargo")
EXAMPLE = os.path.join("crates", "fenec-ql", "examples", "corpus.rs")

# The words a statement begins with: FenecQL's, SQL's that it reads, and
# those that are refused naming what to write. A text whose first word is
# another is no statement.
WORDS = {
    "get", "select", "put", "insert", "set", "update", "del", "delete", "create",
    "drop", "alter", "explain", "collections", "describe", "compact", "truncate",
    "begin", "start", "commit", "rollback", "call", "statements",
}

SOURCES = {
    ".rs": ('"',),
    ".js": ('"', "'", "`"),
    ".mjs": ('"', "'", "`"),
    ".cjs": ('"', "'", "`"),
    ".ts": ('"', "'", "`"),
    ".tsx": ('"', "'", "`"),
    ".py": ('"', "'"),
    ".go": ('"', "`"),
    ".cs": ('"',),
    ".swift": ('"',),
    ".kt": ('"',),
    ".dart": ('"', "'"),
}


def begins_statement(text):
    word = re.match(r"\s*([A-Za-z_]+)", text)
    return bool(word) and word.group(1).lower() in WORDS


def tracked():
    out = subprocess.run(["git", "ls-files", "-z"], cwd=ROOT, check=True,
                         stdout=subprocess.PIPE).stdout.decode()
    return [p for p in out.split("\0") if p]


def read(path):
    try:
        with open(os.path.join(ROOT, path), encoding="utf-8") as f:
            return f.read()
    except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
        return None


def block(add, text, where):
    """A block whole, and each of its lines that begins a statement."""
    add(text, where)
    for line in text.splitlines():
        if begins_statement(line):
            add(line, where)


def spans(add, text, where):
    """Each inline code span, between single backticks, that begins one."""
    for m in re.finditer(r"(?<!`)`([^`\n]+)`(?!`)", text):
        if begins_statement(m.group(1)):
            add(m.group(1), where)


def from_html(add, text, where):
    for m in re.finditer(r'<pre data-lang="(fenecql|sql)"[^>]*>(.*?)</pre>', text, re.S):
        body = re.sub(r"<[^>]+>", "", m.group(2))
        block(add, html.unescape(body), where)
    for m in re.finditer(r"<code>(.*?)</code>", text, re.S):
        code = html.unescape(re.sub(r"<[^>]+>", "", m.group(1)))
        if begins_statement(code):
            add(code, where)


def from_markdown(add, text, where):
    for m in re.finditer(r"^```(fenecql|sql)\s*\n(.*?)^```", text, re.S | re.M):
        block(add, m.group(2), where)
    spans(add, re.sub(r"^```.*?^```", "", text, flags=re.S | re.M), where)


def from_json(add, value, where):
    if isinstance(value, str):
        if begins_statement(value):
            block(add, value, where)
    elif isinstance(value, list):
        for v in value:
            from_json(add, v, where)
    elif isinstance(value, dict):
        for v in value.values():
            from_json(add, v, where)


def unescape(raw, quote):
    """A string literal's text, its common escapes read; a Rust line ending
    in a backslash joined to the next line's text."""
    out, i = [], 0
    while i < len(raw):
        c = raw[i]
        if c == "\\" and i + 1 < len(raw):
            n = raw[i + 1]
            if n == "\n":
                i += 2
                while i < len(raw) and raw[i] in " \t":
                    i += 1
                continue
            out.append({"n": "\n", "t": "\t", "r": "\r", "0": "\0"}.get(n, n))
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)


def from_source(add, text, where, quotes):
    """String literals that begin a statement, and the comments' code spans."""
    rust = where.endswith(".rs")
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        # Comments: a line's, or a block's; their code spans are texts too.
        if text.startswith("//", i) or (c == "#" and where.endswith(".py")):
            end = text.find("\n", i)
            end = n if end < 0 else end
            spans(add, text[i:end], where)
            i = end
            continue
        if text.startswith("/*", i):
            end = text.find("*/", i + 2)
            end = n if end < 0 else end + 2
            spans(add, text[i:end], where)
            i = end
            continue
        # Rust's raw strings: r"..", r#".."#.
        if rust and c == "r" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            m = re.match(r'r(#*)"', text[i:])
            if m:
                close = '"' + m.group(1)
                start = i + len(m.group(0))
                end = text.find(close, start)
                if end < 0:
                    break
                body = text[start:end]
                if begins_statement(body):
                    block(add, body, where)
                i = end + len(close)
                continue
        # Rust's character literals and lifetimes hold no statement.
        if c in quotes:
            triple = text.startswith(c * 3, i) and c != "`"
            close = c * 3 if triple else c
            j = i + len(close)
            while j < n:
                if text[j] == "\\" and c != "`":
                    j += 2
                    continue
                if text.startswith(close, j):
                    break
                if not triple and c != "`" and text[j] == "\n" and not rust:
                    break
                j += 1
            body = text[i + len(close):j]
            if begins_statement(body):
                body = body if c == "`" else unescape(body, c)
                block(add, body, where)
            i = j + len(close)
            continue
        i += 1


def harvest():
    texts, seen = [], set()

    def add(text, where):
        t = text.strip()
        if t and t not in seen:
            seen.add(t)
            texts.append({"text": t, "from": where})

    for path in tracked():
        ext = os.path.splitext(path)[1]
        if path.startswith(("target/", "node_modules/")) or "/node_modules/" in path:
            continue
        if ext == ".html" and path.startswith("site/"):
            body = read(path)
            if body:
                from_html(add, body, path)
        elif ext == ".md":
            body = read(path)
            if body:
                from_markdown(add, body, path)
        elif ext == ".json" and path.startswith("integrations/"):
            body = read(path)
            try:
                from_json(add, json.loads(body), path) if body else None
            except ValueError:
                pass
        elif ext in SOURCES and not path.endswith(".min.js"):
            body = read(path)
            if body:
                from_source(add, body, path, SOURCES[ext])
    return texts


def run(tree, texts, target):
    """Each text's trees as `tree`'s fenec-ql reads it, in order."""
    if os.path.abspath(tree) != ROOT:
        os.makedirs(os.path.join(tree, os.path.dirname(EXAMPLE)), exist_ok=True)
        shutil.copy(os.path.join(ROOT, EXAMPLE), os.path.join(tree, EXAMPLE))
    env = dict(os.environ, CARGO_TARGET_DIR=target)
    subprocess.run([CARGO, "build", "-q", "--release", "-p", "fenec-ql", "--example", "corpus"],
                   cwd=tree, env=env, check=True)
    exe = os.path.join(target, "release", "examples", "corpus")
    data = "\n".join(json.dumps(t["text"], ensure_ascii=False) for t in texts) + "\n"
    out = subprocess.run([exe], input=data.encode("utf-8"), stdout=subprocess.PIPE, check=True).stdout
    lines = out.decode().split("\n")[: len(texts)]
    assert len(lines) == len(texts), (len(lines), len(texts))
    return lines


def main():
    args = sys.argv[1:]
    texts = harvest()
    if args and args[0] == "--list":
        for t in texts:
            print(json.dumps(t, ensure_ascii=False))
        return
    base_ref = args[0] if args else "origin/main"
    target = os.path.join(ROOT, "target", "fenecql-corpus")
    head = run(ROOT, texts, os.path.join(target, "head"))
    tree = os.path.join(tempfile.mkdtemp(prefix="fenec-corpus-base-"), "tree")
    subprocess.run(["git", "worktree", "add", "--detach", "-f", tree, base_ref], check=True,
                   cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        base = run(tree, texts, os.path.join(target, "base"))
    finally:
        subprocess.run(["git", "worktree", "remove", "--force", tree], cwd=ROOT)
        shutil.rmtree(os.path.dirname(tree), ignore_errors=True)
    changed, now_parse, now_refused, reworded, same = [], [], [], [], 0
    parses = 0
    for t, h, b in zip(texts, head, base):
        hp, he = h.split("\t")
        bp, be = b.split("\t")
        for (hx, bx) in ((hp, bp), (he, be)):
            parses += 1
            h_ok, b_ok = hx.startswith("ok "), bx.startswith("ok ")
            if hx == bx:
                same += 1
            elif h_ok and b_ok:
                changed.append(t)
            elif h_ok:
                now_parse.append(t)
            elif b_ok:
                now_refused.append(t)
            else:
                reworded.append(t)
    uniq = lambda v: list({id(t): t for t in v}.values())
    changed, now_parse, now_refused, reworded = map(uniq, (changed, now_parse, now_refused, reworded))
    print(f"## FenecQL's corpus, against `{base_ref}`\n")
    print(f"{len(texts)} texts, {parses} parses (`parse` and `parse_exact`): {same} read alike.\n")
    print(f"- trees changed: {len(changed)}")
    print(f"- refused now, parsed before: {len(now_refused)}")
    print(f"- parse now, refused before: {len(now_parse)}")
    print(f"- refused in other words: {len(reworded)}")
    for title, group in (("Trees changed", changed), ("Refused now, parsed before", now_refused),
                         ("Parse now, refused before", now_parse),
                         ("Refused in other words", reworded)):
        if group:
            print(f"\n### {title}\n")
            for t in group:
                print(f"- `{t['from']}`: {json.dumps(t['text'], ensure_ascii=False)[:200]}")
    if changed or now_refused:
        sys.exit(1)


if __name__ == "__main__":
    main()
