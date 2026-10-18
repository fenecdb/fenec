# Agents writing FenecQL, against agents writing SQL

How well does an LLM agent query fenecdb when it writes FenecQL, and how
well when it writes the SQL it knows by habit? The answer decides which SQL
spellings fenecdb's parser should take: the ones agents reach for and fail
on.

**Every run spends tokens.** A run is one `claude -p` session. The 240 runs
of 2026-10-18 (A once and B twice, two models, 40 tasks) cost $3.10 as the
CLI counts it (`total_cost_usd`): 171k output tokens, 2.66M read from the
prompt cache and 0.69M written to it, about a cent a run. `run --max-usd`
stops a batch once it has spent that much, and `--budget` caps each run
(`--max-budget-usd`).

## What a run is

One task, fresh: the fixture file copied, a debug `fenec-server` started over
the copy on a port the system picks, and `claude -p` handed the task. Its
one tool is `fq`, which POSTs a statement to that server's `/query` and
prints the status and the JSON answer:

    fq '<statement>' ['<params as a JSON array>']

The agent may run at most **3** statements; a fourth is refused without
being sent. Everything else is shut off: `--tools Bash` with only
`Bash(fq:*)` allowed and every other prompt denied, `--safe-mode`,
`--setting-sources local`, `--strict-mcp-config`, and the run's directory a
fresh one outside any repository, so no CLAUDE.md, skill, plugin or setting
of the machine's reaches it. Its system prompt (`COMMON` in `eval.py`) says
how it is graded, then one of:

| condition | what the agent is told |
|---|---|
| **A** FenecQL | "The database is fenecdb. Its query language is FenecQL, which is not SQL: write FenecQL." The brief `/llms.txt` opens with (`LLMS_BRIEF` in `site/build.py`, read from there), then the schema. |
| **B** SQL on main | "The database is fenecdb. Write SQL as you normally would." The schema, no brief. |
| **C** SQL on the SQL-spellings branch | B's prompt word for word, against that branch's `fenec-server` (`--server`). |

The schema is what `GET /_schema?as=fenecql` answers -- `create collection`
statements, the text the MCP `schema` tool gives an agent too. B is the
agent that meets fenecdb through a tool without having read its docs; it
gets the server's error messages, which is how it can correct itself.

## The fixture and the tasks

`seed.py` makes a shop from a fixed seed: 60 customers (a `nocase @unique`
email, a `@hash` country, a nullable tier, a `@sorted` signup time, a `json`
meta), 40 products (a `@unique` sku, an `int` price in cents `@sorted`, a
`@text` description, a list of tags), 200 orders (a nullable discount and
note) and 490 order lines. Ids are given, 1 up, so a task can name one and
an insert takes the next.

`tasks.py` holds 40 tasks in plain words, each answer worked out in Python
over seed.py's rows -- never by asking the server:

| area | tasks | what they reach for in SQL |
|---|---|---|
| filter | T01-T07 | `IN (...)`, `BETWEEN`, `LIKE`/`ILIKE`, `NOT IN`, ranges of time |
| order | T08-T10 | `ORDER BY`, `LIMIT`/`OFFSET`, a join or subquery for "the customer whose email is" |
| aggregate | T11-T18 | `GROUP BY`, `HAVING`, `COUNT(DISTINCT)`, `SELECT DISTINCT` |
| related | T19-T23 | `JOIN`, `IN (SELECT ...)`, `NOT IN`/`NOT EXISTS` (an anti-join) |
| text | T24-T25 | `LIKE '%word%'` (FenecQL: `match` or `~`) |
| null | T26-T30 | `IS NULL`, `<>` beside nulls, `AVG` over nulls, `COALESCE`, a json field (`->>`) |
| insert | T31-T32 | `INSERT ... VALUES`, several rows |
| update | T33-T34 | `UPDATE ... SET col = col + 10` |
| delete | T35 | `DELETE ... WHERE` |
| upsert | T36-T37 | `INSERT ... ON CONFLICT DO UPDATE`, the row there and not |
| returning | T38-T40 | `UPDATE/INSERT/DELETE ... RETURNING` |

T27 is a trap of fenecdb's own: in FenecQL `tier != "gold"` holds for a row
whose tier is null, where SQL's three-valued logic leaves the row out.

Each task also has a reference FenecQL answer (`ref`), and `eval.py check`
runs every one through the scoring an agent meets: what each task asks can
be said in one statement, and the answers above are the engine's.

## Scoring

- **A read** is right when the last statement that succeeded answered as
  many rows as the answer, each holding the answer's values -- other
  columns may sit beside them, nested `lookup` children count, an average
  may be rounded to a whole cent, and the order counts only where the task
  asks for one. A last statement naming every value of the answer as a
  literal -- the rows read, the answer picked out by hand, and asked for
  again by its values -- is wrong: the prompt says not to.
