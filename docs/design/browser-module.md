# The browser module

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The browser module*: threads and features, what is core, and the bytes the module holds. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## No threads in WASM

**Threads are `cfg`'d out of WASM.** The parallel HNSW build path must not enter
the wasm32 target. Likewise wasm32-unknown-unknown has no clock: `now()` reads
the time the module is handed with every statement (`fenec_query`'s last
argument, `db.now`'s `Date.now()`, through `Database::set_clock`) and
errors where it is handed none.

## Index features

**The indexes are features, and a build without one opens a file that
declares it.** `fenec-core`'s `vector`, `text`, `sparse`, `sorted` and
`composite` (an index over several fields, which needs `sorted`; the five
are `indexes`, and with `language` below make `full`, on by default) are
what a browser module may leave out: `make wasm FEATURES="text sorted"`,
`FEATURES=none` for none, and `make wasm-sizes` weighs each (below). What stands in for a missing one is a type of no value with
the real one's methods (`off.rs`: a field of an empty enum), so the engine
compiles unchanged and the compiler drops every path through it; only the
places that make one are `cfg`'d (`Collection::new`,
`reset_index_structures`, `build_index`, the maintenance build). The file
does not change with the build: a collection declaring the index is made
and opened and its documents read and written, a `near` over it measures
every vector and a sparse `near` scores every document, as `exact` does
(below), a `match` over it is refused naming the feature (`not_built`), and
so is a `create index` of its kind -- while one replayed from the log is taken and
not built, since refusing it would refuse the file. A `@sorted` field's
comparisons and orders are the scan's, the same rows, so which types it
takes is the type's answer in both builds (`sorted::orderable`). A graph in
the file is passed over, and a checkpoint without one holds none, which a
full build rebuilds on open like a graph that does not validate. `rerank`
reads vectors out of the store and needs only `text`. The checks for a
missing index ask `EVERY_INDEX` first: the lookup of a field is a loop the
compiler cannot prove ends, and one left in cost the full module 197 bytes
brotli; asked first, `EVERY_INDEX` left it the size it had been before the
features, to the byte. A crate that depends on `fenec-core` names `full`
itself (the workspace takes it without default features), and `fenec-ql`
only as a dev dependency -- in its normal ones it would put them back into
every browser module. Without `composite` an index over several fields is
the scan's, and a unique one is asked by reading the rows, as it was
without `sorted`.
`tests/features.rs` and the unit tests run without them in `make test`,
`web/fenec.test.js` hands files between the full module and the one
`make wasm-lite` makes, both ways, and CI runs clippy over none and each
alone.

## Language features

