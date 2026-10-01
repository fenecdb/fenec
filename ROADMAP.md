# Roadmap: the general-database gaps

fenecdb is positioned as an embedded document database with full-text and
vector search built in. Against that, four gaps are worth closing, and three
are not. This is the plan for the four; each phase ships on its own, with its
measurements, as every feature here does.

| Phase | Feature | Size | Why it fits |
|---|---|---|---|
| 1 | `alter` and `@unique` -- **done** | small | expected of any database; both ride on what exists |
| 2 | Objects (`json` fields, paths) -- **done** | medium | the largest gap for a *document* database |
| 3 | `in (get ...)` and `@ttl` | small each | the reverse of `lookup`; caches and sessions |
| 4 | TLS 1.3, our own | large | the security story ends at a terminator today |

Not planned, on purpose: a general JOIN and full SQL (FenecQL and `lookup`
are the design; SQL is spoken where tools need it, over the catalog), several
writers to one file or MVCC (the single writer is what removes the WAL, the
row headers and the visibility map; scale is a file per tenant), multi-writer
replication. A `decimal` type is wanted but waits behind these.

The rules every phase keeps: zero dependencies in the crates; the browser
module grows only by what a feature costs it, measured; a feature the browser
build leaves out opens a file that uses it (`off.rs`); a binary from before a
new record kind or tag refuses the file rather than misread it; limits error,
they do not truncate.

---

## Phase 1: `alter` and `@unique` -- done

Both shipped as designed below, with three changes the code asked for:

- **A dropped place is not a field.** `Schema::fields` stays the fields a
  document is read and written by, and the places dropped ones held are a
  list beside it (`Schema::dropped`), written into the schema as nameless
  fields of type tag 12. A tombstone field in `fields` would have had every
  listing of a schema -- the catalog, `describe`, the HTTP schema, `fenec
  types`, the sync layer -- learn to skip it. The store keeps the list and
  works a field's place out from its position; `read_fields` takes places,
  worked out once a query, since once a row took a scan of a million rows
  21.2 -> 21.8 ms.
- **The end of a payload is told where the skip looks for the next tag
  anyway** (`codec::skip_field`), not by "the last field the schema had
  when the record was written", which no record says. A payload cut inside
  a value is still refused.
- **A unique index is built by the first write's check after an open, not
  by the open.** The check asks the index, which builds it from the
  documents, so it is as exact, and an open that never writes -- a replica,
  `fenec types` -- never pays for it.

Measured (in memory, a million rows): an add, a rename and a drop take
0.025, 0.021 and 0.004 ms; five scans 1.7% slower to 1.7% faster (21.2 ->
21.6 ms, 36.8 -> 36.2, 39.1 -> 39.3), as the same build with the reads
written as before was, the browser module's 1.70 -> 1.71 ms; a compact
taking a dropped field out 95 ms against 56. An open with a unique field
costs what it did (19 ms for a million rows), its first write 259 ms (15
over 100 000), a put after it 940 ns against 850 under `@hash`, one without
a unique field 852 against 848. The browser module: `@unique` 0.7 KB
brotli, `alter` 4.0 KB.

### `alter`

```
alter collection orders add field note text
alter collection orders drop field note
alter collection orders rename field total to amount
```

Documents are stored positionally (`Store::read_fields` walks the tagged
values in schema order), which decides what each change costs:

- **add** appends a field at the end. No document is rewritten: a payload
  that ends before a position reads as `null` there. `read_fields`,
  `decode_value`'s callers and the filter binding (`Filter`) learn that an
  exhausted payload is nulls, not corruption -- today `skip_value` at the end
  of a payload is `Error::Corrupt`, so the check moves to "past the last
  field the schema had when the record was written" vs. "truly short".
- **rename** is the schema alone: positions do not move.
- **drop** marks the position dropped in the schema (a tombstone field: no
  name, skipped on read, refused on write). `compact` rewrites documents
  without it and removes the tombstone. An index on the field is dropped
  with it.
- **type changes** are out: a rewrite of every document under the write lock
  is what `alter` should never be.

On disk: `REC_ALTER` (kind 5) today carries an index added; it gains an
operation byte (add / drop / rename) and is undone in a block like a create
index (`Undo`). A binary from before reads a kind-5 record it does not know
as an index -- so the new operations get a record kind of their own (12),
which an old binary refuses. Over the pg wire it answers `ALTER TABLE`, as a
create answers `CREATE TABLE`, and the catalog shows the field at once.

Cost to watch: the null-padding check sits on every row read. Measured
against the scan benchmarks (`make bench`), it must not move a scan.

