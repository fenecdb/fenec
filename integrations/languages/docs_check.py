#!/usr/bin/env python3
"""What the docs show, held to what `languages-test` runs.

    docs_check.py <server url>

Three things, all read from the pages' source (site/content/docs):

* Every FenecQL statement in the first example of each HTTP tab of
  languages.html -- which http.html and quickstart.html show again, taken
  from it by `{{language_tabs}}` -- is in the program of that language in
  this directory. The pages' programs are cut down from these ones (the
  asserts are gone, and a page's does not wrap a main), so a page cannot be
  held to a file byte for byte; a statement reworded on the page and not
  here, or the other way, is what would otherwise drift unseen.
* The curl tab of http.html is run, command by command, against the server
  at <server url> -- one with no token set, since its curls send none, and
  a new file, since its `put`s have no id -- and each answer is the comment
  under the commands it follows.
* The curl steps of quickstart.html are run the same way, after that tab:
  each block of commands is followed by the answers it prints, one a line
  (curl prints them with no newline between), and each command's output must
  be its line. They use a collection of their own, `notes`, so they meet
  none of the rows the tab above wrote.

Standard library only. Run by run-tests.sh.
"""

import html
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PAGES = os.path.join(HERE, "..", "..", "site", "content", "docs")

# Each HTTP tab of languages.html, and the program of this directory its
# first example is cut from (the Go and .NET tabs are the SDKs').
PROGRAMS = {
    "Python": "python/search.py",
    "JavaScript": "javascript/search.mjs",
    "Go": "go-sdk/main.go",
    "Rust": "rust/src/main.rs",
    ".NET": "dotnet-sdk/Program.cs",
    "Java": "java/Search.java",
    "PHP": "php/search.php",
    "Ruby": "ruby/search.rb",
}
# A statement, up to the quote that ends the string it is in. Python writes
# one over two strings, and the page cuts it where the program does.
STATEMENT = re.compile(r"""(?:create collection|put docs|get docs|get nowhere)[^"'\n]*""")


def page(name):
    return open(os.path.join(PAGES, name), encoding="utf-8").read()


def tab(src, name):
    m = re.search(r'<div class="lang" data-name="' + re.escape(name) + r'"[^>]*>\n([\s\S]*?)\n</div>', src)
    if m is None:
        raise SystemExit(f"docs: no `{name}` tab")
    return m.group(1)


def statements():
    src = page("languages.html")
    for name, program in PROGRAMS.items():
        # The first example is the code before the tab's first prose.
        example = html.unescape(tab(src, name).split("\n<p>")[0])
        found = {s.strip() for s in STATEMENT.findall(example)}
        if len(found) < 4:
            raise SystemExit(f"docs: the `{name}` tab holds {len(found)} statements, not the example's")
        code = open(os.path.join(HERE, program), encoding="utf-8").read()
        for s in sorted(found):
            if s not in code:
                raise SystemExit(f"docs: the `{name}` tab sends `{s}`, which {program} does not")
    print(f"docs: the statements of {len(PROGRAMS)} tabs are the programs'")


def commands(block):
    """`(run, text)` and `(answer, text)` in order: a command is the lines
    from one starting `curl` to the one that does not end in a backslash, an
    answer the text of a `# ` line."""
    items, cur = [], None
    for line in block.splitlines():
        if cur is not None:
            cur.append(line)
            if not line.endswith("\\"):
                items.append(("run", "\n".join(cur)))
                cur = None
        elif line.startswith("curl"):
            cur = [line]
            if not line.endswith("\\"):
                items.append(("run", line))
                cur = None
        elif line.startswith("# "):
            items.append(("answer", line[2:]))
    return items


def curl(url):
    block = html.unescape(re.search(r"<pre[^>]*>([\s\S]*?)</pre>", tab(page("http.html"), "curl")).group(1))
    waiting, ran = [], 0
    for kind, text in commands(block):
        if kind == "run":
            out = subprocess.run(["sh", "-c", text.replace("http://127.0.0.1:8080", url)],
                                 capture_output=True, text=True, check=True).stdout
            waiting.append((text, out.rstrip("\n")))
            ran += 1
            continue
        for cmd, out in waiting:
            if out != text:
                raise SystemExit(f"docs: the curl tab says {text!r}, and\n{cmd}\nanswers {out!r}")
        waiting = []
    if waiting or ran < 5:
        raise SystemExit(f"docs: the curl tab ran {ran} commands, the last {len(waiting)} with no answer shown")
    print(f"docs: the curl tab's {ran} commands answer as it says")


# A block of the Quickstart's commands and the block of answers right under
# it. Neither may run past its own </pre>, or a block that is not followed by
# answers (the install, the server's banner) would take the next one in.
QUICKSTART_STEP = re.compile(
    r'<pre data-lang="bash">((?:(?!</pre>)[\s\S])*)</pre>\s*'
    r'<pre data-lang="text" data-label="answers">((?:(?!</pre>)[\s\S])*)</pre>')


def quickstart(url):
    """The Quickstart's curl steps, run: the answers it shows, a line each."""
    ran = 0
    for block, answers in QUICKSTART_STEP.findall(page("quickstart.html")):
        block, answers = html.unescape(block), html.unescape(answers).splitlines()
        runs = [text for kind, text in commands(block) if kind == "run"]
        if len(runs) != len(answers):
            raise SystemExit(f"docs: the Quickstart's block of {len(runs)} commands has {len(answers)} answers")
        for text, answer in zip(runs, answers):
            out = subprocess.run(["sh", "-c", text.replace("http://127.0.0.1:8080", url)],
                                 capture_output=True, text=True, check=True).stdout
            if out != answer:
                raise SystemExit(f"docs: the Quickstart says {answer!r}, and\n{text}\nanswers {out!r}")
            ran += 1
    if ran < 5:
        raise SystemExit(f"docs: the Quickstart's curl steps ran {ran} commands, not its five")
    print(f"docs: the Quickstart's {ran} curl commands answer as it says")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    statements()
    curl(sys.argv[1].rstrip("/"))
    quickstart(sys.argv[1].rstrip("/"))
