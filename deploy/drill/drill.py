#!/usr/bin/env python3
"""The failover drill: two regions of fenecdb in Docker, broken on purpose.

    make drill                       # builds the kit image, then runs this
    python3 deploy/drill/drill.py    # the image built already (DRILL_IMAGE)

What runs, each process a container on one Docker network, as the units in
deploy/systemd/ run them on machines -- the same flags, every token in the
environment and none on a command line:

    g1, g2              the global router and its standby
    us-r1, us-r2        us's router, keeping its own directory, and its standby
    eu-r1, eu-r2        eu's, the same
    us-a, us-b          us's nodes, zones us-1 and us-2 (--dir --lease --sync always)
    eu-a, eu-b          eu's nodes, zones eu-1 and eu-2
    arch-<node>         a node-wide archiver for each node (fenec archive --tenants)

The region routers lease their own nodes (--auto-failover) and give every
tenant a replica in the other zone (--replicas). Tenants are made in both
regions through the global router, and a writer thread a tenant puts rows by
id -- 1, 2, 3, ... -- through its region's routers, the next one only once
the one before was answered, so what a tenant acknowledged is exactly 1..K.
Then, in turn:

    restore point   the writers paused, a moment noted, the writers resumed
    partition       the global routers cut off the network: both regions write on
    node kill       the us node holding the most primaries cut off the network
                    and then killed (`docker kill`), as a machine lost: no FIN
                    or reset reaches its peers, and its replicas keep what its
                    sockets had sent; the router's failover timed off its log
                    (Docker's clock), its tenants' writes again on their
                    replicas, and the tenants whose replica it held and the
                    rest timed apart
    node back       it starts again: the router's repair has its stale copies
                    follow the new primaries, which they catch up with
    router kill     `docker kill` of eu's router: its standby forwards until the
                    leases lapse, is promoted as an operator promotes it --
                    the API call, then its unit switched to the primary's --
                    and leases the nodes; the old router comes back as its
                    standby
    verify          every acknowledged write read back from each tenant's primary
                    and from its replica; one tenant restored from the archive to
                    the restore point, holding exactly what was acknowledged by then

No port is fixed: the routers and nodes publish on ports Docker picks, on
the host's loopback, and this driver reads them back. Every container,
volume and the network carry the run's label and are removed at the end,
however it ends. Exit status 0 when nothing acknowledged was lost and every
check held, 1 otherwise.

Knobs (environment): DRILL_IMAGE (fenec-kit), DRILL_LEASE seconds (2),
DRILL_TENANTS a region (3), DRILL_RATE writes a second a tenant (100),
DRILL_KEEP=1 to leave everything running for a look, DRILL_JSON=<path> to
write the results there too, DRILL_LOG_LINES of each router's and node's
log printed when it fails (300).

The standard library alone, so a machine with Docker and python3 runs it.
"""

import http.client
import json
import os
import secrets
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone

IMAGE = os.environ.get("DRILL_IMAGE", "fenec-kit")
LEASE = int(os.environ.get("DRILL_LEASE", "2"))
PER_REGION = int(os.environ.get("DRILL_TENANTS", "3"))
RATE = float(os.environ.get("DRILL_RATE", "100"))
KEEP = os.environ.get("DRILL_KEEP") == "1"

RUN = "fdrill-" + secrets.token_hex(3)
LABEL = f"fenec-drill={RUN}"
NET = RUN

# Every token is the run's own, and each a different one: the router
# refuses an entry token equal to any token it holds for something else.
ROOT = secrets.token_hex(16)      # the routers' --token: /_shard/, relay marks
DIRS = secrets.token_hex(16)      # the routers' directory replication
ENTRY = secrets.token_hex(16)     # the global entry's ENTRY_TOKEN
DATA = secrets.token_hex(16)      # the nodes' --http-token: the tenants' data
REPL = secrets.token_hex(16)      # the nodes' --replication-token, one for all
ADMIN = {n: secrets.token_hex(16) for n in ["us-a", "us-b", "eu-a", "eu-b"]}

REGIONS = {
    "us": {"routers": ["us-r1", "us-r2"], "nodes": [("us-a", "us-1"), ("us-b", "us-2")]},
    "eu": {"routers": ["eu-r1", "eu-r2"], "nodes": [("eu-a", "eu-1"), ("eu-b", "eu-2")]},
}

T0 = time.monotonic()


def say(msg):
    print(f"[{time.monotonic() - T0:7.2f}s] {msg}", flush=True)


class Failed(Exception):
    pass


def check(ok, msg):
    if not ok:
        raise Failed(msg)


# ------------------------------------------------------------------ docker


