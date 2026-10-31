# AGENTS.md

This file provides guidance to Codex and other coding agents when working with code in this repository. It is written from CLAUDE.md by `make agents-md`: edit that one.

fenecdb -- a minimal embedded database in Rust. Compiles to WASM for the
browser, has its own query language (FenecQL), and its server
(`fenec-server`) speaks HTTP; every language reaches it that way, Python,
JavaScript, Go, .NET, Swift, Kotlin and Dart through official clients. User
docs are `site/content/docs/`, `README.md` their front door; this file is
the working summary, the long form of each invariant and convention in
`docs/design/` (ordered by its `README.md`); `tools/agents_md.py --check`
fails once this file passes 64 000 bytes. `AGENTS.md` is written from this
file (`make agents-md`) under its own title and first line, and CI fails
when the two differ: edit `CLAUDE.md`.

## Commands

```bash
make test          # Rust (fenec-core also without its indexes and without any feature), then the JS tests and sync-scenarios-check
make wasm          # fenec-wasm for wasm32 to web/fenec.wasm
make wasm FEATURES="text sorted"   # with those features alone (indexes, language: a group; none: none)
make wasm-lite     # test build without the indexes (web/fenec-lite.wasm), shipped by nothing; make wasm-min: with no feature at all
make wasm-sizes    # what each feature adds to the module with none, and what leaving it out saves
make size-report   # where the module's bytes go (BASE=main, BIN=fenec-server, FEATURES=none)
make ffi           # the native library for apps (crates/fenec-ffi); TARGET=... another, JNI=1 for Kotlin
make sync-scenarios-check   # both runners of integrations/sync-scenarios.json passed every scenario (after make test)
make packages      # the PyPI, npm and NuGet packages as a release publishes them, installed and used
make packaging-test    # site/install.sh against a release of its own on localhost, and the Homebrew formula held to its script
make version V=X.Y.Z   # one version wherever a release reads it (RELEASING.md)
make agents-md     # AGENTS.md written again from this file
make builder-golden   # integrations/builder-golden.json written again from the JS builder (web/golden.mjs)
make docs-builder     # the SDKs' tests of the site's builder tabs written again (site/builder_tabs.py)
make schema-golden    # integrations/schema-golden.json written again through the module (web/schema-golden.mjs)
make types-check  # web/fenec.d.ts held to web/fenec.js and a caller's code under tsc --strict, then docs-types
make studio-highlight   # studio/highlight.js and the Grafana data source's copy written again from site/highlight.js
make pgvector-up   # PostgreSQL 17 + pgvector in Docker on 127.0.0.1:55432, for the comparisons and --follow (pgvector-down)
make server TOKEN=secret HTTP=127.0.0.1:8080   # the server against ./data.fenec
make node ADMIN=secret   # a tenant node: fenec-server --dir tenants
make shard               # the router in front of the nodes (./shard.fenec)
# Every other target is described by the Makefile's `##` comments above its
# rule: serve, small, the suites (*-test, grafana-check, deploy-check, drill;
# Docker for several, `make wasm` first for react, langchain, ai-sdk and
# cloudflare) and the measurements, none of them in CI (every *-bench, bench
# memory sweep compare beir ycsb wasm-speed wasm-exact-speed text-memory;
# `make pgvector-up` first where PostgreSQL is the other side).
```

Single tests:

```bash
cargo test -p fenec-core --test all persist::   # one file of a crate's integration tests (one binary)
cargo test -p fenec-ql near                      # by name substring (integration: fn name only)
cargo test -p fenec-core codec::tests            # inline unit tests in a module
cargo test -p fenec-core --no-default-features --features std-fs,language --lib --test features   # without the indexes (`--features std-fs` alone: without any feature)
cargo test -p fenec-import --test all pg:: -- --ignored   # needs a live PostgreSQL
node --test web/fenec.test.js                    # JS: builder
node --test --test-name-pattern 'shape' web/fenec.sync.test.js
```

`make test` runs Rust first on purpose: `cargo test` builds the
`fenec-server` binary that `web/fenec.sync.test.js` runs against (it
self-skips without it or `web/fenec.wasm`). **The wasm32 target comes from
rustup**, never Homebrew's `cargo` (`$(CARGO)` prefers `~/.cargo/bin/cargo`;
by hand, `rustup target add wasm32-unknown-unknown`).

## Architecture

Dependency direction (nothing points back up):

```
fenec-core  (std only, zero deps)
     |
fenec-ql    (lexer + parser)          fenec-abi   (both C ABIs' answers, a replica's sync)
     |                                fenec-wasm  (browser C ABI)   fenec-ffi (native C ABI, apps)
     |
fenec-http  (REST/JSON + SSE, tenant registry, replication, /_metrics)
     |                     \
fenec-wire  (a PostgreSQL     fenec-shard (tenant router: directory,
     |      client: framing,             placement, move)
     |      SCRAM, COPY out, pgoutput)
fenec-import (SQLite file reader + PG COPY source + --follow)
     |                                  \
