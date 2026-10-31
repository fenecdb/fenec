#!/usr/bin/env python3
"""What the recipes' run is held to (ci.sh).

  golden.py normalize                 stdin to stdout, without what changes from run to run
  golden.py check <golden> <output>   the two normalized, compared section by section
                                      (a `== N.` heading); a section the run left out
                                      (no wrangler) is not held to anything
  golden.py pages <golden> <page>...  every line of the golden is on one of the pages

The docs paste these outputs. A line that is in the golden and on no page means
the page was not written from this run; a line that differs means the code now
does something else than the page says. Either way the build of the pages is
what to fix, with `RECIPES_UPDATE=1 sh ci.sh` to write the golden again first.
"""
import html
import re
import sys

VARIES = [
    (re.compile(r"\x1b\[[0-9;]*m"), ""),                       # colour a terminal adds
    (re.compile(r"127\.0\.0\.1:\d+"), "127.0.0.1:PORT"),         # a port the system picked
    (re.compile(r"\bseq \d+"), "seq N"),                         # the change counter moves with every write
    (re.compile(r"\"seq\":\d+"), '"seq":N'),
    (re.compile(r"\bfrom \d+\b"), "from N"),                     # the first write the server keeps
    (re.compile(r"\bafter \d+ s\b"), "after N s"),               # a lease, to the second
]


def normalize(text):
    for pattern, to in VARIES:
        text = pattern.sub(to, text)
    return "\n".join(line.rstrip() for line in text.splitlines()).strip() + "\n"


def sections(text):
    parts = re.split(r"^(== \d+\..*)$", text, flags=re.M)
    head = parts[0]
    return head, {parts[i]: parts[i + 1] for i in range(1, len(parts), 2)}


def main():
    cmd = sys.argv[1]
    if cmd == "normalize":
        sys.stdout.write(normalize(sys.stdin.read()))
    elif cmd == "check":
        golden, got = (normalize(open(p, encoding="utf-8").read()) for p in sys.argv[2:4])
        gh, gs = sections(golden)
        oh, os_ = sections(got)
        bad = []
        if gh.strip() != oh.strip():
            bad.append(("(before the first section)", gh, oh))
        skipped = [t for t, body in os_.items() if body.lstrip().startswith("(skipped")]
        for title, body in os_.items():
            if title in skipped:
                continue
            if title in gs and gs[title].strip() != body.strip():
                bad.append((title, gs[title], body))
            elif title not in gs:
                bad.append((title, "", body))
        # A section only the golden has is one this run did not make: a
        # tool it needs (wrangler) was not there. That is said, not held.
        if not os_ and gs:
            bad.append(("(no sections)", golden, got))
        for title, want, have in bad:
            print(f"differs: {title}")
            import difflib
            for line in difflib.unified_diff(want.splitlines(), have.splitlines(), "golden", "this run", lineterm="", n=1):
                print("  " + line)
        left = [t for t in gs if t not in os_ or t in skipped]
        if left:
            print("not run here (a tool is missing): " + ", ".join(left))
        sys.exit(1 if bad else 0)
    elif cmd == "pages":
        golden = normalize(open(sys.argv[2], encoding="utf-8").read())
        pages = ""
        for p in sys.argv[3:]:
            raw = open(p, encoding="utf-8").read()
            # as the page shows it: tags out, entities back
            pages += html.unescape(re.sub(r"<[^>]+>", "", raw)) + "\n"
        pages = normalize(pages)
        # A line is on a page where it is a pasted line or the whole of a sentence's
        # code: the page's text holds it.
        missing = [l for l in golden.splitlines() if l.strip() and not l.startswith("== ") and l.strip() not in pages]
        for l in missing:
            print(f"on no page: {l}")
        sys.exit(1 if missing else 0)


main()