### `@unique`

```
create collection users (email text @unique, name text)
```

A `@hash` index that refuses a second document with the same value:
`IndexKind::Hash` gains a `unique` flag (schema index byte, a new value so an
old binary refuses it). Checked in the write path where `insert` checks ids
today -- `put`, `set`, and in a block against the block's own writes, which
the hash already holds -- and refused with `Error::Duplicate` (`23505`,
`409`), the statement put back whole. `null` is not a value: two nulls do not
collide, as in SQL. `create index ... @unique` over existing data fails if
duplicates exist and names one. Replicas apply, never check: the primary
already did.

The hash index is derived and built lazily on first read (`Derived`); a
unique one has to be built before the first write after an open, so the
check is exact. That is an open-time cost for collections that declare it,
measured with `make open-bench`.

---

## Phase 2: objects -- done

Shipped as designed below, with these changes the code asked for:

- **Tags 13 and 14, not 12.** Phase 1 took type tag 12 for a dropped place,
  and the value and type tags share their numbers, so an object is value
  tag 13 and a `json` field type tag 14 -- new to both spaces, 11 and 12
  being a collation's and a dropped place's. A binary from before refuses
  the schema (*unknown type tag 14*) before it could meet an object.
- **A path is a name, not an `Expr::Path`.** A field's name holds no dot
  (`Schema::new` refuses one), so `meta.source.rank` is an `Expr::Field`
  whose name `Schema::path_of` splits; every place a name already went --
  `select`'s columns, `order`'s keys, an index's name in `Fields`, the
  planner's equalities and ranges -- takes it unchanged, and an
  `Expr::Path` would have been one more variant for every match over an
  expression in six crates.
- **A path's index is a field of its own, beside the fields**
  (`Schema::paths`, a `json` field named by the path), written inside its
  json field's entry in the schema. One per path, so a field holds several,
  and a rename or a drop of the field takes them with it.
- **`@sorted` on a path holds numbers and text, and holds the rest apart.**
  A value has no declared type there: numbers are keyed as `f64`s (an int
  past 2^53 has no exact place and goes apart), text by its bytes, every
  number below every text as `cmp_value` ranks them; a boolean, a list or an
  object goes apart too, and while any is held the index answers nothing
  and the scan does. `@hash` files a whole float under the int it equals, so
  `3` and `3.0` share a bucket.
- **Path updates are in**: `set docs {meta.lang: "en"}` sets one key and
  keeps the rest, an object made where there is none, a non-object on the
  way refused.

