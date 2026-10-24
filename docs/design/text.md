# Text

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Text*: collations, `nocase`, `match`, the text index, `fuzzy` and `prefix`, `rerank`, `fuse`, `highlight()` and facets. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Collations

**`collate und` and `collate tr` are ICU's orders, a query's or a field's.**
The weights are ICU's own for every assigned code point --
`crates/fenec-core/tools/collate/gen.py` reads ICU 76.1 out of macOS's libicucore into
`collate/`: the root cut into twelve chunks by script and each tailoring as
what it changes, with ICU's own contractions (a letter and a mark it
composes with, a two-part vowel sign, a Thai or Lao vowel written before its
consonant; up to three characters) and Hangul syllables decomposed into
jamo. ICU gives 133 535 primary weights, which no packed entry has the bits
for, so consecutive code points that alone give consecutive primaries share
a rank and are told apart by their code points (`BY_CODE_POINT`), a run of
them one range (`UNIFORM`), and a table's words are written as differences
in LEB128: 24 931 ranks, 154 KB for every script (324 KB plain), 161 KB of
`make small`'s 1331. A comparison walks ICU's three levels, letters then
accents then case, over the whole string before it falls back to the
bytes, so the order is total.
It starts at the first byte the two strings do not share, stepped back past
any character that can continue a contraction (`CONTINUES`; a fixed step is
wrong, Gurung Khema's overlap), and a Latin letter's element is made once
per collation (`Collation::latin`): 68 -> 30 ns before the root, and 33.5 ns
for `tr` and 35.2 for `und` now over a million Turkish names -- 41 while
every letter went through the tailoring's table and the root's chunk. Held
to ICU itself over 1 250 000 random pairs, five apart -- marks that compose
with their letter across another mark or out of canonical order, which
ICU's normalisation and discontiguous contractions find and fenecdb does
not -- and to `Intl.Collator` in `web/fenec.test.js`.
`order name collate und` names it for one key; a field declared `name text
collate und` has it wherever its text compares -- `order` naming none, `<`
and `>` in `where` (`RowAccess::text_order`), `min` and `max`, and a
`@sorted` index over it, whose chunks order through `sorted::Entry` with the
collation passed in. Equality stays the bytes' (a `nocase` field's is its
fold's, below). The schema writes a collated field behind type tag 11 (`TAG_COLLATED`) and the collation's code (1 `tr`,
2 `und`), which an older binary refuses rather than read the field in byte
order. A native build carries every
chunk. The browser module carries `latin` (9 KB) and is handed the rest
(`fenec_add_chunk`), which `make wasm` puts in `web/collate/`: a comparison
that meets a script it has not got notes the chunk (`collate::take_missing`)
and the statement is refused (`collate::refuse`) -- a read after it ran, a
write before it changes anything (`put`, `update`'s pass of its own,
`delete`'s filter), a load after it (`fenec_load`: its `@sorted` indexes
were built without the chunk) -- and `Fenec.query`, the builder, `restore`
and the sync layer fetch the chunks and run it again. So every stored
collated value's chunks are there, which keeps a `@sorted` index in order.
A chunk carries the tables' stamp, and another version's is refused. The
root cost the browser module 17.5 KB, 8.2 KB gzip and 6.8 KB brotli,
over the 10.4 KB `tr` and the field had; the other chunks are 145 KB, 67
KB gzip, 61 KB brotli, where plain words were 102 and 90.

## nocase

**`nocase` is a text compared by its case fold wherever it is compared.**
`email text nocase` (`Field::nocase`, schema tag 17 before the type,
`TAG_NOCASE`, so a binary from before refuses the file rather than take a
second `Alice@x.com` its `@unique` refuses): `=`, `!=`, `in`, `<` and
`>`, `~`, the keys of `@hash`, `@unique`, `@sorted` and a composite's part,
`order`, `group`, `count(distinct)`, `approx_count_distinct`, `facet`,
`min`, `max`, `first ... by`, a `lookup`'s child key and a scoped token's
rules all go through one rule (`Field::text_order`, a `collate::TextOrder`
of a collation and a `case::CaseFold`) -- an index option alone would have
left `=` comparing bytes beside a `@unique` that folds, and a `lookup` on
it finding what `where` did not. The text is kept as written; a group's
and a facet's value is the first row holding its fold, in id order (the
bucket's first member, read). The fold is Unicode's full case folding,
CaseFolding.txt's C and F -- Python's `str.casefold()`, which
`tests/nocase.rs` holds it to at every code point (`tests/casefold.txt`,
`tools/casefold.py`) -- with Turkish's T mappings under `collate tr`
(`ISTANBUL` is `ıstanbul`, `İSTANBUL` is `istanbul`). Full rather than
simple: simple folding keeps `ß` apart from `SS`, and would be a table of
its own, where the full one is the standard library's lowercase mapping,
already in the browser module -- and for the 297 code points whose fold is
not their lowercase (`UNLIKE_LOWER`, 41 runs, which a test derives from
the standard library again), its lowercase, uppercase and lowercase again,
`ı` left alone and Cherokee's small letters made its capitals. Nothing is
normalised: `é` and `e` with a combining accent stay apart, and a
full-width letter is not ASCII. A collated `nocase` field orders by the
collation over the folds, which ties exactly the texts that fold to one,
so its order and its equality agree. The indexes hold folds
(`HashIndex::key`, `folded_key`, `encode_folded` writing an ASCII text's
lowercased as it copies it; `sorted::Ordered`'s `fold`; `composite::Part`'s);
the bound `Filter` folds a value once a query and a row as it compares it
(`Test::Folded`, `case::cmp_to_folded`: the ASCII both start with byte by
byte, the rest a character at a time until the two part): folded whole as
a row was read, a scan for one name over 100 000 rows, a third past ASCII,
went 5.4 -> 17.0 ms, and goes to 6.8. An order folds its `nocase` keys
once a row in a pass after the reading (`folded_keys`, so an order over
none reads as it did), 8.5 -> 16.2 ms, where folding both sides at every
comparison was 17.4; a `@sorted` one is walked. A collection without a
`nocase` field reads and writes as it did (`make nocase-bench`'s plain
column against main's, in turns). Made `nocase` or no longer, a field is
rebuilt -- the declared schema's `nocase_changed` refusal names
`rebuild`, which a `@unique` refuses where two values become one. A
text's alone: a list's elements meet `has`, an index each and a facet a
value at a time. `fenec import` takes PostgreSQL's `citext` and SQLite's
`COLLATE NOCASE` as it. The browser module grew 11.5 KB, 3.7 KB brotli
(the engine's paths 6.4 KB, `case` 2.1); its scans of a plain field moved
within 2% of main's under load, once a `nocase` field's tests shared a
point's out-of-line arm of `Filter::test` and `compare` stopped asking the
order of a text `=` (an arm of their own and that question were 5 and 7%
of a count by numbers and by a text over 20 000 rows).

## MaxScore

**`match` prunes with MaxScore.** The exhaustive merge is not selective --
on BEIR FiQA the average query reaches 86% of the corpus -- so terms whose
remaining ceiling cannot beat the worst kept score stop driving the frontier.
Measured, with identical output: SciFact 179 -> 47 us, FiQA 1918 -> 311 us,
Turkish WebFAQ 401 -> 82 us. `pruning_never_changes_the_answer` compares it
against an exhaustive reference over 25 000 generated cases; scores accumulate
in `f64` so the two orders of summation agree once rounded back to `f32`.

## Tokenizer

**`@text` is word-boundary matching unless told otherwise.** `prefix=N` also
indexes each word's prefixes, which is a dictionary-free stemmer for inflected
languages: on Turkish WebFAQ it is worth +14.6% nDCG@10 for 3.4x the postings
(58 MB -> 182 MB, 82 -> 485 us per query). Off by default — the corpus
decides. The tokenizer also folds `I`, `İ`, `ı`, `i` onto one term, because the
locale-blind Unicode mapping turns `İ` into two code points nobody can type and
would hide every capitalised Turkish word. A script written without spaces
is split into runs of characters rather than words (`text::grams`): pairs of
Han, kana and Hangul, triples of Thai, Lao, Khmer and Myanmar letters --
pairs of Thai letters were too common to tell documents apart, 0.495 against
0.547 at a query three times as slow. nDCG@10 went 0.006 -> 0.439 on
C-MTEB's Chinese EcomRetrieval, 0.162 -> 0.868 on its CovidRetrieval, 0.119
-> 0.582 on the Japanese JaGovFaqs and 0.537 -> 0.682 on WebFAQ's Korean;
English scores to the fourth decimal what it did. `@text(chars)` adds each
character of Han, kana and Hangul, worth 0.439 -> 0.528 on product titles
and a little less on longer text, for 1.6 to 1.9x the postings: an index
kind of its own (7), as a quantized graph is, so a binary that knows no
`chars` refuses the file. The tokenizer takes its callback as `&mut dyn
FnMut`: generic, it was compiled six times, 8 KB of the browser module.

## Text index is derived

**The text index is derived data as well, but it is not persisted.** `@text`
builds an inverted index from the documents the first time a statement reads it
after an open — 16 µs per document against the HNSW graph's ~34 µs. Nothing
about it reaches the file, so there is no validation path and no stale-index
case to handle. It is shrunk to fit where it is known complete (its build,
`create index`); live ingest keeps `Vec` growth slack. Its terms grow a
256th at a time, as a hash index's keys do (below).

## Term table

**A text index's word is 9 bytes of a table, 16 of a term, and its bytes**
(`text/terms.rs`). A map of `String` to two `Vec`s, its hash beside it, was
a slot of 88 bytes and three allocations a word: a `--dir` node's tenant of
200 notes of 30 words held 565 KB of anonymous memory for its text index
over a Zipf vocabulary and 1 135 over made-up words (musl, in Docker, 500
tenants) -- 11 to 22 times the tenant's 51 KB, its hash and ordered indexes
4-5 KB each, the map's table 729 KB of the second -- and `memory_bytes`
counted 28% of the tenant, no table and no allocator. A slot is now a
word's place under half its hash, with a control byte; its bytes are in an
arena after their length; a word one document holds (three in four over a
Zipf vocabulary) is a `One` of 16 bytes, the document and its count, no
allocation; a word of more a `List` the slot names directly, as the map's
slot held it. The same tenants' indexes hold 192 and 225 KB, a tenant 594
-> 221 and 1 164 -> 255 KB, and 500 of them building their indexes as they
were written 375 -> 173 and 703 -> 172 MB; in musl's slots (`make
text-memory`) SciFact's index 15.0 -> 10.4 MB and FiQA's 75.8 -> 66.5.
`memory_bytes` counts the table, the arrays and what each list's allocation
takes past its bytes (`ALLOC`, 12): within a tenth of musl's slots where it
was a third (a tenant's notes) to four fifths (FiQA), and 79 and 86% of a
tenant's anonymous memory. A word that goes leaves its place to the next
(`free`, and `spare` for a list, its arrays kept to 16 documents: moved
into by the last, its slot found by its hash again, a browser's update of a
note took 12% longer) and its bytes dead in the arena, written again once
half of it is; a slot that goes moves the ones after it back, no tombstone;
a build closes the places up and shrinks every array. A probe reads a group
of 8 slots by their control bytes, as hashbrown does without SIMD
(`GROUP`): a slot at a time, a word past its own slot cost a mispredicted
exit and `make search-bench`'s build took 149 ms against the map's 142 --
though FiQA, its common words in their own slots, built in 505 against 536
by groups. A word is compared by its length and first seven bytes as one
masked word (`Key`): through `memcmp` a length's branch mispredicted at
each. Past 98 304 words natively the table, arrays and arena are 256 shards
by the hash's top byte, as `maps::Sharded` splits a map: `make growth-bench
GROWTH=text`'s put p50 1.21-1.25 -> 0.83-0.88 us, p99 2.9-7.1 -> 1.6-2.0,
the split's 5.2-6.3 ms -> 3.4-4.7, the index built after an open 1.8-2.6 s
-> 0.46-0.58. In ABBA turns against main, load 2-5: search-bench's build
140-146 -> 139-142 ms and `match` 0.054 either way, SciFact's build 90-91
-> 87 ms (one run of six 94) and FiQA's 559-565 -> 535-539, `match` over
them 4-6% quicker, 400 000 words each in one document 113-178 -> 38-47 ms,
BEIR's nDCG@10 the same to the fourth digit and its loose `match`es as
quick or quicker, an update of a FiQA document 1% slower on macOS (306-309
-> 310-312 us, though its own instructions only +0.23%: the same binary
search, out of line in both, 5-8% apart in samples, and `#[inline(never)]`
on main's own `drop_doc` moved main's up to 0.8%, its removals 0.4-1.2%; a
list's record a cache line, its word's head in it, or its drop out of line,
no nearer) and the same on Linux in Docker's VM, in turns (musl 1 505-1 521
against 1 509-1 514 us, glibc 2 100-2 160 against 2 105-2 161), and of a
SciFact one the same; the browser module's build of SciFact 151-154 ->
145-146 ms (one run of six 155) and of 20 000 notes of made-up words 75-77
-> 32, `match` 3-7% quicker, `make wasm-speed` as it was, for 2.2 KB brotli
of the module, 3.1 KB raw (hashbrown's copy for the map, 3.0 KB raw, gone).
Rejected: control bytes in each group's cache line beside its slots, FiQA
533 ms and search-bench 147; a group's tags compared one by one, 171; the
first two slots picked without a branch, 155.

## fuzzy and prefix

**`match ... fuzzy` and `prefix` walk a sorted copy of the index's words**
(`fuzzy.rs`, `TextIndex::search_loose`). `fuzzy [N]` reads a query's word
within the edits its length allows -- none under 5 characters, one up to 8,
two from 9, Meilisearch's bounds, `N` lowering them -- in restricted
Damerau distance (a swap one edit, as Lucene, Elasticsearch and Meilisearch
count it), an edit touching a word's first character counting two
(Meilisearch's rule: allowed at one, the walk took 4.6x the nodes for 9%
more words, mostly words no one meant); `prefix` reads the last word as the
start of one too. The words are a `Dictionary`: the index's terms but the
`^` ones, in byte order, in chunks of up to 256 a byte arena and their ends
each (1.3 MB for 60 000 words, where a `Box<str>` a word was twice that and
25% slower to walk), sorted by the first loose `match` -- a `OnceLock`, a
`OnceCell` in the browser, as `Derived` is -- and kept up by every write
after it, never in the file. A prefix is a range of it; the words within
`k` edits are found walking it as the trie it is, a row of the table a
character banded to `2k + 1` cells, a node's children by binary search --
only the query's characters near the depth where any other would end the
row -- and a branch with no edit left looked up as the rest of the query
rather than walked (a swap, counted from the row above, keeps it walking
one row more). Over FiQA's 135 500 words, 2 000 typo'd words a bound: 14 us
a word at one edit, 120 at two, 4 a prefix; SymSpell's deletions 9 us but
304 MB and 2 s to build, an index of trigrams 8.4 MB and 316 us and no
prefixes, the table over every word 4.3 ms; comparing through `memcmp` was
a third of the walk. Each word's variants of one distance are one more
term -- their lists merged two at a time (`union`), a document's counts
summed, so BM25 saturates over them and their idf is theirs together, a
rare misspelling ranking no higher than its word -- at the word's weight
one edit away or begun, a quarter two away (`NEAR_WEIGHT`: on BEIR with a
typo in each query, the whole weight for one edit took SciFact 0.636 ->
0.663 where half took it to 0.657, a quarter for two took FiQA 0.213 ->
0.229 where the whole took it to 0.226). Where the index holds the word as
typed, its variants together are scaled below the least it adds to a
document -- once, in the longest document there is (`longest`, only
raised) -- so a one-word query ranks every document holding it first, and
queries written whole lose nothing (0.662 -> 0.665 and 0.232 -> 0.233;
unscaled, at half the weight a word one edit away, 0.656 and 0.229). They
are cursors as any term's, so MaxScore stays rank-safe
(`pruning_never_changes_a_loose_answer`). A word reaching
more than `MAX_EXPANSIONS` (10 000) words is refused, never cut: FiQA's
80 426 words hold no one-letter prefix past 7 020. `highlight()` marks the
variants (`Terms::add`), `facet` counts their rows (`matching_loose`). A
plain `match` takes the path it did, `search_within` split into
`query_terms`, `cursor` and `ranked`, the last in line natively: FiQA's
0.261 ms p50 on main, 0.257 here (0.269 called), SciFact's 0.042 either
way, `make search-bench`'s 0.056 ms and `make wasm-speed`'s `match` 0.81 ms
either way. Core, in `text` and no feature of its own -- a search box is no
domain's extra: the browser module grew 17.1 KB, 6.0 KB brotli, the
standard library's sort for `&[u8]` (3.7 KB) a merge of its own there.

## rerank

**`rerank` deliberately uses no index.** `match ... rerank` takes candidates
from the inverted index and reorders them by exact distance over vectors read
straight out of the store — so a collection can do vector retrieval with no
HNSW graph to build, hold, validate or rebuild. Measured on BEIR it matches or
beats a full dense scan while scoring under 2% of the corpus. It requires
`match`: without candidates there is nothing to reorder.

## fuse

**`fuse` adds ranks, not scores.** `match ... near ... fuse` runs both searches
to their own depth -- `candidates`, 20 unless given, never under the page --
with the filter applied to each, and a document scores `1 / (k + rank)` from
each list it is on (`k` 60). A BM25 score and a cosine distance share no
scale, and a weight between them would need retuning per corpus. Measured
with `make beir` (nDCG@10): SciFact 0.700 against 0.662 for `match` and 0.645
for `near`; FiQA 0.366 against 0.232 and 0.366 -- the one path near the top
of both. The depth is the knob that matters: up to 61 a side a document both
searches found outranks every document only one found, and at 100 a side
both scores fall (0.688, 0.359). It is built from what the engine already
had -- both searches, the `HashMap<DocId, u32>` a `DocMap` keeps sparse ids
in, the text index's `best_first` sort -- because in types of its own it was 11 KB of the
browser module; this way it is 2.

## highlight and facet

**`highlight()` reads the text again; `facet` counts beside the rows.**
`highlight(body)` and `snippet(body, N)` are select-list items (`Mark`,
its arguments one `Vec<Expr>`: Select's clone of them as fields was 432
bytes of the browser module), answered under their label, after the fields
or after `*`; the same one twice is refused, since a JSON row holds a name
once. They need `match`, and go with `fuse` and `rerank` -- a row only
`near` found has no marks. Nothing is kept in the index for them: the
row's text is split again by the tokenizer of the `@text` index the
`match` searched (`text::terms` hands each term its byte span, which the
index passes over: the build and `match` measured as before),
`highlight::Terms` holds the query's terms in the text index's own map (a
sorted `Vec<String>` was a sort of its own, 3.5 KB), and a span is marked
where a term the text yields is one of them -- so a mark is what the index
matched: a word whatever its case, the whole word a `prefix=N` term was cut
from, the characters of a Han or Thai run's matched pairs or triples,
overlapping ones merged. A span is widened over the marks of the characters
at its ends (`highlight::extends`, the blocks a text puts combining marks,
joiners, selectors and Hangul jamo in, not Unicode's whole table), and the
offsets go out as UTF-16 code units, what every client but Go's and
Python's indexes a string by. Without tags a column is `[start, end]`
pairs, a snippet's `{marks, text}`; with them the marked text, not escaped.
A snippet is the window of `N` words (a Han character a word) with the most
marks, centred on them. `facet f [top N]` counts over the rows the query
selects before `offset` and `limit` -- the filter's, `match`'s
(`TextIndex::matching`, every row holding a term, the filter tested on
those), a required `lookup`'s -- and is refused beside `near`, which ranks
every row the filter passes rather than selecting some. A `@hash` field
of text, int, bool or timestamp, whose bucket key is the value's encoding,
counts from the buckets (a bitset of the set when there is a filter); the
rest reads the field of every row, a list once a row for each value, null
a value, counted under `codec::encode_value` as `group` keys its rows, so
`tests/facets.rs` holds a scalar facet to `group ... count` over the same
filter. Ordered most first, then by value in the field's collation,
through `order_rows`; past `MAX_FACET_VALUES` (10 000) without `top`
refused, never cut. The counts live in `ResultSet::facets`, beside `nested`,
never in a `Value`, and a JSON answer carries them as `"facets"` after the
rows -- over HTTP the bare array becomes `{"rows", "facets"}` only when a
query asked for them. Over 100 000 documents a row's marks cost 1 to 3 us,
a snippet's 2 to 4; a facet over every row 0.01 ms through `@hash` and 6.1 by
the scan (`make search-bench`). The browser module grew 20.2 KB, 7.0 KB
brotli; the one without indexes 3.9 KB brotli, which counts facets and
refuses a `match` and its marks.

## Disjunctive facets

**A disjunctive facet's filter is split where the filter is written.**
`facet brand disjunctive` counts over the rows the query selects with the
`and` chain's terms that read the field alone left out (`Facet::rest`,
`Select::split_facets`); a term reading it beside another field is
refused, not guessed at. The split is the parser's and a REST `facet=`'s,
from the filter as written: `scoped()` and `@ttl`'s `alive` reach the
split filters through `each_filter_mut`, so a token's rules and an expiry
are never what is left out -- split after them, `facet owner disjunctive`
counted every user's rows (`tests/access.rs`). A select built in code is
split in `answered_select`, before the expiry; one that leaves nothing
out is a plain facet. Each disjunctive facet finds its own rows
(`facet_set`: `matching_ids`, `match`'s `matched_set`, a required
`lookup`) in the same statement under the same lock. `facet price ranges
[0, 2500, 5000]` counts a number by range, every range in order, value
`[from, to]` (`facet_ranges`): through a `@sorted` field's index when every
bound is a key it holds exactly (`SortedIndex::range_counts`: over every
row two searches and the chunks' lengths between, `Chunked::count`; under a
filter its entries walked against the set's bits) and by reading the field
otherwise; the lexer keeps `ranges [..]` as written, as it keeps `in
[..]`, an `f32` vector holding no bound past 2^24 exactly. Over 100 000
rows: six ranges 0.004 ms through the index against 4.7 read, 0.15
against 0.19 under a filter keeping 3%; a page with two disjunctive facets
0.75 ms in process as its three statements were, over HTTP 0.95 against
1.19 in turn. The value-count scan keeps its own loop (`each_value` serves
the ranges): through a shared one, inlined and all, it went 4.8 -> 5.2
ms, and with `facet` out of line 4.8 -> 5.6 (`#[inline(always)]`). The
two cost the browser module 2.9 KB brotli -- the split, the ranges'
read and index paths, the parser -- kept in it, so a replica or a page
answers a shop's sidebar as its server does.
