#!/usr/bin/env python3
"""tools/image-cargo.sh -- the build both images' binaries go through --
held to what it is for: binaries made from the sources a build was given,
whatever their mtimes, and a build that fails when they are not.

    python3 deploy/test/image_cargo.py      # make deploy-check runs it

A workspace of two crates, one reading a file outside itself through
include_bytes! as fenec-http reads the studio's, is built in rust:alpine
by the script, its /work a volume of the test's own as the Dockerfiles'
is a cache mount:

- a tree whose every file is older than the last build, one of them
  changed, builds the change: with target/ alone in the cache, as the
  images were built before, cargo compiled nothing and a branch's image
  carried main's fenec-shard;
- the same tree again compiles nothing;
- a file changed in the kept tree with an old time, the context changed
  alike -- cargo's freshness lied to, whatever the cause -- fails the
  check, which names the file, and the build after starts clean;
- a build whose library compiles and whose binary does not, then the
  binary fixed, passes the check: a unit is recorded as it is compiled,
  not when the whole build succeeds.

Each fails with its part taken out: times kept in the copy (`rsync -t`,
COPY's way), the check, or a unit's record kept only once the whole build
has succeeded.
"""

import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(ROOT, "tools", "image-cargo.sh")
# The build stage's own first layers, so the layer is shared with it.
BASE = "FROM rust:alpine\nRUN apk add --no-cache musl-dev rsync\n"
# Its own names, so two runs at once remove only their own.
IMAGE = f"fenec-image-cargo-test-{os.getpid()}"
VOLUME = IMAGE
# 2020-01-01: older than any build the test makes.
OLD = 1577836800


def lib(n):
    return ("pub const N: u32 = %d;\n"
            "pub const WORD: &[u8] = include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), "
            "\"/../assets/word.txt\"));\n" % n)


def main_rs(extra=""):
    return ("fn main() {\n"
            "    println!(\"{} {}\", a::N, String::from_utf8_lossy(a::WORD).trim());\n"
            "%s}\n" % extra)


def write(tree, files):
    for path, body in files.items():
        p = os.path.join(tree, path)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(body)


def age(tree):
    """Every file of the tree as old as a worktree made long ago."""
    for d, _, names in os.walk(tree):
        for n in names:
            os.utime(os.path.join(d, n), (OLD, OLD))


def build(tree):
    p = subprocess.run(
        ["docker", "run", "--rm", "-v", f"{tree}:/src:ro", "-v", f"{VOLUME}:/work",
         "-v", f"{SCRIPT}:/tools/image-cargo.sh:ro", "-w", "/src", IMAGE,
         "sh", "-c", "/tools/image-cargo.sh /out --release -p b && /out/b"],
        capture_output=True, text=True, timeout=600)
    return p.returncode, p.stdout.strip(), p.stderr


def expect(what, got, want, err):
    if got != want:
        raise SystemExit(f"image-cargo: {what}: got {got!r}, want {want!r}\n{err}")
    print(f"ok  {what}: {got}")


def main():
    subprocess.run(["docker", "build", "-q", "-t", IMAGE, "-"], input=BASE, text=True,
                   check=True, capture_output=True, timeout=600)
    subprocess.run(["docker", "volume", "create", VOLUME], check=True, capture_output=True)
    tree = tempfile.mkdtemp(prefix="image-cargo-")
    try:
        write(tree, {
            "Cargo.toml": '[workspace]\nresolver = "2"\nmembers = ["a", "b"]\n',
            "a/Cargo.toml": '[package]\nname = "a"\nversion = "0.1.0"\nedition = "2021"\n',
            "a/src/lib.rs": lib(1),
            "b/Cargo.toml": ('[package]\nname = "b"\nversion = "0.1.0"\nedition = "2021"\n'
                             '[dependencies]\na = { path = "../a" }\n'),
            "b/src/main.rs": main_rs(),
            "assets/word.txt": "one\n",
        })
        code, out, err = build(tree)
        expect("the first build", (code, out), (0, "1 one"), err)

        # Every file older than that build, one changed: the change is built.
        write(tree, {"a/src/lib.rs": lib(2)})
        age(tree)
        code, out, err = build(tree)
        expect("a change in files older than the last build", (code, out), (0, "2 one"), err)

        code, out, err = build(tree)
        expect("the same tree again", (code, out, "Compiling" in err), (0, "2 one", False), err)

        # Cargo's freshness lied to: the kept tree changed with an old time,
        # the context alike, so the copy finds nothing to do.
        write(tree, {"assets/word.txt": "three\n"})
        age(tree)
        subprocess.run(
            ["docker", "run", "--rm", "-v", f"{VOLUME}:/work", IMAGE, "sh", "-c",
             "printf 'three\\n' > /work/src/assets/word.txt && "
             "touch -t 202001010000 /work/src/assets/word.txt"],
            check=True, capture_output=True, timeout=120)
        code, out, err = build(tree)
        named = "word.txt: FAILED" in err and "compiled from other bytes" in err
        expect("a unit kept from other bytes", (code, out, named), (1, "", True), err)

        code, out, err = build(tree)
        expect("the build after it", (code, out), (0, "2 three"), err)

        # The library compiles, the binary does not; then the binary is fixed.
        write(tree, {"a/src/lib.rs": lib(4), "b/src/main.rs": main_rs("    nope\n")})
        code, out, err = build(tree)
        expect("a build that fails to compile",
               (code != 0, out, "image-cargo:" in err), (True, "", False), err)
        write(tree, {"b/src/main.rs": main_rs()})
        code, out, err = build(tree)
        expect("its fix", (code, out), (0, "4 three"), err)
    finally:
        shutil.rmtree(tree)
        subprocess.run(["docker", "volume", "rm", "-f", VOLUME], capture_output=True)
        subprocess.run(["docker", "rmi", IMAGE], capture_output=True)


if __name__ == "__main__":
    sys.exit(main())