def docker(*args, env=None, check_=True, timeout=120):
    p = subprocess.run(
        ["docker", *args],
        capture_output=True,
        text=True,
        env=None if env is None else {**os.environ, **env},
        timeout=timeout,
    )
    if check_ and p.returncode != 0:
        raise Failed(f"docker {' '.join(args[:3])} ...: {p.stderr.strip() or p.stdout.strip()}")
    return p.stdout.strip()


def cname(name):
    return f"{RUN}-{name}"


def volume(name):
    v = f"{RUN}-{name}"
    docker("volume", "create", "--label", LABEL, v)
    return v


def start(name, cmd, env, port=None, mounts=(), data=True):
    """A container on the drill's network, reachable there as `name`; the
    tokens go in its environment by name (`-e NAME`, the value from this
    process's), so they are on no command line here or in it."""
    args = ["run", "-d", "--name", cname(name), "--label", LABEL, "--network", NET,
            "--network-alias", name, "--hostname", name]
    if data:
        args += ["-v", f"{volume(name)}:/data"]
    for m in mounts:
        args += ["-v", m]
    for k in env:
        args += ["-e", k]
    if port:
        args += ["-p", f"127.0.0.1::{port}"]
    docker(*args, IMAGE, *cmd, env=env)


def recreate(name, cmd, env, port):
    """The container again with another command, on the same volume: a
    machine whose unit was changed and restarted."""
    docker("rm", "-f", cname(name))
    args = ["run", "-d", "--name", cname(name), "--label", LABEL, "--network", NET,
            "--network-alias", name, "--hostname", name, "-v", f"{RUN}-{name}:/data",
            "-p", f"127.0.0.1::{port}"]
    for k in env:
        args += ["-e", k]
    docker(*args, IMAGE, *cmd, env=env)


ADDRS = {}


def addr(name, port):
    """Where the host reaches `name`'s published port, read back from Docker
    as the container (re)starts: a port it picked."""
    key = (name, port)
    if key not in ADDRS:
        out = docker("port", cname(name), f"{port}/tcp").splitlines()[0]
        ADDRS[key] = out.replace("0.0.0.0", "127.0.0.1")
    return ADDRS[key]


def forget(name):
    for k in [k for k in ADDRS if k[0] == name]:
        del ADDRS[k]


def listening(name, within=60):
    end = time.monotonic() + within
    while time.monotonic() < end:
        p = subprocess.run(["docker", "logs", cname(name)], capture_output=True, text=True)
        if "listening on: http://" in p.stderr + p.stdout:
            return
        state = docker("inspect", "-f", "{{.State.Running}}", cname(name), check_=False)
        if state == "false":
            raise Failed(f"{name} ended before it listened:\n{p.stderr[-2000:]}")
        time.sleep(0.1)
    raise Failed(f"{name} did not listen within {within} s")


def cleanup():
    ids = docker("ps", "-aq", "--filter", f"label={LABEL}", check_=False).split()
    if ids:
        docker("rm", "-f", *ids, check_=False)
    vols = docker("volume", "ls", "-q", "--filter", f"label={LABEL}", check_=False).split()
    if vols:
        docker("volume", "rm", "-f", *vols, check_=False)
    docker("network", "rm", NET, check_=False)


# -------------------------------------------------------------------- http


def call(where, method, path, body=None, token=None, timeout=10, headers=None):
    host, port = where.rsplit(":", 1)
    c = http.client.HTTPConnection(host, int(port), timeout=timeout)
    try:
        h = dict(headers or {})
        if token:
            h["Authorization"] = f"Bearer {token}"
        data = None
        if body is not None:
            data = body if isinstance(body, (bytes, str)) else json.dumps(body)
            h["Content-Type"] = "application/json"
        c.request(method, path, body=data, headers=h)
        r = c.getresponse()
        return r.status, r.read().decode("utf-8", "replace")
    finally:
        c.close()


def shard(router, method, path, body=None):
    return call(addr(router, 8090), method, path, body, token=ROOT)


def query(where, tenant, sql, params=None, timeout=10):
    body = {"query": sql} if params is None else {"query": sql, "params": params}
    return call(where, "POST", f"/t/{tenant}/query", body, token=DATA, timeout=timeout)


def until(ok, what, within=60.0, every=0.1):
    end = time.monotonic() + within
    last = None
    while time.monotonic() < end:
        try:
            got = ok()
        except (OSError, http.client.HTTPException, Failed) as e:
            got, last = None, e
        if got:
            return got
        time.sleep(every)
    raise Failed(f"waited {within} s for {what}" + (f" (last: {last})" if last else ""))


# ------------------------------------------------------------------ writer


