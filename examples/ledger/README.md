# Quire: a payments ledger on fenecdb

A real-world example of money moving through fenecdb. Quire is a
double-entry ledger with an operator console. It has:

- accounts in several currencies, balances in integer minor units, open or
  frozen, held alone or jointly;
- transfers that land whole or not at all, made once however often they are
  sent;
- an append-only journal that the ledger's own server cannot rewrite;
- refunds and reversals as new movements, never edits, and never more than
  was paid, however many race;
- holds that reserve money, then are captured, released or lapse, and give
  the money back exactly once;
- reconciliation of balances against the journal at one moment;
- the journal streamed to a sink outside the database, at least once, with a
  deduplication key;
- customers who see only their own money, an operator role, tenant-bound
  tokens, an audit log and a rate limit on transfers.

Its claims are held by tests: 16 clients making 20 000 random operations in
process and over HTTP, a server killed with `kill -9` under the load, the
security of every route, a sealed backup restored. Its numbers are measured.

The stack:

- TypeScript on Node, with a plain `node:http` server rendering the console
  as strings;
- `fenec-server` as the only database, a tenant node (`--dir`) with one file
  per tenant;
- the engine in process (`@fenecdb/web`'s WebAssembly module) for the same
  tests without a server.

Why no framework: the console is four pages of tables and forms, and the
substance is the server and its tests. A framework would add a runtime and
a build step to every page and nothing the ledger needs. No script runs in
the browser: forms post and redirect, links get.

Every person, account and amount is invented.

## Architecture

```
 browser                 ledger server (src/server.ts)                     fenec-server --dir (tenant node)
 ───────                 ─────────────────────────────                     ────────────────────────────────
 console pages ◀──────── every page read under the signed-in    ─────────▶ /t/acme/  accounts, journal,
 (forms, no JS)          person's own JWT:                                            transfers, holds,
                           customer  {sub, tenant, accounts: [...]}                   limits, events, users
                           operator  {sub, tenant, role: admin}            policy.txt: rules per token
 POST /transfers ──────▶ the person's token reads the source,  ─────────▶  (USING and WITH CHECK)
 POST /api/...           then one /batch under the app's JWT,              journal, events: append-only
                         {sub, tenant, role: app},
                         with an Idempotency-Key
                                                                          /t/globex/  another tenant's file
 operator jobs (the node's own token, never the console's)
   scripts/sink.ts   GET /t/acme/_changes  ───────────────────────────▶  the journal, at least once,
                     → data/journal-sink.ndjson (dedup by entry id)       into a file outside
   fenec backup --key-file  → a sealed backup
   reconciliation: one /batch of reads (a snapshot), from the console
                     under the operator's admin JWT
```

Four kinds of credential, each with one job:

| Who | Credential | May |
| --- | --- | --- |
| A customer | a JWT minted per request: `sub`, `tenant`, and `accounts`, the list of accounts they hold (joint ones included), looked up in `accounts.holders` | read their accounts, the movements and entries touching them, their holds. Write nothing |
| An operator | a JWT with `role: admin` | read everything in the tenant. Write nothing directly: freezing, opening and depositing go through the ledger, which records them in `events` |
| The ledger's server | a JWT with `role: app` | move money: update balances and counters, insert entries, movements, holds and events, insert an account only at zero. Never update or delete an entry or an event, never delete an account, never change the schema |
| The node's operator | `--http-token`, `--admin-token`, `--replication-token` | everything: tenants, the schema, corrections, the change stream, backups. The ledger's server never holds these |

## Run it

```sh
cargo build --release -p fenec-server -p fenec-cli   # from the repository's root
cd examples/ledger
npm install
npm run db        # fenec-server --dir data/tenants on :8080, --sync always, policy.txt, the audit log
npm run setup     # tenants acme and globex, the schema, a few people and accounts
npm start         # the console: http://127.0.0.1:3000
npm run sink      # the journal into data/journal-sink.ndjson, until stopped
```

Sign in as `ops` (an operator) or `ada`, `ben`, `cleo`, `dev` (customers;
Ada and Ben share a household account). The password is `ledger-demo`.

| Variable | Default | For |
| --- | --- | --- |
| `FENEC_URL` | `http://127.0.0.1:8080` | the tenant node |
| `LEDGER_TENANT` | `acme` | the tenant the console serves |
| `FENEC_JWT_SECRET` | a dev value | signs the JWTs the node checks |
| `FENEC_TOKEN`, `FENEC_ADMIN_TOKEN`, `FENEC_REPLICATION_TOKEN` | dev values | the node's own tokens, for setup, the sink and backups |
| `LEDGER_SESSION_SECRET` | a dev value | signs the console's session cookie |
| `LEDGER_RATE_LIMIT` | `30` | transfers an account may make a minute |
| `LEDGER_SINK_FILE` | `data/journal-sink.ndjson` | where the sink writes |

The defaults are for development only; set every secret in a deployment, and
put TLS in front of both servers.

## Tests and measurements

```sh
npm run ci              # a node over a new directory, setup, lint, every test
npm test                # the tests alone, against a running node (npm run db && npm run setup)
npm run bench           # transfers a second and their latency (servers of its own)
npm run recon-bench     # reconciliation at a million entries (a server of its own)
```

From the repository's root, `examples/run-tests.sh ledger` runs `npm run ci`
on this checkout's packages and binaries. CI's `examples (ledger)` job runs
it against a release `fenec-server`.

| File | What it holds |
| --- | --- |
| `test/invariants.test.ts` | 16 clients, 20 000 random operations, in process and over HTTP (below), then every invariant |
| `test/crash.test.ts` | `kill -9` under the load, `--sync always`, a restart over the same files |
| `test/security.test.ts` | every route a customer, the app and an operator could misuse; tokens forged, expired, unexpiring, unsigned and tampered; tenants; the rate limit |
| `test/sink.test.ts` | the change stream into the sink through a crash before the commit, a lost file, and its lag |
| `test/backup.test.ts` | a sealed backup: nothing readable, a changed byte refused, restored into balanced books |

## The invariants, and how each is enforced

Every movement of money is one block of FenecQL statements sent as one
`/batch` (`src/ledger.ts`). A transfer:

```
put limits {account: $1, n: 0, at: now()} if absent                         -- the rate window
set limits {n: n + 1} where account = $1 and n < $10 require 1              -- 429 past the limit
get accounts select ext where ext = $2 and status = "open"
    and currency = $4 and kind = "customer" limit 1 require 1                  -- the recipient
get accounts select ext where ext = $1 and status = "open"
    and currency = $4 and kind = "customer" limit 1 require 1                  -- the source
set accounts {balance: balance - $3} where ext = $1 and balance - held >= $3 require 1
set accounts {balance: balance + $3} where ext = $2 require 1
get accounts select ext where ext = $1 and holders has $11 limit 1 require 1   -- a customer's own
insert journal [{entry: $8, tx: $5, account: $1, amount: 0 - $3, ...},
                {entry: $9, tx: $5, account: $2, amount: $3, ...}]
insert transfers {ref: $5, kind: "transfer", src: $1, dst: $2, amount: $3, refunded: 0, ...}
```

There is one writer, and a `/batch` holds the write lock from its first
statement to its last: nothing comes between a guard and the write it
guards. A `require 1` that is not met stops the batch with 412 and `"at"`,
the statement that stopped it, and every statement before it is put back.
The ledger names the refusal from `at`: rate limited, recipient, source,
funds, not the holder.

| Invariant | Enforced by | Held by |
| --- | --- | --- |
| The money in each currency is constant | Every movement is a debit and a credit of one amount in one block; money enters only from the currency's outside account (`world:eur`), which goes negative by what is in the ledger, so the balances of a currency sum to zero | `check`: the sum of customer balances per currency equals what was deposited, and with the outside account it is zero |
| No customer balance goes below zero | `balance - held >= $3` in the debit's `where`, `require 1` | no `balance < 0`, `held < 0` or `balance - held < 0` |
| Journal = balances | Both entries land in the block that moves the balances, or nothing does | per account, the balance is the sum of its entries; per currency, both sides; per movement, its two entries sum to zero |
| A movement to a missing, frozen or other-currency account moves nothing | `get accounts ... require 1` guards that read without writing | refused as `recipient`/`source`, and the sums above |
| Each idempotency key makes one movement | `Idempotency-Key` on the `/batch`: the key and its answer land in the same block as the movement, and a retry, even one sent at the same moment, is answered with the first answer. Behind it, `transfers.ref` is `@unique`, and the ref is derived from the key | per key, at most one answer that made it; the movements in the ledger are exactly those acknowledged |
| Refunds never exceed the original | The refund's first statement is a counter and its guard: `set transfers {refunded: refunded + $3} where ref = $10 and refunded + $3 <= amount require 1` | four refunds of one payment, each up to 60% of it, raced again and again: per payment, `refunded` is the sum of its refunds and never past `amount` |
| A hold reserves money it has; capture, release and lapse happen once | `held` grows under the debit's guard; the hold's `state` moves from `"held"` in a guarded `set` (`state = "held" and until > now()` to capture, `until <= now()` to lapse) | per account, `held` is the sum of its live holds; every hold ended exactly once (captures, releases and lapses counted per hold); captures racing releases and the reaper |
| The journal and events are never edited | `journal append-only` and `events append-only` in `policy.txt`: no JWT updates or deletes there, whatever its role | 403 on every path, below |
| A customer spends only from an account they hold | The console reads the source under the customer's own token (their `accounts` list claim), and the block requires `holders has $11` again under the lock | below |
| Transfers per account per minute are limited | The counter recipe: a window's row made by `put ... if absent`, expiring a minute later (`@ttl`), counted under a guard. The window lives in the row, not its key, so a retry a minute later is the same request and its key still matches | below |

A refused transfer is put back whole, its window count included, so the
limit counts transfers made, not attempts.

**Holds are `@sorted`, not `@ttl`.** A row past its `@ttl` leaves every
read at once and the sweeper deletes it, so a hold that lapsed could no
longer be found to give its money back: the `held` counter would keep it
for ever. A hold keeps `until` under `@sorted`, and the reaper (every 5 s
in the console's server) releases those past it,
each in a block guarded by `state = "held" and until <= now()`. A capture
checks `until > now()` in its own guard, so a lapsed hold cannot be
captured even before the reaper comes by.

### Under concurrency: 20 000 operations, 16 clients

`test/invariants.test.ts` runs the same mix (`test/workload.ts`) twice:
through the engine in this process (`LocalStore`) and through
`fenec-server` over HTTP (`HttpStore`), 40 open accounts in two currencies
and 4 frozen ones. Of every operation:

- 55% transfers: 4% from a frozen account, 4% to a missing one, 4% to a
  frozen one, 4% to another currency, and 5% for more than the account
  holds;
- 10% retries of an earlier transfer with its key, a third of them sent
  twice at once;
- 10% four refunds of one payment, raced;
- 22% holds: captured, released, captured and released at once, or left to
  lapse with a reaper running beside the load;
- 3% an account frozen and unfrozen while transfers run.

Then `check` (`test/workload.ts`): reconciliation finds no difference, no
balance below zero, every movement's entries sum to zero, the money per
currency is what was deposited, no key made two movements, the ledger holds
exactly the movements acknowledged, every refund counter is its refunds'
sum and within its payment, no hold is left held and each ended once.

Both pass. In process the module has one thread, so the 16 clients are
interleaved whole blocks; over HTTP they are 16 connections, the blocks
serialised by the server's write lock. With a guard removed, the check
fails: without `refunded + $3 <= amount`, 3 000 operations in process
refunded a payment past its amount.

### Crash safety

`test/crash.test.ts` starts a node of its own under `--sync always`, runs
the mix from 16 clients, and kills the server with SIGKILL once 1 500
movements have been acknowledged. Started again over the same files:

- every acknowledged movement is there, with both its entries;
- 200 acknowledged keys sent again are answered `Idempotent-Replayed`: the
  key landed in the block with the movement;
- the transfers in flight when it died (5 in one run) are sent again with
  their keys: one that had landed is replayed, one that had not is made
  now, and none twice;
- holds the crash left held lapse, and the reaper gives them back;
- every invariant holds.

A failover to a replica is not tested: the example runs no replica.
Replication ships only what is on disk, but a replica follows a primary
asynchronously, so a write acknowledged a moment before the primary died may
not have reached it (`site/content/docs/replication.html`).

## Security

`test/security.test.ts`, against the node, with the token an attacker would
hold:

- **A customer** reads only their accounts, the movements and entries
  touching them and their holds, joint accounts included, and nothing
  through a filter naming another's account, a count or a sum. `users`,
  `events` and `limits` do not exist for them (404). Every write by
  `/query`, `/batch` and REST is 403, and the change stream is refused.
  Through the console, another holder's account is 404 to read, spend from,
  refund from or freeze, and an operator's action is 403.
- **The block checks who asks.** A customer's transfer and refund carry
  `holders has $11` as a guard inside the block: had the console skipped its
  own check, the block would still refuse (`not_holder`).
- **The app's token cannot rewrite history.** `set` and `del` on the
  journal or the events, by `/query`, `/batch` (a write after a read in the
  same batch too), `PATCH` and `DELETE`; a `put` naming an entry's id;
  `drop`, `alter` and `compact`; `/_schema/apply`: each 403. It cannot open
  an account holding money, open an outside account, or delete one.
- **What it can do is caught.** The app's token can still edit a balance:
  grants are per collection, not per field (Gaps). A test does exactly that,
  and reconciliation reports the account, its balance and the sum of its
  entries. The journal, which no JWT can edit, is the truth to correct it
  from, with the node's own token.
- **An operator's admin token** reads everything and writes nothing.
- **Tokens:** forged (another secret), expired, with no `exp`, unsigned
  (`alg: none`) and tampered (a list claim widened): each 401, logged in the
  audit log, and slowed by the server's growing delay.
- **Tenants:** the app's, an operator's and a customer's token for `acme`
  are 403 on every route of `globex`, and one naming no tenant on any. A
  tenant is a file of its own, so ids, the change stream and the books are
  per tenant.
- **The console** refuses a write from another origin, a forged session
  cookie and no session; its cookie is `HttpOnly` and `SameSite=Lax`, its
  pages carry a strict content security policy and run no script.
- **Rate limit:** of 12 transfers from one account sent at once with a limit
  of 5, exactly 5 land and 7 are refused, another account is not held back,
  and the window counts 5. Through the console the sixth in a minute is 429.

## Reconciliation, and what "one moment" means here

fenecdb has one writer and no snapshots of its own. A read takes the shared
lock, so one statement sees one state; but balances and entries are two
collections, and a write may land between two reads. So `src/reconcile.ts`
sends its four reads as one `/batch`. A batch runs under the write lock, of
reads too, so no write lands between the first read and the last, and the
answer's `Fenec-Seq` names the change the books stood at. The price is that
transfers wait for it: at a million entries the snapshot takes 250 ms, and
a transfer arriving meanwhile waits up to that long (below).

The other way is the change stream. `/_changes` hands out every write on
disk, numbered by the change counter, documents and all. A consumer that
applies them in order holds the books as they stood at each `seq`, and can
reconcile there without taking any lock. The sink (`src/sink.ts`) keeps the
journal's half: an entry per line, its `seq` beside it.

### The journal sink

`npm run sink` reads the tenant's `/_changes` and appends the journal's
entries to a file, fsyncing it before it moves the consumer the server
keeps for it (`/_changes/consumers/journal-sink`). Delivery is at least
once: a crash between the sync and the commit hands the same writes over
again. The entry's id (`<ref>:dr`, `<ref>:cr`, `@unique` in the journal) is
the deduplication key, so the file holds each entry once. The test kills a
sink between its sync and its commit under load, starts another over the
same file, and holds the file to the journal entry for entry, with no line
twice and every movement summing to zero. A sink whose file is lost copies
the journal again, in one snapshot, before streaming on. The console's
reconciliation page shows the file against the journal.

## Measured

On an Apple M1 laptop with 8 cores, which other agents were using at the
same time (load average 2 to 3), with this checkout's release
`fenec-server`. Each figure is one run; take them as indicative.

### Transfers

Each is the whole block above, sent as one `/batch` with an
`Idempotency-Key`, between 64 accounts, from 1 or 16 clients for 10 s
(`npm run bench`):

| sync | clients | transfers/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| `--sync always` | 1 | 217 | 4.90 ms | 7.50 ms |
| `--sync always` | 16 | 2 239 | 6.97 ms | 13.88 ms |
| `--sync 250` | 1 | 5 837 | 0.15 ms | 0.48 ms |
| `--sync 250` | 16 | 9 258 | 1.50 ms | 5.32 ms |

Under `--sync always` a transfer is answered once it is on disk, and macOS's
`F_FULLFSYNC` takes about 4 ms: one client is one fsync a transfer. Sixteen
share their fsyncs (the group commit) and reach ten times the rate. Under
`--sync 250` a crash can lose the last 250 ms of acknowledged transfers.

### Reconciliation at a million entries

500 000 transfers made through the ledger (16 clients, `--sync 250`:
9 159 a second), 1 002 000 entries over 1 003 accounts in three
currencies (`npm run recon-bench`):

| | |
| --- | --- |
| the reconciliation's snapshot: accounts, entries by account, live holds, refunds past their amount | 247 to 323 ms |
| every movement's two entries summed (`group tx`, 501 000 groups) | 686 ms |
| transfers from 4 clients, alone | 8 487/s, p99 3.28 ms, the longest 6.7 ms |
| the same beside a reconciliation every second | 6 217/s, p99 3.34 ms, the longest 350 ms |

The snapshot holds the write lock, so the transfers that arrive during it
wait for it: the p99 does not move, the longest wait is the snapshot's
length.

### The change stream

From a transfer's answer to its entries synced into the sink's file:
p50 3.9 to 4.5 ms, p99 10.9 to 14.7 ms over 300 transfers under
`--sync always` (`test/sink.test.ts` prints it). The sink commits its place
at most every 250 ms: a commit is a durable write of its own, and made after
every page it put 13 ms between a transfer and its line at the median.

## What each part uses

| Part | fenecdb |
| --- | --- |
| Accounts | `ext text @unique` (the external id), `currency`, `status`, `kind` `@hash`; balances and holds as `int` minor units; `holders [text]` read with `has` |
| A transfer | one `/batch` with an `Idempotency-Key`; `set ... require 1` on the debit and the credit; `get ... require 1` guards that read without writing; expressions in `set` (`balance - $3`, `refunded + $3`); `transfers.ref @unique` |
| The journal | `entry text @unique` (the sink's deduplication key), `tx`, `account` `@hash`, `at @sorted`; `journal append-only` in the policy |
| Refunds and reversals | a counter and its guard on the original (`refunded + $3 <= amount require 1`) |
| Holds | the account's `held` counter under the debit's guard; `until timestamp @sorted` and a reaper; state moves guarded by `require 1` |
| Rate limit | the counter recipe: `put limits {...} if absent` with `at timestamp @ttl(1m)`, then `set limits {n: n + 1} where ... and n < $10 require 1` |
| Security | per-operation grants (`read`, `insert`, `update`), roles (`for app`, `for admin`), a list claim (`ext in $jwt.accounts`), `append-only`, tenant-bound tokens on a `--dir` node, `exp` required, `--audit` |
| Reconciliation | a `/batch` of reads under one lock, its `Fenec-Seq`; `group` with `sum` and `count(*)` |
| The sink | `/_changes` with `wait`, a server-kept consumer (`/_changes/consumers/<name>`), `410` handled by a snapshot copy |
| Backups | `fenec key`, `fenec backup <node>/t/<tenant> --key-file` (sealed with ChaCha20-Poly1305), `fenec restore`; `fenec archive --key-file` keeps every write the same way |
| Schema | `schema.fenecql`, applied with `POST /t/<tenant>/_schema/apply` and handed to `Fenec.open` in process |
| Tenants | `fenec-server --dir`, `PUT /_admin/tenants/<t>`, a file each |

## Limits, and honest notes

- **No foreign exchange.** A transfer moves one amount in one currency;
  accounts of two currencies cannot pay each other. FX would be two
  movements through an exchange account per currency, at a rate recorded in
  the movement, and reconciliation per currency would hold as it does.
- **Every currency here has two decimals.** One with none or three would
  carry its exponent beside its code.
- **The balance is a counter; the journal is the record.** The two are
  written in one block, so they agree whenever a block lands. A token that
  can update balances can make them disagree, which reconciliation reports.
- **A compromised ledger server can still move money.** It holds the token
  that moves money, so it can append balanced movements of its own. It
  cannot hide them: the journal and the events are append-only to it, and
  the sink has copied them out of its reach.
- **The reconciliation's snapshot stops writes while it reads** (250 ms at
  a million entries). A ledger much larger would reconcile from the change
  stream, or in parts, account ranges at a time.
- **Holds lapse when the reaper comes by**, every 5 s, not at `until`; a
  lapsed hold cannot be captured meanwhile, but its money is reserved until
  then.
- **The rate limit counts transfers made.** A refused transfer is put back
  with its count, so a client probing with transfers that fail is not
  slowed by it; the node's growing delay slows only refused tokens.
- **The demo's sign-in is the console's own**: names and scrypt hashes in
  `users`. A deployment would take an identity provider's tokens
  (`--jwt-keys`) and map its subjects to holders.
- **In process** there is no `Idempotency-Key`: the `@unique` ref is what
  tells a retry it landed (`duplicate`).

## Gaps this example hit

Each with the statement that showed it and the smallest feature that would
close it.

1. **A `/batch` of reads takes the write lock.** `handle_batch` takes
   `held::write` whatever the statements, so the reconciliation's four
   reads hold writers out for 250 ms at a million entries, and readers too.
   Smallest: take the read lock when every statement only reads, as the
   native library already does (`fenec_abi::read_only`); readers would go on
   beside it. Writers waiting is the price of one writer and no snapshot.
2. **A consumer's commit wakes its own reads.** `GET
   /t/acme/_changes?consumer=journal-sink&wait=1000` answered at once, with
   no lines and `Fenec-Next` past its place, after every
   `POST /_changes/consumers/journal-sink`: the commit is a write to
   `_consumers`, which the stream leaves out but counts. A sink that
   committed every answer committed in a loop, 30 000 fsynced writes in a
   few idle minutes. The sink reads by its own cursor (`since=`) and commits
   only pages that held writes. Smallest: a read by `consumer=` starts past
   the `_consumers` writes after its place, so it waits.
3. **Grants are per collection, not per field.**
   `set accounts {balance: balance + 500000} where ext = "dev-main"` under
   the app's token is 200: `update` on `accounts` reaches every field, and
   `WITH CHECK` tests only the row after. Smallest: fields a grant may
   change, `accounts update(balance, held, status) for app`, any other
   field refused (403); or a check over the row before and after
   (`where new.kind = old.kind`).
4. **`@ttl` cannot give anything back.** A hold whose `until` were `@ttl`
   would leave every read when it lapsed, and its money would stay in
   `held`. Smallest: a read of the rows past their time for a reaper
   (`get holds expired where ...`), so the sweep's work can be done once
   by the application before the row goes. (The shop met the same with
   stock.)
5. **The module's `run` of several statements does not say which one
   stopped it.** In process a refused transfer's error names `set accounts`,
   and the debit and the credit are both that. `LocalStore` runs the
   prefixes again, each ended by a statement that always fails, to find it.
   Smallest: `"at"` in the module's error, which `fenec_abi::execute`
   already knows.
6. **No text concatenation.** `insert journal {entry: $5 + ":dr", ...}` is
   a type error, so each block's entry ids travel as two more parameters.
   Smallest: `+` over two texts, or `concat(...)`.

`/batch` with a key goes through `@fenecdb/web/client`'s `db.batch(...,
{ idempotencyKey })` (`src/store.ts`), whose `FenecError` carries the
refusal's `status` and `at`. The change stream has no client method, so the
sink reads `/_changes` with a `fetch` of its own.

## Files

```
schema.fenecql      the collections
policy.txt          what each token may read and write
src/
  ledger.ts         every movement as one block of statements
  store.ts          where they run: fenec-server over HTTP, or the engine in process
  reconcile.ts      the snapshot and its checks
  sink.ts           the change stream into a file, deduplicated
  server.ts         the console and the JSON API
  views.ts          the console's pages
  tokens.ts         JWTs for the app, operators and customers; the session cookie
  setup.ts          tenants, the schema, outside accounts, the demo's people
  users.ts, money.ts, config.ts
scripts/            db.sh, setup.ts, sink.ts, ci.sh, bench.ts, recon-bench.ts
test/               invariants, crash, security, sink, backup; workload.ts is the mix and its checks
```
