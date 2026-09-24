#!/usr/bin/env python3
"""Writes src/collate/: ICU's root collation, cut into chunks by script, and
the tailorings over it.

    python3 crates/fenec-core/tools/collate/gen.py

dump.c is compiled against macOS's libicucore and prints the collation
elements ICU gives every assigned code point, and every string it has a
contraction for, once for the root collation and once for each tailoring;
this script turns them into the tables collate.rs compares with. Only the
order of the weights matters, so each level's weights are renumbered
densely over the root and every tailoring together: ICU's own are up to 32
bits wide, and a tailoring's letters sit between the root's.

Every assigned code point has an entry but the Hangul syllables, which
collate.rs decomposes into their jamo as ICU does, and the chunk a script's
entries are in is loaded only where it is needed: the browser module
carries `latin` and is handed the rest (collate.rs, web/).

ICU gives every ideograph a primary weight of its own, and most alphabets'
letters: 133 534 of them, which no entry has the bits for. Where
consecutive code points alone give consecutive primaries -- an alphabet in
code point order, most ideographs in radical and stroke order -- they
share one rank and are told apart by their code points, as collate.rs
compares them: 24 931 ranks, and a range of such code points is written
as one entry.

A chunk is the magic, the chunk's number and the stamp of the tables it was
cut with -- a chunk of other tables would order by ranks that mean
something else -- as little-endian u32s, then four sections of u32 words:
its ranges
(`lo`, `hi` half-open and ascending, then where their entries start -- or,
with the top bit, the one entry every code point of the range has), the
entries, the elements the entries that expand point at, and the
contractions -- two or three characters ICU sorts as one, `(first, second,
third or 0, entry)` ascending. A section is its length and then each word
as the difference from the word a record back (3 in the ranges, 4 in the
contractions, 1 in the others), zigzag-folded and in LEB128: 324 KB of
words is 154 KB so, which a native build carries and the browser module
the Latin chunk of. A tailoring is the same shape over only the code
points it changes.
"""

import bisect
import collections
import os
import struct
import subprocess
import sys
import tempfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.normpath(os.path.join(HERE, "..", "..", "src", "collate"))

TAILORINGS = ["tr"]

