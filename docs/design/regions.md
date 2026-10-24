# Regions and the global entry

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Regions and the global entry*: regions and zones, a region's own directory, and the global entry. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Regions and zones

**A node is in a region and a zone, and a tenant in its node's region**
(`directory::Node`, `Router::replica_node`, `relocate`;
`site/content/docs/sharding.html#regions`). A global service runs nodes in
several regions, each in two datacenters or more, and a tenant has one home:
`PUT /_shard/nodes/<n> {region, zone}` records them -- fields of `nodes`,
added by an `alter` to a directory from before them, whose nodes read the
unnamed region, one region like any other, so a directory naming none places
and fails over as it did; a router from before them reads `name, addr,
token`, ignores the rest and places without regard to regions -- and a
tenant's region is its node's, kept nowhere else, so a move or a failover
changes it with the node and no second record drifts. `PUT
/_shard/tenants/<t> {region}` places on the fewest bytes there, 409 where
no node is and nothing created (`--default-region` where a create names
neither region nor node, otherwise any node as before); a node's region is
fixed while it holds a tenant, a replica or a place in a pair (`holds`).
A replica (`--replicas`) goes to a node of its primary's region only, one of
another zone ranked first -- before a node holding a copy and before the
fewest replicas, since a datacenter lost takes a replica in its primary's
zone with it -- a zone not named between; one placed in its primary's zone
where nothing else was free is logged, answered `"same_zone": true` and
counted (`fenec_router_same_zone_copies{region}`), and a pair's standby must
share its node's region. So a failover, by hand or on a lease, promotes
within the region by construction: nothing placed a copy anywhere else. A
move leaves the region only where the request names the region it goes to
(`{to, region}`, or `{region}` for the fewest bytes there), and the replica
is placed again where it no longer fits (`replica_node(to, [r]) != r`):
kept, a failover would carry the tenant back. The global entry is one router
a region: a standby router already forwarded from its replicated maps and
refused `/_shard/` changes with 409 (not 503: asked again it refuses again),
and added 23.7 us p50 to a request as the primary did (`make shard-bench
ARGS=standby`, the two asked in turns, five runs: 23.0-23.8 and 23.1-23.9);
a move now deletes its source once every standby keeping up has been sent
the flip (`standbys_sent`, `SENT_WAIT` at most), where one in another region
forwarded the tenant's next request to a node that held it no more, 404.
With one directory leases come from the primary router alone: a region cut
off from it stops writing as its leases lapse and cannot be failed over
until it is reached again -- a region keeping its own directory (below) is
leased by its own router. Against main in ABBA turns the router's added p50 stayed
23.65 us (23.3-23.9, 16 runs) against 23.50 (23.0-24.0), and the failovers
as they were, medians of eight runs a side, ABBA and BAAB: a pair's 20
tenants 79 ms against 75.5 (58-111 and 63-81), one node's 10 over their
replicas 49 against 50.5, the repair after it 172 against 173.5, a lease's
tenants writing again 1.29-1.32 s after the cut against 1.26-1.30; a round
on a machine whose load rose to 12 was left out. The gauges by node carry
`region` where the node names one.

## Region directories

