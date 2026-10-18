# Agents writing FenecQL, against agents writing SQL

How well does an LLM agent query fenecdb when it writes FenecQL, and how
well when it writes the SQL it knows by habit? The answer decides which SQL
spellings fenecdb's parser should take: the ones agents reach for and fail
on.

**Every run spends tokens.** A run is one `claude -p` session. The 240 runs
on main of 2026-10-18 (A once and B twice, two models, 40 tasks) cost $3.10
as the CLI counts it (`total_cost_usd`): 171k output tokens, 2.66M read from
the prompt cache and 0.69M written to it, about a cent a run; the 240 on the
SQL-spellings branch (C twice, A once) $2.70. `run --max-usd` stops a batch
once it has spent that much, and `--budget` caps each run
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
already holds, so a batch cut short goes on where it stopped. Against
another build -- C, or A over a branch's brief -- build that server
elsewhere, make its fixture with it (its schema text is what the agents are
shown), and name the build, which each run records and `report` groups by;
`--brief` takes the `site/build.py` A's brief comes from:

    git worktree add --detach /tmp/sql-branch <commit>
    CARGO_TARGET_DIR=/tmp/sql-target cargo build -p fenec-server --manifest-path /tmp/sql-branch/Cargo.toml
    python3 tools/agent-eval/eval.py fixture --server /tmp/sql-target/debug/fenec-server --work $W2
    python3 tools/agent-eval/eval.py run --condition C --reps 2 --build sql-spellings \
        --server /tmp/sql-target/debug/fenec-server --brief /tmp/sql-branch/site/build.py \
        --work $W2 --out $W2/runs.jsonl
    python3 tools/agent-eval/eval.py report tools/agent-eval/results/2026-10-18.tsv $W2/runs.jsonl

`--tasks T01,T27` runs some tasks, `--reps 2` each one twice, `--keep` keeps
each run's directory (its `fq` log and the server's). It needs the `claude`
CLI logged in; nothing here runs in CI.

`report` reads the TSV it wrote as well as `run`'s JSONL, several at once,
and judges every run again with the classify.py and the scoring it finds: a
committed results file is re-read by a later classifier, not frozen with the
one that wrote it (the `blame` column a TSV holds is the classifier's of the
day it was written).

## Results

`results/2026-10-18.tsv` is every run: its statements with their statuses
and errors, what each refusal was put down to, the SQL constructs it reached
for, the agent's last reply, its turns, tokens and cost. Models:
`claude-haiku-4-5-20251001` and `claude-sonnet-5-5`; fenec-server at
96a84c07 (main). A ran once a task and model, B twice. C and A over the
SQL-spellings branch are [below](#the-sql-spellings-branch-356).

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
| 9 | `rowid` or `_id` for the id | 4 | 4 | 4 | 0 | 24 |
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
  a key guessed SQLite's `rowid` or `_id` (24 refused statements, 9 runs,
  all failed).
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

### The SQL-spellings branch (#356)

C is B's prompt word for word against fenec-server at bfbb0e80
(`sql-spellings`, on main 354074f5), twice a task and model; A ran over it
once more, its brief that commit's `LLMS_BRIEF`. Its fixture was made by
that server, so the schema both were shown opens with the line naming the
implicit `id`. `results/2026-10-18-sql-spellings.tsv` holds the 240 runs,
$2.70.

| condition | model | runs | first try | within 3 | mean statements |
|---|---|---:|---:|---:|---:|
| A main | haiku | 40 | 26 (65%) | 33 (82%) | 1.48 |
| A main | sonnet | 40 | 26 (65%) | 37 (92%) | 1.43 |
| A sql-spellings | haiku | 40 | 31 (78%) | 38 (95%) | 1.27 |
| A sql-spellings | sonnet | 40 | 32 (80%) | 40 (100%) | 1.25 |
| B main | haiku | 80 | 28 (35%) | 49 (61%) | 2.06 |
| B main | sonnet | 80 | 24 (30%) | 59 (74%) | 2.15 |
| C sql-spellings | haiku | 80 | 49 (61%) | 71 (89%) | 1.62 |
| C sql-spellings | sonnet | 80 | 57 (71%) | 78 (98%) | 1.48 |

Within 3 by area, C against B: filter 28/28 (21/28), order 12/12 (8/12),
aggregate 32/32 (29/32), related 13/20 (4/20), text 8/8 (2/8), null 17/20
(17/20), insert 8/8 (3/8), upsert 8/8 (4/8), returning 11/12 (8/12).

What C's statements were still refused for, by the runs that then ended
wrong (columns as above):

