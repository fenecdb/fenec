# Vector search

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Vector search*: the distance kernels, vectors as numbers, quantized codes, filtered `near` and sparse vectors. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## SIMD

**The wasm32 build has SIMD, and its kernels match the scalar ones bit for
bit.** `.cargo/config.toml` turns on `simd128` for that target, and
`vector::simd` holds the distance kernels written against it: the module is
built at `opt-level = "z"`, where LLVM does not vectorise the scalar strips
(a 20 000 x 384 HNSW build went 28.0 -> 9.95 s). They keep the scalar loop's
eight accumulators and reduction order, so a graph built in the browser is the
graph built natively; `web/fenec.test.js` checks that order against a
`Math.fround` reference. The same config links the module with
`--compress-relocations`: wasm-ld wrote each call's function index and each
address the code takes as a padded five-byte LEB, 36 KB of the module and
2.2 KB brotli, and V8, which tiers a function up by its size in bytes, runs
the smaller one faster -- `near` 12%, a page of vectors as JSON 15% (`make
wasm-speed`). A build that keeps the names strips the debug information
the flag refuses (`strip = "debuginfo"`, as `make size-report` does).

## NEON

**aarch64's strips are written out as well, to the same bits.**
`vector::neon` holds every distance strip on aarch64 -- f32, f16 and int8
codes -- over one `strips`, eight accumulators as two four-lane registers,
each lane doing the scalar loop's multiply and add (no fused multiply-add)
and the lanes summed in `strip8!`'s order. Left to the vectoriser, the f32
and f16 strips read through four-way de-interleaving loads into two-lane
registers, half of NEON's width: a 128-dim dot product took 21 ns in cache
against 9.4, and 91 against 60 out of an arena of 100 000; the int8 ones
chained every strip to the one before. f16 is widened with `half`'s own
masks, shifts and multiply, not NEON's conversion, which would turn
infinities and NaNs into what `half` does not.
`f32_and_f16_kernels_match_the_scalar_strips` holds them to the scalar
strips bit for bit. A 100 000 x 128 build went 5.2 -> 4.3 s (f16 6.0 ->
4.5), and `near` at 768 dimensions over f16 0.54 -> 0.34 ms p50. x86_64's
vectoriser keeps four-lane SSE registers and is left as it is.
A walk measures a node's unvisited neighbours four at a time there
(`distances4`, `Arena::dists_to`) and takes them in the order it did: read
together, four vectors wait on memory once -- out of an arena of 100 000 a
128-dim distance took 60 ns alone and 47 four at a time, a 768-dim one 242
and 149 -- so `near` at 768 dimensions went 0.35 -> 0.29 ms p50 over f16
and 0.35 -> 0.28 over f32, a build 3-10% faster, and
`the_walk_scores_as_the_exact_search_does` holds the walk's scores to the
exact search's bit for bit. The browser's module built a graph 4% slower
four at once and grew 9 KB, and gathering the neighbours before measuring
them one at a time still cost its `near` 7%, so everywhere but aarch64 the
walk is as it was (`cfg`). There the walk also asks for the next
candidate's list and the fresh neighbours' vectors ahead of reading them
(`prefetch`, a `prfm` hint): each read was a cache miss waited on after
the one before, and a 100 000 x 128 build went 3.96-4.25 -> 3.58-3.80 s,
`near` 0.10 -> 0.08 ms p50. The diversity heuristic measures a candidate
against four chosen neighbours at once (`any_nearer`), 5 to 9% off a build
at 768 dimensions. Neither changes a bit of the graph.

## Vectors out

