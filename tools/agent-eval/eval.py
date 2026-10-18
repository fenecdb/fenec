#!/usr/bin/env python3
"""How well an agent queries fenecdb in FenecQL, against in the SQL it knows.

    eval.py fixture --server target/debug/fenec-server --work DIR
    eval.py run --condition A --models haiku,sonnet --server ... --work DIR --out runs.jsonl
    eval.py report runs.jsonl [--tsv results/DATE.tsv]

Each run is one task, fresh: the fixture file copied, a fenec-server started
over the copy on a port the system picks, and `claude -p` given the task
with one tool, `fq`, which sends a statement to that server's /query. The
run is scored from what fq logged and, for a write, from the database
afterwards. Every run spends tokens: see README.md.
"""

import argparse
import ast
import concurrent.futures
import datetime as dt
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))

import classify  # noqa: E402
import seed  # noqa: E402
import tasks as tasklib  # noqa: E402

ALLOWANCE = 3

COMMON = """\
You answer a task against a database by running statements on it. The one \
command you can run is `fq`, which sends a single statement to the database \
server and prints the HTTP status and the JSON answer:

  fq '<statement>'
  fq '<statement>' '<params as a JSON array>'    (the values of $1, $2, ...)

A statement holding a single quote can be piped in instead:

  fq <<'EOF'
  <statement>
  EOF

Run fq on its own, with no pipes, `&&` or other commands. You have at most \
3 statements for the task; a 4th is refused.

How you are graded: for a question, the result of the last statement that \
succeeded is your answer, so make your last statement return exactly what is \
asked -- the rows asked for and no others (extra columns are fine). Do not \
work the answer out by hand from several results. For a change to the data, \
the database's contents afterwards are checked; change nothing the task does \
not ask for. When you are done, reply with one short line.
"""

CONDITIONS = {
    # A: told what the database speaks, and given the brief /llms.txt opens with.
    "A": "The database is fenecdb. Its query language is FenecQL, which is not "
         "SQL: write FenecQL. Its brief, from https://fenecdb.com/llms.txt:\n\n"
         "{brief}\n"
         "The schema, as the server gives it (GET /_schema?as=fenecql):\n\n{schema}",
    # B and C: the same schema, and SQL by habit. C is B against another build.
    "B": "The database is fenecdb. Write SQL as you normally would.\n\n"
         "The schema, as the server gives it (GET /_schema?as=fenecql):\n\n{schema}",
}
CONDITIONS["C"] = CONDITIONS["B"]


def brief(build_py=ROOT / "site" / "build.py"):
    """LLMS_BRIEF as a site/build.py writes it into /llms.txt."""
    tree = ast.parse(Path(build_py).read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
                getattr(t, "id", None) == "LLMS_BRIEF" for t in node.targets):
            return ast.literal_eval(node.value)
    sys.exit("site/build.py: no LLMS_BRIEF")


# --- the server --------------------------------------------------------------

