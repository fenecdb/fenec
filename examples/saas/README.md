# Trellis: a multi-tenant team workspace on fenecdb

A real-world example of authentication and tenancy on fenecdb. Trellis is a
small team workspace -- organisations, teams, people in four roles, task
boards, comments, invitations -- with the parts a product like it has to
get right:

- sign-up and sign-in with an email and a password the app hashes (scrypt),
  emails made one before the unique index compares them, the same answer
  whether or not an email has an account, and rate limits per address and
  per account that hold under concurrency;
- sessions as five-minute access tokens and refresh tokens that rotate on
  every use, where a stolen copy used once ends the whole session;
- email verification and password reset links that work once and lapse;
- single sign-on through an OpenID Connect provider (a mock one runs beside
  the app), its ID tokens verified with the provider's published RS256 keys;
- a tenant per organisation, a file of its own on one of three nodes
  behind `fenec-shard`, each with a replica on another node;
- roles and teams that fenec-server enforces on every request a person's
  own token makes, straight from the browser: a guest reads and comments,
  a member edits tasks but not the team they belong to, an admin manages
  people and teams;
- live boards over subscriptions and search with `match`, both held to the
  person's teams, BM25's statistics included;
- append-only security and audit logs that nobody's token can rewrite;
- an organisation moved between nodes while people keep writing, and a
  node killed under them.

Its claims are held by tests: every token attack the server and the app
should refuse, a role matrix of 29 operations by five kinds of person
against the router, scoped streams and search, a move and a failover under
eight writers with every answered write checked. Its numbers are measured.

Every person, company and address in it is invented.

## The stack, and why

- TypeScript on Node, with a plain `node:http` server and no framework.
  The server's job is small on purpose: it holds the signing key and the
  credentials, and everything else -- every read and write of an
  organisation's data -- goes to fenecdb under a scoped token. Next.js
  would have put server actions between the page and the database, which
  hides the one thing this example shows: the browser holding its own
  token, and the database deciding what it may do.
- The page is one ES module (`public/app.js`), served as written, drawn
  with DOM calls (never `innerHTML` over data) under a strict CSP. It reads
  and writes through `@fenecdb/web/client`, served from `node_modules`.
- `fenec-server --dir` on three nodes and `fenec-shard --replicas
  --auto-failover` in front: a tenant per organisation and one for
  accounts, each with a replica on another node, failed over by the router
  on a lease.

## Architecture

```
 browser                        Trellis (src/)                          fenec-shard :8090        fenec-server --dir
 ───────                        ──────────────                          ─────────────────        ───────────────────
 sign-in form ── POST /api/auth/signin ─▶ rate limits + the account's  ─┐                     n1 :8091  accounts (+replica on n2)
                                         row in one /batch, scrypt      │  /t/accounts/*  ──▶ n2 :8092  o-lumen   (+replica on n3)
                ◀── refresh cookie (HttpOnly, SameSite=Strict,          │  under the app's    n3 :8093  o-acme    (+replica on n1)
                    Path=/api/auth), API token                          │  JWT, role app      each: --jwt-keys (public key only),
                                                                        │                     --policy policy.txt, --sync always,
 board ──────── POST /api/auth/refresh {org} ─▶ rotate the refresh     ─┘                     --audit, --lease
                ◀── access token, 5 min:          token (conditional    
                    {sub, tenant: o-lumen,        set ... require 1),   
                     role: [member, guest],       read `members` in     
                     teams: [t_…], exp}           o-lumen, sign RS256   
                                                                        
 board ──────── /db/t/o-lumen/... ──────────── a pipe, the request ────────▶ /t/o-lumen/... ──▶ the node checks the token:
                (Authorization: the access        as it came                                    signature (RS256, kid), exp,
                 token; queries, /batch with                                                    tenant claim = o-lumen,
                 Idempotency-Key, subscriptions)                                                then policy.txt per row and
                                                                                                field (USING + WITH CHECK)
 people page ── POST /api/orgs/lumen/invites ─▶ the admin's own token  ─────────────────────▶ invites, audit: the policy
                (Authorization: access token)    runs the /batch, the app                      decides who may invite whom
                                                 mails the code
 new org ────── POST /api/orgs ──────────────▶ slug in accounts (@unique), PUT /_shard/tenants/o-<slug> (router token),
                                                 schema applied (operator token), first owner and team (app token)
```

