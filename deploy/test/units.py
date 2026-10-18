#!/usr/bin/env python3
"""deploy/systemd/'s units under a real systemd: two machines in Docker,
each booting systemd, the units and environment files installed as
deploy/RUNBOOK.md installs them, and every one started.

    make deploy-check                 # builds the image, then runs this
    python3 deploy/test/units.py      # the image built already (UNITS_IMAGE)

    m1: fenec-global, fenec-router (eu), fenec-node (eu-a, zone eu-1),
        fenec-archive@eu-b, cloudflared
    m2: fenec-global-standby, fenec-router-standby, fenec-node (eu-b, zone
        eu-2), fenec-archive@eu-a, fenec-archive-sync@eu-a (.timer)

What it holds:
  - `systemd-analyze verify` passes every unit, quietly;
  - each starts under its sandbox, and the deployment works through it: a
    region, two nodes and a tenant made through the global router, rows
    written through the router and its standby, a compact (a file renamed
    under the system call filter), a vector graph checkpointed at the stop;
  - no token is on any process's command line (`ps`), and no other user --
    another role's among them -- reads one out of /proc/<pid>/environ;
  - each runs as its role's user; LimitNOFILE and TasksMax reach the
    process; /proc/meminfo reads in the node's namespace (the compact
    before a file outgrows the page cache asks it); `systemd-analyze
    security` puts each fenecdb unit at 2.0 or below;
  - a stop syncs the node within its timeout and exits clean, and the data
    are there after the start; the archiver stops clean, its sealed archive
    verifies with the key the unit hands it; the bucket sync, with real
    rclone, copies the archive while rows are written to its tenant -- the
    copy verifying as an archive -- and node_exporter, scraped, reads when
    it last succeeded; a new disk's archive is refused over the bucket's,
    and copied once the bucket is copied back as the runbook does it; the
    tunnel finds its credentials (cloudflared is a stand-in:
    deploy/test/stub.py; its rules are deploy/test/ingress.py's);
  - the runbook's router promotion, by the units: the standby promoted, its
    unit switched, the old router back as its standby.

Every container and the network carry the run's label and are removed at
the end. The standard library alone.
"""

import json
import os
import re
import secrets
import subprocess
import sys
import time

IMAGE = os.environ.get("UNITS_IMAGE", "fenec-kit-systemd")
HERE = os.path.dirname(os.path.abspath(__file__))
DEPLOY = os.path.dirname(HERE)
RUN = "funits-" + secrets.token_hex(3)
LABEL = f"fenec-units={RUN}"
NET = RUN
T0 = time.monotonic()

TOKENS = {
    "data": secrets.token_hex(16),
    "admin-eu-a": secrets.token_hex(16),
    "admin-eu-b": secrets.token_hex(16),
    "repl": secrets.token_hex(16),
    "root": secrets.token_hex(16),
    "dirs": secrets.token_hex(16),
    "entry": secrets.token_hex(16),
}

FENEC_UNITS = ["fenec-node.service", "fenec-router.service", "fenec-router-standby.service",
               "fenec-global.service", "fenec-global-standby.service"]


class Failed(Exception):
    pass


def say(msg):
    print(f"[{time.monotonic() - T0:6.1f}s] {msg}", flush=True)


def check(ok, msg):
    if not ok:
        raise Failed(msg)


def docker(*args, check_=True, timeout=180, input=None):
    p = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout, input=input)
    if check_ and p.returncode != 0:
        raise Failed(f"docker {' '.join(args[:4])}: {(p.stderr or p.stdout).strip()[-3000:]}")
    return p


def sh(m, script, check_=True, timeout=180):
    """A shell script run as root on machine `m`; its stdout."""
    p = docker("exec", f"{RUN}-{m}", "sh", "-c", script, check_=False, timeout=timeout)
    if check_ and p.returncode != 0:
        raise Failed(f"on {m}: {script}\n{(p.stdout + p.stderr).strip()[-3000:]}")
    return p.stdout


def ask(m, method, url, envkey="", body=None):
    """An HTTP request from `m`, its token read there out of an env file."""
    args = ["exec", f"{RUN}-{m}", "python3", "/kit/test/ask.py", method, url, envkey]
    if body is not None:
        args.append(body if isinstance(body, str) else json.dumps(body))
    p = docker(*args, check_=False)
    if p.returncode != 0:
        return 0, p.stderr
    status, _, rest = p.stdout.partition("\n")
    return int(status), rest.strip()


