#!/usr/bin/env python3
"""Where the browser module's bytes go: `make size-report`, and with
`BASE=<git ref>` what changed against another commit.

The module is built once with its function names kept (target/size-report/).
As served it is those bytes with the custom sections dropped -- identical to
the stripped build, to the byte -- so one build gives both the sizes and the
names. Each function is attributed to where it is written: a method to its
impl block's module, a closure to its function's, and an adapter of the
standard library that runs a closure -- a `fold`, an `extend`, a `call_once`
-- to that closure's, since it holds the closure inlined. The rest of the
standard library is split by what it is for: float formatting, the rest of
`core::fmt`, Unicode tables, the sort, `HashMap`, drop glue, the allocator,
panics.

gzip and brotli move by up to 0.3 KB with nothing but the layout of the bytes
changed -- two lines of comment at the top of engine.rs moved brotli 46 bytes,
through the line numbers the panics carry -- so a delta under that is marked
as noise. The report fails nothing: site/build.py holds the sizes the docs
quote, with the same allowance.

    make size-report                 # the module as it stands
    make size-report BASE=main       # and against main, built in a worktree
"""

import collections
import gzip
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
HOME_CARGO = os.path.expanduser("~/.cargo/bin/cargo")
CARGO = os.environ.get("CARGO") or (HOME_CARGO if os.path.exists(HOME_CARGO) else "cargo")
WASM = os.path.join("wasm32-unknown-unknown", "wasm", "fenec_wasm.wasm")
# What gzip and brotli move by with the layout of the code alone, in bytes.
NOISE = 0.3 * 1024
TOP = 10

# The standard library's code by what it is for, first match wins, over the
# path a function is written in. AREAS are what a size change would go after
# by name; the parts after them hold whatever else of it the module uses.
AREAS = [
    ("std: float formatting", r"^core::num::(imp::)?(flt2dec|bignum|diy_float|fmt)|^core::fmt::float"),
    ("std: float parsing", r"^core::num::(imp::)?dec2flt"),
    ("std: core::fmt, the rest", r"^(core|alloc)::fmt"),
    ("std: Unicode tables", r"^core::unicode|::to_(lower|upper)case$|::map_uppercase_sigma$"),
    ("std: sort", r"^core::slice::sort"),
    ("std: HashMap", r"^hashbrown::|^std::collections::hash|^std::hash::random|"
                     r"^core::hash::(sip|BuildHasher)|^std::sys::random"),
    ("std: drop glue", r"^core::ptr::drop_(in_place|glue)"),
    ("std: allocator", r"^dlmalloc::|^std::sys::alloc|^alloc::alloc|^__rustc::__r(dl|ust)_(alloc|dealloc|realloc)"),
    ("std: panics", r"^(core|std)::panicking|^(core|std)::panic|^__rustc::|_fail(_rt)?$|_failed$"),
]
REST = [
    ("std: Vec, String, Box", r"^alloc::"),
    ("std: core, the rest", r"^core::"),
    ("std: std, the rest", r"^std::"),
    ("compiler_builtins, libm", r"^compiler_builtins::"),
]

BASIC = dict(a="i8", b="bool", c="char", d="f64", e="str", f="f32", h="u8", i="isize",
             j="usize", l="i32", m="u32", n="i128", o="u128", s="i16", t="u16", u="()",
             v="...", x="i64", y="u64", z="!", p="_")
B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"


