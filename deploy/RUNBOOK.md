# Running fenecdb across regions

The operator's runbook for a self-hosted, multi-region fenecdb: what runs
where, the order it comes up in, and what to do when something breaks. The
design behind each step is in [Tenants and sharding](https://fenecdb.com/docs/sharding);
this page is the procedures, and the files beside it are the kit:

| File | What it is |
| --- | --- |
| `systemd/fenec-node.service` | a tenant node, `fenec-server --dir --lease --sync always` |
| `systemd/fenec-router.service`, `fenec-router-standby.service` | a region's router (`fenec-shard --region`) and its standby |
| `systemd/fenec-global.service`, `fenec-global-standby.service` | the global router and its standby |
| `systemd/fenec-archive@.service` | a node's archive, `fenec archive --tenants`, one instance a node |
| `systemd/fenec-archive-sync@.service`, `.timer`, `bin/fenec-archive-sync` | that archive copied to a bucket by rclone, every five minutes |
| `systemd/cloudflared.service`, `cloudflared/config.yml` | the tunnel the global entry Worker reaches a router through |
| `systemd/fenecdb.sysusers` | a user for each role: `fenec-node`, `fenec-router`, `fenec-archive`, `fenec-sync` |
| `env/*.env` | the environment files the units read: addresses, flags and tokens |
| `alerts/fenecdb.rules.yml`, `alerts/grafana.yml` | alert rules, for Prometheus and for Grafana |
| `drill/drill.py` | `make drill`: the failovers below, in Docker, timed |
| `test/units.py` | `make deploy-check`: these units under a real systemd |
| `test/ingress.py` | `make deploy-check`: the tunnel's rules, asked of cloudflared's own matcher |

Nothing here deploys anything: the kit is files to install and procedures
to follow, held to the binaries by `make deploy-check` and `make drill`.

## The shape of a deployment

```
                    clients
                       |  https://db.example.com/t/<tenant>/...
              global entry Worker (integrations/cloudflare/entry)
                 |                               |
      Cloudflare Tunnel (cloudflared)    Cloudflare Tunnel
                 |                               |
   region eu ----+-----------------    region us +------------------
   router-1 (zone fra)  router-2 (ams)  router-1 (iad)  router-2 (pdx)
   fenec-router         -standby        fenec-router    -standby
        |  leases, places, fails over        |
   node eu-a (fra)   node eu-b (ams)     node us-a (iad)  node us-b (pdx)
   archiver of each node, on another machine; its archive -> bucket

   global router (one region) and its standby (another): every tenant's
   name and home region; each region's routers
```

- **A region** is two datacenters or more (zones). Its router keeps the
  region's own directory -- its nodes, its tenants, their leases -- so it
  leases its nodes and fails them over cut off from every other region.
  Its standby, in another zone, follows that directory.
- **A node** holds a file per tenant. With `--replicas` each tenant has a
  replica on a node of another zone of its region, which a failover
  promotes; every node holds primaries and replicas both.
- **The global router** holds what every region must agree on: tenant
  names and their home region. It leases nothing. Its standby is in
  another region.
- **The entry Worker** gives every tenant one URL, learns each tenant's
  home from the routers' answers and sends its requests there, and names
  the client to the routers (`Fenec-Client`, under the entry token).
- **Archivers** keep every write of every tenant of a node, with the time it
  was made, on a machine apart from the node's; rclone copies the archive to
  a bucket.

### The network

fenecdb speaks no TLS and no part of it should face the internet. Nodes,
routers and archivers talk over a private network: a VPC within a region,
and between regions WireGuard or the provider's encrypted peering --
relays, the global directory, read replicas and moves cross it. The only
public way in is Cloudflare's edge, through the tunnels. A tunnel's
hostname is public -- anyone may ask it, not the entry Worker alone -- so
two things stand in front of each router:

- **The tunnel's rules** (`cloudflared/config.yml`) say which paths: a
  tenant's alone. The router's own `/_shard/`, `/_replication`,
  `/_metrics` and `/_studio/` answer 404 at the edge whatever token a
  request carries, and so does a tenant's replication stream,
  `/t/<t>/_replication`: the router forwards it to the node, and the nodes'
  one replication token opens it -- every row of every tenant. cloudflared
  matches a rule against the path decoded, so `/t/acme/%5Freplication` and
  `/t/acme%2F_replication` meet that rule as the plain spelling does;
  `test/ingress.py` (in `make deploy-check`) asks cloudflared's own matcher
  for 28 paths, the tenant's and every refused one.
- **Cloudflare Access** says who. On each tunnel's hostname, a
  self-hosted Access application whose one policy admits a service token
  (action *Service Auth*), and the token handed to the entry Worker:
  ```sh
  cd integrations/cloudflare
  npx wrangler secret put ACCESS_CLIENT_ID --config entry/wrangler.jsonc       # the token's Client ID
  npx wrangler secret put ACCESS_CLIENT_SECRET --config entry/wrangler.jsonc   # and its secret
  ```
  The Worker sends both with every request to a router
  (`CF-Access-Client-Id`, `CF-Access-Client-Secret`, a client's own copies
  dropped), and the routers drop every `Cf-Access-*` header -- the token,
  and the JWT Access adds for it (`Cf-Access-Jwt-Assertion`) -- before a
  request reaches a node. A request that is not the entry's then never
  reaches a router at all; the tunnel's rules and the routers' tokens hold
  either way. The Worker refuses to start with a service token and a
  router named by `http://`, which would carry the secret in clear.

  **Turn on strict service token authentication** (Zero Trust, Access
  controls, Access settings, "Manage service tokens"; through the API,
  `"strict_service_token_auth": true` on the organization; on by default
  for organizations made since 2026-10-05). Without it, Access answers a
  request its service token admitted with a `CF_Authorization` cookie for
  the hostname -- a session that reaches the router past Access with no
  token at all. The entry drops every `Set-Cookie` of a router's answer
  (no router or node sets one), so its clients never see the cookie either
  way; strict authentication keeps Access from making it.

Each node and router listens on its private address (`FENEC_LISTEN`), with
a token: a router or a node refuses an address past loopback without one.

## Tokens

Every token is made with `openssl rand -hex 32` and lives in an
environment file, `/etc/fenecdb/*.env`, mode 0600, owned by root. systemd
reads it as root before the process starts, so no service's user can read
the file, and no token is on a command line (`ps` shows every process's
arguments to every user). A process's environment, `/proc/<pid>/environ`,
is readable by its own user's processes: each role is a user of its own
(`fenec-node`, `fenec-router`, `fenec-archive`, `fenec-sync`), so a node
cannot read a router's tokens, nor rclone the archiver's, where two roles
share a machine. `make deploy-check` holds all of it, each user against
every other role's processes. The archive's key and the tunnel's and
rclone's credentials are files handed to their service alone with
`LoadCredential=`; `env/archive-sync.env` holds no secret (the bucket's
name) and is 0644.

| Token | Variable | Held by | Reaches |
| --- | --- | --- | --- |
| data | `FENEC_HTTP_TOKEN` (node) | the tenants' clients | every tenant of every node, read and write; prefer JWTs (`--jwt-keys`, bound to a tenant) for anything a user holds |
| node admin, one a node | `FENEC_ADMIN_TOKEN` (node) | the region's router, in its directory | `/_admin/` of that node: create, delete, freeze, move, export a tenant |
| node replication, one for all | `FENEC_REPLICATION_TOKEN` (node, archiver) | nodes, archivers | every tenant's replication stream: every row |
| routers | `FENEC_SHARD_TOKEN` (every router) | operators, the app that creates tenants, the scraper | `/_shard/`: place, move, delete every tenant; and the HMAC a router marks a relayed client with |
| directory replication | `FENEC_SHARD_REPLICATION_TOKEN` (every router) | routers | the directories' streams, which carry every node's admin token |
| entry | `FENEC_SHARD_ENTRY_TOKEN` (every router), `ENTRY_TOKEN` (Worker) | the entry Worker | believing the client address the entry names |

The entry token is the only one a third party's machine (Cloudflare's)
holds, so a router refuses to start where it equals `--token`, the
directory replication token or a node's admin token. Keep it apart from
the nodes' tokens too, which the router never sees.

