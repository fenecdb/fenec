# Read replicas

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Read replicas*: a tenant's read replicas in other regions, the reads marked for them, and reading one's own writes across a failover. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Marked reads

**A tenant may keep read replicas in other regions, which a router near
them answers the reads its clients mark from** (`fenec-shard/src/readers.rs`,
`PUT /_shard/tenants/<t>/read-replicas/<region>`, `fenec-shard --near`,
`Fenec-Read: replica`; `site/content/docs/sharding-read-replicas.html#read-replicas`). A
client in Singapore of a tenant living in Frankfurt paid 158 ms a read. A
read replica is a follower of the tenant's primary node (`POST
/_admin/tenants/<t>/follow`, as a replica of its own) on the node of the
region holding the fewest tenants and copies, never a standby, one a region
and none in the tenant's own; a resource a region rather than `{regions:
[...]}`, so each change touches one directory. Its record is a row of
`readers` (`key` `<tenant>/<node>`, `follows` the primary's address) in the
directory holding its node -- the global one or the region's own, whose
router places it, leases its node and routes to it through a partition;
another router relays the request there, and the global router gathers the
list from every directory. It is recorded once its node follows -- recorded
first, a marked read reached the file the `PUT` makes, a primary of its own
holding nothing -- and a node that will not follow has that file deleted
again (`tell_follow`); taken off, the record goes first, the standby routers
are sent that (`standbys_sent`), then the file, and a node that does not
take it off has the record put back and the `DELETE` answered 502, a copy
following unrecorded being a follower nothing stops (`?forget` for a node
gone for good; a node holding one is not removed, `Directory::remove_node`).
`Directory::remove_tenant` keeps the rows: a tenant leaving for a regional
directory keeps the read replicas here, which follow it, and the delete
paths take them off with their files (`take_readers`, all at once), a
replica whose node does not take it off keeping the tenant, 502, as a
`DELETE` of the replica keeps its record: deleted regardless, the record
outlived its tenant, and the look that found the name made again --
another owner's -- pointed the old copy at it, which took the new tenant's
image into a region nobody had chosen. Another directory's go at its
router's next look, so a create of the name is refused, 409, while any
directory keeps a trace of one -- a record, or a change of one under way,
whose record is out while its node is asked to take the copy off
(`reader_traces`, `changing` in the list) -- the global router asking each
region keeping its own, 503 where one is not reached (`no_reader_traces`);
and a marked read goes to no read replica of a tenant no directory the
router reads places (`reader_near`). The regions asked are only those
that may keep one: a region's router tells the global router as it places
one, before the copy exists (`trace_globally`, `PUT
/_shard/tenants/<t>/traces/<r>`, a row of the global directory's `traces`
read where asked and never mapped), and once its last is taken off, under
the replica's hold (`untrace_if_none`); a create asks those regions at once,
each through the pool bounded by `reader_timeout`, takes off the trace of
one that keeps none, and is refused 503 for that name alone where one is
not reached or answers from a router following the region's, whose list
says `changing: null` -- its holds are not the primary's, and `false` let a
create go on while the primary took a replica off that its node then put
back. Every region asked one after another, one region's router out of
reach refused every create in every region, and hung, after the upstream
timeout's minute; a name no region traced now asks none, a create costing
what main's did. The way out for a region gone for good is `DELETE
/_shard/tenants/<t>/traces/<r>` by hand, which the 503 names, and it
cannot copy a new tenant of the name there: a read replica is bound to its
tenant's lineage (`lineages`, recorded from the node's answer to a create
or an import, `"lineage"` in `POST /_shard/placements`; the record's
`lineage`), which every `follow` tells the copy and `.follows` keeps
(`lineage <8 hex>`, `Follower::expect_lineage`): a copy whose primary is of
another lineage takes nothing from it and deletes itself, file and
`.follows`, through the registry it is served from (`refused_copy`,
`Tenants::shared`, `fenec_copies_refused_total`) -- stopped and kept, it
held the deleted tenant's rows on the node for anyone to read -- and a look
finding a replica's tenant of another lineage takes it off
(`another_lineage`). The delete is the copy's that refused alone:
`delete_refused` asks, under the slot's lock it deletes under, that the
tenant is still open as the open whose follower refused (`Tenant::opened`),
following, its follower refusing (`Said::Refused` names the lineage) and
`.follows` naming that lineage -- asked of `.follows` before the slot, a
promotion of the copy or a move installing the tenant there landing
between had the primary deleted (`tenants::refusals` holds a refusal
there for a test). Only a read replica is told a lineage, and a copy that
never had its first image holds nothing to lose. A primary of another
lineage than the directory's, where the directory places the tenant, is
the tenant -- every read and write of it goes there: another file of it
promoted by force (a lineage of its own: `fork_id` keeps the first id's top
half only where there is a first id), an older file restored, a file put in
place by hand. A copy of another lineage -- one that never took its first
image among them -- is not promoted but by force: its node, told the
directory's lineage, refuses it, where it was once promoted into an empty
primary. A failover records the lineage the
promotion answers (`promoted`, `"lineage"` of `.../promote`), and a read
replica's copy lost with nothing else changed has the primary's node
asked first (`verify_lineage`, through `verify_primary`; another region's
router or the global router asked by `POST /_shard/placements {verify}`):
the directory takes the node's word, the lineage before kept as one the
tenant was (`succeed_lineage`, `lineages.was`), and a replica bound to one
it was is bound to the new one rather than taken off (`bound_before`).
Bound to the old, the copy refused, deleted itself and was made again by
every look, for good. A tenant made again through a router keeps no
`was`: its create records a lineage of its own. A copy lost three times
in a row anyway -- refusing a primary whose lineage the router cannot
learn -- is made again no more (`REMAKES`, `may_remake`): said once,
`"given_up": true` in its list and `fenec_router_read_replicas_given_up`,
until its primary or its lineage changes or an operator places it again.
`reader_near` passes it by: still asked, every marked read of its region
went to its node, was answered that the tenant was not there and went
home, waking a look. The count is in memory alone -- a router started
again makes the copy up to three times more, and a standby router knows
nothing of it -- which is a loop bounded, not one stopped for good.
A tenant with no lineage known -- made before they were kept -- has it
asked of its node at the next check (`want_lineage`, `backfill_lineages`,
`GET /_admin/tenants/<t>/lineage`, wanted by a look over a tenant of this
directory and by a placement asked of one), recorded only where none is
yet, asked under the write guard that records it -- a create meanwhile
recorded the new tenant's lineage, which the answer of the one before
wrote over. Each of its read replicas recorded with none is bound once its
copy's node says the copy is of it and holds its primary's history
(`"ready"`: a copy not caught up is of a new file's lineage, and was taken
off as another tenant's as it caught up), the copy told with a follow of
the node it follows, and taken off where the copy is another's
(`bind_reader`). Mixed versions protect less and
break nothing: a node from before lineages ignores `"lineage"` and has no
`lineage` endpoint, a router from before them ignores `lineages`, and no
copy refuses on their word. Lineages are kept in the directory's maps
from the change ring (`lineages`), read by every look and placement: read
from the database, each was a statement parsed and run. `readers` took
`lineage` as an alter, as `nodes` took its fields (`READER_FIELDS`), and
is selected by the fields it has (`reader_fields`): the select naming one
a directory from before lacked failed not found, which `rows` read as an
empty collection, every record forgotten -- now only a collection not
there reads as empty. With only the trace to go by, the trace
let go of and the name made again on the very node, the look found
nothing to change and the copy's follower took the new tenant's image; a
`?forget` of a node that came back, and a router lost between the
`follow` and the record, did the same. A trace names a tenant the global
directory holds, under its claim (`set_trace`, 404 otherwise). A node's
worker looks again where a tenant's primary is as it reaches the replica
(`fresh_primary`), not where the look saw it: reached after a slow node's
earlier calls, it pointed the replica at the primary a failover had left.
This directory's own it reads each time, from its maps; one at home in
another region only once the look's answer is a `PRIMARY_FRESH` (1 s) old,
and then for every such replica it has left at once, one call a region --
asked again for each, the worker made a call an item where the look made
one a region.
A failover reads `pairs` and
`replicas` alone, so none is ever promoted. Each is held to its tenant's
quota wherever it is recorded: `POST /_shard/placements` answers the quota
beside the placement, so every look wants each read replica sent the quota
of the directory placing its tenant -- another region's too -- where it
differs from the one it was last sent (`reader_quotas`), and `set_quota`
wants this directory's, answering once the directory records it; placed in
a region keeping its own directory, one took only what the placing router
read, and a quota set later reached the readers of the setting directory
alone. Wanted, never sent there: one sender a node, started as a quota is
wanted and every `CHECK_EVERY` for one that did not land, on a thread of its
own, sends the newest wanted of each replica in turn (`QuotaQueue`,
`deliver_quotas`) -- sent by the quota request and the look themselves, a
node that did not answer held each up for its `reader_timeout` (a move of
another tenant 4.4 s, and after a router's restart, its sent quotas
forgotten, every look), and a look's send, of the quota its round began
with, landed after a request's newer one; this directory's quota the look
reads under the queue's lock (`want_kept_quota`), so a request's want is
never followed by an older one. A send is noted only where no replica was
forgotten while it was out (`QuotaNotes`, a count of forgets beside the
notes): a follow landing as a send's answer came back forgot the note, the
answer noted it after, and no look sent the copy -- a new one, maybe -- its
quota again; a send more is a `PUT` the copy takes as often. **What a copy
answers**: a tenant's copy on a replicating node is `ready`
(`Tenant::ready`) once, since it opened, the primary it follows has said
hello -- its image adopted, or its place in that history found
(`Follower::on_hello`) -- and a copy taking writes always (`promote`
stores it, and `ready` asks the history where the flag says no). Not kept
past an open: a read replica placed a moment ago is an empty file of a
history of its own and answered a marked read as a tenant with no
collections, and a file reopened or pointed at another node may hold a
history the primary's does not continue -- a copy left on a node gone for
good while its tenant was deleted and made again -- so it answers 503, which
sends the router home, until the primary says hello; and a follow of the
node a copy follows already begins its stream again and waits for the hello
the same way, since a router placing a read replica over a file it found
asks for no more than that follow. Kept in `<t>.follows` across a restart,
a standby's copies, which keep none, waited for a hello after every one, and
a pair's failover onto one promoted before it came answered every marked
read 503 at the tenant's home while it stayed open. A replica cut off from
its primary stays ready, serving what it last applied, until it is reopened
or pointed elsewhere. **Read your writes across a failover**: a
change number names a write only within a history, and a promotion forks it
where the copy promoted stood -- a write the old primary answered and the
copy never got is lost, the new primary's next write takes its number, and a
read replica still on the old branch held a change of that number, the lost
write, and answered a read sent after the new one with it. So every answer
carrying `Fenec-Seq` of a database with a history of its own carries
`Fenec-History: <term>-<id>` too (`http::Fork`, the term the lineage's
length; a `NonZeroU64` id, so `Response` grew 16 bytes, not 24 -- past 128 a
`Result` of one was clippy's `result_large_err`), the root's naming nothing,
and `after` answers once the copy holds that change of that history
(`History::continues`): a copy on another branch waits until it is pointed
at the new primary and takes its image, and a primary whose history does not
hold it -- the write lost -- answers 504 at once with its own `seq` and
`history`, which every SDK takes as the client's position, lower or not --
kept, every read near would wait out the replica and home for good -- but
only over the position that read was sent after, compared and set under one
lock or in one turn (a write answered meanwhile is further, and set back the
next read near missed it), and never an earlier term of the same lineage (a
router whose copy of the directory lags a failover sends the fallback to the
old primary, fenced, whose 504 names the branch it abandoned); a change the
caller named (`after=`, `After`) is not the client's to take. A history's
lineage is the top half of its id: every promotion and restore keeps it
(`replication::fork_id`, the bottom half fresh), and a database made anew
has its own -- a tenant deleted and made again under its name starts again
at term 1, below the old one's, and refused as a fenced primary's word, every
read near of a client that held the old position waited its whole wait, then
home's 504, for good. In the id, not a field of the header: the header and
`Response` the size they were. A binary from before keeps no lineage: a
promotion or a restore it makes starts a fresh one, which an SDK takes at
any term -- so during a rolling upgrade a router whose copy of the directory
lags a failover can again set a client back onto the old branch, until
every node runs a build that keeps it. The SDKs
keep the furthest position an answer from home named -- a write's, a
read-only `/batch`'s, `/_changes`'s where the SDK reads it: of another
lineage, a later term of the same, or further in one, so one arriving late
is not the last -- and send it with
each marked read, whose own answers move nothing, since a replica's may be
behind home or on an abandoned branch. A marked subscription and
`/_changes` -- Go's `Read()` and .NET's `AsRead()` copies send the
position on `Subscribe` and `Changes` too, and a router sends any marked
request near whatever its route, a stream copied as it comes -- wait for
it on the copy as a read does, and their 504 sends them home: both were
answered before the wait, a stream seeded from the replica as it stood,
behind the client's own write or on the branch a failover left, and a
page whose `Fenec-Seq` said every write there was had been read (the
`Fenec-After` paragraph below). The router parses no FenecQL, so a
read goes near only where its client marked it: a `GET` alone was not taken
for one, since a client that sends no `Fenec-After` would have lost reading
its own writes. A router stands in a region (`--near`, or a region's
router's `--region`) and sends a marked request of a tenant with a read
replica there, and not living there, to it -- one header looked for on every
forward, the lookup only for a marked one, and one more on the node, which
reads the mark once a request in `serve_connection` -- bounded by the wait its
`Fenec-After` asks, a `/_changes` page's own `wait` (`cdc::page_wait`;
left out, a consumer's long poll near an idle tenant was cut at the
timeout, its copy rested and the page asked again home for the whole
wait, `wait=10000` answered in 17 s, which
`a_marked_change_page_waiting_past_the_replica_timeout_is_answered_near`
holds -- nothing else marked waits on a node: a held write is refused
there) and `--replica-timeout` (2 s) past them (`Pool::send_within`,
the connect, each write and each read of the head, the write timeout kept
with a pooled connection as its read timeout is, so a request sets it on no
connection a replica's read has not used): at the upstream timeout a node
that took connections and never answered held each marked read a minute,
and one that stopped reading a body past the sockets' buffers held the
write as long. It goes home where the replica does not answer --
and is let be for `RESTING` (5 s, `Config::replica_rest`), its reads going
home at once: the tenant's copy where it answered 504, behind the position
at the end of the wait it was given -- a copy following its primary never
says a position is out of reach, it may yet be pointed there, so its 504 is
always that -- for the reads sent after a position with no longer a wait
(`rest_behind`, `behind`, the longest wait it missed kept): asked again at
every read, a replica cut off from its primary held each its whole wait,
every run of a live query reading near that late for the partition's
length. A read naming no position, which the copy answers as it stands,
and one willing to wait longer stay the copy's: rested whole, two of
`tests/read_replicas.rs`'s were refused 502 with home out of the router's
reach where the copy answered them
(`a_read_replica_behind_its_wait_is_let_be_and_the_reads_after_go_home`).
Left so: a client holding a tenant's token can send that tenant's
positioned reads through a router home, with a `Fenec-After` past its last
change and a wait of a second or more once every rest -- as a failover that
lost writes does on its own, its clients' positions on the lost branch --
at the cost of the trip home, never a wrong answer;
the node, where it was not reached or took no whole request
(`upstream::Near::Unreached`); where it took one and answered nothing in
time (`Unanswered`) or broke its answer off (read whole before its head
goes on, `read_body_kept`: answered 502 before; a stream goes on as it
comes, and the tail `forward` shares took the body as a parameter: an
unmarked request's added p50 20.6 us either way, main's 23.75, sixteen
runs a side in turns), the tenant's copy is let be at once, the read going
home, and the node is asked its `/_health` -- answered before any token,
lock or routing -- on a thread of the router's own within `HEALTH_WAIT`
(500 ms, the replica timeout where shorter; `Router::rest_unanswered`,
`upstream::answers_health`), one question a node at a time and none while
it rests (`probing`): unanswered, the node is let be too -- stopped,
deadlocked: the kernel takes a small request -- so no other tenant waits
it out, where resting a copy at a time had each wait the timeout once a
`RESTING`; answered, the copy alone, since the head comes once the query
has run and a read slower than the timeout -- one every few seconds --
rested the node and sent every tenant's marked reads of the region home.
Asked on the read's own path, a read sent home from a hung node waited the
question out past its timeout, and every read timing out beside it asked
again (`tests/read_replicas.rs`: eight timing out together ask once,
failing with that rule back; `Router::replica_node_resting` and
`health_questions` are what its tests wait on). Rested once a second
tenant's copy there was, as before, the node was any client's to rest:
one holding two tenants' tokens, or two tenants' slow analytics, sent
every tenant's marked reads of the node home (a third tenant's reads held
to its replica beside two slow ones, failing with that rule back). Left:
a node that answers its health while every query on it stalls -- its disk
stalled under the queries, or its tenant registry deadlocked -- costs
each tenant with a copy there one timeout a `RESTING`. A question through
the tenant path, the node's registry and no database's lock, was weighed
and left: it would catch the registry deadlocked, a bug of the node's
own, and nothing a disk does, a disk stalled under a write lock leaving
the registry free; while a tenant's slot is held through its open,
seconds for a large file, so a node opening one copy would read as hung
and rest every tenant's. It goes home too where the replica
answers 503 or 504, or a 404 with
`Fenec-Tenant: absent` (the file gone, made again at the next look; every
`CHECK_EVERY`, 10 s, a thread of its own asks each replica's node what it
holds, a set of its names, since a standby or another region's router routes
the reads and keeps no record -- in the look, a node out of reach held each
re-pointing after a failover up for its call, 10 s);
`fenec_router_replica_reads_total{outcome}`. Every server runs a marked
request read-only on every route, the one place `serve_connection` reads the
mark (`marked`, a `Config` with `read_only` and `marked`, whose refusals say
why, `read_only_refused`): the MCP endpoint was answered before the mark was
read, a consumer's commit before `handle`, and a frozen or unleased tenant's
skip let a request through on the header alone -- it passes a freeze and a
lease only where `read_only` holds. The router replays a marked request on a
pooled connection's end only for the value a node runs read-only
(`fenec_http::marks_read`). Every router looks once a second (`FOLLOW_EVERY`)
at where each primary it keeps a read replica of is -- its directory, its
copy of the global one, or another region's router through `POST
/_shard/placements`, one call a region a look -- and re-points the ones
following another address, those needing a call on a worker a node the
look waits for none of (`look_on`, one at a time a node; on threads the look
waited for, a node that did not answer held every look after it up for its
timeout, the next tenant failed over left following the old primary as
long), each call through a pool bounded by `CALL` (10 s,
`Config::reader_timeout`) and no lock
held across one: a mutex over every change made a move's `readers_arriving`
wait out a node a region away. Each read replica is held for its change
alone (`reader_busy`, `<tenant>/<region>`), which a placement, a `DELETE`, a
move and a look take, a look passing over one held; no tenant's claim,
which a placement held across its calls, and which a look holding one would
have had a failover of that tenant refused. A refused hold makes none
(`then`, never `then_some`: made and dropped at once, it let go of the
other's key under the lock its drop takes again, and a delete beside a
look's re-point waited on itself for good, a CI run's 120 s). A tenant's
delete holds every region's changes until the tenant is gone, one under way
waited out (`hold_readers`, `wait_reader`) before its claim on the tenant,
which a failover of it would have found taken and passed it over, and for
`reader_timeout` or `CHANGE_WAIT_MOST` (15 s) at most in all, then 409,
inside the minute a global router's relay waits: passed over, a replica
whose record was out while its node was asked to take it off stayed after
the 204, a marked read answered from the deleted tenant's copy until the
next look. A move into a replica's region waits one out the same way and
holds it until the directory places the tenant there (`readers_arriving`):
passed over, the look's `follow` landing after the move installed the
tenant on the replica's node made the new primary a follower of the node
it had left, which the move then deleted. A failover or a move
it makes wakes the look (`follow_soon`) rather than call the replicas'
nodes. Following through a router URL instead was rejected: a stream to a
fenced but living old primary never ends, and a deployment of one directory
records no router's URL. The Swift, Kotlin and Dart remotes mark reads as
Go's and .NET's clients do (`reads: .nearest`, `Reads.NEAREST`,
`Reads.nearest`, and `asRead()`), the position shared by every copy, each
held to a real global router, its standby near the replica and two nodes in
its own tests. JavaScript's `live` keeps its streams home and reads its
runs near, each after the change a stream told of, and polls home
(`FenecHttp.live`, below). Not done: reads of a bounded staleness (a
decision, not a detail), JavaScript's `subscribe` near and the synced
replicas' streams, which go home (Go's and .NET's read copies mark a
subscription, held for its position as a read is, above).
`tests/read_replicas.rs` holds a marked read answered with the rows only the
replica held and while home is out of reach, a read after the client's write
going home past the wait, a failover onto the replica of its own and a move
re-pointing it, a write marked by mistake refused 403 by a leased replica's
node, both regional directions, and each fix's case, every one failing with
its fix taken out: a replica placed with home cut off answering 503 (the
router's 502 home, never the empty copy), a forked history's lost write
answered by neither the old branch (504) nor home (504 at once, naming the
new history), a hung replica's node passed over by its bound and let be (no
second request line, its rest longer than the test), a deleted file sent
home and made again, a record kept past a `DELETE` its node refused and a
node refused removal, a quota reaching the replica and, both ways, a quota
kept by one region's directory reaching a read replica kept by the other's,
a pair's standby copy promoted before its primary ever said hello answering
marked reads at home, a copy told to follow its node again answering once
that node has said hello -- the tenant there made again meanwhile -- and a
read replica re-pointed after a failover while another's node holds up the
check for ten minutes, a tenant deleted while its replica is being taken
off by a silent node answering only once the copy is gone, its claim free
meanwhile, a tenant whose replica's node is out of reach kept (502), a name not
made again while another region's directory keeps its deleted tenant's
replica, a quota a silent node does not take holding up neither the quota
request nor the look, a replica sent no quota older than one it was sent
(a look's `follow` held at the node's door by a parking proxy across a quota
request), a move onto a replica's node waiting out a look's `follow` held
there, and a failover keeping the tenant's lineage where a tenant made
again starts another (`readers::tests` a refused hold keeping the other's,
no marked read to a tenant gone, the quota queue's order, a quota whose
send a forget crossed left unnoted, the calls bounded by `reader_timeout`;
`replication::tests` the fork's id; `directory::tests` a directory
whose `readers` lacks `lineage` keeping its records, a field not found
never an empty collection, lineages following the ring), a copy taking nothing from another
tenant of its name made again on its very node, a replica of a tenant made
again taken off where its trace was let go of by hand, a trace of a name no
tenant holds refused, a look's worker pointing a replica at the primary as
it reaches it (the parking proxy holding one tenant's `follow` as the other
moves), a copy refusing another tenant of its name deleting itself (an
orphan told its lineage by hand, and a `?forget`'s), a refused copy
promoted or installed anew while its delete is held kept, a tenant and its
read replica from before lineages bound to one, a replica not caught up
bound once it is (its stream parked), a lineage backfilled late writing
over none recorded since (the node's answer held: the proxy holds answers
too), a replica following a primary promoted from a standby that never
synced -- its copy refusing nothing -- and one following its tenant's file
put in place by hand, one whose copy keeps refusing given up on (the
primary's lineage asked, its answer broken off), a worker asking a region for its
replicas' primaries at once (at most one call names one tenant alone; the proxy
records what it reads while parking), a create asking only the regions that
may keep a replica of its name -- one out of reach refusing those names
alone, a name no region traced made with every region cut off -- a router
following a region's saying `changing: null`, a placement the directory
cannot record answering and taking its copy off (a directory database made
to fail its writes), and a node that does not answer holding up no other's
re-pointing (fenec-http's `archive.rs` a restore's and a backup's
lineage); fenec-http's `api.rs`, `mcp.rs` and `cdc.rs` a marked
statement, MCP write and consumer commit refused; `web/fenec.replicas.test.js`
the JS client against real nodes and routers, eu1 killed, and its position
taken from a 504 -- over its own alone, a write answered meanwhile and an
earlier term's 504 left where they stand, another lineage's taken from a 504
and an answer, its term lower -- as Go's, .NET's and Python's tests hold
theirs, and a live query's runs read near (below). A marked read through
the replica's region's router took 59.2-68.2 us p50 against 58.8-66.9 sent
home, three runs at loads of 4.3-7.8, each node behind a loopback proxy,
and 60.9-70.3 sent after a position the replica
held, `Fenec-History` and its check included, 1.8-3.4 us over home where
the number alone was 1.4 (`make shard-bench ARGS=reads`); a client's read
of the write it was just answered 168-386 us on the replica, its wait
included, against 165-343 at home, run by run. `make requests-bench` in
five ABBA quartets against main: a row by id 0.020-0.022 ms p50 either way,
eight clients 135-146k a second against 132-145k (one branch run at a load
of 8.4, 123k, left out), a put 0.023-0.025 ms against 0.023. The router's
added p50 for a forward no client marks: `forward` is main's body with one
check of the mark after its prologue, a marked read handed to
`forward_near` (`#[cold]`, out of line) and the answer written by
`answer_out` (`#[inline(always)]`, the tail both share). In one body with
the replica's path the unmarked request ran 0.4-0.65 us slower at the
median in ABBA turns against main (23.7 against 23.2 us), the work the
same. Split, and with every per-request addition taken out besides --
the mark looked for on neither side, no `Fenec-History`, no readers'
threads -- builds still sat 0.2-0.5 us over main, and builds differing
only off the path (the tail inlined or not, the threads started or not)
0.1-0.3 us apart either way, so what was left was where the code lands:
with every function aligned to 64 bytes the node's own time came level
(+0.05 us). So the head the router writes is sized once with no `format!`
a header, and goes out with its body in one `writev`
(`http::write_parts`, which the node's answers share): written apart they
were two sends, and the router's added p50 went 23.6 -> 20.6-20.8 us on
main and on this branch alike. In 16 ABBA rounds against main with that
change and nothing else, at loads of 2.0-2.1: 20.55 us median (20.5-20.8
quartiles) against 20.7 (20.6-20.9), the branch's per-round difference
-0.15 us and positive in 5 of 16; against main as it is, 20.6 against
23.6, lower in all 20 rounds. A request straight to the node stays
0.15-0.25 us over main's 21.3, which the aligned builds put on layout.
What the unmarked path does more: `Fenec-Read` looked for once on the
router and once on the node, `Fenec-History` written where an answer
names a history, and a `Response` 16 bytes larger; the pool asks
`replayable` only where a send failed (`Replay`), where every request
scanned its headers for it before.
The JS client grew 4.1 KB raw, 1.4 KB gzip, 1.2 brotli (59 KB gzip, 50
brotli, as README and benchmarks-size.html quote), an app's minified bundle of
`connect` and a query's rows 1 114 bytes, 406 brotli; the browser module not
by a byte.