class V0:
    """Rust's v0 symbol names (RFC 2603), as far as this report reads them:
    a path as text -- with its generic arguments, or with them erased so
    that every copy of a generic reads alike -- where its code is written
    (`home`, crate first), and the homes of the closures among its
    arguments."""

    def __init__(self, sym, erase):
        self.s, self.i, self.erase, self.closures = sym, 0, erase, []

    def peek(self):
        return self.s[self.i] if self.i < len(self.s) else ""

    def eat(self, c):
        if self.peek() == c:
            self.i += 1
            return True
        return False

    def take(self):
        c = self.peek()
        if not c:
            raise ValueError("the name ended early")
        self.i += 1
        return c

    def b62(self):
        if self.eat("_"):
            return 0
        n = 0
        while (c := self.take()) != "_":
            n = n * 62 + B62.index(c)
        return n + 1

    def disambiguator(self):
        return self.b62() + 1 if self.eat("s") else 0

    def ident(self):
        self.eat("u")  # punycode, which is read as written
        start = self.i
        if self.take() != "0":
            while self.peek().isdigit():
                self.i += 1
        n = int(self.s[start:self.i])
        self.eat("_")
        name = self.s[self.i:self.i + n]
        if len(name) != n:
            raise ValueError("an identifier past the end")
        self.i += n
        return name

    def backref(self, parse):
        at = self.i - 1
        to = self.b62()
        if to >= at:
            raise ValueError("a reference forwards")
        back, self.i = self.i, to
        try:
            return parse()
        finally:
            self.i = back

    def list(self, parse):
        out = []
        while not self.eat("E"):
            out.append(parse())
        return out

    def path(self):
        """`(text, home)`."""
        c = self.take()
        if c == "C":
            self.disambiguator()
            name = self.ident()
            return name, [name]
        if c == "M":
            self.disambiguator()
            _, home = self.path()
            return f"<{self.type()}>", home
        if c == "X":
            self.disambiguator()
            _, home = self.path()
            ty = self.type()
            return f"<{ty} as {self.path()[0]}>", home
        if c == "Y":
            ty = self.type()
            trait, home = self.path()
            return f"<{ty} as {trait}>", home
        if c == "N":
            ns = self.take()
            inner, home = self.path()
            n = self.disambiguator()
            name = self.ident()
            if ns.isupper():
                kind = {"C": "closure", "S": "shim"}.get(ns, ns)
                if ns == "C":
                    self.closures.append(home)
                return f"{inner}::{{{kind}{':' + name if name else ''}#{n}}}", home
            return (f"{inner}::{name}", home + [name]) if name else (inner, home)
        if c == "I":
            inner, home = self.path()
            args = self.list(self.generic)
            return (inner if self.erase else f"{inner}<{', '.join(args)}>"), home
        if c == "B":
            return self.backref(self.path)
        raise ValueError(f"a path at {c!r}")

    def generic(self):
        if self.eat("L"):
            self.b62()
            return "'_"
        if self.eat("K"):
            return self.const()
        return self.type()

    def type(self):
        c = self.take()
        if c in BASIC:
            return BASIC[c]
        if c in "CMXYNI":
            self.i -= 1
            return self.path()[0]
        if c == "B":
            return self.backref(self.type)
        if c == "A":
            ty = self.type()
            return f"[{ty}; {self.const()}]"
        if c == "S":
            return f"[{self.type()}]"
        if c == "T":
            tys = self.list(self.type)
            return "(" + ", ".join(tys) + ("," if len(tys) == 1 else "") + ")"
        if c in "RQ":
            if self.eat("L"):
                self.b62()
            return ("&" if c == "R" else "&mut ") + self.type()
        if c in "PO":
            return ("*const " if c == "P" else "*mut ") + self.type()
        if c == "F":
            if self.eat("G"):
                self.b62()
            self.eat("U")
            if self.eat("K") and not self.eat("C"):
                self.ident()
            args = self.list(self.type)
            return f"fn({', '.join(args)}) -> {self.type()}"
        if c == "D":
            if self.eat("G"):
                self.b62()
            traits = []
            while not self.eat("E"):
                traits.append(self.path()[0])
                while self.eat("p"):
                    self.ident()
                    self.type()
            if not self.eat("L"):
                raise ValueError("a dyn without its lifetime")
            self.b62()
            return "dyn " + " + ".join(traits)
        raise ValueError(f"a type at {c!r}")

    def const(self):
        c = self.take()
        if c == "p":
            return "_"
        if c == "B":
            return self.backref(self.const)
        if c in BASIC:
            neg = self.eat("n")
            start = self.i
            while self.peek() and self.peek() in "0123456789abcdef":
                self.i += 1
            digits = self.s[start:self.i]
            if not self.eat("_"):
                raise ValueError("a constant without its end")
            return ("-" if neg else "") + str(int(digits or "0", 16))
        if c in "RQ":
            return "&" + self.const()
        if c in "AT":
            return ("[" if c == "A" else "(") + ", ".join(self.list(self.const)) + ("]" if c == "A" else ")")
        if c == "V":
            name = self.path()[0]
            if self.eat("T"):
                self.list(self.const)
            elif self.eat("S"):
                while not self.eat("E"):
                    self.disambiguator()
                    self.ident()
                    self.const()
            elif not self.eat("U"):
                raise ValueError("a constant's fields")
            return name
        raise ValueError(f"a constant at {c!r}")