class Writer(threading.Thread):
    """Puts rows 1, 2, 3, ... into one tenant through its region's routers,
    the next only once the one before was answered 2xx: a put by id, so a
    write sent again after an answer that never came writes the same row.
    A router that cannot be reached is passed over for the next, as the
    global entry passes one over; any other refusal is waited out and the
    same row sent again."""

    def __init__(self, tenant, routers):
        super().__init__(daemon=True)
        self.tenant = tenant
        self.routers = routers
        self.at = 0
        self.lock = threading.Lock()
        self.acked = []          # (row, monotonic time it was answered)
        self.refusals = []       # (monotonic time, what)
        self.stop = threading.Event()
        self.pause = threading.Event()
        self.idle = threading.Event()
        self.conn = None

    def connect(self):
        host, port = addr(self.routers[self.at], 8090).rsplit(":", 1)
        self.conn = http.client.HTTPConnection(host, int(port), timeout=5)

    def put(self, k):
        if self.conn is None:
            try:
                self.connect()
            except (Failed, OSError, subprocess.SubprocessError) as e:
                # Its container being made again: no port to ask yet.
                self.at = (self.at + 1) % len(self.routers)
                return False, f"no address: {e}"[:120]
        body = json.dumps({"query": "put events {id: $1, n: $1}", "params": [k]})
        try:
            self.conn.request("POST", f"/t/{self.tenant}/query", body=body, headers={
                "Authorization": f"Bearer {DATA}", "Content-Type": "application/json"})
            r = self.conn.getresponse()
            text = r.read().decode("utf-8", "replace")
            if r.getheader("Connection", "").lower() == "close":
                self.conn.close()
                self.conn = None
            if 200 <= r.status < 300:
                return True, None
            return False, f"{r.status} {text[:120]}"
        except (OSError, http.client.HTTPException) as e:
            # Not reached, or no answer: the next router, as the entry goes
            # on to one -- the row is sent again either way.
            self.conn = None
            refused = isinstance(e, ConnectionRefusedError)
            self.at = (self.at + 1) % len(self.routers)
            return False, f"{type(e).__name__}{' (refused)' if refused else ''}"

    def run(self):
        k = 1
        gap = 1.0 / RATE if RATE > 0 else 0
        while not self.stop.is_set():
            if self.pause.is_set():
                self.idle.set()
                while self.pause.is_set() and not self.stop.is_set():
                    time.sleep(0.01)
                self.idle.clear()
                continue
            began = time.monotonic()
            ok, why = self.put(k)
            now = time.monotonic()
            with self.lock:
                if ok:
                    self.acked.append((k, now))
                else:
                    self.refusals.append((now, why))
            if ok:
                k += 1
                rest = gap - (now - began)
                if rest > 0:
                    time.sleep(rest)
            else:
                time.sleep(0.02)

    def snapshot(self):
        with self.lock:
            return list(self.acked), list(self.refusals)

    def last(self):
        with self.lock:
            return self.acked[-1][0] if self.acked else 0


def pause_all(writers):
    for w in writers:
        w.pause.set()
    until(lambda: all(w.idle.is_set() for w in writers), "every writer to pause", 30)


def resume_all(writers):
    for w in writers:
        w.pause.clear()


def first_ack_after(w, t):
    acked, _ = w.snapshot()
    for k, at in acked:
        if at > t:
            return at
    return None


def last_ack_before(w, t):
    acked, _ = w.snapshot()
    best = None
    for k, at in acked:
        if at <= t:
            best = at
    return best


def longest_gap(w, start, end):
    acked, _ = w.snapshot()
    times = [at for _, at in acked if start <= at <= end]
    if not times:
        return end - start
    gaps = [b - a for a, b in zip([start] + times, times + [end])]
    return max(gaps)


# -------------------------------------------------------------- the world


def node_cmd():
    # As deploy/systemd/fenec-node.service runs it, with fewer connections
    # than its 4000: a container's threads are the Docker VM's.
    return ["fenec-server", "--dir", "/data", "--http", "0.0.0.0:8081", "--sync", "always",
            "--lease", "--max-connections", "200"]


def node_env(n):
    return {"FENEC_HTTP_TOKEN": DATA, "FENEC_ADMIN_TOKEN": ADMIN[n], "FENEC_REPLICATION_TOKEN": REPL}


ROUTER_ENV = {"FENEC_SHARD_TOKEN": ROOT, "FENEC_SHARD_REPLICATION_TOKEN": DIRS,
              "FENEC_SHARD_ENTRY_TOKEN": ENTRY}
GLOBALS = "http://g1:8090,http://g2:8090"