**A region may keep its own directory, and leases and fails over inside
itself** (`fenec-shard/src/region.rs`, `--region`, `--global`, `PUT
/_shard/regions/<r>`; `site/content/docs/sharding.html#region-directories`).
With one directory every lease came from the primary router, so every
region's writes hung on the primary's region. A region's router (`--region us --global
<url>`) keeps the region's nodes and tenants in a directory of its own -- the
same collections, its region in `scope` (`settle_scope`: a router opened
over another's, or over the global one, refuses it) -- and alone leases
them: a lease, a failover, a repair, a move between its nodes, a quota and a
node added write that directory alone and go on through a partition. The
global directory keeps every tenant's name, a home for each of such a
region's (`@<region>` as its node, a name no node can have, kept apart in
`Directory::homes` so nothing reading placements meets it) and the regions'
routers (`regions`, the one keeping the directory first). A create takes the
name there and asks the region's router to make it -- given back where it
refuses or is not reached, kept where it says the tenant exists, which takes
in a create whose answer was lost -- a delete asks first and gives the name
back after, and a move across regions (`move_across`) freezes and exports
in one directory, installs and records in the other, releases in the first
-- whose directory then places the tenant nowhere, so no lease names it,
and a source restarted takes no write before a grant from there -- and only
then flips the home, one statement; each step undone where the next fails,
so a region not reached refuses it 503 with `Retry-After` and nothing
changed. A router relays a change it does not keep once (`regional_admin`),
a region's to the global router, the global router's to the region's under
`Fenec-Global`, which is carried out and never relayed on; a deployment
whose regions keep none returns before reading a body (`route_global`), as
before. Forwarding relays to the region's routers rather than keep a copy of
every region's directory in every router -- rejected: a follower a region in
each router, re-pointed as authorities move, and a copy that lags a failover
by the partition's length, sending writes to the node failed over away from
-- so the routers holding the placement answer, and after a partition every
router reaches the new one with its first request, for a hop inside the
tenant's region a client of that region never takes; a relayed request is
never relayed again (503). The relaying router names its client -- the
one the global entry named, where it believed one (`--entry-token`) -- in
`Fenec-Router` under an HMAC of the shared `--token` (`relay_mark`, believed
through `audit::request`, `Fenec-Verified` passed back), and moves to the
next router of a region only where the one asked cannot have run the
request (`Pool::send_tried`'s `Tried::Unrun`), passing over one answering
that it follows another. A lease names its holder (`fenec_http::lease::Holder`:
the directory's region, its term -- its history's forks, one more at each
promotion -- and its history): a node takes a grant from the directory it
holds at a term no lower and none from another while that lease runs,
refuses a superseded router's (409 `held_by`, which a router from before
reads as a node taking no lease and so fails over never) and its promotions
and follows (`Fenec-Lease-Holder`, `Lease::admits`). So a router promoted
over one still running fences it at every node it reaches; a node only the
old one reaches is beyond that, and a router is promoted once the one before
is gone, as before. A region's router fails over no node that took a grant
without echoing its term (`Answer::Unfenced`, a node from before terms); the
global directory leases one as before, and its nodes, once they hold a term,
refuse a router from before terms put back in front of them until they
restart, which forgets it. A router from before regional directories reads
a home as a node it does not know: 404 for the tenant, every change of it
refused, none of the region's nodes leased. A query across tenants asks the
router's own directory's and names each of another's as failed. A tenant
the global directory places is forwarded by the region's router's copy of
it (`Target::Node`), which fails nothing over: what the region's router
had out to a node the global router failed over is ended as the copy
shows the failover, as a standby router's is
([router.md](router.md#failover-cuts)).
`tests/region_leases.rs` cuts a region off from the global router through
proxies and holds it writing past every lease granted before the cut, its
node fenced before the region's router promotes the replica, the move and
the create across refused and nothing changed, then every router -- the
global one, its standby, the region's -- writing on the new placement once
healed, and a promoted region's router fencing the one before at the node,
grant and promotion alike. Cut off
from the global router, a region's node cut off from its router stopped
writing 0.38-0.63 s after the cut (0.67-0.98 when it ran its lease from its
last renewal) and its 10 tenants wrote again on their replicas 1.41-1.70 s
after at a lease of 1 s, ten runs, as the global router's own failover
beside it (1.27-1.36), and 3.9-4.1 and 5.5-6.7 s at 5 s (`make shard-bench
ARGS=regions`); a relayed
request paid 32-42 us p50 over one to the region's router, a router's 24 and
the loopback proxy standing for the link. Against main in ABBA turns the
router's added p50 stayed 23.6 us (15.5-24.0, twelve runs at a load of 2-3)
against 23.75 (16.3-24.3), and the failovers as they were, medians of four
a side: a pair's 20 tenants 64 ms against 67, one node's 10 over their
replicas 43.5 against 45.5, the repair after it 157.5 against 196, a lease's
tenants writing again 1.36 s after the cut against 1.37. Nothing below
`fenec-http`'s lease and admin changed.

## The global entry

**One URL reaches every region: a Worker learns each tenant's home from
the routers** (`integrations/cloudflare/entry`, `region::HOME_HEADER`). A
client had to know its tenant's region, a router of another relaying the
request once, a hop between regions every request. The entry is a plain
Worker -- Workers for Platforms' dispatch, about 17 ms, stays out of the
path -- sending `/t/<tenant>/...` to the routers of the tenant's home, and
asking nobody where that is: every request carries `Fenec-Home: 1`, and the
router forwarding it to the tenant's node -- from its directory, or from
its copy of the global one (`Target::Node` carries the node's region now)
-- answers `Fenec-Home: <region>`, the node's; a relaying router names
nothing and passes the home router's through, and the entry takes it off
the answer (a router names it on a 401 too, to anyone with a tenant's
name). A tenant not heard goes to the region of the client's continent
(`REGIONS`' `continents`, `request.cf.continent`) or `DEFAULT_REGION`, is
relayed home once, and its next request goes home; where that region's
routers fail, it goes on to the others (`guesses`), since any router
relays a tenant home and only the home's places it on a node -- a known
home is never left (a 503). A region whose routers failed is asked last:
by a read or a keyed write for 10 s (`DOWN_MS`), by any other write until
one of its routers answers -- a write that may have reached a dead guess is
not resent, and dropped from the end after 10 s it was a 502 for an unrun
write every 10 s an isolate while the default region stayed down. A
guessed region's router answering 5xx naming no home (cut off from the
home's region) sends a read on to the next region; the placing router's own
502 for a node it could not reach names the home (`region::homed`), and is
the answer. A move across leaves the old region's router relaying
the tenant and the answer naming the new home, a hop and never an error.
A read marked for a replica (`Fenec-Read: replica`, the read replicas
below; `marked`, `near`) goes to the region of the client's continent
first whatever the home, then to the home where that region fails or
answers a 5xx naming none: sent to the known home alone, every marked read
went home once one unmarked request had taught the colo the home, and the
replica near the client was never asked. A continent no region names goes
home alone -- through `DEFAULT_REGION`, an Asian client's read of a us
tenant went by eu, where it had no replica. It may go to two -- the mark
has every router and node run a request to a tenant's path read-only, so
`repeatable` takes it there as the routers' `replayable` does, and not on
`/_shard/`, where a marked change of the global router runs as any and a
thrown fetch had it sent to the standby again -- while an unmarked request
never leaves a known home; a region found down is passed over for
`DOWN_MS`, and where it keeps no replica its router relays home, a hop.
`entry.test.js` holds a marked read of a tenant whose home was learned to
eu's router and its replica, us asked none, under `wrangler dev` with the
client's continent pinned (`serve`'s `cf`, `CLOUDFLARE_CF_FETCH_PATH`:
miniflare otherwise gives this machine's), and in Node the home alone from
a continent no region names and a marked `/_shard/` change not sent
again.
Rejected: the global router asked at each miss (its `--token`, which moves
and deletes every tenant, in the Worker, a round trip to its region, and
every unknown name a client tries a question for that one router), and
Workers KV (read eventually consistently, billed a read, and something must
write it). An isolate keeps 50 000 homes (`HOMES`), forgotten first heard
first -- by use it was a delete and a set every request -- and shares each
it learns through the colo's Cache API (`waitUntil`, read only on an
isolate's miss; nothing on `workers.dev`). Asked for, so every other
client is told no home: not asked, the router's added p50 was
23.75 us against origin/main's 23.9 with its mark on every answer
(23.4-24.1 and 23.7-24.0, six runs a side in ABBA turns, `make shard-bench
ARGS=latency`; 24.15 against 24.1 before the mark), and asked, a router
answered in 82-83 us p50 against 81. A path is judged as the router and the node read
it, decoded as fenec-http decodes one (`percentDecode`, `segments`):
WHATWG's URL leaves `%5F` and `%2F` encoded, and `/t/acme/%5Freplication`
with a tenant's replication token was its whole replication stream through
the public entry. A request goes on with what the client sent but the
hop-by-hop headers, the routers' marks (`Fenec-Router`, `Fenec-Global`,
`Fenec-Verified`, the last dropped from answers too) and every address a
client claims (`X-Forwarded-For`, `X-Real-IP`, `Forwarded`,
`True-Client-IP`, `cf-*`); `X-Request-Id` is the client's or Cloudflare's
ray id. The client is named in `Fenec-Client: <address> <seconds>
<mark>`, the address `CF-Connecting-IP`, the mark 16 bytes of HMAC-SHA256
of `fenec-client <address> <seconds>` under `ENTRY_TOKEN`, base64url
(`clientMark`): bound to the address and the time, a mark seen names that
address for a minute, where one of the token alone, as `audit::router_mark`
is, would name any address for good -- and not `CF-Connecting-IP` believed
from Cloudflare's ranges, since a Worker sets it and `X-Real-IP` as it
likes towards an origin outside Cloudflare (Cloudflare's HTTP headers page),
so any account's Worker could claim any address. A router started with
the same token (`fenec-shard --entry-token`, `FENEC_SHARD_ENTRY_TOKEN`, set
and empty being unset) reads it (`audit::entry`, after `audit::request`
and for a request no router relayed): the mark first, in constant time
against its own HMAC of `fenec-client <address> <seconds>` (`EntryKey`, the
key's padded blocks hashed once, a `HmacKey`: 0.73-0.78 us a mark checked
where the HMAC alone took 1.28-1.36 with them hashed each time), then the seconds
within 60 of its clock either way (`CLIENT_WITHIN_SECS`), so a stale header
is one the entry made. The address then stands in for the connection's
peer for that request alone (`audit::NAMED`, cleared by each request's
`audit::request`, a kept-alive connection carrying many clients) -- the
refusal's wait and count (`audit::http`), a verified token's clearing
(`audit::verified`), the audit and slow lines' `peer`, the entry's address
their `via` (`audit::line`), the trace's `client.address` (on a node too,
for the client a router named; the connection's own `network.peer.address`
always), and the `Fenec-Router` the router names to a node, a read
replica's (`forward_near`) or a relay, which names it to the node in turn
-- the router still waiting a refusal out and still relaying a tenant of
another region, where for a relayed request it does neither. A count is
kept by the address made canonical (`counted_as`, and the name the entry
gives): a router on `[::]` saw an IPv4 client as `::ffff:a.b.c.d`, the
entry named it `a.b.c.d`, and one client had two counts. Replayed, a header
names that one address for its minute: what the minute costs against a
mark of the token alone, which named any for good. A header that does not
check, or a router without the flag, leaves the connection's address, and
every router drops the header from what it forwards, flag or not
(`forward`'s list, `forwarded_headers`), so a node never reads a client's
claim: relays, held requests, streams and a read replica's reads go out
through those lists, and `/_shard/`'s relays and a fan-out write headers of
their own. What each came to is counted
(`fenec_router_entry_clients_total{outcome}`: `named`, `bad_mark`,
`stale`, and `absent` for a tenant's request carrying none -- every one,
behind an entry whose `ENTRY_TOKEN` is unset, which names nobody and
otherwise shows nothing -- in the thread's metrics shard as a request is),
one not believed said in the log a minute apart at most a kind -- once a
process, a router running for weeks said it once -- and none is an audit
event: each audit line costs its sender a refusal's wait, a token or a
write, and such a header costs nothing, so a line each would grow the log
at the router's request rate, and a mistyped token write one for every
request through the entry. On a router only the entry reaches, all but
`named` is worth an alert (monitoring.html). The entry's token is refused
where the router holds it for anything else (`main.rs`, `set_node`): empty,
equal to `--token` (moves and deletes every tenant), to
`--replication-token` (the directory's stream carries every node's admin
token) or to a node's admin token in its directories, at the start and as
a node is added; the nodes' data and replication tokens it never sees.
One token at a time: rotated by starting the routers with the new one and
then the Worker, every request served in between and its client counted by
Cloudflare's address (`bad_mark`) -- a second accepted token was not worth
its flag against minutes of the counting there was before. Nodes read none
of it: the entry reaches routers alone (`REGIONS`, `ADMIN`). A forwarded
request compares each header once more, to drop it: the router's added p50
stayed 20.4 us against main's 20.4 (20.1-21.0 and 20.0-22.9, ten runs a
side in ABBA turns at loads of 1.7-2.3, `make shard-bench ARGS=latency`).
With the flag, in the bench's `entry` part (each request through a router
with it and one without, in ABBA turns, eight runs at loads of 2.2-2.9):
one without the header -- a look among its headers, an `absent` count --
took 42.0 us p50 against 42.05 (the runs' differences -0.2 to +0.1), and
one the entry named its client in 41.7 against 42.1 (-0.8 to +0.1, one run
+2.8): the mark's 0.75 us is not seen in a round trip. The router without
the flag was timed first in every run before, an order no turns of whole
runs undo; it takes turns inside each run now.
`tests/entry.rs` holds two named clients' counts apart and a good token
clearing its own alone, the router's own refusals and the node's; a bad
mark, a moved address, a stale time either way and a router without the
flag to the connection's address, and a request carrying no name counted
`absent`; one kept-alive connection named, then not, then named again; the
header at no node, through a forward, a held request and a stream; the
audit lines; a relay carrying the client to the home router's and the
node's lines and spans; a read a read replica answers naming it to the
replica's node; and the entry's token refused as each other token, at the
start and as a node is added, an empty environment's taken as unset --
each failing with its part taken out -- and `entry.test.js` a token
refused through the Worker in front of real routers waiting by
`CF-Connecting-IP`'s address, never cleared by another client's good
token. The routers' CORS headers pass unchanged; with `CORS`
(their `--http-cors`) the entry answers a preflight itself and puts the
same headers on its own answers alone, which the test holds to a router's
preflight, and an isolate seeing a router's `Access-Control-Allow-Origin`
with no `CORS` logs it once: its own 503s would read as the network
failing. Only `/t/<tenant>/` is served: `/_shard/` where `ADMIN` names the
global router, everything else and a tenant's `/_replication` a 404. A
region's routers are tried from the one that answered last; every answer a
router writes carries its mark (`Fenec-Shard: 1`, `region::SHARD_HEADER`,
written by `Router::cors` and `forward`, a relayed answer's the relaying
router's alone), and one without it is not the router's (`judged`) -- not
`X-Request-Id`, which a load balancer may write on its own 502, so that a
dead router behind one looked like the router's answer and its standby was
never tried. Cloudflare's 521/523/525/526/530 never reached one, so
any request goes on; one that may have -- a thrown fetch (workerd says
`Network connection lost.` of a connection refused and of one reset after
the request alike, probed), Cloudflare's 520/522/524 (its 522 page counts a
request unacknowledged for 90 s on a connection made), a tunnel's connector
answering 502 for a router it cannot reach (Cloudflare's tunnel
troubleshooting: 530 means no connector, the 502 a connector up and its
origin not) -- goes on only for a `GET`, `HEAD`, `OPTIONS` or a keyed
request, the router's own rule towards a node, any other a 502 saying it
may have run; a router's own 502 is its answer. A body up to a mebibyte is
read whole for a retry, a longer one streamed to one router (`request.body`
handed on keeps its length, probed; a router refuses a chunked body, 411),
and one sent with no length is read up to a mebibyte and refused past it
(411): read whole, it was as large as a client sent, in isolates sharing
128 MB. An answer goes back as its body arrives; `request.signal` (the
`enable_request_signal` flag) cancels the fetch with the client. Waiting
on a fetch is not CPU time and an HTTP-triggered Worker has no duration
limit while its client is connected (Cloudflare's Workers limits), so a
held write or a stream is not cut; a proxied hostname's answer must start
within the proxy read timeout (100 s below Enterprise), past
`Fenec-Wait`'s 30. Under `wrangler dev` a client gone before its answer's
head never reaches the Worker -- workerd notices a client only as it writes
to it, past wrangler's own proxy Worker too -- so that case is held in Node,
the handler over Node's `fetch`; a stream is let go of at a stand-in router
2 s after its client, at a node through a router in 4 to 8. And a router
waiting on a node's held answer watches its client and passes its end on
to the node (`upstream::answered_or_gone`, below): blocked in the head's
`read_line`, it had a claim whose client went through it take the next
job, leased to nobody, where asked of the node itself the claim passed it
on -- so cancellation reaches the node from the entry through every
router on the way. `entry.test.js`
runs the Worker under `wrangler dev` in front of real routers and nodes
(`entry/cluster.mjs`: a global router, us with a standby, eu, each on port
0) and of stand-ins (a tunnel's connector, Cloudflare's 522 and 530), each
fix held by a test that fails without it. Under `wrangler dev`, a read by
id took 81 us p50 of us's router, 1.18-1.22 ms through the entry with its
home heard -- 0.66-0.68 past wrangler's proxy -- and 1.23-1.24 with the
client named besides (an HMAC a request), 1.29-1.31 never heard (eu
relaying, 114-115 us asked directly) and 1.60-1.61 heard through
miniflare's cache, two runs at loads of 3.4 to 5 (`make entry-bench`):
loopback and workerd, not a colo and a region away.
