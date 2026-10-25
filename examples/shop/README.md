# Sandgrouse: a shop on fenecdb

A real-world e-commerce example. Sandgrouse is an invented desert and travel
gear shop with 10 000 generated products in 30 categories. It has:

- category pages with facets, sorting and crawlable pagination;
- search with highlighted matches, snippets and facets;
- product pages kept by incremental static regeneration, with live stock;
- guest and signed-in carts held to each shopper's own token;
- a checkout that cannot oversell, and an idempotent mock payment.

It is built for search engines and page speed. Its claims are measured, and
tests hold its correctness, security and SEO.

The stack:

- Next.js 16 App Router and TypeScript;
- `fenec-server` as the only database;
- server components that read through `@fenecdb/web/client` (`connect`),
  typed by the schema declared in code (`lib/schema.ts`).

Every product, maker and review is generated. No card is charged.

## Architecture

```
 browser                       Next.js server (app/, lib/)                    fenec-server
 ───────                       ───────────────────────────                    ────────────
 HTML pages, no JS needed  ◀── server components: home, /c/<category>,        products, categories,
 (forms post, links GET)       /p/<slug> (ISR, 5 min), /search               inventory  (catalog)
                                 │  connect(url, {token, schema})  ─────────▶ get ... facet ... match
                                 │  the shop's own token                       ... highlight ... near
 POST /api/cart ─────────────▶ route handlers                                  
                                 │  a JWT minted per shopper (sub = owner) ──▶ carts, orders, order_lines
                                 │  policy.txt: where owner = $jwt.sub          (scoped: USING + WITH CHECK)
 POST /api/checkout ─────────▶  │  /batch + Idempotency-Key, shop's token ──▶ set inventory ... require 1
 POST /api/pay      ─────────▶  │  mock provider capture, then /batch      ──▶ insert orders, order_lines
                                                                              del carts  (one block)
 live stock island  ──── GET /api/stock-token (role: stock, 10 min) ───────▶ FenecHttp.live: a
   (after the first touch)      then straight to fenec-server, CORS          poll, 304 while unchanged
```

The site's server holds two kinds of token:

- **Its own token** reads the catalog and runs checkout. Checkout moves
  stock across every shopper's orders, so no single shopper's token could
  run it.
- **A JWT minted per shopper** (`lib/db.ts`, `lib/jwt.ts`) reads and writes
  carts and orders. The `sub` is `u:<user id>` for a signed-in shopper, or
  `g:<hash of the guest cookie>` for a guest. `policy.txt` holds every such
  token to `owner = $jwt.sub`. So a bug in the shop that passed another
  shopper's id would still reach nothing: the database refuses, not only the
  code.

## Run it

```sh
cargo build --release -p fenec-server        # from the repository's root
cd examples/shop
npm install
npm run db                                    # fenec-server on :8080, data/shop.fenec, policy.txt
npm run seed                                  # 30 categories, 10 000 products, their stock: about 1 s
npm run build && npm start                    # http://localhost:3000
```

`npm run dev` works too. The build renders the home page and the sitemaps,
so it needs the server running and seeded.

| Variable | Default | For |
| --- | --- | --- |
| `FENEC_URL` | `http://127.0.0.1:8080` | where the site's server reaches fenec-server |
| `FENEC_PUBLIC_URL` | `FENEC_URL` | where a browser reaches it, for the live stock |
| `FENEC_TOKEN` | `shop-dev-token` | the server's own token |
| `FENEC_JWT_SECRET` | a dev value | the HS256 secret fenec-server checks shoppers' tokens with |
| `SHOP_SECRET` | a dev value | signs the session cookie |
| `SHOP_ADMIN_TOKEN` | `shop-dev-admin` | for `POST /api/reap` from a cron job |
| `SITE_URL` | `http://localhost:3000` | canonical URLs, sitemaps, CORS |

The defaults are for development only; set every secret in a deployment.

Test cards:

- `4242 4242 4242 4242` is approved;
- a card ending in `0002` is declined;
- a card ending in `0069` has expired.