**A vector goes out as its `f32`s.** JSON writes a vector's components
and a row's `_score` as the shortest text that reads back as the `f32`
(`json::num32_into`), as pgvector and a sparse vector's weights
do: `0.1`, where the `f64` each widens to wrote
`0.10000000149011612`. JavaScript reads it as an `f64` and a
`Float32Array` rounds that again, which gives every `f32` back but
`7.038531e-26`: as an `f64` it lands exactly between itself and the `f32`
above, and the tie goes to the even one, so it keeps its `f64`'s text
(`json::TIE`; `every_f32_reads_back_through_an_f64` tries all 2^32). The
digits of an `f32` from 2^-24 to 2^25 are exact 64-bit products
(`num::shortest`), where Ryu's multipliers took 128-bit ones that wasm
emulates (`__multi3`), and they go out in one `push_str` rather than a
`char` at a time: a page of 200 768-dim vectors as JSON, 43% shorter, went
27.7 -> 10.5 ms in the browser module, and a `near` answering ten 128-dim
rows 0.373 -> 0.226 ms (`make wasm-speed`), for 37 bytes brotli.

## Numbers in

**A number is read three ways, each exact.** `num::parse_f64` takes
Clinger's path where the digits fit in 53 bits, divides a whole number of
up to 19 digits by `5^n` for `10^-1` to `10^-25` (`divided`; `div128`
divides in two 32-bit digits, since a `u128` division took the compiler's
own, 1.4 KB, into the browser module), and shifts decimal digits
otherwise. JavaScript writes most floats, every `f32` it widens among
them, with seventeen digits, past Clinger's 53 bits. A page of 200
768-dim vectors took 74.6 ms to read in the browser module; divided, 51.9;
with the JSON reader walking the text's bytes rather than collecting it
into `char`s first, 23.7; read where the page left it rather than copied
(`str_from`), 21.9. 10 000 128-dim vectors went in with their graph in
1 281 ms against 1 706 (`make wasm-speed`), for 368 bytes brotli.
Natively an array of numbers alone is read straight into a vector's `f32`s,
each number in the one pass that finds its end where Clinger's path takes
it (`num::clinger`) and handed to the readers above otherwise: read a
`Value` at a time, each number's text found and then read twice over, a
128-dim vector took 5.26 us, now 2.05, and 100 000 128-dim rows without an
index go in at 220k rows/s over HTTP against 123k (`make load-bench`;
213k as REST arrays, 146k as a `/batch` of single puts now). A page's
vectors come into the browser module as `f32`s already, and the reader
stays out of it: there it was 474 bytes brotli for nothing.

## Vector parameters

**A vector parameter goes into the module as `f32`s.** `web/fenec.js`'s
`run` takes each parameter that is a typed array or an array of finite
numbers -- which the module reads as a vector either way -- out of the
JSON, where it leaves `null`, and hands it over beside it: its place, its
length and its values (`vectorsApart`; `fenec_query`'s two arguments
before its last, the clock; `with_vectors`). A `-0` goes over as `0`, as JSON writes it,
so the file is the same either way (`web/fenec.test.js`). 200 768-dim
vectors went in in 21.9 ms as JSON and take 4.4, and `make wasm-speed`
builds 10 000 128-dim ones with their graph in 1 165 ms against 1 307.

## Quantized indexes

**A quantized index holds codes, and `near` orders by the documents'
vectors.** `@hnsw(..., quant=int8)` keeps a byte a component over a scale a
vector, `quant=bit` the signs of its distance from a centre (cosine only). A
code only estimates a distance,
so `Space` (`engine.rs`) takes the beam's `ef` candidates and puts them in
order by the vectors read out of the store, as `rerank` does, so every score
is exact -- reading one only while it can still make the page. An int8 code
is off by its rounding, a step a component, and `VectorIndex::floor` is the
nearest its vector can plausibly lie (six standard deviations of that
rounding, wrong less than 1.5e-8 of the time); a candidate whose floor is past
the k-th exact distance held is neither tested nor read. At 100 000 x 768 that
is 16.8 of a beam of 100, and of 400, and 20 over a million, with recall
unchanged; a bit code bounds nothing and reads the whole beam. A filtered set under the ANN budget is
ranked by its codes and its beam's worth ordered the same way -- read whole it
was up to 12 800 vectors a query under bit codes -- and `exact` reads every
vector. Bit codes take the wider beam `BIT_EF_SEARCH` -- 200: over a million
clustered 768-dim vectors a beam of 100 held 97.9% of the true ten and 200
held 99.7% in 0.66 ms, int8 codes 97.1% at 100 (`make quant-bench`); over
the vectors' own signs the beam was 400, for 98.4% in 2.16 ms. The core settles it
wherever a spec comes in -- `VectorIndexSpec::default()` leaves `ef_search`
0 for `resolved` to fill by the codes -- since only the parser knew it once,
and the Rust API's bit indexes searched 100. How well bits estimate
depends on the vectors: spread in every dimension, 89.7% at 100 and 97.5% at
200. The code
kernels add in `strip8!`'s order on every target, so a graph over codes is the
browser's graph bit for bit; on aarch64 the int8 strips are NEON intrinsics
(`vector::neon`, as every strip there is), because the vectoriser widened
codes through a register it also accumulated in, which chained every strip to
the one before -- 5x slower, or not, depending on the code around it. A graph
over codes was record version 4, every other graph 3, so no file was rebuilt
for the feature; laid out flat (7), every record carries its quantization.