# Which chunk a code point's entry goes in: the first whose ranges hold it;
# past them `other` for the Basic Multilingual Plane and `smp` for the
# Supplementary Multilingual one. Grouped by what a page holds together --
# a Turkish or a French page needs `latin` alone, one with emoji `symbols`
# -- and `latin` is the one the browser module carries.
CHUNKS = [
    # The one the browser module carries, so the letters a Latin page holds
    # and no more: the phonetic extensions, the combining supplement and
    # Latin Extended-D and -E -- phonetics and medieval letters -- went to
    # `other`, 2.4 KB of the module.
    ("latin", [
        (0x0000, 0x0370),  # ASCII, Latin-1, Extended-A and -B, IPA, modifiers, combining marks
        (0x1E00, 0x1F00),  # Latin Extended Additional: Vietnamese
        (0x2000, 0x2190),  # punctuation, sub- and superscripts, currency, letterlike, number forms
        (0x2C60, 0x2C80),  # Latin Extended-C
        (0xFB00, 0xFB07),  # Latin ligatures
        (0xFE00, 0xFE10),  # variation selectors
        (0xFEFF, 0xFF00),  # the byte order mark
        (0xFFF0, 0x10000),  # specials
    ]),
    ("greek", [(0x0370, 0x0400), (0x1F00, 0x2000)]),
    ("cyrillic", [(0x0400, 0x0530), (0x1C80, 0x1C90), (0x2DE0, 0x2E00), (0xA640, 0xA6A0)]),
    ("middle-east", [
        (0x0530, 0x0900),  # Armenian, Hebrew, Arabic, Syriac, Thaana, NKo, Samaritan, Mandaic
        (0xFB13, 0xFB18),  # Armenian ligatures
        (0xFB1D, 0xFE00),  # Hebrew and Arabic presentation forms
        (0xFE70, 0xFEFF),  # Arabic presentation forms-B
    ]),
    ("indic", [(0x0900, 0x0E00), (0x1CD0, 0x1D00), (0xA830, 0xA840), (0xA8E0, 0xA900)]),
    ("southeast-asia", [
        (0x0E00, 0x1000),  # Thai, Lao, Tibetan
        (0x1000, 0x10A0),  # Myanmar
        (0x1780, 0x1800),  # Khmer
        (0x1950, 0x1AB0),  # Tai Le, New Tai Lue, Khmer symbols, Buginese, Tai Tham
        (0x1B00, 0x1C80),  # Balinese, Sundanese, Batak, Lepcha, Ol Chiki
        (0xA900, 0xAB00),  # Kayah Li, Rejang, Javanese, Cham, Myanmar extended, Tai Viet
        (0xABC0, 0xAC00),  # Meetei Mayek
    ]),
    ("east-asia", [
        (0x1100, 0x1200),  # Hangul Jamo
        (0x2E80, 0x3400),  # CJK radicals, Kangxi, symbols and punctuation, kana, Bopomofo, compatibility jamo
        (0xA000, 0xA4D0),  # Yi
        (0xA960, 0xA980),  # Hangul Jamo Extended-A
        (0xD7B0, 0xD800),  # Hangul Jamo Extended-B
        (0xFE30, 0xFE50),  # CJK compatibility forms
        (0xFF00, 0xFFF0),  # halfwidth and fullwidth forms
    ]),
    ("han", [(0x3400, 0xA000), (0xF900, 0xFB00)]),
    ("han-ext", [(0x20000, 0x40000)]),  # Extensions B to I, the compatibility supplement
    ("symbols", [
        (0x2190, 0x2C00),  # arrows, mathematical operators, technical, box drawing, dingbats
        (0x2E00, 0x2E80),  # supplemental punctuation
        (0x1D000, 0x1D800),  # musical symbols, counting rods, mathematical alphanumerics
        (0x1F000, 0x1FC00),  # game symbols, enclosed alphanumerics, emoji, pictographs
        (0xE0000, 0xE1000),  # tags, variation selectors supplement
    ]),
    ("other", []),
    ("smp", []),
]
OTHER = [n for n, _ in CHUNKS].index("other")
SMP = [n for n, _ in CHUNKS].index("smp")

HANGUL = (0xAC00, 0xD7A4)

# Packing of an entry (see collate.rs): the primary's rank in bits 0..16, the
# secondary's in 16..25, the tertiary's in 25..30, and bit 30 when the
# primary is told apart by the code point -- or, with bit 31, as many
# elements as bits 24..30 count, starting at bits 0..24 of the table's
# `more`. `NONE` is a code point a table has no entry for inside one of its
# ranges: the next table is asked, and past the root the fallback answers.
# 0 is an entry with no elements at all -- a control character, the soft
# hyphen. UNIFORM marks a range whose every code point has one entry.
P_BITS, S_BITS, T_BITS = 16, 9, 5
BY_CODE_POINT = 1 << 30
EXPANDS = 1 << 31
UNIFORM = 1 << 31
NONE = 0x3FFFFFFF
# The most elements one entry expands to: collate.rs queues them in a fixed
# array. U+FDFA, an Arabic ligature of a whole phrase, has 18.
MAX_COUNT = 24
# A run of code points with one entry is written as one range from this
# long: a range costs three words, and splitting the list it sits in three
# more.
MIN_RUN = 8
# ICU's alphabetic-index boundaries: U+FDD1, a noncharacter, before a
# script's first letter. Text never holds them.
INDEX_MARK = 0xFDD1


