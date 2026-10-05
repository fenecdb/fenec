# Kestrel: product analytics on fenecdb

A real-world example of event analytics on fenecdb. Kestrel counts the
visits to a customer's sites and shows them on a dashboard. It has:

- a tracker of 572 bytes (brotli) that a site puts on its pages, batching
  events into beacons and sending a failed one again;
- an ingest endpoint that checks every beacon, limits each site's rate, and
  writes a beacon once however often it is sent;
- raw events kept 30 days in an append-only collection, and rollups by
  minute, day, page, referrer, country, device, browser, visitor and week
  kept for good, folded from the change stream exactly once;
- a dashboard: visitors over time by `bucket` for the range chosen, top
  pages and referrers with `count(distinct user)`, a three-step funnel,
  weekly retention cohorts, a live "now", and filters with disjunctive
  facets, reading the raw events for a day or less and the rollups past it;
- a site a tenant on a `fenec-server --dir` node, with tokens bound to it:
  a site's dashboard token reads its own site only, and the ingest token
  can only insert;
- a market data view: ticks for 50 invented symbols from a seeded random
  walk, one-minute bars and VWAP by `first`/`last` and `bucket`, the latest
  price a symbol, a window over the last minutes, live by polling.

Its claims are held by tests -- the rollups against the raw events after a
load with duplicates, retries and a crashed worker; the funnel, retention,
bars and VWAP against brute force; every route of another site under a
site's token; Lighthouse's budgets -- and its numbers are measured.

The stack:

- TypeScript on Node, with a plain `node:http` server rendering HTML and
  SVG as strings;
- `fenec-server` as the only database, a tenant node with one file per site;
- `@fenecdb/web/client` (the HTTP client, no WebAssembly) for every query,
  `db.batch` with an `Idempotency-Key` for every beacon, and
  `FenecHttp.live` with `{ poll }` for the live parts.

**Why no framework.** A dashboard's first paint is its numbers. Rendered on
the server as HTML and SVG, a page paints whole with no script at all;
the one script (1.8 KB brotli) only keeps the live parts live and applies
a filter as it is ticked. An empty Next.js App Router page ships 132 KB of
compressed JavaScript before any of its own (the shop example measured
it), on a phone over slow 4G that is the difference between the LCP budget
and over it. A framework would add a runtime and a build step to every
page and nothing a dashboard of tables and charts needs.

**No chart library.** The charts are columns with a step line, a
sparkline, candles with a line over them, and a table of shaded cells:
`src/charts.ts`, 200 lines, made on the server for the first paint and in
the browser for live updates (the same file, bundled). A general chart
library, even a small one, weighs several times the page's whole script
(1.8 KB brotli) and would still need its own layout for phones.

Every site, visitor, symbol and number is invented.

## Architecture

```
 a customer's site                Kestrel (src/server.ts)                          fenec-server --dir (tenant node)
 ─────────────────                ───────────────────────                          ────────────────────────────────
 <script src="/k.js"  ── beacon ─▶ POST /e: parse and check, the site's     ─────▶ /t/fieldnotes/  events (@ttl 30d,
  data-site="key">     (text/plain, origin and rate limit, one block a beacon       append-only), the rollups,
 572 B brotli          no preflight) under the site's ingest JWT,                    rollup_state, pulse
                                     Idempotency-Key <site>:<batch>
                                                                                   /t/tidepool/    another site's file
 the dashboard  ◀──── HTML + SVG ─── GET /s/<site>: questions side by side
 (sign-in cookie)                    under the site's viewer JWT; raw events       /t/markets/     ticks (@ttl 1d,
   polls /s/<site>/now.json ───────▶ for a day or less, rollups past it            append-only), quotes
   (ETag, 304)                       one FenecHttp.live({poll}) a site, shared
                                                                                   /t/kestrel/     sites, users
 the market page ◀─── HTML + SVG ─── GET /markets: bars and the window, one
   polls quotes.json, bars.json      statement each; one live poll of quotes       policy.txt: a JWT's grants;
                                                                                   a token reaches only the
 rollup workers, one a site (in the server, or scripts/rollup.ts):                 tenant its claim names
   GET /t/<site>/_changes ──▶ fold a page ──▶ one /batch: the counts, and
   the worker's place moved under `require 1`  (the node's own token)
 market feed: ticks and quotes, one /batch every 500 ms (the feed's JWT)
```

