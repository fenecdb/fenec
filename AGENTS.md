# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

fenecdb — a minimal, vector-native embedded database in Rust. Compiles to WASM for
the browser, has its own query language (FenecQL) and speaks the PostgreSQL v3 wire
protocol. The docs under `site/content/docs/` are the long-form reference (design
rationale, benchmarks, full FenecQL and HTTP surface); `README.md` is the
front door and links into them, and this file is the working summary.

## Commands

```bash
make test          # cargo test, then the JS tests (node --test)
make wasm          # builds fenec-wasm for wasm32, copies to web/fenec.wasm
make serve         # wasm + python3 http.server -> http://localhost:8787
make bench         # scale measurement (fenec-core/examples/bench.rs)
make memory        # memory footprint, for calibrating --max-memory
make sweep         # ef / recall trade-off
make compare       # vs SQLite + pgvector (needs `make pgvector-up` first)
make import-test   # the PostgreSQL arm of import (needs Docker)
make small         # smallest `fenec` binary: --profile cli --no-default-features
make pg PGPASS=secret HTTP=127.0.0.1:8080   # run the server against ./data.fenec
```

Single tests:

```bash
cargo test -p fenec-core --test persist          # one integration test file
cargo test -p fenec-ql near                      # by name substring (integration: fn name only)
cargo test -p fenec-core codec::tests            # inline unit tests in a module
cargo test -p fenec-import --test pg -- --ignored   # needs a live PostgreSQL
node --test web/fenec.test.js                    # JS: builder
node --test --test-name-pattern 'shape' web/fenec.sync.test.js
```

`make test` runs Rust first on purpose: `cargo test` builds the `fenec-pg` binary
and `web/fenec.sync.test.js` runs against it (it self-skips when the binary or
`web/fenec.wasm` is missing).

**The wasm32 target comes from rustup.** Homebrew's `cargo` has no wasm32 std
library; the Makefile prefers `~/.cargo/bin/cargo` via `$(CARGO)`. Building by
hand needs `rustup target add wasm32-unknown-unknown`.

## Architecture

Dependency direction (nothing points back up):

```
fenec-core  (std only, zero deps)
     |
fenec-ql    (lexer + parser)          fenec-wasm  (C ABI, core+ql)
     |
fenec-http  (REST/JSON + SSE)
     |
fenec-pg    (wire protocol: server AND client)
     |
fenec-import (SQLite file reader + PG COPY source)
     |
fenec-cli   (`fenec` shell, `fenec import`, `fenec types`)
```

`fenec-bench` is a measurement harness (`publish = false`) and is the only crate
allowed external crates — that is where `rusqlite`/`postgres` live.

`fenec-core` modules: `store` (segments, offset index), `engine` (`Database`,
`Collection`, replay/snapshot/compact/checkpoint), `vector` (HNSW + distance
kernels), `text` (tokenizer, inverted index, BM25), `query` (`Statement`, plan
execution), `schema`, `value`, `codec`,
`json`, `num` (decimal text to `f64`), `time` (calendar arithmetic), `changes`
(the change ring), `plugin` (registry), `fs` (buffered file I/O, behind the
`std-fs` feature).

The browser client is `web/fenec.js` — WASM glue (~175 lines), the query builder,
the HTTP client and the sync layer, in one dependency-free ES module. `web/fenec.d.ts`
holds the types; `fenec types <file>` generates schema-specific declarations.

## Invariants worth knowing before you change things

**Zero dependencies is a hard rule** for `fenec-core`, `fenec-ql`, `fenec-wasm`,
`fenec-http`, `fenec-pg`, `fenec-import`. The WASM output has to stay small and
auditable; own codec, own JSON, own HNSW, own SCRAM/crypto, own decimal-to-`f64`
(`str::parse` drags in a 12 KB table -- see `num.rs`). `fenec-core` does
dev-depend on `fenec-ql` (Cargo allows the cycle through a dev dependency) so tests
can write real queries.

**No page cache.** The byte sequence on disk and in memory are the same format;
a read decodes directly over the arena slice. There is no eviction policy, no
dirty pages, and the whole database is resident — open peak ≈ 2× the file,
`compact`/`checkpoint` peak ≈ 3×.

**Single writer.** Reads take a shared lock (`Database::query`), writes the
exclusive one (`execute_with`). There are no transactions — `BEGIN`/`COMMIT` are
accepted and do nothing. Two processes opening the same file corrupts it, which
is why `fenec-http` is a second listener inside `fenec-pg`, never its own binary.

**File format** (see README *File format*): every record is
`[kind][collection-id][length][body]`. The length is written even for an empty
body and the reader **must** consume it, or the stray byte is read as the next
record kind. The change counter record (kind 6) is at the front and fixed width;
the id counter (kind 7) exists so `compact` cannot hand out a deleted id again.

