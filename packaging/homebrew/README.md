# Homebrew tap for fenecdb

```bash
brew install fenecdb/tap/fenecdb
```

This installs `fenec`, the shell, and `fenec-server`, the HTTP server, from
the [fenecdb](https://github.com/fenecdb/fenecdb) release for your Mac
(Apple silicon or Intel) or Linux machine (arm64 or x86_64). They are the
release's own binaries, static and checked against the SHA-256 the formula
names; nothing is built on your machine.

```bash
fenec notes.fenec                              # the shell on a file of its own
FENEC_HTTP_TOKEN=secret fenec-server --file data.fenec   # the server
brew upgrade fenecdb                           # a new release
brew uninstall fenecdb
```

A first query, step by step: [fenecdb.com/docs/quickstart](https://fenecdb.com/docs/quickstart).

## How this repository is kept

`Formula/fenecdb.rb` is not edited here. Each time a release of
`fenecdb/fenecdb` is published, its `packages.yml` runs
`tools/homebrew-formula.py` on the release's `SHA256SUMS` and pushes the
result to this repository's `main`, so the version and the four checksums
are always the release's own. Pre-releases are not offered.

To write it by hand, from a clone of `fenecdb/fenecdb`:

```bash
gh release download vX.Y.Z -R fenecdb/fenecdb -p SHA256SUMS
python3 tools/homebrew-formula.py --version X.Y.Z --sums SHA256SUMS -o ../homebrew-tap/Formula/fenecdb.rb
```

Problems with fenecdb itself belong in
[fenecdb/fenecdb](https://github.com/fenecdb/fenecdb/issues).
