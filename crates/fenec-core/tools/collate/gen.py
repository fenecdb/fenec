#!/usr/bin/env python3
"""Writes src/collate/table.rs from ICU's `tr` collation.

    python3 crates/fenec-core/tools/collate/gen.py

dump.c is compiled against macOS's libicucore and prints the collation
elements ICU gives every code point the table covers; this script turns them
into the ranks `collate.rs` compares. Only the order of the weights matters,
so each level's weights are renumbered densely -- ICU's own are up to 32 bits
wide and would not pack into one `u32` per code point.
"""

import os
import subprocess
import sys
import tempfile
import unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.normpath(os.path.join(HERE, "..", "..", "src", "collate", "table.rs"))

# The code points the table answers for, as half-open ranges: Latin-1,
# Latin Extended-A and -B, the IPA extensions and spacing modifiers -- where
# the lowercase of an Extended-B capital lives, Azerbaijani `ə` among them,
# and Uzbek's `ʻ` -- the combining marks, general punctuation, the currency
# signs, Latin Extended-C for the rest of those case pairs (`Ⱥ` and `ⱥ`),
# and U+FEFF: a byte order mark left at the front of an imported value is
# ignored, not sorted as a symbol. Everything else takes the fallback in
# collate.rs.
RANGES = [
    (0x0000, 0x0370),
    (0x2000, 0x2070),
    (0x20A0, 0x20C1),
    (0x2C60, 0x2C80),
    (0xFEFF, 0xFF00),
]

# Packing of a table entry and of an element in MORE (see collate.rs).
P_BITS, S_BITS, T_BITS = 11, 7, 5
MARK_SHIFT = P_BITS + S_BITS + T_BITS
EXPANDS = 1 << 30
MAX_ELEMENTS = 4


def dump():
    with tempfile.TemporaryDirectory() as d:
        exe = os.path.join(d, "dump")
        subprocess.run(
            ["cc", "-O1", "-o", exe, os.path.join(HERE, "dump.c"), "-licucore"],
            check=True,
        )
        out = subprocess.run([exe], check=True, capture_output=True, text=True).stdout
    lines = out.splitlines()
    version = lines[0].split()[1]
    rows = {}
    for line in lines[1:]:
        parts = line.split()
        key = tuple(int(x, 16) for x in parts[0].split("+"))
        rows[key] = merge(int(x, 16) for x in parts[1:])
    return version, rows


def merge(halves):
    """ICU's elements are 64 bits wide and `ucol_next` hands them out as two
    32-bit halves, the second marked 0xc0 in its low byte and carrying the
    low bits of the primary and the secondary. Returns (primary, secondary,
    tertiary), the tertiary without its case bits: that is how ICU compares
    it when caseFirst is off, which it is for `tr`."""
    out = []
    for ce in halves:
        if ce & 0xC0 == 0xC0:
            p, s, t = out[-1]
            out[-1] = (p | ce >> 16, s | (ce >> 8 & 0xFF), t | (ce & 0x3F))
        else:
            out.append((ce & 0xFFFF0000, (ce >> 8 & 0xFF) << 8, (ce & 0x3F) << 8))
    return [ce for ce in out if ce != (0, 0, 0)]