## Tests and measurements

```sh
npm run ci                       # a server over a new file, seed, lint, build, start, the tests
SHOP_LIGHTHOUSE=1 npm run ci     # and Lighthouse CI's budgets
SHOP_LOAD=1 npm run ci           # and the page timings and the load test
npm test                         # the tests alone, against a running shop and server
```

From the repository's root, `examples/run-tests.sh shop` runs the same on
this checkout's packages. CI's `examples (shop)` job runs it with
`SHOP_LIGHTHOUSE=1`.

## What each part uses

| Part | fenecdb |
| --- | --- |
| Schema | Declared in code with `fenecTable` (`lib/schema.ts`); `connect(url, { schema, migrate: true })` makes what is missing when seeding, and the pages connect with `schema` alone, which only checks |
| Catalog | `@unique` on `sku` and `slug`, `@hash` on `category`, `brand` and `colour`, `@sorted` on `price`, `rating` and `added`; prices in integer cents |
| Category page | `facet brand, price ranges [...], colour, material` beside the page, in its statement; a facet with a filter on is `disjunctive`, counted without its own filter; `order` over a `@sorted` field walks the index; `offset`/`limit` pages |
| Search | `match description $1` over `@text(prefix=5)`, so a part of a word or a late typo finds it ("titanum" finds titanium); `highlight(name)` and `snippet(description, 26)` as UTF-16 offsets, drawn as `<mark>` nodes so no text is read as HTML; `facet category, brand, ...` |
| Related products | `near features $v` within the category, over an `@hnsw` vector of the product's attributes (`lib/features.ts`) |
| Product page | Incremental static regeneration (5 minutes); stock from `inventory`, then `FenecHttp.live` polled in the browser |
| Cart | Under the shopper's JWT: `put carts {...} if absent`, then `set carts {qty: qty + $1} where line = $2 and qty + $1 <= 20 require 1`, one `/batch`; `@ttl(7d)` on `touched` expires an abandoned line; `lookup products on sku = sku` prices the lines in the same query |
| Checkout | One `/batch` with an `Idempotency-Key`: `set inventory {available: available - $1, reserved: reserved + $1} where sku = $2 and available >= $1 require 1` for each line, `insert orders`, `insert order_lines`, `del carts` |
| Payment | A mock provider's idempotent capture, then one `/batch`: `insert payments` (`@unique` on the order, so a second capture is a clash), `set orders {status: "paid"} where number = $1 and status = "reserved" require 1`, the units moved from reserved to sold |
| Reservations | An order is a hold: `holdUntil` is `now() + 15 min`; a decline or a lapse releases it with `where status = "reserved" require 1`, so a payment and a release racing for one order cannot both win |
| Accounts | `users.email @unique`, so two sign-ups with one email cannot both land; passwords as scrypt |
| Security | A JWT per shopper and `policy.txt`: reads are filtered, writes checked before and after (`USING` and `WITH CHECK`) |

### Why checkout cannot oversell

Stock is a counter per sku. A line's units are taken by a `set` whose
condition is the guard (`available >= $1`), and `require 1` turns a miss
into an error. Without `require`, a write that matches no row answers
`{"affected": 0}`, and the rest of the batch lands: an order for stock that
was not there. With it, the server answers 412 with `"at"`, the index of the
statement that missed. Every statement of the `/batch` is put back: every
line's reservation, the order and its lines. The shop names the product
that ran out from `at`.

There is one writer, and the block holds the lock. Two shoppers after the
last unit therefore cannot both pass the guard.

Units move between three counters:

| Counter | Holds |
| --- | --- |
| `available` | what can still be sold |
| `reserved` | what orders awaiting payment hold |
| `sold` | what was paid for |

Every move is guarded the same way, so the three always add up to what was
stocked. The tests and the load test check that sum against the order
lines.

The `Idempotency-Key` comes from the checkout form, made when the form was
drawn, and the order number is derived from it. A second click, or a retry
after a timeout, is answered with the first answer (`Idempotent-Replayed`)
and makes no second order.

