#!/usr/bin/env python3
"""Write the Homebrew formula of a fenecdb release.

    tools/homebrew-formula.py --version 0.1.11 --sums SHA256SUMS -o Formula/fenecdb.rb
    tools/homebrew-formula.py --check packaging/homebrew/Formula/fenecdb.rb
    tools/homebrew-formula.py --verify Formula/fenecdb.rb

The formula installs `fenec` and `fenec-server` from the release's tarballs
(`fenec-vX.Y.Z-<target>.tar.gz`, release.yml) and names each one's SHA-256,
read from the release's own `SHA256SUMS`. The release job (packages.yml,
`homebrew`) and a maintainer by hand both come through here, so the formula
in the tap is never typed: a hand-edited copy is what `--check` refuses, and
`--verify` downloads what each `url` names and holds it to its `sha256`, which
is what a user's `brew install` will do.

Standard library only, like the rest of tools/.
"""

import argparse
import difflib
import hashlib
import re
import sys
import urllib.request

REPO = "fenecdb/fenecdb"

# (Homebrew's block, target triple in the tarball's name), in the order the
# formula lists them.
TARGETS = [
    ("on_macos", "on_arm", "aarch64-apple-darwin"),
    ("on_macos", "on_intel", "x86_64-apple-darwin"),
    ("on_linux", "on_arm", "aarch64-unknown-linux-musl"),
    ("on_linux", "on_intel", "x86_64-unknown-linux-musl"),
]

# Only a release proper: a pre-release's tarballs are fine to download, but a
# tap's `brew upgrade` would hand it to everyone.
STABLE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")

TEMPLATE = """\
# Written by tools/homebrew-formula.py in {repo} from release v{version}'s
# SHA256SUMS: the release workflow rewrites it, so a change by hand is lost.
class Fenecdb < Formula
  desc "Embedded database with its own query language, FenecQL"
  homepage "https://fenecdb.com"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

{blocks}
  def install
    bin.install "fenec", "fenec-server"
  end

  test do
    # Neither binary has a version flag: fenec-server names its version on the
    # first line of --help, and the shell is run on a script, in memory.
    assert_match "fenec-server #{{version}}", shell_output("#{{bin}}/fenec-server --help 2>&1")
    script = "create collection t (n int)\\nput t {{id: 1, n: 7}}\\nget t\\n"
    assert_match(/^1\\s+7$/, pipe_output("#{{bin}}/fenec", script))
  end
end
"""


def parse_sums(text):
    """{file name: sha256} from `sha256sum`'s output (a `*` before the name
    is binary mode)."""
    sums = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        m = re.fullmatch(r"([0-9a-f]{64}) [ *](\S+)", line.strip("\r\n"))
        if not m:
            raise SystemExit(f"SHA256SUMS: cannot read the line {line!r}")
        sums[m.group(2)] = m.group(1)
    return sums


def tarball(version, target):
    return f"fenec-v{version}-{target}.tar.gz"


def render(version, sums, repo=REPO):
    version = version.removeprefix("v")
    if not STABLE.match(version):
        raise SystemExit(
            f"{version!r} is not a release version (X.Y.Z): a pre-release is "
            "not offered through Homebrew"
        )
    blocks = {}
    for outer, inner, target in TARGETS:
        name = tarball(version, target)
        if name not in sums:
            raise SystemExit(f"SHA256SUMS lists no {name}")
        url = f"https://github.com/{repo}/releases/download/v{version}/{name}"
        blocks.setdefault(outer, []).append(
            f'    {inner} do\n'
            f'      url "{url}"\n'
            f'      sha256 "{sums[name]}"\n'
            f'    end\n'
        )
    text = ""
    for outer in ("on_macos", "on_linux"):
        text += f"  {outer} do\n" + "".join(blocks[outer]) + "  end\n\n"
    return TEMPLATE.format(repo=repo, version=version, blocks=text.rstrip("\n") + "\n")


def read_formula(text):
    """The version and SHA256SUMS a formula was written from."""
    pairs = re.findall(
        r'url "https://github.com/[^/]+/[^/]+/releases/download/v([^/]+)/([^"]+)"\n\s+sha256 "([0-9a-f]{64})"',
        text,
    )
    if len(pairs) != len(TARGETS):
        raise SystemExit(f"expected {len(TARGETS)} url and sha256 pairs, found {len(pairs)}")
    versions = {v for v, _, _ in pairs}
    if len(versions) != 1:
        raise SystemExit(f"the formula names several versions: {sorted(versions)}")
    repo = re.search(r"Written by tools/homebrew-formula.py in (\S+) from", text)
    return versions.pop(), {name: sha for _, name, sha in pairs}, repo.group(1) if repo else REPO


def verify(text):
    """Download every url the formula names and hold it to its sha256."""
    pairs = re.findall(r'url "(https://[^"]+\.tar\.gz)"\n\s+sha256 "([0-9a-f]{64})"', text)
    if len(pairs) != len(TARGETS):
        raise SystemExit(f"expected {len(TARGETS)} url and sha256 pairs, found {len(pairs)}")
    bad = 0
    for url, want in pairs:
        digest = hashlib.sha256()
        with urllib.request.urlopen(url, timeout=120) as response:
            while chunk := response.read(1 << 20):
                digest.update(chunk)
        got = digest.hexdigest()
        print(f"{'ok  ' if got == want else 'FAIL'} {url}")
        if got != want:
            print(f"     the file is {got}, the formula says {want}")
            bad += 1
    if bad:
        raise SystemExit(f"{bad} of {len(pairs)} downloads are not what the formula says")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--version", help="the release, X.Y.Z or vX.Y.Z")
    p.add_argument("--sums", help="the release's SHA256SUMS ('-' for stdin)")
    p.add_argument("-o", "--output", default="-", help="the formula to write (default: stdout)")
    p.add_argument("--repo", default=REPO, help=f"owner/name of the release's repository (default {REPO})")
    p.add_argument("--check", metavar="FORMULA", help="fail unless FORMULA is what this script writes from the sums it holds")
    p.add_argument("--verify", metavar="FORMULA", help="download each url FORMULA names and fail unless it has the sha256 the formula says")
    args = p.parse_args()

    if args.verify:
        with open(args.verify, encoding="utf-8") as f:
            verify(f.read())
        return

    if args.check:
        with open(args.check, encoding="utf-8") as f:
            have = f.read()
        version, sums, repo = read_formula(have)
        want = render(version, sums, repo)
        if have != want:
            sys.stdout.writelines(
                difflib.unified_diff(
                    have.splitlines(True), want.splitlines(True), args.check, "rendered"
                )
            )
            raise SystemExit(f"{args.check} is not what tools/homebrew-formula.py writes")
        print(f"{args.check}: v{version}, as written")
        return

    if not args.version or not args.sums:
        p.error("--version and --sums are needed (or --check)")
    if args.sums == "-":
        sums = parse_sums(sys.stdin.read())
    else:
        with open(args.sums, encoding="utf-8") as f:
            sums = parse_sums(f.read())
    formula = render(args.version, sums, args.repo)
    if args.output == "-":
        sys.stdout.write(formula)
    else:
        with open(args.output, "w", encoding="utf-8", newline="\n") as f:
            f.write(formula)


if __name__ == "__main__":
    main()