Tokens, each with one job:

| Who | Credential | May |
| --- | --- | --- |
| A person, in an organisation | an access token: `sub`, `tenant` (the organisation's), `role` (their role and every role below it), `teams` (a list claim), `exp` five minutes on | what policy.txt grants their role, over the rows of their teams: from the browser, straight to the database |
| A person, signed in | a refresh token, 32 random bytes in an HttpOnly cookie, stored as its SHA-256; and an API token naming no tenant | get access tokens; call this app's API. fenec-server refuses the API token everywhere (403: no tenant) |
| This app | its own access token per tenant (`role: app`) | the auth flows: accounts, sessions, links, limits, the first owner of an organisation, invitations taken up. Never rewrite a log |
| This app, provisioning | the router's token and the nodes' operator token | place a new organisation's tenant and apply its schema. No request handler uses either |
| The operator | `--http-token`, `--admin-token`, the router's token | everything, including corrections to append-only logs |

The signing key is RSA, made once into `data/keys/` and held by the app
alone; the nodes read only the public half (`--jwt-keys app.jwks.json`).
A node, its disk or a backup cannot mint a token.

## Run it

```sh
cargo build --release -p fenec-server -p fenec-shard   # from the repository's root
cd examples/saas
npm install
npm install --no-save ../../web   # this checkout's client: 0.1.10 on npm has no db.batch yet
npm run dev      # three nodes and the router over data/dev, Trellis on :3000,
                 # the mock identity provider on :3001, the demo organisation seeded
```

Sign in at http://127.0.0.1:3000 as `ines@lumen.studio` with `trellis demo
password` (the owner), or `theo@` (admin), `mara@`, `dev@`, `sol@` (members)
and `jun@client.example` (a guest), all `@lumen.studio` but Jun. What the
app would mail -- confirmation links, invitations, resets -- is at
http://127.0.0.1:3000/dev/mail. `npm run cluster` and `npm start` run the
database and the app apart; `npm run shots` takes the screenshots of a
running `npm run dev`.

```sh
npm test         # every test file starts a cluster of its own
npm run lint     # tsc and eslint
npm run bench    # the measurements below
```

From the repository's root, `examples/run-tests.sh saas` runs `scripts/ci.sh`
on this checkout's packages and binaries; CI's `examples (saas)` job runs
it against release builds of `fenec-server` and `fenec-shard`.

## What fenecdb does here

| Part | fenecdb feature |
| --- | --- |
| One account per email | `users.email text @unique`, after the app normalises the email (NFC, trim, lowercase): `@unique` compares bytes |
| Sign-up that tells nobody whether an email exists | `put users {...} if absent`: affected 0 for a taken email, no 409 to tell apart, and the inbox told instead |
| Rate limits per address and per account | the counter recipe: `put limits {key, n: 0, at: now()} if absent`, then `set limits {n: n + 1} where key = $1 and n < $2 require 1`, both limits and the account's row in one `/batch`; `limits.at @ttl(10m)` ends a window, and an expired row stops holding its unique key at once |
| Refresh rotation and reuse detection | `set refresh {used: true} where hash = $1 and used = false and revoked = false require 1` beside the insert of the next token, one block: two racing requests cannot both rotate one token; a used one coming back revokes its `family` |
| Sessions, links and invitations that lapse | `@ttl(14d)`, `@ttl(30m)`, `@ttl(24h)`, `@ttl(72h)`: a row past its time is out of every read and delete at once, the sweeper removes it later |
| Links that work once | `del resets where hash = $1 require 1` in the block that sets the password and revokes every session |
| Invitations used once | `insert redemptions {invite: $1, ...} if absent require 1`: the second use writes nothing and its block -- the member, the audit row -- is put back |
| Tenant per organisation | `fenec-server --dir`, `fenec-shard`; the tenant comes from the path, never the query |
| A token for one organisation only | the `tenant` claim: a token reaches `/t/<t>/` only when it names `<t>` (403 elsewhere, by every route) |
| Tokens the nodes cannot mint | `--jwt-keys` with the app's RSA public key; `exp` required (the default) |
| Roles | rules `for <role>` and a role claim that is a list holding the roles below it |
| Teams | `where team in $jwt.teams`: a list claim, on reads, writes (WITH CHECK) and subscriptions |
| A member edits tasks but not their team | `tasks update(title, body, status, assignee, priority, due, project, updated) ... for member`: a field grant, judged by the fields a write changes, by every route |
| Comments only as yourself | `comments insert,update(body, edited) where team in $jwt.teams and author = $jwt.sub`: the author is pinned to the token |
| Admins cannot touch owners | `members update(role, teams),delete where role != "owner" for admin`: the filter holds the row before and after |
| Logs nobody rewrites | `audit append-only`, `security_log append-only`: no JWT updates or deletes, the app's included |
| Live boards | `GET /t/<t>/tasks/changes?team=eq.<key>` with the person's token: a seed and every change, held to `teams` |
| Search | `match` with `highlight()` and `snippet()`; BM25 over the rows the token may read |
| Writes that land once | `db.batch([...], { idempotencyKey })`, retried with the same key after a 503 or a dropped connection |
| Moving an organisation | `POST /_shard/tenants/<t>/move`: writes get 503 with `Retry-After` for the moment of the copy |
| Losing a node | `--replicas`, `--auto-failover <s>` and nodes with `--lease`; `--sync always` |
| Refused tokens | `--audit` logs every 401, and a refusal waits 100 ms, doubled for each more from its address |

The whole policy is [`policy.txt`](policy.txt); the schemas are
[`schema/accounts.fenecql`](schema/accounts.fenecql) and
[`schema/org.fenecql`](schema/org.fenecql).

## Threat model

### Defended, and how

| Threat | Defence | Test |
| --- | --- | --- |
| A password database leaks | scrypt (N = 2^15, r = 8, p = 1, 16-byte salt) in the app; fenecdb stores the result. Refresh tokens and link codes are stored as SHA-256, so the file signs nobody in | `auth`: hashes and salts, the stored row holds no password, no refresh token as itself |
| Account enumeration | the same status and body for an unknown email and a wrong password, a dummy scrypt for the unknown one (medians 20.25 against 20.82 ms over 40 attempts each); sign-up and forgot answer the same either way; rate limits key on the normalised email whether it exists or not | `auth` |
| Online guessing | 8 attempts per account and 30 per address in ten minutes, each counted under the write lock (`require 1`), 16 concurrent attempts letting exactly 8 through; a right password after the limit is refused too | `auth` |
| Duplicate accounts by case or Unicode form | normalisation before `@unique` | `auth`: `Alice@X.io` and `alice@x.io`, `José` composed and decomposed |
| A stolen refresh token | rotation on every use, conditional and in one block; reuse revokes the family and is logged; HttpOnly, SameSite=Strict, Path=/api/auth | `auth`: reuse revokes the victim's current token too; 8 tabs racing with one token, at most one wins and the family ends |
| A stolen access token | lives five minutes, names one tenant, holds only the person's role and teams | `tokens`, `tenancy` |
| Forged tokens | RS256 against the app's public key: a forged signature, `alg: none`, HS256 signed with the public key (PEM or modulus), an unknown `kid`, a body changed after signing, no `exp`, `nbf` ahead, an expired one: 401. No tenant, another tenant: 403 | `tokens`, on fenec-server and on the app's API |
| One organisation reading another | a tenant each, a token bound to its tenant: REST, `/query`, `/batch`, subscriptions, `/_changes` | `tokens`, `tenancy` |
| A user reading another team | `in $jwt.teams` on every read, write and stream | `tenancy`, `realtime` |
| Privilege escalation inside an organisation | per-operation and per-field grants; admin rules filtered on `role != "owner"` before and after a write; comments pinned to their author | `tenancy`: the 29 x 5 matrix, each refusal checked to have written nothing |
| Learning about rows through scores | `match` takes BM25's statistics over the token's rows: Mia's score stayed 2.766 while another team wrote 200 tasks with the same words, which moved the collection-wide score 5.14 -> 0.26 | `realtime` |
| Invitation reuse, forwarding, expiry | `insert ... if absent require 1`, the invite's address must be the signed-in, confirmed one, `@ttl(72h)` | `tenancy`: six concurrent redemptions, one joins |
| Login CSRF and cross-site requests | Origin checked, JSON only, SameSite=Strict; the SSO callback must carry the state this browser was given | `auth`, `tokens` |
| Script injection in the page | DOM calls only, `highlight()` used as offsets rather than tags, a CSP with `script-src 'self'` and no inline styles | by construction |
| Rewriting history | `audit` and `security_log` append-only for every JWT, the app's included | `auth`, `tenancy` |
| Losing writes to operations | a move freezes, copies and flips; a failover promotes a replica fed with `--sync always`; writes retried with their idempotency key | `operations` |

### Out of scope, or the operator's

- **TLS.** fenec-server and this app speak plain HTTP; both go behind a TLS
  terminator, which `Secure` cookies then need (`TRELLIS_ORIGIN=https://…`).
- **The app's process.** It holds the signing key, the router's token and
  the nodes' operator token (for provisioning). Compromised, it can do
  anything; what it cannot do with its request-path token is rewrite the
  logs. A deployment splits provisioning into a service of its own.
- **A token's life.** A role changed or a member removed counts from the
  next refresh, at most five minutes. A subscription is checked when it
  opens, so the page reopens its streams at every refresh (gaps, below).
- **Mail.** The development mailbox stands in for a mail service; SPF,
  DKIM and the inbox itself are outside.
- **Abuse past one machine's limits.** The rate limits live in the
  accounts tenant; a distributed attack from many addresses meets the
  per-account limit only. No CAPTCHA, no second factor.
- **Timing of other users' rows.** A `match` takes longer the more rows,
  readable or not, hold its words (SECURITY.md); organisations are
  tenants, so this is about teams inside one.
- **Per-tenant quotas.** A noisy organisation shares its node's CPU and
  disk; it is measured below, not limited.

## Test matrix

`npm test`, on an M1 (all pass; in CI on every push):

| File | What |
| --- | --- |
| `auth.test.ts` (21) | hashing, normalisation, the same answer and time for an unknown email, both rate limits under 16 and 40 concurrent attempts, rotation, reuse revoking the family, a race of 8, sign-out, cross-origin, verification once, a reset once and not after 30 minutes (a row 31 minutes old), forgot's same answer, the security log append-only |
| `tokens.test.ts` (23) | 14 tokens against fenec-server, every route of another tenant, admin routes, the app's API, refusal waits doubling, ID token attacks, single sign-on end to end and a callback in another browser |
| `tenancy.test.ts` (6) | the role matrix below, a member's team change by `/query`, REST `PATCH` and `/batch` (put back whole), the author pinned, invitations once, for their address and not after 72 hours, a role change and a removal counting from the next refresh |
| `realtime.test.ts` (7) | a subscription hears its teams only, a task leaving the team is a deletion then silence, a shape naming another team, another tenant's stream, the `/db/` pipe, scoped BM25 and facets, comments |
| `operations.test.ts` (2) | a tenant moved under 8 writers, a node killed under 8 writers |

The role matrix, each operation tried with the person's own token straight
against the router (or through the app where marked), each refused write
checked with the operator's token to have written nothing:

| operation | owner | admin | member | guest | other org |
|---|---|---|---|---|---|
| read tasks of their own team | yes | yes | yes | yes | no |
| read tasks of another team | yes | yes | no | no | no |
| create a task in their team | yes | yes | yes | no | no |
| create a task in another team | yes | yes | no | no | no |
| edit a task's title | yes | yes | yes | no | no |
| move a task to another team | yes | yes | no | no | no |
| delete a task | yes | yes | no | no | no |
| comment as themselves | yes | yes | yes | yes | no |
| comment as someone else | no | no | no | no | no |
| edit their own comment | yes | yes | yes | yes | no |
| edit someone else's comment | no | no | no | no | no |
| delete a comment | yes | yes | no | no | no |
| read the member list | yes | yes | yes | yes | no |
| change a guest's role | yes | yes | no | no | no |
| make someone an owner | yes | no | no | no | no |
| change an owner's role | yes | no | no | no | no |
| read the audit log | yes | yes | no | no | no |
| write an audit row naming themselves | yes | yes | no | no | no |
| write an audit row naming someone else | no | no | no | no | no |
| rewrite or delete an audit row | no | no | no | no | no |
| read invitations | yes | yes | no | no | no |
| read another organisation | no | no | no | no | yes (their own) |
| read the accounts tenant | no | no | no | no | no |
| change the schema | no | no | no | no | no |
| read the change stream | no | no | no | no | no |
| invite a member (app) | yes | yes | no | no | no |
| invite an owner (app) | no | no | no | no | no |
| change a member's teams (app) | yes | yes | no | no | no |
| create a team (app) | yes | yes | no | no | no |

Operations, eight runs in a row: a move of the organisation's tenant
under 8 writers took 74 to 688 ms; each writer met one 503 and retried it
with its key; 617 to 920 writes answered a run, none lost, none made twice;
the token held before the move read on the new node, the session
refreshed, the old node answered 404, and the subscription, ended by the
move, seeded again from the target. Killing the tenant's node with SIGKILL
under 8 writers (a one-second lease): writes were answered again on the
replica 1.18 to 1.42 s later, 613 to 626 answered a run, none lost.

## Measured

`npm run bench` on an Apple M1 (8 cores), nodes with `--sync always`, three
nodes and the router on one machine, 10 s a rate:

| What | Result |
| --- | --- |
| sign-in (scrypt N = 2^15), 1 client | 11.9/s, p50 84 ms, p99 97 |
| sign-in, 16 clients | 47/s with libuv's 4 threads (p50 351 ms); 61/s with `UV_THREADPOOL_SIZE=8` (p50 264) |
| token refresh with an organisation's token, 1 client | 128/s, p50 7.8 ms, p99 14.2 |
| token refresh, 16 clients | 413/s, p50 40 ms, p99 51 |
| a board's load (teams, people, 500 tasks), through the router | p50 0.92 ms, p99 2.7 |
| the same through the app's `/db/` pipe | p50 1.30 ms, p99 3.8 |
| board loads, 16 clients | 2 390/s, p50 6.1 ms, p99 14.9 |
| a write to the board hearing it, 1 subscriber | p50 0.24-0.48 ms, p99 0.55-3.4 |
| the same, the last of 50 subscribers | p50 0.83-1.13 ms, p99 1.2-2.9 |
| an organisation's board read, alone on its node | p50 0.27 ms, p99 0.51 |
| the same beside a noisy one on that node: 6 bulk writers (24 000 rows/s) and 4 scanners (330 scans/s) | p50 0.28 ms, p99 2.3 |

A sign-in is scrypt's: 32 MB of memory a hash, about 84 ms on one core,
which is the point of it. A refresh is four round trips to the accounts
tenant, one to the organisation's and an RS256 signature. The noisy
neighbour moves the quiet organisation's median by 0.01 ms and its p99 by
1.8 ms: the two are separate files with separate locks, sharing the node's
cores and disk.

## Gaps found

New fenecdb gaps this example hit, each with the statement and the
smallest fix:

1. **A CORS preflight is refused whenever the server has a token.**
   `OPTIONS /t/o-acme/query` with `Origin` and
   `Access-Control-Request-Headers: authorization` answers 401 (with the
   CORS headers), and a browser then sends nothing: `handle` in
   `fenec-http/src/lib.rs` authenticates before it answers a preflight,
   which by the Fetch standard never carries `Authorization`. So "a
   browser can talk to the database directly" holds only from the same
   origin; Trellis pipes `/db/` to the router for that reason. Fix: answer
   `Method::Options` with 204 before `authenticate`, on a single database
   and under `/t/<t>/`, when `--http-cors` is set.
2. **`FenecHttp.live` never fires under a scoped token.**
   `connect(url, { token }).live('get tasks where team = $1', cb, { params,
   collections: ['tasks'] })` calls `cb` once: its stream's shape is
   `where=false`, ANDed with the token's filter, and a scoped subscription
   is told only of rows it was sent, so no write ever reaches it. The board
   opens a real shape (`tasks/changes?team=eq.<key>`) instead. Fix: for a
   scoped token, tell an empty-shape subscription of writes to rows the
   token may read (ids only), or have `live` open `select=id` without
   `where=false` and pay the seed of ids.
3. **The client has no shape subscription, and keeps its SSE reader to
   itself.** `@fenecdb/web/client` exports `live` but no `subscribe(shape)`
   with its seed and changes, and `sseEvents` is not in the package's
   exports, so `public/app.js` and `test/sse.ts` each carry one. Fix:
   export `sseEvents` and a `subscribe(collection, shape, onEvent)`.
4. **A subscription outlives its token.** A stream opened with a token
   whose `exp` was two seconds away still delivered a change written four
   seconds later, while the same token's `get` was 401: a stream is checked
   when it opens. A member removed from a team keeps hearing it until the
   stream closes. Trellis closes and reopens its streams at every refresh.
   Fix: end a scoped stream at its token's `exp` with an `error` event the
   client reopens on.
5. **Refusal waits count the router's address.** Behind `fenec-shard`
   every request reaches a node from the router, so the node's doubling
   wait keys every client's refusals to one address: four forged tokens in
   a row waited 101, 204, 402 and 803 ms, a fifth user's expired token
   would wait 1.6 s, and any good token from anyone resets the attacker's
   count. Fix: the router sends the client's address (`Forwarded`) and the
   node keys its waits by it when the peer is a router it trusts; or the
   router applies the wait itself to a 401 it forwards.
6. **A new tenant's schema needs the operator's token.** Placing a tenant
   takes the router's token and applying its schema the nodes'
   `--http-token`, which reaches every tenant on every node, so the process
   that creates organisations holds both. Fix: `PUT
   /_shard/tenants/<t>` takes a schema description and applies it as it
   creates the tenant, so provisioning needs the router's token alone.

What earlier work closed and this example uses rather than works around:
`require`, tenant-bound tokens, `exp` required, list claims, per-operation
and per-field grants, `append-only`, scoped `@unique` clashes, scoped BM25,
`expired()` and `@ttl`'s expired rows leaving the unique index at once,
`db.batch` with `idempotencyKey`.

## Files

| Path | What |
| --- | --- |
| `policy.txt` | every rule a token is held to |
| `schema/*.fenecql` | the accounts tenant and an organisation's |
| `src/trellis.ts` | accounts, sessions, links, organisations, invitations, members |
| `src/server.ts` | the HTTP server, the API, the `/db/` pipe, the security headers |
| `src/tokens.ts`, `src/jwt.ts` | RS256 tokens: the app's, a person's; verifying |
| `src/passwords.ts` | scrypt, email normalisation, secrets and their digests |
| `src/oidc.ts`, `src/idp.ts` | single sign-on, and the mock identity provider |
| `src/cluster.ts` | three nodes and the router, for development and the tests |
| `public/app.js`, `public/style.css` | the page |
| `scripts/` | `dev`, `seed`, `bench`, `shots`, `ci.sh` |
| `test/` | the tests and their harness |
