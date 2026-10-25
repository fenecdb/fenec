#!/usr/bin/env python3
"""tools/homebrew-formula.py: what it writes from a release's SHA256SUMS, and
what it refuses.

    python3 tools/test_homebrew_formula.py
"""

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "homebrew-formula.py")
COMMITTED = os.path.join(HERE, "..", "packaging", "homebrew", "Formula", "fenecdb.rb")

spec = importlib.util.spec_from_file_location("formula", SCRIPT)
formula = importlib.util.module_from_spec(spec)
spec.loader.exec_module(formula)

TARGETS = [t for _, _, t in formula.TARGETS]


def sums(version, skip=None, extra=True):
    """A SHA256SUMS as release.yml writes one: the tarballs, and the rest of
    the release's files beside them."""
    lines = []
    for i, target in enumerate(TARGETS):
        if target != skip:
            lines.append(f"{str(i + 1) * 64}  fenec-v{version}-{target}.tar.gz")
    if extra:
        lines += [
            f"{'a' * 64}  fenec-web-v{version}.tar.gz",
            f"{'b' * 64}  fenecdb-android-{version}.aar",
            f"{'c' * 64}  fenecdb_{version}-1_amd64.deb",
            f"{'d' * 64}  fenecdb-{version}-1.x86_64.rpm",
        ]
    return "\n".join(lines) + "\n"


class Render(unittest.TestCase):
    def test_every_platform_gets_its_own_tarball_and_sum(self):
        text = formula.render("1.2.3", formula.parse_sums(sums("1.2.3")))
        for i, target in enumerate(TARGETS):
            url = f"https://github.com/fenecdb/fenecdb/releases/download/v1.2.3/fenec-v1.2.3-{target}.tar.gz"
            self.assertIn(f'url "{url}"\n      sha256 "{str(i + 1) * 64}"', text)
        # The release's other files are not the formula's business.
        self.assertNotIn("android", text)
        self.assertNotIn(".deb", text)

    def test_the_blocks_are_homebrews(self):
        text = formula.render("1.2.3", formula.parse_sums(sums("1.2.3")))
        self.assertIn("on_macos do\n    on_arm do\n", text)
        self.assertIn("on_linux do\n    on_arm do\n", text)
        self.assertEqual(text.count("    on_intel do\n"), 2)
        self.assertIn("bin.install \"fenec\", \"fenec-server\"", text)

    def test_a_v_in_front_is_the_same_release(self):
        s = formula.parse_sums(sums("1.2.3"))
        self.assertEqual(formula.render("v1.2.3", s), formula.render("1.2.3", s))

    def test_binary_mode_sums_are_read(self):
        text = sums("1.2.3").replace("  fenec-v", " *fenec-v")
        formula.render("1.2.3", formula.parse_sums(text))

    def test_a_missing_tarball_is_refused(self):
        s = formula.parse_sums(sums("1.2.3", skip="x86_64-apple-darwin"))
        with self.assertRaises(SystemExit) as e:
            formula.render("1.2.3", s)
        self.assertIn("x86_64-apple-darwin", str(e.exception))

    def test_a_pre_release_is_not_offered(self):
        for version in ("1.2.3-rc1", "1.2", "1.2.3.4", "main"):
            with self.assertRaises(SystemExit, msg=version):
                formula.render(version, formula.parse_sums(sums(version)))

    def test_a_line_that_is_not_a_sum_is_refused(self):
        with self.assertRaises(SystemExit):
            formula.parse_sums("not a checksum\n")

    def test_the_sums_of_another_version_do_not_do(self):
        with self.assertRaises(SystemExit):
            formula.render("1.2.4", formula.parse_sums(sums("1.2.3")))


class Check(unittest.TestCase):
    def run_check(self, text):
        with tempfile.NamedTemporaryFile("w", suffix=".rb", delete=False) as f:
            f.write(text)
        try:
            return subprocess.run(
                [sys.executable, SCRIPT, "--check", f.name], capture_output=True, text=True
            )
        finally:
            os.unlink(f.name)

    def test_what_it_wrote_passes(self):
        text = formula.render("1.2.3", formula.parse_sums(sums("1.2.3")))
        self.assertEqual(self.run_check(text).returncode, 0)

    def test_a_change_by_hand_does_not(self):
        text = formula.render("1.2.3", formula.parse_sums(sums("1.2.3")))
        done = self.run_check(text.replace('license "Apache-2.0"', 'license "MIT"'))
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("MIT", done.stdout)

    def test_the_committed_formula_is_what_it_writes(self):
        with open(COMMITTED, encoding="utf-8") as f:
            self.assertEqual(self.run_check(f.read()).returncode, 0)

    def test_the_command_line_writes_the_same(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "SHA256SUMS")
            with open(path, "w") as f:
                f.write(sums("1.2.3"))
            done = subprocess.run(
                [sys.executable, SCRIPT, "--version", "v1.2.3", "--sums", path],
                capture_output=True,
                text=True,
            )
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, formula.render("1.2.3", formula.parse_sums(sums("1.2.3"))))


if __name__ == "__main__":
    unittest.main()
