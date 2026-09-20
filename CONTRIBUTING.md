# Contributing to fenecdb

Thanks for taking an interest. This file is the short version of what a change
has to respect; `README.md` carries the long-form reasoning behind each rule.

## Getting set up

```bash
rustup target add wasm32-unknown-unknown   # rust-toolchain.toml asks for it
make test                                  # cargo test, then the JS tests
make wasm                                  # builds web/fenec.wasm
make serve                                 # the browser demo on :8787
```

**The wasm32 target comes from rustup.** Homebrew's `cargo` ships no wasm32 std
library, so the Makefile prefers `~/.cargo/bin/cargo`.

`make test` runs Rust first on purpose: `cargo test` is also what builds the
`fenec-pg` binary, and `web/fenec.sync.test.js` runs against it. Both JS suites
skip themselves when that binary or `web/fenec.wasm` is missing, so run
`make wasm` first if you want the end-to-end cases to actually execute.

Narrower runs:

```bash
cargo test -p fenec-core --test persist        # one integration test file
cargo test -p fenec-ql near                    # by name substring
cargo test -p fenec-core codec::tests          # inline unit tests in a module
node --test web/fenec.test.js
```

`cargo test -p fenec-import --test pg -- --ignored` needs a live PostgreSQL;
`make import-test` starts one in Docker and tears it down afterwards.

## The rules a change must not break

**Zero dependencies.** `fenec-core`, `fenec-ql`, `fenec-wasm`, `fenec-http`,
`fenec-pg` and `fenec-import` take no external crates — own codec, own JSON, own
HNSW, own SCRAM. The WASM output has to stay small and auditable. `fenec-bench`
is the single exception (`publish = false`); that is where `rusqlite` and
`postgres` live. A PR that adds a dependency to any other crate needs to argue
the case first, in an issue.

**Dependency direction.** `core -> ql -> http -> pg -> import -> cli`, and
nothing points back up. `fenec-core` does dev-depend on `fenec-ql` so tests can
write real queries; Cargo allows that cycle because a dev dependency never
enters the product build.

**Limits error, they do not truncate.** A silently cut result is a wrong answer
believed right. `near` caps at 10 000 rows and expression depth at 512 levels,
and both return a query error. Keep new limits in that shape.

**The HNSW graph is derived data.** It is written by `snapshot`, `compact` and
`checkpoint` only, never on the write path. On open, the version, dimension,
node count and link bounds are validated and anything off triggers a silent full
rebuild — which is why a corrupt graph can never cost you data. Do not add a
path that makes the graph load-bearing.

**The file format is append-only and self-delimiting.** Every record is
`[kind][collection-id][length][body]`. The length is written even for an empty
body and the reader *must* consume it, or the stray byte is read as the next
record kind.

**Threads are `cfg`'d out of WASM.** The parallel HNSW build path must not reach
wasm32, and `now()` errors there — wasm32-unknown-unknown has no clock, so time
is passed in as a parameter.

**Filtered `near` keeps its fallback.** When the ANN path lands under the limit
it must fall back to scanning the filter set in full. Without it, a filter
correlated with the vector eliminates every candidate and the query returns
empty.

## Style

- Comments explain *why*: a measured cost, a trap that was hit, an alternative
  that was rejected. Not what the line already says.
- All prose in the repo — comments, docs, README — is English.
- `Error` in `fenec-core/src/error.rs` is the single error type: allocation-free
  variants, no `Box`. `fenec-pg` maps it onto PostgreSQL SQLSTATE codes.
- Unit tests go inline in `#[cfg(test)] mod tests`. Cross-crate and protocol
  tests go in `crates/*/tests/`. Measurement programs are
  `crates/fenec-core/examples/` and are wired to `make`, not to CI.
- Run `cargo fmt --all` before you push; CI checks it. rustfmt settles
  formatting so that review can be about the change itself.

## Performance claims

`make bench`, `make sweep`, `make memory` and `make compare` are the harnesses
behind the numbers in the README. If a change moves any of them, say by how much
in the PR description and on what hardware — the README quotes measurements, not
estimates.

## Pull requests

Keep the diff to one concern. Say what you measured, and name the invariant
above that the change touches if it touches one. CI runs `make test` on Linux
and macOS plus a release build; it has to be green.

## Conduct and security

Participation is covered by `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1).

Do not open a public issue for a vulnerability — `SECURITY.md` has the private
reporting form and says what counts as one. Notably, the absence of TLS and the
fact that two processes opening the same file corrupts it are documented
design, not bugs.
