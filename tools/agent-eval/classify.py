"""What an agent's statement reaches for, and what a refusal is blamed on.

`constructs` lists the SQL habits a statement holds, found with its string
literals blanked out so that a value cannot look like a keyword. `blame`
names the one a refusal is put down to: the parser's own words where they
name it, else the construct the parser's `position N` falls in, else the
error's words. A JOIN brings aliases and qualified names with it, and the
parser stops at the first of those, so a statement holding a JOIN is blamed
on the JOIN.
"""

import json
import re

import seed

# (name, pattern) over the statement lowercased, its literals blanked.
SQL = [
    ("JOIN", r"\b(inner\s+|left\s+(outer\s+)?|right\s+|full\s+|cross\s+)?join\b"),
    ("comma join (FROM a, b)", r"\bfrom\s+\w+(\s+(as\s+)?\w+)?\s*,\s*\w+"),
    ("WITH (CTE)", r"^\s*with\b"),
    ("UNION", r"\bunion\b"),
    ("subquery in FROM", r"\bfrom\s*\("),
    ("EXISTS", r"\b(not\s+)?exists\s*\("),
    ("scalar subquery: = (SELECT ...)", r"(=|<|>)\s*\(\s*select\b"),
    ("INSERT ... VALUES", r"\binsert\s+(into\s+)?\w+\s*\([\s\S]*?\)\s*values\b|\bvalues\s*\("),
    ("upsert (ON CONFLICT / MERGE)",
     r"\bon\s+conflict\b|\bon\s+duplicate\s+key\b|^\s*merge\b|\binsert\s+or\s+replace\b|^\s*replace\s+into\b|^\s*upsert\b"),
    ("several rows as {...}, {...} without [ ]", r"^(?![\s\S]*\[\s*\{)[\s\S]*?\}\s*,\s*\{"),
    ("UPDATE ... SET col = v", r"\bupdate\s+\w+\s+set\s+(?!\{)\w+"),
    ("col = v inside { }", r"\{\s*\w+\s*=(?!=)"),
    ("DELETE FROM", r"\bdelete\s+from\b"),
    ("SELECT DISTINCT", r"\bselect\s+distinct\b"),
    ("SELECT TOP", r"\bselect\s+top\b"),
    ("NOT IN", r"\bnot\s+in\b"),
    ("IN (SELECT ...)", r"\bin\s*\(\s*select\b"),
    ("IN (list)", r"\bin\s*\((?!\s*(get|select)\b)"),
    ("BETWEEN", r"\bbetween\b"),
    ("LIKE / ILIKE", r"\b(i?like)\b"),
    ("full-text search in WHERE (MATCH, @@)",
     r"\b\w+\s+match\s+(?=['\"$])|\bmatch\s*\(|@@|@search|\bto_tsvector\b|\bto_tsquery\b"),
    ("regex operators (=~, REGEXP, SIMILAR TO)", r"=~|!~|\bregexp\b|\brlike\b|\bsimilar\s+to\b"),
    ("string functions (contains, ends_with, ...)",
     r"\b(contains|ends_with|starts_with|endswith|startswith|ends|starts|position|strpos|instr|charindex)\b"),
    ("JSON operators (->>, ['key'], json_extract)",
     r"->>?|#>>?|\w\[\s*['\"]|\bjson_extract\b|\bjson_value\b|\bjsonb?_extract_path(_text)?\b"),
    ("CAST / ::", r"::|\bcast\s*\("),
    ("date functions", r"\b(extract|date_trunc|date_part|strftime|to_char|date|year|month|day)\s*\("),
    ("INTERVAL", r"\binterval\b"),
    ("CURRENT_DATE / CURRENT_TIMESTAMP", r"\bcurrent_(date|timestamp)\b"),
    ("ROUND()", r"\bround\s*\("),
    ("|| concatenation", r"\|\|"),
    ("NULLS FIRST / LAST", r"\bnulls\s+(first|last)\b"),
    ("LIMIT n, m", r"\blimit\s+\d+\s*,"),
    ("COUNT(column)", r"\bcount\s*\(\s*(?!\*|distinct\b)[a-z_][\w.]*\s*\)"),
    ("aggregate of an expression", r"\b(sum|avg|min|max|count)\s*\(\s*(coalesce|case|nullif|\w+\s*[-+*/])"),
    ("COALESCE", r"\bcoalesce\s*\("),
    ("CASE WHEN", r"\bcase\s+when\b"),
    ("COUNT(DISTINCT ...)", r"\bcount\s*\(\s*distinct\b"),
    ("GROUP BY", r"\bgroup\s+by\b"),
    ("HAVING", r"\bhaving\b"),
    ("ORDER BY", r"\border\s+by\b"),
    ("OFFSET", r"\boffset\b"),
    ("IS [NOT] NULL", r"\bis\s+(not\s+)?null\b"),
    ("<>", r"<>"),
    ("! for not", r"!(?!=)"),
    ("RETURNING", r"\breturning\b"),
    ("rowid", r"\browid\b"),
    ("table alias", r"\b(from|join)\s+\w+\s+(as\s+)?(?!(where|order|group|limit|lookup|join|left|right|inner|full|cross|on|offset|having|match|near|count|returning|facet|select|require|union)\b)[a-z_]\w*"),
    ("qualified name (t.col)", r"\b(?!meta\.)[a-z_]\w*\.[a-z_]\w*\b"),
    ("SELECT ... FROM", r"^\s*select\b[\s\S]*?\bfrom\b"),
    ("several statements", r";\s*\S"),
]