## Measured

Every number below was measured on an Apple M1 laptop with 8 cores, which
other agents were using at the same time (load average 3 to 6). The server
was this checkout's `fenec-server`, in release mode, with `--sync 250`, and
the site ran on `next start`, so the absolute figures are indicative. The
data was the generated catalog of 10 000 products.

### Lighthouse

The profile is Lighthouse's mobile one, a Moto G Power-class screen on slow
4G (150 ms round trips, 1.6 Mbps down), with the CPU of a mid-tier phone:
slowed until Lighthouse's own benchmark scores 1 000, DevTools' calibrated
"mid-tier mobile" (a Pixel 5), which took 3.13x on this machine. Why the
slowdown is worked out rather than Lighthouse's 4x is below. Each row is the
median of 3 runs (`npm run lighthouse`, `lighthouserc.cjs`).

**With the throttling applied** (`throttlingMethod: devtools`), which is what
CI asserts:

| page | perf | a11y | best practices | SEO | FCP | LCP | TBT | CLS | JS (compressed) | page weight |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `/` | 100 | 100 | 100 | 100 | 1.39 s | 1.39 s | 20 ms | 0.000 | 132.8 KB | 163.2 KB |
| `/c/backpacks` | 100 | 100 | 100 | 100 | 1.39 s | 1.39 s | 14 ms | 0.000 | 136.6 KB | 181.4 KB |
| `/search?q=titanium+stove` | 100 | 100 | 100 | 66 | 1.38 s | 1.38 s | 14 ms | 0.000 | 136.6 KB | 184.1 KB |
| `/p/<a tent>` | 100 | 100 | 100 | 100 | 1.40 s | 1.40 s | 15 ms | 0.000 | 134.2 KB | 162.5 KB |

**With Lighthouse's default estimate** (`simulate`, Lantern), measured the
same day with the same slowdown:

| page | perf | FCP | LCP | TBT | CLS |
| --- | --- | --- | --- | --- | --- |
| `/` | 99 | 0.76 s | 2.02 s | 11 ms | 0.000 |
| `/c/backpacks` | 99 | 0.91 s | 2.17 s | 14 ms | 0.000 |
| `/search?q=titanium+stove` | 99 | 0.91 s | 2.17 s | 15 ms | 0.000 |
| `/p/<a tent>` | 99 | 0.76 s | 2.02 s | 11 ms | 0.000 |

Against the budgets:

- **CLS under 0.05** is met both ways. Pictures are inline SVG in boxes of
  fixed size, and the font's fallback is sized to it.
- **TBT under 150 ms** is met both ways on a mid-tier phone's CPU, and not
  on a low-tier phone's (below).
- **LCP under 1.8 s** is met with the throttling applied (1.38 to 1.40 s)
  and missed under the estimate (2.02 to 2.17 s). The estimate replays the
  page's own load. On this machine the page's scripts arrive and run before
  its first paint, so Lantern puts Next.js's runtime on the LCP's path, even
  though the scripts are `async` and on a phone load after the paint. The
  observed LCP was the first paint, 83 ms, every time. An empty Next 16 page
  was estimated at 1.37 s with the CPU slowed 4x. CI asserts the applied
  numbers, and both are here.
- **JavaScript under about 70 KB, compressed, on product and category
  pages** is not met, and cannot be with this stack. An empty Next 16 App
  Router page (one `<h1>`) ships 132.3 KB of JavaScript, compressed, on the
  same profile. The shop's pages ship 132.8 to 136.6 KB, so its own code is
  2 to 4 KB on top. That code is the "Show more" island and the live stock
  island, which loads the client entry only after the shopper's first
  interaction. The 70 KB budget is below the framework's floor. CI asserts
  145 KB, the floor plus a small allowance for the shop's own code. Meeting
  70 KB would mean a lighter renderer than the App Router.
- **SEO** is 100 everywhere but search, whose results pages carry `noindex`
  on purpose.