# As deploy/env/router.env sets it, scaled to the drill's lease: what the
# router had out to the node killed is answered at its failover whatever
# this is, and this bounds what no failover ends.
UPSTREAM = 2 * LEASE


def global_cmd(standby_of=None):
    # As deploy/systemd/fenec-global(.standby).service runs it.
    cmd = ["fenec-shard", "--listen", "0.0.0.0:8090", "--directory", "/data/directory.fenec",
           "--upstream-timeout", str(UPSTREAM)]
    return cmd + (["--replica-of", standby_of] if standby_of else [])


def region_cmd(region, standby_of=None):
    # As deploy/systemd/fenec-router(.standby).service runs it: a standby
    # carries the primary's flags, so that once promoted it leases and fails
    # over as the primary did.
    cmd = ["fenec-shard", "--listen", "0.0.0.0:8090", "--directory", "/data/directory.fenec",
           "--region", region, "--global", GLOBALS, "--replicas",
           "--auto-failover", str(LEASE), "--upstream-timeout", str(UPSTREAM)]
    return cmd + (["--replica-of", standby_of] if standby_of else [])


def archive_cmd(n):
    return ["fenec", "archive", "--tenants", f"http://{n}:8081", f"/archive/{n}", "--image-every", "1h"]


def build_world():
    docker("network", "create", "--label", LABEL, NET)
    volume("archive")
    say(f"run {RUN}: image {IMAGE}, lease {LEASE} s, {PER_REGION} tenants a region, {RATE:g} writes/s each")

    start("g1", global_cmd(), ROUTER_ENV, port=8090)
    listening("g1")
    start("g2", global_cmd("http://g1:8090"), ROUTER_ENV, port=8090)
    for region, r in REGIONS.items():
        primary, standby = r["routers"]
        start(primary, region_cmd(region), ROUTER_ENV, port=8090)
        listening(primary)
        start(standby, region_cmd(region, f"http://{primary}:8090"), ROUTER_ENV, port=8090)
        for n, _ in r["nodes"]:
            start(n, node_cmd(), node_env(n), port=8081)
    for name in ["g2", "us-r2", "eu-r2", "us-a", "us-b", "eu-a", "eu-b"]:
        listening(name)
    say("12 processes up: 2 global routers, 4 region routers, 4 nodes (archivers next)")

    for region, r in REGIONS.items():
        routers = [f"http://{x}:8090" for x in r["routers"]]
        s, b = shard("g1", "PUT", f"/_shard/regions/{region}", {"routers": routers})
        check(s == 200, f"region {region}: {s} {b}")
    for router in ["us-r1", "us-r2", "eu-r1", "eu-r2"]:
        until(lambda: '"name":"us"' in shard(router, "GET", "/_shard/regions")[1]
              and '"name":"eu"' in shard(router, "GET", "/_shard/regions")[1],
              f"{router}'s copy of the global directory")
    for region, r in REGIONS.items():
        for n, zone in r["nodes"]:
            body = {"addr": f"{n}:8081", "token": ADMIN[n], "region": region, "zone": zone}
            s, b = shard("g1", "PUT", f"/_shard/nodes/{n}", body)
            check(s in (200, 201), f"node {n}: {s} {b}")

    tenants = []
    for region in REGIONS:
        for i in range(1, PER_REGION + 1):
            t = f"{region}-t{i}"
            s, b = shard("g1", "PUT", f"/_shard/tenants/{t}",
                         {"region": region, "schema": "create collection events (n int)"})
            check(s == 201, f"tenant {t}: {s} {b}")
            info = json.loads(b)
            check(info.get("replica_on"), f"tenant {t} was given no replica: {b}")
            tenants.append((t, region))
    for t, region in tenants:
        for router in REGIONS[region]["routers"]:
            until(lambda: query(addr(router, 8090), t, "get events count")[0] == 200,
                  f"{t} through {router}")
    for n in ["us-a", "us-b", "eu-a", "eu-b"]:
        start(f"arch-{n}", archive_cmd(n), {"FENEC_REPLICATION_TOKEN": REPL},
              mounts=[f"{RUN}-archive:/archive"], data=False)
    say(f"{len(tenants)} tenants made through the global router, each with a replica in the other zone; 4 archivers following")
    return tenants


def placement(region, t, router=None):
    router = router or REGIONS[region]["routers"][0]
    s, b = shard(router, "GET", f"/_shard/tenants/{t}")
    check(s == 200, f"{t} on {router}: {s} {b}")
    return json.loads(b)


# -------------------------------------------------------------- the phases