def until(ok, what, within=60.0):
    end = time.monotonic() + within
    while time.monotonic() < end:
        got = ok()
        if got:
            return got
        time.sleep(0.2)
    raise Failed(f"waited {within} s for {what}")


def active(m, unit):
    return sh(m, f"systemctl is-active {unit} || true").strip()


def journal(m, unit):
    return sh(m, f"journalctl -u {unit} --no-pager -o cat || true")


def show(m, unit, prop):
    return sh(m, f"systemctl show -p {prop} --value {unit}").strip()


# ---------------------------------------------------------------- machines


def boot(m):
    docker("run", "-d", "--name", f"{RUN}-{m}", "--label", LABEL, "--privileged",
           "--cgroupns=private", "--network", NET, "--network-alias", m, "--hostname", m, IMAGE)
    docker("cp", DEPLOY + "/.", f"{RUN}-{m}:/kit")
    until(lambda: sh(m, "systemctl is-system-running 2>/dev/null || true").strip() in ("running", "degraded"),
          f"{m}'s systemd to boot")


def envfile(name, values):
    """deploy/env/<name> with the test's values in place of the examples'."""
    text = open(os.path.join(DEPLOY, "env", name)).read()
    for key, value in values.items():
        text, n = re.subn(rf"^{key}=.*$", f"{key}={value}", text, flags=re.M)
        check(n == 1, f"{name} has no {key} line")
    return text


def install(m, files):
    """The units, the user, the environment files (0600, root's) and the
    stand-ins, as the runbook installs them."""
    sh(m, "mkdir -p /etc/sysusers.d /etc/fenecdb /etc/cloudflared /usr/local/share/doc/fenecdb && "
          "cp /kit/systemd/fenecdb.sysusers /etc/sysusers.d/fenecdb.conf && systemd-sysusers && "
          "cp /kit/systemd/*.service /kit/systemd/*.timer /etc/systemd/system/ && "
          "cp /kit/RUNBOOK.md /usr/local/share/doc/fenecdb/ 2>/dev/null; "
          "install -m 0755 /kit/test/stub.py /usr/bin/cloudflared && "
          "install -D -m 0755 /kit/bin/fenec-archive-sync /usr/local/libexec/fenecdb/fenec-archive-sync && "
          # The bucket: a directory, rclone's `local` remote standing for S3,
          # writable by the sync alone, through a drop-in of the test's.
          "install -d -o fenec-sync -g fenec-sync -m 0700 /srv/bucket && "
          "mkdir -p '/etc/systemd/system/fenec-archive-sync@.service.d' && "
          "printf '[Service]\\nReadWritePaths=/srv/bucket\\n' "
          "> '/etc/systemd/system/fenec-archive-sync@.service.d/test-bucket.conf' && "
          # node_exporter's textfile collector (Debian's, read by its
          # prometheus user), writable by the sync as the runbook makes it.
          "setfacl -m u:fenec-sync:rwx /var/lib/prometheus/node-exporter")
    for path, text in files.items():
        # Secrets 0600 root's; cloudflared's config, which names none, is
        # read by its DynamicUser and so 0644, as the unit says.
        mode = "0644" if path.endswith(("config.yml", "archive-sync.env")) else "0600"
        docker("exec", "-i", f"{RUN}-{m}", "sh", "-c", f"cat > {path} && chmod {mode} {path}", input=text)
    sh(m, "systemctl daemon-reload")


def node_env(name):
    return envfile("node.env", {
        "FENEC_LISTEN": "0.0.0.0:8081",
        "FENEC_HTTP_TOKEN": TOKENS["data"],
        "FENEC_ADMIN_TOKEN": TOKENS[f"admin-{name}"],
        "FENEC_REPLICATION_TOKEN": TOKENS["repl"],
    })


def router_env(primary):
    return envfile("router.env", {
        "FENEC_LISTEN": "0.0.0.0:8090",
        "FENEC_REGION": "eu",
        "FENEC_GLOBAL": "http://m1:8091,http://m2:8091",
        "FENEC_LEASE": "5",
        "FENEC_PRIMARY": f"http://{primary}:8090",
        "FENEC_SHARD_TOKEN": TOKENS["root"],
        "FENEC_SHARD_REPLICATION_TOKEN": TOKENS["dirs"],
        "FENEC_SHARD_ENTRY_TOKEN": TOKENS["entry"],
        "FENEC_ROUTER_FLAGS": "--http-cors https://app.example.com",
    })


