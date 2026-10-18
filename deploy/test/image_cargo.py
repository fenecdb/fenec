#!/usr/bin/env python3
"""tools/image-cargo.sh -- the build both images' binaries go through --
held to what it is for: binaries made from the sources a build was given,
whatever their mtimes, and a build that fails when they are not.

    python3 deploy/test/image_cargo.py      # make deploy-check runs it

First, both Dockerfiles build their binaries through the script and copy
them from /built: either put back to a plain `cargo build` passed the rest.
Then a workspace of a library, read through include_bytes! from a file
outside itself as fenec-http reads the studio's, and two binaries using it
is built in rust:alpine by the script, its /work a volume of the test's own
as the Dockerfiles' is a cache mount:

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
  not when the whole build succeeds;
- the library changed and built for one binary alone, then the other's
  outputs made newer than it, as a clock set back between the two builds
  leaves them: that binary, its own sources unchanged, fails the check,
  which names the library it linked.

Each fails with its part taken out: times kept in the copy (`rsync -t`,
COPY's way), the check, a unit's record kept only once the whole build
has succeeded, or the libraries a compile was handed (--extern) left out
of its record.
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


def build(tree, package="b"):
    p = subprocess.run(
        ["docker", "run", "--rm", "-v", f"{tree}:/src:ro", "-v", f"{VOLUME}:/work",
         "-v", f"{SCRIPT}:/tools/image-cargo.sh:ro", "-w", "/src", IMAGE,
         "sh", "-c", f"/tools/image-cargo.sh /out --release -p {package} && /out/{package}"],
        capture_output=True, text=True, timeout=600)
    return p.returncode, p.stdout.strip(), p.stderr


def in_volume(command):
    subprocess.run(["docker", "run", "--rm", "-v", f"{VOLUME}:/work", IMAGE, "sh", "-c", command],
                   check=True, capture_output=True, timeout=120)


def held_dockerfiles():
    """Each Dockerfile runs the script once and no `cargo build` of its own,
    and copies every binary it installs from /built."""
    for path in ("Dockerfile", "deploy/Dockerfile"):
        with open(os.path.join(ROOT, path)) as f:
            text = f.read().replace("\\\n", " ")
        lines = [l.strip() for l in text.splitlines() if not l.lstrip().startswith("#")]
        runs = [l for l in lines if l.startswith("RUN")]
        through = [l for l in runs if "tools/image-cargo.sh /built " in l]
        plain = [l for l in runs if "cargo build" in l]
        copies = [l for l in lines if l.startswith("COPY --from=build") and "/usr/local/bin" in l]
        if len(through) != 1 or plain or not copies or not all(" /built/" in l for l in copies):
            raise SystemExit(f"image-cargo: {path} does not build through tools/image-cargo.sh "
                             "and copy from /built:\n" + "\n".join(runs + copies))
        print(f"ok  {path}: built through the script, copied from /built")


def expect(what, got, want, err):
    if got != want:
        raise SystemExit(f"image-cargo: {what}: got {got!r}, want {want!r}\n{err}")
    print(f"ok  {what}: {got}")


def main():
    held_dockerfiles()
    subprocess.run(["docker", "build", "-q", "-t", IMAGE, "-"], input=BASE, text=True,
                   check=True, capture_output=True, timeout=600)
    subprocess.run(["docker", "volume", "create", VOLUME], check=True, capture_output=True)
    tree = tempfile.mkdtemp(prefix="image-cargo-")
    try:
        write(tree, {
            "Cargo.toml": '[workspace]\nresolver = "2"\nmembers = ["a", "b", "c"]\n',
            "a/Cargo.toml": '[package]\nname = "a"\nversion = "0.1.0"\nedition = "2021"\n',
            "a/src/lib.rs": lib(1),
            "b/Cargo.toml": ('[package]\nname = "b"\nversion = "0.1.0"\nedition = "2021"\n'
                             '[dependencies]\na = { path = "../a" }\n'),
            "b/src/main.rs": main_rs(),
            "c/Cargo.toml": ('[package]\nname = "c"\nversion = "0.1.0"\nedition = "2021"\n'
                             '[dependencies]\na = { path = "../a" }\n'),
            "c/src/main.rs": 'fn main() { println!("c {}", a::N); }\n',
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
        in_volume("printf 'three\\n' > /work/src/assets/word.txt && "
                  "touch -t 202001010000 /work/src/assets/word.txt")
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

        # The library changed and built for c alone; then b's outputs made
        # newer than the library's, so cargo takes b for fresh.
        code, out, err = build(tree, "c")
        expect("the other binary", (code, out), (0, "c 4"), err)
        write(tree, {"a/src/lib.rs": lib(5)})
        code, out, err = build(tree, "c")
        expect("the library changed, built for it alone", (code, out), (0, "c 5"), err)
        in_volume("touch -d '2030-01-01 00:00:00' /work/target/release/deps/b-* "
                  "/work/target/release/.fingerprint/b-*/* /work/target/release/b")
        code, out, err = build(tree)
        named = "compiled from other bytes" in err and ".rlib: FAILED" in err and "/liba-" in err
        expect("a binary kept from another build of its library",
               (code, out, named), (1, "", True), err)
        code, out, err = build(tree)
        expect("the build after it", (code, out), (0, "5 three"), err)
    finally:
        shutil.rmtree(tree)
        subprocess.run(["docker", "volume", "rm", "-f", VOLUME], capture_output=True)
        subprocess.run(["docker", "rmi", IMAGE], capture_output=True)


if __name__ == "__main__":
    sys.exit(main())