def dump(locale):
    with tempfile.TemporaryDirectory() as d:
        exe = os.path.join(d, "dump")
        subprocess.run(
            ["cc", "-O1", "-o", exe, os.path.join(HERE, "dump.c"), "-licucore"],
            check=True,
        )
        out = subprocess.run([exe, locale], check=True, capture_output=True, text=True).stdout
    lines = out.splitlines()
    version = lines[0].split()[1]
    rows, conts = {}, {}
    for line in lines[1:]:
        parts = line.split()
        if parts[0] == "c":
            key = tuple(int(x, 16) for x in parts[1].split(","))
            if key[0] != INDEX_MARK:
                conts[key] = merge(int(x, 16) for x in parts[2:])
        else:
            rows[int(parts[0], 16)] = merge(int(x, 16) for x in parts[1:])
    return version, rows, conts


def merge(halves):
    """ICU's elements are 64 bits wide and `ucol_next` hands them out as two
    32-bit halves, the second marked 0xc0 in its low byte and carrying the
    low bits of the primary and the secondary. Returns (primary, secondary,
    tertiary), the tertiary without its case bits: that is how ICU compares
    it when caseFirst is off, which it is for the root and for `tr`."""
    out = []
    for ce in halves:
        if ce & 0xC0 == 0xC0:
            p, s, t = out[-1]
            out[-1] = (p | ce >> 16, s | (ce >> 8 & 0xFF), t | (ce & 0x3F))
        else:
            out.append((ce & 0xFFFF0000, (ce >> 8 & 0xFF) << 8, (ce & 0x3F) << 8))
    return [ce for ce in out if ce != (0, 0, 0)]


def chunk_of(cp):
    for i, (_, ranges) in enumerate(CHUNKS):
        if any(lo <= cp < hi for lo, hi in ranges):
            return i
    if cp < 0x10000:
        return OTHER
    if cp < 0x20000:
        return SMP
    return None