**The CPU is worked out for the machine.** Applied throttling slows the
machine's own CPU by a factor, so Lighthouse's fixed 4x measures a different
phone on every machine, and GitHub's runners come with different CPUs. Over
67 CI runs at 4x the pages' TBT came in bands, the four pages of a run
moving together (the product page against the other three's mean: a
correlation of 0.77), and the one failure, the product page at 194 ms, came
from the slowest band. A median of three runs takes out a run, not the
machine. So `scripts/cpu-slowdown.mjs` runs Lighthouse's own benchmark, the
`benchmarkIndex` every report carries, in a Chrome launched as Lighthouse
launches one, before the runs, and slows the CPU by what brings it to 1 000,
as DevTools' calibrated CPU throttling finds its rates. Ten runners at once
each way, each page's median TBT:

| CPU | benchmarkIndex | TBT at 4x | slowed to 1 000 | TBT there |
| --- | --- | --- | --- | --- |
| AMD EPYC 7763 | 2 140-2 520 | 103-133 ms | 2.46-2.64x | 40-54 ms |
| Intel Xeon Platinum 8370C | 2 630-2 700 | | 2.58x | 37-43 ms |
| Intel Xeon Platinum 8573C | 2 740-3 350 | 72-85 ms | | |
| AMD EPYC 9V74 | 2 860-3 130 | 64-78 ms | | |
| Intel Xeon 6973P-C | 3 140-3 750 | 54-62 ms | 3.72x | 41-49 ms |
| AMD EPYC 9V45 | 3 530-4 540 | 40-57 ms | 3.99-4.44x | 48-63 ms |
| this M1 | 2 390-3 050 | 31-36 ms | 2.99-3.13x | 8-14 ms |

The benchmarkIndex is the one Lighthouse measured as each run began; a
blank is a CPU the draw of runners did not give that way. The benchmark is
not the pages' work: at one score the runners still differ by up to 1.5
times, and at the mid tier this M1 blocks for a quarter to a third of what
they do, so CI's numbers are not this machine's. After the runs `npm run
lighthouse` prints each run's TBT and the benchmarkIndex it began at, and
marks one a fifth or more off the index the slowdown was worked out from,
which ran on a busier or an idler machine than that: here at 4x, while
another job ran, the product page's last two runs began at 1 740 and 1 710
against 2 390-2 880 for the rest, and blocked for 84 and 85 ms.

**Chrome is read in before its first launch.** `scripts/warm-chrome.mjs`,
which `scripts/lighthouse.sh` runs first, reads Chrome's own files through
once, a mebibyte at a time. On a fresh CI runner Chrome's first launch
faults its binary in from a cold disk, and Lighthouse's chrome-launcher
gives it 25 s (50 polls of 500 ms) to say where it listens; once it did not,
and the job failed with `waiting for dynamic debugging port in
chrome-err.log` after every test had passed. The same launch in a container
with its page cache dropped and its reads of the disk held to 80 a second
failed with that error after 27.8 s, twice; with Chromium read first (4.7
s) it took 5.6 s, twice. The step prints what it read and how long that
took (here, 1.5 GB of the whole app in 1.1 s the first time, 0.2 s after).
`CHROME_PATH` names Chrome itself where `google-chrome` is a wrapper script
in a directory that is not Chrome's.

**On a low-tier phone the TBT budget is not met.** DevTools' "low-tier
mobile", a benchmarkIndex of 264, is the Moto G Power whose screen the
profile has. Slowed to it (`LHCI_CPU_TARGET=264 npm run lighthouse`: 11.52x
here, 7.5 to 15.5x on the runners), the pages block for 182 to 191 ms here
and 242 to 400 ms on the runners. Lighthouse says its 4x moves a desktop
into the mid-tier mobile bracket, and CI asserts the mid tier.

Two choices were measured on the way:

- **The display face.** Schibsted Grotesk as a variable font, every
  weight, is 47 KB. It cost the product page 265 ms of estimated LCP
  against no web font: 2.18 s against 1.92 s. The shop uses it at 800 only,
  for display type, cut to ASCII and a few marks (15.7 KB). Running text is
  the system's sans.
- **A cold search.** The first `match` after a server starts builds the
  text index, which is derived and built on first read. That first search
  page took 2.68 s of LCP; every one after took 1.40 s.

### Server timings

Each row is one request at a time, from the first byte sent to the last
received, on this machine. Each page kind was asked 200 times over
different addresses, after 20 to warm up (`npm run timing`).

| page | p50 | p95 | p99 | HTML |
| --- | --- | --- | --- | --- |
| home (kept, regenerated every 5 min) | 1.87 ms | 2.44 ms | 3.39 ms | 67.1 KB |
| category: a page, a filter, an order | 8.36 ms | 10.8 ms | 14.4 ms | 99.6 KB |
| search: match, highlight, snippet, facets | 8.98 ms | 12.1 ms | 15.0 ms | 104.9 KB |
| product, made on its first request | 7.66 ms | 9.85 ms | 13.4 ms | 44.8 KB |
| product, kept (ISR) | 1.33 ms | 2.05 ms | 3.36 ms | 44.8 KB |

About two thirds of a category page's HTML is Next's inlined React Server
Components payload, which repeats the page for the client's router.

### Load test

16 concurrent clients, 10 s a route, every request answered by the Next
server with its data in fenec-server (`npm run load`):

| route | rate | p50 | p99 | failed |
| --- | --- | --- | --- | --- |
| search (`GET /api/search`) | 1532/s | 10.2 ms | 17.2 ms | 0 |
| category (`GET /api/c/<slug>`) | 1833/s | 8.12 ms | 15.7 ms | 0 |
| add to cart (`POST /api/cart`) | 1197/s | 13.1 ms | 21.4 ms | 0 |
| checkout (an add, then the order) | 400/s | 27.7 ms | 109 ms | 0 |

4 004 orders were placed. Afterwards every sku's `reserved + sold` equalled
its order lines, and `available + taken` equalled what was stocked. The
checkout row is a whole checkout: the add, then reaping lapsed holds, the
cart read under the shopper's token, and the one `/batch`. A single Node
process serves the site, so these are the site's limits as much as the
database's.

### Tests

There are 18, all passing (`npm test`, against the running shop and
server).

| File | What it holds |
| --- | --- |
| `test/checkout.test.ts` | 24 shoppers checking out the last 5 units: exactly 5 orders, the rest refused naming the product, and inventory `{0, 5, 0}`. Mixed quantities from 16 shoppers over 11 units: never more than 11 sold, and stock equals what was stocked less every order line. One checkout key sent twice at once and once more after: one order, one line, stock taken once. Cart totals equal catalog prices summed, the shipping threshold applies, and the cap of 20 holds. Payment moves units once, a second capture is the same reference, and a decline puts the units back and cancels the order. A lapsed hold is released once and cannot then be paid for |
| `test/security.test.ts` | Bob reaching Alice's order (page, JSON, payment) or her cart lines through any route: 404, and her cart unchanged. A forged session cookie is no session. A shopper's JWT sent straight to fenec-server reads only its own rows (lookups and counts included), changes none of another's (`affected: 0`), and is refused a row written for another (403) and the collections no rule names (404). The stock token reads stock and writes nothing. Guessed guest cookies find nothing; the database holds only a hash of the cookie; cookies are `HttpOnly` and `SameSite=Lax`. A price, total or discount in a request is ignored. Bad quantities and unknown skus are refused. Seven injection strings in search and in a filter change nothing and widen nothing. A write sent from another origin is refused |
| `test/seo.test.ts` | A product's title, description, canonical, Open Graph, and JSON-LD `Product` with `Offer` (price as a decimal string, `USD`, availability) and `AggregateRating` (left out with no reviews). `BreadcrumbList`. A category's `ItemList` and positions, its canonical by page, `noindex` on filtered views, and `rel="next"`. The sitemap index and its files list every product and category. robots.txt. The content is in the HTML |