| construct | refused statements | first refused | runs hit | runs failed | next ran | in refused |
|---|---:|---:|---:|---:|---:|---:|
| JOIN | 32 | 18 | 21 | 8 | 12 | 32 |
| JSON `->>`, `json_extract` | 6 | 2 | 4 | 3 | 1 | 6 |
| subquery in the select list | 3 | 1 | 3 | 2 | 2 | 3 |
| `SELECT DISTINCT` | 6 | 6 | 6 | 1 | 1 | 8 |
| column alias without `AS` (`count(*) c`) | 2 | 1 | 2 | 1 | 1 | 2 |
| comma join `FROM a, b` | 1 | 0 | 1 | 1 | 0 | 1 |
| `CAST(... AS ...)` | 1 | 0 | 1 | 1 | 0 | 1 |
| `LIKE` over a json field | 1 | 0 | 1 | 1 | 0 | |
| `ON CONFLICT (sku) DO UPDATE` | 4 | 4 | 4 | 0 | 4 | 4 |
| unquoted text value (`in (paid, shipped)`) | 3 | 3 | 3 | 0 | 3 | |
| `GROUP BY` with no aggregate | 3 | 0 | 3 | 0 | 3 | |
| scalar subquery `= (SELECT ...)` | 3 | 3 | 3 | 0 | 2 | 3 |
| `NOT EXISTS` | 2 | 2 | 2 | 0 | 2 | 3 |
| a clause after `lookup` reading the parent | 2 | 0 | 2 | 0 | 2 | |
| `?` placeholders | 1 | 1 | 1 | 0 | 1 | |
| `INSERT ... SELECT` | 1 | 0 | 1 | 0 | 1 | 1 |

`INSERT ... VALUES` (17 runs), `UPDATE ... SET` (16), `LIKE`/`ILIKE` (18),
`IN (list)` (15), `NOT IN` (8), `BETWEEN` and `COUNT(col)` were taken
wherever written; none lost a task. No run gave `~` a regex: they wrote
`LIKE`. No run guessed `rowid` or `_id`. Over A the brief's new lines ended
the substring failures (T04 and T06 right in both runs) and T27's null trap.

**Recovered with the hints.** The JOIN hint (16 of the 32 refusals that
held a JOIN) was followed by `IN (SELECT ...)` 9 times and by `lookup`
twice; the related-row tasks went from 4 of 20 runs right to 13. After the
scalar subquery hint (`= (SELECT ...)` -> `in`) the next statement ran 2
times of 3, after `NOT EXISTS`'s refusal (`NOT IN`) 2 of 2, after `ON
CONFLICT (sku)`'s (`ON CONFLICT` with no target) 7 of 7 in C and A -- each
at a statement's cost.

**Still refused, where a hint or a spelling would have done it:**

- **A JOIN with a `GROUP BY`** is refused for the grouping (`` `c.id` is
  neither aggregated nor grouped by ``), never with the JOIN hint: 11 of the
  32, 9 of them in runs that failed. T23 (orders per Italian customer)
  failed in 3 of 4 C runs on all three statements. An alias before `JOIN`
  (``unknown command `c`: an alias``) and `SELECT DISTINCT` in front of one
  likewise answer before the JOIN does. Looking for `join` before anything
  else in a refused statement would hand the hint over at the first try.
- **JSON operators** (`meta->>'source'`, `json_extract(meta, '$.source')`)
  have no hint (``expected a value, found `>` ``); 3 of 4 runs lost T30.
  `meta.source` is the spelling, and could be the hint or `->>`'s reading.
- **`SELECT DISTINCT`** is refused with the `group` hint, after which 3
  runs wrote `GROUP BY city` with no aggregate and were refused again;
  `DISTINCT` inside `NOT IN (SELECT DISTINCT ...)` is refused too, where it
  changes nothing.
- **A column alias with no `AS`** (`count(*) c`, `sum(discount) s`) is
  refused as ``expected a command`` at a place that is not the alias.
- **A subquery in the select list** (`select (select name from products
  where id = product_id) as name, qty ...`) and a comma join (`FROM a, b`)
  have no hint; both were T20's second and third tries after the JOIN hint
  in the runs that lost it.
- **`ON CONFLICT (sku)`** with a target naming the `@unique` field could
  be taken: 7 of the 8 runs that wrote `ON CONFLICT` named one first.
- A `lookup`'s child to parent direction (`get order_lines lookup products
  on id = product_id`) is in the hint, and both runs that took it put the
  parent's `where order_id = 1` after the `lookup`, where it reads the
  child (``field `order_id` ``), and were refused once more before getting
  it right.
