# The router

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *The router*: the router's CORS, its pooled connections, the cuts a failover makes -- by the router failing the node over and by the routers following its directory -- and queries across tenants. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## CORS

**The router tells a page one CORS rule, its own** (`fenec-shard
--http-cors`, `fenec_http::cors`, the one list a node and the router
send). Without it the router's own answers -- an unknown tenant's 404, a
node's 502, a standby's 503, the connection ceiling's 503, `/_shard/`'s
401 -- carried no CORS header while the nodes' answers through it carried
the nodes', so a page read each as the network failing and the sync and
`FenecHttp.live` backed off from an answer; and a preflight was
forwarded, so where the tenant was unknown or its node unreachable the
request after it was never sent (a node's own connection ceiling went
without them too, and has them now). With it the router answers a preflight
itself (`cors::allows`) before the directory is read, puts the headers
on every answer it writes but the studio's files (a node's go without
too), and on a forwarded answer, a stream's head included, drops the
node's (`cors::is_cors_header`: `Access-Control-*`, a `Vary: Origin`
alone) for its own: kept both, a node allowing `*` behind a router
allowing one origin answered two `Access-Control-Allow-Origin`s, which
Chrome refuses whole (`studio/test/router-cors.test.mjs` holds it in the
browser), and the answer has to say what the router's preflight said.
Without the flag nothing changes: a node's headers pass through and a
preflight goes to the node. The preflight is answered without
`audit::http`, which clears an address's refusal count for any answer to
a request bearing `Authorization`. The headers are the semantics a node's
are: the configured origin on every answer whatever the request's
`Origin`, the browser comparing. Off, a request through the router costs
what it did: `make shard-bench`'s added p50 22.6 us against main's 23.3,
six runs each in ABBA turns (20.7-25.2 and 20.5-25.7); on, 24.3
(21.9-27.6), six header lines more in every answer.

## Pooled connections

