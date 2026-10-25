# The HTTP server

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The HTTP server*: the studio, MCP, idempotency keys, the audit log, request ids, traces, metrics, statement counts, the parse cache, JSON written from stored rows, and connections no thread is made for (added after the move). Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Studio

**fenec studio is a page with no authority of its own** (`studio/`,
`fenec_http::studio`, `--studio` on `fenec-server` and `fenec-shard`; off,
`/_studio/` is a 404 as an unknown path is). Plain ES modules and CSS with
no build step, importing `web/client.js` as any page would; the files are
embedded by `include_bytes!` under fenec-http's `studio` feature, which the
two binaries turn on and the shell does not: +210 KB of each release binary
on aarch64-apple-darwin, the files 193 KB of it, and the later views 116
KB more of `fenec-server` and 99 of `fenec-shard`, their files 106 KB.
They are served before any token is asked, with a strict CSP (`script-src 'self'`, no inline script,
`connect-src 'self'` and the one `--studio-connect` origin, which is
checked to be an origin since it is written into a header), `DENY` framing,
the page `no-store` and the rest revalidated by an `ETag` of their bytes;
the names are not hashed, since the modules import each other by name. The
token is pasted into the page, kept in its tab's `sessionStorage` and sent
as a client sends it, so the server's scope rewrite holds the grid, the
counts and the facets to a scoped token's rows. Every statement is written
in `studio/statements.js` -- the later views' in `statements-views.js`,
under the same rules: values as parameters, a name refused unless it is
one FenecQL writes, the typed `where` held inside parentheses it cannot
close; writes are `/batch`es under an `Idempotency-Key`, a cell's `set ...
where id = $2 require 1`, so a row deleted meanwhile is a 412. `GET
/_whoami` is what it shows a token by: `open`, `full` or `scoped` with the
rules that apply to it, the filter as the policy writes it
(`Scope::summary_into`). The grid reads 100 rows a block, the block after
one in id order by `id > $last`, a jump or an order by offset, and draws
only the rows in view: a scroll down 100 000 rows and back took no long task,
the longest frame 18.7 ms in headless Chrome. Its first load is 65 KB of JS
and CSS gzipped (`site/build.py` holds the docs to it), served uncompressed.
The query editor, the schema, the live rows and the admin view are modules
fetched the first time each opens (`app.js`'s `LOAD`, their stylesheet
`views.css`): 19, 19, 7 and 13 KB gzipped with what they share, none of
it on the first load (`statements.test.mjs` holds both). The editor runs
the text as typed -- cut at each `;` outside strings and comments, one
statement to `/query`, several one `/batch` -- and shows the status,
`Fenec-Seq`, `X-Request-Id` (exposed to CORS for it) and `explain` of a
`get`; it is coloured by `studio/highlight.js`, `site/highlight.js` with
build.py's rules written in (`make studio-highlight`;
`site/test_highlight.py` refuses a stale copy), whose mirror is text
nodes, never `innerHTML`. The schema view shows a collection as the
engine writes it (`GET /_schema?as=fenecql`) and every change as its
statement and `/_schema/plan` of the collection as it would be; a drop
runs once its name is typed, and a text box's `change` on blur no longer
asks the plan again, which held the apply button back. The live view is a
subscription of the typed shape, refused past 10 000 rows since its seed
holds them all; the admin view, for the server's token, reads
`/_stats/statements`, `/_metrics` and on a router `/_shard/tenants?bytes`
(each node's `/_admin/sizes`). The site's playground is the same studio
over a database in the page: every view reads through `state.db`, one
surface -- `run`, `batch` and `subscribe` as `FenecHttp` has them,
`request(path)` for an answer shown whole, `stats()` -- which
`connect.js`'s `Remote` answers over HTTP (its `request` the views'
`kit.js` code, fetched with them) and `local.js`'s `Local` from the
browser module in a dedicated worker (`worker.js`), as a server would
(`engine.js`): `/query`, `/batch` as one block over calls
(`Fenec.batch`, the module's `fenec_block`), `/_schema` and
`?as=fenecql` (`Fenec.describe`, `fenec_schema` modes 3 and 4),
`/_schema/plan` (the JSON description written as FenecQL, which alone
the module reads), every refusal with the status `api::status_of` gives
-- a parse's message kept whole, as a server keeps it -- and a
subscription as a `Fenec.live` of the shape whose differences are the
change. The token, the tenant, the admin view and the file's sizes are
left out rather than faked, "Running in your browser." where the token
was, with Keep it in this browser (`persist`, off by default) and Reset
data. `studio/test/transport.test.mjs` sends the same statements, batches,
schema requests and a subscription's writes to a debug fenec-server and to
the module and holds every answer equal; a server's change also names as
deleted a written row its shape never held, which a view passes over, and
the test drops those. `site/build.py` serves the studio's own files in
`dist/studio/`, minified and named by their hash with every import
rewritten, the page `studio/index.html` with `data-mode="local"`, which
`app.js` takes to wait for `site/playground.js` to call `local(db,
{examples, first})`, and `local.css`; the playground frames it between the
site's header and footer, since both stylesheets name `.top`, `.btn` and
`--sun`, and `_headers` lets the site alone frame `/studio/*`. A
server's first load names none of it (`statements.test.mjs`) and grew
1.1 KB gzipped for `Remote` and the branches; the module 3.4 KB, 1.0 KB
brotli, `describe` 1.5 KB of it. The playground's studio is 72 KB of JS
and CSS gzipped beside the module's 200 KB brotli, its first answer
150 ms after the page is asked for with the cache off on this machine and
1.6 s over a throttled 4G, Lighthouse 98, 100, 100 on a phone.
`make studio-test` runs it in Chrome (puppeteer-core, `studio/`'s one dev
dependency) against a debug server, a tenant node and a router; on macOS
Chrome 148's new headless mode never answered puppeteer's clicks, so the
harness prefers the headless shell, and it builds the site and drives the
playground as well (`local.test.mjs`). Before a process's first launch the
harness reads Chrome's own files through (`warm`): on a fresh GitHub runner
a cold launch faulted its binary in for 5 to 20 s of the 30 puppeteer waits
for its endpoint, and once past them every e2e test failed in its hook;
read first, 457 MB in 2.5 to 13 s, the launch took 0.2 to 1.6. The shop's
and Kestrel's Lighthouse steps read Chrome so too
(`examples/{shop,analytics}/scripts/warm-chrome.mjs`, run first by
`lighthouse.sh`): Lighthouse's chrome-launcher waits 25 s (50 polls of 500
ms) for Chrome's endpoint, and the shop's job once failed in its first
launch of Chrome, 25.4 s after its last test passed, `waiting for dynamic
debugging port in chrome-err.log` with Chrome still running. The log holds
no more, so GitHub's disk is the cause that fits, not one proved. In a
Debian container of three cores with its page cache dropped and its reads
of the disk held to 80 a second (a cgroup's `io.max`, which Chrome's page
faults of some 77 KB each feel and a sequential read of mebibytes hardly
does) that launch failed with that error after 27.8 s in two runs, took
16.0 s at 150 a second and 7.7 at 300 (1.0 unthrottled); reading Chromium
first took 4.7, 2.5 and 1.2 s and the launch after it 5.6, 3.0 and 1.5
(the libraries Chrome links from outside its directory are not read). The
router run's one timeout did not come back in twelve runs at a load of 27 (busy
loops on every core, three suites at once); a run filtered by name charges
its first test with the `before` hook's 100 000-row seed, about 10 s, which
is the hook's and not the router's.

An app embedding the studio -- the desktop app's window on the
`fenec-server` it started for a file (#381) -- hands it a session as
`window.fenecHost.session`, `{server, token, tenant}`, frozen and written
by a preload before the modules run. The preload defines it only where
`location.origin` is the origin the app started or connected to, and names
that origin as `server`: a preload runs in every document the window
loads, and a page of another origin -- a redirect's, a proxy's sign-in
page -- would read the token from it, which no check of the studio's can
stop. Keeping the token from other origins is the app's; the studio's own
check refuses only a session naming a server other than the page's. A
preload in an isolated world -- Electron's under its default context
isolation -- defines nothing the page sees: there it goes through
`contextBridge.exposeInMainWorld`, whose copy must pass `Object.isFrozen`.
`connect.js` reads it once as it loads, and only a plain object, since an
element with the id `fenecHost` is `window.fenecHost` too by named access,
its fields read once each, so a getter cannot answer one way to the check
and another to the use; takes the session only frozen and naming the
page's own origin, so the studio sends its token to no server but the one
that served the page; and keeps nothing under a host: `save` writes
nothing and a session in `sessionStorage` is not read. The spike's preload
had written `sessionStorage["fenec-studio"]` for `saved()` to find, a
private contract on a key that put the token in storage. `app.js` shows no
Sign out and no sign-in form under a host: a refused token or a session it
cannot take is said on a page of its own, since a token typed there would
be a connection the app never sees; the expiry's toasts and a 401
(`explain`) say to connect again from the app. The first load grew from
66.3 to 67.3 KB gzipped. `e2e.test.mjs` holds it with a page that defines
`fenecHost` before the document's scripts, as a preload does: the session
taken and taken again on a reload, its token in no storage, another
token's session in `sessionStorage` passed over, a refused token, and a
session for another origin or not frozen sending no request.

## MCP

**An agent's MCP tool is a statement, held to the token as `/query` is**
(`fenec_http::mcp`, `fenec-server --mcp`, `fenec mcp`). One `answer`
serves the stateless 2026-07-28 revision -- its version and the client's
capabilities in every request's `_meta`, `server/discover`, no session --
and the handshake revisions 2025-11-25, 2025-06-18 and 2025-03-26 (an
`initialize`), without a session either: the official TypeScript client
of October 2026 opens in the old era unless told. Streamable HTTP is
`POST /_mcp`, its underscore keeping it out of a collection's way as
`/_metrics`'s does (a tenant's `/t/<t>/_mcp`, which a router forwards as any
tenant path; GET and DELETE 405, a batch refused, an answer one JSON
object, never a stream), with the 2026-07-28 headers held to the body
(`-32020`); stdio is `fenec mcp --file` (`serve_lines`) and `--url`
(`bridge`, the token from `FENEC_TOKEN`, never argv). The tools are
`schema`, `search` (`match`, its `fuzzy` and `prefix` too, `near` or
`fuse`, a snippet in `**bold**`),
`query`, `get`; `write` with `--mcp-write` (put, insert, set and del:
never a schema change or a compact, by any token); `remember`, `recall`
and `forget` with `--mcp-memory`, whose collection `remember` makes at the
first memory (`memory_schema`: `expires timestamp @ttl(1ms)` for a
memory's own expiry, the vector field added at the first vector). A
filter a model writes is parsed alone, `get <c> where <raw>`, and refused
unless the `Select` is the bare one with a filter (its `PartialEq`), its
condition put into a statement built here, never its text; names are
checked against FenecQL's rule, values are parameters. A read past
`--mcp-max-rows` is asked for one row more and refused (413), one past
`--mcp-max-bytes` refused, never cut: a model told nothing of the cut
takes half an answer for the whole. Refusals are the tool's result with
`isError` and `/query`'s status, which a model corrects from; an unknown
tool or a malformed call is JSON-RPC's error. The origin is asked before
the token, and a server with no token answers only a `Host` naming
localhost or an address, which a rebound DNS name cannot. A served
session reads pinned and writes through `handle_query` (read-only mode,
quotas, the ceiling, the fsync); a local one (`Run::local`) runs under the
lock -- a function pointer, not a flag, since a flag kept `/query`'s path
in the shell: `fenec mcp` added 243 KB to `fenec` with it (`maintain_with`
alone 24 KB) and 130 KB without, `fenec-server` 97 KB, `fenec-shard` and
the browser module not a byte. `<file>.lock` (`fenec_abi::lock`, moved
from fenec-ffi) is taken by `fenec mcp --file`, `fenec-server --file` and
the native library alike, so any two refuse each other. Calls are held to
`--mcp-calls-per-sec` by a scoped token's subject or the caller's address,
counted in `fenec_mcp_tool_calls_total` once a process serves MCP, and a
write tool's call is an `mcp` line of the audit log. A row by id costs
3.8 us through the `query` tool against `/query`'s 1.2 in process, 21.9
against 17.7 us p50 over HTTP, a search 1-2% more (`make mcp-bench`);
a server without `--mcp` pays a comparison a request, and `make
requests-bench` in eight runs in turns against main moved inside its
own spread (a row by id 0.027 against 0.028 ms p50).
`integrations/mcp` holds it to `@modelcontextprotocol/client` in both
eras over both transports, with every configuration `mcp.html` gives
(`make mcp-test`).

## Idempotency keys

**An `Idempotency-Key` makes a write once** (`idempotent.rs`). A `put`
with no id makes a row each time, so a retry after a timeout wrote it
twice. A keyed REST write, `/query` or `/batch` runs as a block under the
write lock, and its answer is kept in the same block as a row of
`_idempotency` -- the key the subject's for a scoped token, and the
request's hash beside it -- so the write and its key land together, a
second request with the key waits for the lock and is handed the answer
(`Idempotent-Replayed`), the key with another request is 422, a failed
write keeps none, and a `compact`, which cannot be put back, takes none.
Kept for `--idempotency-ttl` (a day): `at` is `@ttl`, so a key past its
time is out of every read and the sweeper deletes it; the change stream
leaves them out.
A write without a key is rendered after the lock as before; with one, 16
200 single puts a second over HTTP against 18 300 -- with its statements
parsed each time and the collection made if missing every time, 14 600.

## Audit

**Refusals and schema changes are logged; a refusal waits**
(`fenec_http::audit`, `--audit`, `--auth-delay`). A JSON line an event: an
HTTP 401, a statement whose shape starts with
`create`/`drop`/`alter`/`compact` -- found where `statements::record` shapes
every statement, so no literal reaches the log -- and a
`/_admin/` or `/_shard/` request that changes something. Who is a
thread-local set as the connection starts, a connection being a thread. A
refusal waits 100 ms, doubled for each more from its address within a
minute, 5 s at most, and only a token verified clears the count
(`audit::verified`): called where a token is compared or a JWT's signature
checked and found good -- `authenticate`'s server token and JWT,
`admin::handle`, `metrics::allowed`, `Replication::authorized`, a tenant's
replication token in `route_tenant`, the router's `refused` -- and nowhere
else, asking the table only when an address has one (`FAILING`): the
hooks cost a request 5.4 ns in process, 5.7 with a token verified, where
reading the answer -- `Authorization` looked for among the headers -- took
5.8 and 8.6. It was inferred from the answer, any status
but 401 to a request carrying `Authorization`, and most answers verify
nothing: an unknown path's 404 on a node or the router, `/_studio/`, an
open `/_metrics` or `/_shard/`, `POST /_whoami`'s 405, a body past
`max_body` (read whole where an admin token raises the ceiling) refused
413, a `--metrics` listener's 404, `/_admin/` off, a preflight the router
forwarded, and every answer of a server that asks for no token but holds a
replication or an admin one (`Who::Full` there verifies nothing) -- a
client guessing tokens sent `GET /nosuch` between its guesses and never
waited past the first 100 ms. A good token answered 400, 403, 404, 409 or
412 was verified all the same, and clears. Behind the router the node
verifies and the router keeps the count, so `verified` under the router's
mark sets a thread-local instead, which the answer's head carries as
`Fenec-Verified: 1` (`audit::push_verified`, in `Response::write` and a
subscription's and a replication stream's heads), taken as it is written
and reset as each request is read, so the request before on a pooled
connection says nothing of the next; the router drops it from what it
sends on and clears its client's count on it alone. Against main in
three ABBA rounds (`make requests-bench` with and without a token, `make
shard-bench`'s latency, a node and the router asked with a data token and
a JWT; one side of twelve left out, the machine's load past 6 from other
builds), every median stayed inside the runs' spread. The count is an
address's, not a token's: a client holding any good token -- a JWT an app
hands every user among them -- clears its own between guesses at another,
as it always could. `tests/audit.rs` (fenec-http) and `tests/refusals.rs`
(fenec-shard) send a guessed token to each of those answers and hold the
count where it was, a good token to the others and see it cleared, and
fail with the inference put back.
Behind `fenec-shard` every request came from the router's address -- four
forged tokens made a fifth client's expired one wait 1.6 s, and any good
token cleared the attacker's count -- so the router waits out a refusal
itself, by its client's address (`forward` calls `audit::http`), and a node
waits none for a request bearing `Fenec-Router: <address> <mark>`, the mark
16 bytes of an HMAC of its admin token (`audit::router_mark`,
`Config::router_mark`), the address the log's `peer` (the router `via`):
the router drops the header from what a client sends, and a node believes
it from no one else. At the router the count is one across every node.
Behind the global entry the router's peer is Cloudflare's address, so with
`--entry-token` it counts, clears, logs and names onwards the address the
entry's `Fenec-Client` names (`audit::entry`, `NAMED`; under *One URL
reaches every region*). As two headers and the whole HMAC it added 2.5 us to the 22 a request
through the router took; as one, with the forwarded headers borrowed
rather than cloned and written without `format!` (`Pool::send`), 0.85 us
on average over four turns against main, inside its spread of 21.2-23.0. A browser's preflight is
answered 204 before any token is asked, where `--http-cors` lets its origin
in (`cors_allows`): authenticated first, it was 401 and a page on another
origin could send nothing.
Reads and writes are no events, but for an MCP write tool's call (`mcp`)
and a quota's refusal (`quota`), above. A test that opens the log is a
`[[test]]`
of its own: the log and the counts are the process's.

## Request ids

**Every request has an id, in its answer and in every line it wrote**
(`fenec_http::request_id`). Taken from `X-Request-Id` when it is printable
ASCII of at most 128 bytes, made otherwise -- 16 hex characters,
SplitMix64's finalizer over a key read from the system once a process, the
thread's number and its count of requests, so no system call a request --
and set as a thread-local where the request is read, as the audit log's
peer is: `Response::write`, a subscription's and a replication stream's
head send it, `audit::line` writes it as `request_id`, and `log!` ends a
line with `request_id=<id>` while one is set. The slow statements are JSON
lines on stderr through `audit::line` (`event` `slow`, `duration_ms`,
`kind`, `tenant`, `statement`), so one parser reads both logs, and every
line has a `level`; an admin event's status is `http_status`, since
Datadog takes a `status` for the level. `--slow-ms 0` logs every statement,
as PostgreSQL's `log_min_duration_statement = 0`. The router takes the
client's id or makes one, drops the client's header, and `Pool::send` sends
the thread's id with every request to a node, which takes it as its own;
the router's answer carries its own id in place of the node's. An id costs 15 ns
to make and 12 to keep a client's; in `make requests-bench`, eight
runs in ABBA turns a minute apart, one client's p50 stayed 0.016-0.022 ms
for a row by id either way, and eight clients' medians moved -12% to +6%
by case with no sign shared -- the runs' own spread, the machine busy with
other builds. `fenec_refused_total` (and the router's
`fenec_router_refused_total`) count the 401s the audit log names.
`monitoring/datadog/` holds the Agent's `conf.yaml` (OpenMetrics check,
`namespace: fenecdb`, `raw_metric_prefix: fenec_`), `pipeline.json` and
`dashboard.json`: `fenec-shard`'s `tests/datadog.rs` scrapes a primary, its
replica, a tenant node and a router in process and fails on a metric the
check keeps or the dashboard asks for that none of them sends, and
`fenec-server`'s `tests/logs.rs` on an attribute the pipeline or the
dashboard reads that no real line has.

## Traces

**A request is a trace, sent over OTLP** (`fenec_http::trace`,
`--otlp-endpoint`, `--otlp-header`, `--trace-sample`, the standard `OTEL_*`
variables). A W3C `traceparent` is continued -- a sampled parent followed
whatever the ratio (`parentbased`), `tracestate` passed on unchanged -- and
a trace with none starts at the ratio, its id's low 56 bits against it
(`TraceIdRatioBased`, so a router and its nodes keep the same traces). The
trace is a thread-local, as the request id is: `begin` where a request is
read past the health check, the scrape, the preflight and the studio's
files (most of the requests and none of the time), `end` once the answer is
written, `discard` before a subscription or a replication stream takes the
connection over (its spans would be held for as long as it lasts), a trace
64 spans at most. The spans are the waits: the server span (`http.route` a
template, `db.query.text` the statement's shape from `statements::record`,
never a value -- a REST target's query string holds words the shape keeps,
so it has its route alone -- `fenec.request_id`, `fenec.tenant`),
`lock.wait` in `held::read`/`write`, `execute`, `durability` in
`await_durable` holding the `Tee`'s `fsync` and `replication.wait_sent`,
`fenec.wait_for_write` for `Fenec-After`, and the router's `forward`, a
client span whose id `Pool::send` sends the node as its `traceparent` --
the client's dropped from the forwarded headers while the router traces,
passed through as they came while it does not. Every start and end is a
read of the wall clock (`now_unix`), as OpenTelemetry's Go, Rust and
Python SDKs take theirs: a request's children were timed by the monotonic
clock from an anchor of both, two reads that land past 100 us apart about
once in 45 000 and up to 83 ms beside load, so two requests of one process
-- a router's `forward` and the node's span under it -- were each off by
their own; a step of the clock shows inside a span, macOS reads it in
microseconds, and a read costs what the monotonic one did. A server span
ends once its answer is written, so a node's outlasts the router's
`forward` by its write: `fenec-shard`'s test held the whole span inside the
forward and failed 22 runs of 203 in three cores beside six busy loops,
the node ending 82 us to 1.3 ms after; it holds the start and every span
the node ended before writing, none of 300 failing. A finished trace goes
into a queue (`OTEL_BSP_MAX_QUEUE_SIZE`, 2 048 spans) under a mutex held for a
push, the exporter taking it whole in one swap; full, the spans are dropped
and counted (`fenec_trace_spans_dropped_total{reason}`), so a request never
waits on the collector, and a thread posts it every second or at 512 spans
as OTLP/HTTP JSON written with `json::escape_into` and a client of a page,
the connection kept alive, a post bounded by `OTEL_EXPORTER_OTLP_TIMEOUT`.
fenecdb speaks no TLS, so an `https://` endpoint and a `grpc` protocol are
refused at the start: the endpoint is a collector on loopback -- the
OpenTelemetry Collector, the Datadog Agent's OTLP receiver -- forwarding
over TLS. Off, each hook is one relaxed load: in `make requests-bench`,
six runs in turns against main, one client's p50 stayed 0.022, 0.034, 0.059
and 0.022 ms (main 0.022, 0.033, 0.059, 0.021) and eight clients' medians
inside the runs' spread; on at 100% to a sink on loopback 0.022, 0.034,
0.060 and 0.024, eight clients 2 to 9% fewer requests a second, the
exporter's JSON a core of the eight. `fenec-server` grew 49.7 KB,
`fenec-shard` 82.7 KB (aarch64-apple-darwin), and the browser module not by
a byte: nothing below `fenec-http` changed. `fenec-server`'s
`tests/tracing.rs` holds the spans an in-process receiver (`tests/otlp.rs`)
is sent to OTLP's JSON, their parents and attributes, a ratio of 0 to
nothing but a sampled parent's trace, and a collector that hangs or is down
to costing no request its time; `fenec-shard`'s, a `[[test]]` of its own
since tracing is the process's, a trace through the router into the node.

## Metrics

**`/_metrics` counts at the edge, a shard per thread.** A statement is timed
around `handle`, from arrival to answer, so
the lock wait and the `--sync always` fsync are in it; whether it wrote is a
thread-local set where the write lock is taken (`metrics::wrote`), since a
connection is a thread running one statement at a time. The counters are
sixteen 128-byte-aligned shards handed to threads in turn: eight threads
counting into one set cost 720 ns a statement, 6.7 ns with the shards. The
path has an underscore because a collection may be called `metrics`.
`--metrics <addr>` is a listener for it alone that never attaches a watcher
-- a second one would take the HTTP endpoint's subscription wake-ups -- and a
tenant node publishes counts of its tenants, never a tenant's collection
names. The router counts its own the same way (`fenec-shard/src/metrics.rs`,
over `fenec_http::metrics::Timings`): requests by route and status class, a
forwarded one's time at its node, the nodes it could not reach and its moves,
never a tenant in a label -- 13.3 ns a request with eight threads counting,
beside the 17 µs the router adds. A subscription is timed to its head, and a
standby's replication stream only counted: both last as long as their
client. The node and the router alike count a request before its answer is
written, so a client holding its answer finds it in the next scrape: the
router counted after the write, and `entry.test.js` read eu's router a
relayed request short and then one over (CI) -- 8 runs of 10 with the
router's thread held 5 to 10 ms past the write, none once it counts first
(`fenec_shard::metrics::counted`). The router times a request once its
answer is written, as before: count, clock and histogram all moved ahead of
the write took its added p50 up 0.1 us, the count alone nothing measurable.

## Statement counts

**`/_stats/statements` counts by shape, a tenant's apart.** Every
statement is also counted by its text with each literal and parameter as
`$n` and a list of literals alone as one (`fenec_http::statements::shape`)
-- over HTTP by the FenecQL of `/query` and `/batch`, noted as
`api::parse_query` reads it (`statements::text`), since the strings of the
JSON would all have been one shape. The counts are shards a thread each
behind a mutex only a reader of the whole also takes, 5 000 shapes at most,
the least called forgotten: 0.34 us for a statement of 87 bytes, against the
2.2 us its parse takes (`make statements-bench`). A shape is made only as
far as the 1 000 bytes an entry keeps of it, which is as far as two can be
told apart where they are shown: shaped whole, a `put` of 1 000 128-dim
rows spent 1.7 ms of its 12.6 there. They need what reads the data without a JWT's scope, or the admin
token -- a shape names every collection -- and a tenant node keeps each
tenant's apart (`statements::View`): its own under `/t/<tenant>/`, every
tenant's, named, to the admin alone.

## Parse cache

**A statement is parsed once, and an answer goes out in one write.**
`POST /query` keeps what `api::parse_query` parsed by the text's hash, 16
shards of at most 64, a shard emptied when full and a text over 1 KB,
which holds its literals, never kept; a JWT's scope is ANDed into a copy
(`Arc::unwrap_or_clone`), never into the shared one. An HTTP answer goes
out in one `writev`, its head written without `format!`: head and body
apart put the head in a packet of its own on a socket that sends at once,
and cost a second send each answer. One client asking a row by id over
10 000 x 128 went 40.4k -> 47.1k requests a second, and eight 14 to 20%
more (`make requests-bench`). The router writes the answer it forwards
the same way (`answer_out`, `http::write_parts`): its added p50 went 23.6
-> 20.6-20.8 us (`make shard-bench ARGS=latency`). FenecQL's lexer walks a text's bytes,
reading a character whole only outside ASCII and a number where it stands,
and a list of numbers alone becomes its vector without an expression each
(`numbers_in_brackets`): a query holding a 128-dim vector parsed in 16.9 us
and takes 6.2, a row by id 1.1 and takes 0.9. Natively the lexer reads a
list of numbers alone at once, into the vector the parser would make of it
(`Tok::Vector`, `tokenize_vectors`), each number in the one pass that finds
its end (`num::clinger`, the JSON reader's), and not after `in`, whose list
keeps its integers and `f64`s: a token a number and a comma, and each
number's text read for its end, for a `_` and for its value, a `put` of
1 000 128-dim rows parsed in 5.4 ms, now 2.4, and
`a_list_read_as_a_vector_parses_as_its_tokens_did` holds 20 000 generated
texts to the token-at-a-time parse. The browser module reads such a list
into a vector too, at once, each number the general way, for its memory
rather than the time: read a token a number, a `put` of 1 000 768-dim rows
as text took it from 58 to 130 MB, which it never gives back. The text is sliced with
`get`: an index that can panic brought 2.7 KB brotli of a `char`'s
formatting back into the browser module, which is 1 KB smaller instead.
`the_byte_walk_reads_as_the_char_walk_did` holds the tokens, positions and
errors to the old lexer's over 40 000 generated texts.

## JSON from stored rows

**A plain `get` is written as JSON from the stored documents**
(`Database::query_json`). Fields alone, in the order the payload holds
them -- `select *` and most lists, with no `match`, `near`, `lookup`,
`facet`, aggregate or `count` -- are answered over HTTP by writing each
row's bytes out as JSON under the read lock, a text escaped where it lies
(`codec::text_at`), rather than decoded into a `Value` each, rendered after
the lock and dropped; anything else returns `None` and goes through `query`.
It must write byte for byte what `json::rows_array_into` writes of
`query`'s rows (`tests/query_json.rs`: every type, a field added after a
document, a dropped one, a mapped file, rows past their time). In the
container, musl's allocator gives a freed group of blocks back to the
system and faults it in again at the next request: a YCSB scan of 50 1 KB
records took 37 page faults and 213-220 us inside fenec-server, a read by
id a page mapped and unmapped while its columns were `Vec`s -- so the
columns sit in an array on the stack, and an answer's body is the
connection's spare buffer (`http::spare_body`, given back after the
`writev`, up to 1 MB): no fault, 50-60 us, and a read by id 22.4 -> 17.7
(`make roundtrip-bench`, `--measure scan50`). In process a 1 KB row took
0.95 us to decode, a field a look-up and a skip of the fields before it,
and 1.17 to write out; `select` reads a row in one pass now (`one_pass`,
0.43), `json::escape_into` tests eight bytes at a time (`needs_escape`,
0.39), and `query_json` takes 0.46 for both. The browser module's scan of
100 such rows went 0.355 -> 0.254 ms for 353 bytes brotli; `make
load-bench`'s pages read back 165.8k -> 212.0k rows a second.

## Connections without a thread

**A connection no thread can be made for is answered 503, as one past the
ceiling is** (`fenec_http::accept`). Every connection is a thread, and past
the system's thread limit -- macOS's 2 048 a process, a cgroup's
`pids.max`, systemd's `TasksMax=`, `ulimit -u` -- `thread::Builder::spawn`
fails and drops the closure it could not run, and the stream inside it
closed with nothing said. Its client had sent its request and read
`Connection reset by peer`; through the router a write fared worse:
written whole, its connection ended before a byte of an answer, which is
what a node that ran it and went down looks like, so `replayable` had it
answered 502, "did not answer", where the node had run nothing (#372 found
it, fenec-shard's test binary at 2 047 threads). Both accept loops, the
node's -- the HTTP endpoint, `--metrics`, a `--dir` node, and with them MCP,
replication and the node's archive streams, which are its paths -- and the
router's, now spawn through `accept::spawn`: the stream goes into the
closure in a `Handed`, whose drop, where the closure never ran, puts it in
a thread-local of the thread that called `spawn` -- which is where std
drops a closure it made no thread for -- for `spawn` to hand back. Safe
where a raw descriptor taken out and made a stream again would not be, and
nothing more on the way: no `try_clone` (a `dup` a connection) and no
allocation. `accept::refuse` answers it on the accepting thread as the
ceiling's refusal is answered, which goes the same way now: 503,
`Retry-After: 1`, `{"error", "retry_after"}`, the CORS headers and
`Fenec-Unserved: no_thread` (or `ceiling`), the router's mark
(`Fenec-Shard: 1`) on its own. A pooled connection has its thread already.

The router passes a node's 503 on as the node's answer, as it passes every
503 that is not a cut's (`forward`) -- once it reads it: it wrote a
request's head and body in two `write`s (`exchange`), and a node that had
answered the connection and closed it before the head arrived had Linux
take the reset its closed socket sent the head back inside the head's own
`write`, the body's then failing EPIPE, unsent, the 503 unread in the
buffer and the write told 502 (CI; in Docker's Linux, three cores beside
six busy loops, 809 and 853 puts of 20 000 through a router, 11 to 37% of
`Pool::send`s, a send in 30 000 idle). Head and body go in one `writev`
now (`http::write_parts`), and a write that fails on a connection made for
the request reads what answer has arrived (`answer_waiting`, a peek, then
the head as any): an answer in before the request went out whole is the
node's to a request it never read whole, so it ran nothing -- a body past
what one `writev` takes meets the reset after its first bytes. Only a
status of 400 or more: a fenec node answers a request it has not read
whole only to refuse it (400, 411, 413, 431, the 503 unserved), never a 2xx
or a 401, reading the whole body before it asks the token or runs a byte,
so a 200 there is a peer claiming what it cannot have done and the request
stays unsent. A failover's cut that meets the head there is a cut of a
request unsent (`unsent_cut`), its last byte never gone out: counted sent
whole, an unkeyed write was told 502 that could go to the new primary. Not
on a pooled connection, whose waiting byte may be no answer of this
request's and whose failure goes on to a fresh one. A write that fails
leaves the request's last byte unsent, though not that its call took none
of its bytes: Linux takes none, and macOS's `dofilewrite` answers EPIPE
with -1 where part of the call went out. Either holds the small write
alone: 0 of 30 000 sends failed under that load with one, the other or
both, and 0 of 40 000 puts through a router with both. A router drops a
node's `Fenec-Unserved` from what it forwards: the router's own
connection was served, and a client of it, the global entry among them,
reads the mark as the router's. A router relaying to a region's routers
(`relay_send`, `call_routers_by`) and the global entry (`judged`'s
`unserved`) send a request a router answered marked on to the next
router, whatever the request -- it never ran there -- that router not
noted as the one that answered last, its 503 the answer only where every
router gives one; taken for any router's answer, a router at its thread
limit kept every request, where a reset of its connection had sent a read
on to the standby beside it. A standby's 409 refusing a `/_shard/` change
is passed over the same way, marked `Fenec-Follows`
([regions.md](regions.md#the-global-entry)): asked after a busy primary
and taken for the answer, it held every later change of the entry's
isolate at the standby.

The socket is made non-blocking before the write: the accepting thread is
every connection's, and a client that reads nothing must not hold it. The
answer, a few hundred bytes into a new socket's empty send buffer, goes out
whole in one write all the same, and where it would not it is let go of. The
request is not waited for -- the answer does not hang on it -- and the
socket is not closed with the answer either (below). Each is counted -- `fenec_connections_refused_total`
and the router's `fenec_router_connections_refused_total`, by `reason`,
`ceiling` or `no_thread`, the ceiling's counted nowhere before, neither in
`fenec_router_requests_total` -- and one with no thread is said in the log
a minute apart at most by the monotonic clock (a wall clock set back held
it silent), naming the limits to raise, its peer looked up only then and
the line carrying the answer's request id; one whose stream a std would
not give back -- none does -- is counted and said as well, closed
unanswered, and a debug build's assertion. `FenecNoThreadForConnection`
pages on any, since the process's other threads, a router's lease grants
among them, meet the same limit; a scrape is a connection too, so `up`
falls to 0 beside it.

A refusal was closed with its answer, what had arrived read off first, up
to 64 KiB, so the close was no reset for a request already in. A request
still on its way met a socket that was gone, and one arriving between the
read and the close was unread at it: each system answered both with a
reset (Linux's `TCPAbortOnData` and `TCPAbortOnClose`). A client writing
its request in one `write` read the answer before it all the same, both
systems handing a socket's received bytes over before its error -- but not
always on macOS, where a client writing 200 us after its connect read the
reset before the answer 58 times in 116 247 refusals. A client writing its
head and body apart -- Python's `http.client`, and with it `requests` and
every library on `urllib3` -- had the reset come back between the two, the
body's `write` failed `EPIPE`, and it reported the connection broken
without reading the 503 waiting for it. A review counted 3 343 resets and
130 503s in some 36 000 requests from 300 clients against a `--dir` node
at `--max-connections 100`. Reproduced with 300 `http.client` threads for
10 s, a `get` each, against such a node: on Linux (Docker, three cores)
16 376 refusals were read as 1 356 503s, 14 999 broken pipes and 21
resets, beside 12 400 `TCPAbortOnData` and 30 `TCPAbortOnClose` (another
run: 26 016 as 2 968, 23 026 and 22); on macOS 40 194 as 3 732, 36 361
and 101; through a router at its own ceiling of 100, `put`s, 19 467 as
918, 18 529 and 20. A Rust client writing head and body 200 us apart,
reconnecting 5 ms after each 503, had 498 bodies fail `EPIPE` in 125 911
refusals on macOS and 692 (`EPIPE`, `ECONNRESET`) in 110 796 on Linux. A
router in front of the node at its ceiling read every refusal the node
sent it, before and now: its one `writev`, and the answer read after a
failed write, already held it -- no 502 in runs of 1 107 to 69 677
refused requests, on macOS and on Linux beside six busy loops, but where
macOS ran out of ports (`EADDRNOTAVAIL` dialling the node), a 502 the
router answers for a request it never sent. Run such loads inside a Linux
container, its own network namespace and ports: on the host's loopback
the refused connections' 16 000 sockets in `TIME_WAIT` held macOS's whole
ephemeral range (49152-65535, `TIME_WAIT` 30 s), and every other
connection the machine made failed with them.

Now `refuse` shuts the socket's writing after the answer -- the client
reads the answer and its end -- and hands the stream to the process's one
lingering thread (`accept::linger`, `hold`), which reads off what arrives
until the client ends or resets the connection, then closes it: nothing
unread, the client gone, no reset is sent. A client that does neither is
held 2 s (`LINGER`), and read off up to 1 MiB (`LINGER_DRAIN`), past which
a body still coming meets the reset, after the answer, as a body past what
one `writev` takes met it for the router. std has no `poll`, so the thread
reads each held socket again after a pause doubling from 1 ms to 64 ms
(`FIRST_LOOK`, `LONGEST_LOOK`) -- a client that reads its answer ends the
connection within a look or two -- and waits on a condition variable while
it holds none. It is made as an accept loop starts (`accept::ready`), so a
process at its thread limit has it for the refusals it can make no thread
for; where it could not be made, a refusal is closed as before. At most
512 are handed over or held (`LINGERING`, `HOLDING`), a descriptor each,
so clients that never end their connections cost 512 descriptors for 2 s,
not the process's limit; past that a refusal is closed at once, as before.
In the same runs: on Linux 13 406 refusals read as 13 406 503s and nothing
else, `TCPAbortOnData` and `TCPAbortOnClose` 0 (another run: 22 424 of
22 424); on macOS 32 063 of 32 063; through the router at its ceiling
15 789 of 15 789; the Rust client's split writes failed none of 117 073
(macOS) and of 53 821 (Linux, beside six busy loops). The held connections
peaked at 199 to 206 under `http.client` clients, whose threads end a
connection slowly; the Rust client's 300 threads beside six busy loops on
three cores held up to 417, and in one run passed 512 for 136 refusals of
59 627, the throttled cgroup running the lingering thread rarely -- those
closed as before, no client of them reading a reset.

The listen queue is the other place a refused connection was lost. std's
`TcpListener::bind` asks 128, and the Rust client's reconnecting threads
overflowed it on Linux 451 to 961 times an 8 s run (`ListenOverflows`):
Linux drops the handshake's last segment and the client's request with it,
and the client sends both again after a retransmission's wait. Not a
reset -- Linux sends one there only under `tcp_abort_on_overflow` -- but a
wait before the 503. Both accept loops listen again with a queue of 1 024
(`accept::widen`), which each system holds to its own ceiling:
`net.core.somaxconn`, 4 096 on Linux since 5.4, and `kern.ipc.somaxconn`,
128 on macOS, where nothing changes unless it is raised. The same runs
overflowed it none. A flood past the queue is the kernel's to answer
(SYN cookies). What remains a reset: past 512 held, a refusal with no
lingering thread, a body past 1 MiB or a client still writing after 2 s.

A test makes the next spawns fail (`Server::fail_spawns`,
`Router::fail_spawns`, `#[doc(hidden)]`) by asking for a stack of 2 EiB,
which no address space maps: macOS, glibc and musl each refused 2^50 to
2^62 with EAGAIN, the error at the thread limit, where 2^46 was mapped;
reaching the real limit would fail every other test's threads in the
process. A connection pays one relaxed load for it. fenec-http's
`a_connection_with_no_thread_is_answered_503_and_told_when_to_come_back`,
fenec-shard's `no_thread.rs` -- a write through the router answered the
node's 503 and not run, where it was a 502; the router's own; a relay
asking the region's next router -- `upstream`'s
`a_node_refusing_a_connection_as_it_takes_it_is_answered_by_its_refusal`
(2 000 sends, failing on Linux with both of the router's fixes taken out)
and `an_answer_in_before_the_body_went_out_whole_is_the_answer` (64 MiB,
failing without the read after the write), `entry.test.js`'s router that
refused the connection passed over, and `accept`'s unit tests -- the
stream given back; an end, not a reset, after the answer -- each fail with
its part taken out. So do `accept`'s
`a_request_after_the_refusal_is_read_off_not_reset`, fenec-http's
`a_request_written_after_the_ceilings_refusal_is_read_off_not_reset` and
fenec-shard's `a_request_written_after_the_routers_refusal_is_read_off_not_reset`:
three refused connections each, read to the answer's end -- where the
refusal closed the socket, it had closed it by then -- then written a
head and a 512 KiB body apart and ended, each over when it has no peer,
its end acknowledged or reset, and then asked for the error a reset
leaves (`take_error`). The body is larger than a send buffer, macOS's
128 KiB and Linux's first 16 KiB, so written to a socket that is gone its
`write` waits for room that never comes and the reset ends it: closed
with the answer, the first connection failed every run on both systems,
`EPIPE` or `ECONNRESET`. A body that fit failed on Linux each time and on
macOS 136 to 350 times in 1 000 connections, the reset taken in as it
came -- a thousand connections a test, which no test should open on a
host. A test waits for no time: the connection's end comes either way.
The counts by reason, a delta of one each and none of
the other, are `refused_counts.rs`'s in fenec-http and fenec-shard, each a
`[[test]]` of its own, the count being the process's.

In ABBA turns against main at loads of 1.95-2.97, the machine moving
between two modes run by run on either build, the accept loop alone
changed: `make requests-bench`, three quartets, every figure inside the
runs' spread, no request's path having changed -- a row by id 0.022 ms p50
against 0.0235 (0.022-0.025 and 0.020-0.025), eight clients 146.1k a
second against 136.2k (133.7-147.3k and 133.3-148.7k), a put 0.023 ms
either way and 128.3k against 123.1k (118.2-130.9k and 119.3-128.5k).
Both benches keep their connection alive, so they meet the accept loop
once: a connection a request -- connect, `GET /`, read to the close, 5 000
a turn, three ABBA quartets from one process -- took 75.55 us p50 against
75.95 (74.7-77.1 and 73.2-76.4). With the router's one `writev` and the
mark, `make shard-bench ARGS=latency`, six quartets at loads of 2.5-2.97:
a request through the router 42.2 us p50 against 42.1 (42.0-42.6 and
39.5-42.4; the quartets' differences 0 to +0.3 us, one +1.55 over main's
39.5), the node asked directly 20.1 against 20.6 (19.8-21.6 and
20.2-21.6), so the added p50 read 22.05 against 21.1 -- the node's move,
whose path did not change, not the router's; a `GET`'s head went in one
`write` before as well, its empty body writing nothing. `fenec-server`'s
`__text` grew 4 032 bytes and `fenec-shard`'s 6 720 (aarch64-apple-darwin):
`refuse`, `no_thread`, its lines and the family, the relays' and the
early answer's code. Nothing below `fenec-http` changed, and the browser
module is the same to the byte.

The lingering thread and the wider queue touch no served connection: a
loop's start makes the thread and listens again, and a refusal shuts the
socket's writing and hands it over where it read and closed. In ABBA
turns against main under the ceiling, 5 s each, three quartets in Docker's
Linux on three cores: 32 clients kept alive, `get`s, 230.3-246.4k a second
against 196.1-243.4k, p50 27-29 us against 20-29; a connection a request,
eight clients, 15 868-16 084 a second against 14 635-16 233 in the first
two quartets (the third slowed both, 8 614 and 13 890 against 9 212 and
15 735), p50 296-300 us against 293-303. On macOS, 32 clients kept alive at
loads of 6.6 to 16: 124.1-165.2k against 131.6-158.7k, p50 115-201 us
against 132-225. `fenec-server`'s `__text` grew 5 376 bytes and
`fenec-shard`'s 5 312 (aarch64-apple-darwin): `linger`, `hold`, `ready`
and `widen`.
