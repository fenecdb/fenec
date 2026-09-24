#!/usr/bin/env python3
"""The browser module's size with each set of its four indexes, as it is
served: `make wasm-sizes`.

Every one of the sixteen sets of `vector`, `text`, `sparse` and `sorted` is
built into target/wasm-sizes and measured raw, under gzip -9, and under
brotli -q 11 when the `brotli` tool is there. The module with all four is
what `make wasm` builds; a page that needs fewer loads one built with
`make wasm FEATURES="text sorted"`.
"""

import gzip
import itertools
import os
import shutil
import subprocess

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
HOME_CARGO = os.path.expanduser("~/.cargo/bin/cargo")
CARGO = os.environ.get("CARGO") or (HOME_CARGO if os.path.exists(HOME_CARGO) else "cargo")
TARGET = os.path.join(ROOT, "target", "wasm-sizes")
OUT = os.path.join(TARGET, "wasm32-unknown-unknown", "wasm", "fenec_wasm.wasm")
FEATURES = ["vector", "text", "sparse", "sorted"]


def brotli(data):
    if not shutil.which("brotli"):
        return None
    done = subprocess.run(["brotli", "-c", "-q", "11"], input=data, capture_output=True, check=True)
    return len(done.stdout)


def kb(n):
    return "--" if n is None else "%.1f" % (n / 1024)


def main():
    rows = []
    for n in range(len(FEATURES), -1, -1):
        for chosen in itertools.combinations(FEATURES, n):
            args = [CARGO, "build", "-q", "-p", "fenec-wasm", "--target", "wasm32-unknown-unknown",
                    "--profile", "wasm", "--no-default-features"]
            if chosen:
                args += ["--features", ",".join(chosen)]
            subprocess.run(args, check=True, cwd=ROOT, env=dict(os.environ, CARGO_TARGET_DIR=TARGET))
            data = open(OUT, "rb").read()
            rows.append((" ".join(chosen) or "none", len(data), len(gzip.compress(data, 9)), brotli(data)))
            print("%-28s %8s %8s %8s KB" % (rows[-1][0], kb(rows[-1][1]), kb(rows[-1][2]), kb(rows[-1][3])), flush=True)
    full = rows[0]
    print()
    print("%-28s %8s %8s %8s   against all four" % ("indexes", "raw", "gzip", "brotli"))
    for name, raw, gz, br in rows:
        less = "" if br is None or full[3] is None else "-%.1f" % ((full[3] - br) / 1024)
        print("%-28s %8s %8s %8s %8s" % (name, kb(raw), kb(gz), kb(br), less))


if __name__ == "__main__":
    main()