**The router looks at a pooled connection before it writes, and sends a
request again only where the node cannot have run it or may run it
twice** (`fenec-shard/src/upstream.rs`). A node closes a keep-alive
connection silent past `--idle-timeout`, and on macOS the router's end is
reset about a minute after the node's FIN (`setsockopt` fine 59 s after
the close, refused 62 s after; `net.inet.tcp.fin_timeout` is 60 s), after
which XNU's `setsockopt` refuses it with EINVAL: `set_read_timeout`
returned that before a byte was written, the retry knew EOF, a broken
pipe and a reset, and a request after a quiet minute was answered 502
(the dispatch example ran its nodes with `--idle-timeout 0` for it). A
pooled connection is looked at first (`open`: a `recv` of `MSG_PEEK |
MSG_DONTWAIT`, declared by hand) -- 0 is the node's FIN, an error its
reset, a byte an answer nobody asked for, each let go of and the next
looked at -- and a failure is judged by how far the exchange got
(`Stage`), not by the errno, which each system names its own way (one
reset connection: EINVAL then EPIPE on macOS, EPIPE or ECONNRESET on
Linux). Failing before the request's last byte was handed to the system
-- the read timeout's `setsockopt`, a write, not a write's timeout -- the
node never read a whole request, so it goes again on a fresh connection
whatever it is. Written whole, with the connection ending before the
first byte of an answer, the node may have run it: a node that ran a
`put` and went down looks just like a close on its way, so only a `GET`,
`HEAD`, `OPTIONS` or a request carrying an `Idempotency-Key` goes again
(Go `net/http`'s rule), anything else is the client's 502. The retry it
replaced sent every request again on that EOF, and made such a `put`
twice. What the look cannot see is a close in the microseconds between
it and the node's read; a bound on how long the pool keeps a connection
would need each node's `--idle-timeout`, which the router is not told,
and a guessed one below the default narrows nothing for a node set
lower. `tests/upstream.rs` puts a relay between the router and a node
that passes the node's close on as a reset (`SO_LINGER` 0, macOS's state
a minute on) or as the FIN, or swallows an answer, the directory written
before the router starts so no request but the test's meets the node's
200 ms idle timeout. The look is one system call, 0.31 us on an M1
(`set_nonblocking`, `peek` and `set_nonblocking` again 0.74), and the
two `setsockopt`s every exchange made, 0.67 us, are gone from a reused
connection: the write timeout is set at the connect, the read timeout
only when it changes (a `Fenec-Wait`), kept with the pooled connection.
`make shard-bench`'s added p50 stayed 23.3 us against main's 23.3, six
runs each in ABBA turns (23.1-24.8 and 20.0-24.4). A connection's name is
looked up within the bound its connect has (`dial` through
`fenec_http::dial::resolve`, which every dial of the router's goes
through: a forwarded request, a relay to a region's router, a query
across tenants, a read replica's read and upkeep, a health probe, a lease
grant): `getaddrinfo` takes no timeout, and a node recorded by a name its
resolver was slow to refuse -- a container gone, whose name Docker's
resolver forwards upstream -- held every dial to it for the resolver's 5
s, a lease grant's among them. The lookup runs on a thread of its own
that the caller walks away from at its bound; a caller asking a name
whose lookup began under 500 ms before, or that has eight out, waits on
the newest (`JOIN_WITHIN`, `MAX_IN_FLIGHT`), since every request to a dead
node's tenants would otherwise leave a thread waiting out the resolver.
Nothing is cached -- Docker hands a stopped container's address to the
next one it starts, and kept, a node's old address would send its grant
there -- and an address written out is read where it stands, with no
thread: `make shard-bench ARGS=latency`'s added p50 stayed 20.7 us against
main's 20.5, eight runs a side in ABBA turns at loads of 2.2-2.8
(20.2-21.3 and 17.0-20.8), and the whole bench's 20.4 against 20.7.
`fenec-shard`'s code grew 59.9 KB with the leasing beside it, the held
tenants and the refusal's lineage and history -- 9.3 KB of it the promotion
that sends them and records its answer in one block -- and
`fenec-server`'s 14.5, 2.0 of it the copy's word of its primary's hello
(aarch64-apple-darwin); the follower's dial answers
through a slot and a condvar, an `mpsc` channel 13 KB more of each. A
request the node may
hold (`Fenec-Wait`) watches its client while its head is waited for
(`answered_or_gone`: one `poll` of the node's socket and the client's,
declared by hand as `recv` is, `nfds_t` each system's own, until either
moves), and passes on what the client did as the node would read it from
a client of its own. The client's end -- closed, or half-closed and
reading on, which read alike -- is the node's connection shut for
writing, not pooled after: the node's held claim sees it before its next
run and hands its row on, and the node's answer goes on to the client as
any. A reset, which can read nothing more, is the client gone
(`Stage::Abandoned`, `upstream::client_gone`): the node's connection let
go of, nothing written, sent again or counted against the node, the
request counted in `fenec_router_requests_total`'s `gone` class (counted
nowhere before) and its trace ended 499. `gone` counts resets alone: an
end cannot be told from a half-close, so a client gone with its end --
most timeouts and aborts -- is passed on as one, its claim leaving at the
node's next run or its wait's end and its answer counted by its status,
as asked of the node itself, the job handed on all the same. Taken for
gone as a reset is, an end let go of the node's connection, and a client
that half-closed after its claim -- which the node answers as any, its
first run looking at no client -- was answered nothing while its claim
took a job on a busy queue. Bytes from the client
are a next request sent ahead, after which the node alone is waited for,
as the node lets a client be whose next request it holds; so is a client
whose next request the router's reader holds already. Blocked in the
head's `read_line`, a claim whose client went through the router took the
next job, leased to nobody until the lease ran out. A router relaying the
request watches its client the same way (`relay_send`), so the end goes on
hop by hop. `tests/upstream.rs` holds two claims through one router,
through a global router relaying to a region's, and through a region's
router finding the node in its copy of the global directory
(`Target::Node`), the first one's client closed or reset and the job put
once the relay saw the router let go: the job is the second's, failing
with either hop's watch taken out; and a half-closed claim on a busy queue
answered its job through the router as straight to the node, failing with
the end taken for gone. The `poll` is a `Fenec-Wait` request's alone: one
with no such header passes no socket to watch and makes the system calls
it made: with the body's buffer and the read replicas' health below,
`make shard-bench ARGS=latency`'s added p50 stayed 20.5 us against main's
20.5, sixteen runs a side in ABBA turns at loads of 2.7-2.9 (20.0-21.2 and
19.8-23.0). A claim
answered at once came through the router 61.3 us p50 against 60.8 on
main, 0.3 us more past the node's answer at the median (27.4 against
27.1), and a held claim woken by a job put straight to the node 332
against 324 (70.1 against 61.8 past the node), eight runs a side in ABBA
turns of 20 000 and 2 000 claims (a scratch bench), on macOS; the review
measured 0.7 us there and 1.2 on Linux on a claim answered at once. A
watcher thread, prototyped in the review, was worse; the head read in
slices with the client looked at between was left on its reasoning, every
one of a thousand held claims woken each slice for nothing and a client's
end seen a slice late.
A node's answer is read into a buffer of the length it says, allocated
so its failure is an error (`zeroed`: `alloc_zeroed`, the allocation
`vec![0u8; len]` makes, the system's zero pages taken up as the bytes
reach them, null a 502), by `read_body`, `read_body_kept` and
`read_body_by` alike: through `vec!`, a node -- buggy, or not the
router's -- declaring a petabyte and sending three bytes aborted the
router on the allocation's failure, every tenant's clients with it, and is
a 502 now, the router answering on (`tests/upstream.rs`'s stand-in node,
and each reader in the unit tests, both aborting without it). A buffer
grown as the bytes came, from a mebibyte fourfold, copied what came at
each growth -- a body of 4 MiB and a byte 596 us against 440 in the
review's measure, every forwarded answer past a mebibyte slower -- and a
node sending without end
under such a length met a growth whose allocation aborted all the same;
one of 64 MiB first was as fast, past which the growth was the same; a
bound on the length would refuse a tenant's image on its way through a
move. So the length is believed as far as the system lends it: macOS
refuses a petabyte, the refusal the 502, and Linux, overcommitting, lends
it -- two pages of it taken up in the review's Docker run -- the 502 then
the answer broken off at its three bytes; either way a lying length costs
the pages its bytes reach, and a node that keeps sending grows the
router's memory as any large answer does, as it did before. It costs
what `vec!` did: through `Pool::call` off loopback, eight runs a side in
ABBA turns, 200 B 20.3 us either way, 64 KiB 28.1 against 28.2, 1.5 MiB
126.4 against 126.1, 4 000 000 B 332 against 327 (325-338 and 324-336),
4 MiB and a byte 350 against 350, 16 MiB and a byte 2.96 ms either way, 64
MiB 12.7 against 12.5 (12.65-12.75 and 10.9-12.7). A query across tenants
refuses a declared length past `--fanout-max-bytes` before it adds it to
the query's sum, which one near `usize::MAX` overflowed.
An answer with no length is refused by `read_body` as by the other two,
a 502 at its head and its connection let go of: read to its close, it
grew the router's memory for as long as its peer sent, the request
waiting all the while. What reads a body there is the router's own call
-- `Pool::call` to a node's `/_admin/`, `call_routers_by` to a router's
`/_shard/` -- and every answer of those peers carries its length
(`Response::write`, a tenant's image among them), none of them a stream;
a stream a client asked for goes out as it comes (`answer_out`, which
reads a body only where it has a length: `router.rs`'s
`a_subscription_streams_through_the_router`, `entry.rs`'s `_changes`).
Refused rather than capped as a request's body is: a cap is a bound
picked for an answer that never comes, and reading up to it held the
thread as long as the peer trickled it in. `tests/upstream.rs` registers
a node standing in with stats of no length and 64 MiB, which the router
read whole and recorded the node by (201), and the unit tests send each
reader such a body without end; both fail with the read to the close put
back. A declared length is still believed, as above. `make shard-bench
ARGS=latency`'s added p50 stayed 20.6 us against main's 20.5, 24 runs a
side in ABBA turns over three rounds at loads of 1.3-2.7 (20.1-23.8 and
18.6-21.5; the third round 20.5 against 20.55): `forward`, `answer_out`
inlined in it, is main's to the instruction, and `read_body`'s path for a
body with a length the same instructions in other registers.

## Failover cuts

**A failover ends what the router has out to the node for the tenants it
moved** (`upstream::Pool::cut`, `Router::after_cut`, `cut_moved`). A node
that dies with no reset -- a machine lost, a network cut -- answers
nothing and closes nothing, and what the router had out to it waited for
`--upstream-timeout` though its tenants answered on their new primaries:
in V5's drill, at a lease of 2 s and the kit's timeout of twice that, the
router failed the killed node over 1.9-2.6 s after the kill and its
writers wrote again at 3.90-4.02 s, their requests let go of at the
timeout. The pool knows every connection it made, by node, among the
idle ones under the lock a request takes anyway (`Kept::flights`, a
connection put among them as it is made, a lock a connect), and each
connection's `Flight` is one word: the state in its low bits beside a
mark of the tenant whose request is out on it, so a cut compares and
changes both at once -- read apart, a request of another tenant out on
the connection a moment later could be the one cut. The mark (`mark_of`)
is a multiply chain keyed by a number the process draws from
`RandomState`, MurmurHash3's finisher and its high 60 bits: kept low, the
first cut's `(h | 1) << 3` dropped the top three, and the low 61 bits of a
product depend on the low 61 of what is multiplied alone, so two names of
a multiple of 8 bytes differing only in their last byte's top bits --
`tenant-1` and `tenant-q` -- always shared one (603 592 groups among
`shop` and four of `[a-z0-9_-]`, none now), and a failover of one cut the
other's write on a node that still served it, a 502 for a write whose 200
was on its way; keyed, nobody naming tenants can make two share one. A
request's word is `WAITING` from the moment the pool hands it the
connection (`take`, `register`, written under the pool's lock) and it
claims its answer with a compare-and-swap to `ANSWERING` once the head has
begun;
`into_stream` marks a stream `STREAMING` and asks its bucket's cut count
again (below), sequentially consistent with the cut's look, so a stream
whose tenant was failed over between its head and then -- its answer
begun, which the cut passes over -- ends there for its client to open
again, where it was copied for good; asked at the head instead, every
forward read its answer's headers for a length once more.
`promote_on`, once the directory places the tenant on its copy, has the
pool shut down (`shutdown`, both ways) each connection `WAITING` or
`STREAMING` with the tenant's mark, through `CUTTING` to `CUT`: a thread
waiting on one wakes to its end, sees it was cut (`was_cut`,
`Ended::Cut`) and is answered by what the node may have run -- not run, a
read, a request marked as one or a write under an `Idempotency-Key` sent
to the new primary, anything else 502 saying it may have run, the rule
`replayable` already was; a held request by the same rule with what is
left of its wait (the router cannot tell one held from one the node had
just run as it died, and a claim run twice takes two jobs) -- and a
stream ends for its client to open it again. An answer begun is never
cut: the swap to `ANSWERING` and the cut's from `WAITING` are one word's,
so the first wins whole, and macOS's `shutdown` drops what the socket had
read but not handed over (a cut stream's last events, which its client's
next seed brings). A connection is marked `CLOSED` before its descriptor
is (`Conn`'s `Drop`), waiting out a cut in `CUTTING`, so a cut never
shuts a descriptor another connection took since. Only a failover cuts,
by hand or on a lease, a pair's or a replica's: a node merely suspected
may be slow, its lease running, and a write cut there would be a 502 for
one that may well land. A forward that read the directory before the flip
and goes out after the cut looked would wait as before, so the pool
counts its cuts in 64 buckets by the mark's top bits (`Pool::moved`), and
a forward reads its bucket's before the directory, acquired -- a count a
cut moved comes with the directory as its failover left it -- and again
before its first byte -- moved, it writes nothing, puts its connection
back
(`Ended::Moved`, `unsent_moved`) and reads the directory again, which
mostly places it where it was. One count for every tenant, any failover
had each forward straddling it close a healthy connection and count as
cut. The second read needs no fence: the cut moves the count and then
takes the pool's lock to list the node's connections, under which the
forward's `WAITING` was written, so the cut's lock came after and finds
`WAITING`, or before and the count is read. Written and read sequentially
consistent instead, the two were an `stlr` and an `ldar` on aarch64, the
load waiting for the store to drain, and an `xchg` on x86, every
request. Its dial tries a second at a time (`connect_watching`,
`CONNECT_LOOK`) and gives up once the count moved, and a connect refused
as it moved reads the directory again rather than answer 502: past the
node's pooled connections, each held by a request waiting on it, a
machine gone answers no connect, and those waited out the timeout too.
Under leases a copy takes its tenant's writes once the router's grant
names it there, which a failover sends each node it moved tenants to as
it ends -- a lapse's from `granted_after`, and one by hand now too
(`failover_by_hand`, before the operator is answered): left to the next
round, a third of a lease away, the copy refused every write of a tenant
failed over by hand until then. A write a cut sent there before is
refused unrun 503 -- every time in a test of four writes -- so the router
grants that node its list (`grant_cut_onto`: one grant a node at a time
on a thread of its own, every request refused meanwhile waiting on it two
thirds of a lease at most, since a grant each was a reading of every
node's list each and a turn waited on the request's thread) and asks
again, twice (`CUT_ASKS`): the grant it joins may have read the list
before its tenant was placed there, begun for another tenant's request a
promotion earlier, and asking once, such a request was answered the
copy's 503; the second grant began after the first ended, so it reads
the tenant there. Only for a cut, and onto another node than the one it
was cut from. A standby router and a region's router forwarding by its copy of the
global directory fail nothing over, and end what they had out as their
copy of the directory shows the failover, by the same rules (the next
paragraph). Cuts are counted by what became of them
(`fenec_router_cut_requests_total{outcome}`), besides each answer's status
class. `tests/cuts.rs` (a binary of its own) stands a wire that falls
silent between the router and a node, carrying nothing and closing
nothing, with the router's timeout at a minute and each client's at half
of it: by hand, a read, a keyed and an unkeyed write the node ran with
their answers swallowed, a keyed and an unkeyed held claim and a
subscription, each unanswered as the failover begins and answered within
the client's wait -- the read and the keyed write by the copy promoted,
the latter from its key, the unkeyed write and claim 502 and run once, the
keyed claim held there and handed the job put after; the same on a lease
of 10 s, so its five requests are out well before it lapses; the keyed
write and claim under leases answered by the copy, not refused; a tenant
failed over by hand under a lease whose rounds come twenty minutes apart
taking a write as soon as the failover answers; two tenants promoted onto
one node, the second's write joining a grant that read the node's list
before its promotion -- the grant and the second promotion parked at the
node's wire, and a hook counting the requests waiting on a grant
(`cut_grants_waiting`) -- and asking again; an answer
whose first 16 MB had reached the router as the failover ran coming
whole; `tenant-1`'s write whose answer had begun short of a status line
answered 200 through `tenant-q`'s failover; and the cut passed on through
a global router relaying to the region's router that failed the node
over. Each fails with its part taken out -- the cut, the replay rule, the
claim of the answer, the stream's mark, the grant, the failover's grant,
the second ask, the mark kept low --
and the unit tests hold the flight's states, a sweep of 110 000 names of
8 and 16 bytes to no shared mark, a stream failed over between its head
and its copy ending as it is handed over, the count read before a request
goes out --
the connection put back, another bucket's failover passed over -- and a
dial to a listener that answers no SYN (its queue made one long, `listen`
again with 1 -- macOS reads 0 as its most -- and filled) and to one
refused. In `make drill` the dead node's writers wrote again as it was
failed over, 2.52 and 1.93 s after the kill, against main's 3.94 and 3.95
(two runs a side in turns, 0 lost); natively, the drill's flags and a
proxy falling silent, at 3.27-3.32 s against 4.02-4.04 at the drill's
timeout of 4 s and 5.02-5.04 -- the writers' own 5 s -- at the kit's 10,
the failover at 3.26-3.30 either way. A new write landing on a copy
between its promotion and the failover's grant is refused 503 with
`Retry-After`, as during any failover under leases; one a cut sent there
asks for the grant itself. A forward hashes its tenant's name, reads its
bucket's count twice -- acquired, then relaxed -- and writes its
connection's word once, under the pool's lock it takes anyway, and swaps
it once: `make shard-bench ARGS=latency`'s added p50 20.45 us against
main's 20.55, 8 runs a side in ABBA turns at loads of 3.7-4.8 (lower in 3
rounds of 4); with the word stored and the count read sequentially
consistent it was 20.55 against 20.4 over 24 runs a side (the rounds'
differences' median +0.08 us, 9 of 12 positive). Marking a stream at its
head rather than as it is handed over had every forward read its answer's
headers for a length once more, and that build ran +0.2-0.3 us. A read
through a read replica's region's router (`ARGS=reads`, 8 runs a side at
loads of 1.9-4.2) took 63.9 us p50 against main's 66.75, sent home 61.75
against 64.15, lower in every round: the replica's connection itself goes
back to the pool once its body is read and the answer keeps none
(`Answer::reader` an `Option`), where main pools a copy of its descriptor,
a `dup` and a `close` more a read, and a first version of this change
registered that copy under a flight of its own, a lock more and 0.4-0.5
us. `fenec-shard`'s code grew 29.4 KB (aarch64-apple-darwin): `after_cut`
9.3, one out-of-line copy of `answer_out` for the cold callers 8.6 -- a
read replica's answer goes through it too, `forward_near` 8.8 KB smaller
-- `grant_cut_onto` and its thread 5.3, the grants after a failover by
hand 2.2 (a set of the nodes and a thread each were 8); `forward` itself
is 3.1 KB smaller, `elsewhere` out of line now that two inline `toward`.
On x86_64 the answer's claim is a `lock cmpxchg`, some 20 cycles, and the
`xchg` the store before a request's first byte was is a plain `mov` now;
Docker on this Mac runs that architecture only emulated, which times the
emulator, so it was not measured.

**A router following the directory ends what it had out to a node failed
over, as its copy shows the failover** (`Router::catch_up_directories`,
`directory::FailedOver`, `tenants.failed_from`). Only the router failing
a node over cut, and a standby router -- asked by clients a load balancer
sends there, or by the global entry trying a region's routers in turn --
and a region's router forwarding by its copy of the global directory
(`Target::Node`) fail nothing over: what they had out to a node gone with
no reset waited for `--upstream-timeout` though the tenant answered on its
new primary. Each now reads the failover off the directory it follows and
cuts by the leading router's rules (`Pool::cut`, `after_cut`). What tells
a failover from a move is a field of the placement itself:
`failed_from`, the node a failover took the tenant off, written by
`place_promoted` in the very statement that places the tenant on the copy
promoted, and cleared by every other placement (`place`: a create, a
move's two steps, a home). A move must end nothing: its source still
answers, and refuses its frozen writes on its own -- cut, an unkeyed write
out to it as the move ran was a 502 saying it may have run, where the
source answered it. The history `place_promoted` writes into `lineages`
in the same block was weighed and not taken: it is written only where
the tenant's lineage is known, the node names a history in its answer
and the field is there -- a tenant made before lineages, or a node from
before histories, writes none, and its failover would cut nothing -- and
it names no node, so a move and then a failover read in one catch-up
cut the move's source; nor the `state` a promotion writes, which says
`active` as a create and a move's flip do. A follower cuts what it had out
to node `L` for tenant `t` only where its maps placed `t` on `L`, the row
places it elsewhere now and names `L` as the node failed over off
(`Directory::failed_over_off`, asked of the maps before they take the
changes, through the change ring and through a reading of them alike):
a move and then a failover read at once names the move's target, not
`L`; a failover and then a move clears it, and a failover off `L` and
then another off `Z` names `Z`, and what was out to `L` waits as before
either way -- a cut missed, never one made wrong; a later change of a row
a failover wrote leaves `L` in it, but the maps no longer place the
tenant there. `failed_from` is a field of `tenants` (`TENANT_FIELDS`),
added to a directory from before it as `nodes` took its region; a router
from before it neither writes nor reads it, so a router following a
directory such a router leads cuts nothing, as before, and such a router
writing placements into a newer directory leaves `failed_from` as it
was -- a tenant failed over off `L`, moved back onto `L` and off again
by that router is the one case where a follower cuts a move, which an
older standby promoted in the middle of a rolling upgrade over a
directory with the field makes as a downgrade does: so the RUNBOOK has
the standbys upgraded first. A standby read the directory's
changes only as its next request did, and one asked nothing more kept
what it had out waiting: a directory a router follows now tells a thread
of the router's as each write lands (`Arrived`, a `Watcher` on its
database, which the follower's `apply` and an image adopted call under
that database's lock, taking nothing but its own), and the thread brings
the maps up and cuts, as a request finding them stale does. A thread a
router: the `all` test binary, near macOS's 2 048 threads a process
already, lost 14 of its 120 tests to `thread::spawn` failing, so only a
router that follows a directory -- a standby, a region's router -- starts
one, a router leading its own never following it after (a promotion goes
the other way). Each directory is asked under its read lock first and its
write lock taken only where it moved, as each request asked before:
std's lock holds new readers behind a waiting writer, and both write
guards taken for nothing at each wake-up had every forward's `read_dir`
wait on them. The cuts are made under the directory's write lock, so
whoever reads the maps after reads them cut: a request ended goes where
they place its tenant, a forward that read them before and goes out after
finds its bucket's count moved (`Flying`), as the leader's do, and a test
waiting for a standby to place a tenant elsewhere knows whatever it cut
is cut. Both directories are caught up first, their write locks held
together, and what either shows failed over is cut at once
(`cut_failed_over`, `Pool::cut_all`): every bucket of the tenants moved
once, before any connection is shut -- moved a node or a directory at a
time, a request cut off the first went on with the count it read and met
the next one's move before its first byte -- then each node's
connections listed once and each one's mark looked up among its tenants',
where a cut a tenant copied the node's every connection a tenant
-- a node of 1 000 tenants failed over at once, 300 connections open to
it and 100 of them out for those tenants, held the write lock 0.68-0.78
ms, its cuts 0.16-0.20 of it, against 7.35-8.44 and 6.8-7.8 a tenant at a
time, the refresh 0.50-0.66 either way (a scratch test over a directory
and a pool, the median of nine a run, four runs a side in ABBA turns at
loads of 1.5-3.0, the node's tenants grouped through a map; through the
walk of the list it is now, three runs at 3.3, 0.78-0.91 and 0.18-0.26).
Only where the directory follows another router's: the leader's own maps
took its failover as it made it, and it cut then. Under leases a
write a follower replays may reach the copy before the leading router's
grant names the tenant there -- the leader sends it as its failover ends,
and the follower may read the placement first -- and the copy refuses it,
unrun, 503. The leader grants the copy then (`grant_cut_onto`); a follower
grants nothing (`places_as_leader`; `grant_cut_onto` refuses on a
following directory, as `grant_term` did already): the leases are the
leading router's, and a follower's lists may lag its -- a list read
before a failover names the tenant on the node failed over, and granted
it there that node takes writes beside the new primary. Nor can it be
told when the grant lands: the leader records none, and the copy is what
knows. So it asks the copy again, the copy having run nothing
(`copy_pause`: 5 ms first, each wait twice the one before up to a third
of the lease, the leading router's rounds' apart), for two thirds of the
lease the directory records or `--upstream-timeout`, whichever is sooner
(`lease_record`, never shorter than the leading router's own; its own
grant to a copy waits as long), and passes the copy's 503, which says
when to come back, on past that or where no lease is recorded: two
thirds of the tests' hour-long lease, a request was sent again for 40
minutes. Its client is watched between the asks, as a held request's is
(`pause_watching`, one `poll`, watching no more where the `poll` fails):
its end has the router answer it 503 -- saying it ran nowhere where the
cut came before it was sent whole, and otherwise that the node failed
over may have run it and its key sends it again once -- and its
reset lets it go; asked on past its client, a request went to the copy
every pause for as long as the pauses ran. A request asked again that
finds its bucket's count moved before its first byte -- a second
failover in its bucket, read by a catch-up or made by the leader -- wrote
nothing, and stays what it was, cut off its node with its asks where
they stood, a try spent: taken for one never cut, it went on to a copy
not yet granted and passed its 503 on. And it is
counted sent again once, however many times the copy is asked or its
bucket moves: counted at each ask, one request a grant kept waiting was
seven. Fencing is the
leader's, and a follower's cut adds nothing to what may write: it cuts
only on a placement the leader recorded once the copy answered its
promotion -- under a lease, after the node's lease lapsed by its own
clock; by hand, on the operator's word that the node is gone, the leader
cutting at the same moment -- and it replays only what may run twice, a
read, a marked read or a keyed write, which the new primary answers from
its key or makes once, or what never reached the node whole; a write
that reached the node is a 502. `tests/follower_cuts.rs` (a binary of its
own; the wire and the requests are `cuts.rs`'s, moved to `cut_wire.rs`)
holds a standby router's clients, n1 failed over by hand at the primary,
with nothing asked of the standby after: a read, a keyed write and an
unkeyed one n1 ran, a keyed and an unkeyed held claim and a subscription,
each unanswered as the failover begins and answered within the client's
half of the routers' minute -- the read and the keyed write by the copy
promoted, the latter from its key, the unkeyed write and claim 502 and
run once, the keyed claim handed the job put after, the stream ended; the
same through a region's router whose copy of the global directory places
the tenant; a write out to n1 as the tenant moves, held at n1's wire all
through the move, n1's to answer once the standby places the tenant on
its target; a forward whose dial of n1 -- named by a name the test's
resolver holds -- straddles the failover and the standby's catch-up, n1
alive and taking writes, going to the replica promoted; under leases a
keyed write the standby replays before the primary's grant -- held at the
copy's wire -- refused and asked again, then answered by the copy once
the grant lands, the standby waiting on no grant of its own and counting
it sent again once; the same write's client leaving while the standby
waits to ask again, the standby stopping at once and the write landing
nowhere once the copy is granted; and a region's router that leads its
own directory under leases cutting a tenant its copy of the global
directory places, waiting for the global router's grant to the copy and
granting nothing -- taken for its own, the tenant no tenant of the
directory it leads, it granted the copy that directory's list, naming
nothing there, twice, and passed the copy's 503 on; and a keyed write
the standby cut off n1 whose dial of the copy -- its name held at the
test's resolver -- straddles the standby's catch-up of a second tenant of
n1, in its bucket, its promotion held at its copy's wire: the write meets
its bucket moved, and waits for the grant as before, counted once. The
binary's tests run
one at a time, since the cuts counted are the process's, every router's
in it, and each is held to what its own routers counted exactly; the job
a keyed claim takes is put once the claim has been sent to the copy, so
its wait bounds nothing. Each fails with its part taken out: no cut (seven
of the eight, all but the move's), no thread (five: nothing asked them
after the failover), a cut on any node changed (the move's), no
`failed_from` written (seven), no bump of the bucket's count (the dial's),
no asking again (the three lease tests'), a standby that grants -- both
its checks and `grant_term`'s refusal taken out (the same three: its own
grant held at the wire), a count at each ask (the two lease tests that
count), the client unwatched (the leaving client's), `places_as_leader`
asking `following` alone (the region leader's), a move after a cut taken
for a request never cut (the bucket's: the copy's 503 passed on), a count
at each turn of the cut's (the bucket's, two); `directory::tests` hold
the rule over each order of a failover and a move read at once, from the
ring and from a reading, and `upstream::tests` a cut of several tenants,
each bucket moved once (a tenant at a time, two in a bucket moved it
twice), every bucket of a cut off several nodes moved before any
connection is shut (read on the cutting thread as each node's are, a node
at a time it read one short), and a pause watching its client; the
lib's `tests` the asks of a copy over every lease to 12 s and three
upstream timeouts, ending at two thirds of the lease or the timeout,
whichever is sooner, a third of the lease apart at most (either bound
taken out, it fails). Nothing on the forward path
changed but the client's socket handed on to a cut request -- `toward` is
main's code, `connection` asks the same two `stale` reads as before with
the refresh behind them out of line now -- and a router leading its own
directory and keeping no copy of the global one sets no watcher and
starts no thread: `make shard-bench ARGS=latency`'s added p50 was 23.0 us
against main's 23.35 (22.1-23.9 and 21.5-29.4, eight runs a side in ABBA
turns at loads of 1.9-2.9 beside other agents' work, the runs'
differences' median -0.45 us), `ARGS=standby`'s standby 22.5 against 22.7
(22.2-23.3 and 22.1-23.1, the differences' median +0.10), and
`ARGS=regions`' region's router 62.85 against 62.75 (-0.10), its relay
40.95 against 41.2 (-0.25), and its failover as it was: at a lease of 1 s
the node cut off stopped 560 ms after the cut against 526 (533-631 and
470-572, the cut's phase against rounds a third of a lease apart) and its
tenants wrote again 1 516-1 550 ms after against 1 528-1 560, at 5 s 4 105
and 5 998-6 028 against 4 105 and 6 001-6 033, medians of eight a side;
the change's first form, on a quieter machine, ran 20.8 against 20.9 and
21.1 against 21.45 through a standby. A standby's catch-up reads
`failed_from` beside each placement and asks the maps of a failover's
row: 4.08 us p50 a change that moves one of 100 000 tenants and 4.25 one
a failover made, against 3.88-3.92 on main, and 3.94 and 4.22 at 10 000
against 3.85-3.88 (a scratch harness applying a primary directory's
records to a follower's database, 4 000 changes a run, four runs a side
in ABBA turns) -- off the request path now, the thread the writes wake
catching up before a request finds the maps stale. `fenec-shard`'s code
grew 24.8 KB (`__text`, aarch64-apple-darwin, `size_report.py --bin` in
each tree): `catch_up_directories` 6.7, `after_cut`'s asking again, its
client watched and counted once, 6.0, the directory's field, its
statements and the row's check 4.9, the thread and `serve_on`, which
`main` inlines no more, 3.4, the watchers 2.8, `cut_failed_over` and
`Pool::cut_marks` 2.6, `grant_cut_onto`'s refusal 1.2, `connection` 1.6
smaller. The browser module is the same to the byte: no crate it is built
from changed.

## Queries across tenants

**A query across tenants is the router's admin's, merged by what the
request says** (`fenec-shard/src/fanout.rs`, `POST /_shard/query`). An
operator asked every tenant themselves for a count each or the newest
sign-ups of all. Tenants are the boundary, so it sits behind the
router's `--token` like the rest of `/_shard/`: a JWT bound to a tenant
and the nodes' data token are 401 there. Each tenant is asked through its
node's `POST /_admin/tenants/<t>/query` (`fenec_http::admin_query`) with
the admin token the router holds -- one that exports a tenant's whole
image learns nothing more by reading it; the data token reaches every
tenant and writes -- which refuses a statement that writes before it runs
and runs the rest in read-only mode besides. The router parses no FenecQL:
`query` and `params` go to every node as their JSON came (validated with
`parse_json`, never re-encoded), and the merge is the request's own JSON,
three kinds exact from the answers alone -- none (rows one after another,
a `tenant` column spliced in), `order` with `limit` (a k-way merge of rows
each tenant gives in that order, checked, a tenant out of it failed rather
than merged wrong) and a fold (`sum`, `min`, `max` by `group`; every
column grouped or folded or the tenant fails, since a column dropped is an
answer cut; no `avg`). A text column the tenants compare otherwise than by
its bytes -- a field's `collate tr` or `und`, a statement's `order ...
collate`, a `nocase` field's fold -- is said again, since the router reads
no schema: an object in place of its name (`{"column", "collate",
"nocase", "desc"}`, `column_of`), an `order` key or a `min`, `max` or
`group` column alike, `"nocase": true` the fold a field in that collation
has (Turkish's under `tr`) and `"tr"` or `"und"` the field's own where the
statement's `collate` names another. It compares by fenec-core's
`TextOrder` as the engine's `order` does (`value_in`, `cmp_in`): read as
text, never as the time an ISO form names; folded once as it is read, as
`folded_keys` folds; the collation over the folds; and the texts folding
to one tied -- tied by their bytes, a tenant's `ada@x` before its `Ada@x`
in id order failed it -- so a `nocase` group is one for every text folding
to it. Not named, an order key fails such a tenant as before, while a
collated `min` was, and is, the bytes' least of each tenant's least,
unrefused; a collation other than `tr` or `und` is 400, named.
`a_text_order_named_compares_as_the_engine_orders` holds the check and the
ties to the engine's own `order`, both ways, over 240 words of `I ı İ i`,
`ß ẞ`, `Ç` and ISO forms in eight ways a text orders, failing with an ISO
form read as a time or ties broken by bytes, and a tenant told the other
fold fails. The router linked every collation chunk already, through its
directory's engine: its code grew 8.0 KB (`__text`, aarch64-apple-darwin),
its file not by a byte (3 117 168). Rows are passed on as the nodes wrote them
(`scan`: extents found, only the merge's values decoded, no key allocated
a member), and a timestamp, which travels as ISO text with its
milliseconds left out where they are 0 so its bytes do not order as its
time, is read from its digits (`iso_ms`, held to `format_iso` over 200 000
instants): read through `time::parse` and on the request's thread after
the last answer, 2 000 rows held it 0.69 ms, now read by the worker that
fetched each answer. A tenant failing, timing out (one timeout a query,
every connect and read bounded by it: `Pool::send_by`,
`Answer::read_body_by`) or not fitting the merge is named with node,
status and why; the query fails with them (the first stops the asking)
unless `"partial": true`. A tenant moved under the query is asked again
where the directory places it now, one deleted is a failure. Rows and
bytes gathered are bounded (`--fanout-max-rows`, `--fanout-max-bytes`,
the bytes before a body is read) and refused past, 413, never cut.
Consistency is a tenant's: no one moment across them. Calls go out
`--fanout-concurrency` at once, node by node in turn; one span a node
under a `fanout` span (`trace::Remote`, recorded by the request's thread
for the workers' calls), the request id on every call, three metric
families with no tenant label, one audit line at the router (the nodes'
reads are not admin changes, `audit::http`). Over 100 tenants on three
nodes a summed count took 1.15-1.23 ms and a merged top 20 1.35-1.39,
against 4.3-5.5 asking each in turn through the router and 1.9-2.2
sixteen at once with no merge (`make shard-bench ARGS=fanout`), a top 20
of `collate tr nocase` names 1.33-1.48 -- in twelve ABBA turns beside other
agents' work (load 3.5-6.4), the plain top 20 1.34-1.46 against main's
1.34-1.49, medians 1.40 and 1.395; a forwarded request's added p50 23.0
us against main's 23.8 in ABBA turns.
The SDKs have no `/_shard/` call, so none for this; the studio does not ask it.