def restore_point(writers, results):
    """Writers paused, so the acknowledged writes are exactly those before
    the moment noted and nothing is in flight; the moment is a second past
    the last answer and a second before the next request, the clocks of
    this machine and Docker's not trusted closer than that."""
    pause_all(writers)
    time.sleep(1.5)
    moment = datetime.now(timezone.utc)
    acked = {w.tenant: w.last() for w in writers}
    time.sleep(1.5)
    resume_all(writers)
    stamp = moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"
    results["restore_point"] = {"at": stamp, "acked": acked}
    say(f"restore point {stamp}: {sum(acked.values())} writes acknowledged by then")


def partition(writers, results):
    """Both global routers off the network for three leases: each region's
    router leases its own nodes, and its tenants go on writing."""
    began = time.monotonic()
    for g in ["g1", "g2"]:
        docker("network", "disconnect", NET, cname(g))
    time.sleep(3 * LEASE)
    for g in ["g1", "g2"]:
        docker("network", "connect", "--alias", g, NET, cname(g))
    ended = time.monotonic()
    forget("g1")
    forget("g2")
    gaps = {w.tenant: longest_gap(w, began, ended) for w in writers}
    worst = max(gaps.values())
    results["partition"] = {"seconds": round(ended - began, 2),
                            "longest_write_gap_ms": round(worst * 1000, 1)}
    say(f"partition: the global routers cut off {ended - began:.1f} s; longest gap between "
        f"acknowledged writes of any tenant {worst * 1000:.0f} ms")
    # The property, not a time: every tenant wrote past every lease granted
    # before the cut, so its leases came from its region's router alone.
    past = began + 1.5 * LEASE
    silent = [w.tenant for w in writers if not first_ack_after(w, past) or first_ack_after(w, past) > ended]
    check(not silent, f"{silent} took no write past the leases granted before the global routers were cut off")


def kill_node(writers, tenants, results):
    region = "us"
    by_node = {}
    for t, r in tenants:
        if r == region:
            by_node.setdefault(placement(region, t)["node"], []).append(t)
    victim, primaries = max(by_node.items(), key=lambda kv: len(kv[1]))
    # The tenants whose replica the victim holds: their primaries' durable
    # writes wait for a stream that will never answer again, a second at
    # most (SENT_WAIT), before it is not waited for.
    replicated = [t for t, r in tenants if r == region and t not in primaries
                  and placement(region, t).get("replica") == victim]
    say(f"node kill: {victim} cut off the network, then docker kill; primary of {', '.join(primaries)}"
        + (f", replica of {', '.join(replicated)}" if replicated else ""))
    called = time.monotonic()
    # A machine lost, not a process: cut off first, its sockets send no FIN
    # or reset as the process dies, and whatever its send buffers held never
    # leaves. Killed alone, the kernel delivered what they held.
    docker("network", "disconnect", NET, cname(victim))
    docker("kill", cname(victim))
    killed = time.monotonic()
    affected = [w for w in writers if w.tenant in primaries]
    until(lambda: all(first_ack_after(w, killed) for w in affected),
          f"{victim}'s tenants to take writes on their replicas", within=30 + 5 * LEASE)
    times = {}
    for w in affected:
        first = first_ack_after(w, killed)
        before = last_ack_before(w, called)
        times[w.tenant] = {"after_kill_ms": round((first - killed) * 1000),
                           "blocked_ms": round((first - before) * 1000) if before else None}
    for t in primaries:
        p = placement(region, t)
        check(p["node"] != victim, f"{t} still placed on {victim}: {p}")
    now = time.monotonic()
    replica_gap = max((longest_gap(w, called, now) for w in writers if w.tenant in replicated), default=0)
    others = [w for w in writers if w.tenant not in primaries and w.tenant not in replicated]
    other_gap = max(longest_gap(w, called, now) for w in others)
    failed_over = failed_over_after(REGIONS[region]["routers"][0], victim)
    results["node_kill"] = {"node": victim, "tenants": times, "lease_s": LEASE,
                            "failed_over_after_kill_ms": failed_over,
                            "replica_tenants": replicated,
                            "replica_tenants_longest_gap_ms": round(replica_gap * 1000),
                            "other_tenants_longest_gap_ms": round(other_gap * 1000)}
    say(f"  the router failed {victim} over {failed_over} ms after the kill" if failed_over is not None
        else f"  the router's log names no failover of {victim}")
    for t, v in times.items():
        say(f"  {t}: writes again {v['after_kill_ms']} ms after the kill "
            f"(no write answered for {v['blocked_ms']} ms)")
    if replicated:
        say(f"  tenants whose replica it held: longest gap {replica_gap * 1000:.0f} ms")
    say(f"  every other tenant: longest gap {other_gap * 1000:.0f} ms")
    return victim, primaries