| Who | Credential | May |
| --- | --- | --- |
| A site's dashboard | a JWT `{sub, tenant: <site>, role: viewer}`, minted by Kestrel per signed-in person and site | read the site's events and rollups. Write nothing. Another site is 403 on every route |
| The ingest endpoint | a JWT `{tenant: <site>, role: ingest}` | insert events. Not read them, not update or delete them, not touch a rollup |
| The market feed | a JWT `{tenant: markets, role: feed}` | insert ticks, keep the quotes |
| Kestrel's server | a JWT `{tenant: kestrel, role: app}` | read the sites and who signs in |
| The node's operator | `--http-token`, `--admin-token`, `--replication-token` | everything: tenants, the schema, the change stream (the rollup workers), corrections and erasure. Never in a browser |

## Run it

```sh
cargo build --release -p fenec-server   # from the repository's root
cd examples/analytics
npm install
npm run db        # fenec-server --dir data/tenants on :8080, policy.txt, the change stream kept
npm run setup     # the tenants, two sites with 90 days of history, two hours of ticks
npm run build     # the browser's script and the tracker, the font
npm start         # http://127.0.0.1:3000, with the rollup workers, the market feed and demo traffic
```

Sign in as `nadia` (Fieldnotes), `omar` (Tidepool) or `ops` (both); the
password is `kestrel-demo`. `/markets` needs no sign-in. A real site would
add:

```html
<script defer src="https://kestrel.example/k.js" data-site="fn4k7q2m9x"></script>
<script>/* later */ kestrel('signup_started', { plan: 'pro' })</script>
```

| Variable | Default | For |
| --- | --- | --- |
| `FENEC_URL` | `http://127.0.0.1:8080` | the tenant node |
| `FENEC_JWT_SECRET` | a dev value | signs the JWTs the node checks |
| `FENEC_TOKEN`, `FENEC_ADMIN_TOKEN` | dev values | the node's own tokens: setup and the rollup workers |
| `KESTREL_SESSION_SECRET` | a dev value | signs the dashboard's cookie |
| `KESTREL_COUNTRY_HEADER` | none | the header a CDN in front sets with the visitor's country (`cf-ipcountry`); without it, the region of the browser's language |
| `KESTREL_ROLLUPS`, `KESTREL_FEED`, `KESTREL_DEMO_TRAFFIC` | on | `0` runs the server without the workers, the market feed or the demo's beacons |
| `KESTREL_SEED_DAYS` | `90` | days of history `npm run setup` writes |

## Tests and measurements

```sh
npm run ci              # a node over a new directory, history, lint, build, the server, every test
npm test                # the tests alone, against a running node and server
npm run lighthouse      # Lighthouse CI's budgets against a running server
npm run bench:ingest    # events a second at 1, 4 and 16 clients, rollups on ingest, the worker's lag
npm run bench:queries   # the dashboard at a million and ten million events (BENCH_SIZES)
```

