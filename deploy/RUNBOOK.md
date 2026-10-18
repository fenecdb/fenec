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
| `systemd/fenec-archive-sync@.service`, `.timer` | that archive copied to a bucket by rclone, every five minutes |
| `systemd/cloudflared.service`, `cloudflared/config.yml` | the tunnel the global entry Worker reaches a router through |
| `systemd/fenecdb.sysusers` | the `fenec` user every unit runs as |
| `env/*.env` | the environment files the units read: addresses, flags and tokens |
| `alerts/fenecdb.rules.yml`, `alerts/grafana.yml` | alert rules, for Prometheus and for Grafana |
| `drill/drill.py` | `make drill`: the failovers below, in Docker, timed |
| `test/units.py` | `make deploy-check`: these units under a real systemd |

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
public way in is Cloudflare's edge, through the tunnels, and
`cloudflared/config.yml` lets nothing through but a tenant's path: the
tunnel's hostname is public, and the router's own `/_shard/`,
`/_replication` and `/_metrics` answer 404 there whatever token a request
carries. A tenant's paths pass, its replication stream
(`/t/<t>/_replication`) among them, behind the nodes' replication token;
the entry itself refuses that one.

Each node and router listens on its private address (`FENEC_LISTEN`), with
a token: a router or a node refuses an address past loopback without one.

## Tokens