def docker_time(stamp):
    """A Docker timestamp (RFC 3339, nanoseconds) as seconds since 1970."""
    head, _, frac = stamp.strip().rstrip("Z").partition(".")
    return datetime.fromisoformat(f"{head}.{(frac or '0')[:6]}").replace(tzinfo=timezone.utc).timestamp()


def failed_over_after(router, victim):
    """Milliseconds from the victim's end to the router's log saying it
    failed the victim over, both by the Docker VM's clock -- the writers'
    figures carry the request each had in flight to the dead node, which
    the failover answers, and the write each sends after it. None where the
    log names no failover."""
    ended = docker_time(docker("inspect", "-f", "{{.State.FinishedAt}}", cname(victim)))
    r = subprocess.run(["docker", "logs", "--timestamps", cname(router)],
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60)
    after = [docker_time(line.split(" ", 1)[0]) - ended for line in r.stdout.splitlines()
             if f"failover of `{victim}`: 200" in line]
    after = [t for t in after if t > 0]
    return round(min(after) * 1000) if after else None


def node_back(victim, primaries, writers, results):
    began = time.monotonic()
    docker("network", "connect", "--alias", victim, NET, cname(victim))
    docker("start", cname(victim))
    forget(victim)
    listening(victim)
    region = "us"
    # The router's repair, as the node answers its lease again: each of the
    # tenants it lost follows its new primary from this node.
    until(lambda: all(placement(region, t).get("replica") == victim for t in primaries),
          f"the repair to make {victim} the replica of {primaries}", within=60)
    repaired = time.monotonic()
    # Its copies catch up: what the primary acknowledged is on the copy.
    for t in primaries:
        w = next(w for w in writers if w.tenant == t)
        k = w.last()
        until(lambda: count_upto(addr(victim, 8081), t, k) == k,
              f"{victim}'s copy of {t} to hold row {k}", within=60)
    caught = time.monotonic()
    results["node_back"] = {"repaired_ms": round((repaired - began) * 1000),
                            "caught_up_ms": round((caught - began) * 1000)}
    say(f"node back: {victim} started again; replica of its lost tenants after "
        f"{(repaired - began) * 1000:.0f} ms, its copies caught up after {(caught - began) * 1000:.0f} ms")


def kill_router(writers, results):
    region = "eu"
    primary, standby = REGIONS[region]["routers"]
    mine = [w for w in writers if w.tenant.startswith(region + "-")]
    say(f"router kill: docker kill {primary}, which keeps {region}'s directory and leases its nodes")
    docker("kill", cname(primary))
    killed = time.monotonic()
    # The standby forwards from its copy of the directory, the nodes writing
    # until their leases lapse with nobody to renew them.
    time.sleep(0.5)
    def stopped():
        # Each writer refused for a while: the nodes' leases have lapsed.
        now = time.monotonic()
        return all(now - (w.snapshot()[0][-1][1]) > 1.0 for w in mine)
    until(stopped, "the leases to lapse without a router to renew them", within=10 + 3 * LEASE)
    lapsed = max(w.snapshot()[0][-1][1] for w in mine)
    # The operator promotes the standby (deploy/RUNBOOK.md, "A router lost").
    s, b = call(addr(standby, 8090), "POST", "/_replication/promote", token=DIRS)
    check(s == 200, f"promoting {standby}: {s} {b}")
    promoted = time.monotonic()
    s, b = shard("g1", "PUT", f"/_shard/regions/{region}",
                 {"routers": [f"http://{standby}:8090", f"http://{primary}:8090"]})
    check(s == 200, f"region {region} with {standby} first: {s} {b}")
    until(lambda: all(first_ack_after(w, promoted) for w in mine),
          f"{region}'s tenants to write again under {standby}'s leases", within=30 + 3 * LEASE)
    back = {w.tenant: round((first_ack_after(w, promoted) - promoted) * 1000) for w in mine}
    # The operator switches the promoted machine's unit at once
    # (systemctl start fenec-router): the same file, opened as the
    # primary's, where the standby's unit started again would have it follow
    # the dead router. A restart of the one router the region has.
    switched = time.monotonic()
    recreate(standby, region_cmd(region), ROUTER_ENV, 8090)
    forget(standby)
    listening(standby)
    ready = time.monotonic()
    until(lambda: all(first_ack_after(w, ready) for w in mine),
          f"{region}'s tenants to write through {standby} restarted as the primary", within=30)
    switch_gap = max(longest_gap(w, switched, time.monotonic()) for w in mine)
    # The old router comes back as the new one's standby, its unit changed
    # (deploy/RUNBOOK.md): pointed anywhere else its file would be refused.
    recreate(primary, region_cmd(region, f"http://{standby}:8090"), ROUTER_ENV, 8090)
    forget(primary)
    listening(primary)
    until(lambda: "fenec_router_following 1" in shard(primary, "GET", "/_metrics")[1],
          f"{primary} to follow {standby}")
    for w in mine:
        until(lambda: query(addr(primary, 8090), w.tenant, "get events count")[0] == 200,
              f"{w.tenant} through {primary}, now a standby")
    results["router_kill"] = {
        "router": primary,
        "writes_stopped_after_kill_ms": round((lapsed - killed) * 1000),
        "promoted_after_kill_ms": round((promoted - killed) * 1000),
        "writes_again_after_promotion_ms": back,
        "unit_switch_longest_gap_ms": round(switch_gap * 1000),
    }
    say(f"  the standby forwarded until the leases lapsed, {(lapsed - killed) * 1000:.0f} ms after the kill")
    say(f"  promoted {(promoted - killed) * 1000:.0f} ms after the kill; writes again "
        + ", ".join(f"{t} {ms} ms" for t, ms in back.items()) + " after the promotion")
    say(f"  {standby} restarted under the primary's unit: longest gap {switch_gap * 1000:.0f} ms; "
        f"{primary} back as its standby, forwarding")


