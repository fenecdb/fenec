# Held writes

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Held writes*: `Fenec-Wait`, the writes that wake held ones, the bids that settle a row, and the leader's looks. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Fenec-Wait

**`Fenec-Wait` holds a `set` or a `del` that writes nothing until it
can** (`fenec_http::waits`, `Database::wakes_at` in `engine/wake.rs`). A
worker whose claim found no job slept and claimed again: late by its
sleep, or a claim under the write lock every few milliseconds a worker.
Sent to `/query` or `/batch` with `Fenec-Wait: <ms>` (30 s at most; beside
`Fenec-After` the header stays that read's wait), a request whose `set`s
and `del`s wrote nothing -- no row, or a `require` unmet, its block put
back -- is held and run again once it may write: the same request at a
later moment, answered with what it wrote, or at the end of the wait with
what it answered last. A header, not a clause: a statement runs under the
write lock, where nothing may wait, and a page's or an app's database has
no other worker to wait for, so the builders refuse `wait` there
(`LOCAL_WAIT`) and no builder's text changed. A `put` in the request, or a
filter reading the time other than as a field compared with `now()` plus
or minus integers and parameters (`bucket(now(), ..)`, `run_at + 1000 <=
now()`, `= now()`), is refused before it runs (400, `holdable`,
`engine::placeable`). Two things wake one: a write to a collection it
reads -- `Hub::landed` under the write lock, with the collections the
block wrote (below), a load when nothing is held -- and the time reaching
a row's moment, which nothing announces:
`wakes_at` (native only) takes each such comparison anywhere in the filter,
inner `get`s too, and each `@ttl`, and asks the least value past the bound
(`get c select f where f > bound order f limit 1`, a step of `@sorted`),
the run's own time the bound so a row due between the run and the
question wakes it at once. Every living row's value counts: counted among
the rows the other `and`s let through it was exact, and walked the index
past every one they refused -- a claim held on an empty queue beside
200 000 jobs scanned them at each look, the busy workers beside it 144 000
-> 20 200 jobs a second. `tests/wakes.rs` walks generated filters and rows
moment to moment and holds the matching set unchanged between two. Held
requests are grouped by what they would pick -- each `set`/`del` as the
`get` of one row it amounts to, scope and parameters bound in -- and only
a group's head, the one held longest, is woken: it looks under the read
lock (the collections written since it last looked, then the gets), runs
the request only when a get finds a row, and having written has the next
look at once; one that found nothing is the group's answer, so a job goes
to the worker held longest, as Redis serves `BLPOP`, and a write costs a
group a look, not a run a worker (20 held, one job: one run, at most
three looks). A
look that found nothing is not repeated for `LOOK_GAP` (1 ms) however many
writes land, a write in the gap a counter: beside 16 claims held on an
empty queue, 16 busy HTTP workers took 148 000 jobs a second against
150 600 with none held, the heads looking 730 times a second, where woken
at every write they looked 3 400 times and the workers took 142 000. A
held request runs `handle` whole each time -- its token asked again, a
tenant's gate entered and let go, so a freeze goes through beside it --
and its time is out of the slow log; a tenant closed answers it 503 with
`Retry-After` and `retry_after`, a wait that ran out answers its last
answer though a write it has not looked at landed in the moment before
(its head woken late: the job is the next claim's, never lost), a SIGTERM
answers every one at once with its last answer before the sync
(`end_all`, `HELD_GRACE`), a client gone hands the row to the next (a
`peek` before each run), a keyed one keeps no key while it writes nothing
(sent again it waits again), a replica refuses it as any write, and the
router waits `--upstream-timeout` plus the wait (`Pool::send_held`: cut
at the timeout, a 502 while the node leased the job to no one). `/_metrics`
counts `fenec_held_requests` and the heads' looks and runs. A job
enqueued reached one of 8 held workers in 0.38 ms p50, 0.76 p99, against
0.83/10.1 polling every 10 ms (ten times the claims) and 15.5/85.8 every
100 ms; a delayed job 0.04/1.9 ms after its time against 1.2/8.4 and
45.8/99.4. A thousand claims held ten seconds on an empty queue took no
measurable CPU and 46 KB of memory and a thread each -- a subscription's
45 KB, a connection counted against `--max-connections` -- where a
thousand polling every 100 ms took 86% of a core. Each claim sent with the
header on a busy queue: 150 000 jobs a second either way; `make
requests-bench` in ABBA turns against main inside its spread (a row by id
0.021 ms p50 both, eight clients 125-134k a second against main's
123-149k). The browser
module compiles none of it: the same size to the byte, 149 bytes moved
where `engine.rs`'s panics carry their line numbers (`mod wake` declared
at the file's end, identical). The JS client grew 0.6 KB brotli (`wait`,
`signal`, the page's refusal). Every client has it: JS `{ wait, signal }` on `run`, `batch`, `update` and
`delete`, Python `wait=` seconds (the socket's timeout that much longer;
`AsyncClient` closes a cancelled one's connection, whose answer would be
the next request's), Go `db.Wait(d)` and .NET `WithWait(d)` copies under
the context or token, Swift `withWait(seconds)`, Kotlin `withWait(ms)` (a
cancelled coroutine disconnects the `HttpURLConnection`, whose read heeds
no interrupt) and Dart `withWait(Duration)`.

## Waking held writes

**A write wakes the held writes that read what it wrote, and a row several
groups would take goes to the one ranking it first** (`Watcher::landed`,
`waits::settle`). Woken by every write, each group's head looked under the
read lock whatever was written: 150 claims held on `orders`, each a group
of its own as couriers' claims are (a point and a `sub` in each filter),
took a put of `positions` beside them from 22 us p50 to 50-95 and from
30-90 us p99 to 200-480, the heads looking 115 000-132 000 times a second
(`make queue-bench`'s `held other`, in turns). The engine tells its
watcher the change a block landed from (`Watcher::landed`, a default
method calling `notify`; native only, so the browser module, which holds
no write, is the same size to the byte, 196 bytes moved where
`engine.rs`'s panics carry their line numbers); `Hub::landed` asks
`changed_collections_since(before)` only while something is held
(`Waits::holds`), and `Waits::wrote` marks stale and wakes the groups
whose collections -- their tests' and their inner `get`s' -- it names
alone, `None` (a load, a drop, a compact) every one: the same put 22-25
us p50 and 30-97 p99 beside 150 held, as beside none, and no look. And a
row several groups would take went to whichever head ran first: of 10
claims each a group of its own the one held longest took it 4-16% of the
time, of 100 1-2%, of 1 000 2-4%, and each head that found it ran under
the write lock, 3.2-6.4 runs a job (`held contested`). A head that finds a
row bids for it now (`Bid`: the row, its rank by the claim's own order --
`near`'s `_score`, a sparse dot product turned round, `None` under fields
-- and when its head was held) and waits; `settle` gives each row to the
bid ranking it first, the head held longest on a tie, the rest behind the
winner's run (`behind`): a tie's in a line by arrival (`line`), the next
looking once the run before ends -- looking all at once after each run, a
burst of N jobs to N claims was N rounds of N looks -- and a look in the
line that finds nothing disbanding it; a ranked loser looks again at once.
A round waits for every group a write before its first bid woke (`horizon`,
the writes announced as the bid lands: a look waits for the read lock and
reads what landed meanwhile, and held to the look's start a job went to a
later claim), told to look, come to its moment or behind a run
(`Group::owes`), each once, so a round ends however many writes land; but
a tie settles as soon as no group held before the best bid is still to
bid. Groups are kept in their heads' order (`remove`, and a group whose
head left re-placed by its next; `swap_remove` shuffled them), so `settle`
scans to the first still to bid: a full scan and a sort at every look held
the mutex every look waits for through a thousand of each, 38-60 ms a job
at 1 000 claims. A bid for a row given to a run under way goes behind it
at once; one for a row a run took after its look began (`gone`) looks
again; a round ends the gap of a head it waits for, and only then (ended at
every look, a claim beside a busy queue looked 1 500 times a second
against 700). One job every 2 ms at least: the one held longest took it
every time, one run a job; from the put to a claim holding it 0.43-0.47 ms
against 0.37-0.43 at 10 claims, 0.83-0.99 against 0.57-0.68 at 100, 11-13
against 2.6-5.2 at 1 000 -- the longest held's look, which a hundred or a
thousand threads woken at once reach late, against the fastest of them --
and the server's CPU a job 2.0-2.1, 12.6-13.9 and 110-131 ms against
1.8-2.1, 11-14 and 138-144. Not opt-in: the claim's order says what "first"
means and a tie keeps the order requests came in, `BLPOP` across groups as
within one, and the one run a job against five under the write lock is
every other writer's. A line of workers of one claim is served as before:
a job enqueued reached one of 8 in 0.34-0.36 ms p50 either way, and 16 busy
workers beside 16 claims held took 143 000-148 000 jobs a second against
146 000-148 000, the claims looking 734-779 times a second against
719-775. `tests/waits.rs` holds the longest held to every job of twelve
groups, a burst of sixteen to the order they came at three looks a claim at
most, and a gap to a look a millisecond beside eight writers;
`waits::tests` a claim ranked by `near` to the job though it was held last.
A claim ordered by a key worked out of the row -- a courier's `order
distance(pickup, $2) limit 1` -- has the key bound with the request's
parameters, as its filter is (copied unbound, the test ran with a `$2`
it had no value for), and answers it beside the id (`_rank`, a computed
column), so a row goes to the claim nearest it: one order at a time to
couriers each claiming the order nearest it, the nearest took every one
at 10, 100 and 1 000 waiting, run for once, 0.4-0.7, 1.4-1.6 and 19 ms
from the put, against a raced 0.2-0.4 ms to a courier a median of 3.3-5
km further and two runs an order (`held nearest`, in turns) -- the
price every courier's look, 2-2.8, 14-17 and 160-168 ms of the server's
CPU an order.

## The leader's looks

**One held request looks for every group, and a group that found
nothing looks among the rows written since** (`waits::look_all`,
`Known`; `Database::written_since`, `passing_among`, `picked_among` in
`engine/wake.rs`). Each head woken to look for itself, an order placed
beside 1 000 couriers' claims woke 1 000 threads, each taking the mutex,
the read lock and its whole pick -- 80-86 us a claim in process, the
point index walked past 2 000 leased orders -- 20-21 ms from the put and
174-179 ms of the server's CPU an order (`held nearest`, in turns). Now
the first held request to wait while none does is the leader
(`State::leader`): it takes every group with a look to make (`jobs`:
told to, its moment come, a write past its gap), looks for each under one
read lock -- let go each `LOOK_HOLD` (2 ms), so a writer waits out a part
-- records the bids and settles once, and sleeps to the next moment, gap
end or rate's pause (`deadline`); a head is woken only when its group's
bid won or its own wait ends, and the leader hands the looks on as it
leaves (`resign`, to any request waiting but a head about to run). A
write, a release, a join that brings a moment sooner or a pause tells it
(`kick`), a group in its gap only at the first write in it: told of each,
a claim held beside a busy queue would wake it at every write. It asks a
timed wait for two thirds of the way (`asked_for`): macOS runs one late
by half again, 3 ms took 4.5 every time, and a delayed job went from 0.2
ms after its time to 1.3 at the median, 29 at p99, until it did. A look
knows what its group's one test passes (`Known`: no row after a whole
pick that found none, every row a look among rows found, up to `AMONG`)
at its change, its writes' epoch and its time, and until the test's next
moment (`wakes_at`, each index step asked once a batch, `Moments`) no
row starts or stops passing but by a write; so the next look tests those
rows and the ring's rows written since alone, as a scan tests a row
(`Filter`, the time folded as `answered` folds it) and in its order
(`order_ids_by`, `order_ids`; one row needs none), the rows a filter
passes found once for every group of it (`Passed`, four filters a
batch). The whole pick runs where that cannot be shown the same: a ring
that no longer reaches back, a schema mark, a load, a compact or a
maintenance (`epoch`, moved by `wrote(None)`), a collection made again
(`cid`), more than 32 rows, an inner `get`, `@ttl` or `expired()`, a
time in the order, two tests, the clock set back, a moment come, an
error (the whole pick's to meet). A test holds it
(`waits::tests::a_look_among_the_rows_written_picks_as_the_whole_pick`)
over 3 000 generated waves -- points, radii, kinds, priorities, `or`s,
orders by distance, a field, an expression and two keys, ties of each,
couriers' tokens' rules, orders placed, moved and cancelled, rounds of
winners taking theirs, the time past moments and back, bursts past 32,
alters, compacts, inner `get`s -- to the whole pick's row and rank, what it then knows to every row the
test passes in the test's order, and each row's winner to the whole
picks' arbiter's: 35 000 looks among rows beside 10 300 whole picks, two
mutations of it (the known rows left out, the moment ignored) each
failing within two seeds. One order to couriers each claiming the
nearest: 0.18, 0.31-0.32 and 1.67-1.68 ms from the put at 10, 100 and
1 000 against 0.27, 1.42-1.52 and 20.0-20.7, the server's CPU an order
0.2, 0.4-0.5 and 3.1-3.2 ms against 1.2, 13.4-16.8 and 174-179; a look
among rows 1.1 us in process, 2.1 before a filter's rows were shared and
one row went unordered. One job to claims of their own (`held
contested`): 0.30-0.39, 0.75-0.80 and 2.4 ms against 0.38-0.49,
0.86-0.91 and 9.9-12.5, the CPU 0.7-0.9, 1.8-2.3 and 5.9 ms against
1.9-2.3, 12.3-13.2 and 110-126, at 29, 298 and 2 540 looks a job
against 27, 205 and 1 290-1 470: a claim behind the winner looks again at
once, and again after the ack, each among rows. A line of one claim is
served as before -- a job to one of eight 0.31-0.34 ms either way; 16
busy workers beside 16 claims 149 000-167 000 jobs a second against
146 000, the claims looking 670-830 times a second against 770 -- a put
of another collection beside 150 claims 23 us either way, a delayed job
p99 0.0 ms after its time against 1.5-2.2, and a thousand held claims
use no CPU and 46 KB each against 63, their threads no longer each
running a pick. `fenec-server` grew 16.2 KB (aarch64-apple-darwin), and
the browser module not by a byte: `wake.rs` is native only.