There is no token that reads `/_metrics` alone: a node's takes its data or
its admin token, a router's its `--token`. Run the scraper on the private
network, keep its token in its own secret store (Prometheus's
`credentials_file`, the Datadog Agent's secrets), and scrape nodes with the
data token rather than the admin token.

## Installing a machine

On every machine (Debian 12 or later, Ubuntu 22.04 or later -- systemd 248
or later for `LoadCredential=`'s `%d`):

```sh
install -m 0755 fenec-server fenec-shard fenec /usr/local/bin/   # from a release
install -m 0644 systemd/fenecdb.sysusers /etc/sysusers.d/fenecdb.conf && systemd-sysusers
install -m 0644 systemd/*.service systemd/*.timer /etc/systemd/system/
install -D -m 0755 bin/fenec-archive-sync /usr/local/libexec/fenecdb/fenec-archive-sync   # an archiver's machine
install -d -m 0755 /etc/fenecdb
install -D -m 0644 RUNBOOK.md /usr/local/share/doc/fenecdb/RUNBOOK.md
systemctl daemon-reload
```

Then the environment file of each role the machine has, copied from
`env/`, every example value replaced, mode 0600 (`install -m 0600`). The
data go under `/var/lib/fenecdb/`, which a disk of its own mounted there
keeps apart from the system's.

Each unit runs as its role's user with nothing writable but its own state
and log directories, no capability, a system call filter, a private `/tmp`
and other users' processes hidden (`systemd-analyze security` puts each at
1.2). `/proc/meminfo` stays readable: a node compacts a file before it
outgrows the page cache, and reads the cache's room there.

Every connection is a thread and a descriptor, and a node's
`--max-connections` (4 000 in `fenec-node.service`) is shared by clients'
requests, their subscriptions and held writes, the followers of the
tenants it holds replicas of -- one each, for as long as they follow --
and its archivers. A node holding the replicas of 1 000 tenants holds
1 000 of its connections for them alone; size it as 1 000 tenants of
replicas, the clients' peak and a quarter more, and raise `LimitNOFILE`
and `TasksMax` with it (65 536 and 16 384 leave room for 4 000).
`FenecConnectionsNearCeiling` fires at four fifths. A router's ceiling (its
default, 1 000) is its clients' and the regions' relays.

`TimeoutStopSec` leaves a node 120 s to sync every open tenant and
checkpoint their graphs on SIGTERM; a kill past it loses no write under
`--sync always`, only the checkpoint. Never `systemctl reload` a fenecdb
unit: the servers take SIGHUP for a shutdown.

### Logs

The servers write their log to the journal and refusals, schema changes
and admin changes to an audit log, `/var/log/fenecdb/<role>/audit.log`
(`node`, `router`, `global`), 0600 under the units' `UMask` -- it names
clients' addresses and every tenant changed. A log shipper reads it through
an ACL, set once on the directory so each new file inherits it:

```sh
setfacl -m u:<shipper>:rx -m d:u:<shipper>:r /var/log/fenecdb/node
```

The server opens the file once and appends; it does not reopen it, and
SIGHUP shuts it down. So logrotate copies and truncates:

```
/var/log/fenecdb/*/audit.log {
    daily
    rotate 14
    compress
    missingok
    copytruncate
}
```

A line written between the copy and the truncate is lost from the
rotated file; the journal keeps the servers' own log.

### The routers' upstream timeout

`FENEC_UPSTREAM_TIMEOUT` (`router.env`, `global.env`: 10 s, fenec-shard's
own default 60) is how long a router waits on a node -- or the global
router on a region's router -- for each read of an answer: the connect,
the answer's head, and every read after it but a stream's, which reads
with none. Past it the client is answered 502, the request perhaps run.

- **What it buys.** A request in flight to a node whose machine is gone
  waits out the timeout, its client unanswered, unless the client gives up
  first; at fenec-shard's 60 s every client without a bound of its own
  waited a minute. At twice the lease it is let go of once the node's
  tenants have been failed over (a lease and a tenth), so a client's
  retry of a read or a keyed write reaches the new primary. With
  `make drill`'s lease of 2 s and a timeout of 4 s, the router failed the
  killed node over about 2 s after the kill and its tenants' writers wrote
  again at about 4 s, where at 60 s their own 5 s client timeout had them
  at 4.9.
