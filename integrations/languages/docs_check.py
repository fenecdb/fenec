#!/usr/bin/env python3
"""What the docs show, held to what `languages-test` runs.

    docs_check.py <server url>            the checks below that need a server
    docs_check.py programs                the Quickstart's programs are their files
    docs_check.py seed <server url>       the Quickstart's step 2, run: a server for a program
    docs_check.py prints <output file>    what a program printed is what the Quickstart shows

Four things, all read from the pages' source (site/content/docs):

* Every FenecQL statement in the first example of each HTTP tab of
  languages.html -- which http.html shows again, taken from it by
  `{{language_tabs}}` -- is in the program of that language in this
  directory. The pages' programs are cut down from these ones (the asserts
  are gone, and a page's does not wrap a main), so a page cannot be held to
  a file byte for byte; a statement reworded on the page and not here, or
  the other way, is what would otherwise drift unseen.
* The curl tab of http.html is run, command by command, against the server
  at <server url> -- one with no token set, since its curls send none, and
  a new file, since its `put`s have no id -- and each answer is the comment
  under the commands it follows.
* The curl steps of quickstart.html are run the same way, after that tab:
  each block of commands is followed by the answers it prints, one a line
  (curl prints them with no newline between), and each command's output must
  be its line. They use a collection of their own, `notes`, so they meet
  none of the rows the tab above wrote.
* The Quickstart's programs, one for each language of its fourth step, are
  the files of quickstart/ byte for byte (`programs`), since each is a whole
  program a reader pastes. run-tests.sh runs those files against a server
  `seed` has given the notes of steps 2 and 3, and what each prints is the
  block the page shows under its tabs (`prints`): the same three lines in
  every language.

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


def steps():
    """The Quickstart's curl steps: each block's commands and the answers
    shown under it, a line for each."""
    out = []
    for block, answers in QUICKSTART_STEP.findall(page("quickstart.html")):
        block, answers = html.unescape(block), html.unescape(answers).splitlines()
        runs = [text for kind, text in commands(block) if kind == "run"]
        if len(runs) != len(answers):
            raise SystemExit(f"docs: the Quickstart's block of {len(runs)} commands has {len(answers)} answers")
        out.append(list(zip(runs, answers)))
    return out


def run_step(url, pairs):
    """Each command of a step against `url`, its output the answer shown."""
    for text, answer in pairs:
        out = subprocess.run(["sh", "-c", text.replace("http://127.0.0.1:8080", url)],
                             capture_output=True, text=True, check=True).stdout
        if out != answer:
            raise SystemExit(f"docs: the Quickstart says {answer!r}, and\n{text}\nanswers {out!r}")
    return len(pairs)


def quickstart(url):
    """The Quickstart's curl steps, run: the answers it shows, a line each."""
    ran = sum(run_step(url, pairs) for pairs in steps())
    if ran < 6:
        raise SystemExit(f"docs: the Quickstart's curl steps ran {ran} commands, not its six")
    print(f"docs: the Quickstart's {ran} curl commands answer as it says")


def seed(url):
    """The Quickstart's step 2 alone: the notes a program of step 4 reads."""
    run_step(url, steps()[0])


# The programs of the Quickstart's fourth step, by tab: the file of
# quickstart/ each is, and the label the page gives it.
QUICKSTART_PROGRAMS = {
    "Python": ("python/notes.py", "notes.py"),
    "JavaScript": ("javascript/notes.mjs", "notes.mjs"),
    "Go": ("go/main.go", "main.go"),
    "Rust": ("rust/src/main.rs", "src/main.rs"),
    ".NET": ("dotnet/Program.cs", "Program.cs"),
    "Java": ("java/Notes.java", "Notes.java"),
    "PHP": ("php/notes.php", "notes.php"),
    "Ruby": ("ruby/notes.rb", "notes.rb"),
}


def programs():
    """Each tab's program on the Quickstart is its file, to the byte."""
    src = page("quickstart.html")
    for name, (path, label) in QUICKSTART_PROGRAMS.items():
        found = re.findall(r'<pre data-lang="[a-z]+" data-label="' + re.escape(label) + r'">((?:(?!</pre>)[\s\S])*)</pre>',
                           tab(src, name))
        if len(found) != 1:
            raise SystemExit(f"docs: the Quickstart's `{name}` tab has {len(found)} blocks labelled {label}, not one")
        want = open(os.path.join(HERE, "quickstart", path), encoding="utf-8").read().rstrip("\n")
        if html.unescape(found[0]) != want:
            raise SystemExit(f"docs: the Quickstart's `{name}` program is not quickstart/{path}")
    print(f"docs: the Quickstart's {len(QUICKSTART_PROGRAMS)} programs are the files that run")


PRINTS = re.compile(r'<pre data-lang="text" data-label="prints">((?:(?!</pre>)[\s\S])*)</pre>\s*'
                    r'<p>Java, PHP and Ruby need no package')


def prints(output):
    """What a program printed is what the page shows under the tabs."""
    found = PRINTS.search(page("quickstart.html"))
    if found is None:
        raise SystemExit("docs: the Quickstart shows no output under its program tabs")
    want = html.unescape(found.group(1)).strip()
    got = open(output, encoding="utf-8").read().strip()
    if got != want:
        raise SystemExit(f"docs: the Quickstart's programs print\n{want}\nand this one printed\n{got}")


if __name__ == "__main__":
    args = sys.argv[1:]
    if args == ["programs"]:
        programs()
    elif len(args) == 2 and args[0] == "seed":
        seed(args[1].rstrip("/"))
    elif len(args) == 2 and args[0] == "prints":
        prints(args[1])
    elif len(args) == 1:
        statements()
        curl(args[0].rstrip("/"))
        quickstart(args[0].rstrip("/"))
        programs()
    else:
        raise SystemExit(__doc__)
