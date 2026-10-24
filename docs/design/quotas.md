# Quotas and retries

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Quotas and retries*: a tenant's quotas, how the sync meets them, and the wait every 503 names. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Tenant quotas

**A tenant is held to its quota by its node, a block as it lands**
(`fenec_http::quota`, `fenec_core::engine::limit`). A node's tenants
share its disk, memory and threads, and only `--max-connections` and
`--max-memory` bounded them together. The node's flags
(`--tenant-max-bytes`, `-rows`, `-streams`, `-requests`,
`--tenant-writes-per-sec`, `--tenant-write-bytes-per-sec`) are every
tenant's, and a tenant's own set (`PUT /_admin/tenants/<t>/quota`, kept as
`<t>.quota` beside its file, written beside and renamed) goes over them, 0
for none. The file's bytes (`appended`, no system call) and the rows of
its collections (`Database::documents`, the `_`-named being the server's)
are asked as each block lands (`Database::set_limit`, a `Landing`): the
record's length is known to the byte there, so a block that would take the
file past its quota is put back as a failed one is, 507 -- asked before
the write ran, one `set` over every row would double a file at 99% of it
-- and so is each spill of a block past 16 MB, before it is written, or
every refused block left its spills dead in the file past the quota. A
block that only takes away (`grows` false: deletes, a drop, the server's
own rows, a keyed `del`'s key among them) always lands and a `compact`
always runs, and a file within a quarter of its quota is compacted by the
node once an eighth of the quota is dead (`link::quota_policy`), where the
policy every file has waits for 64 MB and a tenant at a 10 MB quota would
have deleted to no avail. Streams (subscriptions, `/_changes` waits,
`Fenec-After` waits -- once `after` has looked and must wait, its `slot`
taken then: taken for every request sent with the header, a tenant whose
live queries' subscriptions filled its quota, 25 of a default node's
connections, had every read after a write the node held refused 429, each
run of a live query reading near and every SDK's read-your-writes read
that reached home, `tests/quotas.rs` failing with the slot taken first;
a request without the header looks once as before: `make requests-bench`
in three ABBA quartets under the bench lock at loads of 3.4-4.9, a row by
id 0.022 ms p50 in every run either way, eight clients 143.6-147.2k a
second against main's 143.9-147.4k, a put 0.022-0.023 ms and 124.9-129.2k
against 0.022-0.023 and 127.5-128.6k, one branch run at a load of 4.7 left
out), held writes (`held`, `Fenec-Wait`'s: a plan sizes how
many workers wait apart from how many dashboards stream, and counted
together forty couriers waiting left a dispatcher's map no stream to open)
and requests in flight are counters a request takes in `serve_connection`,
`respond` and `handle_tenant`, only where the quota is set; rows and bytes
written a second are token buckets a second deep, a write let through
where it is known to be one (`quota::writing`, a thread-local as
`access::within` is) while a row's worth is left, and charged by the limit
as it lands -- the record's writes to the app's collections
(`Landing::writes`, `Database::app_writes`: an `Idempotency-Key`'s row is
the server's), on a request's own thread alone (`writing_here`: the sweep,
a compact, a replica's apply are the node's) -- so a write larger than a
second's worth goes through, the next waits its debt out, and a write that
wrote nothing costs nothing: taken as a write started, 45 claims held at
once by a fleet on a small plan were refused for `writes` before any for
being held. A held claim the rate refuses as it is to run stays held
(`Ticket::pause`, `rated_out`, `Held::write_wait`) and runs once a row's
worth is back, rather than come back at the end of every group. 429 with
`Retry-After` in whole seconds rounded up, as HTTP has it, and
`retry_after_ms` the wait itself (`quota::millis`): a bucket of 600 a
second refills a row in 1.7 ms, and fifty couriers told a second waited
fifty seconds between them. On a `--dir` node that sets neither
`--tenant-max-streams` nor `--tenant-max-held`, each is a quarter of
`--max-connections` (`quota::shared`, logged at the start): every stream
and held write is a connection and its thread, `--http-max-streams` counts
a database's own -- each tenant's on a node, not all of theirs together --
and nothing counted a tenant's held writes, so a hundred workers of one
tenant took every connection of a default node and its neighbours were
answered 503. Every refusal carries `Fenec-Quota`
and `"quota"` (with `"retry_after"`, a rate's `"retry_after_ms"` too, which
SDKs reading no header take),
and `quota::noted` counts it by quota (`fenec_quota_refused_total`, never
a tenant label) and logs it as an audit `quota` event; its message names
the bound, never what the tenant holds, so a scoped user is told the
tenant is full and not how many rows the others make. Left out: memory
(`memory_bytes` counts what has been built, which a read changes), a
statement's time (the engine stops no scan) and rows per collection. The
router enforces nothing -- it parses no query, and a tenant lives on one
node -- but keeps each tenant's quota in its directory's `quotas` and
sends it to the tenant's node, standby and replica, to a move's target
before the flip, and to a promoted copy again. The tests hold eight
writers to a file's quota and eight to a rows quota exactly, and a tenant
at its streams or requests quota beside another answered, by what each is
answered. A lone put held to bytes and rows took 593 ns against 566 in
process, and 618 with the bytes bucket's mutex and clock besides; over
HTTP a tenant with every quota answered as one with none, reads 20.9 us
either way and writes 23.7 against 23.6 (`make quota-bench`, in turns).
And a tenant with none costs nothing measurable: against main in ABBA
turns, `make requests-bench`'s one client stayed 0.021-0.022 ms a row by
id and 0.020-0.023 a put (main 0.022-0.029 and 0.022-0.029), eight
clients' 137-144k and 115-121k a second inside main's 122-143k and
89-153k; `make shard-bench`'s tenant request 19.5-21.7 us straight to the
node and 43.7-46.0 through the router, main 19.9-21.8 and 43.6-45.7, in
twelve runs before the machine grew busy. The limit is native only, as the
fence is: the browser module grew 63 bytes, the new error's words, and
brotli made it 49 smaller.

## Quotas in the sync

**A tenant's quota is a refusal where waiting makes no room, a wait the
server names otherwise.** A synced write a tenant node refused 507 for its
quota of `bytes` or `rows` was retried as a 5xx: room comes only from
deletes, and writes go in order, so every write after it -- the delete
that would make the room among them, which the engine lands on a tenant
past its quota (`engine/limit.rs`) -- waited behind it for good, and the
app was never told. A 507 whose body names a `quota` is a refusal now
(`Sync::on_answer`, `#pump`): put back, `refused` and `onRefused` told
with the quota, the queue going on; one naming none -- `--max-memory`'s,
which a handover or an idle tenant's close brings back under, or a
proxy's -- is waited out as a 5xx. A retried answer's `retry_after_ms`,
the wait itself where the body says it, or its `retry_after` otherwise, is
waited where it is longer than the backoff (a scenario holds both runners
to a 2 ms wait waited as the backoff's 250 and a 900 ms one as itself; every
SDK's error reads the milliseconds first, Go's and .NET's as a duration), never cut to the backoff's 15
s and an hour at most (`RETRY_AFTER_MAX_MS`: a browser's `setTimeout`
fires at once past 2^31 ms), the 30% jitter on top, since a bucket tells
every client it refused in one moment the same wait. A stream a 429 of
`streams` refuses waits the same way, its shape not `connected`: the
status reads `offline`, as for the network, its error naming the quota
(`error.quota`, JS `status()[i].quota`) rather than a state of its own --
why a replica is not live is the error's to say, as a 401's is. `online`
forgets the backoff but keeps a server's wait (each timer flagged, the
third member of `Sync::timers`, `#wait`'s `server`) and lets the streams
open past a write held so. Both runners read the body, not the headers:
`Sync::response` is handed a body, a header would be a parameter more
through the C ABI and three bindings, and a page on another origin reads
`Retry-After` only where CORS lists it -- so every 503 says it in its body
too (below). Five scenarios hold both runners to it, and the Swift,
Kotlin, Dart and browser tests a real tenant node at its rows quota: the
insert refused naming `rows`, the keyed delete behind it landing.
`fenec.js` grew 99.4 -> 102.4 KB, 27.0 -> 27.9 KB brotli, 0.26 KB of
that code (minified, 12.2 -> 12.5 KB); the native library's code 2.3 KB,
its file 16 bytes on aarch64-apple-darwin; the browser module not by a
byte, the sync being outside it.

## Retry-After in bodies

**Every 503 says when to come back, in its body too.** A move's or a
failover's 503 carried `Retry-After: 1` as a header alone, and the sync,
which reads bodies, came back after its own 250 and 500 ms where the node
had said a second. `Response::retry_after` writes `{"error",
"retry_after"}` beside the header, as `quota::refusal` writes a 429's: a
write to a frozen tenant or one the lease does not let this node take
(`handle_admitted`, the schema's apply), a held claim on a tenant closed
here, a standby router without its directory, the ceilings of connections
(node and router) and subscriptions (JSON now, plain text before), a node
going down (`Refused::response`, the 503 of a tenant opened during the
shutdown), `/_changes` over a database going. Both runners already waited
a 5xx's `retry_after`; two scenarios hold them to a write's 503s of a move
and a lapsed lease, waited a second and not cut short by `online`, and a
stream's 503 from a standby router. Every SDK's error carried a body's
`retry_after` for any status; the Go and .NET tests freeze a tenant on a
real node and read it. A page on another origin reads the router's own
answers -- a standby's 503, a 404, a 502 -- once the router has
`--http-cors` (below).