- **What it costs.** An answer slower than the timeout to begin is a 502
  while the node goes on with it: a `create index` over a large
  collection, a `compact` of a large tenant, a large batch or a heavy
  aggregate. A write so cut may have run, which an `Idempotency-Key` makes
  safe to send again; run such statements against the node on the private
  network, or raise the timeout for them. A move copies a tenant's image
  through the region's router under the same bound, read by read: a
  tenant whose export or install takes longer fails the move, which puts
  it back as it was.
- **What it leaves alone.** A request carrying `Fenec-Wait` -- a held
  claim, or a read waiting for a write with `Fenec-After` -- is allowed the
  timeout plus its wait (30 s at most), so a claim held 25 s is answered
  within 35. A `Fenec-After` read with no `Fenec-Wait` waits 5 s, inside
  it. A `/_changes` page's `wait` is a query parameter the router does not
  read: keep it under the timeout through a router. Streams -- a
  subscription, a replication stream -- read with no timeout once their
  head is in.

Raise it with the lease, keeping it at twice the lease; lower it no
further than the slowest statement your clients send through the routers.

## Bring-up

In this order, each step checked before the next. `$ROOT` is the routers'
token; `g1` the global router's private address.

1. **The global router**, `fenec-global` with `global.env`, then its
   standby, `fenec-global-standby` in another region, `FENEC_PRIMARY`
   naming it.
2. **Each region's router**, `fenec-router` with `router.env` --
   `FENEC_REGION`, `FENEC_GLOBAL` the global router then its standby --
   and its standby, `fenec-router-standby`, on a machine of another zone.
3. **Record each region** on the global router, its router first:
   ```sh
   curl -X PUT http://g1:8090/_shard/regions/eu -H "Authorization: Bearer $ROOT" \
        -d '{"routers":["http://10.1.0.21:8090","http://10.1.1.21:8090"]}'
   curl http://10.1.0.21:8090/_shard/regions -H "Authorization: Bearer $ROOT"   # the region's router has heard
   ```