# Habits that bring others with them: a refusal at one of the second kind in
# a statement holding the first is put down to the first.
CARRIES = {
    "JOIN": {"table alias", "qualified name (t.col)", "comma join (FROM a, b)", "SELECT DISTINCT"},
    "comma join (FROM a, b)": {"table alias", "qualified name (t.col)"},
    "EXISTS": {"table alias", "qualified name (t.col)"},
}

FENECQL = [
    ("get", r"^\s*(explain\s+)?get\b"),
    ("put", r"^\s*put\b"),
    ("insert", r"^\s*insert\b(?![\s\S]*\bvalues\b)"),
    ("set", r"^\s*set\b"),
    ("del", r"^\s*del\b"),
    ("lookup", r"\blookup\b"),
    ("in (get ...)", r"\bin\s*\(\s*get\b"),
    ("in [list]", r"\bin\s*\["),
    ("match", r"\bmatch\b"),
    ("~ (contains)", r"~"),
    ("if absent else set", r"\bif\s+absent\b"),
    ("group (FenecQL)", r"\bgroup\s+(?!by\b)\w"),
    ("order (FenecQL)", r"\border\s+(?!by\b)\w"),
    ("count (suffix)", r"\bcount\s*$"),
    ("json path meta.x", r"\bmeta\.\w+"),
    ("require", r"\brequire\s+\d"),
]


def masked(statement):
    """The statement lowercased, each string literal's contents blanked out."""
    out = list(statement.lower())
    i, n = 0, len(statement)
    while i < n:
        q = statement[i]
        if q in "'\"":
            j = i + 1
            while j < n and statement[j] != q:
                j += 2 if statement[j] == "\\" else 1
            for k in range(i + 1, min(j, n)):
                out[k] = " "
            i = j + 1
        else:
            i += 1
    return "".join(out)


# Read in the literal itself: PostgreSQL's `~` is a regex match, FenecQL's a
# substring one, so `email ~ '@example\.org$'` runs and matches nothing.
REGEX_TILDE = ("~ given a regex (PostgreSQL's ~)", r"~\s*['\"][^'\"]*(\^|\$['\"]|\.\*|\(\?i\)|\\\.|\[)")


