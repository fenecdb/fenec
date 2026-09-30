#!/usr/bin/env python3
"""One version wherever a release reads it: `make version V=0.1.5`.

The workspace's version and fenec-wire's where the workspace names it, the
Python package's, both npm packages' and the React package's lock file, and
the image the README pulls -- site/build.py holds the README to the
workspace, and packages.yml refuses a release whose packages say another
number. Cargo.lock follows at the next build."""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def sub(path, pattern, repl):
    """Replaces the one match of `pattern` in `path`, or stops: a file whose
    shape moved would otherwise keep its old number without a word."""
    p = ROOT / path
    text, n = re.subn(pattern, repl, p.read_text(encoding="utf-8"), count=1, flags=re.M)
    if n != 1:
        sys.exit(f"{path}: no match for {pattern!r}")
    p.write_text(text, encoding="utf-8")


def main():
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        sys.exit("usage: version.py X.Y.Z")
    v = sys.argv[1]
    sub("Cargo.toml", r'^version = "[^"]+"', f'version = "{v}"')
    sub("Cargo.toml", r'^(fenec-wire = \{ version = )"[^"]+"', rf'\g<1>"{v}"')
    sub("integrations/python/pyproject.toml", r'^version = "[^"]+"', f'version = "{v}"')
    for path in ("web/package.json", "integrations/react/package.json", "integrations/cloudflare/package.json", "integrations/langchain/package.json"):
        sub(path, r'^(  "version": )"[^"]+"', rf'\g<1>"{v}"')
    # A lock file names the package's version twice, at its top and as the
    # root of `packages`; each package's `make ...-test` installs from it
    # with `npm ci`. The bump to 0.1.5 found React's still at 0.1.4, and the
    # one to 0.1.6 the two locks made since at 0.1.5.
    for name in ("react", "cloudflare", "langchain"):
        lock = f"integrations/{name}/package-lock.json"
        sub(lock, r'^(  "version": )"[^"]+"', rf'\g<1>"{v}"')
        sub(lock, rf'^(    "": \{{\n      "name": "@fenecdb/{name}",\n      "version": )"[^"]+"', rf'\g<1>"{v}"')
    sub("README.md", r"(ghcr\.io/fenecdb/fenec-pg:)\d+\.\d+\.\d+", rf"\g<1>{v}")
    print(f"version {v}: Cargo.toml, pyproject.toml, every package.json and lock, README.md")


if __name__ == "__main__":
    main()