4. **The nodes**, `fenec-node` with `node.env`, at least two a region in
   two zones, then each recorded with its region and zone (through any
   router; the region's takes it):
   ```sh
   curl -X PUT http://g1:8090/_shard/nodes/eu-a -H "Authorization: Bearer $ROOT" \
        -d '{"addr":"10.1.0.11:8081","token":"<eu-a FENEC_ADMIN_TOKEN>","region":"eu","zone":"fra"}'
   ```
   The router checks the node with the token before it records it, and
   leases it from then on.
5. **The tunnels**: `cloudflared` on each router's machine, its hostname
   routed to that router alone (`cloudflared/config.yml`). Each tunnel is
   made on an operator's machine -- `cloudflared tunnel login` leaves the
   account's certificate, which makes and deletes every tunnel, there --
   and only its credentials file is copied to the router
   (`systemd/cloudflared.service` has the lines). Then an Access
   application on each hostname (The network).
6. **The entry Worker**: each region's routers in `REGIONS`
   (`integrations/cloudflare/entry/wrangler.jsonc`), the one keeping its
   directory first, then
   ```sh
   cd integrations/cloudflare
   npx wrangler secret put ENTRY_TOKEN --config entry/wrangler.jsonc   # every router's FENEC_SHARD_ENTRY_TOKEN
   npx wrangler deploy --config entry/wrangler.jsonc
   ```
7. **The archivers**, `fenec-archive@<node>` for each node on a machine
   apart from it, and `fenec-archive-sync@<node>.timer` (Archives and
   restores, below).
8. **Monitoring**: Prometheus or Grafana with `alerts/`, scraping every
   node and router, and node_exporter on each archiver's machine with its
   textfile collector (`alerts/fenecdb.rules.yml` has the scrape config;
   its job names are the rules').

A first tenant, made through the global router and written through the
entry:

```sh
curl -X PUT http://g1:8090/_shard/tenants/acme -H "Authorization: Bearer $ROOT" \
     -d '{"region":"eu","schema":"create collection notes (title text)"}'
# {"tenant":"acme","node":"eu-a","region":"eu","replica_on":"eu-b",...}
curl -X POST https://db.example.com/t/acme/notes -H "Authorization: Bearer $DATA" -d '{"title":"hello"}'
```

## Adding a node

Start it with `node.env` (its own admin token) and record it with its
region and zone, as in bring-up. New tenants and new replicas go to it by
the rules: a replica to a node of another zone first, then the fewest. A
region of a single zone gives each tenant a replica in its own zone where
nothing else is free, logs it, and counts it in
`fenec_router_same_zone_copies` (alert `FenecSameZoneCopies`): a
datacenter lost takes both. A node added in another zone does not move
those copies; moving such a tenant (`POST /_shard/tenants/<t>/move`)
places its replica again by the zone rule.

A node is taken out with `DELETE /_shard/nodes/<n>` once it holds no
tenant, replica or read replica: move them off first.

## Adding a region

1. Its nodes, two zones at least, and its router and standby
   (`router.env` with the new `FENEC_REGION`), as in bring-up steps 2 to 5.
2. Record the region on the global router (`PUT /_shard/regions/<r>`),
   then its nodes.
3. Add it to the entry's `REGIONS` with the continents whose clients a new
   tenant is guessed there for, and deploy the Worker again.

## Placing and moving a tenant

```sh
# in a region, on its node with the fewest bytes, with its schema and quota
curl -X PUT http://g1:8090/_shard/tenants/acme -H "Authorization: Bearer $ROOT" \
     -d '{"region":"eu","schema":"create collection notes (title text)","quota":{"bytes":1073741824}}'
# to another node of its region, or to another region (its image copied over)
curl -X POST http://g1:8090/_shard/tenants/acme/move -H "Authorization: Bearer $ROOT" -d '{"to":"eu-b"}'
curl -X POST http://g1:8090/_shard/tenants/acme/move -H "Authorization: Bearer $ROOT" -d '{"region":"us"}'
```

A tenant made naming no region goes to the global router's
`--default-region` (`FENEC_GLOBAL_FLAGS`). Writes during a move are answered
503 with `Retry-After`; reads go on. A move across regions, a create and a
delete need every region they touch and are refused, changing nothing,
while one is out of reach. A move that does not record done
(`FenecTenantsMoving`) left the tenant served where it was: move it again.

## Quotas

The node's flags (`FENEC_NODE_FLAGS`) are every tenant's defaults; a
tenant's own set goes through its region's router (`10.1.0.21`, eu's, here),
which keeps it in its directory and sends it wherever the tenant goes -- its
replica, a move's target, a promoted copy:

```sh
curl -X PUT http://10.1.0.21:8090/_shard/tenants/acme/quota -H "Authorization: Bearer $ROOT" \
     -d '{"bytes":1073741824,"rows":0,"writes":100}'
curl http://10.1.0.21:8090/_shard/tenants/acme/quota -H "Authorization: Bearer $ROOT"   # set, held to, used
```

`bytes` and `rows` past the quota are 507, the write put back whole;
`streams`, `held`, `requests`, `writes` and `write_bytes` are 429 with
`Retry-After`. Deletes and compacts always land, so a tenant deletes its
way back under. Unset, `streams` and `held` are each a quarter of
`--max-connections`, so no one tenant's subscriptions and held claims take
the node's connections. `fenec_quota_refused_total{quota}` counts the
refusals (alert `FenecQuotaRefusals`, informational: a tenant on its plan's
limit is working as sold); `GET /_admin/quotas` on a node, or
`/_shard/tenants?quota` on the region's router, names the tenants.

## A node lost

With `--auto-failover` the region's router fails a node over by itself. A
node stops writing as its lease lapses by its own clock; the router, a
tenth of a lease later, promotes each of its tenants on its replica in the
region's other zone and routes it there.

What a failover keeps: under `--sync always` a write is answered once it is
on the primary's disk and written into its replica's stream -- handed to
the primary's kernel, not yet on the replica's. That survives the primary's
process dying, whose kernel still sends what its socket holds, and the
replica's machine dying, since the primary holds the write. It does not
survive the primary's machine dying in the moment between the answer and
the bytes leaving it: a write answered then is on a disk no one can read
until the machine is back, and the promoted replica never had it. And a
stream that has missed a write for a second is not waited for until it
catches up (`SENT_WAIT`): while a replica is behind, writes are answered
with no copy beside them, and a primary lost then loses them. Writes the
old primary took that were never sent are dropped when it comes back as a
replica (A node back).

Seen in `make drill` (lease 2 s, the routers' upstream timeout 4 s, three
tenants a region, the node cut off the network and then killed, as a
machine lost) -- three runs on one laptop, which are observations, not
bounds: another machine's runs fall outside them. The router failed the
node over 1.92, 2.56 and 1.90 s after the kill -- a lease and a tenth
after its last renewal, then the promotions; its own tenants' writers
wrote again 3.90, 3.97 and 3.93 s after the kill, the request each had in
flight to the dead node let go of at the upstream timeout (The routers'
upstream timeout); the tenants whose replica it
held, whose primaries went on writing, went at most 17, 18 and 17 ms
between two answered writes, and every other tenant 16, 18 and 17; none
of 41 577 acknowledged writes was lost. The second kind waits up
to a second (`SENT_WAIT`) where its replica's stream stops taking bytes:
with the node killed alone rather than cut off, one such tenant went 639 ms
without an answered write. A node slow to answer, or to be found -- one
recorded by a name its resolver is slow to refuse once the machine is gone
-- holds up its own renewal and its own failover alone: each grant goes out
on a thread of its own, a name is looked up within a third of the lease,
and a replica being promoted stops reconnecting to the dead node at once.
With the default lease of 5 s, count about 6 s to the failover.

What to do:

1. `FenecNodeFailedOver` and `FenecAutoFailover` fire. Check the router's
   log (`failover of <node>: 200 {...}`) and `GET /_shard/tenants?region=<r>`:
   every tenant placed on a live node. `FenecAutoFailoverIncomplete` means
   tenants are still placed on the failed-over node, unwritable until it
   answers again or they are failed over. The router's log names why each:
   - its replica's node did not answer -- fix that node, then `POST
     /_shard/nodes/<n>/failover` on the region's router;
   - another operation held it (a move, a quota being set) -- it is failed
     over once that ends, onto the copy it had then;
   - its replica's node refused the copy, 409: a copy of another lineage
     (one that never took its primary's image, or a file put in its place
     by hand), or one lacking the history of the tenant's last failover
     that has heard nothing from its primary since it was told to follow
     it (an old primary's copy a repair pointed at the new primary). The
     tenant waits for its node, where its rows are: bring the node back.
2. The failed-over tenants now have no replica (`FenecTenantsWithoutReplica`)
   until a node takes one: bring the node back, or give them replicas
   elsewhere with `POST /_shard/replicas` on the region's router. No tenant
   is given a replica while its primary does not answer; the router looks
   again over the next four minutes, and that call asks at once.

**Force.** `{"force": true}` on `POST /_shard/nodes/<n>/failover` (or on
the copy's node, `POST /_admin/tenants/<t>/promote`) promotes whatever the
copy holds, past the node's refusal; the router writes `failover_forced`
and the node `promote_forced` into their audit logs, the node naming the
refusal it went past. Reach for it only when the node is gone for good --
its disk lost, no archive newer than the copy -- and the copy is the best
there is: whatever the copy never took from the primary is lost, and a
copy of another lineage is another file. Or when a file of another
lineage was put in the primary's place on purpose (a tenant made again,
another file restored there) and no read replica's look has taught the
router its lineage yet. Never while the node may come back with its rows:
once a copy is forced, the node back follows it as any old primary does,
and whatever it held that the copy lacked is gone (A node back). Where an
archive holds more than the copy, restore from it instead (Archives and
restores).

**By hand.** Without leases, or to fail a node over before its lease
lapses because you know it is gone: `POST /_shard/nodes/<n>/failover` on
the region's router. Never on a node that may still be running and
reachable by clients without leases: that makes two primaries.

## A node back

Start it (`systemctl start fenec-node`). It comes back with no lease, its
old tenants' files still primaries' files. As it takes its first lease the
router runs a repair: each tenant it lost follows its new primary from
this node -- this node becomes its replica -- and the writes it took that
were never sent are gone, the trade any failover makes. Three `make drill`
runs saw the repair 0.51, 0.51 and 0.50 s after the start (1.07 in a
reviewer's run), the copies caught up 25 to 39 ms later.

Check: `GET /_shard/tenants/<t>` names the node as `replica`;
`FenecNodeFailedOver` and `FenecTenantsWithoutReplica` clear. A node whose
disk was lost stays down until it has been failed over -- a lease -- and
then starts empty: the repair places replicas there from images. Started
sooner, it would hold the leases of tenants it no longer has.

## A lease late

`FenecLeaseRenewalLate`: in the last five minutes a node went more than
three fifths of a lease without one, a renewal missed, and was not failed
over. The node or the network between it and its router is slow or
dropping packets; if it lasts a lease, the node is failed over, and
`FenecNodeFailedOver` says so instead -- the rule passes over a node failed
over in the last ten minutes, and the minute before a failover marks one.
Look at the node's load and disk, and at the path between the two.

## A router lost

The standby keeps forwarding from its copy of the directory, so data
requests go on -- the entry and the other routers pass over a router they
cannot reach. But only the primary leases the region's nodes: unrenewed,
they stop writing as their leases lapse, and the region's tenants answer
503 until the standby is promoted. Promotion is an operator's decision,
never the routers': a standby that promoted itself whenever it could not
reach the primary would make two primaries in a partition.

1. Make sure the old router is gone, and stays gone: where its machine is
   reachable, `systemctl mask --now fenec-router` there, so a reboot or a
   `systemctl start` cannot bring it back as a primary; where it is not,
   power it off from the provider's console and mask the unit the first
   time it boots. A promoted router fences it at every node it reaches --
   grants of a lower term are refused -- but not at a node only the old one
   still reaches.
2. Promote the standby and switch its unit at once (`make deploy-check`
   runs exactly these lines):
   ```sh
   curl -X POST http://<standby>:8090/_replication/promote \
        -H "Authorization: Bearer $FENEC_SHARD_REPLICATION_TOKEN"
   systemctl enable fenec-router && systemctl disable fenec-router-standby
   systemctl start fenec-router     # stops the standby's unit; same file, opened as the primary's
   ```
   The promoted router grants every node its lease and writes resume. Left
   under the standby's unit, a restart -- a crash, a reboot -- would have the
   promoted file follow the dead router again.
3. Put it first in the region's record on the global router:
   `PUT /_shard/regions/<r> {"routers":["http://<new>:8090","http://<old>:8090"]}`.
   And take the old router out of Prometheus's targets, or silence it,
   until it rejoins: down and last seen a primary, it is
   `FenecPrimaryRouterDown` (critical) for an hour, and then
   `FenecRouterDown`, critical, for as long as it is scraped.
4. When the old machine returns, it rejoins as the new one's standby:
   `FENEC_PRIMARY` pointed at the new router, `fenec-router-standby`
   enabled, `fenec-router` left masked. Started as a primary, it would lease
   at its old term -- refused by every node the new one has reached -- and
   take `/_shard/` changes into a directory nobody else reads.

Seen in `make drill` (lease 2 s), three runs: the region's writes stopped
1.67, 1.61 and 1.66 s after its router was killed, as the leases lapsed
(1.31 in a reviewer's run); once the standby was promoted -- an operator's
minute in production, 2.7 s in the drill -- writes resumed 0.45, 0.44 and
0.51 s after the promotion, and the switch of units, a restart of the only
router the region then had, left gaps of 0.51, 0.39 and 0.42 s between two
answered writes.

**The global router.** Tenants go on working: every region keeps its own
directory. Creates, deletes and moves across regions are refused (503)
until its standby is promoted the same way (`fenec-global`). Then put the
new one first in every region router's `FENEC_GLOBAL` and restart each,
standby first: a region's router follows the global directory from the
first router named, and a restart within the lease leaves its nodes'
leases standing.

## Upgrading with no downtime

Upgrade one machine at a time, standbys before primaries, and watch the
alerts between steps:

1. **`fenec` (archivers, operators' machines)**: restart each archiver; it
   reconnects to its node and goes on in the same archive.
2. **Routers**: each region's standby, then promote it (A router lost) --
   or restart the primary in place, which loses no acknowledged write,
   since the nodes' leases outlast it, but answers the requests in flight
   through it as the connection ends: the entry sends a read or a keyed
   write on to the standby, and answers any other write 502, which may have
   run (send writes with an `Idempotency-Key` and they are retried safely).
   Then the global standby and the global router the same way.
3. **Nodes**, one at a time, a zone at a time: replace the binary and
   `systemctl restart fenec-node`. A restart shorter than the lease costs
   its tenants the restart; one longer -- a shutdown checkpointing large
   graphs -- has them failed over to their replicas, and the node comes
   back as a replica of each (A node back). Either way no acknowledged
   write is lost. Wait for `FenecTenantsWithoutReplica` to clear before the
   next node.
4. **The entry Worker** last: it reads only the routers' answers.

What a mix of versions tolerates is said release by release in the
changelog; the docs hold these today: a router from before regions reads
the directory as it always did, forwarding every tenant where it is and,
promoted, placing without regard to regions; one from before regional
directories answers 404 for a tenant of a region keeping its own -- so
upgrade every router before turning either on. A node from before lease
terms takes grants without checking the term, and a region's router then
does not fail it over on its own; a node or router from before lineages
protects less and breaks nothing. Files go one way: a binary refuses record
kinds newer than it, so once an upgraded
node has written a tenant, that node cannot go back to the older binary --
and an older node may not follow a newer primary's replication stream where
the stream's version moved. Upgrade every node of a region within one
window, and roll forward, not back. fenecdb is in alpha: read the release
notes before each upgrade.

## Archives and restores

Each node is archived by `fenec-archive@<node>` on another machine: one
connection for all its tenants, an hourly image of each, a week kept
(`--image-every 1h --keep 7d`), sealed with the key in
`/etc/fenecdb/archive.key` (`fenec key` writes one; keep a copy apart from
the archive -- without it nothing restores). A replica's copy is archived
as a primary is, its writes the primary's and numbered alike, so every
tenant is in two archives: its primary's node's and its replica's. A write
is in the archive once its node's stream has sent it -- under load the
stream may be behind its node (Known gaps) -- and in the bucket after the
next `fenec-archive-sync` run that succeeds: the timer starts one every
five minutes, so a write is there five minutes and a run's length after
the archive took it, while runs succeed. One that fails is retried at the
next tick, and `FenecArchiveSyncStale` fires once no run has succeeded for
half an hour. Each success is written into node_exporter's textfile
collector, `/var/lib/prometheus/node-exporter`, which the `fenec-sync` user
must be able to write (`setfacl -m u:fenec-sync:rwx` on it), as a file
node_exporter's own user reads (0644): written 0600 it read nothing, and an
absent series fires no alert, so `FenecTextfileUnread` says when
node_exporter cannot read a file there and `FenecArchiveSyncUnreported`
when no success has been read anywhere for an hour; `make deploy-check`
scrapes a real node_exporter for it. A node taken out of service leaves
its file, and `FenecArchiveSyncStale` would fire for it for good: remove
`fenec-archive-sync-<node>.prom` with its timer.

**The bucket keeps what the archive lets go of.** The sync is `rclone
copy`, never `sync`: it adds and replaces files and deletes none. An
archive lost with its disk, or `--keep` pruning a week back, cannot take
the bucket's copy with it. So the bucket needs a lifecycle rule of its own
(S3 and R2 both have them) expiring objects a day past the archive's
`--keep`: eight days for `--keep 7d`. A restore from the bucket to a
moment older than the archive's own oldest image needs the files `--keep`
let go of, which the bucket still holds until that rule expires them.

But a copy replaces files, and an archive's `history`, `began` and
`presence` are rewritten in place, so a copy from the wrong archive writes
over the bucket's. Three things keep it:

- **A first copy from a new disk is refused** where the bucket holds a file
  of the node's archive the disk does not (`bin/fenec-archive-sync`: it
  knows the archive it last copied from by its directory's filesystem and
  inode). A machine rebuilt or a disk replaced starts a new archive at
  once, and the timer's first run, five minutes on, would have written over
  the bucket before anyone looked; refused, `FenecArchiveSyncStale` fires
  and the bucket is as it was (`make deploy-check` holds both, and the way
  back below).
- **A bucket that keeps what is written over**: versioning, with a rule
  expiring versions no longer current after a week, or Object Lock in
  governance mode for the same week. Every version of an open segment is
  kept with it -- the cost below, again, for that week.
- **A key that may write and not delete** (S3's `s3:PutObject`,
  `s3:GetObject`, `s3:ListBucket` without `s3:DeleteObject*`; an R2 token
  scoped to the bucket, with versioning doing the rest), so a mistake on
  the machine, or someone holding its key, cannot empty the bucket.

**What the copy costs.** Each run lists the node's archive in the bucket
(`--fast-list`: a request a thousand names) and uploads, whole, every file
whose size or time changed: each tenant written since the last run uploads
its open segment again -- an hour of its writes at most, since each hourly
image starts a new one -- and an image an hour while it writes. A tenant
writing W an hour so uploads about 6.5 W an hour (twelve uploads of a
segment growing through the hour), in thirteen PUTs. A thousand tenants
all writing, the bucket keeping eight days of their hourly images and
segments: about 13 000 PUTs and 5 000 list requests an hour, some 13
million a month, which S3 and R2 bill per million (about $5 and $4.50 at
the time of writing, R2 counting the lists among them); uploads cost no
bandwidth on either, and R2 no egress for a restore. With versioning,
every upload of an open segment is also a version kept for the week:
about 6.5 W of storage an hour for each tenant writing W.

rclone refuses by default a file that grows while it reads it, and a
segment is appended to for as long as its tenant writes: the sync passes
`--local-no-check-updated`, so a busy tenant's newest segment is copied as
it stood, a cut-short segment a restore reads to its last whole record, and
copied again whole at the next run. `make deploy-check` runs real rclone
against a tenant written as fast as it takes rows, three runs, and verifies
the bucket's copy as an archive; without the flag every run failed.

**The archiver's disk replaced**, or its machine rebuilt: the archiver
has begun a new archive and the sync refuses to copy it. Put the bucket's
back in its place, and the archiver goes on in the same archive, which the
next copy takes (`make deploy-check` runs these lines):

```sh
systemctl stop fenec-archive@eu-a fenec-archive-sync@eu-a.timer
rm -rf /var/lib/fenecdb/archive/eu-a      # the new archive: its writes come again from the node
rclone copy r2:fenecdb-archive/eu/eu-a /var/lib/fenecdb/archive/eu-a --config /etc/fenecdb/rclone.conf
chown -R fenec-archive:fenec-archive /var/lib/fenecdb/archive/eu-a
chmod -R u=rwX,g=rX,o= /var/lib/fenecdb/archive/eu-a
FENEC_KEY_FILE=/etc/fenecdb/archive.key fenec verify --tenants /var/lib/fenecdb/archive/eu-a
systemctl start fenec-archive@eu-a fenec-archive-sync@eu-a.timer
```

To keep the new archive and copy it over the bucket's after all -- the
bucket versioned, or its old archive not wanted -- write what the refusal
names into `/var/lib/fenecdb/sync/eu-a/copied-from`.

```sh
# what an archive can restore
FENEC_KEY_FILE=/etc/fenecdb/archive.key fenec verify --tenants /var/lib/fenecdb/archive/eu-a
# from the bucket, on any machine
rclone copy r2:fenecdb-archive/eu/eu-a /restore/eu-a --config rclone.conf
# every tenant as it stood at a moment, a file each
FENEC_KEY_FILE=archive.key fenec restore --tenants /restore/eu-a /restore/out --to 2026-10-18T09:14:59Z
fenec /restore/out/acme.fenec -c 'get notes count'
```

**A tenant's data back from before a bad write**: restore its node's
archive to the moment before, open the tenant's file (`fenec <file>`, or
`fenec-server --file <file> --http 127.0.0.1:0` to read it over HTTP), and
write back what was lost through the router. **A node lost with its
replicas** (both machines holding its tenants gone, no failover possible):
on a new machine given the node's address, restore the node's archive into
`FENEC_DIR` before `fenec-node` first starts (`fenec restore --tenants
<archive> <dir>`, to the archive's end); what the archive had not received
is lost, and each copy that does not lead from a restored primary is sent
an image.

**The weekly restore**, from the bucket and on a machine of no role, as a
disaster would have it -- the archiver's own disk proves nothing about the
bucket:

1. Copy one node's archive out of the bucket (`rclone copy
   r2:fenecdb-archive/eu/eu-a /restore/eu-a`) and `fenec verify --tenants`
   it with the key kept apart from the archive -- which proves that copy
   of the key too.
2. Pick a tenant and a moment an hour back; ask the live tenant, through
   the router, for something written before it and nothing after -- the
   rows of a collection whose `created` field is older, a count by
   `bucket(created, 1h)` -- and note it.
3. `fenec restore --tenants /restore/eu-a /restore/out --to <moment>` and
   ask the restored file the same (`fenec /restore/out/<t>.fenec -c '...'`):
   the same answer, and the restore took as long as a real one would.
4. Remove `/restore`: it holds a tenant's rows.

`make drill` runs the same restore against writes it recorded, in Docker.
`FenecArchiverDisconnected` fires when no archiver follows a node and
`FenecArchiveSyncStale` when the bucket stopped moving; nothing exports how
far an archive is behind its node (Known gaps), so verify the restore, not
the process.

## Read replicas

A tenant can keep a read copy in another region
(`PUT /_shard/tenants/<t>/read-replicas/<region>`), which that region's
router answers marked reads from. `FenecReadReplicasGivenUp`: a copy lost
and made again three times is made again no more -- place it again
(`DELETE` and `PUT` the read replica) once the cause is fixed.
`FenecCopiesRefused` counts copies that found their primary to be another
tenant of the same name and deleted themselves: expected after a tenant is
deleted and made again.

## Rotating tokens

Each process takes one value of each token at a time, so a rotation is a
short window in which the old and the new disagree; each procedure keeps
it short and says what it costs.

- **Entry token**: start every router with the new
  `FENEC_SHARD_ENTRY_TOKEN` (a restart each, standbys first), then put it
  in the Worker (`npx wrangler secret put ENTRY_TOKEN`). In between every
  request is served, its client counted by Cloudflare's address instead
  of its own (`bad_mark` in `fenec_router_entry_clients_total`;
  `FenecEntryMarkRefused` may fire for those minutes).
- **Routers' token** (`FENEC_SHARD_TOKEN`): every router, then the
  scraper's and the app's copies. Data requests do not carry it; while
  routers disagree, a relayed client is counted by the relaying router's
  address.
- **Directory replication token**: every router in quick succession.
  While they disagree, standbys and the regions' copies of the global
  directory stop following -- forwarding goes on from what they hold, and
  `FenecReplicaDisconnected` fires if it lasts two minutes.
- **A node's admin token**: restart the node with the new
  `FENEC_ADMIN_TOKEN`, then at once record it on the region's router
  (`PUT /_shard/nodes/<n> {"addr": ..., "token": ...}`, which checks it
  against the node). Until then the router's lease renewals are refused: finish within
  the lease, or the node is failed over (and repaired as it is renewed).
- **Nodes' replication token**: every node in quick succession, then the
  archivers. While nodes disagree their replicas stop following; a write
  is then answered without its replica (a stream that misses a second is
  not waited for), so a node lost in that window can lose writes. Do it
  when nothing else is happening.
- **Data token**: one value a node, so its clients see 401 between the
  node's restart and their own update. Issue users JWTs instead
  (`--jwt-keys`): the keys file is read again as it changes, so a new key
  is added, tokens move to it, and the old key is removed with no restart.
- **Archive key**: a new key is a new archive (`fenec key` refuses to
  write over one). Point the archiver at a new directory with the new key,
  and keep the old key as long as the old archive is kept.
- **Tunnel credentials, rclone keys**: replace the file and restart the
  unit; both are read at the start.
- **The Access service token** expires -- a year from its making, by
  default -- and an expired one is a total outage that no router metric
  shows: Access answers the entry itself, an answer with no router's mark,
  so the entry takes every router for dead, answers writes 502 and then
  every region 503, and every tenant is unreachable through it. Turn on
  the dashboard's "Expiring Access Service Token" notification, which
  warns a week before, and check the expiry at every review. Before it:
  **Refresh** the token (a year more, the same ID and secret, nothing to
  change). To change its secret: **rotate** it with a grace period (an hour
  to 30 days, the old secret accepted meanwhile), then put the new secret
  in the Worker within the grace (`npx wrangler secret put
  ACCESS_CLIENT_SECRET`). To replace it: make a second token, add it to
  the Service Auth policy of every router's Access application, put both
  of its values in the Worker (`ACCESS_CLIENT_ID`, `ACCESS_CLIENT_SECRET`),
  and only then take the old one out of the policies and delete it.

## Errors and refusals

- **Every region answering 503 through the entry** while every router
  answers its own address: Access refuses the entry -- its service token
  expired or deleted, or a policy changed -- or a tunnel is down. Ask a
  router's hostname with the token (`curl -H "CF-Access-Client-Id: ..."
  -H "CF-Access-Client-Secret: ..." https://router-1.eu.example.com/t/x/`):
  an answer without `Fenec-Shard: 1` is Access's or Cloudflare's, not the
  router's (Rotating tokens).
- `FenecRouter5xx`: more than 1% of tenants' requests answered 5xx. 502 is
  a node the router could not reach (`FenecNodeUnreachable` names it), 503
  a tenant frozen for a move or a lease lapsed, 504 a replica behind the
  read's position. Look at which, and at the node.
- `FenecNodeUnreachable`: the router cannot reach a node. Before a lease
  lapses its tenants answer 502; past it the node is failed over.
- `FenecStatementErrors`: more than 5% of a node's statements end in an
  error -- clients' mistakes and the server's alike; the slow log and the
  node's log say which. A tenant whose disk refused a write takes no more
  writes until its node restarts: that is in the log as `Io`.
- `FenecStorageFailed` (a single-file server): the disk refused a write;
  writes stop until the process is restarted on a working disk.
- `FenecRouterRefusals`, `FenecNodeRefusals`: more than one refused token a
  second -- a client with a stale token, or someone guessing. The audit log
  (`/var/log/fenecdb/<role>/audit.log`) names the address; each refusal
  waits, longer for each one more from the same address.

## The entry token

- `FenecEntryMarkRefused` with `bad_mark`: the router and the Worker hold
  different entry tokens -- a rotation half done, or a client sending a
  `Fenec-Client` of its own. With `stale`: the router's clock is more than
  a minute off Cloudflare's; fix its NTP.
- `FenecEntryMarkAbsent`: tenants' requests that name no client -- the
  Worker's `ENTRY_TOKEN` unset, or a client reaching the router past the
  entry. Only meaningful on routers the entry alone reaches.

## Alerts

`alerts/fenecdb.rules.yml` for Prometheus, `alerts/grafana.yml` the same
rules for Grafana's alerting; `make deploy-check` runs promtool's check and
tests -- each rule firing and each held back -- and
`crates/fenec-shard/tests/datadog.rs` holds every metric and label value
they read to a real scrape, the jobs they name to the scrape config, and
the two files to each other. A process that is down exports nothing, so
the target-down rules read Prometheus's own `up` by job: keep the job
names of the rules' scrape config (`fenecdb-node`, `fenecdb-router`,
`fenecdb-global`, `node-exporter` on each archiver's machine with its
textfile collector).

| Alert | Severity | Means | Do |
| --- | --- | --- | --- |
| `FenecNodeDown` | critical | Prometheus cannot scrape a node | A node lost |
| `FenecPrimaryRouterDown` | critical | a router that kept its directory does not answer: its region's writes stop as their leases lapse | A router lost |
| `FenecStandbyRouterDown` | warning | a standby router, a region's or the global one's, does not answer: nothing to promote | A router lost, step 4 |
| `FenecRouterDown` | critical | a router never seen up in the hour does not answer | A router lost |
| `FenecScrapeMissing` | warning | a job has no target at all, node_exporter's among them: every rule over it is blind | Alerts |
| `FenecConnectionsNearCeiling` | warning | a process at four fifths of its `--max-connections` | Installing a machine |
| `FenecNodeFailedOver` | critical | a node was failed over and has not answered since | A node lost |
| `FenecAutoFailoverIncomplete` | critical | a node failed over still has tenants placed on it: they take no writes | A node lost, step 1 |
| `FenecAutoFailover` | warning | a node was failed over | A node lost |
| `FenecLeaseRenewalLate` | warning | a renewal to a node was late in the last five minutes (threshold for a 5 s lease); not a node failed over in the last ten | A lease late |
| `FenecTenantsWithoutReplica` | warning | tenants with no replica, a node lost away | A node back |
| `FenecSameZoneCopies` | warning | copies in their primary's zone | Adding a node |
| `FenecReadReplicasGivenUp` | warning | read replicas no longer made again | Read replicas |
| `FenecTenantsMoving` | warning | a move did not record done | Placing and moving a tenant |
| `FenecNodeUnreachable` | warning | a router cannot reach a node | Errors and refusals |
| `FenecRouter5xx` | warning | tenants' requests failing | Errors and refusals |
| `FenecEntryMarkRefused` | warning | the entry's client marks not believed | The entry token |
| `FenecEntryMarkAbsent` | info | requests naming no client | The entry token |
| `FenecRouterRefusals`, `FenecNodeRefusals` | warning | tokens refused | Errors and refusals |
| `FenecReplicaDisconnected`, `FenecReplicaBehind`, `FenecReplicaSilent` | warning | a standby router (or a file's replica) not following | A router lost |
| `FenecDirectoryUnreplicated` | warning | a region router's directory has no standby following | A router lost, step 4 |
| `FenecQuotaRefusals` | info | a tenant at its quota | Quotas |
| `FenecStatementErrors` | warning | statements failing | Errors and refusals |
| `FenecStorageFailed` | critical | a single-file server's disk refused a write | Errors and refusals |
| `FenecArchiverDisconnected` | warning | no archiver follows a node | Archives and restores |
| `FenecArchiveSyncStale` | warning | no bucket sync of a node's archive succeeded in half an hour | Archives and restores |
| `FenecTextfileUnread` | warning | node_exporter cannot read a file of its textfile collector | Archives and restores |
| `FenecArchiveSyncUnreported` | warning | no sync's success read anywhere for an hour | Archives and restores |
| `FenecCopiesRefused` | info | a copy of a tenant made again deleted itself | Read replicas |

## The drill

`make drill` builds the binaries into an image and runs two regions in
Docker -- a router keeping each region's directory and its standby, two
nodes in two zones each, an archiver a node, the global router and its
standby -- with every token in the processes' environments, as the units
hand them over. Writers put rows through the routers and record every one
answered; then the global routers are cut off the network, a node is
cut off the network and killed (`docker kill`), as a machine lost, and
brought back, a region's router is killed and
its standby promoted and switched as above, and a tenant is restored from
its archive to a moment marked earlier. Every acknowledged write is read
back from each tenant's primary and its replica; the summary prints the
times and `lost`, which must be 0. No port is fixed and everything it
made is removed; `DRILL_KEEP=1` leaves it for a look.

## Known gaps

- **No scrape-only token.** `/_metrics` takes a node's data or admin token,
  a router's `--token`.
- **No per-tenant replica lag** on a node's `/_metrics`
  (`/t/<t>/_replication/status` on the primary's node has each), and **no
  archive lag**: `fenec archive` exports nothing, and the node's
  `fenec_node_streams` says only whether an archiver is connected.
- **A late grant.** A grant whose answer the router gave up on counts as
  no grant at the router, but the node takes it whenever it reads it, and
  grants carry no number: a node stalled with one unread -- a process
  paused, a link delivering late -- can take writes for up to a lease after
  its tenants were promoted elsewhere, two primaries for that while. The
  fix is under way.
- **Requests in flight to a dead machine** wait at the router for its
  upstream timeout (10 s in the kit, The routers' upstream timeout) where
  the machine's address swallows packets, unless the client gives up
  first, even once the node has been failed over: clients should bound
  their requests and retry with an `Idempotency-Key`.