fenec-server (HTTP, the syncer and       fenec-cli (`fenec` shell,
     shutdown, the importer's --follow)   `fenec import`, `fenec types`)
```

In `crates/fenec-core/src/`, `engine` holds `Database` and `Collection`,
`store` the segments and offset index; `fs`, `cache` and `synclog` are
behind `std-fs`; the indexes are `vector`, `text`, `highlight`, `fuzzy`,
`sparse`, `sorted`, `composite`, `geo`, `hll` and `maps`, and `off` stands
in for one a build is made without.

The browser client is `web/fenec.js` -- WASM glue (~605 lines), persistence
and the sync layer -- importing and re-exporting the query builder
(`web/builder.js`) and the HTTP client (`web/http.js`), dependency-free ES
modules; `web/fenec.d.ts` and `web/client.d.ts` hold the types,
`fenec types <file>` a schema's.
[In the page](docs/design/js-client.md#persistence-in-the-page)
`persist`/`restore` keep an image and the journal's chunks in IndexedDB
(`fenec_journal`, `fenec_drain`; off until asked for), a `cryptoKey`
sealing both; `openFile` keeps `fenec-server`'s own file in OPFS, never
sealed, a new image flushed into `<name>~` before the file is written over.

## Invariants

### The crates

**[Zero dependencies is a hard rule](docs/design/crates.md#zero-dependencies)**
for every crate but `fenec-bench`; `fenec-core` dev-depends on `fenec-ql`.

**[Limits error, they do not truncate.](docs/design/crates.md#limits)**
`near`, `match` and `fuse` cap at 10 000 rows, expressions at 512 levels,
a `lookup` chain at 8 and `in (get ...)` at 100 000 values and 4 levels,
each a query error, never a cut result; a new limit takes that shape.

**[Profiles differ on purpose.](docs/design/crates.md#profiles)**
`fenec-cli` uses `cli` (`panic = abort`); `fenec-server` stays on
`release`: a connection's panic unwinds alone.

### Storage

**[No buffer pool of its own, and the file is mapped.](docs/design/storage.md#the-mapped-file)**
`fs::open` maps it, a read decoding over its bytes; a rewrite points the
stores at the new file (`Database::repoint`). "Page cache" is always the
operating system's, a "buffer pool" a database's own, which fenecdb has
none of, in code, docs and site alike.

**[Writes are handed over to the mapped file.](docs/design/storage.md#handover)**
Past `HANDOVER_AT` or `HANDOVER_DOCS`, `Database::hand_over` points the
stores at it, never with a block open, the file the same byte for byte.

**[A block past 16 MB spills into the file before it lands.](docs/design/storage.md#spills)**
A spill (kind 10) counts only where a land (kind 11) names it; the feed
sends the block as one record (`landed_block`).

**[A lone `create index` or `compact` runs beside the database](docs/design/storage.md#maintenance-beside-the-database)**
(`Database::maintain`): every write must pass `Database::note`; a schema
change meanwhile fails it, and an image adopted meanwhile marks every tail
changed (`Tails::all_changed`).

**[A file compacts itself once half of it is dead](docs/design/storage.md#compaction)**
and 64 MB: `CompactPolicy` is the one rule, run beside the database by
`Compactor` or `fenec_http::link` (`--auto-compact`), on replicas too.

**[A file is compacted before it outgrows the page cache](docs/design/storage.md#compaction-and-the-page-cache)**
(`CompactPolicy::memory`): its live records fitting the cache's room
(`cache::room`), once a third of what the compact writes is dead and the
file reached `ROOM_SHARE` of that room or the process faults
(`cache::faulting`), a compact's own faults forgotten
(`cache::forget_faults`).

**[File format](docs/design/storage.md#file-format):** a record is
`[kind][collection-id][length][body]`, its length read even when empty;
`head` writes a head (the counter's `image_head` aside), `record_at` reads
it, `Walk` walks them. A torn tail is cut on open, a torn image refused,
and the file only grows; a tool that only looks opens with
`fs::open_read_only`, which cuts, creates and writes nothing. A new record
kind, index kind, type tag or graph version is a number a binary from
before refuses (a graph's, rebuilds from), never one it would misread.

### The engine's lock and blocks

**[Single writer, and readers beside it.](docs/design/engine.md#single-writer)**
Never two processes over one file: whatever writes a served file -- the
endpoint, a follower, the graph keeper, `--follow`'s mirror -- is a
`fenec-server` thread, never a binary of its own, and a block is open only
under its opener's write lock. Whatever opens a file to write it, or
renames one into its place, holds `<file>.lock` (`fenec_abi::lock`),
refused while another does, a symbolic link resolved to its file for the
lock and every rename (`fs::resolved`): `fenec-server --file`,
each open tenant of a `--dir` node (in its sink, `lock::Holding`, let go
with the database), fenec-shard's directory, the native library and every
`fenec` way that writes; `--read-only` and `fenec types` take none, and a
`backup` or `restore` replaces a file only with `--force`. A hard link, a
Docker Desktop bind mount or a file system keeping no locks escapes it.

**[A block's documents are built beside the
readers](docs/design/engine.md#prepared-blocks)** (`Database::prepare`,
`early_put`), taken only where the collection holds the schema `Arc` they
were built against; all else is decided under the write lock. A hook that
may change a document keeps `Hook::rewrites` true, its payload encoded after
it.

**[A long read is pinned, and read with no lock held](docs/design/engine.md#pinned-reads)**
(`Database::pin`), sharing nothing a write changes in place; it declines a
read an index answers, and a pinned index refuses (`PINNED_OUT`), so a stray read reruns under the
lock.

**[Every write is a block, and a block is one record.](docs/design/engine.md#blocks)**
`commit` appends it whole and only then notes it, so a crash, a replica
and a subscriber see all of it or none. A failed one is undone in memory,
a schema change too (`Undo`, a variant each); whatever counts records
counts their writes (`writes_in`), and what cannot be undone, a compact,
is refused in a block (`Statement::fits_block`).

### Durability

**[A storage error stops writes.](docs/design/durability.md#storage-errors)**
Writes after one are `Error::Io` until reopened (`Database::failure`). A
mapped file is replaced only by rename. The fsync runs after the lock
(`Database::flush`'s `Durability`), a durable write answered only once it
has; a failed one is never retried (500 under `--sync always`) and goes to
`Database::fail`. A write the lock still makes is put off while a
durability holds the disk (`Sink::syncing`).

**[Buffered writes are written behind the lock](docs/design/durability.md#writes-behind-the-lock)**
(`Database::write_behind`), asked after every write no fsync follows.

**[A durable write syncs a log beside the file, not the file](docs/design/durability.md#the-sync-log)**
(Linux and Android, `synclog.rs`). Whatever renames a file into a
database's place removes the log first, durably (`fs::forget_sync_log`).

**[Every way a server stops is the one shutdown.](docs/design/durability.md#the-one-shutdown)**
A signal's handler, `--exit-on-stdin-close`'s end of stdin
(`request_shutdown`) and a failure of what runs beside the database -- a thread the
start cannot make, the listener, `--follow`'s mirror (`stop_with`, its exit
code) -- set one flag, `SHUTDOWN`; the syncer, a file's
or a node's, looks every `LOOK` whatever `--sync` is, syncs, checkpoints
(only once `serving`, and exiting 0, `checkpoints`) and exits holding the
write lock. A new way down sets that flag and never exits on its own. What
fails before anything writes the file or runs beside it -- a flag, the open,
a port, both listeners bound straight after the open -- exits where it
fails, having written only `<file>.lock` and what the open does.

### Replication and archives

**[Replication ships only what is on disk, by the change counter.](docs/design/replication.md#replication)**
Every write goes through `wal`, its record moving the counter by its
writes (`writes_in`); the history and a tail's graphs, which move none, are
never sent. A durable write waits for every stream keeping up
(`Feed::wait_sent`, `SENT_WAIT` at most, a lagging one not waited for); a
promotion halts the follower before it forks, and a following database
refuses writes (`Error::ReadOnly`).

**[A node's tenants are archived over one stream](docs/design/replication.md#a-nodes-archive)**
(`fenec_http::node_stream`), a sender on each open tenant's feed:
`Tenants::close` refuses while it has a write left (`Feed::unsent`), and
whatever the stream does that can block sets `Sender::held` first, which
no durable write waits for; a sync waits for no stream
(`replication::unwaited`). Whatever makes, installs, deletes, promotes or
makes a tenant follow touches the stream, and an image on a history not
leading from an archive's end never lands among its changes
(`archive::Took::Diverged`).

**[`/_changes` reads the writes on disk, documents and all](docs/design/replication.md#change-data-capture)**
(`Database::changes_in`): only what an fsync covered, never with writes
missing (410); a scoped token is refused. A consumer is made by a `POST`,
which never moves one that is there (200 at its place, 409 for a `since`
ahead of it), and moved by a `PATCH` alone, which makes none
(`cdc::make`, `cdc::commit`); on a tenant each is a write held against a
move and the lease (`consumer_write`).

**[A read can wait for a write on a replica](docs/design/replication.md#fenec-after)**
(`Hub::reached`): served once the database holds that change or 504
after `Fenec-Wait` (30 s at most), never before. Every wait comes after
its token (`admitted`), a stream's before its head.

**[An archive and a backup can be sealed](docs/design/replication.md#sealed-archives)**
(`seal.rs`, `--key-file`), never the live file.

**[`--follow` confirms nothing that is not on disk.](docs/design/replication.md#follow)**
Every write is a put or delete by id; the mirror is a `fenec-server` thread,
its collections' only writer. A wire message's length is held to
`proto::MAX_MESSAGE` before anything is allocated (`proto::zeroed`).

### Tenants and leases

**[Scaling out is by tenant, one file
each.](docs/design/tenants.md#scaling-out-by-tenant)** The tenant comes from
the path, never the query; the router forwards bytes, parsing none; the
registry opens a file under its lock and closes it on its last `Arc` alone,
never while its follower or anything beside it holds the database, so no two `Database`s share a file. A tenant's role is its file's, not the node's
`--replica-of`; a move is `freeze` (a gate taken exclusively, so no write lands
after the export), copy, install, the flip in one statement, the source
deleted once the standbys were sent it (`standbys_sent`). A node
is failed over on its own only under a lease, run on the boot clock
(`lease::Moment`) from the grant the router acknowledged, never a grant's
reading (`grant_from`); the router counts a node by the directory's
`leasing` record until the node ran one of its leases (`Leases::inherited`,
`Leases::entry`), granting one at a time, never with a list older than the
last it sent (`Leases::turn`, `lease::round_due`). Every write path meets
the engine's fence (`Database::set_fence`), every route answering a write
asks `answered_under_lease` (500 `outcome: unknown` once lapsed), and a node
promotes only a copy of the tenant's lineage and history
(`tenants::Expected`, 409) unless `force`d.

**[A failed attach is tried again, a file the router left on a node
reported.](docs/design/tenants.md#replicas-tried-again)** A failed attach or
a held tenant has `try_again` run a repair again, rechecked under the claim,
a copy's node first within the zone rule, none on a node silent at the look.
A file the router made, or failed to remove, that no placement records is a
`leftovers` row, reported (a repair's `left`, `fenec_router_files_left`),
never deleted; `fenec_router_tenants_unreplicated` counts the uncopied.

### Regions and the global entry

**[A node is in a region and a zone, and a tenant in its node's region.](docs/design/regions.md#regions-and-zones)**
Its region is its node's, kept nowhere else and fixed while the node holds
a tenant, a replica or a place in a pair (`holds`); a `--replicas` copy
and a pair's standby stay in the primary's region, another zone first
(`Router::replica_node`).

**[A region may keep its own directory, and leases and fails over inside itself.](docs/design/regions.md#region-directories)**
A request is relayed once, never again (`regional_admin`), and a move
across regions undoes each step where the next fails (`move_across`). A
node refuses a router its lease's holder superseded (409 `held_by`,
`Lease::admits`), and another directory's grant while its lease runs.

**[One URL reaches every region: the Worker learns homes from the routers.](docs/design/regions.md#the-global-entry)**
It serves `/t/<tenant>/`, and `/_shard/` where `ADMIN` is set, never a
tenant's `/_replication`, decoding a path as fenec-http does
(`percentDecode`). It drops every address a client claims and names the
client (`CF-Connecting-IP`) in `Fenec-Client`, an HMAC under `ENTRY_TOKEN` a
router believes only with `--entry-token` and within 60 s (`audit::entry`),
and drops from all it forwards (`forwarded_headers`); a router refuses an
entry token empty or equal to another it holds, at the start and as a node
is added (`set_node`). Every router
answer carries `Fenec-Shard: 1`; a request that may have reached one goes
again only as a `GET`, `HEAD`, `OPTIONS` or keyed.

### Read replicas

**[Read replicas in other regions answer only marked reads.](docs/design/read-replicas.md#marked-reads)**
The router parses no FenecQL: only `Fenec-Read: replica` goes near, and
every server runs a marked request read-only on every route (`marked`, read
in `serve_connection` alone); a failover never promotes a read replica. One
is recorded once its node follows; taken off, its record goes first, sent to
the standbys, then its file; no name is made again while a directory keeps a
trace of one (`reader_traces`). A copy answers only once its primary said
hello since it opened (`Tenant::ready`, else 503); a read replica is bound
to its tenant's lineage, which every promotion and restore keeps
(`fork_id`), and deletes itself rather than take another's
(`Follower::expect_lineage`). `Fenec-History` goes beside `Fenec-Seq`, and
`after` waits for that change of that history (`History::continues`). SDKs
send home's furthest position with each marked read, whose answer moves
none; home's 504 sets it, lower or not, only over the position that read was
sent after, never to an earlier term of its lineage. No lock is held across a call to a node, and a refused hold
makes none (`then`, never `then_some`).

### Access

**[A scoped token is held to its rules at every level, twice for
writes.](docs/design/access.md#scoped-tokens)** A new path that runs a
statement must go through `scoped()` or it reads everything, and a scoped
write run outside `within()` goes unchecked; a write over a row meets
`before_overwrite` with the row as it was, which judges `update(a, b)` and
transitions (`new.`, granting `update` alone), and `append-only` binds
every scoped token. A scoped stream tells deletions only of ids it sent, a
scoped poll's tag digests its answer (`content_tag`), a scoped `match`
scores its rows alone (`Match::within`), and the algorithm is the key's,
never the token's. A claim held as `null`, `""` or a value its rule cannot
compare (`usable`) is a claim the token lacks: the rule naming it does
not apply and no filter is bound to it. A claim a rule orders by holds
the other side to its kind under any number of `not`s (`kind_held`), a row
with no value answering as the rule written out, and an inner `get`'s rules
are bound for the `not`s it stands under (`hold`); one inside a call or
arithmetic -- `like`'s pattern aside -- or compared in order inside a
value, is refused at startup (`claims_held`). A tenant claim naming no
tenant reaches none, a `sub` of `""` is no subject, and a policy writing a
`$<n>` of its own is refused at startup.

**[A field a read rule withholds is held out of what a statement
does.](docs/design/access.md#field-lists)** `fields::hold` and
`fields::write` guard or refuse (403) each use of it before the rules are
ANDed in; it is `null` in a row (`Scope::shown`), absent from every schema
answer (`Scope::shown_schema`) and refused by name as a missing one, and a
stream sends no row a write left showing the same (`sse::Seen`).

**[A rule may read another collection, answered before each
statement.](docs/design/access.md#rules-reading-another-collection)**
Its `in` lists are answered under each lock (`Scope::bound`), the scope
`Check`, the mask and streams use; a list past `MAX_CLAIM_VALUES` is
refused, never cut, and rules reading each other's collections, a `*` rule
reading one, or `new.` or `expired()` in one are refused at startup
(`ring`).

**[A token is bound to the tenant it
names.](docs/design/access.md#tenant-binding)** `route_tenant` asks it,
and no route below `/t/<t>/` may skip it; every tenant's database installs
the `Check` hook (`Tenants::check_scoped_writes`).

**[A token is taken only from the issuers, for the audiences and clients,
a server was told.](docs/design/access.md#whom-a-token-is-for)**
`Demands::whom` asks `iss`, `aud` (a text or a list) and `azp` (else
`client_id`) in `Access::check`, after the signature and before a token is
kept, so every route meets it through `authenticate`; a refusal is 401
naming the check, never the token's value, clears no wait, and its audit
line names the check (`audit::refusal`). A server told none takes every
token its keys sign (said at the start under `--jwt-keys`), and a `--jwt-*`
flag without keys is refused. A policy names top-level claims alone, quoted
where not a word (`claim_name`): a path into a nested object, or an
unquoted name running into `-` or `:`, is refused at startup, never read; a
role is the word after the last `for` or a quoted name (`role_of`).

**[An empty token is no token.](docs/design/access.md#empty-tokens)**
A variable set empty is unset (`env_secret`, fenec-shard's `env`); a token
flag, a JWT secret's file or a key file given empty is refused at the
start; a library constructor takes an empty token as none
(`Config::settled`).

### Quotas and retries

**[A tenant is held to its quota by its node, a block as it
lands.](docs/design/quotas.md#tenant-quotas)** A block or spill past
its bytes or rows is put back (507) as it lands (`Database::set_limit`),
one only taking away let through; a refusal names the bound alone, and a
rate charges a request's own thread alone (`writing_here`). The router
enforces nothing but sends the quota to the tenant's node, standby,
replica, a move's target before the flip and a promoted copy.

**[A quota is a refusal where waiting makes no room, a wait the server
names otherwise.](docs/design/quotas.md#quotas-in-the-sync)** A synced
507 naming a `quota` is put back; a retry waits `retry_after_ms` where
longer than its backoff.

**[Every 503 says when to come back, in its body
too.](docs/design/quotas.md#retry-after-in-bodies)** Write one with
`Response::retry_after`.

### The HTTP server

**[fenec studio is a page with no authority of its
own.](docs/design/http.md#studio)** Its files are served before any token
under a strict CSP, and it sends the pasted token as any client does. An app
embedding it hands a frozen `window.fenecHost.session` over, taken for the
page's own origin alone and never stored, with no sign-in or sign-out beside
it. Each statement is written in
`studio/statements*.js`: values as parameters, a name only as FenecQL writes
one, the typed `where` inside parentheses it cannot close;
`transport.test.mjs` holds the in-page answers to a server's.

**[An agent's MCP tool is a statement, held to the token as `/query`
is.](docs/design/http.md#mcp)** A model's filter goes in parsed, never as
text, and a read past `--mcp-max-rows` or `--mcp-max-bytes` is refused,
never cut. The origin is asked before the token, and with no token only a
`Host` naming localhost or an address is answered; `write` never changes a
schema or compacts. `<file>.lock` (`fenec_abi::lock`) is taken by `fenec
mcp --file`, `fenec-server --file`, the native library and the shell alike.

**[An `Idempotency-Key` makes a write
once.](docs/design/http.md#idempotency-keys)** The write and its key's
`_idempotency` row are one block, the key the subject's for a scoped token
and the key with another request 422; a failed write or a `compact` keeps
none.

**[Refusals and schema changes are logged; a refusal
waits.](docs/design/http.md#audit)** Only `audit::verified`, where a token
checks out, clears the wait; a new check must call it, and nothing infers
it from an answer. A node believes `Fenec-Router` from the router alone; a
test opening the log is a `[[test]]` of its own.

**[Every request has an id, in its answer and every line it
wrote.](docs/design/http.md#request-ids)** `tests/datadog.rs` and
`tests/logs.rs` hold `monitoring/datadog/` to what is sent.

**[A request is a trace, sent over OTLP.](docs/design/http.md#traces)** A
thread-local from `begin` to `end`, `discard`ed before a stream takes the
connection over; a span names a statement's shape, never a value, and a
request never waits on the collector.

**[`/_metrics` counts at the edge, a shard per
thread.](docs/design/http.md#metrics)** Timed around `handle`, counted
before the answer is written; no label names a tenant or its collections,
and `--metrics` attaches no watcher.

**[`/_stats/statements` counts by shape, a tenant's
apart.](docs/design/http.md#statement-counts)** Literals are `$n`
(`statements::shape`). A shape names every collection, so they take what
reads the data unscoped or the admin token; on a tenant node a tenant
reads its own, and every tenant's, named, are the admin's alone.

**[A statement is parsed once, and an answer goes out in one
write.](docs/design/http.md#parse-cache)** A JWT's scope is ANDed into a
copy, never the cached parse; an answer is one `writev`
(`http::write_parts`).

**[A plain `get` is written as JSON from the stored
documents.](docs/design/http.md#json-from-stored-rows)**
`Database::query_json` must match `query` through `json::rows_array_into`
byte for byte (`tests/query_json.rs`).

**[A connection no thread can be made for is answered
503.](docs/design/http.md#connections-without-a-thread)** Both accept loops
spawn through `accept::spawn`, which hands back a refused thread's stream
for `accept::refuse` to answer on the accepting thread, non-blocking,
counted by `reason` and marked `Fenec-Unserved`: unrun to the entry and
relays, as a standby's 409 marked `Fenec-Follows` is. Its writing shut after
the answer, it goes to the one lingering thread (`accept::linger`) to be
read off until the client ends it, closed at once only with no such thread,
no room (`most_held`) or an answer not sent whole. A new accept loop calls
`accept::ready()` and `accept::widen` before its first accept.

### The HNSW graph

**[The HNSW graph is derived data, not a
cache.](docs/design/hnsw.md#the-graph-is-derived)** Never written on the
write path; an open that cannot validate it rebuilds it silently. A
tombstone carries its vector, a rewrite touches only indexes whose field
changed, to the bit (`unindex_doc`), and an unfiltered `near` tombstones cut
short walks again wider or searches exactly (`past_tombstones`).

**[A restore gives back the arena it had, to the
bit.](docs/design/hnsw.md#restored-arena)** Each vector is normalised in
the write path's order (`flat_sq`, `flat_sqs8`).

**[A graph record is laid out flat.](docs/design/hnsw.md#flat-graph-records)**
Versions 9 (images) and 10 (tails) are written; every layout reads into
one set of arrays (`Read`), built in one place (`build_restored`).

**[A batch links in parallel, into the graph it would link in
turn.](docs/design/hnsw.md#parallel-linking)** A pruning reads only the list
it prunes and the vectors, and each list's additions go in node order
(`link_batch`).

**[The upper layers are walked with a
beam.](docs/design/hnsw.md#upper-layer-beam)** A search and an insert walk
each layer above 0 with `UPPER_BEAM` (4).

**[A block's `put`s link their vectors
together.](docs/design/hnsw.md#block-linking)** Natively they wait
(`defer_batch`), linked before the block's record is written; a search in
the block measures them exactly, and a rollback drops those it made
tombstones (`forget_waiting`).

**[A server answers before its graph is
linked.](docs/design/hnsw.md#deferred-linking)** `fs::open_serving`
leaves the tail's vectors for `link::beside` to link, a search measuring
each beside its walk.

**[A server keeps its graphs in its
file.](docs/design/hnsw.md#graphs-in-the-tail)** `link::keep` appends a
changed graph (kind 4) under the read lock (`Database::save_graphs`); it
moves no counter and no replica is sent it.

**[A vector written again is one node.](docs/design/hnsw.md#aliases)**
Every insert, batch and deferral goes through `VectorIndex::place`: a
vector a live node holds to the bit makes its document an alias.

**[Natively the copies table grows from its own
slots.](docs/design/hnsw.md#the-copies-table)** `Same` grows reading no
vector, in 256 shards past `SAME_SPLIT`.

**[A graph record carries the copies table's
halves.](docs/design/hnsw.md#halves)** Every writer writes them
(`HALVES`), so a record is the same however its index was made.

**[The browser module hashes as a native build
does.](docs/design/hnsw.md#halves-in-the-browser)** It takes and writes the
halves.

**[An open copies a graph's vectors into the
arena.](docs/design/hnsw.md#opening-a-graph)** It restores and validates
the graph then.

### Benchmarks

**[`make scale-bench` holds fenec-server to pgvector, each over its own protocol.](docs/design/benchmarks.md#scale-bench)**
Sides run in turns after idle minutes; a hot run's figure is not one.

**[`make ycsb` is fenecdb as a general database.](docs/design/benchmarks.md#ycsb)**
`--verify` checks every answer; `ycsb/results.tsv` is appended to, never
rewritten (a replaced run named in `ycsb/superseded.tsv`), and
`site/build.py` holds every YCSB figure on the site to its median there.

### Writes

**[`insert` never writes over; `put`
does.](docs/design/writes.md#insert-and-put)** A held id is
`Error::Duplicate` (409); a scoped rewrite keeps `insert`, and the JS
builder's `.insert()` sends `put`, which the sync's undo writes rows back
through.

**[A `set` reads the row it writes, and every write says what it
did.](docs/design/writes.md#arithmetic-in-set)** `query::arith` never
wraps past 64 bits; the record holds the result, never the expression.

**[`require <n>` makes a write's count a
condition.](docs/design/writes.md#require)** Any other count is
`Error::Unmet` (412), the block put back; `n` is an integer, never a
parameter. A `get` takes it too, counting after `offset` and `limit`
(`read_required`); a sync replica refuses one reaching an unanswered
insert's row (`REQUIRE_UNANSWERED`) or a `get`'s beside a synced write
(`GUARDED`). `else '<reason>'` after it is a text of the statement, 1 to
200 bytes, no backtick and never a parameter, leading the refusal's
message, where `Error::reason` reads it back up to the first `` (` `` --
never a field beside it -- for every answer's `reason`.

**[`if absent else set` is an upsert; `put <c> $n` takes a parameter's
documents.](docs/design/writes.md#upserts)** Scoped, the row it sets
passes the update filter (`Check::before_overwrite`) and `$n`'s documents
go in first (`Statement::with_documents`); a sync replica refuses a
document naming neither id nor key (`UPSERT_KEY`).

**[A `set` or a `del` picks its rows with `order` and `limit`, and
`returning` answers them.](docs/design/writes.md#picked-writes)**
`page_ids` picks them under one lock; `returning` is held to the read
rules (`Hook::answering`).

**[A key of `order` may be an
expression](docs/design/writes.md#order-by-expression)** (`Sort::expr`,
`order_ids_by`), ties in id order; `check_order` refuses an inner `get`,
an aggregate or `expired()` in one.

**[A `put` answers the rows it wrote with
`returning`.](docs/design/writes.md#put-returning)** Each row reaches
`Hook::answering` as the write it was. A write clause goes into every
builder, held to the golden file, and the sync refuses one a replica
cannot answer (`PICKED`), as a scenario holds.

### FenecQL and schemas

**[SQL's spellings are FenecQL, read by the one
parser](docs/design/fenecql.md#sql-spellings)** into the same statements:
no mode, no second front end, no pg wire. What has no FenecQL meaning is
refused naming the FenecQL to write, without consuming its word; `"name"`
where SQL reads a column (`Parser::quoted`) and a regex-like `~`
(`regex_like`) are refused, `<>` stays `!=`.

**[A JOIN on a key is a `lookup` answered flat, or one table's `get` with
the other's rows a filter](docs/design/fenecql.md#sql-joins-subqueries-and-schema)**
(`join::relate`). The flat rows are `Select::flat`, set by SQL alone and
rendered by `ResultSet::answered` after a token's masks; a join that would
answer otherwise is refused. Every table it names is held to a token's read
rules (`Scope::select`) and every name to its table's fields (`Flat::names`,
a `*`'s to what the rules show, `Flat::shown`), read or not; a join found by
`id` whose list passes 100 000 values is read as the `lookup`'s probe
(`Flat::probe`, `Select::probed`). `create table` maps types by
`fenec_ql::sqltypes`, as `fenec import` does; pgvector's `<->` is `near`
held to the index's metric (`Near::metric`); SQL's `order by 1` names the
list's first item.

**[`limit`, `offset` and `top` take a parameter, read where the plan reads
them](docs/design/fenecql.md#count-parameters)** (`Count::at`); a value is
refused as its literal is (`Select::check_counts`).

**[`in (get ...)` is answered before the query, as the list it
is.](docs/design/fenecql.md#inner-gets)** `Database::answered` puts an
`Expr::In` in its place on every path, so `eval` never meets one.

**[`alter collection` rewrites no document; positions are not
places.](docs/design/fenecql.md#alter-collection)** A drop keeps its place
until `compact`; `read_fields` takes places (`Schema::place`), never
positions, and wherever the schema changes the store is told its dropped
places (`Store::set_dropped`).

**[A `json` field holds any value, and a path is a
name.](docs/design/fenecql.md#json-fields-and-paths)** A field name holds
no dot, so a path is an `Expr::Field`; a list of numbers read with no
schema is `f32`s, so `Database::exactly` must walk every place a statement
holds a value.

**[The shell runs a statement once nothing after it can go on with
it](docs/design/fenecql.md#the-shell)** (`fenec_ql::reading`). A new
statement form or lexer syntax owes a place in fenec-cli's `kind`, a mark in
`Parser::cut` wherever the text's end decides a reading the parser
backtracks over, `lexer::Unclosed` taught any new string, comment or
bracket, and a line in `a_statement_written_over_lines_reads_as_one`'s
corpus.

**[A schema declared in code is checked at every open, in the
engine.](docs/design/fenecql.md#declared-schemas)** `declared::plan`
plans additions alone, never guessing a rename; `make schema-golden`
writes `integrations/schema-golden.json`.

**[Named statements are read, and nothing runs one
yet.](docs/design/named-statements.md#what-is-built)** `create statement`,
`drop statement` and `call` parse into `fenec_core::named`'s trees, boxed
so a `Statement` stays a `get`'s size, and are refused before any block
opens (`Statement::is_named`, `NOT_BUILT`), in `scoped()` and MCP's
`write` too, until storage and authority land; `$name` is a definition's
alone, and `call` and `statements` reserve no word: an alias takes either
(`NAMED_COMMANDS`, never `COMMANDS`). They are fenec-core's and fenec-ql's
`statements` feature, outside `full` and off in the workspace's
dependency: the server, the shell and the native library name it, the
module refuses in words of its own, and a path only a definition takes
stays out of the hot parse, `#[cold]`. `Definition::canonical` writes a
field only where it holds something and an operator by a byte of its own;
a change reading a stored text otherwise raises `GRAMMAR_EPOCH`, and `make
fenecql-corpus` holds every FenecQL text of the repository to main's tree.

### Held writes

**[`Fenec-Wait` holds a `set` or a `del` that writes nothing until it
can](docs/design/waits.md#fenec-wait)** (`fenec_http::waits`, 30 s at
most), each run `handle` whole, token and all; a `put`, or the time read
but as a field against `now()` plus or minus integers or parameters, is
refused (`holdable`). The builders refuse `wait` on a database in the
page (`LOCAL_WAIT`).

**[A write wakes the held writes reading what it wrote, a row going to the
group ranking it first](docs/design/waits.md#waking-held-writes)**
(`Waits::wrote`, `waits::settle`). A landing that cannot name its
collections wakes every group (`None`).

**[One held request looks for every group, and a group finding nothing
looks among the rows written since](docs/design/waits.md#the-leaders-looks)**
(`waits::look_all`): a look among the rows written must pick as the whole
pick does, which runs wherever that is not shown -- a load, a compact, a
maintenance or a schema mark moves `epoch` (`wrote(None)`).

### Indexes

**[`@unique` is a `@hash` that asks its bucket before a
write.](docs/design/indexes.md#unique)** `Collection::unique_clash` runs
after the hooks, before any write; a scoped token is told the field alone
(`access::told`).

**[A `@hash` bucket holds its ids ascending, in
runs](docs/design/indexes.md#hash-buckets)** (`engine/bucket.rs`), read in
order through `Bucket::iter`.

**[A hash index's table grows a 256th at a time, and a text index's terms
too](docs/design/indexes.md#sharded-tables)** (`maps::Sharded`): natively,
past `SPLIT_AT`, 256 maps, each key kept beside its hash; the browser
module keeps one map.

**[`@ttl` is an ordered index whose rows expire, and every read leaves them
out at once.](docs/design/indexes.md#ttl)** Every filter must have
`Database::alive` ANDed in; a server sweeps (`fenec_http::sweep`) a row a
pass past its time (`sweep::GRACE`), never a replica, a following tenant
or the browser, and a scoped token reads `expired()` only by the `expired`
grant.

**[Only an `and` chain reaches an index.](docs/design/indexes.md#index-reach)**
An `in` takes buckets only when every element resolves, else the scan.

**[A filter is bound to its collection once a
query.](docs/design/indexes.md#bound-filters)** What `Filter` cannot bind
stays `eval`: an error at the first row, never over none.

**[`@sorted` must give the scan's answer, row for
row.](docs/design/indexes.md#sorted)** Keys order as `Value::cmp_value`,
`null` and `NaN` apart, ties in id order; a literal of another kind than a
typed field's -- or part's -- is no bound or none (`SortedIndex::by_kind`),
and a time claim's `or` edge is folded by the field's type (`fold_kinds`).

**[An index over several fields is its parts' bytes laid end to
end](docs/design/indexes.md#composite-indexes)** (`composite.rs`), a part's
bytes ordering as `cmp_value` orders its values or held apart.

**[An open builds no hash, text, ordered or sparse
index.](docs/design/indexes.md#lazy-indexes)** The first read fills its
`Derived`; a write skips an unbuilt one, and `stats` and `memory_bytes`
count what is built, never building to count.

**[A server warms what an open left for the first
read](docs/design/indexes.md#warm)** (`--warm`, `fenec_http::warm`), an
index at a time under the read lock; a bench's warm-up reads every index
its writes keep up.

**[A point is two `f64`s on Redis's sphere; `@geo` is Morton
keys](docs/design/indexes.md#geo)**: `distance` uses `geo.rs`'s own
series, never libm; a list bound for a point is read as written
(`DataType::keeps_numbers`).

### The browser module

**[Threads are `cfg`'d out of WASM.](docs/design/browser-module.md#no-threads-in-wasm)**
Nor has it a clock: `now()` reads the time a statement is handed
(`Database::set_clock`), or errors.

**[Indexes are features; a build without one opens a file declaring
it.](docs/design/browser-module.md#index-features)** A missing one is a type
of no value (`off.rs`), its checks behind `EVERY_INDEX`, and a replayed
`create index` of one is taken unbuilt, never refused; a crate using
`fenec-core` names `full`, and `fenec-ql` and `fenec-abi` name it in their
dev-dependencies alone.

**[The rest of the language is features
too.](docs/design/browser-module.md#language-features)** Its checks ask
`crate::FULL` first; a build without one opens every file, refusing what
it cannot answer -- a function by name (`missing_function`), every
statement over a `nocase` or collated field's collection
(`refuse_unbuilt`). `FENEC_WRITE_FIXTURE=1` rewrites `tests/language.fenec`.

**[What is core, and why one module
ships.](docs/design/browser-module.md#what-is-core)** What any app expects
is core, a domain's extra a feature.

**[The module holds one copy of a
generic.](docs/design/browser-module.md#one-copy-of-a-generic)** Sort
through `sort_unstable`, `text::best_first` or `order_rows`; slice a `str`
with `get`, never `s[a..b]`.

**[The core lost what it held twice or for one
use.](docs/design/browser-module.md#size-cuts)** What std brings in for one
call is the crate's own (`num::parse_i64`).

**[The module grows nothing by doubling past a
mebibyte.](docs/design/browser-module.md#chunked-memory)** Its memory never
goes back, so vectors are 1 MiB chunks (`ROWS_CHUNK`) and a segment
doubles only to its sealing record.

### Vector search

**[wasm32's SIMD kernels match the scalar ones bit for
bit.](docs/design/vector-search.md#simd)** `vector::simd` keeps the
scalar loop's order (`web/fenec.test.js`).

**[aarch64's strips are NEON, to the same
bits.](docs/design/vector-search.md#neon)** `vector::neon` multiplies and
adds unfused in `strip8!`'s order, held by
`f32_and_f16_kernels_match_the_scalar_strips`.

**[A vector goes out as its
`f32`s.](docs/design/vector-search.md#vectors-out)** JSON writes the
shortest text reading back as it (`json::num32_into`), but for
`json::TIE`.

**[A number is read exactly.](docs/design/vector-search.md#numbers-in)**
`num::parse_f64` reads one, and natively `num::clinger` a list of numbers
alone into `f32`s.

**[A vector parameter goes into the module as
`f32`s.](docs/design/vector-search.md#vector-parameters)** `run` hands it
beside the JSON (`vectorsApart`), `-0` as `0`.

**[A quantized index holds codes, and `near` orders by the
vectors.](docs/design/vector-search.md#quantized-indexes)** `Space` orders
the beam by the stored vectors, so every score is exact.

**[A bit code is a residual's signs from a learned
centre.](docs/design/vector-search.md#bit-codes)** Its centres are
learned alike on every target and travel in the graph record
(`BIT_CENTRED`).

**[Filtered `near` needs its
fallback.](docs/design/vector-search.md#filtered-near)** A filtered ANN walk
short of the limit must search the whole set.

**[`sparse<N>` is pgvector's `sparsevec`, and `@inverted` answers
exactly.](docs/design/vector-search.md#sparse-vectors)** Every way in goes
through `sparse::normalise`.

### The JS client and Cloudflare

**[One module and a client entry, by where the queries
run.](docs/design/js-client.md#the-module-and-the-client)** `fenec-lite` and
`fenec-min` are test builds, never shipped; `web/client.js`
(`@fenecdb/web/client`) is `builder.js` and `http.js` alone
(`web/fenec.client.test.js`), and `FenecHttp` calls `fetch` on its own
(`#request`), never as its method. On a client reading near (`reads:
'nearest'`), `FenecHttp.live`'s streams and polls stay home and its runs
read near.

**[A Durable Object keeps a database as a file
would.](docs/design/js-client.md#durable-objects)** `persist` writes an
image, then a row a flush (`storage.sql`), a new image and the old's drop
in one `transactionSync`; a failed flush stores nothing and rejects every
later `persist`. The example opens and restores inside
`blockConcurrencyWhile`.

**[An object's alarm keeps its
database.](docs/design/js-client.md#alarms)** `schedule` sets one only when
something is due; `maintain` returns a failure as `error`, never a
rejection; every database let go goes through `#retire`, a released one
refusing `restore`.

**[A live query learns what changed from the change
ring.](docs/design/js-client.md#live-queries)** `Query.reads` names each
collection read (`Lives`), `fenec_changes` those written.

### Text

**[`collate und` and `collate tr` are ICU's
orders.](docs/design/text.md#collations)**
`crates/fenec-core/tools/collate/gen.py` writes the tables, and equality
stays the bytes'. In the browser a statement meeting a chunk it lacks is
refused (`collate::refuse`), a write before it changes anything, and
`Fenec.query`, the builder, `restore` and the sync fetch it and run it
again.

**[`nocase` text compares by its case fold wherever it
compares.](docs/design/text.md#nocase)** Every such place goes through
`Field::text_order`; the fold is `str.casefold()`'s, Turkish's under
`collate tr`, and a field made `nocase` or no longer is a `rebuild`.

**[`match` prunes with MaxScore.](docs/design/text.md#maxscore)** Scores
add in `f64`; `pruning_never_changes_the_answer` holds it to an
exhaustive merge.

**[`@text` matches whole words unless told
otherwise.](docs/design/text.md#tokenizer)** It folds `I`, `İ`, `ı`, `i`
together; unspaced scripts go by grams (`text::grams`).

**[The text index is derived and never
persisted.](docs/design/text.md#text-index-is-derived)**

**[A text index's words are a table of their own
(`text/terms.rs`).](docs/design/text.md#term-table)** `memory_bytes`
counts each list's allocation past its bytes (`ALLOC`).

**[`fuzzy` and `prefix` walk a sorted copy of the
words.](docs/design/text.md#fuzzy-and-prefix)** Every write keeps it up
once built, never in the file; a word reaching more than
`MAX_EXPANSIONS` (10 000) words is refused, never cut.

**[`rerank` deliberately uses no index.](docs/design/text.md#rerank)** It
reorders `match`'s candidates by exact distance over stored vectors.

**[`fuse` adds ranks, not scores.](docs/design/text.md#fuse)** Each search
runs `candidates` deep, filtered; a row scores `1 / (k + rank)` from
each, `k` 60.

**[`highlight()` re-reads the text; `facet` counts beside the
rows.](docs/design/text.md#highlight-and-facet)** Marks are the searched
index's tokens; a facet past `MAX_FACET_VALUES` (10 000) without `top` is
refused, never cut.

**[A disjunctive facet's filter is split as
written.](docs/design/text.md#disjunctive-facets)** `scoped()`, `@ttl`'s
`alive` and anything else ANDed in after must reach every split filter
through `each_filter_mut`.

### lookup and aggregates

**[`lookup` chains, and the chain is still
positional.](docs/design/lookup-and-aggregates.md#lookup-chains)** `on
child = parent` names the level just above, and a collection appears once
a query.

**[`lookup` is one bucket probe per parent, not a
join.](docs/design/lookup-and-aggregates.md#lookup)** An unindexed child
key is refused; nesting is never a `Value`.

**[`required` is the other half, and it is a
pass.](docs/design/lookup-and-aggregates.md#required)** It drops a parent
no child matches before `offset` and `limit`.

**[A `lookup` over a key of several fields is a run of an index over
several fields.](docs/design/lookup-and-aggregates.md#lookup-over-several-fields)**
`tests/composite_lookup.rs` holds it to plain `get`s.

**[A `lookup` level may fold its children, and a `lookup` hangs off
groups.](docs/design/lookup-and-aggregates.md#folded-levels)** A level is
the `get` its children would be; `tests/lookup_aggregate.rs` holds it.

**[An item of a select list is an expression, and an aggregate folds
one](docs/design/lookup-and-aggregates.md#select-expressions)**: an
aggregate is a call, never an `Expr` variant, and `count(distinct)` past
`MAX_DISTINCT_VALUES` is refused.

**[`approx_count_distinct` is HyperLogLog, and `having` keeps
groups](docs/design/lookup-and-aggregates.md#hyperloglog-and-having)**
(`hll.rs`): its own hash, the same on every target.

**[`bucket(at, 1d, 'Europe/Berlin')` reads a zone's
clock](docs/design/lookup-and-aggregates.md#time-zones)** (`tz.rs`):
`tools/tz/gen.py` writes `tz/zones.bin`; before 2000 a named zone is
refused.

### The native library and sync

**[An app keeps its database through
`fenec-ffi`.](docs/design/native-and-sync.md#the-native-library)** A text
is answered through `fenec-abi`, as the browser module's is, to the same
bytes. A handle indexes a table of `Arc`s, never a pointer; every
call returns a code under `catch_unwind`, built in the `ffi` profile,
which unwinds; `include/fenec.h`, by hand, is held to the exports by a
test. A long read pins, the fsync runs after the lock and a buffered write
goes behind it (`Native::after`), as a server's; JNI passes text as UTF-8
bytes; `<file>.lock` is an `fcntl` record lock, never `flock`.

**[A replica's sync is a state machine; the network is the
binding's.](docs/design/native-and-sync.md#sync-core)** `fenec_abi::sync`
does no I/O: a binding feeds it events in turn and performs its actions.
Only `fenec-ffi` turns on `fenec-abi`'s `sync` feature; the browser module
has none of it.

**[Both sync runners are held to one scenario
file.](docs/design/native-and-sync.md#sync-scenarios)** A behaviour of
either changes in `integrations/sync-scenarios.json` first; `make
sync-scenarios-check` fails unless both ran every scenario.

### The router

**[The router tells a page one CORS rule, its
own.](docs/design/router.md#cors)** With `--http-cors` it answers
preflights and puts its own CORS headers in place of a node's
(`cors::is_cors_header`), never both.

**[The router looks at a pooled connection first, and resends only what
cannot have run or may run twice.](docs/design/router.md#pooled-connections)**
A request not sent whole goes again, one sent whole only if `replayable`
(a `GET`, `HEAD`, `OPTIONS`, keyed or marked read), judged by `Stage`, not
errno; head and body are one `writev`. Every dial goes through `dial::resolve`, caching nothing; every
body read allocates through `zeroed` and refuses an answer with no length.
A held request watches its client for the head (`answered_or_gone`,
`relay_send` at a relay): an end goes on to the node, never taken for
gone; a reset lets the node's connection go.

**[A failover ends what the router has out to the node for the tenants it
moved.](docs/design/router.md#failover-cuts)** `promote_on` has `Pool::cut`
shut what is out to the node under the tenant's keyed mark (`mark_of`),
one word with the connection's state, never an answer begun (`ANSWERING`)
nor a descriptor reused (`CLOSED` before the close); a
cut request follows `replayable`'s rule, and a forward reads `Pool::moved`
before the directory and before its first byte. Only a failover cuts,
granting the nodes it moved tenants to as it ends (`granted_after`). A
router following the directory cuts too, as a write lands (`Arrived`)
naming the node failed over off (`failed_from`, cleared by every other: a
move cuts nothing), every
bucket moved first (`cut_all`); it grants nothing, asking a copy that
refuses a replay unrun again while its client stays, counted once, for
two thirds of the recorded lease or `--upstream-timeout`, whichever is
sooner (`copy_pause`).

**[A query across tenants is the router's admin's, merged as the request
says.](docs/design/router.md#queries-across-tenants)** Nodes run it
read-only; the router parses no FenecQL, fails and names a tenant its
merge cannot hold exactly, and refuses (413) past its bounds, never cuts.

### Integrations and deploy

**[`integrations/` may use outside packages; the crates may
not.](docs/design/integrations.md#integrations)** A builder change starts
in `web/builder.js` and a case in `golden.mjs`, then every SDK's follows,
held to `integrations/builder-golden.json`; `make types-check` holds
`fenec.d.ts` to `fenec.js`. A builder tab on the site (a FenecQL page's,
Your language's tour) is a golden case's chain rendered by
`site/builder_tabs.py`, never written by hand: its statement held to the
page's or written from the case, the JS and Python tabs run by its
`--check`, the others tests of their SDKs (`make docs-builder`); a
builder's API change owes its renderer. Go's and .NET's SDKs bound a
request by its context or token, never the HTTP client's timeout; a
store checks its names against FenecQL's and passes values as
parameters.

**[The desktop app is a window on a studio a `fenec-server` serves, one
server per file.](docs/design/integrations.md#studio-desktop)**
`integrations/studio-desktop` starts a server only in `server.ts`, its token
in an allowlisted environment (never argv), its stdin a pipe the app holds
(`--exit-on-stdin-close`: no server outlives the app, `kill -9` included); a
token reaches a page only through `session.ts` (a frozen
`window.fenecHost`, defined only where `location.origin` is the server's).
A studio window is `sandbox: true` (the event bridge only) and held to its origin, a window with the RPC bridge to its `views://` folder
(`holdTo`). A saved server's token is in the Keychain alone with its address
(`token.ts`), never sent elsewhere or along a redirect; a failure is a
notice window, never a native alert. Each window has its own data store, a
removed connection's cleared by `STORE_KINDS` (never
`clearStorageData("all")`) after its window closes (`checks/storage.mjs`).
The shipped bundle holds no test hook: an environment read in it is one
unless behind `__TESTABLE__` (`checks/release.mjs`). The toolchain is fetched into
`.hutch-home/`, never `~/.hutch` or a shell file, and held to
`hutch-pin.json` (`scripts/electrobun.mjs`); signing runs only by
hand from `main`, in an Environment given its reviewer and `main` rule
before its secrets
(`desktop.yml`).

**[Grafana's data source binds a dashboard's values; it never splices
them.](docs/design/integrations.md#grafana)** Names become `$n`s
(`fenecql.go`); only `get`, `select` and `explain` go out.

**[`deploy/` is the regions' kit, held to the binaries by two
checks.](docs/design/integrations.md#deploy)** Tokens go in env files or
`LoadCredential=`, never on a command line, a role a user each; no
`ProcSubset=pid`. The tunnel passes only tenant paths, never
`_replication` (`deploy/test/ingress.py`); the bucket copy is `rclone
copy`, never `sync`; a standby router's unit conflicts with its primary's,
promoted by `POST /_replication/promote` and the units switched at once;
`tests/datadog.rs` holds the alerts to scrapes.

**[A release is installed from its own files, each checked before it is
used.](docs/design/integrations.md#install-channels)** Homebrew's formula,
`install.sh`, the `.deb` and the `.rpm` install `release.yml`'s binaries by
checksum, building none, so a published release's assets are never replaced
(`publish` only adds). The formula is `tools/homebrew-formula.py`'s alone,
held by its `--check` and `--verify`, and pushed by `packages.yml` once
published, never a pre-release or a version older than the tap's, one run at
a time, `HOMEBREW_TAP_TOKEN` a `homebrew` environment secret given `git` by
its environment alone. `install.sh` holds only functions, assignments and
comments before its last line, `main "$@"` (`site/build.py`), takes curl or
a wget checking certificates, checks the sum before it unpacks, touches no
shell file and evals nothing (`site/test_install.sh`). A package is
installed and run in a container before it is kept
(`tools/linux-packages-check.sh`); a new channel names what it installs by
its checksum.

## Conventions

- `Error` (`fenec-core/src/error.rs`) is the single error type,
  allocation-free, no `Box`; `fenec-http` maps it onto HTTP statuses
  (`api::status_of`).
- [Unit tests live inline](docs/design/conventions.md#test-binaries) (`#[cfg(test)] mod tests`);
  cross-crate and protocol tests in `crates/*/tests/`, a file each, one
  binary a crate through `tests/all.rs` (`autotests = false`: a new file
  needs its `mod` line). A test reading the process's own counts
  (`/_metrics`, the statements'), one whose servers hold many threads (past
  macOS's 2 048 a process), and `tests/audit.rs` and `tests/refusals.rs` are
  each a `[[test]]` of its own, the other tests' servers asking with no refusal
  wait (`audit::set_delay(0)`). Measurement programs (`examples/`,
  `fenec-bench`) are wired to `make`, not CI.
- [A test that fails and passes on a rerun](docs/design/conventions.md#reruns) is a bug, the
  test's or the code's: reproduce it in a loop under load (`docker run
  --cpus=3` beside busy loops is GitHub's runner) and fix it, never retry it
  away or lengthen a sleep. A test waits for an event, not a time: it holds
  what it races (`Replica.setOnline(false)` before a write it reads back) and
  bounds only the wait for what must happen. A step launching Chrome reads
  Chrome's files first (`warm`, `warm-chrome.mjs`): a longer wait for
  `waiting for dynamic debugging port` only moves the bound. A CI log names
  every failing test with its assertion (`node --test
  --test-reporter=spec`, Gradle's `testLogging`, `dart test`'s GitHub
  reporter); a failing mobile job uploads `test-reports-android` or
  `test-results-apple`.
- [A test reads the clock from below](docs/design/conventions.md#the-clock) (`took >= wait`;
  `taskpolicy -c background` gives macOS's timer leeway), bounding it from
  above only where the time is the property, by what it stands against with
  seconds to spare; elsewhere it asserts the event, the count or the order
  (`audit::would_wait`, `Router::grants_waiting`, `Router::round_grants`).
  A timeout given a test's servers or clients bounds every request they
  make.
- [A server a test or a harness starts](docs/design/conventions.md#ports) listens on port 0, never
  a fixed port or one found free and handed over, found by its
  `listening on: http://<address>` stderr line, a router told its nodes by
  the addresses they said; only a site needing its address before it binds
  takes a port the system picks, a stand-in listener holding it till then.
- [A change a user meets](docs/design/conventions.md#docs-a-change-owes) says so where a user
  looks, in the same pull request: its page under `site/content/docs/`,
  README's feature table and the links beside it, the home page and docs
  index where a visitor would pick fenecdb for it, and the SDK lists
  (`languages.html`, `mobile.html`, `sharding-read-replicas.html`,
  `integrations.html`) when an SDK changes; after sections change, commit
  the next `site` deploy's `search-embeddings` artifact as
  `site/search-embeddings.json` (`gh run download <run> -n
  search-embeddings`) where it differs.
- An invariant's rule goes in CLAUDE.md in a few sentences: what must hold,
  what must never be done, what enforces it and what a change owes. Its
  measurements, rejected alternatives and history go in its `docs/design/`
  file, in the same pull request; a new invariant gets both.
- Comments explain *why* -- a measured cost, a trap that was hit, an
  alternative rejected; match that when adding code.
- All prose in the repo (comments, docs, README) is English.