def demangle(name):
    """`(text, erased, home)`: the path with its generic arguments and
    without them, and where the code is attributed. A name that is not v0 --
    fenec-wasm's exports, compiler_builtins' `memcmp` -- is its own text,
    with a home made up to match."""
    if name.startswith("_R"):
        try:
            full = V0(name[2:], erase=False)
            text, home = full.path()
            erased = V0(name[2:], erase=True).path()[0]
            if (full.closures and not home[0].startswith("fenec_")
                    and not any(re.search(p, "::".join(home)) for _, p in AREAS)):
                # An adapter -- a `fold`, an `extend`, a closure's `call_once`
                # -- holds the closure it runs inlined: the code is the
                # closure's, fenec's own more often than not.
                home = full.closures[0]
            return text, erased, home
        except (ValueError, IndexError, RecursionError):
            pass
    if name.startswith("fenec_"):
        return name, name, ["fenec_wasm", name]
    return name, name, ["compiler_builtins", name]


def part_of(home):
    if home[0].startswith("fenec_"):
        return "::".join(home[:2]) if len(home) > 2 else home[0]
    path = "::".join(home)
    for label, pattern in AREAS + REST:
        if re.search(pattern, path):
            return label
    return "the rest"


def build(root, target):
    """The module built with its names, as bytes."""
    env = dict(os.environ, CARGO_TARGET_DIR=target, CARGO_PROFILE_WASM_STRIP="false")
    subprocess.run(
        [CARGO, "build", "-q", "-p", "fenec-wasm", "--target", "wasm32-unknown-unknown", "--profile", "wasm"],
        check=True, cwd=root, env=env,
    )
    with open(os.path.join(target, WASM), "rb") as f:
        return f.read()


def uleb(b, p):
    r = s = 0
    while True:
        x = b[p]
        p += 1
        r |= (x & 0x7F) << s
        s += 7
        if x < 0x80:
            return r, p


def read(module):
    """`(served, [(name, body bytes)], data section bytes)`: the module as
    served -- every custom section dropped, which is what the linker's strip
    leaves -- and each function's body under its name."""
    pos, served, bodies, names, data = 8, [module[:8]], [], {}, 0
    imports = 0
    while pos < len(module):
        start, sid = pos, module[pos]
        size, pos = uleb(module, pos + 1)
        end = pos + size
        if sid != 0:
            served.append(module[start:end])
        if sid == 2:  # imported functions take the first indices
            n, p = uleb(module, pos)
            for _ in range(n):
                for _ in range(2):
                    ln, p = uleb(module, p)
                    p += ln
                kind, p = module[p], p + 1
                if kind == 0:
                    _, p = uleb(module, p)
                    imports += 1
                elif kind in (1, 2):
                    p += kind == 1
                    flags, p = module[p], p + 1
                    _, p = uleb(module, p)
                    if flags & 1:
                        _, p = uleb(module, p)
                else:
                    p += 2
        elif sid == 10:
            n, p = uleb(module, pos)
            for _ in range(n):
                ln, p = uleb(module, p)
                bodies.append(ln)
                p += ln
        elif sid == 11:
            data = size
        elif sid == 0:
            ln, p = uleb(module, pos)
            if module[p:p + ln] == b"name":
                p += ln
                while p < end:
                    sub = module[p]
                    sl, p = uleb(module, p + 1)
                    if sub == 1:
                        n, q = uleb(module, p)
                        for _ in range(n):
                            idx, q = uleb(module, q)
                            ln, q = uleb(module, q)
                            names[idx] = module[q:q + ln].decode("utf8", "replace")
                            q += ln
                    p += sl
        pos = end
    return b"".join(served), [(names.get(imports + i, "?"), b) for i, b in enumerate(bodies)], data


def measure(root, target):
    served, bodies, data = read(build(root, target))
    brotli = None
    if shutil.which("brotli"):
        brotli = len(subprocess.run(["brotli", "-c", "-q", "11"], input=served,
                                    stdout=subprocess.PIPE, check=True).stdout)
    fns, generics, copies, parts = (collections.Counter() for _ in range(4))
    for name, size in bodies:
        text, erased, home = demangle(name)
        fns[text] += size
        generics[erased] += size
        copies[erased] += 1
        parts[part_of(home)] += size
    return {
        "raw": len(served),
        "gzip": len(gzip.compress(served, 9, mtime=0)),
        "brotli": brotli,
        "code": sum(size for _, size in bodies),
        "data": data,
        "parts": parts,
        "fns": fns,
        "generics": generics,
        "copies": copies,
    }


