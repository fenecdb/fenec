#!/usr/bin/env python3
"""Writes tests/casefold.txt: Python's own case folding, which the fold of a
`nocase` field (src/case.rs) is held to code point by code point.

    python3 crates/fenec-core/tools/casefold.py

`str.casefold()` is Unicode's full case folding -- CaseFolding.txt's C and
F mappings -- from Python's own tables, an implementation case.rs shares no
code with: case.rs composes the standard library's lowercase and uppercase
mappings instead. Every code point the fold changes is a line, the code
point and then what it folds to, in hex; every other one folds to itself.
"""
import os
import sys
import unicodedata

out = [
    "# str.casefold() of Python %s (Unicode %s), for every code point it changes:"
    % (sys.version.split()[0], unicodedata.unidata_version),
    "# the code point, then its fold, in hex. Written by tools/casefold.py.",
]
for cp in range(0x110000):
    c = chr(cp)
    f = c.casefold()
    if f != c:
        out.append("%x %s" % (cp, " ".join("%x" % ord(x) for x in f)))
here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "..", "tests", "casefold.txt"), "w") as fh:
    fh.write("\n".join(out) + "\n")