From the repository's root, `examples/run-tests.sh analytics` runs `npm run
ci` on this checkout's packages and binary. CI's `examples (analytics)` job
runs it against a release `fenec-server`, with Lighthouse.

| File | What it holds |
| --- | --- |
| `test/correctness.test.ts` | buckets to the millisecond against the server's; every beacon counted once through retries, a beacon sent twice at once, one sent again with a later clock, and its events written again under another key; every rollup equal to the raw events after a worker that crashed after its block landed, two workers racing, and a rebuild, and after late events move 25 visitors to an earlier cohort; the funnel (raw and rollups, filtered and not), retention and the totals against brute force over four weeks; bars, VWAP, the window and the kept quotes against brute force over ticks written out of order |
| `test/security.test.ts` | a site's viewer and ingest tokens on 22 routes of another site; tokens with no tenant, another secret, no `exp`, an expiry past, `alg: none`; the ingest token's one grant; the viewer's none; 22 kinds of malformed beacon, an oversized one with and without a length, another origin, a crawler; the rate limit under a burst; sessions, forged cookies, cross-site sign-in, the CSP |
| `test/seo.test.ts` | the public pages indexable, titled and described; the dashboards not; the tracker's size and caching |
| `test/perf.test.ts` | every range of the dashboard within 400 ms at the median, an event in the rollups within 3 s of its beacon |

## Decisions

### Rollups from the change stream, not on ingest

Kept on ingest, the rollups make every beacon a block of a dozen statements
under the one write lock: the spike that led to this example measured
ingest at 29 000 events a second that way against 333 000 for the raw
events alone (one client, 10 000-event requests). Measured here with
beacons of 20 events (`npm run bench:ingest`), the rollups written in each
beacon's block take ingest from 168 000 to 65 000 events a second at 16
clients, and from 81 000 to 29 000 at one.

So ingest writes one statement a beacon -- `put events [...] if absent` --
and a worker a site folds the events from `/_changes` into the rollups, a
page of up to 2 000 at a time, as one `/batch`. A page's rows of a
rollup are written as two statements a collection: the worker first reads
which of the page's keys have rows already (`existing`), then writes the
new rows with their counts and the old ones over by id with theirs added.
Written a key at a time, a `set {n: n + k}` each, a page of a backfill was
a block of a thousand statements.

**Exactly once.** The stream hands every write over at least once. The
worker keeps its place in the database, in `rollup_state`, and moves it in
the block that writes the counts:

```
set rollup_state {seq: $next, events: events + $n, ...} where name = "rollups" and seq = $prev require 1
put minutes [{key: ..., n: ...}, ...]            -- the page's new rows
put minutes [{id: ..., key: ..., n: ...}, ...]   -- the old ones, their counts added
...
```

A page applied twice finds `seq` moved and the whole block is refused
(412, `at` 0): no event counts twice, and none is skipped, since the place
moves only with the counts of every event before it. The same guard makes
the read of the existing rows safe: every rollup write is such a block, so
one landing between the read and the write moved the place, and the stale
block is refused and read again. Each event is a write the change counter
numbers once, so the change number is the event's key here; the event's
own id (`eid`, `@unique`) is what kept a beacon sent twice from being
written twice before the stream saw it. The tests crash a worker after its
block landed, race two workers, and rebuild, and compare every rollup with
the raw events each time.

A worker that falls further behind than the node keeps (`410`) rebuilds
the rollups from the raw events from the week before (`rebuild`), in one
read block and one guarded write block.

### Each beacon counted once

The tracker gives each beacon a random batch id and each event its place
in it; a beacon sent again is the same body. The endpoint writes it with
`Idempotency-Key: <site>:<batch>`: a retry is answered as the first try
was and writes nothing. A beacon whose first try landed but whose answer
was lost, sent again a moment later, carries the same events but its
event times are worked out against a later server clock, so the request
differs and the node answers 422 for the key: that is a duplicate, and the
endpoint answers 204. Behind both, `eid` is `@unique` and the events are
written `if absent`, so a batch written again under any key adds nothing.

### Raw events up to a day, rollups past it

The raw events hold every field: any filter, distinct visitors per page,
the funnel's exact times. They are kept 30 days (`@ttl(30d)`, an ordered
index whose rows leave every read at once, swept a minute later), so 90
days could not come from them at all. The rollups are small and kept for
good, but answer only what they were made for.

So: up to 24 hours, and whenever a filter is set, the dashboard reads the
raw events (a filter reaches back 30 days at most); past 24 hours,
unfiltered, the rollups. The switch is at a day because a day of raw
events answers in 23 ms at a million events and 272 at ten million, and a
week takes 0.3 and 4.6 s (below, "the raw path forced"). The page says which it
read, with each question's time.

What the rollups give up: visitors per page and per referrer (a row per
visitor, page and day would be most of the raw events again), and facets
that count as if the other filters were set.

### Funnel and retention

A visitor counts at a step when they took it no earlier than the first
time they took the step before; a visit is the first step, so its count
is the range's visitors. From the raw events the steps' first times are
one statement (`group user, name` with `min(at)`); past a day they come
from `firsts`, a row for each visitor, day and event that is not a
pageview -- a few percent of the events -- and the code walks them.

Retention is one statement over `cohorts`, a counter for each cohort (the
week a visitor first came) and week: a cohort's size is its own week's
count. The worker keeps it as it makes `weekly` rows, a row for each
visitor and week they came: it reads which of the page's week rows exist
and the page's visitors' first days before its block (`seen`), adds one
for each new week row at its visitor's cohort, and for a visitor whose
first day a late event moves into an earlier week, moves each of their
week rows from the old cohort to the new -- a test sends 25 visitors an
event from three weeks back and holds every count to the raw events.
Two ways that came first did not hold up: a cohort's visitors as an inner
`get` (`user in (get visitors ...)`) passed the inner `get`'s 100 000
values at a busy site's cohort, and `group cohort, week` over `weekly`
read every visitor's weeks, 560 to 700 ms of each dashboard at ten
million events.

### Visitors over a range, without a distinct count

`count(distinct user)` holds every value in memory and refuses past a
million rather than count short. A month of the busiest site measured
here has 2.4 million visitors, so the rollups do not count distinct values
for the dashboard's long ranges: `visitors` keeps each visitor's first
and last day (`@sorted`), every range ends now, and the visitors of a
range are those last seen in it -- `get visitors select count(*) where
last >= $1`, a range of an ordered index. From the raw events, a range
or a filter wide enough to pass a million is told so on the page, with
the event counts it could give.

### Live by polling, twice

The server holds one `FenecHttp.live(text, cb, { poll: 1000 })` a site for
its "now", and one for the market's quotes. `pulse` and `quotes` have no
`@ttl`, so a poll is answered 304 by the node, the query not run, while
nothing they read was written; the rollup worker writes `pulse` only when
its numbers change. The browser polls Kestrel with an ETag of its own:
a hundred open dashboards are one poll of the database a second, and no
browser holds a connection open between polls. The pages start polling
four seconds after they load, by when they have loaded.

### No `@hash` on the events' low-cardinality fields

The facets and the funnel read `name`, `country`, `device` and `browser`,
and a hash index would answer an equality on them. But a hash bucket is a
list, and taking a row out of one walks it: with a few values each of a
hundred thousand rows, the `@ttl` sweep held the write lock 370 to 500 ms
for every 1 000 rows it deleted, and every dashboard question waited
behind it (Gaps, 1). Every question has a range of `at`, which the `@ttl`
index answers, and its filter reads those rows alone.

## Design

The dashboard is a field station's console: a cool survey-paper ground,
slate ink, the kestrel's blue-grey for data and its tawny ochre for what is
live -- the visitors line, the newest minute, the "now" dot, the VWAP line.
One family, Archivo, whose width axis does the work a second face would:
figures condensed and heavy, prose at its normal width. Every panel names
where its numbers came from and how long they took. Dark mode follows the
system. Retention is a table, not a picture, so a screen reader reads it as
one; every chart has a text alternative and each column its numbers on
hover; the filters work without script. On a phone the filters fold away
before the first paint, and the charts keep their labels the size of text.

## Measured

On an Apple M1 laptop with 8 cores, which other agents were using at the
same time (load average 2 to 8), with this checkout's release
`fenec-server` under `--sync 250`. Each figure is one run; take them as
indicative.

### Ingest

Beacons of 20 events of simulated traffic, each client sending one after
another for 10 s (`npm run bench:ingest`), against a node of the
measurement's own:

| | 1 client | 4 clients | 16 clients |
| --- | --- | --- | --- |
| through the endpoint (check, limit, one keyed block a beacon) | 45 216 events/s, p50 0.40 ms, p99 1.44 ms a beacon | 114 153/s, 0.60 / 2.91 ms | 116 978/s, 2.39 / 6.13 ms |
| straight into the node, the same block | 80 689/s, 0.23 / 0.49 ms | 174 147/s, 0.40 / 2.63 ms | 167 757/s, 1.68 / 4.68 ms |
| the rollups written in each beacon's block instead | 28 772/s, 0.68 / 1.27 ms | | 65 390/s, 4.73 / 7.70 ms |

Kestrel's endpoint is one Node process, and at 4 clients and more it is
what limits ingest: it parses and checks each beacon's JSON. Several
processes behind a balancer would take the node's rate, as the "straight
into the node" row shows, with the rate limit made shared (Limits).

### The rollup worker

| | |
| --- | --- |
| lag behind 16 clients writing as fast as they can, from a beacon's answer to the block that folded it landing | p50 257 ms, p99 547 ms, the most 578 ms; 61 370 events/s came in meanwhile |
| catching up on 1.67 million events it did not see arrive | 22.1 s, 75 317 events/s |
| from a beacon through the demo server to `rollup_state` (`test/perf.test.ts`) | p50 500 ms |

Most of the lag is the stream itself: under `--sync 250` a write reaches
`/_changes` once an fsync covered it, up to 250 ms later. The worker's
blocks share the write lock with ingest, so while it keeps up with 16
clients ingest runs at 61 000 events a second rather than 168 000; at a
site's real rate -- a thousand events a second is a busy site -- the
worker's block lands every second and holds the lock for milliseconds.

### The dashboard at a million and ten million events

A site of 1 000 000 and of 9 881 917 events over 30 days, written with the
worker folding them beside the load (`npm run bench:queries`), each range
asked 25 times through `dashboard()` over HTTP under the viewer's token
-- every question of the page side by side, as the server asks them.
"Raw" with every device as a filter forces the raw path over the same
rows, to show what the switch saves.

| | 1 000 000 events | | 9 881 917 events | |
| --- | --- | --- | --- | --- |
| range | p50 | p99 | p50 | p99 |
| 1 hour, raw | 0.7 ms | 1.0 ms | 5.0 ms | 5.9 ms |
| 24 hours, raw | 23.3 ms | 26.2 ms | 272 ms | 347 ms |
| 24 hours, raw, one country | 23.6 ms | 25.4 ms | 321 ms | 335 ms |
| 7 days, rollups | 25.3 ms | 28.0 ms | 285 ms | 419 ms |
| 7 days, the raw path forced | 300 ms | 315 ms | 4.62 s | 4.97 s |
| 30 days, rollups | 88.0 ms | 93.9 ms | 1.15 s | 1.73 s |
| 30 days, the raw path forced | 1.12 s | 1.16 s | 15.5 s | 21.4 s |
| 90 days, rollups | 87.6 ms | 100 ms | 1.13 s | 1.41 s |

The load: 17.5 s for a million events with the worker beside it, folded
21.3 s after the start (46 900 events/s), a file of 140 MB; ten million in
247 s, folded 249 s after the start (39 700 events/s), a file of 1 370 MB,
the node at 1.56 GB resident (macOS's count, with its allocator's cache).

Where the time goes at ten million events: over a day of raw events
(330 000 rows), the facets and the series, each a scan of the day, about
265 ms; over 30 days of rollups, the funnel, 968 ms -- a row for each
visitor who started a signup in the month leaves the node, since the
order of their steps is compared in the code (Gaps, 6) -- then the
visitors' count, 350 ms. Without the funnel a month of rollups answers in
under 400 ms. Two first ways measured at this size and replaced: the
retention table grouped from a row per visitor and week took 560 to 700
ms of every range, and `count(distinct user)` over a month of visitor-days
refused to count past a million.

### The pages

Lighthouse CI on its mobile profile with the throttling applied (slow 4G,
150 ms round trips, the CPU slowed 4x), the median of three runs
(`npm run lighthouse`), over the demo's data:

| page | perf | a11y | best practices | SEO | FCP | LCP | TBT | CLS | JS (compressed) | page weight |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `/s/fieldnotes?range=24h` | 100 | 100 | 100 | 100 | 0.75 s | 0.75 s | 0 ms | 0.000 | 2.0 KB | 111.4 KB |
| `/s/fieldnotes?range=90d` | 100 | 100 | 100 | 100 | 0.75 s | 0.75 s | 0 ms | 0.000 | 2.0 KB | 114.0 KB |
| `/markets` | 100 | 100 | 100 | 100 | 0.74 s | 0.74 s | 0 ms | 0.000 | 2.0 KB | 201.8 KB |
| `/signin` | 100 | 100 | 100 | 100 | 0.70 s | 0.70 s | 0 ms | 0.000 | 0.0 KB | 93.1 KB |

The budgets CI asserts on every page: LCP under 1.8 s, CLS under 0.05,
TBT under 150 ms, script under 10 KB, performance, accessibility and SEO
at 0.9 at least (`lighthouserc.cjs`; a dashboard's crawlability audit is
skipped, since it says `noindex` on purpose). The page weight is mostly
the font, 90 KB, preloaded and shown only when it is there by the first
paint (`font-display: optional`): swapped in later, the narrower figures
moved the tables under them, a layout shift of 0.25 to 0.36 on the 90-day
dashboard and the market page. The market page's weight counts the polls
Lighthouse waited through. Before the pages were compressed (brotli at
quality 5, about a millisecond a page) a dashboard sent 38 to 47 KB of
HTML; it sends 7.5 to 9.

| file | bytes | gzip | brotli |
| --- | --- | --- | --- |
| `k.js`, the tracker | 1 050 | 677 | 572 |
| `app.js`, the pages' script | 4 300 | 2 047 | 1 796 |
| a dashboard's HTML, a day and 90 days | 38 000 and 46 648 | | 7 549 and 8 925 |
| `archivo.woff2`, the font (Latin, every weight and width) | 90 104 | | |

## Security

`test/security.test.ts`, against the node and the server:

- **Tenants.** A site's viewer and ingest tokens are 403 on every route of
  another site -- `/query` (reads, writes, a drop), `/batch`, REST reads,
  writes, updates and deletes, a subscription, `/_schema`, `/_changes`,
  `/_stats/statements` -- and 401 on its replication feed, which asks its
  own token first; no answer holds another site's row. A tenant that does
  not exist is refused the same way, so a refusal says nothing of which
  exist. A token naming no tenant is refused everywhere.
- **The ingest token** inserts events and does nothing else: reading an
  event or a rollup, a count, `set`, `del`, writing a rollup, the change
  stream, a `PATCH` or `DELETE` over REST, `create`, `drop`: each 403 or
  404, and no event changed.
- **The dashboard token** reads its site and writes nothing; no JWT updates
  or deletes an event (`events append-only`).
- **Tokens** with another secret, no `exp`, an expiry past or `alg: none`:
  401.
- **Beacons**: not JSON, not an object, no events, a bad key, batch id,
  visitor id or time; a bad event name, path (none, too long, a control
  character), an event older than an hour or later than its beacon,
  properties that are not an object, nested, too many or too long, a
  referrer too long: 400, saying which. 51 events: 413. A body over 16 KB,
  with or without `content-length`: 413. Another origin, or none: 403. An
  unknown key: 404. A crawler's user agent: 204, and not counted. None of
  it writes an event.
- **The rate limit**: 16 beacons of 20 events at once to a site limited to
  20 events a second with a burst of 200: exactly 10 land, 6 are 429, the
  site holds 200 events; another site is not held back; a second later the
  site takes more.
- **The dashboard**: another person's site is 404 on every route, its name
  nowhere on the page; no session, a forged or unsigned cookie: back to
  sign-in; a sign-in posted from another site: 403. The cookie is
  `HttpOnly` and `SameSite=Lax`; every page carries a content security
  policy of `default-src 'none'` with its one inline style and script
  allowed by their hashes; dashboards say `noindex`.

## What each part uses

| Part | fenecdb |
| --- | --- |
| Raw events | `eid text @unique`, `at timestamp @ttl(30d)`, a `json` field for properties; `put ... if absent`; `events append-only` and an `insert`-only grant in `policy.txt` |
| Ingest | `db.batch([...], { idempotencyKey })` (`POST /batch` with `Idempotency-Key`), its 422 for a key sent with another request |
| Rollups | `/_changes` with `since` and `wait`; a `/batch` guarded by `set ... require 1`; `put` with ids to write rows over; `greatest`; a read `/batch` for a rebuild's snapshot and its `Fenec-Seq` |
| Visitors over time | `bucket(at, 1m | 30m)` with `count(distinct user)` and `sum(case when ... end)`; past a day, `bucket` over the rollups, and the day's visitors by `count(*)` over `day_users`, which reads the `@sorted` index alone |
| Top pages and referrers | `group path` with `count(*)` and `count(distinct user)`, `order ... desc limit 10` |
| Filters | `facet country top 12 disjunctive, device disjunctive, browser top 8 disjunctive` with `country in [...]`; past a day, `group dim, value` over `day_dims` |
| Funnel | `group user, name` with `min(at)`, `name in [...]` |
| Retention | `cohorts`, counters kept by the worker; `weekly` with `user @hash` for a visitor whose cohort moves |
| Visitors of a range | `count(*) where last >= $1` over `visitors.last @sorted` |
| Live "now" | `FenecHttp.live(text, cb, { poll })` and its 304s; a row without `@ttl` |
| Market data | `first(px by at)`, `last(px by at)`, `max`, `min`, `sum(qty)`, `sum(px * qty) / sum(qty)` by `bucket(at, 1m)`; `@ttl(1d)` on ticks; quotes kept with `greatest`, `least` and `case` in the block that writes the ticks |
| Tenants and tokens | `fenec-server --dir`, `PUT /_admin/tenants/<t>`, `POST /t/<t>/_schema/apply`; JWTs with `tenant`, `role` and `exp`; per-operation grants; `--audit` |
| Warm start | `--warm all`: a site's indexes built as it opens, not in the first dashboard request |

## Limits, and honest notes

- **The rate limit is in the process's memory.** Two ingest processes would
  each allow a site its rate. A shared limit would be the counter recipe
  (`put limits ... if absent`, `set ... require 1`) in the beacon's block,
  a statement more a beacon.
- **The visitor id is the browser's.** A visitor on two devices is two
  visitors, one who clears storage a new one; there is no cookie and no
  fingerprint.
- **Times are UTC**, days and weeks included; a site in Tokyo sees its
  evening split across two days.
- **The country** comes from a CDN's header when there is one, else from
  the browser's language (`en-US` counts as the United States).
- **A filter over 30 days of a busy site is slow** (below): it reads the
  raw events. A deployment with filters as its main use would keep
  rollups by country and device too.
- **Visitors of a range that does not end now** are not kept: every
  range the dashboard offers ends now. Past days' visitors are each day's,
  from `day_users`.
- **A distinct count from the raw events stops at a million.** Visitors
  per page and per referrer, and a range's visitors with a filter set,
  are counted exactly or the page says they passed the bound.
- **The worker shares the write lock with ingest**: while it keeps up with
  16 clients writing as fast as they can, ingest runs at about a third of
  the rate it has alone.

## Gaps this example hit

Each with the statement that showed it and the smallest feature that would
close it.

1. **Taking a row out of a `@hash` bucket walks the bucket.**
   `HashIndex::remove` is `bucket.retain(|d| *d != id)`. With `country`,
   `device`, `browser` and `name` under `@hash` -- a few values each,
   buckets of a hundred thousand rows over 594 000 events -- the `@ttl`
   sweep logged `1000 expired rows of events swept, 370.1ms under the
   write lock` again and again, and every dashboard question, 1 ms alone,
   took 400 ms behind it. Without the four indexes the sweep stays under
   the log's threshold. The bucket is linear in its size per removal, so a
   low-cardinality hash index makes every delete of the collection slow,
   not only the sweep's. Smallest: remove a block's ids from a bucket in
   one pass (the sweep and a `del` group their ids by key and `retain`
   against a set once), or keep a bucket's ids ascending -- they are
   appended so -- and binary-search the one to take out.
2. **No upsert that adds.** A rollup row is "make it at zero if missing,
   then add": `put minutes {key: $1, n: 0} if absent` and `set minutes {n:
   n + $2} where key = $1`, two statements a key, a thousand keys a page in
   a backfill. The worker now reads which keys exist before its block and
   writes the new rows and the old ones over by id, two statements a
   collection; a read the guard has to make safe. Smallest: `put ... if
   absent else set {n: n + $k}` -- an upsert by the `@unique` key, the
   `set` reading the row it finds -- so a page is one statement a
   collection and no read.
3. **A parameter cannot be the documents of a `put`.** `put events $1 if
   absent` with a list of objects is `expected {, found $1`, so every
   beacon's statement is written out with ten parameters an event
   (`{eid: $1, name: $2, ...}, {eid: $11, ...}`): a text per beacon size,
   and a rollup block's statements are texts of up to 500 documents, past
   the 1 KB the parse cache keeps. Smallest: a parameter where a document
   or a list of them goes.
4. **`/_changes` does not say where the writes end.** Under `--sync 250`
   a write is in the stream only once an fsync covered it, so an empty
   page cannot tell a worker that has caught up from one whose writes are
   not on disk yet: `catchUp` waits out 600 ms of an empty page before it
   believes it. Smallest: the database's change counter beside
   `Fenec-Next` (`Fenec-Seq` on the answer), so a reader knows what is
   still to come.
5. **A distinct count stops at a million, and has no mergeable form.**
   `get day_users select count(distinct user) where day >= $1` over the
   last 30 days of ten million events answered `count(distinct ...) holds
   more than 1000000 values: a count is not cut short, so narrow the
   filter`: the month had 2.4 million visitors. The rollups count a
   range's visitors as `last >= day` instead, which only works for a range
   that ends now; a raw range wide enough says so on the page. Days'
   distinct counts cannot be added, and fenecdb keeps no sketch a day
   could hold. Smallest (M): `approx_count_distinct(user)` folding
   HyperLogLog registers, mergeable across a rollup's rows.
6. **No condition over a group's aggregates.** The ordered funnel's last
   step is "visitors whose first finish is no earlier than their first
   start": `get firsts select user, min(case when name = $3 then at end) as
   a, min(case when name = $4 then at end) as b where day >= $1 group user`
   and then `b >= a` counted. There is no `having`, and no `get` over the
   rows of another, so every visitor's row leaves the node and the code
   counts: 968 ms of a month's dashboard at ten million events. Smallest:
   `having <expr>` over a group's items, with `count` after it counting the
   groups that pass (`... group user having b >= a count`).

## Files

```
schema.fenecql      a site's collections: raw events and every rollup
markets.fenecql     ticks and quotes
control.fenecql     the sites and who signs in
policy.txt          what each token may read and write
client/
  tracker.ts        the tracker a site loads (public/k.js)
  app.ts            the pages' one script (public/app.js)
src/
  server.ts         the endpoint, the dashboard, the market page; the workers and the feed beside them
  ingest.ts         a beacon checked, limited, written once
  rollup.ts         the rollup worker: the fold, the guarded block, the rebuild, the pulse
  queries.ts        the dashboard's questions and the raw/rollup switch
  market.ts         the tick generator, the feed's blocks, bars and the window
  charts.ts         the SVG charts, for the server and the browser
  views.ts, styles.ts   the pages and their one stylesheet
  live.ts           the shared live polls
  sim.ts, traffic.ts    invented visitors, and the demo's beacons
  setup.ts, users.ts, tokens.ts, db.ts, time.ts, config.ts
scripts/            db.sh, setup.ts, rollup.ts, build.ts, ci.sh, lighthouse.sh, bench-*.ts
test/               correctness, security, seo, perf
```