def global_env():
    return envfile("global.env", {
        "FENEC_LISTEN": "0.0.0.0:8091",
        "FENEC_PRIMARY": "http://m1:8091",
        "FENEC_SHARD_TOKEN": TOKENS["root"],
        "FENEC_SHARD_REPLICATION_TOKEN": TOKENS["dirs"],
        "FENEC_SHARD_ENTRY_TOKEN": TOKENS["entry"],
        "FENEC_GLOBAL_FLAGS": "--default-region eu",
    })


def archive_env():
    return envfile("archive.env", {"FENEC_REPLICATION_TOKEN": TOKENS["repl"]})


def archive_sync_env():
    return envfile("archive-sync.env", {"FENEC_ARCHIVE_REMOTE": "test-remote:/srv/bucket/eu"})


def archive_node_env(machine):
    return envfile("archive-node.env", {"FENEC_NODE_URL": f"http://{machine}:8081"})


def start(m, unit):
    sh(m, f"systemctl enable --now {unit}", check_=False)
    try:
        until(lambda: active(m, unit) == "active" and (
            "listening on: http://" in journal(m, unit)
            or not unit.startswith(("fenec-node", "fenec-router", "fenec-global"))),
            f"{unit} on {m} to start", within=30)
    except Failed as e:
        status = sh(m, f"systemctl status {unit} --no-pager -l || true")
        raise Failed(f"{e}\n{status}\n{journal(m, unit)[-3000:]}")


# ------------------------------------------------------------------- steps


def verify_units():
    """`systemd-analyze verify` over every unit, templates by an instance:
    exit 0 and nothing said about any of ours."""
    units = FENEC_UNITS + ["fenec-archive@eu-a.service", "fenec-archive-sync@eu-a.service",
                           "fenec-archive-sync@eu-a.timer", "cloudflared.service"]
    p = docker("exec", f"{RUN}-m2", "systemd-analyze", "verify", *units, check_=False)
    said = [l for l in (p.stdout + p.stderr).splitlines()
            if any(u.split("@")[0].split(".")[0] in l for u in units)]
    check(p.returncode == 0 and not said, f"systemd-analyze verify: exit {p.returncode}\n" + "\n".join(said))
    say(f"systemd-analyze verify: {len(units)} units, nothing to say")


def wire_up():
    s, b = ask("m1", "PUT", "http://m1:8091/_shard/regions/eu", "/etc/fenecdb/global.env:FENEC_SHARD_TOKEN",
               {"routers": ["http://m1:8090", "http://m2:8090"]})
    check(s == 200, f"region eu: {s} {b}")
    for m in ["m1", "m2"]:
        until(lambda: '"name":"eu"' in ask(m, "GET", f"http://{m}:8090/_shard/regions",
                                            "/etc/fenecdb/router.env:FENEC_SHARD_TOKEN")[1],
              f"{m}'s router to hear of region eu")
    for name, m, zone in [("eu-a", "m1", "eu-1"), ("eu-b", "m2", "eu-2")]:
        s, b = ask("m1", "PUT", f"http://m1:8091/_shard/nodes/{name}", "/etc/fenecdb/global.env:FENEC_SHARD_TOKEN",
                   {"addr": f"{m}:8081", "token": TOKENS[f"admin-{name}"], "region": "eu", "zone": zone})
        check(s in (200, 201), f"node {name}: {s} {b}")
    s, b = ask("m1", "PUT", "http://m1:8091/_shard/tenants/acme", "/etc/fenecdb/global.env:FENEC_SHARD_TOKEN",
               {"region": "eu", "schema": "create collection notes (n int, title text, e vector<2> @hnsw(cosine))"})
    check(s == 201 and "replica_on" in b, f"tenant acme: {s} {b}")
    until(lambda: "fenec_router_following 1" in ask("m2", "GET", "http://m2:8091/_metrics",
                                                     "/etc/fenecdb/global.env:FENEC_SHARD_TOKEN")[1],
          "the global standby to follow")
    say(f"wired up through the global router: region eu, nodes eu-a and eu-b, tenant acme ({b})")


