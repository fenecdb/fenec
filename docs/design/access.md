# Access

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Access*: scoped tokens, field lists, rules reading another collection, a token bound to its tenant, and empty tokens. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Scoped tokens

**A scoped token is held to its rules at every level, twice for writes**
(`fenec-http/src/access.rs`). A JWT's policy filter is ANDed into the statement
-- the `where`, each `lookup` level, a subscription's shape -- so a new path
that runs a statement must go through `scoped()` or it reads everything. Writes
also get `WITH CHECK`: the `Check` write hook tests every document a put or a
set writes against the filter, found through a thread-local set by `within()`
around the execution -- a scoped write executed outside `within` goes
unchecked. A scoped subscription keeps the ids it sent and reports deletions
only for those; the unscoped shape's "a changed id that does not match is a
deletion" would hand every user everyone's ids. A scoped stream of a shape
that holds nothing (`where=false`, what `FenecHttp.live` opens) follows the
token's own filter instead, its ids kept server side and never sent, and a
write to one, or one leaving, is a change naming no row: ANDed with the
filter the shape matched nothing and a scoped live query never ran again,
and told of every write it would have had the moments of other users'. A
stream is ended at its token's `exp` (`Scope::expires_ms`, the keep-alive
wait bounded by it) with `event: error` and `{"error":"the token has
expired","status":401}`, which every client takes as a refused request --
`FenecHttp.live` and `subscribe` stop with a `FenecError` of status 401,
the sync layer asks for a token (a scenario), Go's stream ends with an
`*Error` of 401, .NET's with `Event.Status`: a stream opened two seconds
before `exp` delivered a change written four after, while the token's `get`
was 401. A scoped read's poll tag is a digest of its answer (`content_tag`,
the query run every time): the change counter's tag, and a 304 answered
unrun, told it when anyone wrote to what it reads. The algorithm is the key's,
never the token's: `--jwt-keys` reads a JWKS file, `oct` keys for HS256 and
`RSA` keys (2048 bits up, `crypto::RsaKey`, 64-bit limbs: 166 us a check in
32-bit ones, 38 now) for an identity provider's RS256, a `kid` picking the
key; an RSA modulus taken for an HS256 secret would sign anything. The file
is read again as it changes, looked at once a second, which is the rotation.
A verified token is kept by its text (`verified`, 16 shards of 256, emptied
when the keys change, `exp` asked each time): 0.27 us against 2.9 for HS256.
A token naming no `exp` is refused (`Demands::require_exp`,
`--jwt-require-exp off` takes it), and `--jwt-max-age` bounds how far ahead
one may lie; `mint` stamps an hour on claims naming none. Grants are
`read`, `insert`, `update` and `delete` (`write` the three, bits in
`Rule::ops`), `update(a, b)` the update of those fields alone
(`Rule::fields`): `update` on `accounts` let a ledger's app token `set
accounts {balance: balance + 500000}` and change an account's kind too,
and `WITH CHECK` saw only the row after. A write over a row is judged by
the fields that differ, as encodings (`Scope::overwrites`, through the
hook's `before_overwrite`, which the engine calls with the row as it was
-- a `set`'s and a `put`'s over an id, the old row read before the hooks
rather than after, so no read more): each must be one granting rule's,
that rule admitting the row before and after; a policy naming no field
list judges nothing there, and an unscoped write meets an empty hook
list or a thread-local found empty. And the `Check` hook takes the write's op: an insert held to
the rules granting it, an update found and checked by its own, a delete
found by its own; an update and a delete need read beside them, an insert
none, so a client appends to a stream it cannot see. `<c> append-only`, a
line of no filter and no role, takes updates and deletes from every
scoped token whatever its rules say -- a rule naming them there is refused
at startup -- and not from the server's token, which keeps corrections,
erasure and `@ttl`'s sweep: in the schema it would bind the operator too,
a format change for an invariant whoever holds the file can break anyway.
A rule's filter naming `new.<field>` is a transition (`Rule::transition`):
`new.` reads the row as the write leaves it, as in an upsert's `else set`,
a bare name the row as it stands, and the rule is true or not of the two
together (`Pair`). One filter asked of each row could not say which way a
row may go: under examples/dispatch's `status = "ready" or status =
"cancelled"` a customer took a cancelled order back and moved one to the
queue's head, and a courier delivered a ready order nobody claimed. It
grants `update` alone, refused at startup beside any other grant. A write
over a row is the token's when one rule naming every field it changes
admits the change, a transition by the pair, any other by each row;
rules with no field list and no transition still widen each other a half
each, as PostgreSQL's permissive policies, and a transition lends no half.
A `set` picks a transition's rows by `Rule::before`, the filter with each
term naming `new.` taken as true or false by the `not`s above it
(`relaxed`), so it admits every row the pair could -- held to that over
every filter of three levels and every assignment of 0, 1 and null by
a test -- and `before_write` leaves an update to `before_overwrite`, with
the row before, where a transition grants it. `returning` answers rows
held to the read rules as the write leaves them: a `del`'s picked through
them as they stood, a `set`'s through their terms over fields it does not
write, the rest taken as passing (`Scope::answerable`) and asked of each
row as written by `Hook::answering`, which the engine's `returned` calls
-- a row the token may not read refuses the statement (403), as
PostgreSQL makes its `SELECT` policies `WITH CHECK` options on an
`UPDATE ... RETURNING`'s new row, an error rather than a row dropped
unseen.
ANDed in whole, the read rules judged the row before: a courier, who
reads the orders they carry, claiming a ready order with `returning *`
took nothing and was told nothing. `Scope::admits` asks the rules one
after another rather than making the `or` of their filters, which copied
each, twice an update: a scoped `set` cost 1.6-1.9 us over the server
token's in process against 2.2-2.9 on main, a field list 2.3-2.8 against
3.0-3.7, a transition 2.2-2.6, answering its row 4.1-4.9 against 4.6-5.6;
over HTTP 5.0 us of 45 under a plain rule, 5.7-5.9 a field list, 6.1-6.8
a transition and 8.7-9.3 answering, main's 5.1, 5.8-6.0 and 8.5-9.0 (`make
policy-bench`, three runs each in turns). The browser module registers no
hook and has the `answering` loop `cfg`'d out: the size it was.
A list claim goes after `in` (`room in $jwt.rooms` is read as `in
[$jwt.rooms]`, which `bind` spreads), more than `MAX_CLAIM_VALUES` (1 000)
refuses the token, and claims are read exact (`json::parse_json`): a list
of numbers read the quick way was `f32`s, team 123456789 matching
123456792. A `match` the token reads through a filter is `Match::within`:
BM25's count, mean length and document frequencies over the rows its
filter selects (`TextIndex::search_within`), so a score is the one a
collection of those rows alone gives -- alice's memo went 9.87 -> 4.79
once bob wrote 200 holding the word -- 0.16 -> 0.24 ms for 2.5% of 100
000 rows; the browser module leaves the counting out.

## Field lists

**A read rule may name the fields it shows, and a field it withholds is
held out of what a statement does, not only of its rows**
(`fenec-http/src/access/fields.rs`). `read(pickup, run_at)` grants the
rows its filter admits with those fields and `id`
(`Rule::read_fields`); a row shows the union of the fields of the read
rules admitting it, the rest `null` (`Scope::shown`, asked of each row
over its own columns -- the fields the rules read appended to the list
where it leaves them out, `Level::added`, and taken off after), in
`/query`, `/batch`, REST, each `lookup` level by its own collection's
rules, `returning`, a stream's seed and changes and the MCP tools. A field
no rule grants is not there: dropped from `select *`, from `describe`,
`/collections`, `/_schema` and MCP `schema` (`Scope::shown_schema`), and
refused by name (403) as a name the collection lacks is, so a name tried
says nothing -- a SQL join's `x.*` too, whose names the engine checks for
a clash before the levels are masked: `hold` hands it what each level
shows (`Flat::shown`), where `s.place as code` beside `x.*` was refused as
two columns named `code` and `as nosuch` answered; and every other name a
join writes, which the engine holds to its tables' fields (`Flat::names`),
is held here first -- a LEFT JOIN's `on` term the reading leaves out was
answered naming a withheld field and refused naming a missing one -- with
the engine's refusal of a name written with no table naming no other
table's field. Every table a join names is held to the read rules
(`Scope::select`, refused as a collection the token is granted nothing on
does not exist), whether its reading reads it or not: a LEFT JOIN whose
`on` term the reading leaves out made no inner `get` for the rules to
reach, and the engine's check of the join's names told a token holding
`stops read` alone that `orders` has a `dropoff` and no `nosuch` -- 200
against 400 -- where a plain join of `orders` was 404, as for a table no
database holds. A scoped token's join keeps its list alone (`Flat::probe`
cleared): the probe the engine reads past the list's 100 000 values hangs
a level off the rows that the token's masks, made for the list's one
level, would not reach, so such a join is refused past them. Masking alone leaked the field through everything else a
statement does with it: `where dropoff = $1` answered the order going
there, `order dropoff` sorted the doors, `{n: dropoff}` wrote one into a
field the token reads, `1 / n` failed with an error naming the division.
So `fields::hold` and `fields::write` run before the rules are ANDed in:
each `where` term reading a field a row may not show becomes
`(showing_f and term)`, `showing_f` the `or` of the filters of the rules
granting it (`View::guard`) -- false, under any `not`, wherever the row
does not show it, and never worked out there since `and` stops at the
first false (the bound `Filter` and `eval` alike) -- in a `get`, a `set`,
a `del`, a `lookup` level, a facet's rest and an inner `get`, whose column
is listed from the rows showing it; `order`, `group`, `having`, `facet`,
an aggregate, `match`, `near`, `rerank`, a highlight, a `lookup` key (both
sides) and a `set`'s or upsert's values are refused (403, naming the
field) unless every row they reach shows it (`View::open`): every read
rule grants it, or one granting it has no filter or one whose `and` terms
are each an `and` term of the statement's own `where`, parameters bound
(`Reach::implies`, structural: a courier's `where courier = $1` is their
rule's `courier = $jwt.sub`) -- nothing finer is proved. A computed column
over one is `case when showing_f then <it> end`, null and never worked out
where hidden. Writes reach the rows they reached: a field every read rule
grants is as exposed to an update rule wider than the read rules as every
field was before field lists. A stream keeps a digest of each row as sent
(`sse::Seen`) and sends no row a write left showing the same, which would
tell it when a hidden field changed, as the scoped poll's content tag
keeps it from a poll. PostgreSQL's column privileges are a role's, the
same on every row, and refuse a statement naming the column, `SELECT *`
included; a per-row column is a view of `CASE WHEN` columns, whose
`WHERE` reads `NULL` where this reads false. A token none of whose rules
name fields takes the path it took (`Scope::fielded`, one bool a
statement, its mask `None`, its plain `get`s through `query_json`): in
eight ABBA turns against main a scoped `set` in process took 1 988 ns
against 1 981, over HTTP 26.9-27.2 us against 26.8-27.1, and `make
requests-bench` stayed at 0.022 ms a row by id either way. The mask goes
out beside the statement (`rewrite_with`'s out-parameter): returned in a
pair, the kilobyte `Statement` was copied twice more, 1 990 -> 2 060 ns.
One whose rules do name fields pays its rules over each row answered:
a row by id 2.12 -> 2.75 us in process and a page of twenty 12.6 -> 15.1
(0.13 us a row), over HTTP 26.0 -> 28.0 and 34.4 -> 39.5 us, the rest of
that the stored-bytes path a masked `get` gives up. Each attack is
a test of `tests/fields.rs` (and `mcp.rs`), every one failing with its
part of the holding taken out. Native only: the browser module is the
same to the byte.

## Rules reading another collection

**A rule may read another collection, answered before each statement**
(`access.rs`: `inner_gets`, `ring`, `Scope::held`, `Scope::bound`).
`positions read where courier in (get orders select courier where
customer = $jwt.sub and ...)` lets a customer read the courier carrying
their order; a rule took no `in (get ...)`, since `admits` tests a lone
document inside the `Check` hook, where no query runs (`eval` refuses an
`InSelect`). `$jwt` binds inside the inner `get`'s `where`, and nowhere
else in it (`max_param` of the rest, at startup); the inner `get` is held
to the token's read rules on its collection once a token (`held`, from the
rules as bound: none granting `read` is `false`, no rows, as PostgreSQL's
RLS gives a policy's subquery none of a table no policy grants -- refused
as missing, it would fail every read of `positions`). Rules reading each
other's collections at any depth and whatever their roles, a `*` rule
reading one, `new.` or `expired()` inside one, are refused at startup
(`ring`). Under the lock that runs a statement, the rules of the
collections it reads or writes (`statement_reads`) are answered as the
`in` lists they are (`bound`, through lib.rs's `scoped_under` and
`run_bound`): the statement then holds lists the engine reads as any, and
`within` installs the bound scope, so `Check` -- an insert's document, an
update's pair, a transition's, an upsert's row written over, `answering`
-- tests by them; and the mask (`fields::hold`'s `showing_f` guards and
`case` wraps, made by `rewrite_with` over the bound rules) and its
application (`Scope::shown`, called on the bound scope), a stream's view
and digests, are the bound scope's too: a rule naming fields that reads
another collection shows each row the fields of the rules admitting it as
their lists stand, where unanswered `eval` refuses the `InSelect` and the
rule admits no row asked of alone (`tests/reach.rs` fails with either the
statement's or the stream's bound scope taken out). The inner `get`'s own
fields are the policy's, not held to the token's field lists: its list is
never shown, only what it admits. The rewrite before the lock stays, with the rules
unanswered: it refuses a scoped schema change before `Database::maintain`
could build it beside the database, and it is what a held write is planned
from (`waits::plan`), so its look reads what the rule reads and a write
there wakes it. A `/batch` answers them for each line (a claim, then the
read the claim opened, one round trip), a read loop and a read batch again
for each lock they take, the MCP tools as `/query`. The list is held to
`MAX_CLAIM_VALUES` and refused past it, never cut (`listed`: the inner
`get` read up to 1 002 rows, and grouped by its field for its distinct
values only when it fills them -- grouped every time, a `set` under such
a rule took 4.5 -> 5.1 us). A scoped stream of a collection whose rules reach out
(`sse::Reach`) binds them again under each look's lock where a collection
they read was written since its change (every look where an inner `get`
reads the time), runs the shape where the list moved and diffs by id
against what it sent (`enter_and_leave`), a row leaving a deletion only of
a row it was sent: a customer's stream gains the courier as the order is
claimed and loses them at the delivery, no write to `positions` between.
It costs one inner `get` a statement -- `make policy-bench`, a crew's
bucket of two: a `set` 2.1 us over a plain rule in process, 4.1 against
2.0, a read by id 1.7 (3.9 against 2.2), over HTTP 4.7 and 4.3 us of 32.0
and 30.6 -- and a stream one at each write to what its rules read: beside
10 and 100 scoped streams a write, half of them to `orders`, took 162 us
and 1.89 ms of the process's CPU against 129 us and 1.65 ms under a rule
naming the courier as a claim (`ARGS=streams`), every scoped stream
looking at every write either way; examples/dispatch's beat, which writes
`orders`, beside 50 customers following `positions` so took 1.94 ms
against 0.69 with the courier's point on the order, so Plover keeps
`courier_loc` and reads the courier's name through such a rule, its code
withheld by a field list (`ARGS=beats`). A token with no such rule runs
as it did: `Who::reaches_out` is a field read and the rest is untouched
-- the policy bench's common sides in four ABBA quartets against main on
a quiet machine, a `set` in process 1 313-1 335 ns against 1 312-1 327
under the server's token and 1 975-2 012 against 1 976-2 014 under a
plain rule, over HTTP 25.1-25.4 us p50 against 24.8-25.3 and a read by
id 23.7-23.9 either way; `make requests-bench` in three quartets, one
client the same to the microsecond and eight inside the spread. A read
in process moved 1-1.5% either way as the bench's own functions were
laid out (the server token's reaches nothing this changed, only
`Database::query`), and nothing below fenec-http changed: the browser
module is the same to the byte.

## Tenant binding

**A token is bound to the tenant it names** (`Scope::reaches`,
`route_tenant`). The policy is the node's, not a tenant's, so `owner =
$jwt.sub` matched alice's rows in every tenant's file: a token minted for
one tenant read and subscribed to every other, on its node and through the
router. On a `--dir` node a JWT reaches `/t/<t>/` only when its tenant
claim (`--jwt-tenant-claim`, `tenant`) names `<t>`, a text or a list
holding it; one naming none is 403 unless `--jwt-unbound-tenants`. It is
asked once in `route_tenant`, before the tenant is looked up -- so a
refusal says nothing of which tenants exist -- and every route below the
prefix passes there: a new one cannot skip it. The router forwards
`Authorization` and holds no keys, so the node enforces it. A tenant is
created with its schema through the router (`PUT /_shard/tenants/<t>
{schema}`, FenecQL text read before anything is placed, applied on the
node by `POST /_admin/tenants/<t>/schema` with the admin token the router
holds, once the lease names the tenant, and the tenant taken back off the
node and the directory when refused): applying it took the nodes'
`--http-token`, which reaches every tenant, so the app creating tenants
held it. A tenant's
database installs the `Check` hook as a single one does
(`Tenants::check_scoped_writes`, from `Server::with_tenants`): none did,
and a scoped write to a tenant went unchecked.

## Empty tokens

**An empty token is no token.** Every check reads a request carrying no
`Authorization` as the empty token (`unwrap_or("")` before the constant-time
compare), so an empty token configured matched everyone: an empty
`FENEC_SHARD_TOKEN` -- an environment file's line left blank -- opened
`/_shard/` and passed the router's check against a public address with no
token, and an empty `--admin-token` opened `/_admin/`. Every token and key
variable is read through a filter, set and empty being unset
(`env_secret` in fenec-server, `env` in fenec-shard's main,
`FENEC_HTTP_TOKEN` and `FENEC_SHARD_ENTRY_TOKEN` as they were), and every
token flag given empty is refused at the start (`--http-token
"$TOKEN"` with `TOKEN` unset), as is an empty JWT secret's file; the
library holds an embedder to the same -- `Config::settled` in each
`Server` constructor, `Router::assemble`, `Replication::new` and
`Tenants::replication_token` take an empty token for none. The archiver's
`FENEC_REPLICATION_TOKEN` is unset when empty and its `--token` refused,
and an empty key file (`--key-file`, `FENEC_KEY_FILE`) is refused, never
none, which would write an archive in the clear. `tests/tokens.rs` in
fenec-server, fenec-shard and fenec-cli and fenec-http's
`tests/empty_tokens.rs` fail without it. A node also reads `--admin-token`
from `FENEC_ADMIN_TOKEN` and a router `--replication-token` from
`FENEC_SHARD_REPLICATION_TOKEN`, the two that had no variable, so no token
need be on a command line `ps` shows.