class Server:
    """A fenec-server over one file, on the port it says it took."""

    def __init__(self, binary, file, log):
        self.log = open(log, "w")
        self.proc = subprocess.Popen(
            [str(binary), "--file", str(file), "--http", "127.0.0.1:0"],
            stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.time() + 60
        while True:
            text = Path(log).read_text()
            m = re.search(r"listening on: (http://\S+)", text)
            if m:
                self.url = m.group(1)
                return
            if self.proc.poll() is not None or time.time() > deadline:
                self.stop()
                raise RuntimeError(f"fenec-server did not start: {text[-500:]}")
            time.sleep(0.05)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def post(url, statement, params=None):
    body = json.dumps({"query": statement, "params": params or []}).encode()
    req = urllib.request.Request(url + "/query", data=body,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def fixture(binary, work):
    """work/fixture.fenec and work/schema.fenecql, from seed.py."""
    work.mkdir(parents=True, exist_ok=True)
    path = work / "fixture.fenec"
    if path.exists():
        path.unlink()
    server = Server(binary, path, work / "fixture.log")
    try:
        for statement in seed.SCHEMA:
            status, body = post(server.url, statement)
            assert status == 200, body
        for name, docs in seed.data().items():
            status, body = post(server.url, f"put {name} $1", [docs])
            assert status == 200 and body == {"affected": len(docs)}, body
        with urllib.request.urlopen(server.url + "/_schema?as=fenecql") as r:
            schema = json.loads(r.read())["fenecql"]
    finally:
        server.stop()
    (work / "schema.fenecql").write_text(schema)
    print(f"{path}\n{schema}")


# --- scoring -------------------------------------------------------------------

def flat(v):
    if isinstance(v, dict):
        for x in v.values():
            yield from flat(x)
    elif isinstance(v, list):
        for x in v:
            yield from flat(x)
    else:
        yield v


def instant(s):
    try:
        return dt.datetime.fromisoformat(s.replace("Z", "+00:00")) if re.match(r"\d{4}-\d\d-\d\d", s) else None
    except ValueError:
        return None


def same(want, got):
    if want is None or got is None:
        return want is None and got is None
    if isinstance(want, bool) or isinstance(got, bool):
        return type(want) is type(got) and want == got
    if isinstance(want, (int, float)) and isinstance(got, (int, float)):
        if isinstance(want, float) and not want.is_integer():
            # An average may come back rounded to a whole cent.
            return abs(want - got) <= max(0.51, 1e-6 * abs(want))
        return abs(want - got) <= 1e-6 * max(1, abs(want))
    if isinstance(want, str) and isinstance(got, str):
        return want == got or (instant(want) is not None and instant(want) == instant(got))
    return False


def holds(want_row, got_row):
    """Every value of `want_row` among the values of `got_row`, each once."""
    cells = list(flat(got_row))
    for w in want_row:
        for i, g in enumerate(cells):
            if same(w, g):
                del cells[i]
                break
        else:
            return False
    return True


def rows_of(body):
    if isinstance(body, str):
        try:
            body = json.loads(body)
        except ValueError:
            return None
    if isinstance(body, dict) and isinstance(body.get("rows"), list):
        body = body["rows"]
    if isinstance(body, list) and all(isinstance(r, dict) for r in body):
        return body
    return None


def rows_match(want, got, ordered):
    if got is None or len(got) != len(want):
        return False
    if ordered:
        return all(holds(w, g) for w, g in zip(want, got))
    # A matching of wanted rows onto got rows (Kuhn's augmenting paths).
    fits = [[j for j, g in enumerate(got) if holds(w, g)] for w in want]
    owner = [-1] * len(got)

    def place(i, seen):
        for j in fits[i]:
            if j not in seen:
                seen.add(j)
                if owner[j] < 0 or place(owner[j], seen):
                    owner[j] = i
                    return True
        return False

    return all(place(i, set()) for i in range(len(want)))


WRITES = {"put", "insert", "set", "del", "update", "delete", "upsert", "replace", "merge"}


def writes(call):
    words = re.findall(r"[a-z]+", call["statement"].lower())
    return bool(words) and (words[0] in WRITES or (words[0] == "with" and any(w in WRITES for w in words)))


def ok(call):
    return 200 <= call["status"] < 300


def score(task, calls, url):
    """(success, first_try, detail) of a run, from fq's log and the server."""
    detail = {}
    good = [c for c in calls if ok(c)]
    if task["kind"] == "read":
        last = good[-1] if good else None
        success = last is not None and rows_match(task["expect"], rows_of(last["body"]), task["ordered"])
        first = bool(calls) and ok(calls[0]) and rows_match(
            task["expect"], rows_of(calls[0]["body"]), task["ordered"])
        return success, first and success, detail
    checks = []
    for v in task["verify"]:
        status, body = post(url, v["query"])
        checks.append(status == 200 and rows_match(v["rows"], rows_of(body), v["ordered"]))
    detail["state_ok"] = all(checks)
    answered = True
    if task.get("answer"):
        answered = any(rows_match(task["answer"], rows_of(c["body"]), False) for c in good)
        detail["answer_ok"] = answered
    success = detail["state_ok"] and answered
    first = (bool(calls) and ok(calls[0]) and writes(calls[0])
             and not any(ok(c) and writes(c) for c in calls[1:])
             and (not task.get("answer") or rows_match(task["answer"], rows_of(calls[0]["body"]), False)))
    return success, first and success, detail


def error_of(call):
    try:
        return json.loads(call["body"]).get("error", call["body"])
    except Exception:
        return call["body"]


# --- one run -------------------------------------------------------------------

def run_one(job):
    cond, model, task, rep, a = job
    rd = a.work / "runs" / f"{cond}-{model}-{task['id']}-{rep}"
    if rd.exists():
        shutil.rmtree(rd)
    (rd / "bin").mkdir(parents=True)
    db = rd / "db.fenec"
    shutil.copyfile(a.work / "fixture.fenec", db)
    server = Server(a.server, db, rd / "server.log")
    record = {"date": dt.date.today().isoformat(), "condition": cond, "build": a.build, "model": model,
              "task": task["id"], "area": task["area"], "kind": task["kind"], "rep": rep}
    try:
        calls_log = rd / "calls.jsonl"
        fq = rd / "bin" / "fq"
        fq.write_text("#!/bin/sh\nexec python3 " + " ".join(shlex.quote(str(x)) for x in (
            HERE / "fq.py", server.url, calls_log, ALLOWANCE)) + ' "$@"\n')
        fq.chmod(0o755)
        env = dict(os.environ, PATH=f"{rd / 'bin'}:{os.environ['PATH']}")
        cmd = [a.claude, "-p", task["prompt"], "--model", model,
               "--system-prompt", a.prompts[cond],
               "--tools", "Bash", "--allowedTools", "Bash(fq:*)",
               "--permission-prompts", "none", "--setting-sources", "local",
               "--safe-mode", "--strict-mcp-config", "--disable-slash-commands",
               "--no-session-persistence", "--output-format", "json",
               "--max-budget-usd", str(a.budget)]
        start = time.time()
        try:
            p = subprocess.run(cmd, cwd=rd, env=env, capture_output=True, text=True, timeout=a.timeout)
            out = p.stdout
            record["exit"] = p.returncode
        except subprocess.TimeoutExpired as e:
            out = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
            record["exit"] = "timeout"
        record["seconds"] = round(time.time() - start, 1)
        try:
            result = json.loads(out)
        except ValueError:
            result = {"result": out[-500:]}
        record["cost_usd"] = result.get("total_cost_usd")
        record["turns"] = result.get("num_turns")
        record["stop"] = result.get("subtype")
        record["model_id"] = ",".join(sorted((result.get("modelUsage") or {}).keys()))
        usage = result.get("usage") or {}
        record["tokens"] = {k: usage.get(k) for k in (
            "input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens")}
        record["reply"] = (result.get("result") or "")[:500]
        calls = [json.loads(line) for line in calls_log.read_text().splitlines()] if calls_log.exists() else []
        success, first, detail = score(task, calls, server.url)
        record.update(success=success, first_try=first, **detail)
    finally:
        server.stop()
    record["calls"] = [{"statement": c["statement"], "params": c["params"], "status": c["status"],
                        "error": None if ok(c) else str(error_of(c))[:300]}
                       for c in calls]
    record["n_calls"] = len(calls)
    if not a.keep:
        shutil.rmtree(rd)
    return rescore(record, task)


def run(a):
    a.work = a.work.resolve()
    schema = (a.work / "schema.fenecql").read_text()
    base = brief(a.brief)
    a.prompts = {c: COMMON + "\n" + CONDITIONS[c].format(brief=base, schema=schema) for c in CONDITIONS}
    wanted = set(a.tasks.split(",")) if a.tasks else None
    todo = [t for t in tasklib.tasks() if not wanted or t["id"] in wanted]
    done = set()
    if a.out.exists():
        for line in a.out.read_text().splitlines():
            r = json.loads(line)
            done.add((r["condition"], r.get("build") or "main", r["model"], r["task"], r["rep"]))
    jobs = [(c, m, t, rep, a) for rep in range(1, a.reps + 1) for m in a.models.split(",")
            for c in a.condition.split(",") for t in todo if (c, a.build, m, t["id"], rep) not in done]
    print(f"{len(jobs)} runs, {a.jobs} at a time", flush=True)
    lock = threading.Lock()
    spent = 0.0
    with concurrent.futures.ThreadPoolExecutor(a.jobs) as pool:
        futures = {pool.submit(run_one, j): j for j in jobs}
        for f in concurrent.futures.as_completed(futures):
            j = futures[f]
            try:
                r = f.result()
            except Exception as e:  # a run that could not be made is said, not scored
                print(f"{j[0]} {j[1]} {j[2]['id']}: {e!r}", flush=True)
                continue
            spent += r.get("cost_usd") or 0
            with lock, a.out.open("a") as out:
                out.write(json.dumps(r) + "\n")
            mark = "first" if r["first_try"] else ("ok" if r["success"] else r.get("failure"))
            print(f"{r['condition']} {r['model']:6} {r['task']} {mark:22} calls={r['n_calls']} "
                  f"${r.get('cost_usd') or 0:.3f} total=${spent:.2f}", flush=True)
            if a.max_usd and spent > a.max_usd:
                print(f"stopping: ${spent:.2f} spent, past --max-usd", flush=True)
                for other in futures:
                    other.cancel()
                break


def check(a):
    """Each task's reference FenecQL through the scoring an agent meets."""
    a.work = a.work.resolve()
    failed = 0
    for task in tasklib.tasks():
        rd = a.work / "check" / task["id"]
        if rd.exists():
            shutil.rmtree(rd)
        rd.mkdir(parents=True)
        shutil.copyfile(a.work / "fixture.fenec", rd / "db.fenec")
        server = Server(a.server, rd / "db.fenec", rd / "server.log")
        try:
            status, body = post(server.url, task["ref"])
            call = {"statement": task["ref"], "params": [], "status": status,
                    "body": body if isinstance(body, str) else json.dumps(body)}
            success, first, detail = score(task, [call], server.url)
        finally:
            server.stop()
        shutil.rmtree(rd)
        failed += not first
        print(f"{task['id']} {'ok' if first else 'FAILED'} [{status}] {json.dumps(body)[:150]} {detail or ''}")
    (a.work / "check").rmdir()
    sys.exit(1 if failed else 0)


# --- the report ------------------------------------------------------------------

TSV_COLUMNS = ["date", "condition", "build", "model", "model_id", "task", "area", "kind", "rep", "success",
               "first_try", "failure", "n_calls", "blame", "sql", "fenecql", "turns", "cost_usd",
               "seconds", "state_ok", "answer_ok", "handpicked", "tokens", "reply", "calls"]


def read_runs(path):
    """The runs of a JSONL `run` wrote, or of a TSV `report` wrote: the
    TSV holds what scoring them again needs, so a committed one can be
    judged again by a later classify.py."""
    text = path.read_text(encoding="utf-8").splitlines()
    if path.suffix != ".tsv":
        return [json.loads(line) for line in text]
    head = text[0].split("\t")
    runs = []
    for line in text[1:]:
        r = {}
        for k, v in zip(head, line.split("\t")):
            if v == "":
                r[k] = None
            elif v in ("True", "False"):
                r[k] = v == "True"
            elif k in ("blame", "sql", "fenecql", "tokens", "calls"):
                r[k] = json.loads(v)
            elif k in ("rep", "n_calls", "turns"):
                r[k] = int(v)
            elif k in ("cost_usd", "seconds"):
                r[k] = float(v)
            else:
                r[k] = v
        runs.append(r)
    return runs


def literal_in(value, text):
    if isinstance(value, str):
        return f'"{value}"' in text or f"'{value}'" in text
    return re.search(rf"(?<![\w.]){int(value)}(?![\w.])", text) is not None


def handpicked(task, statement):
    """A read whose last statement names every value of the answer the task
    does not: the agent read the rows, picked the answer by hand and asked
    for it back by its values, which the prompt tells it not to do."""
    if task["kind"] != "read":
        return False
    cells = [v for row in task["expect"] for v in row
             if v is not None and not isinstance(v, bool)
             and not (isinstance(v, float) and not v.is_integer())]
    if len(cells) < 2:
        return False
    return all(literal_in(v, statement) and not literal_in(v, task["prompt"]) for v in cells)


def told(answer, reply):
    """Every id of a write's answer said in the agent's last reply."""
    return all(re.search(rf"(?<![\w.]){v}(?![\w.])", reply or "") for row in answer for v in row)


def rescore(r, task):
    """A run's refusals blamed and its constructs found again, by this
    classify.py; a read answered by hand taken back, and a write's ids
    taken from the agent's reply where no statement answered them. The
    runs' records keep what the agent did, and the judging of it can change
    after: `report` runs this over every record again."""
    calls = r["calls"]
    for c in calls:
        c["blame"] = None if ok(c) else classify.blame(
            c["statement"], c["status"], json.dumps({"error": c["error"]}))
    r["sql"] = sorted({k for c in calls for k in classify.constructs(c["statement"])})
    r["fenecql"] = sorted({k for c in calls for k in classify.constructs(c["statement"], classify.FENECQL)})
    good = [c for c in calls if ok(c)]
    if r["success"] and good and handpicked(task, good[-1]["statement"]):
        r["handpicked"] = True
    if r.get("handpicked"):
        r["success"] = r["first_try"] = False
    if task.get("answer") and r.get("state_ok") and not r["success"] and told(task["answer"], r.get("reply")):
        r["answer_ok"] = r["success"] = True
    r.pop("failure", None)
    if not r["success"]:
        if r.get("handpicked"):
            r["failure"] = "answer picked by hand"
        elif not calls:
            r["failure"] = "no statement run"
        elif not good:
            r["failure"] = "every statement refused"
        elif task["kind"] == "write" and not r.get("state_ok"):
            r["failure"] = "wrong state"
        else:
            r["failure"] = "wrong result"
    return r


def report(a):
    by_id = {t["id"]: t for t in tasklib.tasks()}
    runs = [rescore(r, by_id[r["task"]]) for path in a.runs for r in read_runs(path)]
    for r in runs:
        r["build"] = r.get("build") or "main"
    cell = lambda r: f"{r['condition']} {r['build']}"
    pct = lambda n, d: f"{100 * n / d:.0f}%" if d else "-"
    print("| condition | model | runs | first try | within 3 | mean statements | cost |")
    print("|---|---|---:|---:|---:|---:|---:|")
    groups = {}
    for r in runs:
        groups.setdefault((cell(r), r["model"]), []).append(r)
    for (c, m), rs in sorted(groups.items()):
        n = len(rs)
        print(f"| {c} | {m} | {n} | {sum(r['first_try'] for r in rs)} ({pct(sum(r['first_try'] for r in rs), n)}) "
              f"| {sum(r['success'] for r in rs)} ({pct(sum(r['success'] for r in rs), n)}) "
              f"| {sum(r['n_calls'] for r in rs) / n:.2f} | ${sum(r.get('cost_usd') or 0 for r in rs):.2f} |")

    areas = sorted({r["area"] for r in runs}, key=lambda x: [t["area"] for t in tasklib.tasks()].index(x))
    keys = sorted(groups)
    print("\nWithin 3, by area:\n")
    print("| area | " + " | ".join(f"{c} {m}" for c, m in keys) + " |")
    print("|---|" + "---:|" * len(keys))
    for area in areas:
        cells = []
        for k in keys:
            rs = [r for r in groups[k] if r["area"] == area]
            cells.append(f"{sum(r['success'] for r in rs)}/{len(rs)}")
        print(f"| {area} | " + " | ".join(cells) + " |")

    for cond in sorted({cell(r) for r in runs}):
        rs = [r for r in runs if cell(r) == cond]
        blamed = {}
        held = {}
        for r in rs:
            key = (r["model"], r["task"], r["rep"])
            for i, c in enumerate(r["calls"]):
                if c["blame"]:
                    b = blamed.setdefault(c["blame"], {"calls": 0, "next": 0, "first": set(), "runs": set(),
                                                       "lost": set()})
                    b["calls"] += 1
                    b["next"] += i + 1 < len(r["calls"]) and ok(r["calls"][i + 1])
                    b["runs"].add(key)
                    if i == 0:
                        b["first"].add(key)
                    if not r["success"]:
                        b["lost"].add(key)
                    for k in classify.constructs(c["statement"]):
                        held[k] = held.get(k, 0) + 1
        print(f"\nCondition {cond}: refusals by what they are put down to "
              f"({sum(b['calls'] for b in blamed.values())} refused statements in {len(rs)} runs). "
              "First refused: runs whose first statement it refused; runs failed: of the runs it "
              "refused a statement of, those that ended wrong. Next ran: refused statements the "
              "agent's next statement ran after. In refused: refused statements holding it at all, "
              "blamed or not.\n")
        print("| construct | refused statements | first refused | runs hit | runs failed | next ran | in refused |")
        print("|---|---:|---:|---:|---:|---:|---:|")
        for name, b in sorted(blamed.items(), key=lambda kv: (-len(kv[1]["lost"]), -kv[1]["calls"])):
            print(f"| {name} | {b['calls']} | {len(b['first'])} | {len(b['runs'])} | {len(b['lost'])} "
                  f"| {b['next']} | {held.get(name, '')} |")
        reached = {}
        for r in rs:
            for k in r["sql"]:
                x = reached.setdefault(k, [0, 0])
                x[0] += 1
                x[1] += not r["success"]
        print(f"\nCondition {cond}: SQL constructs reached for (runs using it, of them failed)\n")
        print("| construct | runs | failed |")
        print("|---|---:|---:|")
        for name, (n, lost) in sorted(reached.items(), key=lambda kv: -kv[1][0]):
            print(f"| {name} | {n} | {lost} |")
        wrong = [r for r in rs if r.get("failure") in ("wrong result", "wrong state")]
        print(f"\nCondition {cond}: ran but answered wrong: "
              + ", ".join(sorted(f"{r['model']} {r['task']}" for r in wrong)))

    if a.tsv:
        with a.tsv.open("w") as f:
            f.write("\t".join(TSV_COLUMNS) + "\n")
            for r in runs:
                row = dict(r, blame=sorted({c["blame"] for c in r["calls"] if c["blame"]}))
                cells = []
                for k in TSV_COLUMNS:
                    v = row.get(k)
                    if isinstance(v, (list, dict)):
                        v = json.dumps(v, ensure_ascii=False)
                    cells.append("" if v is None else str(v).replace("\t", " ").replace("\n", " "))
                f.write("\t".join(cells) + "\n")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fixture")
    f.add_argument("--server", type=Path, default=ROOT / "target" / "debug" / "fenec-server")
    f.add_argument("--work", type=Path, required=True)
    c = sub.add_parser("check")
    c.add_argument("--server", type=Path, default=ROOT / "target" / "debug" / "fenec-server")
    c.add_argument("--work", type=Path, required=True)
    r = sub.add_parser("run")
    r.add_argument("--condition", default="A", help="A, B or C, or several: A,B")
    r.add_argument("--models", default="haiku,sonnet")
    r.add_argument("--tasks", help="T01,T02,... (every task when absent)")
    r.add_argument("--reps", type=int, default=1)
    r.add_argument("--server", type=Path, default=ROOT / "target" / "debug" / "fenec-server")
    r.add_argument("--work", type=Path, required=True)
    r.add_argument("--out", type=Path, required=True)
    r.add_argument("--jobs", type=int, default=4)
    r.add_argument("--claude", default="claude")
    r.add_argument("--budget", type=float, default=0.5, help="--max-budget-usd of each run")
    r.add_argument("--max-usd", type=float, default=0, help="stop once the runs have spent this much")
    r.add_argument("--timeout", type=int, default=300)
    r.add_argument("--keep", action="store_true", help="keep each run's directory")
    r.add_argument("--build", default="main",
                   help="what the --server binary was built from, recorded with each run: main, a branch")
    r.add_argument("--brief", type=Path, default=ROOT / "site" / "build.py",
                   help="the site/build.py whose LLMS_BRIEF condition A is given (another checkout's)")
    s = sub.add_parser("report")
    s.add_argument("runs", type=Path, nargs="+", help="run's JSONL or report's TSV files, read together")
    s.add_argument("--tsv", type=Path)
    a = p.parse_args()
    if a.cmd == "fixture":
        fixture(a.server, a.work)
    elif a.cmd == "check":
        check(a)
    elif a.cmd == "run":
        a.server = a.server.resolve()
        run(a)
    else:
        report(a)


if __name__ == "__main__":
    main()