**The HNSW graph is derived data, not a cache.** It is written only by
`snapshot`, `compact` and `checkpoint` — never on the write path. On open the
version, dimension, node count and link bounds are validated; anything off means
a silent full rebuild. A corrupt graph can therefore never lose data.

**Limits error, they do not truncate.** `near` results cap at 10 000 rows
(`limit + offset`) and expression depth at 512 levels; both return a query error,
because a silently cut result is a wrong answer believed right. Full table in
README *Fixed limits*.

**Threads are `cfg`'d out of WASM.** The parallel HNSW build path must not enter
the wasm32 target. Likewise `now()` errors there — wasm32-unknown-unknown has no
clock, so time is passed in as a parameter.

**Filtered `near` needs its fallback.** The filter set is extracted first; either
it is scanned directly (when smaller than `ef × m0`) or the ANN runs and
candidates are membership-tested. The second path *must* fall back to scanning
the filter set in full when the result lands under the limit — otherwise a filter
correlated with the vector eliminates every candidate and returns empty.

**Only `@hash` equalities inside an `and` chain reach an index.** Everything else
— `>=`, `~`, `has`, `in`, anything under `or` — is a full scan. `~` is unranked
substring matching; ranked text retrieval is `match` over an `@text` field,
which does reach one. `order` has no top-k: every match is sorted, then `limit`
applies.

**`lookup` is one bucket probe per parent, not a join.** It attaches another
collection's matching documents to the row they belong to, and a `limit` after
it counts children *per parent* -- the shape a join cannot express. It is
terminal in the grammar: everything before it binds to the driving collection,
everything after to the looked-up one, which is exactly why there are no
qualified names (`reviews.stars`) in the language and no filter splitting in
the planner. The child key must be `id` or carry `@hash`; refusing an
unindexed one follows `near` and `match`, because a silent full scan of the
child collection would be a different feature under the same name. Nesting
lives in `ResultSet.nested`, never in `Value` -- JSON transports nest, the
PostgreSQL wire flattens (`ResultSet::flatten`), and the "no nested objects"
rule stands. Measured at 22.7 us in process against 0.332 ms for the same
page as a `/batch` of 21 queries merged on the client.

**`match` prunes with MaxScore.** The exhaustive merge is not selective --
on BEIR FiQA the average query reaches 86% of the corpus -- so terms whose
remaining ceiling cannot beat the worst kept score stop driving the frontier.
Measured, with identical output: SciFact 179 -> 47 us, FiQA 1918 -> 311 us,
Turkish WebFAQ 401 -> 82 us. `pruning_never_changes_the_answer` compares it
against an exhaustive reference over 25 000 generated cases; scores accumulate
in `f64` so the two orders of summation agree once rounded back to `f32`.

**`@text` is word-boundary matching unless told otherwise.** `prefix=N` also
indexes each word's prefixes, which is a dictionary-free stemmer for inflected
languages: on Turkish WebFAQ it is worth +14.6% nDCG@10 for 3.4x the postings
(58 MB -> 182 MB, 82 -> 485 us per query). Off by default — the corpus
decides. The tokenizer also folds `I`, `İ`, `ı`, `i` onto one term, because the
locale-blind Unicode mapping turns `İ` into two code points nobody can type and
would hide every capitalised Turkish word.

**The text index is derived data as well, but it is not persisted.** `@text`
builds an inverted index that is rebuilt from the documents on open — 27 µs per
document against the HNSW graph's ~102 µs, and the rebuild pass already reads
every document for the hash indexes. Nothing about it reaches the file, so
there is no validation path and no stale-index case to handle. It is shrunk to
fit where it is known complete (rebuild, `create index`); live ingest keeps
`Vec` growth slack.

**`rerank` deliberately uses no index.** `match ... rerank` takes candidates
from the inverted index and reorders them by exact distance over vectors read
straight out of the store — so a collection can do vector retrieval with no
HNSW graph to build, hold, validate or rebuild. Measured on BEIR it matches or
beats a full dense scan while scoring under 2% of the corpus. It requires
`match`: without candidates there is nothing to reorder.

**Profiles differ on purpose.** `fenec-cli` uses the `cli` profile (`panic =
abort`, single process, nothing to recover). `fenec-pg` stays on `release`: a
panicking connection thread unwinds and drops only its own session.

## Conventions

- `Error` (`fenec-core/src/error.rs`) is the single error type: allocation-free
  variants, no `Box`. `fenec-pg` maps it onto PostgreSQL SQLSTATE codes.
- Unit tests live inline in `#[cfg(test)] mod tests`; cross-crate and protocol
  tests live in `crates/*/tests/`. Measurement programs are
  `crates/fenec-core/examples/` and are wired to `make` targets, not to CI.
- Comments explain *why* a thing is the way it is — a measured cost, a trap that
  was hit, an alternative that was rejected. Match that when adding code.
- All prose in the repo (comments, docs, README) is English.
