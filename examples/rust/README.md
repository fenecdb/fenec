# Notes -- Rust (embedded)

A notes CLI with the engine inside the program: `fenec-core` opens
`notes.fenec`, `fenec-ql` parses the statements, no server.

## What it shows

- the schema in `schema.fenecql` (compiled in with `include_str!`), made or
  checked at every open through `fenec_abi::schema` -- the same check every
  SDK's `schema(...)` runs -- and four notes seeded into an empty file;
- creating notes, and marking one done;
- full-text search with `match`, fused with `near` over a toy embedding
  (`fuse`);
- filtering by tag (`tags has`) or by `done`, newest first;
- persistence: every write is synced before the command returns, and the
  file opens where it stopped.

Rust has no query builder: the statements are FenecQL, every value a
parameter (`$1`, `$2`, ...). There is no `watch` either: a file is open in
one process at a time, so no other process could change what it would
show.

## Prerequisites

Rust (the toolchain this repository pins, or a recent stable). The crates
are not on crates.io: Cargo fetches them from this repository at the
release's tag.

## Run

```sh
cd examples/rust
cargo run                                # ./notes.fenec: seeds, then lists
cargo run -- add "Dentist" "Call the dentist on Monday." health
cargo run -- list --tag home
cargo run -- list --open
cargo run -- search istanbul trip        # match, then fuse
cargo run -- done 1
cargo run -- smoke                       # the whole tour on a file of its own, reopened: what CI runs
```

`NOTES_FILE` names another file.

## The core

```rust
let mut db = fenec_core::fs::open("notes.fenec")?;
fenec_abi::schema(&mut db, &format!(r#"{{"format": 1, "fenecql": {}}}"#, json_string(SCHEMA)), true, None)?;

let put = &fenec_ql::parse("put notes {title: $1, body: $2, tags: $3, done: $4, at: $5, embed: $6}")?[0];
db.execute_with(put, &[title, body, tags, Value::Bool(false), Value::Timestamp(now), Value::Vector(embed(text))])?;
db.sync()?;                                              // the library never syncs on its own

let hits = &fenec_ql::parse("get notes select title match body $1 near embed $2 fuse limit 5")?[0];
db.query(hits, &[Value::Text(words.into()), Value::Vector(embed(words))])?;
```

## The toy embedding

`embed()` is a placeholder, not a model: character trigrams (of the UTF-8
bytes) hashed into 64 dimensions with FNV-1a, so `near` and `fuse` have
something to rank without a download. It matches spelling, not meaning;
every Notes example computes the same vectors. A real program runs a model
here -- ONNX Runtime or candle locally, or an embeddings API -- and declares
`vector<N>` with that model's `N` in `schema.fenecql`.

## Against this repository's build

CI builds the example on the crates in this checkout: `examples/run-tests.sh
rust` copies the folder and points the three dependencies at their paths,
`fenec-core = { path = "../../crates/fenec-core" }` and so on, then runs
`cargo run -- smoke`.