def write(m, first, n):
    for i in range(first, first + n):
        s, b = ask(m, "POST", f"http://{m}:8090/t/acme/query", "/etc/fenecdb/node.env:FENEC_HTTP_TOKEN",
                   {"query": "put notes {id: $1, n: $1, e: $2}", "params": [i, [1.0, i / 100]]})
        check(200 <= s < 300, f"a write through {m}'s router: {s} {b}")


def count(m, port=8090):
    """The rows of the test's own ids (under 1000; the writes beside the
    bucket sync take ids from 1000) through `m`'s router, or None while it
    does not answer 200 -- a node just started holds no lease until the
    router's next round."""
    s, b = ask(m, "POST", f"http://{m}:{port}/t/acme/query", "/etc/fenecdb/node.env:FENEC_HTTP_TOKEN",
               {"query": "get notes select count() as c where id < 1000"})
    return json.loads(b)[0]["c"] if s == 200 else None


ROLES = {"fenec-node": "fenec-node", "fenec-router": "fenec-router", "fenec-router-standby": "fenec-router",
         "fenec-global": "fenec-router", "fenec-global-standby": "fenec-router",
         "fenec-archive@eu-a": "fenec-archive", "fenec-archive@eu-b": "fenec-archive"}
USERS = ["nobody", "fenec-node", "fenec-router", "fenec-archive", "fenec-sync"]


def secrets_kept():
    """No token on any command line; no other user reads the environment."""
    for m in ["m1", "m2"]:
        args = sh(m, "ps -eo args")
        for name, t in TOKENS.items():
            check(t not in args, f"token {name} is on a command line on {m}:\n{args}")
        for unit, role in ROLES.items():
            pid = sh(m, f"systemctl show -p MainPID --value {unit} 2>/dev/null || echo 0").strip()
            if pid in ("", "0"):
                continue
            check(show(m, unit, "User") == role, f"{unit} runs as {show(m, unit, 'User')}, not {role}")
            # Every other role's user too: a process reads the environment
            # of its own user's processes, ProtectProc or not, so the roles
            # sharing a machine are users of their own.
            for user in USERS:
                if user == role:
                    continue
                out = sh(m, f"setpriv --reuid={user} --regid={'nogroup' if user == 'nobody' else user} "
                            f"--init-groups cat /proc/{pid}/environ >/dev/null 2>&1 && echo READ || echo REFUSED")
                check(out.strip() == "REFUSED", f"{user} read {unit}'s environment on {m}")
        # The secrets' files; archive-sync.env holds none and is 0644.
        modes = sh(m, "stat -c '%a %U %n' $(ls /etc/fenecdb/*.env /etc/fenecdb/*.key "
                      "/etc/fenecdb/rclone.conf 2>/dev/null | grep -v archive-sync.env)")
        check(all(l.startswith("600 root") for l in modes.splitlines()), f"env files on {m}:\n{modes}")
    say("no token on a command line, each unit its role's user, no environment readable by another user "
        "or role, env files and archive key 0600 root's")


def limits_and_exposure():
    pid = sh("m1", "systemctl show -p MainPID --value fenec-node").strip()
    lim = sh("m1", f"grep 'Max open files' /proc/{pid}/limits")
    check("65536" in lim, f"the node's LimitNOFILE: {lim}")
    check(show("m1", "fenec-node", "TasksMax") == "16384", "the node's TasksMax")
    # ProcSubset=pid took /proc/meminfo away, and with it the room the page
    # cache has (cache::room): the compact before a file outgrows it was off.
    meminfo = sh("m1", f"nsenter -t {pid} -m head -1 /proc/meminfo || true")
    check(meminfo.startswith("MemTotal:"), f"/proc/meminfo in the node's namespace: {meminfo!r}")
    scores = {}
    for m, units in [("m1", ["fenec-node", "fenec-router", "fenec-global", "fenec-archive@eu-b"]),
                     ("m2", ["fenec-router-standby", "fenec-global-standby", "fenec-archive@eu-a"])]:
        for u in units:
            out = sh(m, f"systemd-analyze security {u}.service --no-pager | tail -1")
            score = float(re.search(r"level for .*: ([0-9.]+)", out).group(1))
            scores[u] = score
            check(score <= 2.0, f"{u}'s exposure {score}: {out}")
    say("LimitNOFILE 65536 and TasksMax 16384 on the node, /proc/meminfo readable in its namespace; exposure "
        + ", ".join(f"{u} {s}" for u, s in scores.items()))