**The rest of the language is features too, and the smallest module is
149 KB brotli.** `language` is what a page may go without past the
indexes: `geo` (`distance`, `within`, `near` over a point and `@geo`,
which needs `sorted` too), `nocase`, `collate`, `facets`, `highlight`
(which marks what `match` found, so needs `text`) and `analytics` (the
HLL sketches and `bucket`; every other aggregate -- `count`, `sum`,
`avg`, `min`, `max`, `count(distinct)`, `first` and `last` -- and
`having` are in every build). `make wasm-sizes` builds each added to the
module with none (432.5 KB, 149.0 brotli, against 713.8 and 244.1 with
every one) and left out of the full one, brotli added and saved:
`vector` 20.4 and 19.0, `sorted` 8.7 and 23.5 with `composite`,
`composite` beside it 11.6 and 10.9, `collate` 8.9 and 8.4, `text` 13.1
and 15.2 with `fuzzy`, `prefix` and its term table (5.7 and 7.3 before
the first two, 10.9 and 13.0 before the table), `facets` 3.6 and 4.5, `highlight` beside `text` 3.8 and 3.6, `nocase` 2.8 and 2.6, `geo`
2.5 and 6.0 (its index with it), `analytics` 5.5 and 5.1, `sparse` 2.4
and 1.4, the schema check 9.5 and 9.5. The module with every feature is the code
it was (`make size-report BASE=main`: 14 bytes less of it, functions moved
and closures renumbered, brotli within noise), each check for a missing
one behind `crate::FULL`, a `cfg!` of them all as `EVERY_INDEX` is: a
message for a function a missing feature brings, asked before the check,
was 140 bytes of it. The ways a feature goes, each so the compiler sees
the paths dead rather than skipped: a type of no value where a value
would carry it -- `CaseFold` an empty enum without `nocase`, so every
`Option<CaseFold>` is `None` and every folded key, test, order and group
goes with `case.rs`'s fold; `Test::Distance`, `Within` and `Fold::Hll`
`cfg`'d out with their arms -- or the one place that makes one asking the
feature first: `Collation::compare` answers the bytes' order without
`collate`, never asked, and `collate::PARTIAL` takes the chunk machinery
and the module's Latin chunk out with it; `split_facets`, the facet
counts and `mark_plans` behind `cfg!`; `distance`, `within`, `bucket` and
`hll_estimate` left unregistered, a call refused by name
(`missing_function`). A build without one opens every file. A point is
read, written and checked as one, and a radius, a box or `near` over one
refused. A `nocase` or collated field's text compared as written would be
a wrong answer, so every statement over its collection is refused
(`refuse_unbuilt`, asked in `answered` and `answered_select`, which every
read, write, inner `get` and `lookup` level passes), as is a `create
collection` or an `alter` declaring one and `order ... collate`; the
file's other collections answer, and a drop takes the collection away.
`facet`, `highlight` and `snippet` are refused where a select is checked,
at its parse through `fenec_ql`, the sketches where their fold is made. The
parser keeps all of it: its code for these is about 2 KB of the module
(`facet_list` 1.0, a composite's keys 0.8, `collate` 0.3), well under 1 KB
brotli, and the engine's refusal names the feature where a parse error
would not. Measured and left in: `json` -- a path refused and a json
value with it took 0.6 KB brotli off the module with none and nothing off
the full one, since a path is read wherever a field is (the filter's
slots, the aggregate's reader, `order`'s keys, the hash, ordered and
composite keys) and an object is also a document handed as a parameter --
and `@ttl`, 0.7 KB, against refusing every collection whose rows expire.
`tests/features.rs` runs without the indexes (`std-fs,language`) and
without anything in `make test`, opening `tests/language.fenec`, which
only a build with the features can write (`tests/language_file.rs` holds
it to the full build's answers, `FENEC_WRITE_FIXTURE=1` writes it again);
`web/fenec.test.js` opens the full module's file in the one `make
wasm-min` makes and holds it to each refusal; CI's clippy runs the module
with none, each feature alone and each group, a check of no code made,
about 10 s a set.

## What is core

**What is core, and why one module ships.** What a general-purpose
database is expected to do for any app is core and goes in every module:
storage and FenecQL, the hash, ordered, unique and composite indexes, the
schema check, search -- `match`, `highlight` and `snippet`, facets,
`near` with its graph, `fuse` -- and the standard query features. A
plugin is an extra one kind of app needs: geo, sparse vectors, a
language's collation, `bucket` and the HLL sketches. Held to that rule
the core is about 223 KB brotli and the extras about 21 together (`geo`
6.0, `collate` 8.4, `sparse` 1.4, `bucket` and the sketches 5.1), 9% of
the full module's 244: too little to pay for modules built ahead
for sets of features and a choice of one per page, let alone a module a
plugin loaded beside the engine, which would carry its own copy of the
standard library's allocator and panic machinery and reach into the write
path, the undo, the file and replication through a boundary. So
`@fenecdb/web` ships the one full module, the features stay for a page
that builds its own and for the tests of the invariant above, and bytes
are won by making the core smaller (below). `count(distinct)`, `having`
and `first`/`last` are standard, and are in the core.

## One copy of a generic

**The browser module holds one copy of a generic where it can.** Every
type a sort, a map or a `collect` is compiled for is its own copy of the
code, and `make size-report` lists them; `WHY=<pattern>` names what pulls
one in. The sorts go through few: ids as `u64`s (`sort_unstable`),
`(DocId, f32)` by `text::best_first` -- which the text index's cursors take
too, as `(position, -ceiling)`, the stable order they had -- and rows by
`order_rows`, which a `lookup` level's order shares (its children in
ascending id first, a tie's order as before; 10 to 30% faster, its own
`(Vec<Value>, DocId)` sort gone). The vector index's candidates keep a
stable sort of their own: through `best_first` a 30 000 x 128 build took
2.5% longer, for 0.4 KB brotli. A collection's indexes by field are
`Fields`, a `Vec` searched by name, as `sorted` and `sparse` were, and the
registry's functions a `Vec` too -- each map was a copy of hashbrown, and
the registry's lowered the name into a new `String` on every call of every
row (17% of two calls a row). Against 11.2 the module lost 28.5 KB, 3.9 KB
brotli. A `str` slice that can panic keeps its panic's formatting of a
`char` -- `escape_debug` and Unicode's tables of what prints: 16.5 KB of
the module with the standard library's `to_lowercase`, `to_uppercase` and
`contains`, which slice that way. So the browser path slices with `get` and
`split_at_checked`, case is `case`'s, and `~` searches the folded bytes;
`make size-report WHY=slice_error_fail` finds a slice that brings it back.

## Size cuts

**The core lost what it held twice or for one use.** With every feature
the module went 689.9 -> 662.3 KB, 229.3 -> 224.2 KB brotli (gzip 277.1
-> 269.8), and with none 431.7 -> 414.5 KB, 145.3 -> 142.4 brotli, after
`count(distinct)`, `first`/`last` and `having` came into every build (3.4
KB of the one with none, 1.2 brotli) and with no feature taken out. A
function `#[inline(always)]` for a caller the module does not link had two
it does: `build_index` (`create index` and `alter`, 2.2 KB, 0.4 brotli)
and `reset_index_structures` (0.6 KB), each inlined natively alone now;
and `facet`, inlined, was called from two arms of `facets` (2.5 KB, 0.5),
one call now. Each export's closure was a `LocalKey::with` and a
`RefCell` borrow of its own, 2.7 KB, 0.7 brotli, until the handles' table
was reached through a pointer (`fenec-wasm`'s `table`: one thread, and no
export calls another); `Parser::err` was ten copies of its `format!` (1.0,
0.3), `Error`'s `Display` a `write!` a kind (0.5); the builtins a type,
vtable, `arity` and `doc` each (0.9, 0.2), one `Builtin` in a table now --
its arity kept in the registry as it is registered, or the second
indirect call took `lower()` over a scan 158 -> 160 ns a row (kept: 156
-> 152). Standard library code a call brought in for one use:
`str::parse::<i64>`, inlined at the JSON reader, the lexer and a date
(1.5 KB with the `Display` of an `i32` a counter's type brought, 0.6
brotli -- `num::parse_i64`, held to `str::parse` over 60 000 strings);
`from_utf8_lossy` (0.8 KB, 0.3: `Utf8Error` names the same sequences, held
to it in `fenec-wasm`'s tests); a float's 8 bytes through
`try_into().unwrap()`, which kept `Debug` formatting and its `PadAdapter`
(1.2 KB, 0.3); `clamp`'s check of its bounds, `usize`'s `Debug`, and a
`char` formatted (0.5). The `in` union's stable sort of ids was there to
find the runs a bucket already gives (2.8 KB, 0.4): one bucket is its own
answer now, up to eight are merged where they end and more sorted
unstable, quicker than it every way (2 runs of 200 random ids 0.5 us
against 1.6, 2 000 runs of 10 166 us against 233). The collections by name are a
`Vec` in the order of their names' `Fx` hashes (`Names`, 2.4 KB, 0.3,
SipHash over text with it) -- searched by the names themselves, every
statement's look-up compared texts and a read by id over 41 collections
went 450 -> 514 ns -- and a load's ids a `Vec` (`ById`, 1.7 KB).
`collect`s into a `Result` -- `near`'s vector and the functions over
vectors, one `Value::floats` now, bytes from a list, a `put $n`'s
documents, a list literal's constants, the folds a group's row takes --
are loops (4.7 KB, 0.9). A derived index is a `OnceCell` in the browser,
which has one thread to wait for (1.0 KB), and the rest is a few hundred
bytes each: `fenec_abi::refused` out of line, the search scratch made at
compile time, the chunks' names a `static`, `count`'s answer made once.
Tried and left out: `best_first` sorted unstable, a total order (1.1 KB,
0.1 brotli, `match ... limit 1000` 1.4% slower);
constructors for an error of a literal (`.into()` is a call already: +0.1
KB, +0.2 brotli); `index_doc` and `index_vectors_batch` out of line
(brotli +70 and +180 bytes); `Tok::describe` making one `String`; LLVM's
inline threshold at 0 and at 25 (+6.9 and +18.8 KB). What is left is each
crate's: `-Zshare-generics` is the default at opt-level "z" already, and
`drop_glue<Statement>`, `Expr` and `Select` and the standard library's
`#[inline]` code are compiled in fenec-ql, fenec-abi and the core each,
6.7 KB no stable flag merges; dlmalloc's 8.5 KB stays, against an
allocator of the module's own under memory a page never gets back.
Measured in turns against main, in process (load 3-5 from another
agent's YCSB through all of it, on both): a put -0.6 to -4.8%, a read by
id -1 to -3%, `in` over a hash index -6 to -22%, `lower()` and
`coalesce()` a row -3 to -4%, `match` -2%, a JSON page of ints -0.4%,
`make subquery-bench`'s union of 2 000 buckets 0.526 -> 0.444 ms; the
module in Node, interleaved over 21 rounds: `near`, a build, a filter,
`match`, a read by id and a page of vectors as JSON within 1.5%, a put
-3.3%, an `in` -6.1%; `make bench` at 30 000 x 128, twelve runs a side
twice, its writes and ANN within 1%. The same source took 19.6 KB of code off
`fenec-server` (2 407.4 -> 2 387.8 KB, `make size-report
BIN=fenec-server`) and 17.5 KB off the shell (1 526.9 -> 1 509.4).

## Chunked memory

**The browser module grows nothing by doubling past a mebibyte.** Its
memory is never given back, so a peak stays for good. An index's vectors
are `Rows` (`vector.rs`): natively a `Vec`, in the module chunks of about
`ROWS_CHUNK` (1 MiB), a whole number of rows each, reached through where
each starts (`starts`) in a load and one bounds check -- through the
chunks' own `Vec`s a 10 000 x 128 build took 11% longer, and `row` out of
line 5% (`#[inline(always)]`). A segment of documents doubles only up to
the record that seals it (`Store::append`), where it went to 12.7 MB for
8, and the lexer sets aside a token every four bytes up to 65 536 only: a
list of numbers is one token (`Tok::Vector`, in the module too now), and
a `put` of 1 000 768-dim rows had 39 MB set aside for 7 000. 5 000 x 768
written 1 000 a request held 100 MB and holds 58, written as text 130 and
62, 10 000 of them 150 and 84; a put of 200 of them as text went 10.1 ->
7.0 ms, and builds and searches as fast (`make wasm-speed`), for 768
bytes brotli. `web/fenec.test.js` holds an index over three chunks to the
exact search, before and after an image.