## Bit codes

**A bit code is a residual's signs, from a centre the index learned.** The
vectors' own signs were the code before: most of a crowded cluster's signs
are its centre's, and over 100 000 x 768 spread in every dimension a beam of
100 held 36.1% of the true ten, 400 held 74.9%. A bit index learns 128
centres by k-means++ and four Lloyd rounds over its first 2 048 vectors
(`BIT_TRAIN`), holding those whole until then -- an index under that
searches exactly -- in one order on every target, so the browser learns a
server's centres; the insert that learns them takes 195 ms at 768
dimensions. A code is the signs `b` of `r = v - C`, the centre's byte, and a
word after the signs holding `σ = |r|²/|r|₁` and `κ = ⟨C, r⟩ - σ⟨b, C⟩`;
`⟨q, v⟩` is estimated as `⟨q, C⟩ + κ + σ⟨b, q⟩` -- RaBitQ's estimator
without its rotation, the query taken from the centre so that only its part
off the centre goes through the signs (`⟨q, r⟩` estimated whole held
38.7% over centres that gave 92.8%). `query_for` appends the query's
product with each centre, once a search, and a distance between two codes
counts both `κ` (`Arena::offset`), which a widened code leaves out. The same
beams hold 89.7% and 99.6%, and over 32 directions a beam of 100 went 96.8%
-> 100% at 100 000 and 82.5% -> 97.9% at a million, for 105 bytes a 768-dim
vector against 96. In an array of their own the factors were a cache miss
more a distance: the build took 69.7 s against 62.8, a query at a beam of
100 0.414 ms against 0.378. `⟨b, q⟩` is counted, not summed: `query_for`
also cuts the query to six bits a component (`QUERY_BITS`), two's
complement bit planes after the centre products, and `dot_planes` takes the
planes' popcounts under the signs -- 72 over 768 dimensions where `dot_bits`
added a float a component, whole numbers exact on every target. RaBitQ cuts
to four bits the query's distance from a centre; cut whole, a query is
mostly its own centre, and four bits held 84% of a crowded cluster's ten
against the 91.5% six hold, as the query whole does. At a million a query
at a beam of 200 went 1.30 -> 0.66 ms at 99.8% -> 99.7%, filtered 1.36 ->
0.97, the build 1 012 -> 874 s; the browser module's 0.557 -> 0.470 ms at
20 000 x 384. The planes ride in the query's `Vec<f32>` as the bits of two
floats a word, since every search hands that vector on. The centres travel in the graph record behind
quantization code 3 (`BIT_CENTRED`): a graph over the plain signs (code 2)
is built again, and a binary from before them builds its own. The browser
module grew 6.9 KB, 2.2 KB brotli, most of it the k-means.

## Filtered near