## Semantic search

The search is by words: BM25 over `@text(prefix=5)`. Semantic search needs
a sentence model's vectors, and no model can run here without a download or
a key, so the shop has none. `lib/catalog.ts` marks where one would go: an
`embedding vector<384>` filled at seed time by the model, and
`.match('description', q).near('embedding', embed(q)).fuse()` in
`listing()`. A facet beside `near` is refused (it ranks every row rather
than selecting some), so the facets would stay a query of their own over
the `match`.

"Close alternatives" on a product page does use `near`, over a vector the
shop can make without a model: the product's category, colour, material,
price and rating (`lib/features.ts`). It finds what is like the product,
not what means the same.

## Gaps this example hit

These are beyond those already listed for inventory and payments. All seven
are closed now, and the shop uses what closed them:

1. **`/batch` and `Idempotency-Key` in the client.** Checkout and the cart
   go through `db.batch([...], { idempotencyKey })`, which answers each
   statement's result and throws a `FenecError` naming the statement that
   stopped it (`at`) and why (`status`, 412 for a `require` not met).
   `lib/db.ts`'s `batch` is a few lines over it, where it posted `/batch`
   with a `fetch` of its own.
2. **An assertion without a write.** Checkout holds each line's price inside
   its block: `get products select sku where sku = $1 and price = $2 limit 1
   require 1` beside the stock it takes, so a price changed since the cart
   was read is a 412 at that statement and nothing of the order lands.
3. **Disjunctive facets.** A facet with a filter on is `facet brand
   disjunctive`: counted without its own filter, so "Brand" still lists
   the other brands to add, in the page's own statement -- where each was
   a query of its own (`lib/catalog.ts`).
4. **A facet over ranges.** The price bands are `facet price ranges [0,
   2500, 5000, 10000, 25000, ...]` over the `@sorted` price, counted from
   its index; the `priceBand` field written at seed time is gone.
5. **A live query per viewer was a server thread.** The live stock polls
   (`FenecHttp.live(..., { poll: 5000 })`): the server answers 304 without
   running the query while the stock has not moved, and holds no stream,
   no thread, between rounds.
6. **The first search after a start was slow.** `fenec-server` builds the
   derived indexes after the open, beside the first requests (`--warm`,
   on by default for a file; `scripts/db.sh` writes it out), so the first
   search does not build its text index: 2.68 s of LCP for that first
   search page against 1.40 s warm before.
7. **The builder's row type lost `facets`** when a query was built in
   steps. A query whose type names no facet reads its counts as `Facets`,
   maybe absent, and `rows.facets` needs no cast.

The inventory pattern and the composite unique the cart works around with
`line = owner|sku` are as `realworld-gaps.md` describes them. `@ttl` can
give stock back now -- `expired()` reads the rows past their time for a
reaper -- but a reservation here is its order's row, which outlives it, so
the order keeps `holdUntil` under `@sorted` and the reaper finds it so.

## Files

```
app/                 pages and route handlers (App Router)
  c/[slug]           category: facets, sorts, pages, ItemList
  p/[slug]           product: ISR, Product JSON-LD, live stock, related
  search             match, highlight, snippet, facets
  cart, checkout, orders/[number], account
  api/               cart, checkout, pay, account, listing JSON, stock, reap
  sitemap.xml, sitemaps/[file], robots.ts
components/          cards, the drawn product pictures, listings, islands
lib/
  schema.ts          the collections, declared in code
  catalog.ts         the catalog's reads
  cart.ts            the cart, under the shopper's token
  checkout.ts        reserve, order, pay, release, reap
  db.ts, jwt.ts      connections, minted tokens, /batch
  generate.ts        the seeded catalog generator
policy.txt           what a shopper's token may read and write
scripts/             db.sh, seed.ts, ci.sh, lighthouse.sh, warm-chrome.mjs, cpu-slowdown.mjs, timing.ts, load.ts
test/                checkout, security and SEO tests
```