Every token is made with `openssl rand -hex 32` and lives in an
environment file, `/etc/fenecdb/*.env`, mode 0600, owned by root. systemd
reads it as root before the process starts, so the `fenec` user cannot
read the file, no token is on a command line (`ps` shows every process's
arguments to every user), and `/proc/<pid>/environ` is readable by the
process's owner alone. `make deploy-check` holds all three. The archive's
key and the tunnel's and rclone's credentials are files handed to their
service alone with `LoadCredential=`.

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
install -d -m 0755 /etc/fenecdb
install -D -m 0644 RUNBOOK.md /usr/local/share/doc/fenecdb/RUNBOOK.md
systemctl daemon-reload
```

Then the environment file of each role the machine has, copied from
`env/`, every example value replaced, mode 0600 (`install -m 0600`). The
data go under `/var/lib/fenecdb/`, which a disk of its own mounted there
keeps apart from the system's.

Each unit runs as `fenec` with nothing writable but its own state
directory, no capability, a system call filter and a private `/tmp`
(`systemd-analyze security` puts each at 1.2). `LimitNOFILE` and
`TasksMax` follow `--max-connections` (every connection is a thread and a
descriptor): raise them together. `TimeoutStopSec` leaves a node 120 s to
sync every open tenant and checkpoint their graphs on SIGTERM; a kill past
it loses no write under `--sync always`, only the checkpoint. Never
`systemctl reload` a fenecdb unit: the servers take SIGHUP for a shutdown.

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
   routed to that router alone (`cloudflared/config.yml`).
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
   node and router (`alerts/fenecdb.rules.yml` has the scrape config).

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
region's other zone and routes it there. Nothing acknowledged under
`--sync always` is lost: a write is answered once it is on the primary's
disk and written into its replica's stream.

Measured by `make drill` (lease 2 s, three tenants a region, the node's
container killed), three runs: the killed node's tenants wrote again 2.5,
4.9 and 5.0 s after the kill, every other tenant without a gap past 21 ms
between two answered writes, and none of 42 202 acknowledged writes lost.
The lease and a tenth is the floor; the tail is the promotion waiting on
the replica's attempt to reconnect to the dead node, bounded by that
connection's 5 s timeout (Known gaps). With the default lease of 5 s, count
6 to 11 s -- for nodes recorded by address: a node recorded by a name its
resolver is slow to refuse once the machine is gone stalls the router's
leasing of the whole region (Known gaps, measured at 30 s in CI).

What to do:

1. `FenecNodeFailedOver` and `FenecAutoFailover` fire. Check the router's
   log (`failover of <node>: 200 {...}`) and `GET /_shard/tenants?region=<r>`:
   every tenant placed on a live node. `FenecAutoFailoverIncomplete` means
   a tenant was not promoted -- its replica's node did not answer -- and
   is unwritable: fix that node, then `POST /_shard/nodes/<n>/failover` on
   the region's router.
2. The failed-over tenants now have no replica (`FenecTenantsWithoutReplica`)
   until a node takes one: bring the node back, or give them replicas
   elsewhere with `POST /_shard/replicas` on the region's router.

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
runs measured the repair at 0.4 to 0.7 s after the start, the copies caught
up 20 to 50 ms later.

Check: `GET /_shard/tenants/<t>` names the node as `replica`;
`FenecNodeFailedOver` and `FenecTenantsWithoutReplica` clear. A node whose
disk was lost stays down until it has been failed over -- a lease -- and
then starts empty: the repair places replicas there from images. Started
sooner, it would hold the leases of tenants it no longer has.

## A lease late

`FenecLeaseRenewalLate`: a node took its last lease more than three fifths
of one ago, a renewal missed. The node or the network between it and
its router is slow or dropping packets; if it lasts a lease, the node is
failed over. Look at the node's load and disk, and at the path between the
two.

## A router lost

The standby keeps forwarding from its copy of the directory, so data
requests go on -- the entry and the other routers pass over a router they
cannot reach. But only the primary leases the region's nodes: unrenewed,
they stop writing as their leases lapse, and the region's tenants answer
503 until the standby is promoted. Promotion is an operator's decision,
never the routers': a standby that promoted itself whenever it could not
reach the primary would make two primaries in a partition.

1. Make sure the old router is gone (machine down, or `systemctl stop`
   reached it). A promoted router fences it at every node it reaches --
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
4. When the old machine returns, it rejoins as the new one's standby:
   `FENEC_PRIMARY` pointed at the new router, `fenec-router-standby`
   enabled, `fenec-router` disabled. Started as a primary, it would lease
   at its old term -- refused by every node the new one has reached -- and
   take `/_shard/` changes into a directory nobody else reads.

Measured by `make drill` (lease 2 s), three runs: the region's writes
stopped 1.35 to 1.58 s after its router was killed, as the leases lapsed;
once the standby was promoted -- an operator's minute in production, 2.4
to 4.9 s in the drill -- writes resumed 0.11 to 0.48 s after the promotion,
and the switch of units, a restart of the only router the region then had,
left a gap of 0.43 to 0.49 s between two answered writes.

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
   or restart the primary in place, which costs the region's forwarding
   the restart's second and no write, since the nodes' leases outlast it.
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
is in the archive once its node's stream has sent it, and in the bucket at
the next `fenec-archive-sync` run, five minutes at most.

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

**The drill**: once a week, restore one tenant to a moment and hold it to
what was written by then -- `make drill` does exactly that against a
restore point it marks. `FenecArchiverDisconnected` fires when no archiver
follows a node; nothing exports how far an archive is behind (Known gaps),
so verify the restore, not the process.

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

## Errors and refusals

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
  (`/var/log/fenecdb/*-audit.log`) names the address; each refusal waits,
  longer for each one more from the same address.

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
tests, and `crates/fenec-shard/tests/datadog.rs` holds every metric and
label value they read to a real scrape, and the two files to each other.

| Alert | Severity | Means | Do |
| --- | --- | --- | --- |
| `FenecNodeFailedOver` | critical | a node was failed over and has not answered since | A node lost |
| `FenecAutoFailoverIncomplete` | critical | a failover left a tenant unpromoted | A node lost, step 1 |
| `FenecAutoFailover` | warning | a node was failed over | A node lost |
| `FenecLeaseRenewalLate` | warning | renewals to a node are late (threshold for a 5 s lease) | A lease late |
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
| `FenecDirectoryUnreplicated` | warning | a router's directory has no standby | A router lost, step 4 |
| `FenecQuotaRefusals` | info | a tenant at its quota | Quotas |
| `FenecStatementErrors` | warning | statements failing | Errors and refusals |
| `FenecStorageFailed` | critical | a single-file server's disk refused a write | Errors and refusals |
| `FenecArchiverDisconnected` | warning | no archiver follows a node | Archives and restores |
| `FenecCopiesRefused` | info | a copy of a tenant made again deleted itself | Read replicas |

## The drill

`make drill` builds the binaries into an image and runs two regions in
Docker -- a router keeping each region's directory and its standby, two
nodes in two zones each, an archiver a node, the global router and its
standby -- with every token in the processes' environments, as the units
hand them over. Writers put rows through the routers and record every one
answered; then the global routers are cut off the network, a node is
killed (`docker kill`) and brought back, a region's router is killed and
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
- **A dead node's name stalls its region's leases.** The router renews
  every node's lease in one round and checks for lapses once every grant
  of the round has ended, and a grant looks its node's address up with no
  bound of its own. A node recorded by a name that its resolver is slow to
  refuse once the machine is gone holds the round up: the region's other
  nodes go unrenewed, stop writing as their leases lapse, and are failed
  over themselves when the round ends; every failover and repair after it
  waits on the same lookup. In CI's Docker (GitHub's runner, whose Docker
  DNS is slow to refuse a removed container's name) `make drill` measured
  a 30.2 s failover and 18.0 s without a write for the region's other
  tenants, nothing acknowledged lost; locally, with the routers' unknown
  names sent to a resolver that never answers, 21 s and 8 s, and the
  router's log failing the healthy node over. Until the router bounds the
  lookup, record nodes by address, as every example here does.
- **A failover's tail.** Promoting a tenant waits for its replica's
  follower to stop, and a follower in the middle of reconnecting to the
  dead node stops only when that attempt fails -- up to its 5 s connect
  timeout where the dead machine's address swallows packets. Measured in
  `make drill`: 2.2 and 2.5 s after the kill where the reconnect failed at
  once, 4.9 and 5.0 s where it waited.
- **Requests in flight to a dead machine** can wait at the router for its
  `--upstream-timeout` (60 s) where the machine's address swallows packets,
  unless the client gives up first: clients should bound their requests and
  retry with an `Idempotency-Key`.