def node_stop_and_start():
    began = time.monotonic()
    sh("m1", "systemctl stop fenec-node", timeout=200)
    took = time.monotonic() - began
    j = journal("m1", "fenec-node")
    check("shutting down: " in j and "open tenant(s) synced" in j, f"the node's stop said:\n{j[-2000:]}")
    result = show("m1", "fenec-node", "Result")
    status = show("m1", "fenec-node", "ExecMainStatus")
    check(result == "success", f"the node's stop: Result={result} ExecMainStatus={status}")
    start("m1", "fenec-node")
    say(f"the node stopped in {took:.2f} s (timeout 120), synced, exit {status}, and started again")


def archive_checks():
    until(lambda: "acme" in sh("m2", "ls /var/lib/fenecdb/archive/eu-a 2>/dev/null || true"),
          "the archive of eu-a to hold acme")
    # The archive is sealed: verified with the key, refused without it.
    with_key = sh("m2", "FENEC_KEY_FILE=/etc/fenecdb/archive.key fenec verify --tenants /var/lib/fenecdb/archive/eu-a")
    check("acme" in with_key, f"fenec verify with the key: {with_key}")
    without = docker("exec", f"{RUN}-m2", "fenec", "verify", "--tenants", "/var/lib/fenecdb/archive/eu-a", check_=False)
    check(without.returncode != 0, "a sealed archive verified without its key")
    began = time.monotonic()
    sh("m2", "systemctl stop fenec-archive@eu-a")
    took = time.monotonic() - began
    check(show("m2", "fenec-archive@eu-a", "Result") == "success", "the archiver's stop")
    sh("m2", "systemctl start fenec-archive@eu-a")
    # The bucket sync with real rclone, while the tenant is written as fast
    # as it takes rows of 64 KB: its newest segment grows as rclone reads
    # it, which without --local-no-check-updated failed every run ("source
    # file is being updated").
    writer = subprocess.Popen(
        ["docker", "exec", f"{RUN}-m1", "python3", "/kit/test/writes.py", "http://m1:8090/t/acme/query",
         "/etc/fenecdb/node.env:FENEC_HTTP_TOKEN", "/tmp/stop-writes", "120", "1000"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        time.sleep(1)
        for _ in range(3):
            sh("m2", "systemctl start fenec-archive-sync@eu-a.service", check_=False)
            result = show("m2", "fenec-archive-sync@eu-a.service", "Result")
            j = journal("m2", "fenec-archive-sync@eu-a.service")
            check(result == "success", f"the bucket sync: {result}\n{j[-3000:]}")
    finally:
        sh("m1", "touch /tmp/stop-writes", check_=False)
        out, err = writer.communicate(timeout=60)
    check(writer.returncode == 0 and int(out.strip() or 0) > 10, f"the writes beside the sync: {out} {err}")
    copied = sh("m2", "FENEC_KEY_FILE=/etc/fenecdb/archive.key fenec verify --tenants /srv/bucket/eu/eu-a")
    check("acme" in copied, f"the bucket's copy does not verify as an archive: {copied}")
    # The success as node_exporter scrapes it, as its own user: written
    # 0600 under the unit's UMask, it was absent here though root read it.
    metrics = node_metrics("m2")
    m_ = re.search(r'^fenec_archive_sync_last_success_timestamp_seconds\{node="eu-a"\} (\S+)$', metrics, re.M)
    check(m_ and abs(float(m_.group(1)) - int(sh("m2", "date +%s"))) < 120,
          "the sync's success is not among node_exporter's metrics: "
          + "\n".join(l for l in metrics.splitlines() if "textfile" in l or "fenec_" in l))
    check(re.search(r"^node_textfile_scrape_error 0$", metrics, re.M), "node_exporter could not read a textfile")

    # A disk replaced: the archiver begins a new archive at once, the
    # bucket holding the old one. The first copy is refused, and the bucket
    # is as it was.
    sh("m2", "systemctl stop fenec-archive@eu-a && mv /var/lib/fenecdb/archive/eu-a /var/lib/fenecdb/eu-a.lost "
             "&& systemctl start fenec-archive@eu-a")
    until(lambda: "acme" in sh("m2", "ls /var/lib/fenecdb/archive/eu-a 2>/dev/null || true"), "a new archive of eu-a")
    sums = "cd /srv/bucket/eu/eu-a && find . -type f -exec sha256sum {} + | sort"
    before = sh("m2", sums)
    sh("m2", "systemctl start fenec-archive-sync@eu-a.service", check_=False)
    refused = show("m2", "fenec-archive-sync@eu-a.service", "Result")
    j = journal("m2", "fenec-archive-sync@eu-a.service")
    check(refused != "success" and "refusing to copy" in j, f"a new archive copied over the bucket's: {refused}\n{j[-2000:]}")
    check(sh("m2", sums) == before, "the bucket changed under a refused copy")
    # The runbook's way back: the bucket copied back, and the archiver goes
    # on in the same archive, which the next copy takes.
    sh("m2", "systemctl stop fenec-archive@eu-a && rm -rf /var/lib/fenecdb/archive/eu-a && "
             "rclone copy test-remote:/srv/bucket/eu/eu-a /var/lib/fenecdb/archive/eu-a --config /etc/fenecdb/rclone.conf && "
             "chown -R fenec-archive:fenec-archive /var/lib/fenecdb/archive/eu-a && "
             "chmod -R u=rwX,g=rX,o= /var/lib/fenecdb/archive/eu-a && "
             "FENEC_KEY_FILE=/etc/fenecdb/archive.key fenec verify --tenants /var/lib/fenecdb/archive/eu-a && "
             "systemctl start fenec-archive@eu-a")
    write("m1", 2000, 5)
    sh("m2", "systemctl start fenec-archive-sync@eu-a.service", check_=False)
    result = show("m2", "fenec-archive-sync@eu-a.service", "Result")
    check(result == "success", f"the copy after the bucket came back: {result}\n"
                               f"{journal('m2', 'fenec-archive-sync@eu-a.service')[-2000:]}")
    copied = sh("m2", "FENEC_KEY_FILE=/etc/fenecdb/archive.key fenec verify --tenants /srv/bucket/eu/eu-a")
    check("acme" in copied, f"the bucket's copy after the way back: {copied}")
    sh("m2", "systemctl enable --now fenec-archive-sync@eu-a.timer")
    check(active("m2", "fenec-archive-sync@eu-a.timer") == "active", "the sync's timer")
    say(f"archive: sealed, verified with the key and refused without it; the archiver stopped clean in "
        f"{took:.2f} s; rclone copied it to the bucket three times while it was written, the copy "
        f"verifying, and node_exporter read the sync's success; a new disk's archive refused over the "
        f"bucket's, the bucket unchanged, and copied once the bucket was copied back")


def node_metrics(m):
    """node_exporter's metrics on `m`, scraped as Prometheus would."""
    if active(m, "prometheus-node-exporter") != "active":
        sh(m, "systemctl start prometheus-node-exporter")
    return sh(m, "python3 -c 'import urllib.request as u; "
                 "print(u.urlopen(\"http://127.0.0.1:9100/metrics\", timeout=10).read().decode())'")


def tunnel():
    start("m1", "cloudflared")
    j = journal("m1", "cloudflared")
    check("cloudflared stub: ready" in j and "tunnel.json reads" in j, f"cloudflared:\n{j}")
    say("cloudflared: started (Type=notify) with its credentials handed over")


def promote():
    """The runbook's promotion of a region's standby router, by the units."""
    s, b = ask("m2", "POST", "http://m2:8090/_replication/promote",
               "/etc/fenecdb/router.env:FENEC_SHARD_REPLICATION_TOKEN")
    check(s == 200, f"promote: {s} {b}")
    sh("m2", "systemctl enable fenec-router && systemctl disable fenec-router-standby && "
             "systemctl start fenec-router")
    until(lambda: active("m2", "fenec-router") == "active", "fenec-router on m2")
    check(active("m2", "fenec-router-standby") != "active", "the standby unit still runs beside the router's")
    until(lambda: "fenec_router_following 0" in ask("m2", "GET", "http://m2:8090/_metrics",
                                                     "/etc/fenecdb/router.env:FENEC_SHARD_TOKEN")[1],
          "m2's router to be the primary")
    # The old router comes back as the new one's standby.
    sh("m1", "systemctl stop fenec-router && systemctl disable fenec-router && "
             "sed -i 's|^FENEC_PRIMARY=.*|FENEC_PRIMARY=http://m2:8090|' /etc/fenecdb/router.env && "
             "systemctl enable --now fenec-router-standby")
    until(lambda: "fenec_router_following 1" in ask("m1", "GET", "http://m1:8090/_metrics",
                                                     "/etc/fenecdb/router.env:FENEC_SHARD_TOKEN")[1],
          "m1's router to follow m2's")
    say("promotion by the units: m2's standby promoted and switched to fenec-router, m1's router back as its standby")


def main():
    ok = False
    try:
        check(docker("image", "inspect", IMAGE, check_=False).returncode == 0,
              f"no image {IMAGE}: make deploy-check builds it")
        docker("network", "create", "--label", LABEL, NET)
        for m in ["m1", "m2"]:
            boot(m)
        say("two machines booted systemd")
        install("m1", {"/etc/fenecdb/node.env": node_env("eu-a"),
                       "/etc/fenecdb/router.env": router_env("m1"),
                       "/etc/fenecdb/global.env": global_env(),
                       "/etc/fenecdb/archive.env": archive_env(),
                       "/etc/fenecdb/archive-eu-b.env": archive_node_env("m2"),
                       "/etc/cloudflared/config.yml": open(os.path.join(DEPLOY, "cloudflared", "config.yml")).read(),
                       "/etc/cloudflared/tunnel.json": '{"AccountTag":"test","TunnelID":"test","TunnelSecret":"test"}'})
        install("m2", {"/etc/fenecdb/node.env": node_env("eu-b"),
                       "/etc/fenecdb/router.env": router_env("m1"),
                       "/etc/fenecdb/global.env": global_env(),
                       "/etc/fenecdb/archive.env": archive_env(),
                       "/etc/fenecdb/archive-eu-a.env": archive_node_env("m1"),
                       "/etc/fenecdb/archive-sync.env": archive_sync_env(),
                       "/etc/fenecdb/rclone.conf": "[test-remote]\ntype = local\n"})
        for m in ["m1", "m2"]:
            sh(m, "fenec key /etc/fenecdb/archive.key >/dev/null")
        verify_units()

        start("m1", "fenec-global")
        start("m1", "fenec-router")
        start("m1", "fenec-node")
        start("m2", "fenec-global-standby")
        start("m2", "fenec-router-standby")
        start("m2", "fenec-node")
        say("started: the global router and the region's router and node on m1, their standbys and a node on m2")
        wire_up()
        start("m2", "fenec-archive@eu-a")
        start("m1", "fenec-archive@eu-b")

        write("m1", 1, 30)
        write("m2", 31, 30)
        until(lambda: count("m1") == 60 and count("m2") == 60, "60 rows through both routers")
        s, b = ask("m1", "POST", "http://m1:8090/t/acme/query", "/etc/fenecdb/node.env:FENEC_HTTP_TOKEN",
                   {"query": "compact"})
        check(s == 200, f"a compact under the sandbox: {s} {b}")
        say("60 rows written through the router and its standby, read through both; a compact ran")

        secrets_kept()
        limits_and_exposure()
        node_stop_and_start()
        until(lambda: count("m1") == 60, "the rows after the node's restart")
        archive_checks()
        tunnel()
        promote()
        write("m1", 61, 10)
        until(lambda: count("m2") == 70, "70 rows through the promoted router")
        say("10 rows more through the old router, now a standby; 70 read through the promoted one")
        ok = True
    except (Failed, subprocess.TimeoutExpired) as e:
        say(f"FAILED: {e}")
        for m in ["m1", "m2"]:
            failed = sh(m, "systemctl --failed --no-legend || true", check_=False) if docker(
                "inspect", f"{RUN}-{m}", check_=False).returncode == 0 else ""
            if failed.strip():
                print(f"--- failed units on {m}:\n{failed}")
    finally:
        if os.environ.get("UNITS_KEEP") == "1":
            say(f"UNITS_KEEP=1: left running, label {LABEL}")
        else:
            ids = docker("ps", "-aq", "--filter", f"label={LABEL}", check_=False).stdout.split()
            if ids:
                docker("rm", "-f", *ids, check_=False)
            docker("network", "rm", NET, check_=False)
            say("cleaned up: both machines and the network removed")
    print(f"== units {RUN}: {'PASSED' if ok else 'FAILED'} ==")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