- **A write** is right when the harness's own queries over the database
  afterwards (`verify`) find what the task asked -- over the whole
  collection where a task could touch other rows; T38-T40 also need the
  ids asked for answered by some statement or named in the agent's reply.
- **First try** is the first statement alone: a read whose first statement
  answered right, a write whose first statement was a write that succeeded,
  the only write that did, leaving the database right (and answering the
  ids, for T38-T40).
- **Within 3** is the task right after the agent's statements, at most 3.

A refused statement is put down to one construct (`classify.py`): the
parser's own words where they name it (`` `in (...)` takes a `get` ``, a
double-quoted name), else the SQL construct the error's `position` falls in,
else the error's words. A JOIN's aliases and qualified names are the JOIN's.
A run that ran but answered wrong is a `wrong result` or `wrong state`.

## Running it

    cargo build -p fenec-server
    W=$(mktemp -d)
    python3 tools/agent-eval/eval.py fixture --work $W
    python3 tools/agent-eval/eval.py check --work $W
    python3 tools/agent-eval/eval.py run --condition A,B --models haiku,sonnet \
        --work $W --out $W/runs.jsonl --jobs 6 --max-usd 40
    python3 tools/agent-eval/eval.py report $W/runs.jsonl --tsv tools/agent-eval/results/$(date +%F).tsv

`run` appends one JSON line a run and passes over the runs its `--out`
already holds, so a batch cut short goes on where it stopped. For C, build
the branch's server elsewhere and point `--server` at it:

    git worktree add /tmp/sql-branch <branch> && cargo build -p fenec-server --manifest-path /tmp/sql-branch/Cargo.toml
    python3 tools/agent-eval/eval.py run --condition C --server /tmp/sql-branch/target/debug/fenec-server ...

`--tasks T01,T27` runs some tasks, `--reps 2` each one twice, `--keep` keeps
each run's directory (its `fq` log and the server's). It needs the `claude`
CLI logged in; nothing here runs in CI.

`report` reads the TSV it wrote as well as `run`'s JSONL, and judges every
run again with the classify.py and the scoring it finds: a committed
results file is re-read by a later classifier, not frozen with the one that
wrote it.

## Results

`results/2026-10-18.tsv` is every run: its statements with their statuses
and errors, what each refusal was put down to, the SQL constructs it reached
for, the agent's last reply, its turns, tokens and cost. Models:
`claude-haiku-4-5-20251001` and `claude-sonnet-5-5`; fenec-server at
96a84c07 (main). A ran once a task and model, B twice. Condition C waits for
the SQL-spellings branch.

| condition | model | runs | first try | within 3 | mean statements | cost |
|---|---|---:|---:|---:|---:|---:|
| A FenecQL | haiku | 40 | 26 (65%) | 33 (82%) | 1.48 | $0.55 |
| A FenecQL | sonnet | 40 | 26 (65%) | 37 (92%) | 1.43 | $0.39 |
| B SQL on main | haiku | 80 | 28 (35%) | 49 (61%) | 2.06 | $1.37 |
| B SQL on main | sonnet | 80 | 24 (30%) | 59 (74%) | 2.15 | $0.80 |

Within 3 statements, by area:

| area | A haiku | A sonnet | B haiku | B sonnet |
|---|---:|---:|---:|---:|
| filter | 5/7 | 5/7 | 10/14 | 11/14 |
| order | 3/3 | 3/3 | 4/6 | 4/6 |
| aggregate | 8/8 | 7/8 | 14/16 | 15/16 |
| related | 2/5 | 5/5 | 1/10 | 3/10 |
| text | 2/2 | 2/2 | 2/4 | 0/4 |
| null | 3/5 | 5/5 | 7/10 | 10/10 |
| insert | 2/2 | 2/2 | 1/4 | 2/4 |
| update | 2/2 | 2/2 | 4/4 | 4/4 |
| delete | 1/1 | 1/1 | 2/2 | 2/2 |
| upsert | 2/2 | 2/2 | 1/4 | 3/4 |
| returning | 3/3 | 3/3 | 3/6 | 5/6 |

Forty tasks a cell is a coarse instrument: one task is 2.5 points of a row,
and the same model on the same task went both ways between B's two
repetitions in 12 of 80 pairs.

### What SQL costs today (condition B, 160 runs)

Each refused statement put down to one construct, ranked by the runs that
then ended wrong. *Runs hit*: runs with a statement refused for it; *first
refused*: of those, runs whose first statement it was; *in refused*: refused
statements holding it at all, whatever they were put down to.

