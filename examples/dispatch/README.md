# Plover: courier dispatch on fenecdb

A real-world example of a multi-tenant dispatch product on fenecdb. Plover
is what a delivery company runs its couriers with: customers place orders
from merchants, and each order goes to a courier near its pickup, who
rides it to the door. It has:

- **orders as jobs**: a courier's phone claims the nearest ready order
  within 3 km under a lease, or waits at the server until one is placed
  (`Fenec-Wait`); the lease is extended with every position the phone
  sends, so a courier who goes quiet -- a dead phone, a killed app -- holds
  the order until it lapses and not a moment longer, and the next courier
  is handed it with nothing written to say so; a delivery is the ack, an
  order given back waits out a backoff, and one claimed five times
  without a delivery is a dead letter for a dispatcher;
- **couriers on a map**: positions written over every 2.5 s
  (`put ... if absent else set`), gone from every read two minutes after
  the last (`@ttl`); a dispatcher's live map streams every courier and
  every open order, and an order picked shows its ten nearest idle
  couriers (`near loc`) and who is within 1, 2 or 3 km of its pickup
  (`distance(loc, $1) <= $2`), each a live query run again as positions
  land;
- **a company a tenant**, `fenec-server --dir` on two nodes behind
  `fenec-shard`, each placed with its schema and its plan's quota: a
  Starter company writing ten times its plan is refused (429) while a Pro
  one on the same node is answered as before;