def count_upto(where, t, k):
    s, b = query(where, t, "get events select count() as c where id <= $1", [k])
    if s != 200:
        raise Failed(f"count of {t} at {where}: {s} {b}")
    rows = json.loads(b)
    rows = rows["rows"] if isinstance(rows, dict) else rows
    return rows[0]["c"]


def verify(writers, tenants, results):
    lost_total = 0
    acked_total = 0
    per = {}
    for t, region in tenants:
        w = next(w for w in writers if w.tenant == t)
        acked, refusals = w.snapshot()
        k = acked[-1][0] if acked else 0
        rows = [x for x, _ in acked]
        check(rows == list(range(1, k + 1)), f"{t}: the writer's own record is not 1..{k}")
        p = placement(region, t)
        found = count_upto(addr(REGIONS[region]["routers"][0], 8090), t, k)
        # The replica holds them too, once it has caught up.
        copy = p.get("replica")
        copy_found = until(lambda: (lambda c: c if c == k else None)(count_upto(addr(copy, 8081), t, k)),
                           f"{copy}'s copy of {t} to catch up", within=30) if copy else None
        lost = k - found
        lost_total += lost
        acked_total += k
        per[t] = {"acked": k, "found": found, "lost": lost, "primary": p["node"],
                  "replica": copy, "replica_found": copy_found, "refused": len(refusals)}
    results["verify"] = {"acked": acked_total, "found": acked_total - lost_total,
                         "lost": lost_total, "tenants": per}
    say(f"verify: {acked_total} writes acknowledged, {acked_total - lost_total} found on the "
        f"primaries, lost {lost_total}; every replica holds them too")
    check(lost_total == 0, f"{lost_total} acknowledged writes lost: {per}")


def restore(results):
    """One eu tenant from eu-a's archive, to the restore point: exactly the
    rows acknowledged by then. eu-a holds every eu tenant's primary or
    replica, and an archive of a replica is the primary's writes."""
    point = results["restore_point"]
    t = "eu-t1"
    k = point["acked"][t]
    out = docker("run", "--rm", "--label", LABEL, "-v", f"{RUN}-archive:/archive", IMAGE, "sh", "-c",
                 f"fenec verify --tenants /archive/eu-a >/dev/null && "
                 f"fenec restore --tenants /archive/eu-a /tmp/restored --to {point['at']} >/dev/null && "
                 f"fenec /tmp/restored/{t}.fenec -c 'get events select count() as c, min(id) as lo, max(id) as hi'",
                 timeout=300)
    # The shell's table: a header, a rule, then `c lo hi`.
    line = [l for l in out.splitlines() if l.strip() and l.split()[0].isdigit()][0]
    c, lo, hi = (int(x) for x in line.split()[:3])
    results["restore"] = {"tenant": t, "to": point["at"], "acked_by_then": k,
                          "restored": c, "min": lo, "max": hi}
    say(f"restore: {t} from eu-a's archive to {point['at']}: {c} rows, ids {lo}..{hi}, "
        f"against {k} acknowledged by then")
    check(c == k and lo == 1 and hi == k, f"the restore of {t} holds {c} rows {lo}..{hi}, not 1..{k}")


# -------------------------------------------------------------------- main