def jamo(cp):
    s = cp - 0xAC00
    out = [0x1100 + s // 588, 0x1161 + s % 588 // 28]
    if s % 28:
        out.append(0x11A7 + s % 28)
    return out


def matched(key, rows, conts):
    """The elements collate.rs gives `key` without a contraction for the
    whole of it: the contraction for its first two characters and the third
    alone, or each character alone."""
    if len(key) == 3 and key[:2] in conts:
        return conts[key[:2]] + rows[key[2]]
    return [ce for cp in key for ce in rows[cp]]


def ranges(points, entries, gap):
    """`points` (ascending) as ranges: a run of at least MIN_RUN code points
    with one entry that tells them apart by code point as one range, the
    rest as few lists as hold them, a gap of up to `gap` filled with `NONE`
    rather than cut, and never across a run. Returns (lo, hi, the run's
    entry or None)."""
    runs, i = [], 0
    while i < len(points):
        j = i
        e = entries[points[i]]
        while (
            e & BY_CODE_POINT
            and not e & EXPANDS
            and j + 1 < len(points)
            and points[j + 1] == points[j] + 1
            and entries[points[j + 1]] == e
        ):
            j += 1
        if j + 1 - i >= MIN_RUN:
            runs.append((points[i], points[j] + 1, e))
        i = j + 1
    starts = [lo for lo, _, _ in runs]
    in_run = {cp for lo, hi, _ in runs for cp in range(lo, hi)}
    out, span = list(runs), None
    for cp in points:
        if cp in in_run:
            continue
        k = bisect.bisect_right(starts, cp)
        crosses = span is not None and k > 0 and starts[k - 1] >= span[1]
        if span is not None and cp - span[1] <= gap and not crosses:
            span[1] = cp + 1
        else:
            span = [cp, cp + 1, None]
            out.append(span)
    return sorted(tuple(r) for r in out)


def varints(words, stride):
    """`words` as differences from the word `stride` back, zigzag-folded,
    in LEB128."""
    out = bytearray()
    for i, w in enumerate(words):
        d = w - (words[i - stride] if i >= stride else 0)
        n = (d << 1) ^ (d >> 63)
        while n >= 0x80:
            out.append(n & 0x7F | 0x80)
            n >>= 7
        out.append(n)
    return bytes(out)


def main():
    version, root, root_conts = dump("")
    tailored = {name: dump(name)[1:] for name in TAILORINGS}
    cps = sorted(cp for cp in root if not HANGUL[0] <= cp < HANGUL[1])

    # The Hangul syllables are their jamo: collate.rs decomposes them.
    for cp in range(*HANGUL):
        for rows in [root] + [r for r, _ in tailored.values()]:
            assert rows[cp] == [ce for j in jamo(cp) for ce in rows[j]], hex(cp)
    assert all(chunk_of(cp) is not None for cp in cps)

    # A contraction ICU has that changes nothing -- its elements are what
    # its characters give without it -- is left out.
    for key in root_conts:
        assert 2 <= len(key) <= 3 and all(cp in root for cp in key), key
    root_conts = {
        k: v for k, v in root_conts.items() if v != matched(k, root, root_conts)
    }
    # What a tailoring changes: the code points whose elements differ from
    # the root's, and the contractions whose elements differ from what the
    # root's contraction or the tailoring's characters would give.
    changed, tail_conts = {}, {}
    for name, (rows, conts) in tailored.items():
        changed[name] = [cp for cp in cps if rows[cp] != root[cp]]
        tail_conts[name] = {
            k: v for k, v in conts.items()
            if v != root_conts.get(k, matched(k, rows, conts))
        }

    # Every weight the root or a tailoring gives, ranked densely, and a
    # sentinel for what no entry covers: unassigned and private code points,
    # where ICU puts them -- below U+FFFD and U+FFFF alone -- by code point
    # (collate.rs).
    rows_all = [root] + [r for r, _ in tailored.values()]
    conts_all = [root_conts] + list(tail_conts.values())
    every = [ce for r in rows_all for cp in cps for ce in r[cp]]
    every += [ce for c in conts_all for v in c.values() for ce in v]
    IMPLICIT_P = 0xFE000000
    above = sorted(p for p in {ce[0] for ce in every} if p >= IMPLICIT_P)
    assert above == [root[0xFFFD][0][0], root[0xFFFF][0][0]], [hex(p) for p in above]
    secs = sorted({ce[1] for ce in every if ce[1]})
    ters = sorted({ce[2] for ce in every if ce[2]})
    S = {w: i + 1 for i, w in enumerate(secs)}
    T = {w: i + 1 for i, w in enumerate(ters)}

    # The code point that alone gives a primary: its only element, the same
    # in every tailoring, and in no expansion or contraction.
    owners = collections.defaultdict(set)
    for rows in rows_all:
        for cp in cps:
            for ce in rows[cp]:
                owners[ce[0]].add(cp if len(rows[cp]) == 1 else None)
    for c in conts_all:
        for v in c.values():
            for ce in v:
                owners[ce[0]].add(None)
    owners.pop(0, None)

    def sole(p):
        o = owners.get(p, ())
        cp = next(iter(o)) if len(o) == 1 else None
        if cp is None or any(r[cp] != root[cp] for r in rows_all[1:]):
            return None
        return cp

    # Consecutive primaries that consecutive code points give alone, with
    # the same secondary and tertiary, share a rank; collate.rs tells them
    # apart by their code points, which order them the same way.
    P, by_code_point, rank, last = {}, set(), 0, None
    for p in sorted(set(owners) | {IMPLICIT_P}):
        cp = sole(p)
        if cp is not None and last is not None and cp == last + 1 and root[cp][0][1:] == root[last][0][1:]:
            by_code_point |= {last, cp}
        else:
            rank += 1
        P[p] = rank
        last = cp
    # The top rank of each level is NONE's, and never a weight's.
    assert rank < (1 << P_BITS) - 1, rank
    assert len(secs) < (1 << S_BITS) - 1, len(secs)
    assert len(ters) < 1 << T_BITS, len(ters)

    def pack(ce, cp=None):
        p, s, t = ce
        e = (P[p] if p else 0) | (S[s] if s else 0) << P_BITS | (T[t] if t else 0) << (P_BITS + S_BITS)
        return e | BY_CODE_POINT if cp in by_code_point else e

    # Every character that can be the second or third of characters sorting
    # as one, as ranges -- a gap of up to 32 closed, since a character held
    # to continue one only costs a comparison a character more
    # (collate.rs). None is ASCII, which collate.rs takes as read.
    continues = []
    for cp in sorted({cp for c in conts_all for k in c for cp in k[1:]}):
        assert cp >= 0x80, hex(cp)
        if continues and cp - continues[-1][1] <= 32:
            continues[-1][1] = cp + 1
        else:
            continues.append([cp, cp + 1])

    def entry(ces, more, cp=None):
        if not ces:
            return 0
        if len(ces) == 1:
            return pack(ces[0], cp)
        assert len(ces) <= MAX_COUNT, ces
        at = len(more)
        more.extend(pack(ce) for ce in ces)
        return EXPANDS | len(ces) << 24 | at

    def blob(tag, number, points, rows, conts, gap):
        entries, more, cont, head = [], [], [], []
        own = {cp: entry(rows[cp], [], cp) for cp in points}
        for lo, hi, uniform in ranges(points, own, gap):
            if uniform is not None:
                head += [lo, hi, UNIFORM | uniform]
                continue
            head += [lo, hi, len(entries)]
            for cp in range(lo, hi):
                entries.append(entry(rows[cp], more, cp) if cp in own else NONE)
        for key, ces in sorted(conts.items(), key=lambda kv: kv[0] + (0,) * (3 - len(kv[0]))):
            cont += [key[0], key[1], key[2] if len(key) == 3 else 0, entry(ces, more)]
        assert len(more) < 1 << 24
        out = bytearray()
        for words, stride in ((head, 3), (entries, 1), (more, 1), (cont, 4)):
            out += varints([len(words) // stride], 1) + varints(words, stride)
        return [tag, number], bytes(out)

    blobs = []
    for i, (name, _) in enumerate(CHUNKS):
        mine = [cp for cp in cps if chunk_of(cp) == i]
        conts = {k: v for k, v in root_conts.items() if chunk_of(k[0]) == i}
        blobs.append((name, len(mine), blob(0x4C434E46, i, mine, root, conts, 32)))  # "FNCL"
    for n, name in enumerate(TAILORINGS):
        rows = tailored[name][0]
        blobs.append((name, len(changed[name]), blob(0x4C544E46, n, changed[name], rows, tail_conts[name], 8)))  # "FNTL"
    stamp = 0
    for _, _, (_, body) in blobs:
        stamp = zlib.crc32(body, stamp)

    os.makedirs(OUT, exist_ok=True)
    for old in os.listdir(OUT):
        if old.endswith(".bin"):
            os.remove(os.path.join(OUT, old))
    sizes = []
    for name, n, (head, body) in blobs:
        data = struct.pack("<3I", head[0], head[1], stamp) + body
        with open(os.path.join(OUT, "%s.bin" % name), "wb") as f:
            f.write(data)
        sizes.append((name, n, len(data)))

    with open(os.path.join(OUT, "layout.rs"), "w") as f:
        w = f.write
        w("// Generated by tools/collate/gen.py from ICU %s's root collation and\n" % version)
        w("// its tailorings; do not edit. Layout and meaning: collate.rs.\n\n")
        w("/// The chunks the root collation's entries are cut into, by number.\n")
        w("pub const CHUNKS: [&str; %d] = [%s];\n\n" % (len(CHUNKS), ", ".join('"%s"' % n for n, _ in CHUNKS)))
        blocks = sorted((lo, hi, i) for i, (_, rs) in enumerate(CHUNKS) for lo, hi in rs)
        w("/// The ranges of code points each chunk holds, ascending. Past them a\n")
        w("/// code point of the Basic Multilingual Plane is `other`'s and one of the\n")
        w("/// Supplementary Multilingual Plane `smp`'s.\n")
        w("pub(super) const BLOCKS: [(u32, u32, u8); %d] = [\n" % len(blocks))
        for lo, hi, i in blocks:
            w("    (0x%04X, 0x%04X, %d),\n" % (lo, hi, i))
        w("];\n")
        w("pub(super) const OTHER: usize = %d;\n" % OTHER)
        w("pub(super) const SMP: usize = %d;\n\n" % SMP)
        w("/// The characters that can be the second or third of characters sorting\n")
        w("/// as one -- and a few beside them -- as half-open ranges.\n")
        w("pub(super) const CONTINUES: [(u32, u32); %d] = [\n" % len(continues))
        for lo, hi in continues:
            w("    (0x%04X, 0x%04X),\n" % (lo, hi))
        w("];\n\n")
        w("/// The tailorings, by number.\n")
        w("pub(super) const TAILORINGS: [&str; %d] = [%s];\n\n" % (len(TAILORINGS), ", ".join('"%s"' % n for n in TAILORINGS)))
        w("/// Every chunk's bytes the build carries: all of them natively, `latin`\n")
        w("/// alone in the browser module, which is handed the rest as a page\n")
        w("/// needs them.\n")
        w("#[cfg(not(target_arch = \"wasm32\"))]\n")
        w("pub(super) static EMBEDDED: [&[u8]; %d] = [\n" % len(CHUNKS))
        for n, _ in CHUNKS:
            w("    include_bytes!(\"%s.bin\"),\n" % n)
        w("];\n")
        w("#[cfg(target_arch = \"wasm32\")]\n")
        w("pub(super) static EMBEDDED: [&[u8]; %d] = [\n" % len(CHUNKS))
        for i, (n, _) in enumerate(CHUNKS):
            w("    %s,\n" % ('include_bytes!("%s.bin")' % n if i == 0 else "&[]"))
        w("];\n\n")
        w("/// The tailorings' bytes, which every build carries.\n")
        w("pub(super) static TAILORED: [&[u8]; %d] = [\n" % len(TAILORINGS))
        for n in TAILORINGS:
            w("    include_bytes!(\"%s.bin\"),\n" % n)
        w("];\n\n")
        w("/// What every chunk and tailoring of these tables carries.\n")
        w("pub(super) const STAMP: u32 = 0x%08X;\n\n" % stamp)
        w("/// Where ICU puts an unassigned or private code point, by code point.\n")
        w("pub(super) const IMPLICIT: u32 = %d;\n" % P[IMPLICIT_P])
        w("/// The secondary and tertiary ranks of a letter without an accent, in\n")
        w("/// lowercase.\n")
        w("pub(super) const COMMON_S: u32 = %d;\n" % S[root[ord("a")][0][1]])
        w("pub(super) const COMMON_T: u32 = %d;\n" % T[root[ord("a")][0][2]])
    # As `cargo fmt --check` wants it, however long a list comes out.
    subprocess.run(["rustfmt", "--edition", "2021", os.path.join(OUT, "layout.rs")], check=True)
    total = 0
    for name, n, size in sizes:
        total += 0 if name in TAILORINGS else size
        print("%-15s %6d code points %8d bytes" % (name, n, size), file=sys.stderr)
    print(
        "ICU %s: %d entries in %d bytes, %d root contractions, %d primary ranks for %d primaries, "
        "%d secondaries, %d tertiaries"
        % (version, len(cps), total, len(root_conts), rank, len(P), len(secs), len(ters)),
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