**Filtered `near` needs its fallback.** The filter's rows are probed first -- in
blocks spread over the collection, and only until more than `ef × m0` match,
which is all the plan needs to know. A set that stays under that is searched
exactly; a larger one goes through the ANN with each candidate tested against the
filter. That second path *must* fall back to searching the whole set (the probe
carries on from where it stopped) when the result lands under the limit —
otherwise a filter correlated with the vector eliminates every candidate and
returns empty. The probe decides when rows are read, never the answer:
`tests/filtered.rs` checks it against the plan with the whole set found first.
That fallback, a set under the budget and `exact` read every vector they
measure, so natively they measure a share of 8 192 at a time on every core,
each share's nearest kept and all of those kept again in the shares' order
-- the rows, and the order of their ties, one walk in turn keeps
(`nearest_of`) -- and keep the nearest `k` as they come rather than sort
every one (`nearest`). At 1 000 000 x 128 a filter matching a quarter of the
rows, none of them near the query, took 20.8 ms and takes 5.1; `exact`
31.6 and 12.2, which is reading 512 MB. A scan asks for the vectors two
groups ahead of those it measures (`dists_to::<true>`), each a wait on
memory otherwise: 10 000 of a million 128-dim vectors in 0.41 ms against
0.82. A set an index names -- a hash bucket, the buckets of an `in` -- is
sized before it is gathered (`indexed_size`): past the budget the walk runs
with the filter as a row test, its beam widened until about twice the page
passes the filter, and where that beam would measure as many vectors as
the set holds the set is searched exactly at once, as the walk would have
ended doing. Gathered, a quarter's 250 000 ids -- copied, sorted, each
looked up -- were most of the 0.71 ms a filter keeping 25% of a million
128-dim rows took at a beam of 100, against 0.15 unfiltered, and at a beam
of 40, which a quarter passes about the page of, 43% of the walks came up
short and searched the 250 000 exactly (p99 8.9 ms); now 0.16 and 0.15 ms,
and a filter keeping 1% 0.69 against 1.22 (`make scale-bench`). The
browser module grew 2.9 KB, 1.0 KB brotli.

## Sparse vectors

**`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers exactly.** A
sparse vector is held as its non-zero entries, `(index, weight)` ascending
with indices from 0, and travels everywhere in pgvector's text form,
`{1:0.5,3:0.25}/N` with indices from 1 -- a JSON string, a FenecQL
literal, an imported `sparsevec` -- so a pgvector table and `fenec import`
carry the same vector.
Every way in goes through `sparse::normalise` (order, an index given twice
refused, zeros dropped), and the index relies on it. `@inverted` is the text
index's shape with weights where the counts were; `near` by dot product sums
the query's lists a list at a time into a slot a document, a window of 65 536
ids at a time (`sparse::summed`), where they are dense in the ids they span,
and walks them with MaxScore where they are not -- rank-safe, the bounds
compared once rounded to `f32` after a 1e-9 slack, so a tie-break never
depends on the pruning; the browser always sums, and has no walk. A SPLADE
query's dozens of lists reach most of a collection, and the walk, choosing a
document at a time among their heads, took 2.54 ms p50 on FiQA against 0.68
summed, 0.60 against 0.10 on SciFact; bounds per block of a list's postings
(2.69 ms) and per range of ids across the lists (443 of 451 ranges still read,
35 MB more) did not pay. `pruning_never_changes_the_answer` holds both ways to
an exhaustive walk, and `tests/sparse.rs` holds `near` to `near ... exact` row
for row. Only a document sharing a dimension with the query is ranked. Like
the text index it is derived and never persisted. It sorts through the
engine's one `(DocId, f32)` sort and maps dimensions through `DocMap`'s
`HashMap<DocId, u32>` -- its own were 12 KB of the browser module; the
feature cost 16.2 KB, 4.3 KB brotli, as it came, and `make wasm-sizes`
weighs it at 2.7 KB brotli added to the module with none and 0.9 left out
of the full one, the sort and the map being the other indexes' too.
SPLADE++ (`beir/splade.mjs`) scores
nDCG@10 0.693 on SciFact against `match`'s 0.662, and 0.331 on FiQA against
0.232 (the dense vectors 0.366), at 0.68 ms p50 over 57 638 documents: its
queries' 37 to 65 terms reach most of the corpus. Both BEIR scripts cut texts
themselves: transformers.js drops the closing [SEP] when it truncates, SPLADE
without it took SciFact to 0.23, and the dense vectors (`embed.mjs`) moved by
up to 0.003.
