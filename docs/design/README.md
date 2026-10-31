# Design notes

The long form of the invariants in [CLAUDE.md](../../CLAUDE.md): what each measured, the alternatives it rejected and how it came to be. CLAUDE.md keeps each invariant's rule in a few sentences and links to its section here, and the paragraphs under the headings in these files were moved from it word for word, but for two facts the review of the move corrected: the collate generator's path in [text.md](text.md#collations) and `fenec-abi` beside `fenec-ql` in [browser-module.md](browser-module.md#index-features). An invariant added or changed is written in both, in the same pull request.

The invariants were written as one sequence, and "above" and "below" in them refer to that sequence, not to the file a paragraph is in now: [the original order](#the-original-order) lists it, each with the file that holds it. The files below are in the order of the first invariant each holds.

## Files

- [crates.md](crates.md) -- zero dependencies, limits that error rather than truncate, and the build profiles
- [storage.md](storage.md) -- the mapped file, the handover of writes to it, a block's spills, compaction and the file format
- [engine.md](engine.md) -- the single writer, a block's documents built beside the readers, long reads pinned, and a block as one record
- [durability.md](durability.md) -- a storage error stopping writes, buffered writes written behind the lock, the sync log, and the one shutdown
- [replication.md](replication.md) -- replication, a node's tenants archived over one stream, `/_changes`, `Fenec-After`, sealed archives and `--follow`
- [tenants.md](tenants.md) -- scaling out by tenant, the router's directory, moves, failovers and leases, and replicas tried again
- [regions.md](regions.md) -- regions and zones, a region's own directory, and the global entry
- [read-replicas.md](read-replicas.md) -- a tenant's read replicas in other regions, the reads marked for them, and reading one's own writes across a failover
- [access.md](access.md) -- scoped tokens, field lists, rules reading another collection, a token bound to its tenant, whom a token is for, and empty tokens
- [quotas.md](quotas.md) -- a tenant's quotas, how the sync meets them, and the wait every 503 names
- [http.md](http.md) -- the studio, MCP, idempotency keys, the audit log, request ids, traces, metrics, statement counts, the parse cache, JSON written from stored rows and connections no thread is made for
- [hnsw.md](hnsw.md) -- the graph as derived data, its records, its restore and open, its linking, and vectors written again
- [benchmarks.md](benchmarks.md) -- what `make scale-bench` and `make ycsb` measure and how
- [writes.md](writes.md) -- `insert` and `put`, arithmetic in `set`, `require`, upserts, picked writes, an `order` by expression and `returning`
- [fenecql.md](fenecql.md) -- SQL's spellings, its joins, subqueries and schema, parameters for counts, inner `get`s, `alter`, json paths, the shell's reading and declared schemas
- [waits.md](waits.md) -- `Fenec-Wait`, the writes that wake held ones, the bids that settle a row, and the leader's looks
- [indexes.md](indexes.md) -- `@unique`, hash buckets and tables, `@ttl`, what reaches an index, bound filters, `@sorted`, composite indexes, lazy builds, `--warm` and `@geo`
- [browser-module.md](browser-module.md) -- threads and features, what is core, and the bytes the module holds
- [vector-search.md](vector-search.md) -- the distance kernels, vectors as numbers, quantized codes, filtered `near` and sparse vectors
- [js-client.md](js-client.md) -- the module and the client entry, how the page persists a database, live queries, Durable Objects and their alarms
- [text.md](text.md) -- collations, `nocase`, `match`, the text index, `fuzzy` and `prefix`, `rerank`, `fuse`, `highlight()` and facets
- [lookup-and-aggregates.md](lookup-and-aggregates.md) -- `lookup`, `required`, keys of several fields, folded levels, select-list expressions, HyperLogLog and time zones
- [native-and-sync.md](native-and-sync.md) -- `fenec-ffi`, the sans-IO sync core and the scenario file both sync runners are held to
- [router.md](router.md) -- the router's CORS, its pooled connections, the cuts a failover makes, the leading router's and its followers', and queries across tenants
- [integrations.md](integrations.md) -- `integrations/`, the Grafana data source, the desktop app, the `deploy/` kit and the install channels (Homebrew, `install.sh`, `.deb` and `.rpm`)

A proposal holds no invariant until it is built, and is listed apart:

- [named-statements.md](named-statements.md) -- *proposal, not built*: FenecQL defined once in a database and called by name with typed arguments and an authority of its own, its storage, its authority model against `SECURITY DEFINER`'s, its surfaces and the slices that would build it

## The original order

1. [Zero dependencies is a hard rule](crates.md#zero-dependencies)
2. [No buffer pool of its own, and the file is mapped](storage.md#the-mapped-file)
3. [A mapped database hands what it writes over to its file](storage.md#handover)
4. [A block that outgrows 16 MB spills into the file before it lands](storage.md#spills)
5. [Single writer, and readers beside it](engine.md#single-writer)
6. [A block's documents are built beside the readers](engine.md#prepared-blocks)
7. [A long read is pinned, and read with no lock held](engine.md#pinned-reads)
8. [Every write is a block, and a block is one record](engine.md#blocks)
9. [A storage error stops writes](durability.md#storage-errors)
10. [Buffered writes are written behind the lock](durability.md#writes-behind-the-lock)
11. [A durable write syncs a log beside the file, not the file](durability.md#the-sync-log)
12. [Replication ships only what is on disk, numbered by the change counter](replication.md#replication)
13. [A node's tenants are archived over one stream](replication.md#a-nodes-archive)
14. [Scaling out is by tenant, one file each](tenants.md#scaling-out-by-tenant)
15. [A node is in a region and a zone, and a tenant in its node's region](regions.md#regions-and-zones)
16. [A region may keep its own directory, and leases and fails over inside itself](regions.md#region-directories)
17. [One URL reaches every region: a Worker learns each tenant's home from the routers](regions.md#the-global-entry)
18. [A tenant may keep read replicas in other regions, which a router near them answers the reads its clients mark from](read-replicas.md#marked-reads)
19. [A scoped token is held to its rules at every level, twice for writes](access.md#scoped-tokens)
20. [A read rule may name the fields it shows, and a field it withholds is held out of what a statement does, not only of its rows](access.md#field-lists)
21. [A rule may read another collection, answered before each statement](access.md#rules-reading-another-collection)
22. [A token is bound to the tenant it names](access.md#tenant-binding)
23. [An empty token is no token](access.md#empty-tokens)
24. [A tenant is held to its quota by its node, a block as it lands](quotas.md#tenant-quotas)
25. [fenec studio is a page with no authority of its own](http.md#studio)
26. [An agent's MCP tool is a statement, held to the token as `/query` is](http.md#mcp)
27. [A server's `create index` and `compact` run beside the database](storage.md#maintenance-beside-the-database)
28. [A file compacts itself once half of it is dead](storage.md#compaction)
29. [A file is compacted before it outgrows the page cache](storage.md#compaction-and-the-page-cache)
30. [File format](storage.md#file-format)
31. [The HNSW graph is derived data, not a cache](hnsw.md#the-graph-is-derived)
32. [A restore gives back the arena it had, to the bit](hnsw.md#restored-arena)
33. [A graph record is laid out flat](hnsw.md#flat-graph-records)
34. [A batch links in parallel, into the graph it would link in turn](hnsw.md#parallel-linking)
35. [The upper layers are walked with a beam](hnsw.md#upper-layer-beam)
36. [`make scale-bench` holds fenec-server to pgvector, each over its own protocol](benchmarks.md#scale-bench)
37. [`make ycsb` is fenecdb as a general database](benchmarks.md#ycsb)
38. [A block's `put`s link their vectors together](hnsw.md#block-linking)
39. [A server answers before its graph is linked](hnsw.md#deferred-linking)
40. [A server keeps its graphs in its file](hnsw.md#graphs-in-the-tail)
41. [`/_changes` reads the writes on disk, documents and all](replication.md#change-data-capture)
42. [An `Idempotency-Key` makes a write once](http.md#idempotency-keys)
43. [A read can wait for a write on a replica](replication.md#fenec-after)
44. [`insert` never writes over; `put` does](writes.md#insert-and-put)
45. [SQL's spellings are FenecQL, read by the one parser into the same statements](fenecql.md#sql-spellings)
46. [A `set` reads the row it writes, and every write says what it did](writes.md#arithmetic-in-set)
47. [`require <n>` makes a write's count a condition](writes.md#require)
48. [`limit`, `offset` and `top` take a parameter, read where the plan reads them](fenecql.md#count-parameters)
49. [`put ... if absent else set {..}` is an upsert; `put <c> $n` takes its documents from a parameter](writes.md#upserts)
50. [A `set` or a `del` picks its rows with `order` and `limit`, and `returning` answers them](writes.md#picked-writes)
51. [A key of `order` may be an expression](writes.md#order-by-expression)
52. [A `put` answers the rows it wrote with `returning`](writes.md#put-returning)
53. [`Fenec-Wait` holds a `set` or a `del` that writes nothing until it can](waits.md#fenec-wait)
54. [A write wakes the held writes that read what it wrote, and a row several groups would take goes to the one ranking it first](waits.md#waking-held-writes)
55. [One held request looks for every group, and a group that found nothing looks among the rows written since](waits.md#the-leaders-looks)
56. [`@unique` is a `@hash` that asks its bucket before a write](indexes.md#unique)
57. [A `@hash` bucket holds its ids ascending, in runs](indexes.md#hash-buckets)
58. [A hash index's table grows a 256th at a time, and a text index's terms too](indexes.md#sharded-tables)
59. [`in (get ...)` is answered before the query, as the list it is](fenecql.md#inner-gets)
60. [`@ttl` is an ordered index whose rows expire, and every read leaves them out at once](indexes.md#ttl)
61. [`alter collection` rewrites no document; positions are not places](fenecql.md#alter-collection)
62. [A `json` field holds any value, and a path is a name](fenecql.md#json-fields-and-paths)
63. [A vector written again is one node](hnsw.md#aliases)
64. [Natively the table that finds a copy grows from its own slots](hnsw.md#the-copies-table)
65. [A graph record carries the halves the copies table is made from](hnsw.md#halves)
66. [The browser module hashes as a native build does, and takes the halves](hnsw.md#halves-in-the-browser)
67. [An archive and a backup can be sealed](replication.md#sealed-archives)
68. [Refusals and schema changes are logged; a refusal waits](http.md#audit)
69. [Every request has an id, in its answer and in every line it wrote](http.md#request-ids)
70. [A request is a trace, sent over OTLP](http.md#traces)
71. [Limits error, they do not truncate](crates.md#limits)
72. [Threads are `cfg`'d out of WASM](browser-module.md#no-threads-in-wasm)
73. [The wasm32 build has SIMD, and its kernels match the scalar ones bit for bit](vector-search.md#simd)
74. [aarch64's strips are written out as well, to the same bits](vector-search.md#neon)
75. [The indexes are features, and a build without one opens a file that declares it](browser-module.md#index-features)
76. [The rest of the language is features too, and the smallest module is 149 KB brotli](browser-module.md#language-features)
77. [What is core, and why one module ships](browser-module.md#what-is-core)
78. [One module and a client entry, by where the queries run, and a test build](js-client.md#the-module-and-the-client)
79. [The browser module holds one copy of a generic where it can](browser-module.md#one-copy-of-a-generic)
80. [The core lost what it held twice or for one use](browser-module.md#size-cuts)
81. [A vector goes out as its `f32`s](vector-search.md#vectors-out)
82. [A number is read three ways, each exact](vector-search.md#numbers-in)
83. [A vector parameter goes into the module as `f32`s](vector-search.md#vector-parameters)
84. [A quantized index holds codes, and `near` orders by the documents' vectors](vector-search.md#quantized-indexes)
85. [A bit code is a residual's signs, from a centre the index learned](vector-search.md#bit-codes)
86. [Filtered `near` needs its fallback](vector-search.md#filtered-near)
87. [Only an `and` chain reaches an index](indexes.md#index-reach)
88. [A filter is bound to its collection once a query](indexes.md#bound-filters)
89. [`@sorted` must give the scan's answer, row for row](indexes.md#sorted)
90. [An index over several fields is its parts' bytes laid end to end](indexes.md#composite-indexes)
91. [`collate und` and `collate tr` are ICU's orders, a query's or a field's](text.md#collations)
92. [`nocase` is a text compared by its case fold wherever it is compared](text.md#nocase)
93. [`lookup` chains, and the chain is still positional](lookup-and-aggregates.md#lookup-chains)
94. [`lookup` is one bucket probe per parent, not a join](lookup-and-aggregates.md#lookup)
95. [`required` is the other half, and it is a pass](lookup-and-aggregates.md#required)
96. [A `lookup` over a key of several fields is a run of an index over several fields](lookup-and-aggregates.md#lookup-over-several-fields)
97. [A `lookup` level may fold its children, and a `lookup` hangs off groups](lookup-and-aggregates.md#folded-levels)
98. [`match` prunes with MaxScore](text.md#maxscore)
99. [`@text` is word-boundary matching unless told otherwise](text.md#tokenizer)
100. [The text index is derived data as well, but it is not persisted](text.md#text-index-is-derived)
101. [A text index's word is 9 bytes of a table, 16 of a term, and its bytes](text.md#term-table)
102. [`match ... fuzzy` and `prefix` walk a sorted copy of the index's words](text.md#fuzzy-and-prefix)
103. [An open builds no hash, text, ordered or sparse index](indexes.md#lazy-indexes)
104. [An open of a file with a graph copies its vectors, and that is most of it](hnsw.md#opening-a-graph)
105. [A server warms what an open left for the first read](indexes.md#warm)
106. [`rerank` deliberately uses no index](text.md#rerank)
107. [`fuse` adds ranks, not scores](text.md#fuse)
108. [`highlight()` reads the text again; `facet` counts beside the rows](text.md#highlight-and-facet)
109. [A disjunctive facet's filter is split where the filter is written](text.md#disjunctive-facets)
110. [An item of a select list is an expression, and an aggregate folds one](lookup-and-aggregates.md#select-expressions)
111. [`approx_count_distinct` is HyperLogLog, and `having` keeps groups](lookup-and-aggregates.md#hyperloglog-and-having)
112. [`bucket(at, 1d, 'Europe/Berlin')` reads a zone's clock](lookup-and-aggregates.md#time-zones)
113. [`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers exactly](vector-search.md#sparse-vectors)
114. [A point is two `f64`s on Redis's sphere, and `@geo` is Morton keys in the ordered index's chunks](indexes.md#geo)
115. [`--follow` confirms nothing that is not on disk](replication.md#follow)
116. [`/_metrics` counts at the edge, a shard per thread](http.md#metrics)
117. [`/_stats/statements` counts by shape, a tenant's apart](http.md#statement-counts)
118. [A statement is parsed once, and an answer goes out in one write](http.md#parse-cache)
119. [The shell runs a statement once nothing after it can go on with it](fenecql.md#the-shell)
120. [A plain `get` is written as JSON from the stored documents](http.md#json-from-stored-rows)
121. [A Durable Object keeps a database as a file would](js-client.md#durable-objects)
122. [An object's alarm keeps its database](js-client.md#alarms)
123. [The browser module grows nothing by doubling past a mebibyte](browser-module.md#chunked-memory)
124. [A live query learns what a write changed from the change ring](js-client.md#live-queries)
125. [An app keeps its database through `fenec-ffi`](native-and-sync.md#the-native-library)
126. [A replica's sync is a state machine; the network is the binding's](native-and-sync.md#sync-core)
127. [The browser's sync and the native core are held to one scenario file](native-and-sync.md#sync-scenarios)
128. [A tenant's quota is a refusal where waiting makes no room, a wait the server names otherwise](quotas.md#quotas-in-the-sync)
129. [Every 503 says when to come back, in its body too](quotas.md#retry-after-in-bodies)
130. [The router tells a page one CORS rule, its own](router.md#cors)
131. [The router looks at a pooled connection before it writes, and sends a request again only where the node cannot have run it or may run it twice](router.md#pooled-connections)
132. [A failover ends what the router has out to the node for the tenants it moved](router.md#failover-cuts)
133. [A query across tenants is the router's admin's, merged by what the request says](router.md#queries-across-tenants)
134. [A schema declared in code is checked at every open, in the engine](fenecql.md#declared-schemas)
135. [`integrations/` may use outside packages; the crates may not](integrations.md#integrations)
136. [Grafana's data source binds a dashboard's values; it never splices them](integrations.md#grafana)
137. [`deploy/` is the kit for running regions, held to the binaries by two checks](integrations.md#deploy)
138. [Profiles differ on purpose](crates.md#profiles)
139. [A JOIN on a key is a `lookup` answered flat, or one table's `get` with the other's rows a filter](fenecql.md#sql-joins-subqueries-and-schema) (added after the move, under *FenecQL and schemas*)
140. [A replica whose attach failed is tried again, and a file the router left on a node is reported](tenants.md#replicas-tried-again) (added after the move, under *Tenants and leases*)
141. [A connection no thread can be made for is answered 503, as one past the ceiling is](http.md#connections-without-a-thread) (added after the move, under *The HTTP server*)
142. [Every way a server stops is the one shutdown](durability.md#the-one-shutdown) (added after the move, under *Durability*)
143. [A release is installed from its own files, each checked before it is used](integrations.md#install-channels) (added after the move, under *Integrations and deploy*)
144. [The desktop app is a window on a studio a `fenec-server` serves, one server per file](integrations.md#studio-desktop) (added after the move, under *Integrations and deploy*)
145. [A token is taken only from the issuers, for the audiences and clients, a server was told](access.md#whom-a-token-is-for) (added after the move, under *Access*)