| # | construct | refused statements | runs hit | runs failed | first refused | in refused |
|---:|---|---:|---:|---:|---:|---:|
| 1 | JOIN (with its aliases and `t.col`) | 47 | 25 | 22 | 18 | 47 |
| 2 | `LIKE` / `ILIKE` | 23 | 17 | 14 | 16 | 23 |
| 3 | `INSERT INTO t (cols) VALUES (...)` | 18 | 17 | 9 | 17 | 18 |
| 4 | `UPDATE t SET col = v` | 19 | 19 | 6 | 11 | 19 |
| 5 | string functions (`contains()`, `ends_with()`) | 8 | 7 | 6 | 0 | 8 |
| 6 | full text in `WHERE` (`MATCH`, `@@`, `match()`) | 8 | 6 | 5 | 0 | 8 |
| 7 | `NOT IN` | 8 | 8 | 4 | 6 | 8 |
| 8 | scalar subquery `= (SELECT ...)` | 4 | 4 | 4 | 2 | 7 |
| 9 | `rowid` for the id | 4 | 4 | 4 | 0 | 22 |
| 10 | several rows as `{...}, {...}` without `[ ]` | 4 | 4 | 4 | 0 | 4 |
| 11 | `SELECT DISTINCT` | 8 | 5 | 3 | 4 | 17 |
| 12 | `EXISTS` | 2 | 2 | 2 | 2 | 2 |
| 13 | JSON `->>`, `['key']`, `json_extract` | 6 | 4 | 1 | 2 | 6 |
| 14 | `{ col = v }` | 4 | 4 | 1 | 0 | 4 |
| 15 | `ON CONFLICT ... DO UPDATE` | 1 | 1 | 1 | 0 | 6 |
| - | `IN ('a', 'b')` | 11 | 11 | 0 | 11 | 15 |
| - | `COUNT(col)` | 4 | 4 | 0 | 4 | 5 |
| - | `BETWEEN` | 2 | 2 | 0 | 2 | 2 |

Items 5, 6, 10 and 14 are what agents tried after a refusal (`LIKE` led to
`contains()` and `MATCH`; `VALUES` to `{...}, {...}`; `SET col = v` to
`{ col = v }`), and `ON CONFLICT` is refused behind the `VALUES` before it
in five of its six statements. `IN (list)`, `COUNT(col)` and `BETWEEN`
never lost a task -- the agent wrote them out as `or`s, `count(*)` or two
comparisons -- but each cost the first try every time, and the `or`s it
wrote for every `IN (list)` reach no index (only an `and` chain does).

Not a refusal: **`~` given a regex.** In PostgreSQL `~` is a regex match;
in FenecQL it is a case-insensitive substring. Five B runs wrote
`email ~ '@example\.org$'` or `name ~ '(?i)lamp'`, which ran and matched
nothing: all five wrong, and in three the agent told the user there were no
such rows.

### Notes

- **The error words decide the recovery.** Where the parser names what it
  wanted, the agent recovers: `` `in (...)` takes a `get` `` (11 of 11),
  `` expected `{`, found `(` `` after `INSERT INTO t` (agents then wrote
  `INSERT INTO t {...}`, which is taken), `` `group` needs an aggregate ``.
  Where it says ``unknown command `x` ``, it does not: that is its word for
  anything it did not expect mid-statement -- `like`, `not` (of `NOT IN`),
  `between`, `join`, a table alias `o` -- and none of those is a command.
- **The schema shows no `id`.** `GET /_schema?as=fenecql` (and the MCP
  `schema` tool) lists the declared fields, and every SQL agent that needed
  a key guessed SQLite's `rowid` (22 refused statements, 8 runs, all
  failed) or `_id`.
- **Brief gaps (condition A).** The tasks asking for a substring and an
  ending (T04 "lamp" in any case, T06 emails at example.org) failed in all
  four A runs: the brief never names `~`, and the agents tried `like`,
  `match` over a field with no `@text`, `contains()` and `ends_with()`.
  `not in` is refused in FenecQL too, and the brief does not say how to
  negate a list (`not (id in (get ...))`). `lookup` from a child to its
  parent (`lookup products on id = product_id`) is not in the brief's
  examples, and the haiku run that needed it spent its three statements on
  the key named on the wrong side and on fields of the other level.
  `count(col)` is refused (`count(*)`, `count(distinct f)` are the forms).
- **`!=` passes a null.** In FenecQL `tier != "gold"` holds for a row with
  no tier, where SQL's three-valued logic leaves it out (T27); one A run
  answered with it. B's agents all wrote `tier IS NOT NULL AND tier <>
  'gold'`, which the task's wording asked for.
- **Joins are where SQL habit loses most.** Related-row tasks: A 7 of 10, B
  4 of 20. No B agent found `lookup`; they went JOIN, then qualified names
  without aliases, then `IN (SELECT ...)` -- taken -- or a scalar subquery,
  refused with `` expected `)`, found `id` ``.
- **Quotes did not trip anyone.** B's statements held single-quoted
  strings in 185 cases and double-quoted ones in 46; both are taken, and no
  agent quoted an identifier.
- One A run (sonnet, T17) wrote the right query and then a different one
  after it, and is scored on the last: the rule is in the prompt, and the
  run counts as wrong.
