# The crates

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The crates*: zero dependencies, limits that error rather than truncate, and the build profiles. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Zero dependencies

**Zero dependencies is a hard rule** for `fenec-core`, `fenec-ql`, `fenec-abi`, `fenec-wasm`, `fenec-ffi`,
`fenec-http`, `fenec-wire`, `fenec-server`, `fenec-import`, `fenec-shard`. The WASM output has to stay small and
auditable; own codec, own JSON, own HNSW, own SCRAM/crypto, own decimal-to-`f64`
and back (`str::parse` drags in a 12 KB table, `{}` on a float 20.8 KB of
Grisu and Dragon -- see `num.rs`). `fenec-core` does
dev-depend on `fenec-ql` (Cargo allows the cycle through a dev dependency) so tests
can write real queries.

## Limits

**Limits error, they do not truncate.** `near`, `match` and `fuse` results
cap at 10 000 rows (`limit + offset`), expression depth at 512 levels, a
`lookup` chain at 8 and an `in (get ...)` at 100 000 values and 4 levels;
all of them return a query error, because a silently cut result is a wrong
answer believed right. Full table in `site/content/docs/limits.html`.

## Profiles

**Profiles differ on purpose.** `fenec-cli` uses the `cli` profile (`panic =
abort`, single process, nothing to recover). `fenec-server` stays on `release`: a
panicking connection thread unwinds and drops only its own request. A cold
crate is built for size in the profile it is cold in: in `cli`, `fenec-http`, which the shell reaches only for
`backup`, `archive` and `restore`, waiting on the network and the disk,
and for `fenec mcp`, waiting on a model (16.3 KB of the released binary,
measured before `mcp`). Not the importer, whose SQLite import of
200 000 rows took 285 ms against 251 for 32.6 KB, nor the root crate, which
built for size made the binary 129 KB larger (`make size-report BIN=fenec`).
Both binaries hold 72 KB of the standard library's backtrace symbolizer
(`gimli`, `addr2line`, `rustc_demangle`, `object`), which only a nightly
`build-std` leaves out.