def matches(statement, table=SQL):
    m = masked(statement)
    found = []
    for name, pattern in table:
        for hit in re.finditer(pattern, m, re.M):
            found.append((name, hit.start(), hit.end()))
    if table is SQL:
        for hit in re.finditer(REGEX_TILDE[1], statement.lower()):
            found.append((REGEX_TILDE[0], hit.start(), hit.end()))
    return found


def constructs(statement, table=SQL):
    seen = []
    for name, _, _ in matches(statement, table):
        if name not in seen:
            seen.append(name)
    return seen


def _names():
    names = {"id"}
    for statement in seed.SCHEMA:
        m = re.match(r"create collection (\w+) \((.*)\)", statement)
        names.add(m.group(1))
        names.update(re.findall(r"(?:^|, )(\w+) ", m.group(2)))
    return names


NAMES = _names()

# The parser's own words, where they name the habit better than the place it
# stopped at.
FIRST = [
    (r"`in \(\.\.\.\)` takes a `get`", "IN (list)"),
    (r"`group` needs an aggregate", "GROUP BY without an aggregate (as DISTINCT)"),
    (r"answers under a name", "an expression in the select list without AS"),
    (r"`count` counts rows or distinct values", "COUNT(column)"),
    (r"`count` cannot be used together with", "count beside select"),
    (r"expected a single statement", "several statements"),
]
# The words, where no construct stands at the place.
LATER = [
    (r"has no full-text index", "match over a field without @text"),
    (r"^unknown function `(contains|ends_with|starts_with)`", "string functions (contains, ends_with, ...)"),
    (r"^unknown function `match`", "full-text search in WHERE (MATCH, @@)"),
    (r"^unknown function `json_", "JSON operators (->>, ['key'], json_extract)"),
    (r"^field `", "unknown field"),
    (r"^collection `", "unknown collection"),
    (r"refused: the \d+ statements", "out of attempts"),
    # `'DE'` inside the shell's own single quotes: the tool's quoting, not SQL.
    (r"unexpected character `\?`", "quotes lost in the shell"),
]


def blame(statement, status, body):
    """The one construct a refused statement is put down to."""
    try:
        error = json.loads(body).get("error", body)
    except Exception:
        error = body
    error = str(error)
    if status == 0:
        return "no answer from the server"
    found = matches(statement)
    names = {f[0] for f in found}
    detail = re.sub(r"^query error: (position \d+: )?", "", error)
    detail = re.sub(r"^query error: ", "", detail)
    pick = None
    quoted = re.search(r'expected a name, found "([^"]*)"', detail)
    if quoted and quoted.group(1) in NAMES:
        pick = "double-quoted identifier"
    for pattern, name in FIRST:
        if pick is None and re.search(pattern, detail):
            pick = name
    # A lookup's clauses belong to the collection looked up: a field of the
    # other level, or the key named on the wrong side, is not found there.
    if pick is None and re.search(r"\blookup\b", statement) and re.match(r"field `\w+\.\w+`", detail):
        pick = "lookup: a field of the other level"
    at = re.search(r"position (\d+)", error)
    if pick is None and at:
        p = int(at.group(1))
        here = [f for f in found if f[1] <= p <= f[2] + 1 and f[0] != "SELECT ... FROM"]
        # A parser that wanted a name after a keyword stops at the value
        # after it: `description MATCH "word"` is refused at the word.
        near = [f for f in found if f[2] < p <= f[2] + 16 and f[0] != "SELECT ... FROM"]
        if here or near:
            pick = max(here or near, key=lambda f: f[1])[0]
    if pick is None:
        for pattern, name in LATER:
            if re.search(pattern, detail):
                pick = name
                break
        if pick == "unknown field":
            for owner in ("qualified name (t.col)", "JSON operators (->>, ['key'], json_extract)", "rowid"):
                if owner in names:
                    pick = owner
                    break
    if pick is None:
        pick = "other: " + re.sub(r"`[^`]*`|\"[^\"]*\"|\d+", "_", detail)[:70]
    for carrier, carried in CARRIES.items():
        if pick in carried and carrier in names:
            return carrier
    return pick