- **people** in a platform tenant: one account per email whatever its
  case (`email text nocase @unique`), staff codes unique within a company
  (`@unique(company, code)`), a merchant's order reference unique within
  the company so an order sent twice is one (`@unique(merchant, ref)`), a
  courier's latest deliveries a run of `@sorted(courier, delivered_at
  desc)`;
- **scoped access** held by the nodes: a dispatcher reaches their own
  company alone, a courier reads only the orders they carry and writes
  only claims that name themselves, a customer reads and places only
  their own orders and follows their courier on them;
- **analytics**: deliveries an hour on time and late, the day's waves,
  order-to-door time, kilometres ridden, the busiest couriers -- an
  Insights page, and a Grafana dashboard through the fenecdb data source;
- **a courier's phone** as a responsive page, and a customer's tracking
  page.

Its claims are held by tests -- 50 couriers claiming at once and 50
waiting at the server, never one order twice; a courier killed with
SIGKILL mid-delivery and its order handed on as the lease lapses; two
companies' quotas side by side; every token against another company,
another courier and another customer; every Grafana panel under Grafana's
token -- and its numbers are measured. Writing it found the gaps below,
which are its main result.

Every company, person, street and order is invented, and so is Merrow,
the city.

## The stack, and why

- **Next.js App Router and TypeScript**, as the shop: pages rendered on the
  server under the signed-in person's own token, route handlers for the
  courier's phone and the customer's orders, the schema declared in code
  (`lib/schema.ts`) and handed to each tenant as FenecQL when the router
  places it.
- **`@fenecdb/web/client`** for every statement, and for the live parts in
  the browser: the dispatcher's map and the customer's tracking page talk
  to the router themselves with a five-minute token the server mints, so
  the database decides what each page may see.
- **`fenec-server --dir` and `fenec-shard`**, nothing else: a tenant for the
  platform's accounts and one a company.
- **A map drawn from a generated city, not map tiles.** Merrow is a street
  graph made from a seed -- a grid bent out of true, a river with bridges,
  a coast, parks -- laid over real degrees so `distance` and `near` answer
  in metres. The demo and its tests need no network once installed, a tile server's usage
  policy forbids a simulator's bulk requests, and moving couriers along
  streets needs the streets themselves, which a tile is only a picture of.
  The map is SVG drawn on the server from the same graph the couriers ride
  (`lib/city.ts`, `components/map-base.tsx`), with no map library.
- **`lib/fetch.ts`**: the server side passes `connect` a `fetch` over
  `node:http` with a keep-alive agent. Under Node 26.7, ten loops each
  sending a `/batch` every 50 ms through Node's own `fetch` had 60 of 150
  requests take about 450 ms -- against a plain `node:http` server as much
  as against fenec-server, where Node 22's took 1.7 ms p50 and none past
  100 ms. Over `node:http` the same loops took 0.55 ms p50.

## Architecture

```
 browser / phone                 Plover (Next.js)                       fenec-shard :8090         fenec-server --dir
 ───────────────                 ────────────────                       ─────────────────         ──────────────────
 sign in (email, any case) ─────▶ /api/signin: users where email = $1  ─▶ /t/plover/ ──────────▶ n1  plover:   companies, users
                                  (nocase @unique), scrypt, a cookie                                         (the app's token)
                                                                                                  n2  co-tandem: couriers, positions,
 dispatcher's map ◀── HTML ────── /dispatch: open orders, positions,                                         merchants, orders  (Pro)
   │                              couriers, with the dispatcher's token                          n1  co-hopper: the same           (Starter)
   └── streams, live queries ───────────────────────────────────────────▶ /t/co-tandem/...
       (a 5-minute token from /api/token: subscribe positions and                                each node: --jwt-secret-file,
        orders, near loc for the 10 nearest, distance <= r by state)                             --policy policy.txt,
                                                                                                 --http-cors <site>,
 courier's phone ── /api/courier/claim ─▶ set orders ... order distance(pickup, $4) limit 1    ─▶      a quota a company (the router
   (on shift)       {at}                  returning *, the courier's token, sent with              sends it with the tenant)
                                          Fenec-Wait: held at the node while none is ready
                 ── /api/courier/beat ──▶ /batch: put positions ... if absent else set,
                    every 2.5 s            set orders {run_at: now() + lease, courier_loc}
                 ── pickup, deliver ────▶ set orders ... where id = $1 and claim = $2 require 1

 customer ── /api/orders ───────────────▶ insert orders ... returning id (customer's token: WITH
                                          CHECK pins customer = $jwt.sub, status "ready")
   tracking page ── subscribe orders id=eq.<n> (customer's token) ─────▶ the order row, its courier_loc moving

 npm run simulate: 59 + 19 couriers riding Merrow's streets, each through its own token; orders in the day's waves
 Grafana (grafana/): fenecdb-datasource a company, role app; every panel FenecQL
```

Tokens, each with one job (HS256, `lib/tokens.ts`; every token names its
tenant and an `exp`):

| Who | Token | May (policy.txt) |
| --- | --- | --- |
| A dispatcher | role `dispatcher`, their company's tenant | read and write the company's orders, couriers and merchants; read positions |
| A courier | role `courier` | read their own row, the merchants and the orders they carry; write their own position; update an order whose lease is out -- a claim -- or one they carry, and only the queue's fields |
| A customer | role `customer` | read their own orders and the merchants; insert an order as themselves, ready and claimed by nobody; cancel one nobody carries |
| This app | role `app`, a company's tenant or the platform's | read orders, positions, couriers and merchants (the dashboards); on the platform, read accounts to sign people in |
| Grafana | role `app`, a token a company | the same reads, nothing written |
| The operator | the nodes' `--http-token`, the router's token | setup alone: place tenants, set quotas, load the seed |

## Run it

```sh
cargo build --release -p fenec-server -p fenec-shard   # from the repository's root
cd examples/dispatch
npm install
mkdir -p data && (cd ../../web && npm pack -q --pack-destination ../examples/dispatch/data)
npm install --no-save ./data/fenecdb-web-*.tgz   # this checkout's client: 0.1.10 on npm has no geo or Fenec-Wait yet
npm run cluster &                 # two nodes and the router over data/dev
npm run setup                     # the tenants, people, merchants and 14 days of deliveries
npm run simulate &                # the city moving, six times the clock (PLOVER_SPEED, PLOVER_BUSY)
npm run dev                       # http://localhost:3000
```

Sign in from the page as one of the demo accounts (the password is
`plover demo password`): a dispatcher sees the live map and Insights, the
courier rides from the phone page (the simulator leaves that one courier
alone; "Ride for me" moves them along the streets), and a customer orders
from a shop near them and follows the courier.

```sh
npm test            # each file a cluster of its own; the pages file wants the site (scripts/ci.sh)
npm run lint        # tsc and eslint
npm run bench       # the measurements below, over a cluster of its own
npm run shots       # screenshots of the running site
```

`examples/run-tests.sh dispatch` from the repository's root runs
`scripts/ci.sh` on this checkout's packages and binaries; CI's
`examples (dispatch)` job runs it against release builds.

**Grafana.** `npm run grafana-token`, then `docker compose -f
grafana/docker-compose.yml up -d --build` builds Grafana 13.2 with the
fenecdb data source from this checkout and provisions a data source a
company -- the router's prefix for its tenant and a token of role `app` --
and the dashboard `grafana/plover.json`: deliveries on time and late by
`bucket(delivered_at, $__interval_ms)`, the open orders, kilometres
ridden, the couriers on Geomap (a `geo` field is the two columns Geomap
takes) and the busiest couriers, at http://127.0.0.1:3001/d/plover-dispatch.
`test/grafana.test.ts` runs every panel's statement under that token with
Grafana's built-ins bound as the plugin binds them.

## What fenecdb does here

| Part | fenecdb feature |
| --- | --- |
| A company a tenant, with its schema | `PUT /_shard/tenants/co-<slug> {schema}` (FenecQL from `toFenecQL(company)`); an existing tenant brought up to the code's schema with `connect(url, { schema, migrate: true })` through the router (the merchants' `@geo` was added so) |
| A plan's quota | `PUT /_shard/tenants/<t>/quota {bytes, rows, streams, requests, writes, write_bytes}` after the seed's load; the router keeps it and sends it with the tenant |
| One account per email | `users.email text nocase @unique`: `Sami.Lindqvist@Tandem.example` signs in as `sami.lindqvist@tandem.example`, and a sign-up with either is a 409 |
| Staff codes per company | `@unique(company, code)` on `users`: Tandem and Hopper both have a `C-001` |
| An order sent twice is one | `@unique(merchant, ref)`: the second insert is a 409, answered with the order already there |
| A courier's latest deliveries | `get orders where courier = $1 and delivered_at is not null order delivered_at desc limit 10`, a run of `@sorted(courier, delivered_at desc)` |
| Couriers on the map | `positions`: `loc geo @geo`, `seen @ttl(2m)`, `courier @unique`; `put positions {...} if absent else set {loc: new.loc, ...}` |
| The ten nearest idle couriers | `get positions select courier, state, loc, distance(loc, $1) as m where state = "idle" near loc $1 limit 10`, live (`FenecHttp.live`, `collections: ['positions']`) |
| Who is within reach | `get positions select state, count(*) as n where distance(loc, $1) <= $2 group state`, live |
| A claim | `set orders {status: "assigned", courier: $1, claim: $2, run_at: now() + $3, attempts: attempts + 1} where run_at <= now() and distance(pickup, $4) <= $5 order distance(pickup, $4) limit 1 returning *`: the nearest ready order within 3 km, one statement (below) |
| Waiting for an order | the same `set`, in a `/batch` sent with `Fenec-Wait: 25000`: run at once, held only while it writes nothing, and taking the nearest ready once one is |
| An order's id | `insert orders {...} returning id`, the customer's read rules held to the row as written |
| The lease | every beat: `set orders {run_at: now() + $3, courier_loc: $4} where id = $1 and claim = $2` beside the position, one `/batch`; a lapsed lease is `run_at <= now()` again, and a waiting claim is woken at that moment |
| Ack, give back, dead letter | `set ... where id = $1 and claim = $2 require 1`: a courier whose lease lapsed finds out (412); backoff `now() + 1000 * attempts * attempts`; `case when attempts >= 5 then "dead"` |
| Couriers see their own jobs | `orders read where courier = $jwt.sub`, and a claim's `returning *` answers the order as claimed: the read rules are asked of the row the `set` leaves |
| An order's steps | each a transition, `new.` the order as the write leaves it: a claim `where run_at <= now() and new.status = "assigned" and new.courier = $jwt.sub and new.attempts = attempts + 1`, a delivery `where courier = $jwt.sub and (status = "assigned" or status = "picked") and new.status = "delivered" and new.run_at is null`, a customer's cancel `where ... status = "ready" and new.status = "cancelled" and new.run_at is null`: a claim for another courier, an order delivered unclaimed, a cancelled one taken back or one moved up the queue are 403 |
| Customers follow their courier | a subscription to their own order row (`{ id: 'eq.<n>' }`), the courier's point written onto it with each beat |
| Deliveries an hour, on time | `bucket(delivered_at, 3600000)`, `sum(case when delivered_at <= promised then 1 else 0 end)`, `avg(delivered_at - created)` over `@sorted(delivered_at)` |
| Grafana | fenecdb-datasource over `/t/co-<slug>`, `$__from`, `$__to`, `$__interval_ms` bound as parameters |

## Claiming the nearest order

A job queue's claim is `set ... where run_at <= now() order run_at limit
1 returning *`; a courier's orders it by the distance from where they are
(`lib/dispatch.ts`):

```
set orders {status: "assigned", courier: $1, claim: $2, run_at: now() + $3, attempts: attempts + 1}
  where run_at <= now() and distance(pickup, $4) <= $5 order distance(pickup, $4) limit 1 returning *
```

The ready orders within 3 km are walked nearest first through the
pickup's point index, the first one taken under the node's one write
lock, and answered as claimed -- under the courier's own token, whose read
rules are asked of the order as the claim leaves it. The phone sends it
in a `/batch` with `Fenec-Wait`: it runs at once, and with none near and
ready it is held at the node until an order is placed, a lease lapses or
a backoff ends, and then takes the nearest one ready -- one write request
a claim either way.

It holds: 50 couriers claiming 400 orders at once, and 50 waiting while
150 were placed, took every order once (`test/claims.test.ts`). It was
three steps while `order` took fields alone (gaps 1 and 2): a read of the
six nearest ready orders with the app's token, a `/batch` of a
conditional `set` a candidate, and a held claim of the oldest within the
radius. A held claim still goes to whichever waiting courier runs first,
not the one nearest the new order (gap 3).

## Tests

`npm test` (CI runs them on every push, and `scripts/ci.sh` locally):

| File | What |
| --- | --- |
| `claims.test.ts` (2) | 50 couriers claim 400 orders at once, each delivering what it took: no order claimed twice, every one delivered by the courier who claimed it, each claimed once (`attempts = 1`); 50 couriers waiting at the server while 150 orders are placed 5 ms apart: each goes to exactly one. Tandem's writes are unmetered here (gap 7): what they hold is exactly-once, and `quotas.test.ts` holds the plan |
| `lease.test.ts` (3) | a courier as a process of its own claims, picks up and beats every 300 ms, then is killed with SIGKILL: a second courier waiting at the server is handed the order no sooner than the lease (1.5 s) after the last beat and within 2 s of it, `attempts` 2; the dead courier's late pickup, delivery and beat are refused (412, a lease kept `false`); an order given back waits out its backoff; one claimed five times is a dead letter on the sixth draw |
| `quotas.test.ts` (4) | Tandem (Pro) and Hopper (Starter) on one node: Hopper's 20 couriers beating every 50 ms, ten times its 40 writes a second, are refused (429 `writes`) and land at most its rate, while Tandem's ten are never refused and their median beat is unchanged; a bulk import past 64 KB a second is refused (429 `write_bytes`) and lands after its `Retry-After`; 45 claims waiting at once from a Starter fleet: 40 held, 5 refused (429 `streams`); a company at its rows takes no new order (507 `rows`) and deletes still land |
| `security.test.ts` (5) | a dispatcher's token against another company and the platform (403), no tenant (403), lapsed (401), forged (401); a courier reads only their order, row and position, cannot deliver another's order (it is not there to them), deliver a ready order nobody claimed (403), change a fee (403), claim for another courier or without counting the attempt (403), give an order back ahead of the queue (403), write another's position (403) or delete; a customer reads only their own, cannot insert as another, pre-assigned or with attempts (403), cannot move an order up the queue or cancel it claimable (403), take a cancelled one back (412), cancel another's or one carried; a customer's subscription hears their courier move on their order and nothing of another's |
| `grafana.test.ts` (7) | every panel of `grafana/plover.json` runs under Grafana's token with the built-ins bound; that token writes nothing and reaches no other company |
| `pages.test.ts` (5) | over the site `scripts/ci.sh` starts: sign-in with the email in another case, a wrong password, each role's pages, an order placed and followed, another customer's order a 404, a courier's claim, pickup, delivery and a second ack refused through the phone's routes, a write from another site refused |

## Measured

`npm run bench` on an Apple M1 Air (8 cores), two nodes and the router on
one machine, `--sync 250`, a company with no quota, everything through
the router as the app reaches it:

| What | p50 | p99 | Besides |
| --- | --- | --- | --- |
| An order sent, to a courier waiting at the server: 1 waiting | 0.49 ms | 3.1 ms | the nearest took it: 100% |
| the same, 10 waiting | 0.42 ms | 2.7 ms | the nearest took it 10%; the one who did a median of 2.0 km further |
| the same, 50 waiting | 0.46 ms | 11.0 ms | the nearest 2%; 2.6 km further |
| the same, 100 waiting | 0.64 ms | 14.6 ms | the nearest 1%; 2.5 km further |
| Claim the nearest, orders ready: 1 courier | 0.20 ms | 0.35 ms | one statement; the three steps it replaced 0.41 ms, 0.59 |
| the same, 16 couriers at once | 1.44 ms | 6.1 ms | the three steps 2.84 ms, 7.4 |
| the same, 50 couriers at once | 5.2 ms | 28 ms | the three steps 8.2 ms, 28 |
| The 10 nearest idle couriers: 60 on the map | 0.15 ms | 0.18 ms | `near loc $1 limit 10` |
| the same, 1 000 | 0.22 ms | 2.1 ms | |
| the same, 10 000 | 0.23 ms | 0.88 ms | |
| Who is within 2 km, by state: 60 | 0.12 ms | 0.16 ms | `distance(loc, $1) <= $2 group state` |
| the same, 1 000 | 0.26 ms | 2.1 ms | |
| the same, 10 000 | 0.91 ms | 1.8 ms | |
| Position beats: 1 phone | 0.11 ms | 0.22 ms | 8 650 a second |
| 8 phones at once | 0.34 ms | 1.9 ms | 18 100 a second |
| 32 phones | 1.5 ms | 4.8 ms | 18 100 a second |
| 64 phones | 3.1 ms | 8.6 ms | 18 300 a second |
| 100 positions a statement, the app's token, one writer | 0.70 ms | 1.2 ms | 137 000 positions a second |
| A beat beside 0 claims held | 0.12 ms | 0.28 ms | |
| beside 50 | 0.16 ms | 0.69 ms | |
| beside 150 | 0.29 ms | 1.6 ms | the claims wait on `orders`; the beat writes `positions` |

The machine was not idle: other builds ran beside it, the one-minute load
2.35 as it started and 3.41 as it ended. Runs at a load of 6 to 7 gave the
same medians for the waits and the reads but a claim among 50 couriers at
34 ms p50, the contention being the machine's as much as the database's.

- **A claim waiting at the server is handed an order in about half a
  millisecond**, from the insert being sent to the courier's answer, the
  router included: the node's look at the new row and the held `/batch`
  run under the write lock.
- **But not to the nearest courier.** With ten couriers waiting, the one
  nearest the pickup took the order one time in ten, and the one that did
  was a median of 2 km further; with a hundred, one time in a hundred:
  every waiting courier's claim is its own group (its point and its `sub`
  are in it), each group's head is woken by the insert, and whichever runs
  first takes it (gap 3).
- **Claiming the nearest is one statement**, half the three steps' time
  alone and still a third less among 50 couriers at once, who wait for
  each other's claims under the one writer. The claim rows were measured
  again on their own, the one statement and the three steps in turns over
  one cluster (one-minute load 2.9 to 3.5), when the claim became one
  statement: the full bench, run then, met other builds past a load of 18
  and its other rows are the ones from before.
- **The nearest couriers are a walk of `@geo`**: about 0.2 ms over HTTP
  whether 60 or 10 000 couriers are on the map; the radius count reads
  every position in it.
- **Positions**: one node takes 18 000 beats a second -- a position written
  over, a `/batch` under the courier's own token -- from 8 phones and more;
  the app gathering 100 positions a statement, under its own token, writes
  137 000 a second.
- **Couriers waiting cost every write**, whatever it writes: 150 claims held
  on `orders` took a position beat (which writes `positions`) from 0.12 to
  0.29 ms p50 through the router, and straight to the node, in a probe of
  its own, from 0.16 to 0.29 ms p50 and 0.67 to 4.3 ms p99 (gap 3).

From the tests, on the same machine:

- **Quotas keep neighbours apart.** Tandem's beats, 10 couriers every 50 ms:
  p50 0.92 to 1.24 ms alone and 0.67 to 1.05 ms beside Hopper's flood, no
  refusal; Hopper landed 156 to 158 beats in 3.2 s against its 40 a second,
  54 refused.
- **A lease lapses on time.** The killed courier's order went to the
  waiting one 1.51 s after its last beat, the lease being 1.5 s: the node
  works out when `run_at <= now()` next changes and runs the held claim
  then, with nothing written to wake it.

## Gaps found

What fenecdb made awkward, slow or impossible here, the most important
first, each with the statement that showed it and what Plover does
instead. None was fixed in the example's own change; 1, 2, 4, 5, 8, 9
and 13 are fixed since, and say how.

1. **Fixed: a claim could not pick the nearest row.** A job queue's claim
   picked its rows with `order` over fields; a courier's is "the nearest
   ready order":

   ```
   set orders {courier: $1, run_at: now() + 45000} where run_at <= now() near pickup $2 limit 1 returning *
   -- query error: position 68: unknown command `near`
   set orders {courier: $1, run_at: now() + 45000} where run_at <= now() order distance(pickup, $2) limit 1
   -- query error: position 89: expected `)`, found `,`
   ```

   Plover claimed in three steps: a read of the nearest candidates with
   the app's token, a `/batch` of one conditional `set` a candidate, and a
   held claim of the oldest within the radius. It was correct, and it cost
   two round trips where one would do, a write of the plan for every batch
   that lost its candidates (435 to 448 batches for 400 orders among 50
   couriers), and 50 couriers claiming at once waited 7.6 ms p50 for each
   other's batches where one alone took 0.4. Now a key of `order` may be any
   expression over the row, `order distance(pickup, $2)` among them, in a
   `get` and in a write that picks its rows; ascending over a field with
   `@geo` and a `limit`, it walks the point index to the page as `near`
   does. The claim is the one statement of Claiming the nearest order,
   above. A write still takes no `near`: the parser says to write the
   distance as a key of `order`.

2. **Fixed: a point inside an inner `get` was refused (a bug).** Which
   would have made the claim one statement, under the write lock:

   ```
   set orders {courier: $1} where id in (get orders select id where run_at <= now() near pickup $2 limit 1)
   -- a point is [lon, lat], a longitude from -180 to 180 and a latitude from -90 to 90; found [29.09,41.09]
   get orders where id in (get orders select id where distance(pickup, $1) < 5000)
   -- the same refusal; and with the point written as a literal, the same
   ```

   The point is valid; the message is wrong about why. A list of numbers is
   read into a vector's `f32`s by every reader without a schema, and read
   again as written where `Database::exactly` names it
   (`crates/fenec-core/src/engine.rs`, `exactly` and `exactly_for`): that
   walks the statement's own filter, its select list and each `lookup`
   level, but not an `Expr::InSelect`'s inner `get`, so the inner point
   stays a vector and `geo::point_of` refuses it. Now the walk reaches every
   place a statement holds a value, each collection by its own schema: an
   inner `get` at any depth, a `lookup` level, a disjunctive facet's
   filter, `having`, a comparison under a `case`, an `order` key. `set ...
   where id in (get orders select id where ... near pickup $p limit 1)` is
   a claim too, the inner `get` run first under the same lock.

3. **A new order goes to whichever waiting courier runs first, not the
   nearest; and every write wakes every waiting courier.** Each courier's
   held claim is a group of its own -- its point is in its filter, and its
   token's rule names `$jwt.sub`, a group being the filter with the scope
   bound in -- so "one awake in each group, the longest waiting first"
   becomes every courier woken by every insert, each looking under the
   read lock, the first to run taking it. Of 10 couriers waiting within
   reach, the one nearest the pickup was handed the order 8 to 13% of the
   time, of 50 2 to 4%, of 100 0 to 1%; the courier who took it was a
   median of 1.9 to 2.6 km further from the pickup than the nearest (`npm
   run bench`). A dispatcher would send the nearest. And `Waits::wrote`
   (`crates/fenec-http/src/waits.rs`) notifies every group's head on every
   write, of any collection: 150 couriers waiting on `orders` took a
   position beat, which writes `positions`, from 0.16 to 0.29 ms p50 and
   0.67 to 4.3 ms p99 straight to the node. Smallest: wake only the groups
   whose collections the write touched (the watcher knows them); for the
   dispatch itself, a way for a write to hand a row to the nearest of the
   held claims. Since gap 1, a courier's held claim at least takes the
   order nearest it of those ready when it runs.

4. **Fixed: `returning` under a scoped token picked only rows the token
   could read before the write -- silently.** With the natural policy -- a
   courier reads the orders they carry, and updates ready ones to claim
   them:

   ```
   -- courier's token; order 10714 is ready, courier null
   set orders {status: "assigned", courier: $1} where id = $2 returning *
   -- []                       (nothing written)
   set orders {status: "assigned", courier: $1} where id = $2
   -- {"affected":1}
   ```

   `Scope::returning` (`crates/fenec-http/src/access.rs`) ANDed the read
   rules into the write's filter, so they judged the row as it was, and
   Plover claimed without `returning`, reading the row back by its claim
   token in the same `/batch`. Now a `set`'s answer is held to the read
   rules as it wrote the row: their terms over a field the `set` writes
   are asked of the row after (`courier = $jwt.sub`, true once claimed),
   and a `set` that would leave a row its token may not read is refused
   whole (403) rather than written and left out of the answer. The claim
   above answers the order, claimed, and Plover's claims say `returning
   *`.

5. **Fixed: a rule could not say what a write may change a row from and
   to.** WITH CHECK held the row before and after to the same filter, so:

   ```
   -- customer: orders update(status, run_at) where customer = $jwt.sub and courier is null and (status = "ready" or status = "cancelled")
   set orders {status: "ready", run_at: now()} where id = $1     -- a cancelled order back in the queue: affected 1
   set orders {run_at: 0} where id = $1                          -- to the head of the queue: allowed by the same rule
   -- courier: orders update(...) where run_at <= now() or courier = $jwt.sub
   set orders {status: "delivered", courier: $1, delivered_at: now(), run_at: null} where id = $2   -- a ready order delivered unclaimed: affected 1
   ```

   Plover's routes never sent these, but the tokens could. Now a rule's
   filter names the row as the write leaves it with `new.`, as an upsert's
   `else set` does, and each step of an order is a rule of its own
   (`policy.txt`): the customer's cancel is `where customer = $jwt.sub and
   courier is null and status = "ready" and new.status = "cancelled" and
   new.run_at is null`, so the first statement above reaches no row (its
   order is not ready) and the second is 403; the courier's delivery is
   `where courier = $jwt.sub and (status = "assigned" or status =
   "picked") and new.status = "delivered" and new.run_at is null`, so the
   third is 403. `test/security.test.ts` sends all three.

6. **A rule cannot reach another collection, and reads have no field
   grants.** A customer may follow "the courier carrying my order" -- a
   row of `positions` -- which a rule's filter cannot say (it takes no
   inner `get`); Plover writes the courier's point onto the order with each
   beat (`courier_loc`), the customer's subscription to their own order
   carrying it, at the price of writing the order every 2.5 s. A courier
   should see a ready order's pickup to claim it but not the customer's
   door before they take it; a read rule gives whole rows, so the
   candidates are read with the app's token on the server.

7. **Quotas: a second's wait for a 2 ms token, and a write is a write
   whether it writes or not.** A 429 for `writes` answers `Retry-After: 1`
   and `"retry_after":1` -- whole seconds, at least one -- where the Pro
   plan's bucket of 600 a second refills a token in 1.7 ms: the claims test's
   50 couriers met 50 refusals and, doing as told, waited 50 s between
   them. The bucket is taken as a write request starts, so a claim that
   takes nothing costs one, and so does every held claim as it comes in --
   and again each time a write wakes it, whether it then writes or not:
   with 50 couriers waiting, the first few orders placed woke 400 to 650
   runs of held claims in 200 ms, draining Pro's 600 a second, and an order
   placed then was refused (429) one run in two or three of the claims
   test, which meters no writes now:
   45 claims sent to a Starter company at once were refused for `writes`
   (6 of them) before any for `streams`. And a held claim is a stream, so a
   plan's `streams` is how many couriers may wait at once (40 held, the
   41st to 45th 429). Smallest: `retry_after_ms` in the body beside the
   header's whole seconds; charge a held write's token when it runs.

8. **Fixed: the router answered 502 for a connection the node closed
   while idle (macOS).** A request through the router after a minute of
   quiet:

   ```
   502 {"error":"node `n1` (127.0.0.1:8091) did not answer: Invalid argument (os error 22)"}
   ```

   `Pool::send_held` (`crates/fenec-shard/src/upstream.rs`) retried a
   pooled connection the node closed while idle (`--idle-timeout`, 60 s)
   when the first read saw EOF, a broken pipe or a reset; on macOS
   `setsockopt` refuses a socket its peer closed with EINVAL, which
   `set_read_timeout` returned before anything was written --
   `InvalidInput`, not retried. Reproduced every time with a request, 65 s
   of silence, and the same request, and Plover's nodes ran with
   `--idle-timeout 0`. The one-line fix this said (`InvalidInput` among the
   retried kinds) would have kept a retry that was wrong besides: it sent a
   request again whenever its connection ended before an answer, which a
   node that ran a `put` and went down before answering looks like too --
   the `put` made twice. Now the router looks at a pooled connection before
   it writes (a read that does not wait) and lets go of one the node closed
   or reset; a request is sent again on a fresh connection when it failed
   before its last byte went out, and after it only when it is a read or
   carries an `Idempotency-Key`. Plover's nodes keep the default idle
   timeout.

9. **Fixed: an insert could not answer the id it made.** `insert orders
   {...} returning id` was `unknown command returning`, and `POST /orders`
   answered `{"inserted":1}`; Plover read the order back by its unique key
   in the same `/batch`. Now `put` and `insert` take `returning`, the rows
   written in the order of the documents, ids and all, held to a scoped
   token's read rules as written (an upsert's row set as an update), and
   `POST /orders?returning=id` answers them; `lib/orders.ts` places an
   order with `returning id`.

10. **Aggregates and `lookup` do not combine.** The busiest couriers with
    their names:

    ```
    get orders select courier, count(*) as delivered where delivered_at >= $1 group courier order delivered desc limit 10 lookup couriers on user = courier select name, code
    -- aggregates cannot be used together with `lookup`
    ```

    Insights reads the roster apart and joins in the page; the Grafana
    table shows courier ids.

11. **`bucket` has no time zone.** Merrow keeps UTC+3: a local day or hour is
    `bucket(t + 10800000, 86400000)`, an offset written into every
    statement, which cannot follow a zone with daylight saving.

12. **A row past its `@ttl` leaves every read at once, but a subscriber
    hears of it only when the sweeper deletes it**: a position written two minutes stale -- past its time a second later, and out of every read from then -- was heard deleted 73 s after it was written. The map
    keeps showing a courier whose phone went quiet; Plover's map drops a
    position whose `seen` is two minutes old itself.

13. **Fixed: a `nocase` clash named the spelling written, not the one
    stored:**
    ``` `users.email` is unique, and document 1 holds "SAMI.lindqvist@tandem.example" already ```,
    where document 1 holds `sami.lindqvist@tandem.example`. Now the clash
    reads the value from the row holding it; a scoped token is still told
    the field alone.

14. **A node's streams default to 64 for all its tenants together**
    (`--http-max-streams`): a dispatcher's map is two subscriptions and two
    more for its live queries, a customer following an order one. Plover's
    nodes take 2000; each is a thread, so a node's thousands of customers
    following orders want `live` with `{ poll }` rather than a stream each.

## Files

| Path | What |
| --- | --- |
| `policy.txt` | every rule a token is held to |
| `lib/schema.ts` | the platform's and a company's collections, declared in code |
| `lib/dispatch.ts` | the queue: claim the nearest, beat, pick up, deliver, give back, dead letter |
| `lib/orders.ts` | placing and cancelling an order as a customer |
| `lib/city.ts` | Merrow: its streets, addresses, routes, and the map's coordinates |
| `lib/world.ts`, `lib/setup.ts` | the companies, people and merchants; the tenants placed, filled and given their quota |
| `lib/sim.ts` | customers ordering in the day's waves, couriers riding the streets |
| `lib/session.ts`, `lib/tokens.ts`, `lib/passwords.ts` | the session cookie, tokens, scrypt |
| `lib/cluster.ts` | two nodes and the router, for development and the tests |
| `lib/fetch.ts` | `fetch` over `node:http` for the server side |
| `app/`, `components/` | the pages: sign-in, the live map, Insights, the courier's phone, the customer's orders and tracking |
| `grafana/` | the dashboard, its provisioning and a compose file |
| `scripts/` | `cluster`, `setup`, `simulate`, `bench`, `shots`, `grafana-token`, `ci.sh` |
| `test/` | the tests and their harness |
