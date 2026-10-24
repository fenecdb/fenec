#!/usr/bin/env python3
"""AGENTS.md written from CLAUDE.md: `make agents-md`, `--check` in CI.

AGENTS.md is the same working summary for agents that read that name, under
a title and a first line of its own. Copied by hand beside each change, it
fell behind -- some 360 lines of CLAUDE.md missing and ten as they stood
before -- so it is written from CLAUDE.md instead, and CI fails when the
two differ.

`--check` also fails once CLAUDE.md passes MAX_BYTES. Every session and
every agent reads it whole before a word of its task: it had grown to 511 KB,
some 120 000 tokens, each invariant's measurements, history and rejected
alternatives among them, which live in docs/design/ now."""

import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# What CLAUDE.md may weigh: its rules in a few sentences each, the long form
# in docs/design/.
MAX_BYTES = 64_000

# CLAUDE.md's title and the line under it; the rest is shared word for word.
CLAUDE_HEAD = (
    "# CLAUDE.md\n"
    "\n"
    "This file provides guidance to Claude Code (claude.ai/code) when working"
    " with code in this repository.\n"
)
AGENTS_HEAD = (
    "# AGENTS.md\n"
    "\n"
    "This file provides guidance to Codex and other coding agents when working"
    " with code in this repository. It is written from CLAUDE.md by `make"
    " agents-md`: edit that one.\n"
)


def agents():
    claude = (ROOT / "CLAUDE.md").read_text(encoding="utf-8")
    if not claude.startswith(CLAUDE_HEAD):
        sys.exit("CLAUDE.md: its first three lines moved; update tools/agents_md.py")
    return AGENTS_HEAD + claude[len(CLAUDE_HEAD):]


def main():
    want = agents()
    path = ROOT / "AGENTS.md"
    if sys.argv[1:] == ["--check"]:
        have = path.read_text(encoding="utf-8") if path.exists() else ""
        if have != want:
            sys.exit("AGENTS.md differs from CLAUDE.md: run `make agents-md`")
        size = (ROOT / "CLAUDE.md").stat().st_size
        if size > MAX_BYTES:
            sys.exit(
                f"CLAUDE.md is {size} bytes, past {MAX_BYTES}: keep an invariant's"
                " rule there in a few sentences, and its measurements, rejected"
                " alternatives and history in its docs/design/ file"
            )
        return
    if sys.argv[1:]:
        sys.exit("usage: agents_md.py [--check]")
    path.write_text(want, encoding="utf-8")


if __name__ == "__main__":
    main()
