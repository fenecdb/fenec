# lookup and aggregates

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *lookup and aggregates*: `lookup`, `required`, keys of several fields, folded levels, select-list expressions, HyperLogLog and time zones. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## lookup chains

**`lookup` chains, and the chain is still positional.** `lookup a ... lookup b
...` hangs `b` off `a`'s rows: what follows a `lookup` binds to *its*
collection, and `on child = parent` names a field of the level immediately
above -- so exactly one collection is in scope at any point and it is the last
one named, which is what keeps qualified names out of the language at any
depth. `Nested` holds the tree one level at a time: a level's `groups` has one
entry per row of the level above, read left to right and concatenated, so the
alignment is the same sentence at every depth and a level costs one vector
rather than a node per row. A collection may appear once per query, at any
depth -- put the driving one back in scope two levels down and `on child =
parent` reaches a level that could be either. The second level is what a
`/batch` cannot answer in one round trip: its keys live in rows that have not
come back yet. Over 2 000 / 20 000 / 200 000 for a 20 x 3 x 5 page, 49.1 us in
process against 139.8 us for the same page as 81 separate queries, and 0.204 ms
for one HTTP request against 0.415 ms for two `/batch` trips. `required` stays
a statement about its own level -- it drops rows of the level immediately above
and stops there, so it composes by being written at each level rather than by
reaching down.

## lookup

**`lookup` is one bucket probe per parent, not a join.** It attaches another
collection's matching documents to the row they belong to, and a `limit` after
it counts children *per parent* -- the shape a join cannot express. It is
terminal in the grammar: everything before it binds to the driving collection,
everything after to the looked-up one, which is exactly why there are no
qualified names (`reviews.stars`) in the language and no filter splitting in
the planner. The child key must be `id`, carry `@hash` or lead an index over
several fields (below); refusing an unindexed one follows `near` and `match`,
because a silent full scan of the child collection would be a different
feature under the same name. Nesting
lives in `ResultSet.nested`, never in `Value` -- JSON transports nest all the
way down, and a flat one flattens (`ResultSet::flatten`: one row per
root-to-leaf path, nulls below the first level that ran out); a
`Value::Object` is a `json` field's value, not a level of children.
Measured at 22.7 us in process against 0.332 ms for the same
page as a `/batch` of 21 queries merged on the client.

## required

**`required` is the other half, and it is a pass.** `lookup ... required`
drops a parent no child matches, tested before `offset` and `limit` so the page
still fills. It is also the only shape where `count` combines with `lookup`,
since nothing is being attached. There is no index from "a child matching this
filter" back to its parent, so every candidate parent is probed -- it stops at
the first child that passes, which is why the unfiltered form is an order
cheaper. It is answered from whichever side is smaller -- walk the parents probing each,
or read the children a `@hash` equality in the child filter names and take their
parents -- and the counts that decide (candidate parents, bucket length, child
collection size) are all exact, so the choice is derived rather than estimated:
child-driven when `|ids| * n > |bucket|^2`. Over 20 000 parents and 200 000
children that is 12.07 ms against 15.63 ms for the parent side, 2.19 ms with no
child filter, and 0.02 ms for a `bool @hash` field on the parent maintained on
write. Ad hoc, use `required`; on every page load, use the field. With
no `order` and no facet the page is the first parents kept in id order, so
the parent side stops once it holds them (`retain_with_children_to`): it
probed every parent the filter passed, a page of twenty over 20 000
products 1.41-1.43 ms, and probes until twenty are kept, 0.036.

## lookup over several fields