Measured over 100 000 documents: a path filter scans in 5.7 ms (a text
field's equality 4.2), 0.025 ms through `@hash`; a range 7.2 ms, 0.009
through `@sorted`; an index on a path builds in 11 (`@hash`) and 21 ms
(`@sorted`). A scan with no json field moved by nothing beyond noise (3.47
-> 3.38, 4.17 -> 4.21, 3.06 -> 3.08, 2.72 -> 2.60 ms), the browser module's
filter 1.67 -> 1.66 ms. The browser module grew 29.8 KB, 9.6 KB brotli
(164 365 against 154 731 bytes) -- more than the few hundred bytes guessed
below: the object codec and reader, the path reads and writes, the
ordered index's json kind, and reading a list of numbers as written where a
json field is given one (3.7 KB), which the quick read into a vector's
`f32`s had cut to seven digits. The LangChain and LlamaIndex stores were not
moved onto a json field in this phase.

```
create collection docs (title text, meta json)
put docs {title: "a", meta: {lang: "tr", source: {site: "x", rank: 3}}}
get docs where meta.lang = "tr" and meta.source.rank >= 2
create index on docs (meta.lang) @hash
```

`limits.html` refuses object values today ("a field you want to filter on
should be a field"). A document database is expected to hold them, and the
integrations already flatten metadata into fields to get around it.

- **Value and codec.** `Value::Object(Vec<(String, Value)>)`, keys sorted
  and unique, under a new tag (12) so an old binary refuses it. A `json`
  field holds any value -- object, list, scalar -- untyped inside; typed
  fields stay as they are. Encoding is the same tagged form as a list, a key
  before each value.
- **Paths.** `Expr::Path(field, keys)` in the parser; the lexer learns a
  `.` between names (today a dot is only read inside a number). A path that does not exist in a document is `null`.
  `Filter` binds a path as it binds a field: the field's position once, then
  the keys walked per row.
- **Indexes on a path.** `@hash` and `@sorted` take a path; the index reads
  the value at the path. Text, vector and sparse indexes stay on top-level
  fields.
- **Transports.** JSON is native. Over the pg wire a `json` field is `jsonb`
  (OID 3802), sent as text and in jsonb's binary form (a version byte and the
  text), which psycopg, asyncpg and pgx decode. `COPY` reads and writes it as
  JSON text.
- **Integrations.** The LangChain and LlamaIndex stores move metadata into
  one `json` field and filter on paths; their framework suites are the test.
- **Browser cost.** Measured with `make wasm-sizes`; the path walk is a few
  hundred bytes, the codec branch less.

---

## Phase 3: subqueries and expiry

### `in (get ...)`

```
get orders where customer in (get customers select id where country = "TR")
```

The reverse of `lookup`: filter by rows of another collection. Uncorrelated
only -- the inner `get` runs once, before the outer one, and its single
column becomes the set an `in [..]` is answered with today (one bucket per
element on a `@hash` field or `id`, a scan otherwise). The set is capped
(100 000 values) and a larger one is a query error, never a cut set. Scoped
tokens AND their filter into the inner `get` too (`scoped()`), or it reads
what the token may not.

### `@ttl`

```
create collection sessions (user text @hash, seen timestamp @ttl(30m))
```

A timestamp field whose rows expire that long after its value. Two parts:

- **Reads are exact at once.** A query over the collection adds
  `seen > now - ttl` to its filter, so an expired row is never returned even
  before it is deleted. In the browser `now` is passed in, as every time is
  there.
- **Deletes happen on the primary.** The server's sweeper -- beside the graph
  keeper, a pass a minute -- deletes a range of the `@sorted` index at a time
  as ordinary writes, so replicas, `/_changes` and archives see them as any
  delete. A replica never sweeps. `_idempotency` already does this by hand
  and becomes the first user.

---

## Phase 4: TLS 1.3, our own

Today `SSLRequest` is answered `N`, and a port off the machine needs a
terminator (stunnel, nginx `stream`, Caddy). A database whose pitch includes
security should speak TLS itself -- and the zero-dependency rule means
writing it. That is the largest and riskiest item here, so it is scoped
tightly and gated hard.

**Scope.** Server side only, TLS 1.3 only, in `fenec-pg` and its HTTP
listener; `fenec-wire` and the browser module carry none of it.

- Key exchange: X25519 (RFC 7748), new.
- Ciphers: `TLS_CHACHA20_POLY1305_SHA256` -- ChaCha20-Poly1305 exists
  (`crypto.rs`, RFC 8439 vectors) -- and `TLS_AES_128_GCM_SHA256`, which some
  clients require; AES in constant time without tables is bitsliced, the
  slower half of this phase.
- Key schedule: HKDF over the existing SHA-256 and HMAC.
- Certificates: ECDSA P-256 signing, so a Let's Encrypt certificate works
  (an RSA key's PKCS#1 v1.5 / PSS signing as a second step, constant time
  with blinding). The server reads its chain and key from PEM and sends the
  chain as it is; it parses no certificate but its own key.
- pg wire: `SSLRequest` answered `S`, then the handshake on the socket;
  PostgreSQL 17's direct TLS (`sslnegotiation=direct`) as well.
- `--tls-cert`, `--tls-key`, read again on change as `--jwt-keys` is, so a
  renewed certificate needs no restart.

**Out of scope.** Client-side TLS (replication, the router reaching nodes,
`--follow` reaching PostgreSQL) needs certificate-chain validation, a second
project; inter-node links stay plain behind a private network until then.
No TLS 1.2, no client certificates, no session tickets in the first cut
(resumption only costs a full handshake).

**Gates before it ships.** RFC 8448's example handshakes byte for byte;
X25519 and P-256 against their RFC and Wycheproof vectors; a fuzz target on
every parser the handshake reaches; interop in CI with `psql
sslmode=require`, `openssl s_client`, curl, Go's `crypto/tls`, Java's JSSE
(JDBC) and Node; constant-time review of every secret-dependent path; and a
note in `SECURITY.md` that the TLS stack is our own, so a reader can choose
a terminator instead.

---

## Order and what each phase is measured by

1. `alter` and `@unique` -- scan and open benchmarks unchanged; browser size.
2. Objects -- path filters against a field-for-field twin collection, as
   `tests/sorted.rs` holds `@sorted` to the scan; the integrations' suites.
3. Subqueries and expiry -- the inner set against the same query written by
   hand; expiry exact before and after a sweep.
4. TLS -- the gates above; handshake latency and throughput against a
   terminator in front of the same server.