def main():
    version, rows = dump()
    cps = [cp for lo, hi in RANGES for cp in range(lo, hi)]

    # A precomposed letter must sort as its decomposition does -- that is
    # what lets the table hold one entry per code point and still answer
    # for decomposed text. ICU guarantees it; this checks the table's reading.
    for cp in cps:
        nfd = unicodedata.normalize("NFD", chr(cp))
        if nfd != chr(cp) and all((ord(c),) in rows for c in nfd):
            assert rows[(cp,)] == expand([ord(c) for c in nfd], rows), hex(cp)

    # A contraction: an ASCII character and a mark that do not sort as the
    # two one after the other. For `tr` these are the decomposed Turkish
    # letters, and each sorts as a precomposed letter the table holds, which
    # is all collate.rs knows how to replace one with.
    contractions = []
    for key, ces in sorted(rows.items()):
        if len(key) == 2 and ces != rows[(key[0],)] + rows[(key[1],)]:
            to = [cp for cp in cps if rows[(cp,)] == ces]
            assert to, [hex(c) for c in key]
            contractions.append((key[0], key[1], to[0]))

    prims = sorted({ce[0] for cp in cps for ce in rows[(cp,)] if ce[0]})
    secs = sorted({ce[1] for cp in cps for ce in rows[(cp,)] if ce[1]})
    ters = sorted({ce[2] for cp in cps for ce in rows[(cp,)] if ce[2]})
    P = {w: i + 1 for i, w in enumerate(prims)}
    S = {w: i + 1 for i, w in enumerate(secs)}
    T = {w: i + 1 for i, w in enumerate(ters)}
    assert len(prims) < 1 << P_BITS and len(secs) < 1 << S_BITS and len(ters) < 1 << T_BITS

    common_t = T[0x0500]

    def pack(ce):
        p, s, t = ce
        return (P[p] if p else 0) | S[s] << P_BITS | T[t] << (P_BITS + S_BITS)

    table, more = [], []
    for cp in cps:
        ces = rows[(cp,)]
        if not ces:
            table.append(0)
        elif len(ces) == 1:
            table.append(pack(ces[0]))
        elif len(ces) == 2 and ces[1][0] == 0 and T[ces[1][2]] == common_t:
            # A letter and one mark: the shape of most of the range.
            table.append(pack(ces[0]) | S[ces[1][1]] << MARK_SHIFT)
        else:
            assert len(ces) <= MAX_ELEMENTS, hex(cp)
            table.append(EXPANDS | len(ces) << 16 | len(more))
            more.extend(pack(ce) for ce in ces)

    zero = rows[(ord("0"),)][0][0]
    symbols = P[max(p for p in prims if p < zero)]

    with open(OUT, "w") as f:
        w = f.write
        w("// Generated by tools/collate/gen.py from ICU %s's `tr` collation; do\n" % version)
        w("// not edit. Layout and meaning: collate.rs.\n\n")
        w("/// The code points `TABLE` covers, as half-open ranges, in its order.\n")
        w("pub(super) const RANGES: [(u32, u32); %d] = [\n" % len(RANGES))
        for lo, hi in RANGES:
            w("    (0x%04X, 0x%04X),\n" % (lo, hi))
        w("];\n\n")
        emit(w, "/// One entry per code point in `RANGES`.", "TABLE", table)
        emit(w, "/// The elements of the entries marked `EXPANDS`.", "MORE", more)
        w("/// An ASCII character and a combining mark that sort as one letter.\n")
        w("pub(super) const CONTRACTIONS: [(char, char, char); %d] = [\n" % len(contractions))
        for b, m, to in contractions:
            w("    (%s, '\\u{%04x}', '\\u{%04x}'),\n" % (char_lit(b), m, to))
        w("];\n\n")
        w("/// The primary rank of the last symbol before the digits: where the\n")
        w("/// symbols the table does not cover go.\n")
        w("pub(super) const SYMBOLS: u32 = %d;\n" % symbols)
        w("/// Above every primary rank in the table: where the letters it does not\n")
        w("/// cover go.\n")
        w("pub(super) const LETTERS: u32 = %d;\n" % (len(prims) + 1))
        w("/// The secondary rank of a letter without an accent.\n")
        w("pub(super) const COMMON_S: u32 = %d;\n" % S[0x0500])
        w("/// The tertiary ranks of a lowercase and an uppercase letter.\n")
        w("pub(super) const COMMON_T: u32 = %d;\n" % common_t)
        w("pub(super) const UPPER_T: u32 = %d;\n" % T[rows[(ord("A"),)][0][2]])
    print(
        "%s: %d entries, %d more, %d contractions; %d primaries, %d secondaries, %d tertiaries"
        % (OUT, len(table), len(more), len(contractions), len(prims), len(secs), len(ters)),
        file=sys.stderr,
    )


def expand(seq, rows):
    """The elements of a decomposed sequence, contractions applied."""
    out, i = [], 0
    while i < len(seq):
        if i + 1 < len(seq) and (seq[i], seq[i + 1]) in rows:
            pair = rows[(seq[i], seq[i + 1])]
            if pair != rows[(seq[i],)] + rows[(seq[i + 1],)]:
                out += pair
                i += 2
                continue
        out += rows[(seq[i],)]
        i += 1
    return out


def char_lit(cp):
    c = chr(cp)
    return "'\\''" if c == "'" else "'\\\\'" if c == "\\" else "'%s'" % c


def emit(w, doc, name, vals):
    w("%s\n" % doc)
    w("pub(super) static %s: [u32; %d] = [\n" % (name, len(vals)))
    for i in range(0, len(vals), 8):
        w("    %s,\n" % ", ".join("0x%08x" % v for v in vals[i : i + 8]))
    w("];\n\n")


if __name__ == "__main__":
    main()