**A `lookup` over a key of several fields is a run of an index over several
fields.** `on (org, order) = (org, id)` pairs a child field with a field of
the level immediately above for each part, the positional rule unchanged;
`Lookup::on` and `parent` are two lists of names, since a `Vec<String>` is
code the browser module had and pairs were a clone and a collect of their
own. The child side is a `Tuple` (`engine.rs`): the child's index over
several fields whose first parts are its fields in any order
(`Tuple::resolve`, one the level's order walks before the first), refused
otherwise naming the `@sorted(...)` to declare. A parent's values, each made
its child field's type as a single key's is and laid end to end by the
index's own encoding (`Tuple::prefix`, `CompositeIndex::key`), are the start
of its children's keys, so they are one span, two searches -- and equality
is the index's: a `nocase` part finds every spelling of its fold, a collated
one its bytes, as a `@hash` key does. Where the level's `order` is the
index's next parts, in its directions or every one reversed (`walks`, which
`walk_composite` shares), each parent's run is walked in it and stops at
`offset + limit`, ties in id order; otherwise read, put in id order and
sorted as a bucket is. One field leading such an index goes the same way
where it has no `@hash` or the order walks it -- "the latest three reviews
of each product" over `@sorted(product_id, created desc)`. `required`'s
child side, where a part pairs with the parent's `id`: each candidate child
names its parent by that part and is held to the rest of the key by its own
parts' bytes against the parent's (`Tuple::holds`); probing each parent so
named for the whole key read every child again -- 11.7 ms against 5.7 for 20
000 orders whose 200 000 lines a tenth named, the parents' side alone 9.5 --
and mapping the keys back whole sorted `(u64, KeyBytes)`, a sort of its own,
4.5 KB of the module. Without `composite` a `Tuple` holds `off::Never`
(`Built`), so every path through one is dead code, and a key the index would
answer is refused naming the feature. `tests/composite_lookup.rs` holds
every lookup over several fields -- order, ties, page, `required` at each
level and counted, three levels, `flatten` -- to a reference worked out from
plain `get`s over a twin without the indexes, through writes, key parts
moved on both sides, a block read inside and put back, a reopen and rows
past their `@ttl`. A parent's run read forwards over every part -- the
probe, `required`'s witness, a walk whose order is the index's whole -- is
one search and read until a key leaves the prefix (`CompositeIndex::run`): a
span searched for its end too, a score of comparisons of keys on the heap,
and the page below took 125 us against 96. A tenant's 20 shops x 3 latest
orders x 5 lines, over 2 000 shops in 100 tenants, 20 000 orders and 200 000
lines (`make lookup-bench`): 89 us by `(org, shop)` and `(org, order)`,
against 123 through the key joined into one `@hash` text field of each side
-- kept up by hand on every write -- 99 through `@hash` keys that leave the
tenant out, and 64 through those keys leading an index walked; at 500 orders
a shop, 146 us against 1 987, 1 962 and 56, the buckets read whole and
sorted for three. A single key's page as it was: 89.2 us on main and 90.2
here at the quiet runs' median, their instructions retired within 0.2% (the
key a value or an index's bytes through one enum, it was 1 to 2% slower:
`Step::ids`, `Step::any` ask a `Tuple` apart). The browser module grew 8.5
KB, 3.1 KB brotli (`Tuple::resolve` the most of it, 1.7 KB), the one without
the indexes 2.5 KB, 0.8 KB brotli, and the one with no feature 2.5 KB, 0.9
KB brotli: the parse, the check and the refusal by name.

## Folded levels