def main():
    results = {"run": RUN, "lease_s": LEASE, "tenants_per_region": PER_REGION, "rate": RATE}
    writers = []
    ok = False
    try:
        check(docker("image", "inspect", IMAGE, check_=False) != "", f"no image {IMAGE}: make drill builds it")
        tenants = build_world()
        writers = [Writer(t, REGIONS[r]["routers"]) for t, r in tenants]
        for w in writers:
            w.start()
        time.sleep(3)
        say(f"writing: {sum(w.last() for w in writers)} writes acknowledged in the first 3 s")
        # The archivers take each tenant's first image as it starts; the
        # restore point comes after that, with writes before and after it.
        time.sleep(2)
        restore_point(writers, results)
        time.sleep(2)
        partition(writers, results)
        time.sleep(2)
        victim, primaries = kill_node(writers, tenants, results)
        time.sleep(2)
        node_back(victim, primaries, writers, results)
        time.sleep(2)
        kill_router(writers, results)
        time.sleep(2)
        for w in writers:
            w.stop.set()
        for w in writers:
            w.join(timeout=10)
        verify(writers, tenants, results)
        restore(results)
        ok = True
    except Failed as e:
        say(f"FAILED: {e}")
        results["failed"] = str(e)
    except KeyboardInterrupt:
        say("interrupted")
        results["failed"] = "interrupted"
    finally:
        for w in writers:
            w.stop.set()
        if not ok and results.get("failed") != "interrupted":
            logs()
        if KEEP:
            say(f"DRILL_KEEP=1: left running; remove with: docker rm -f $(docker ps -aq --filter label={LABEL}); "
                f"docker volume rm $(docker volume ls -q --filter label={LABEL}); docker network rm {NET}")
        else:
            cleanup()
            left = docker("ps", "-aq", "--filter", f"label={LABEL}", check_=False)
            left += docker("volume", "ls", "-q", "--filter", f"label={LABEL}", check_=False)
            say("cleaned up: every container, volume and the network removed" if not left
                else f"cleanup left: {left}")
    summary(results, ok)
    if os.environ.get("DRILL_JSON"):
        with open(os.environ["DRILL_JSON"], "w") as f:
            json.dump(results, f, indent=2)
    return 0 if ok else 1


def logs():
    """Every router's and node's log, the last DRILL_LOG_LINES lines of
    each (300), for a failure read in CI where nothing is left to look at:
    the containers go with the cleanup, a killed one's log with them."""
    lines = os.environ.get("DRILL_LOG_LINES", "300")
    names = ["g1", "g2"] + [x for r in REGIONS.values() for x in r["routers"]]
    names += [n for r in REGIONS.values() for n, _ in r["nodes"]]
    for name in names:
        print(f"\n---- {name}: the last {lines} lines of its log ----", flush=True)
        r = subprocess.run(["docker", "logs", "--timestamps", "--tail", lines, cname(name)],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60)
        print(r.stdout.rstrip() or "(nothing)", flush=True)
    print("---- end of the logs ----", flush=True)


def summary(results, ok):
    print()
    print(f"== drill {results['run']}: {'PASSED' if ok else 'FAILED'} ==")
    if "partition" in results:
        p = results["partition"]
        print(f"global routers cut off {p['seconds']} s     longest write gap {p['longest_write_gap_ms']} ms")
    if "node_kill" in results:
        n = results["node_kill"]
        ms = [v["after_kill_ms"] for v in n["tenants"].values()]
        print(f"node {n['node']} cut off and killed (lease {n['lease_s']} s)   failed over after "
              f"{n['failed_over_after_kill_ms']} ms, its tenants wrote again after "
              f"{min(ms)}-{max(ms)} ms; tenants it held the replica of, longest gap "
              f"{n['replica_tenants_longest_gap_ms']} ms; others {n['other_tenants_longest_gap_ms']} ms")
    if "node_back" in results:
        b = results["node_back"]
        print(f"node back                      repaired {b['repaired_ms']} ms, caught up {b['caught_up_ms']} ms")
    if "router_kill" in results:
        r = results["router_kill"]
        ms = list(r["writes_again_after_promotion_ms"].values())
        print(f"router {r['router']} killed            writes stopped {r['writes_stopped_after_kill_ms']} ms "
              f"after, again {min(ms)}-{max(ms)} ms after the promotion")
    if "verify" in results:
        v = results["verify"]
        print(f"writes acknowledged {v['acked']}   found {v['found']}   lost {v['lost']}")
    if "restore" in results:
        r = results["restore"]
        print(f"restore of {r['tenant']} to {r['to']}   {r['restored']} rows, {r['acked_by_then']} acknowledged by then")
    if "failed" in results:
        print(f"failed: {results['failed']}")


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    sys.exit(main())