def kb(n):
    return "--" if n is None else f"{n / 1024:.1f}"


def signed(n, noise=0):
    if n is None:
        return "--"
    if n == 0:
        return ""
    s = f"{n:+,}".replace(",", " ")
    return f"{s}, noise" if abs(n) < noise else s


def short(text, width=120):
    return text if len(text) <= width else text[:width - 3] + "..."


def report(head, base=None):
    delta = lambda k, noise=0: signed(None if base is None or head[k] is None or base[k] is None
                                      else head[k] - base[k], noise)
    out = ["| | raw | gzip | brotli | code | data |", "| --- | ---: | ---: | ---: | ---: | ---: |",
           f"| KB | {kb(head['raw'])} | {kb(head['gzip'])} | {kb(head['brotli'])} "
           f"| {kb(head['code'])} | {kb(head['data'])} |"]
    if base:
        out.append(f"| against the base, bytes | {delta('raw')} | {delta('gzip', NOISE)} "
                   f"| {delta('brotli', NOISE)} | {delta('code')} | {delta('data')} |")

    out += ["", "Code by where it is written:", ""]
    out += ["| | KB | share |" + (" against the base, bytes |" if base else ""),
            "| --- | ---: | ---: |" + (" ---: |" if base else "")]

    def row(label, v, was):
        line = f"| {label} | {kb(v)} | {100 * v / head['code']:.1f}% |"
        return line + (f" {signed(v - was)} |" if base else "")

    keys = set(head["parts"]) | set(base["parts"] if base else ())
    for mine in (True, False):
        group = sorted((k for k in keys if k.startswith("fenec") == mine), key=lambda k: -head["parts"].get(k, 0))
        total = lambda m: sum(m["parts"].get(k, 0) for k in group)
        out.append(row("**fenec's own**" if mine else "**the standard library's**",
                       total(head), total(base) if base else 0))
        out += [row(k, head["parts"].get(k, 0), base["parts"].get(k, 0) if base else 0) for k in group]

    if base:
        moved = collections.Counter()
        for f in set(head["fns"]) | set(base["fns"]):
            moved[f] = head["fns"].get(f, 0) - base["fns"].get(f, 0)
        moved = [(f, v) for f, v in sorted(moved.items(), key=lambda kv: -abs(kv[1])) if v][:TOP]
        if moved:
            out += ["", f"The {len(moved)} functions that changed most:", "",
                    "| bytes | function |", "| ---: | --- |"]
            out += [f"| {signed(v)} | `{short(f)}` |" for f, v in moved]
    else:
        out += ["", f"The {TOP} largest functions:", "", "| KB | function |", "| ---: | --- |"]
        out += [f"| {kb(v)} | `{short(f)}` |" for f, v in head["fns"].most_common(TOP)]

    many = [(g, v) for g, v in head["generics"].most_common() if head["copies"][g] > 1][:TOP]
    out += ["", f"The {TOP} generics whose copies weigh most:", "",
            "| KB | copies | generic |" + (" copies against the base |" if base else ""),
            "| ---: | ---: | --- |" + (" ---: |" if base else "")]
    for g, v in many:
        line = f"| {kb(v)} | {head['copies'][g]} | `{short(g)}` |"
        if base:
            line += f" {signed(head['copies'][g] - base['copies'].get(g, 0))} |"
        out.append(line)
    return "\n".join(out)


def main():
    """`size_report.py [base] [what to call it]`: CI passes the merge's first
    parent and the name of the branch it is."""
    base_ref = sys.argv[1] if len(sys.argv) > 1 and sys.argv[1] else None
    called = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else base_ref
    out = os.path.join(ROOT, "target", "size-report")
    head = measure(ROOT, os.path.join(out, "head"))
    base = None
    if base_ref:
        tree = os.path.join(out, "tree")
        subprocess.run(["git", "worktree", "remove", "--force", tree], cwd=ROOT,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["git", "worktree", "add", "--detach", "-f", tree, base_ref], check=True,
                       cwd=ROOT, stdout=subprocess.DEVNULL)
        try:
            base = measure(tree, os.path.join(out, "base"))
        finally:
            subprocess.run(["git", "worktree", "remove", "--force", tree], cwd=ROOT)
    title = "## The browser module" + (f", against `{called}`" if base_ref else "")
    text = title + "\n\n" + report(head, base) + "\n"
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)


if __name__ == "__main__":
    main()