**A `lookup` level may fold its children, and a `lookup` hangs off
groups.** `Lookup` carries `aggregate`, `group` and `having`: a level whose
list aggregates (`Lookup::folds`) answers each parent's children -- probed
once by the code a plain level runs (`kept_children`), held to its `where`
and to any `required` level below -- folded by one `Folding` bound for the
level (`engine/aggregate.rs`: the list, its keys, its `having`, the `Fold`
each item starts at, which a `get`'s aggregate goes through too), its
`Groups` emptied a parent. A level is the `get` its children would be with
the parent's key in its `where`, so its clauses mean what they mean there:
ungrouped, `order`, `limit` and `offset` pick the children folded -- a
courier's latest 50, walked off `@sorted(courier, at desc)` to the page --
and grouped they order and page each parent's groups (a level that groups
takes no walk: its `order` names columns). A `get`'s ungrouped aggregate
takes them the same way now (`page_ids`, the `lookup` left out), where
they were refused, and a column of the list among its keys is refused
(`picks_by_rows`). A `lookup` after a list that aggregates -- a `get`'s or
a level's -- hangs off its rows: `Parents::Rows`, the key read from a
column (`Above::Rows`, `Step::ids_of`, `Tuple::prefix_of`), which must
carry a field's values -- a key, its `bucket`, its `min`, `max`, `first` or
`last` -- so the key's types are held before a row is read
(`check_key_types`). `required` on a level that folds asks of its
children, as on any level (an ungrouped list answers a row for every
parent, so asked of its rows it would keep them all); a `required` level
below one that folds drops its groups (`Folding::rows`'s `keep`), before
the order and the page, and `survives` does not reach past a level that
folds. Grouping parents by a field of theirs beside their children's folds
would be two collections in one list -- the qualified names the scoping
keeps out -- and is no shape. `tests/lookup_aggregate.rs` holds every fold,
page and level to a reference folded by hand from plain `get`s over a twin
without indexes, through writes, a block read inside and put back, a reopen
and `@ttl`; dropping the level's `offset` or letting `survives` past a
folding level each fails it. Over 2 000
couriers and 200 000 deliveries, a page of the 20 busiest with each one's
latest 50 folded takes 53 us in process against 81 for the 50 attached and
folded by the client, every delivery of each 3.5-4.1 ms against 6.4-6.5,
and the ten busiest with their names off a week's groups 8.3 ms, as the
groups and a query for the names apart do less the round trip between them
(`make analytics-bench`'s `folds`). The `get`'s aggregate through the same
`Folding` lost nothing: the aggregates of before over a million rows, eight
runs each in turns against main, 52.0 -> 49.0 ms in 100 groups, 29.6 ->
29.2 whole, 5.5 -> 5.2 over a range; the lookup page (`make lookup-bench`)
89.9-94.4 us against 90.2-93.8 by `@hash`, the same within its spread for
the composite and the walk. An ungrouped level's one row is not put in
order: through `page`, a page of 20 parents took 24 us against 18. The
browser module grew 11.6 KB, 4.0 KB brotli -- the `Folding` split out of
`select` into functions its two callers share, `fold_level`,
`kept_children`, the key read off a column -- and the builders took
`select` of aggregates and `group` on a level, held to the golden file.

## Select expressions

**An item of a select list is an expression, and an aggregate folds
one** (`engine/aggregate.rs`). `Select::aggregate` and `computed` are
`Column`s, an expression and the name it answers under (`as`, or for a
field or an aggregate of one the text it was written as, `label_of`;
anything else is refused without `as`, since a name would have to be the
expression written back), and `group` is a list of expressions, a name of
the list standing for its item (`group_keys`). Aggregates are calls, so no
walk over an expression grew a variant: `count()` is `count(*)`,
`count(distinct e)` a call of `distinct` inside `count`, `first(e by k)` a
call of two, and `case when ... end` a call of its conditions and values,
which `eval` works out lazily; `bucket`, `greatest` and `least` are
registry functions, so `where` and `set` have them too. A key is matched to
an item by being the same field or the very item its name gives
(`is_key`), not by comparing expressions -- `PartialEq` over `Expr` was 1.5
KB of the browser module. `Reader` binds a list once a query: every field
and path read decoded in one pass into slots, `bucket` over a constant
interval and any other expression (its fields made parameters past the
query's own, `Expr::rewrite`) worked out into slots of their own once a
row, so a fold reads a `&Value` by its place. An expression over
aggregates (`sum(px * qty) / sum(qty)`) is the same rewrite over the
group's keys and folds. `count(distinct)` keeps every group's values in one
map of the hash index's type, refused past `MAX_DISTINCT_VALUES` (a
million, about 70 MB), never counted short; `first`/`last` keep the row
least/greatest by `(k, id)`, boxed so a fold stays the size it was, and
both are folded out of line (`row_held`): in the loop the fixed
aggregates took 15% longer. Natively `Fold::add` is inlined and the
paths' and expressions' reading out of line: as they first stood, a
million rows in 100 groups took 52.8 -> 54.5 ms and the whole set 32.1
-> 33.9; now 51 and 31. A `count(*)` by `bucket` of an `@sorted` or
`@ttl` timestamp or int whose filter is a range of it and nothing more
reads the index's keys and no row (`counted_by_index`, native only): a
day's per-minute counts over a million events 19.5 ms by a stored minute
field, 4.0 now. One-minute bars of a symbol for an hour out of a million
ticks, 1.7 ms where open and close were 120 queries, 195 ms (278 over
HTTP); VWAP of 50 symbols 118 ms against reading every tick out, 173;
distinct users an hour over a week 125 ms against 7.7 s (`make
analytics-bench`). The browser module grew 19.5 KB, 6.8 KB brotli, the
test build 6.6: the parser's calls and `case` (2.2 KB raw), binding and
folding (8 KB), the checks and names (1.4 KB), `bucket`'s calendar.
`site/content/docs/analytics.html` is the recipes, each FenecQL block run
by `tests/analytics_docs.rs` through `redis_docs.rs`'s runner.

## HyperLogLog and having

**`approx_count_distinct` is HyperLogLog, and `having` keeps groups**
(`hll.rs`, `Fold::Hll`). `count(distinct)` stops at a million values, and
a day's count cannot be added to the next: Kestrel's month had 2.4 million
visitors. A sketch is 2^14 registers, a standard error of 0.81% (the root
mean square over thirty sketches of 200 000 values is 0.77%, and every
count from 1 to 2 000 000 is within 3% in `hll::tests`), read by Ertl's
improved raw estimator -- no bias tables, no switch to linear counting --
and sparse, `(index, rank)` words, until those would take a quarter of the
16 KB dense ones, so a group of a few values costs a few words. Its hash
is its own (`hll::hash`, MurmurHash3's finalizer a word), over the value's
encoding as `count(distinct)` keys it, the same on every target, so a
sketch the browser made merges with a server's -- `-0.0` folded onto `0.0`
since the SQL spellings' review (`codec::encode_key`), so a sketch
`hll_accumulate` stored before, merged with one made after, may count the
two as two values. `hll_accumulate(e)`
answers the sketch as bytes (`Sketch::to_bytes`, sparse while shorter),
`hll_combine(s)` merges them and `hll_estimate(s)` (a registry function)
reads a count off one; a bytes field takes the list of numbers JSON writes
bytes as, so a sketch read out goes back in through `put $1`. A query's
sketches are held to 64 MB (`hll::MAX_BYTES`) and refused past it, as
`count(distinct)` is. The fold goes through `row_held` with the distinct
one, out of the loop the fixed aggregates fold in. Per hour over a million
events it takes 121 ms against `count(distinct)`'s 128, the furthest of 169
hours 1.3% off (`make analytics-bench`). `having <expr>` after `group`
(`Select::having`) is worked out a group as a column is, its own
aggregates folded after the list's and the list's columns read by name
(`grouped` checks it with them as keys), and a group it does not hold for
is taken back out, keys and all, before `order` and the page; `count`
after `group` counts the groups that pass. The ordered funnel -- each
visitor's first start and first finish, `having b >= a count` -- is one
row where every visitor's row left the node: 968 ms of Kestrel's month at
ten million events. The browser module grew 9.1 KB, 2.9 KB brotli, the
estimator, the sketch's bytes and `having`'s rewrite most of it.

## Time zones

**`bucket(at, 1d, 'Europe/Berlin')` reads a zone's clock** (`tz.rs`). A
third argument names the zone: an offset written out (`'+03:00'`,
`'-0530'`, `'Z'`, `'UTC'`) or one of IANA's names, read from a table of
their rules since 2000 that `tools/tz/gen.py` writes out of the system's
compiled tzdata (`tz/zones.bin`, 2026c: 597 names in 196 tables, a zone the
offset it had at the start of 1999, its transitions since that its TZif
footer's POSIX rule does not make, and that rule from its first change --
not from the year's start: Turks and Caicos kept -4 into 2018 -- 30.6 KB,
10.1 gzip, 8.5 brotli). A day, a week, a month or a year starts at local
midnight, the first instant whose clock reads it or jumps past it, so a
day is 23 or 25 hours where daylight time begins or ends; a shorter
interval starts at the last instant at or before the time whose clock read
its start or jumped past it, so an hour a clock shows twice is two buckets
and one it skips none. Both are a walk over the zone's pieces (stretches of
one offset) near the time, the last piece asked kept (`Rules::last`): rows
in time order ask one piece for months of them. A time before 2000 in a
named zone is refused rather than answered by rules the table does not
hold; an offset answers any time. Native builds carry the table
(`include_bytes!`); the browser module is handed it as it is handed the
collation chunks -- listed after them as `zones` by `fenec_collation`,
taken by `fenec_add_chunk`, `make wasm` putting it in `web/collate/` -- a
statement naming a zone before refused with `chunks: ["zones"]`, which
`Fenec.query` fetches and runs again (8.5 KB brotli of every page
otherwise). A select list binds a constant zone once a query (`Calc::Bucket`
with the zone); a `where`'s `bucket` keeps the zone it named last
(`tz::cached`); the count off an ordered index looks a zoned group up by
its key, since such a bucket may come back (an interval cut across a
fall-back), where UTC's walk asks none. `tests/zones.rs` holds every zone's
offsets, sampled over 45 years and hashed, to zoneinfo's, and the start of
seven intervals around the transitions of twelve zones -- midnight skipped
(Havana, Santiago, Beirut), a day skipped (Apia, 2011), half an hour (Lord
Howe), daylight time in winter (Dublin), Gaza -- worked out in Python by
the definition, not the walk (`zones.txt`), through a select list's, a
`where`'s and the index's paths. Over a million events, a week's days
counted off the index by Istanbul's clock take 29-34 ms against 23-27 in
UTC, where the offset written in -- `bucket(at + 10800000, 1d)` -- read
every row in 133-154; hours by Berlin's 31 against 25 (252 before the piece
was kept); with a filter that reads the rows, 94-103 against 91-98. The
table is read where it lies (a header, offsets and rules as records of a
fixed size, an index of entries): parsed into lists, its names front-coded,
the reader was 7.5 KB of the browser module against 5.9, for a table 5.4 KB
smaller. The module grew 7.8 KB, 3.1 KB brotli, with the table out of it;
every native binary carries the table's 30.6 KB.
