# Tenants and leases

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Tenants and leases*: scaling out by tenant, the router's directory, moves, failovers and leases. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Scaling out by tenant

**Scaling out is by tenant, one file each** (`fenec-server --dir`, `fenec-shard`,
whose directory replicates to a standby router like any other file, the
standby's maps catching up from the change ring -- 4 us a change at 100 000
tenants against 38 ms reading them all again under the router's write lock,
as the primary's writes land, and ending what the standby had out to a node
a failover took a tenant off ([router.md](router.md#failover-cuts)) --
reading `failed_from` beside each placement took a change that moves a
tenant to 4.1 us and one a failover made to 4.3, from 3.9;
`site/content/docs/sharding.html`). The tenant comes from the path
(`/t/<tenant>/`, looked up again per request, so a move, an idle close or
a delete between two of them is seen), never from the query, so tenants
cannot share a file -- they
would read each other's rows. A file each also keeps ids, the change sequence,
BM25 statistics and `lookup` per tenant, which is why the router forwards bytes
and never parses a query. The registry (`fenec-http/src/tenants.rs`) opens a
file only under its lock after checking the open map, and closes one only when
the map holds the last `Arc` -- a second `Database` over one file corrupts it
exactly as a second process would. `freeze` takes a per-tenant gate
exclusively so a write that passed the frozen check cannot land after the final
export. A move is freeze, copy the image, install, flip the directory in one
statement, delete the source; the change sequence travels in the image, so a
caught-up subscriber resumes on the target without a reseed. A node's tenants
are replicated to its standby, each through its own feed at
`/t/<tenant>/_replication`, and `POST /_shard/nodes/<n>/failover` promotes
them there one at a time -- separate databases, nothing to make atomic
between them. A tenant's role is its file's, not the node's `--replica-of`:
a replica's file follows, a primary's stays one, and a file new to a replica
node follows (the standby copy the router made). Every file on a replica
node was made to follow once, so an idle close or a restart undid a
promotion and the old primary's image wiped the writes since. A rejoining
node's tenants follow when the router records the pair
(`POST /_admin/tenants/<t>/follow`, each), a failover that moved every
tenant ends the pair, and no tenant is placed on or moved to a standby. A
tenant whose follower runs is never closed as idle: the follower holds the
database rather than the tenant, and a close left it writing the file under
the next instance. The router cannot tell a node that is gone from one it cannot reach,
and guessing makes two primaries, so it promotes on its own only under a
lease (`fenec-shard --auto-failover`, nodes `fenec-server --dir --lease`;
`fenec-shard/src/lease.rs`, `fenec-http/src/lease.rs`): a node stops
writing as its lease lapses by its own clock -- measured from a moment the
router acknowledged, where the router measures from when that answer came
back, and waits a tenth more -- and only then are its tenants promoted.
Each grant is numbered (`grant`, per node from a random start a router
process, `Leases::number`) and names the newest grant whose answer came
back within the pool's third of a lease (`acked`); the node keeps when it
answered each of its last 16 (`State::answered`, cleared as the holder
changes) and runs its lease from the one acknowledged, never from a
grant's reading (`fenec_http::lease::Numbered`, `grant_from`). Run from the
reading, a grant the router gave up on (`Answer::Silent`) and the node read
late -- held on its way, or left unread by a node stopped or slow -- was a
whole lease after the router had failed the node over, both copies
answering writes 200 (the review of #340's `late_grant.rs`), however late
it came. Read late now, a grant acknowledges what the router had counted
by its sending, a lease the failover outlasted; an answer that came back
late is acknowledged by no grant; a number the node never answered, or
answered for another holder, runs nothing; and a router's restart numbers
afresh, so a grant of the one before, read late, names no number this one
acknowledges. A grant is newer than the one the lease came from
(`State::basis`) by the answer it acknowledges, then by its own number --
one router process's, counted on as it sent them, the only grants that
name one answer -- and taken whole where it is newer, its list with the
lease `ms` from that answer, which may cut the lease held short; its list
alone where the lease has lapsed; not at all otherwise. The list replaced
on its own, as the design first had it, a late grant's list from before a
failover named the tenant again beside the lease a newer grant ran
(`late_grants.rs`'s `a_grant_read_late_takes_no_list_beside_a_newer_lease`);
judged by the lease it would run, as this branch first had it, a router
restarted or promoted with a shorter `--auto-failover` never cut a longer
lease short, failed the node over by its own and left it writing beside
the new primary, and a grant sent before another acknowledging the same
answer, read after it, put its older list back (the review of #343,
`a_router_restarted_with_a_shorter_lease_...`,
`a_late_grant_acknowledging_the_same_answer_takes_nothing`). The node says
whether it took a grant (`"took"`, which a node from before never says and
which means taken), and one it answered and did not take -- the first of a
router restarted or promoted, whose predecessor's lease runs -- is
`Answer::Kept`: the grant acknowledging it goes at once, with the list, and
is newer. Counted taken, the router believed the node held its list, and a
create right after a restart had its schema refused (503) and was taken
back. Two grants make a lease besides, so a grant whose answer says the
lease is short -- none, or under half a lease, what a node new, restarted
or back from a failover holds -- is followed at once by the one
acknowledging it too (`Leases::short`): left to the next round, a tenant
created with its schema was refused it (503), and so was the first write
after every create in the lease tests. A router from before numbers sends
none and its grant runs from the reading, as before, and a node from
before them ignores them (`Leases::unnumbered`, said once): leased the old
way rather than refused or failed over never, so a rolling upgrade in
either order keeps every node writing and failed over on its own, the late
grant open where either side is old. The node's lease runs on the boot
clock (`lease::Moment`: `CLOCK_BOOTTIME` on Linux, `mach_continuous_time` on
Apple's systems through `clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)`,
declared by hand), which counts a suspend: on `Instant`'s
(`CLOCK_MONOTONIC`, `CLOCK_UPTIME_RAW`) a node suspended past its lease
woke with the lease left as it was and wrote beside its tenants' new
primaries. It fails closed, as std's `Instant::now` does: a boot clock that
stops answering panics, Linux decides once whether it has one (before
2.6.39, `Instant` throughout) -- a failure read past from call to call mixed
two clocks' moments, and a timebase guessed at 1 / 1 made an Apple silicon
lease 41.7 times as long. The router counts on `Instant`, which a suspend
of its own machine stops: it fails over later, never sooner. It cost a
write nothing: a fenced put 555-560 -> 548-552 ns on an M1, about 7 ns
saved, eight runs a side in ABBA turns (`make fence-bench`), and in
Docker's Linux VM (musl), where `CLOCK_BOOTTIME` is read through the vDSO as
`CLOCK_MONOTONIC` is, 1 488-1 677 against 1 472-1 720, quartet by quartet
the same at loads of 34-46 from other work; the clock as it stands, read
again at loads of 50-88, lower in every M1 quartet (804-880 against
821-1 114) and the same on Linux. No runner suspends, so the
clock's test reads it between two reads of a boot clock the system vouches
for -- `/proc/uptime`, `mach_continuous_time` -- which CI runs in a time
namespace whose boot clock is a day ahead of its monotonic one (`unshare
--time --boottime`, the test named whole and its one pass asked for):
reading `CLOCK_MONOTONIC` it fails there, and reading `CLOCK_UPTIME_RAW` on
a Mac that has slept. And a write landed under the lease is not answered as
a success once the lease has lapsed by the time it is answered
(`answered_under_lease`, from `handle_admitted`, the admin schema's route
and a `/_changes` consumer's commit): a process stopped between the fence
and the append, a suspend, or a durability that outlasted the lease -- a
cut stream waits out `SENT_WAIT`, a second, where a lease's tenth is half
of one at the default -- had it answered 200 and lost in the failover. It
is answered 500 with `"outcome": "unknown"`, and `seq` and `history` in the
body rather than in `Fenec-Seq`, which a client would take as its own (Go's
and .NET's code `outcome_unknown`, JavaScript's and Python's `outcome`); a
keyed one is told to send it again with its key, which the lapsed node
refuses before the key is looked up and the tenant settled replays or
makes once, and one with none that a read-back means something only a
lease and a tenth after the answer, the node answering reads with the
write in them until its tenant settles. What wrote is a statement that
writes (`metrics::writing`, a keyed replay's too) or a block landed in the
request (`metrics::landed`, set by `Hub::landed` on the committing thread):
a `/batch` holding a compact, a put landed and a later statement fenced,
was told 503 to come back as if nothing had. Both flags are cleared as each
request begins (`metrics::begin_request`): cleared by `record` alone, they
outlived the admin schema's answer, which records none, and the next read
on its keep-alive connection was answered as a write once its lease
lapsed under it (`tests/lease_answers.rs`, each failing with its part taken
out). What
leases do not cover: a VM stopped by its host is no suspend to the guest --
under QEMU/KVM's defaults a `stop` and `cont` (QEMU sets the kvmclock back,
on arm64 the virtual counter), and under any of its settings a save and
restore or a live migration's blackout, leave every clock in the guest
where it stopped, the boot clock too, as do Hyper-V's saved state, VMware's
suspend, Firecracker's snapshot restore and a container's CRIU restore,
whose time namespace keeps the clocks going on from where they were -- so a
node paused so wakes with lease left and writes beside the new primary
until it runs out or it reads
the router's first grant after the failover. Every signal inside the guest
comes once it runs again and none is sound -- KVM's `PVCLOCK_GUEST_STOPPED`
is the soft-lockup watchdog's, in no interface userspace has; the TSC
against the monotonic clock hangs on the VMM's settings and misses a
migration; the wall clock is set by an agent after, or slewed -- so none is
built; a cloud's notice comes ahead (Azure's Scheduled Events `Freeze`,
GCE's `maintenance-event`), and the RUNBOOK's known gaps say which pauses
count; an answer that passed the second check goes out
whenever the process runs again; a stall of 16 rounds or more can push the
acknowledgement out of `answered`, the node holding no lease until the
grant after, sent at once; two directories leasing one node -- a
misconfiguration -- can take it in turns as each other's lease lapses,
neither holding one for long. A router started with a shorter lease than
the one before it -- restarted, or a standby promoted -- counted a node it
never reached by its own: cut off across the change, the node held the
longer lease and wrote while its tenants were failed over. The directory
records the longest lease a router leasing from it may have granted
(`leasing`, one row, written before a router's first grant where its own
is longer -- `Router::grant_term`, once a process; no block is written at
every start to ride in -- and sent to the standbys before any grant, as a
move's flip is, `SENT_WAIT` at most); a router counts a node by that record
(`Leases::inherited`, read as it leads) until the node has run a lease from
one of its grants (`Held::own`, set by `Leases::ran` on the node's
`"ran":true`, never by `Kept`, never by a take of the list alone -- the
answer read as the object it is, `Granted`: looked for as bytes,
`"ran":true` was sought in windows a byte too long and never found, so
every node was counted by the record until it came down, and a router
restarted at 3 s over a record of 10 s failed a node it had cut off over
11 s after it led, not 3.3 s after the node's last answer --
`late_grants.rs`'
`a_node_that_ran_the_router_s_lease_is_counted_by_it_not_the_record`;
#343's tests held only the other side), and
lowers it to its own once it has led a lease and a tenth of it
(`Leases::lowerable`): every lease granted before it led has run out, and
one started after it counts by what may still run. Never lowered, one 20 s
lease would have every router after it wait 22 s for a node it never
reached. Set by any `Taken`, a grant taken for its list alone -- a
restarted router's first, acknowledging nothing, on a node whose lease had
lapsed -- had the node counted by the router's own lease while a held grant
of the router before, acknowledging an answer the node's ring kept (a
restart keeps the holder), ran the longer one again: two primaries (the
review of #343's probe, `a_take_of_the_list_alone_...`). The node says
`ran` as it says `took`, and a node from before says neither, so it stays
counted by the record. A raise that cannot be written grants no longer than
the record, or this router's own where there is none: refused, every
node's lease lapsed and every node was failed over by a router whose
directory then refused the failovers too. The leasing thread's rounds come
a third of the lease granted apart (`Leases::granting`, the thread woken
as a grant finds it shorter, `grants_short`), and its first at once: a
third of its own apart, a 20 s router over a 1 s record granted a second
every 6.7 s and the node held no lease in 158 of 187 samples, and the
first round a third of its own lease after the start left a node without
a lease from the end of a shorter one before it -- a router whose lease
the record was raised to as well (`late_grants.rs`'
`a_router_that_cannot_record_its_lease_renews_at_the_lease_it_grants`, 84
of 113 samples without on e5b8413e). Each round after the first is due a
third after the last was due, not after the last woke
(`lease::round_due`), and one woken more than a third late counts from its
wake, so at most two rounds come together -- one woken just under a third
late, followed at once by the next, due as it starts -- and never the run
of rounds a stopped process missed: counted from the wake, every wake's
lateness put the rounds after it as late, and at background QoS, where a
park of a second wakes up to 0.2 s late, the rounds of a 3 s lease came
1.18 s apart on average and two of them up to 2.40 s, a node's next grant
left 0.6 s to be late in before its lease lapsed; 1.00 s and 2.04-2.18 s
now (a stand-in node timing the grants it was sent, four runs a side in
ABBA turns, the same beside six busy loops). `lease_stalls.rs`'
`the_leasing_thread_s_rounds_are_due_from_when_the_last_was_due` holds the
leasing thread to it by what it works each round's due out as, not by a
clock (`Router::rounds_on_schedule`). A thread finding the record raised
by another of this router's waits for the standbys too before its grant
(`standbys_sent`, at once once sent): each node's grant is a thread, and
the second node's went while the first waited. What it does not cover: the first
start after upgrading from a router without the record finds none and
counts by its own lease, and a record is a directory's, so a node moved from
the global router's leasing to a region's is counted by the region's alone
(the RUNBOOK says how to lower a lease and move a node). A router from
before the record writes and reads none (`late_grants.rs`'
`..._waits_out_a_node_it_never_reaches`, a restart and a standby promoted
at 3 s after 10 s, each failing with the record ignored or never written,
and `a_router_that_cannot_record_its_lease_grants_no_longer_than_the_record`).
Those tests' shorter lease is 3 s, as the other lease tests' is: at 1 s,
once a node that ran it was counted by it, the healthy nodes beside the one
cut off -- each grant bounded by a third of the lease, 333 ms -- went
unrenewed past 1.1 s at background QoS and were failed over, the replica
their failover dropped leaving the cut-off node's tenant nowhere to go.
Cut off, a node
stops about a round sooner, its lease run from the grant before the last it
read: at a lease of a second it stopped 386-406 ms after the cut against
main's 714-777, its 10 tenants writing again on their replicas 1.27-1.36 s
after against 1.26-1.42, four runs a side in ABBA turns; in a region keeping
its own directory 0.38-0.63 s and 1.41-1.70 against 0.67-0.98 and 1.35-1.73,
ten a side (at 5 s the bench cuts before the first round, so the node's
lease runs from its tenants' creates' grants: 3.9-4.1 s against 3.9-4.3). A
node's first create with a schema, its second grant sent at once, took 8.8
ms either way, eight a side; the router's added p50 stayed 20.5 and 20.6 us
against 20.5 in two sets of sixteen ABBA rounds (per round -0.3 to +0.4,
one +1.3; nothing on the path changed), and `fenec-shard`'s `__text` grew 64
bytes, `fenec-server`'s 2.3 KB. The fence is
the engine's (`Database::set_fence`), asked in `commit` as a block lands and
in `may_write` before a schema change or a maintenance, so every write path
honours it; asked as a lone write started too, it took a `put` from 827 to
898 ns, once 822 to 846. The lease names
the node's primaries, not the node: a node back from a failover writes none
of its stale copies before the repair its answer triggers has them follow.
The list rides only when its epoch (FNV of the sorted names) changed, and a
node that does not hold it answers 412; a create or a move grants the new
list at once. Grants go out each on a thread of its own over a pool
bounded by a third of the lease, and a round waits for none of them
(`Router::round`): it waited for every grant before it looked for a lapse,
and in V5's drill a grant to a node gone, recorded by a name its resolver
was slow to refuse -- the lookup bounded by nothing -- held the round up
until the region's healthy nodes stopped writing (18 s in CI) and were
failed over themselves, the dead node's tenants writing again 30 s after
the kill. To one node a grant goes one at a time, a round sending none
while one of its own is under way (`Leases::begin_grant`; sent anyway,
one that took its whole bound had another queued behind it every round),
and never with a list read before the last one it was sent
(`Leases::turn`, each reading of the directory numbered): a round reads
every list as it starts, and its grant, landing after the one a create
sent at once, put the list from before the create back, the new tenant
refused on its node (503) until the next round. A lapse a round suspects
(`Leases::suspects`) is judged again in the node's turn by a failover on a
thread of its own (`fail_over_lapsed`, `Leases::confirm_lapse`): no grant
to the node that the router still waits on is in flight then -- an answer
that comes back says the node is there, which the leasing thread had
because it ran the failover between rounds; one the router gave up on runs
nothing -- and the turn is held
until the node was sent the list the failover left, so no grant over a
list read before it lands after; it is let go of before the other nodes'
grants are waited for, or two nodes failed over at once waited on each
other's turns for good. A repair runs on a thread of its own, one at a
time (`repair_soon`): on the leasing thread, a node back from a failover
had it call a node gone, 5 s a call for its name's lookup alone, 20 s with
no lease renewed, and the healthy node it ran for failed over at its end;
run in the grant that heard the node back, it held that node's renewals
and had it failed over again. A tenant whose claim another operation held
as its node failed over -- a repair, which runs beside the failovers now,
a quota being set, a move -- is failed over once let go of
(`fail_over_held`, waiting on the claims' condvar), in the node's turn and
only while the node has answered nothing since: left, as a failover by hand
leaves it to be asked again, it stayed on a node gone for good. It goes to
the copy it had as the failover ran and to no other (`HeldTenant`, pinned
then, and only while that copy is still its own): read again once let go
of, the copy was the one the repair holding it had just attached,
following the node gone, and the tenant went onto a file holding none of
it, its rows wiped from its old node once that came back and followed it
(the review's repro). It is out of the node's grants from the failover on
(`Leases::withhold`), which would hand it back to the node as it answered
again, and promoted no sooner than a lease and a tenth past the node's last
answer -- at once, the failover's lapse being that; for a node from before
numbered grants, which runs a lease from any grant it reads, past the
router's last grant to it, answered or given up (`settled`), as before
them, the late-grant hole's window a grant naming it stretched to the
whole wait. The wait ends at `HELD_WAIT` (an hour), the node gone from the
directory or this router demoted -- `promote_on` asks `following()` before
every promotion -- the tenant left where it is and named again; and a pair
ends once its node holds no tenant, promoted, moved or deleted alike. Two
guards besides, older than all of it. A node refuses to promote a copy that
is not its tenant as the router knows it (`tenants::Expected`, the
promote's body: 409, the tenant left where its rows are until its node is
back): of another lineage than the directory's -- a copy that never took
its primary's image is of its own, a new file's or none on a standby, and
so is a file put in its place by hand -- or lacking the history the
tenant's primary forked at its last failover, which the router records from
each promotion's answer (`lineages`' `history`, added as an alter, cleared
where the lineage is succeeded) and not heard from the node it follows
since it was last told to follow it: an old primary's copy a repair made
follow the new primary, promoted before it heard from it, took back every
write since that failover -- its lineage the tenant's and its change past
0, so neither caught it. The hello is the copy's own word, kept in
`.follows` (`heard`, written at the first hello after each `follow`, which
halts the follower before it writes the file again: `mark_heard`,
`heard_since_told`; renamed and removed durably, `fs::sync_dir`, or a power
loss could bring back the file before), since the history alone goes stale: a primary's file
restored in place to before that failover (`DELETE` and `PUT .../file`,
`fenec restore --tenants`) keeps its lineage and lacks the fork, so does
every copy that took it, and every failover after was refused, on a lease
for good (the review's `restore_trap.rs`). Kept across a restart, where
`ready` is not: a node restarted between a follow and its hello still
refuses the copy, one restarted after it still promotes it. A copy never
told to follow -- a standby's, made with its node -- keeps no word, and is
held to the history alone. A tenant whose lineage the directory never learnt, or a
call naming none, is held to the one copy known to hold nothing (no write
and no hello since it opened or since it was told to follow). A file of
another lineage put in its primary's place by hand still leaves every
failover refused until the directory learns it (a read replica's look) or an
operator forces one. Refused at change 0 as this first stood, a
tenant not yet written could not fail over at all, its copy ready and of
its lineage (the review's `empty_refused.rs`). `{"force": true}` on `POST
/_admin/tenants/<t>/promote` or `POST /_shard/nodes/<n>/failover` promotes
whatever the copy holds, an audit event on each (`promote_forced`,
`failover_forced`), and `promoted()` then records the other file's lineage
as the tenant's, the one before among those it was. And a repair gives no
tenant a copy while its primary does not answer its `GET /_admin/tenants`
-- one that ran between a node's death and its lapse attached a copy
following nothing, which the failover promoted and the node's return wiped
the tenant into, on main -- and runs again for those it passed over
(`repair_later`, 2 s doubling to 60, eight times): a repair runs only when
a node failed over answers again or an operator asks, so a primary slow at
the one look left its tenant with no replica until then. A name is
looked up within the call's own bound (`fenec_http::dial`, the
router's connections below). In deploy-kit's drill at a lease of 2 s, the
routers' unknown names sent to a resolver that never answers, us-a's
failover was done 2.7-2.8 s after its kill in three runs against main's
5.3, 5.8 and 15.7 s, the region's other tenants' longest gap 17-19 ms
against 8.0-18.0 s, and no healthy node failed over against one or two in
each of main's runs (us-a's tenants writing again 21-26 s after the kill,
one run failing its repair); with every container's unknown names so, the
follower's lookup of the dead primary hanging too, 2.1-3.3 s in three
against 5.7; by names that resolve, 2.0-2.6 s either way. What the drill's writers
waited past that is the router's `--upstream-timeout` on a request to the
dead node, cut by their own 5 s: at `--upstream-timeout 2` they wrote
again 1.96 s after the kill on both builds -- a wait the failover now ends
itself (what a failover ends, below the router's pool). `make shard-bench`'s failovers
as they were, twelve runs a side in ABBA turns at loads under 3 with all
of the above in, the lineage and history sent and recorded among it: a
lease's last tenant writing again 1.28 s after the cut against 1.29
(1.23-1.57 either way, the round's phase), a region's at a lease of 1 s
1.59 against 1.60 and at 5 s 6.20 against 6.21, a pair's 20 tenants 63.5 ms
either way, one node's 10 over their replicas 52 either way, the repair
after it 241.5 against 238, the router's added p50 20.6 us against 20.4. A
standby router forgets
its leases while it follows and counts from its first round once promoted
(`Leases::leading`): it counted from its last round while it followed, up
to a third of a lease before the promotion, and a grant its primary sent
after that round could still run as it failed the node over. `following`
starts set, so one promoted before a round of its own counts from its first
round too, and a router primary from its start from its first round, a
third of a lease after it at most. A node added since counts from its
addition (`Leases::entry`), which no lease of the directory's can have
named before: counted from the router's start, a node added a lease and a
tenth into the router's life was lapsed before its first grant came back,
and with that grant given up on -- a second at a 3 s lease, at background
QoS beside busy loops -- it was failed over at once, every replica it kept
dropped, as `lease_stalls.rs`' traces showed; the count moves on only with
an answer heard, either way, so it never comes sooner than before. A node without
`--lease` is never failed over on its own. A lease of a second: a node cut
off stopped writing 386-406 ms after (697 when it ran its lease from its
last renewal), its 10 tenants took writes again 1.27 to 1.36 s after. A
write is on the standby 0.089 ms after the primary answered it (p99 0.448),
and 20 tenants failed over in 63.5 to 79 ms, the medians of three rounds
(`make shard-bench`). A standby waits
idle and takes all of a node at once, so with `fenec-shard --replicas` a
tenant on a node in no pair gets a replica of its own instead (the
`replicas` collection): on the node holding the fewest replicas of its node,
then the fewest in all -- the fewest in all alone had every tenant of a node
follow on the same other, ties going by name. The node is told
(`POST /_admin/tenants/<t>/follow {from}`) and keeps the URL in
`<t>.follows` beside the file for its restarts (`resume_following`); a file
made to follow ends what it served first (`release`, as a delete does), or
the old primary's feed held it. A failover of a node in no pair promotes each
tenant on its own replica, `POST /_shard/replicas` gives replicas back -- on
a node holding a copy first, so a returning node's old primaries follow --
and a move re-points the replica or, onto its node, places another. Three
nodes, 10 tenants each: one node's failover took 57 ms, 5 onto each other.

## Replicas tried again

**A replica whose attach failed is tried again, and a file the router left
on a node is taken off** (`Router::replicate`, `repair_with`, `try_again`,
`take_back_leftovers`, `settle_leftover`, `directory::Leftover`,
`fenec_http::tenants::Tenants::take_back`). With `fenec-shard --replicas`
a create, a move and a repair each give a tenant its replica on another
node: the file made there (`PUT /_admin/tenants/<t>`) and told to follow
the primary (`POST .../follow`). An attach that failed -- the node refused,
or took the follow past the router's `--upstream-timeout`: a node's rename
of `.follows` took 13.3 s beside eleven other suites, and the deploy kit's
routers wait 10 -- was logged and named in the answer's `unreplicated`, and
nothing tried it again. `repair_later` ran a repair again only for a tenant
whose primary had not answered the repair's look, a repair's own failed
attach went into its `failed`, and a repair runs only when a node failed
over answers again or an operator asks: the tenant had no copy to fail over
to until then, and under `--auto-failover` stayed on its primary as that
died, writing nothing until the node was back. Now an attach that fails, on
a create, a move or a repair, has a repair run again in a while
(`try_again`: 2 s, then doubling to 60, eight in a row, about four
minutes), as a tenant whose primary did not answer had, and so does a
tenant another operation held as the repair came to it; a tenant whose
region has no other node free to keep a replica is not tried again
(`Unplaced::Nowhere`: nothing a while changes gives it one). The try after
places once: the repair asks again under the tenant's claim whether it is
still on that primary, active, in no pair and without a replica -- a
create's own try, a move or a failover may have given it one, or taken it
elsewhere, and two repairs may run at once, an operator's beside the
router's -- and records one replica, as before. And it adopts what an
earlier try left: where the follow landed after the router gave it up, a
copy follows the primary that no directory names, which the repair's look
sees (`GET /_admin/tenants`) and whose node ranks first among those of the
zone the rule picks (`replica_node`'s `holding`, below the zone), the `PUT`
finding the file (409) and the follow beginning its stream again. A node
that did not answer the repair's look is given no replica (`replica_node`'s
`answering`, the repair's alone): it would follow nothing from there, and a
try at it each repair was a call bounded by the upstream timeout, the
tenant without a copy meanwhile; free nodes none of which answered are a
failed try, tried again. The rules a placement keeps are the repair's own,
unchanged: a node of another zone first, never a standby or a node in a
pair, none while the primary does not answer its look, the claim held
across the calls -- so a failover on a lease finding the tenant held pins
the copy it had then (`HeldTenant`), none where it had none -- and the
replica recorded only once it follows.

The tries are counted by reason (`Retries`, `Again`): a tenant without its
replica, and a file left on a node. Counted together, a file on a node
silent for good held every repair at 502, put a fresh failed attach at the
end of its own minute-long waits, and after eight repairs stopped the tries
for every tenant. Now a fresh failure is due `REPAIR_AGAIN_FIRST` after it
whatever ran before, each reason backs off and gives up on its own, a
failure that comes while a repair runs stays due though that repair left
nothing of its reason, and only a tenant without its replica answers a
repair 502 -- the files stay in its `kept`, each with why. A router without
`--replicas` places no replica unasked: a repair it starts itself for a
file left on a node settles the files alone (`repair_with(false)`, where a
take-back that failed had it give every tenant in no pair a replica 2 s
after), and one an operator asked, or a node back from a failover, runs
whole as on main, its own tries after whole too.

A file the router made on a node, or meant to take off one, that no
placement records is noted in the directory (`leftovers`, keyed
`<tenant>/<node>`, with the lineage and change the node said the file stood
at as the router left it, where it said; replicated with the rest, so a
standby promoted knows them): a create taken back -- its placement
unrecorded, its quota or its schema refused -- whose `DELETE` the node did
not take; a create whose `PUT` went unanswered, which the node may have
made all the same; a move undone after its image was installed on the
target (its quota refused there, the flip unrecorded) whose target kept
the copy, and a move whose source would not go once it was done; a replica
whose attach failed once its `PUT` was answered, made (201, where it stood)
or found (409, of no lineage known); a replica detached, or a deleted
tenant's replica or standby copy, that would not come off; and an old
primary's file on a node that did not answer the repair's look, the node a
failover took the tenant off (`failed_from`), where the tenant's replica
went elsewhere -- answering, the node would have kept the replica, its
file made to follow (`note_old_primary`). A later note that knows less, of
no lineage, keeps what was noted. The `DELETE`s that took a create back
ignored their failure, so the file stayed: a create of the name sent again
after the fence's 503 met the node's 409, and so did every create of it
there after.

Each repair, after its attaches, settles every one under its tenant's
claim (`settle_leftover`): forgotten where the directory gives the tenant
that copy now -- placed there, its node's standby, its replica, a read
replica -- or where the node holds no such file; left as it is while its
tenant has no copy, the router gives tenants replicas, and the file is of
another lineage than the tenant's, which the next try adopts (a move's
copies hold the tenant's own lineage, and a move's source stays frozen,
which refused every follow); and taken back otherwise -- the tenant made
elsewhere, deleted, or given its copy on another node -- by the node. The
router asks `POST /_admin/tenants/<t>/take-back {lineage, seq}`, and the
node deletes the file only where it follows a node -- a copy, whose writes
are its primary's -- or stands exactly where the router left it: of that
lineage (any, where the router knows none) at a change no later than
`seq`, 0 where the router knows none, so a file of no lineage known goes
only while it holds no write. Judged under the slot's lock the delete
takes, as `delete_refused` judges, so nothing lands between, and refused
409 (`"outcome": "kept"`) otherwise, or where a sync log beside a closed
file holds durable writes the file does not. Judged by the router, as this
first stood -- the names a node listed, a lineage asked where one was
known, then a plain `DELETE` -- a standby promoted behind its primary took
back a tenant the primary had since made on the node and written (the
review's reproduction: its repair answered the file taken back and the node
then had no tenant; a create through it made the tenant again, empty), and
any file put under the name by hand; and a lineage alone would not have
covered a move's copies, which a later move onto the node carries too. So
the router never deletes a non-following file holding writes it did not
leave: kept, the file is named `kept` by every repair, for an operator,
and tried no more until one. A path of its own rather than a parameter on
`DELETE`: a node from before it answers 404 and deletes nothing -- read
past, a parameter would have had it delete whatever stood under the name
-- and the router keeps the file, named so, and sends no `DELETE` in its
place. The immediate take-back inside a create or a move that fails is the
plain `DELETE` it was: the operation made the file a moment before, and a
tenant is there with its schema or not at all.

What settles a file holds the tenant's claim across one call, the
take-back, through the read replicas' pool, bounded by `reader_timeout`
(10 s) rather than `--upstream-timeout` (60): a failover on a lease that
finds the tenant held waits for it (`fail_over_held`), and the lineage
asked first was a second call besides. Not let go of across it: a repair
attaching the tenant's replica there between the check and the call would
have the copy it made to follow taken back, as a copy following a node.
Two placements hold no claim, so they are asked of besides: a read replica
is recorded only once it follows, under its change's own hold
(`reader_busy`), which the take-back takes for its tenant and region for
as long as it is out; and a pair's standby is recorded before its copy is
made, which the directory's second look, the hold taken, sees. A create
meeting 409 on a node holding such a file has it taken back first and
makes the tenant there (`clear_leftover`); any other file of the name, or
one holding writes since, is not the router's to touch, and the 409 stands.
Both a repair's tries and the create's, rather than either alone: a thread
retrying each take-back would have repeated the repair's backoff and its
claims, and forgotten the file at a restart or a promotion, where the
directory's record outlives both and a create of the name meets it however
long after; the create's cleanup alone would have left the file on the node
for good where nobody made the name again. A record a router from before
`leftovers` neither reads nor writes; a standby promoted over an older
primary's directory makes the collection as it notes the first. A node's
`PUT /_admin/tenants/<t>` and `PUT .../file` answers name the change the
tenant stands at (`"seq"`), beside its lineage; a router from before reads
past it.

`fenec_router_tenants_unreplicated{region}`, with `--replicas`, counts the
tenants with no copy a failover could promote -- no replica of their own and
no standby on their node (`Directory::uncopied`, one pass at the scrape, as
`same_zone_copies` is), every region a node names at 0 or not, no tenant
in a label, the unnamed region unlabelled as `same_zone_copies` sends it --
and `FenecTenantsWithoutReplica` reads it, its summary naming the region
where there is one: the rule was the difference of `fenec_router_tenants`
and `fenec_router_replicas` by router, which counted a paired node's
tenants, whose copy is the standby's, as missing theirs. A router without
`--replicas` sends none, its tenants in no pair kept with no copy by design.

`tests/replica_retries.rs`, a binary of its own (in `all` its proxies'
threads took the binary past macOS's threads a process, and other tests'
spawns failed -- as `tests/read_replicas.rs`, apart since #372, would have
again: its tests held up to 1 195 threads at once and the rest of `all`
868, together 2 028 to 2 045 of the 2 048, past them once the repairs its
routers' failed attaches run again went on beside), puts a proxy before a
node that refuses a request (503),
answers it itself, holds it, or sends it on and swallows the answer, the
connection ended with nothing said, and holds, each failing with its part
taken out (in parentheses, what it met then): a create whose follow was
refused once given its replica by the repair after, the follow asked twice
in all (the create's try not tried again: never); a repair whose `PUT` was
refused, leaving no file to wait on, tried again (its own failed attach not
tried again: its 502 and nothing after); a follow that landed after the
router gave it up adopted -- the copy's node and another of its zone by
then, first by name, which a try blind to the copy picks -- with no second
copy (the copy's node not preferred: one on the other); two repairs at
once, the first held at one tenant's follow while the second gives the
next its replica, giving that one no second (no look again under the
claim: a second on the node with fewer of the primary's replicas, the first
left following unrecorded); a node silent at the repair's look given no
replica though first by name (the silent one given it); a copy left where
the replica went to a node of a better zone taken back (left following); an
attach that found an empty file put there by hand (409) and failed, the
file taken back once the replica went elsewhere (noted on 201 alone: kept
for good); a create taken back whose `DELETE` was refused leaving no file
once the repair after ran, a move undone the same, a move's source that
would not go once the move was done, and a create whose `PUT` went
unanswered (the take-back noting nothing, or the leftover never taken
back: the orphan, for good); a create meeting that file, the repair
kept from it, taking it back and making the tenant (its own leftover not
cleared: 409); a deleted tenant's replica whose node refused its `DELETE`,
and a detached one whose move went to a node of the copy's zone, taken back
(left for good); the review's standby promoted behind its primary, its
directory copied as a create's `PUT` went unanswered, taking back nothing
of the tenant the primary made since and wrote, and refusing a create of
the name 409 (the node judging nothing: the tenant gone, and made again
empty); a file of another lineage put under a known leftover's name by
hand, and one with writes in place of a file whose lineage the router
never heard, both `kept` with their rows (the node judging nothing: the
second deleted); a node from before take-backs, its 404 stood in by the
proxy, keeping the file with no `DELETE` sent in its place (the 404 read
as absent: the file forgotten unsaid); a router without `--replicas` whose
take-back failed giving a tenant in no pair no replica (a repair it starts
itself run whole: a replica 2 s after), and pairs alone, a file left on a
node in no pair taken back though its tenant, made again elsewhere, has no
copy (adoption asked without `--replicas`: left for good); a file left on a
node silent at the look failing no repair, named `kept`, and taken back
once the node answers (the files counted as a tenant's failure: 502); an
old primary's file, its node silent as its tenant was given its replica on
another, noted and then `kept`, its rows intact (unnoted: no word of it);
and the gauge counting the tenant and clearing as the try lands (none sent:
`datadog.rs`' rules fail on it too). fenec-http's `tests/tenants.rs` holds
the node's take-back to a file open and closed: another lineage and a write
since kept, the file as left gone and then absent, a closed empty file of
no lineage known gone and a written one kept, a body naming no change
refused. The lib's tests hold each reason's tries to its own: a file's
spent tries and minute-long waits neither delay a fresh attach's 2 s nor
stop it, a failure while a repair ran stays due, a repair judging no
replica leaves that count. `directory::tests` hold the count by region and
a pair's standby as a copy, and the records kept across an open, noted
once, a known lineage kept against a note of none, and made where a
directory lacks the collection. What is not held by a test of its own: the
read replica's hold and the second look -- a race between a look's follow
and a take-back, which the claims around them leave a call wide.

Nothing of it is on a request's path: the tries run on the repair's
thread, an attach that lands reads the directory's `leftovers` once more,
and a create, a move and a delete write one only where a call failed.
`make shard-bench ARGS=latency`'s added p50 stayed 20.6 us (20.5-21.0)
against main's 20.55 (19.5-20.7), eight runs a side in ABBA turns at loads
of 1.5-2.2, the quartets' differences +0.3, +0.65, 0 and -0.05 us; a pass at
loads of 2.7-2.9 from another agent's bench moved both sides alike, 27.8
against 27.35. `fenec-shard`'s code grew 33.0 KB (`__text`,
aarch64-apple-darwin, `size_report.py --bin`): the files left and their
take-back about 19 KB -- `settle_leftover` 6.2, the directory's calls 9.9
(`write_block` out of line now that two call it, 3.5), `clear_leftover`
1.5, `take_back` and `left_on` 1.7 -- the move 5.3 net (`relocate` +9.2,
`promote_on` -3.9, inlining moved), the retries' waiting thread 3.1, the
gauge 1.3, `attach` 1.7; the repair is one copy where main held two
(`repair_with` 14.8 against `repair`'s 9.5 and the 8.7 inlined in
`admin`). `fenec-server`'s grew 7.4 KB: the node's take-back, its route
and the `seq` its answers name.
