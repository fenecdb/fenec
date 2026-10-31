//! SQL's spellings, read by the one parser into the statements FenecQL
//! writes: each held to its FenecQL by the `Statement` both parse to, run
//! against a database, and refused where it has no exact meaning here.

use fenec_core::prelude::*;
use fenec_ql::{parse, parse_one};

/// Each SQL text and the FenecQL it is: the same statement, to the last
/// expression.
const SAME: &[(&str, &str)] = &[
    // INSERT
    (
        "INSERT INTO t (a, b) VALUES (1, 'x')",
        r#"insert t {a: 1, b: "x"}"#,
    ),
    (
        "insert t (a, b) values (1, 'x'), ($1, $2)",
        r#"insert t [{a: 1, b: "x"}, {a: $1, b: $2}]"#,
    ),
    (
        r#"INSERT INTO "t" ("a", meta) VALUES (now(), {k: 'v'}) RETURNING id, a"#,
        r#"insert t {a: now(), meta: {k: "v"}} returning id, a"#,
    ),
    (
        "INSERT INTO t (e, n) VALUES ('a@x', 1) ON CONFLICT DO NOTHING RETURNING *",
        r#"insert t {e: "a@x", n: 1} if absent returning *"#,
    ),
    (
        "INSERT INTO t (e, n) VALUES ('a@x', 1) ON CONFLICT DO UPDATE SET n = t.n + EXCLUDED.n, \
         seen = now()",
        r#"insert t {e: "a@x", n: 1} if absent else set {n: n + new.n, seen: now()}"#,
    ),
    (
        "insert into t (n) values (1) on conflict do nothing require 1",
        "insert t {n: 1} if absent require 1",
    ),
    // UPDATE
    (
        "UPDATE t SET a = 5, b = b + 1 WHERE c = 'x'",
        r#"set t {a: 5, b: b + 1} where c = "x""#,
    ),
    (
        "update t set owner = $1, run_at = now() + 30000 where run_at <= now() order by run_at \
         limit 1 returning *",
        "set t {owner: $1, run_at: now() + 30000} where run_at <= now() order run_at limit 1 \
         returning *",
    ),
    ("UPDATE t SET meta.k = 1", "set t {meta.k: 1}"),
    // IN
    ("get t where x IN (1, 2, 3)", "get t where x in [1, 2, 3]"),
    ("get t where x in ()", "get t where x in []"),
    (
        "get t where x NOT IN ($1, 'b')",
        r#"get t where not (x in [$1, "b"])"#,
    ),
    (
        "SELECT a FROM t WHERE x IN (SELECT f FROM c WHERE g = 1)",
        "get t select a where x in (get c select f where g = 1)",
    ),
    (
        "get t where x not in (get c select f)",
        "get t where not (x in (get c select f))",
    ),
    // BETWEEN, its `and` its own
    (
        "get t where x BETWEEN 1 AND 3",
        "get t where x >= 1 and x <= 3",
    ),
    (
        "get t where x between 1 and 3 and y = 2",
        "get t where (x >= 1 and x <= 3) and y = 2",
    ),
    (
        "get t where a = 1 or x between $1 and $2 + 1 and y = 2",
        "get t where a = 1 or ((x >= $1 and x <= $2 + 1) and y = 2)",
    ),
    (
        "get t where x NOT BETWEEN 1 AND 3",
        "get t where not (x >= 1 and x <= 3)",
    ),
    (
        "get t where not x between 1 and 3",
        "get t where not (x >= 1 and x <= 3)",
    ),
    // LIKE, a call
    (
        "get t where name LIKE 'A%'",
        r#"get t where like(name, "A%")"#,
    ),
    (
        "get t where name not ilike '%!_%' escape '!'",
        r#"get t where not ilike(name, "%!_%", "!")"#,
    ),
    // Text
    ("get t where n = 'it''s'", r#"get t where n = "it's""#),
    ("get t where n = ''", r#"get t where n = """#),
    (
        "get t select a || ' ' || b as ab",
        r#"get t select a + " " + b as ab"#,
    ),
    // Names quoted where only a name stands
    (r#"SELECT * FROM "users""#, "get users"),
    (
        r#"DELETE FROM "users" WHERE id = 1"#,
        "del users where id = 1",
    ),
    (
        r#"UPDATE "users" SET "n" = 1 RETURNING "n""#,
        "set users {n: 1} returning n",
    ),
    (r#"get t facet "b""#, "get t facet b"),
    // Rows as objects, the brackets left out
    ("put t {a: 1}, {a: 2}", "put t [{a: 1}, {a: 2}]"),
    (
        "insert into t values {a: 1}, {a: 'x'}",
        r#"insert t [{a: 1}, {a: "x"}]"#,
    ),
    // SQL's `is`
    (
        "get t where a is distinct from $1 and b is not distinct from 2",
        "get t where a != $1 and b = 2",
    ),
    ("get t where done is true", "get t where done = true"),
    (
        "get t where done is not false",
        "get t where not (done = false)",
    ),
    (
        "get t where a is not null order a desc nulls last, b nulls first",
        "get t where a is not null order a desc, b",
    ),
    // A `~` of text that is no pattern
    (
        "get t where email ~ '@example.org'",
        r#"get t where email ~ "@example.org""#,
    ),
    (
        r#"create index on t ("a") @hash"#,
        "create index on t (a) @hash",
    ),
    // SQL's keys by their place in the list.
    (
        "SELECT city, count(*) AS n FROM users GROUP BY 1 ORDER BY 2 DESC LIMIT 3",
        "get users select city, count(*) as n group city order n desc limit 3",
    ),
    // A name without `AS`, where the list goes on or ends at `FROM`.
    (
        "SELECT status s, count(*) n FROM orders GROUP BY status",
        "get orders select status as s, count(*) as n group status",
    ),
    // An aggregate's `FILTER`: what `case` leaves of it, a null passed over.
    (
        "SELECT count(*) FILTER (WHERE tier IS NULL) AS nulls, sum(total) FILTER (WHERE status \
         = 'paid') AS paid, count(DISTINCT c) FILTER (WHERE total > 5) AS buyers FROM orders",
        "get orders select count(case when tier is null then 1 end) as nulls, sum(case when \
         status = 'paid' then total end) as paid, count(distinct case when total > 5 then c \
         end) as buyers",
    ),
    (
        "SELECT name, age, age * 2 AS twice FROM users ORDER BY 2 DESC, 3, 1",
        "get users select name, age, age * 2 as twice order age desc, age * 2, name",
    ),
    // An alias alone: the names it qualifies, bare.
    ("SELECT * FROM users u WHERE u.id = 1", "get users where id = 1"),
    (
        "SELECT u.name, count(u.id) FROM users AS u WHERE u.age > 30 GROUP BY u.name ORDER BY \
         count(u.id) DESC",
        "get users select name, count(id) where age > 30 group name order count(id) desc",
    ),
    ("SELECT u.* FROM users u", "get users"),
    // DISTINCT, and GROUP BY with no aggregate: a group of the keys.
    (
        "SELECT DISTINCT city FROM users WHERE age > 1 ORDER BY city",
        "get users select city where age > 1 group city order city",
    ),
    (
        "SELECT city, country FROM users GROUP BY city, country",
        "get users select city, country group city, country",
    ),
    ("SELECT DISTINCT * FROM t", "get t"),
    (
        "SELECT DISTINCT city, count(*) FROM users GROUP BY city",
        "get users select city, count(*) group city",
    ),
    // EXISTS: the one equality with the row around it, the rest the
    // query's.
    (
        "SELECT * FROM users WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = users.id)",
        "get users where id in (get orders select user_id)",
    ),
    (
        "SELECT email FROM customers c WHERE NOT EXISTS (SELECT * FROM orders o WHERE \
         c.id = o.customer_id AND o.status = 'paid')",
        r#"get customers select email where not (id in (get orders select customer_id where status = "paid"))"#,
    ),
    (
        "DELETE FROM customers WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.customer_id = \
         customers.id LIMIT 1)",
        "del customers where not (id in (get orders select customer_id))",
    ),
    (
        "get customers where exists (select 1 from orders where customer_id = customers.id and \
         total > $1)",
        "get customers where id in (get orders select customer_id where total > $1)",
    ),
    // A query's one value: an aggregate, or `limit 1`.
    (
        "SELECT * FROM t WHERE at = (SELECT max(at) FROM t)",
        "get t where at in (get t select max(at))",
    ),
    (
        "UPDATE orders SET status = 'paid' WHERE id = (SELECT id FROM orders WHERE status = \
         'pending' ORDER BY placed LIMIT 1) RETURNING id",
        r#"set orders {status: "paid"} where id in (get orders select id where status = "pending" order placed limit 1) returning id"#,
    ),
    (
        "get t where (select count(*) from u) = n",
        "get t where n in (get u select count(*))",
    ),
    // A join whose other table is a filter: one table's rows, each once.
    (
        "SELECT o.id FROM orders o JOIN customers c ON o.customer_id = c.id WHERE c.email = $1 \
         ORDER BY o.placed DESC LIMIT 3",
        "get orders select id where customer_id in (get customers select id where email = $1) \
         order placed desc limit 3",
    ),
    (
        "SELECT SUM(ol.qty) AS total FROM order_lines ol INNER JOIN products p ON ol.product_id \
         = p.id WHERE p.category = 'lighting' AND ol.qty > 1",
        r#"get order_lines select sum(qty) as total where qty > 1 and product_id in (get products select id where category = "lighting")"#,
    ),
    (
        "SELECT DISTINCT c.name FROM customers c JOIN orders o ON c.id = o.customer_id WHERE \
         o.status = 'cancelled'",
        r#"get customers select name where id in (get orders select customer_id where status = "cancelled") group name"#,
    ),
    // The joined table's rows, the first's key read as theirs.
    (
        "SELECT c.id, COUNT(o.id) AS n FROM customers c JOIN orders o ON c.id = o.customer_id \
         WHERE c.country = 'IT' GROUP BY c.id",
        r#"get orders select customer_id as id, count(id) as n where customer_id in (get customers select id where country = "IT") group customer_id"#,
    ),
    (
        "select o.customer_id as customer_id, count(*) as n from orders o join customers c on \
         o.customer_id = c.id where c.country = $1 group by customer_id",
        "get orders select customer_id, count(*) as n where customer_id in (get customers select \
         id where country = $1) group customer_id",
    ),
    (
        "SELECT o.* FROM customers c, orders o WHERE o.customer_id = c.id AND c.country = 'IT'",
        r#"get orders where customer_id in (get customers select id where country = "IT")"#,
    ),
    // A LEFT JOIN's rows with none.
    (
        "SELECT c.email FROM customers c LEFT JOIN orders o ON c.id = o.customer_id WHERE o.id \
         IS NULL ORDER BY c.email",
        "get customers select email where not (id in (get orders select customer_id)) order email",
    ),
    // A LEFT JOIN by `id` reads each row of the first table once.
    (
        "SELECT o.total FROM orders o LEFT JOIN customers c ON c.id = o.customer_id",
        "get orders select total",
    ),
    // CREATE TABLE, each type as `fenec import` maps it.
    (
        "CREATE TABLE users (id serial PRIMARY KEY, name text NOT NULL, email varchar(200) \
         UNIQUE, age integer, score double precision, at timestamp with time zone, tags text[], \
         emb vector(3), meta jsonb, ok boolean)",
        "create collection users (name text required, email text @unique, age int, score float, \
         at timestamp, tags [text], emb vector<3>, meta json, ok bool)",
    ),
    (
        "create table if not exists p (sku text primary key, n bigint, h halfvec(4), s sparsevec(9))",
        "create collection if not exists p (sku text required @unique, n int, h vector<4, f16>, \
         s sparse<9>)",
    ),
    (
        "CREATE TABLE l (a INT, b INT, CONSTRAINT k PRIMARY KEY (a, b))",
        "create collection l (a int required, b int required, @unique(a, b))",
    ),
    (
        "CREATE TABLE l (id INTEGER PRIMARY KEY AUTOINCREMENT, a INT NULL)",
        "create collection l (a int)",
    ),
    (
        r#"CREATE TABLE u (id bigint GENERATED ALWAYS AS IDENTITY, email citext, name TEXT COLLATE "C", n TINYINT, d DATETIME, PRIMARY KEY (id), UNIQUE (name, n))"#,
        "create collection u (email text nocase, name text, n int, d timestamp, @unique(name, n))",
    ),
    (
        "create table v (title text collate tr, v vector<2>)",
        "create collection v (title text collate tr, v vector<2>)",
    ),
    // CREATE INDEX: btree, unique, hash, hnsw by its operator class, gin.
    ("CREATE INDEX users_age ON users (age)", "create index on users (age) @sorted"),
    ("create index on users (age desc nulls last)", "create index on users (age) @sorted"),
    ("CREATE UNIQUE INDEX ON users (email)", "create index on users (email) @unique"),
    ("CREATE INDEX ON t USING hash (k)", "create index on t (k) @hash"),
    (
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS ix ON t (a, b DESC)",
        "create index if not exists on t (a, b desc) @sorted",
    ),
    ("CREATE UNIQUE INDEX ON t (a, b)", "create index on t (a, b) @unique"),
    (
        "CREATE INDEX ON docs USING hnsw (emb vector_cosine_ops) WITH (m = 16, ef_construction \
         = 64)",
        "create index on docs (emb) @hnsw(cosine, m=16, ef_construction=64)",
    ),
    ("CREATE INDEX ON docs USING hnsw (emb)", "create index on docs (emb) @hnsw(l2)"),
    ("CREATE INDEX ON docs USING hnsw (emb halfvec_ip_ops)", "create index on docs (emb) @hnsw(dot)"),
    ("CREATE INDEX ON docs USING hnsw (s sparsevec_ip_ops)", "create index on docs (s) @inverted"),
    (
        "CREATE INDEX docs_body ON docs USING GIN (to_tsvector('english', body))",
        "create index on docs (body) @text",
    ),
    // SQL's arrays every way written: the length not held.
    (
        "CREATE TABLE t (a int ARRAY, b text array, c integer ARRAY[3], d int[], e text[4])",
        "create collection t (a [int], b [text], c [int], d [int], e [text])",
    ),
    // An `exists` asks no page: a `limit` from 1 changes nothing.
    (
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.id LIMIT 1)",
        "get t where id in (get u select k)",
    ),
];

/// `st` with what a join read without flat rows carries for the engine
/// set aside -- its names, held to its tables' fields, and whether its list
/// may be probed ([`fenec_core::query::Flat`]) -- which FenecQL has no
/// words for.
fn written(mut st: Statement) -> Statement {
    if let Statement::Select(s) = &mut st {
        if s.flat.as_ref().is_some_and(|f| f.columns.is_empty()) {
            s.flat = None;
        }
    }
    st
}

#[test]
fn sql_spellings_parse_to_the_fenecql_they_mean() {
    for (sql, fenecql) in SAME {
        let a = parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let b = parse_one(fenecql).unwrap_or_else(|e| panic!("{fenecql}: {e}"));
        assert_eq!(written(a), b, "{sql}");
    }
}

/// Each text refused, and what the refusal says.
const REFUSED: &[(&str, &str)] = &[
    // An insert that cannot say which field a value is
    ("insert into t values (1)", "an insert names its fields"),
    (
        "insert into t (a, b) values (1)",
        "a row of 1 values for 2 fields",
    ),
    (
        "insert into t (a) values (1), (2, 3)",
        "a row of 2 values for 1 fields",
    ),
    (
        "insert into t (a) select a from u",
        "not a query's rows: read them with `get`",
    ),
    (
        "insert into t select * from u",
        "not a query's rows: read them with `get`",
    ),
    // An upsert that does not map exactly
    (
        "insert into t (e) values (1) on conflict on constraint t_e do nothing",
        "an upsert names its unique field, or none",
    ),
    (
        "insert into t (e) values (1) on conflict do update set n = 1 where n < 3",
        "with no `where`",
    ),
    // The double-quoted name, text here
    (
        r#"select * from t where "name" = 'a'"#,
        r#"`name`, not "name": double quotes make text here, the same in every row"#,
    ),
    (r#"get t where "n" > $1"#, r#"`n`, not "n""#),
    (r#"get t where 'a' = "n""#, r#"`n`, not "n""#),
    (r#"get t where "n" = "m""#, r#"`n`, not "n""#),
    (r#"get t where "n" is not null"#, r#"`n`, not "n""#),
    (r#"get t where "n" in (1, 2)"#, r#"`n`, not "n""#),
    (r#"get t where "n" not in [$1]"#, r#"`n`, not "n""#),
    (r#"get t where "n" in (get u select m)"#, r#"`n`, not "n""#),
    (r#"get t where "n" like 'a%'"#, r#"`n`, not "n""#),
    (r#"get t where "n" between 1 and 2"#, r#"`n`, not "n""#),
    (r#"get t where "n" ~ 'a'"#, r#"`n`, not "n""#),
    (r#"get t where "n" has 'a'"#, r#"`n`, not "n""#),
    (r#"select "name" from t"#, r#"`name`, not "name""#),
    (r#"select "name" as n from t"#, r#"`name`, not "name""#),
    (r#"get t select a, "b""#, r#"`b`, not "b""#),
    (r#"get t order by "a" desc"#, r#"`a`, not "a""#),
    (r#"get t select count(*) group "a""#, r#"`a`, not "a""#),
    (r#"select max("at") from t"#, r#"`at`, not "at""#),
    (r#"select count(distinct "u") from t"#, r#"`u`, not "u""#),
    // SQL with no statement of its own here
    ("DROP TABLE t", "FenecQL writes `drop collection`"),
    (
        "ALTER TABLE t ADD COLUMN a int",
        "FenecQL writes `alter collection`",
    ),
    (
        "get orders join customers on id = customer_id",
        "FenecQL has no `join`",
    ),
    (
        "SELECT * FROM users u WHERE u.id = 1 v",
        "unknown command `v`",
    ),
    // A join whose meaning would change
    (
        "SELECT * FROM a RIGHT JOIN b ON a.id = b.a_id",
        "a RIGHT JOIN a LEFT JOIN with its tables the other way round",
    ),
    (
        "SELECT * FROM a FULL JOIN b ON a.id = b.a_id",
        "inner JOIN or a LEFT JOIN",
    ),
    (
        "SELECT * FROM a JOIN b",
        "a JOIN names the columns it pairs",
    ),
    (
        "SELECT * FROM a JOIN b ON a.x > b.y",
        "a JOIN pairs its columns by `=`",
    ),
    (
        "SELECT * FROM a JOIN b ON a.x > b.y WHERE a.k = 1",
        "a join pairs a column of each table",
    ),
    ("SELECT * FROM a, b", "a join pairs a column of each table"),
    (
        "SELECT a.x, b.y FROM a JOIN b ON b.a_id = a.id ORDER BY b.y",
        "put in order by the first table's columns",
    ),
    (
        "SELECT a.x, count(*) FROM a JOIN b ON b.a_id = a.id GROUP BY a.x",
        "an aggregate or `distinct` over a join reads one table's columns",
    ),
    (
        "SELECT DISTINCT a.x, b.y FROM a JOIN b ON b.a_id = a.id",
        "an aggregate or `distinct` over a join reads one table's columns",
    ),
    (
        "SELECT * FROM a LEFT JOIN b ON b.a_id = a.id WHERE b.x IS NULL",
        "reads the rows it kept with nulls",
    ),
    (
        "SELECT * FROM a LEFT JOIN b ON b.a_id = a.id AND a.k = 1",
        "a condition there on a table before it keeps that table's rows",
    ),
    (
        "SELECT * FROM a JOIN b ON b.a_id = a.id WHERE a.x = b.y",
        "a condition reads one table's columns",
    ),
    (
        "SELECT * FROM a JOIN b ON b.a_id = a.id JOIN c ON c.a_id = a.id",
        "a join hangs off the table just before it",
    ),
    (
        "SELECT * FROM a LEFT JOIN b ON b.a_id = a.id JOIN c ON c.b_id = b.id",
        "an inner JOIN after a LEFT JOIN",
    ),
    (
        "SELECT a.x * b.y AS z FROM a JOIN b ON b.a_id = a.id",
        "a column worked out of a row reads the first table's",
    ),
    (
        "SELECT * FROM a JOIN b ON b.a_id = a.id ORDER BY emb <-> $1 LIMIT 3",
        "a join takes SQL's clauses",
    ),
    (
        "SELECT name FROM users ORDER BY 3",
        "a key `3` names an item of the list by its place, and the list holds 1",
    ),
    (
        "SELECT DISTINCT ON (city) city, name FROM users",
        "`distinct on` is a group",
    ),
    // A subquery that would answer otherwise
    (
        "SELECT * FROM t WHERE x = (SELECT y FROM u)",
        "`= (select ...)` compares one value",
    ),
    (
        "SELECT * FROM t WHERE x > (SELECT avg(y) FROM u)",
        "a query stands after `in`",
    ),
    (
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x > t.y)",
        "an `exists` reads the row around it by one equality",
    ),
    (
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u)",
        "an `exists` reads the row around it by one equality",
    ),
    (
        "SELECT DISTINCT FROM t",
        "`distinct` makes each row of the list once",
    ),
    ("SELECT * FROM t ORDER BY 2", "`*` names none"),
    (
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.id LIMIT 0)",
        "an `exists` query's `limit` is a number from 1",
    ),
    (
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.k = t.id LIMIT $1)",
        "an `exists` query's `limit` is a number from 1",
    ),
    (
        "CREATE INDEX ON docs USING hnsw (emb vector_l2_ops) WITH (m = 1)",
        "pgvector's `m` is from 2 to 100",
    ),
    (
        "CREATE INDEX ON docs USING hnsw (emb) WITH (m = 16, ef_construction = 20)",
        "at least twice `m`",
    ),
    (
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = t.a AND u.b = t.b)",
        "an `exists` reads the row around it by one equality",
    ),
    (
        "SELECT (SELECT name FROM p WHERE id = l.pid) AS name, qty FROM l",
        "another collection's row beside each row is a join",
    ),
    // CREATE TABLE's rules fenecdb holds none of
    ("CREATE TABLE t (a numeric(10,2))", "fenecdb has no decimal"),
    ("CREATE TABLE t (a int DEFAULT 0)", "no `default`"),
    (
        "CREATE TABLE t (a int REFERENCES u(id))",
        "no `default`, `references`",
    ),
    (
        "CREATE TABLE t (a int, CHECK (a > 0))",
        "no `default`, `references`, `check`",
    ),
    (
        "CREATE TABLE t (a int, FOREIGN KEY (a) REFERENCES u(id))",
        "no `default`",
    ),
    (
        "CREATE TABLE t (id uuid PRIMARY KEY, a int)",
        "a row's `id` is an int fenecdb makes",
    ),
    (
        "CREATE TABLE t (id int, a int)",
        "a row's `id` is an int fenecdb makes",
    ),
    (
        "CREATE TABLE t (a serial, b int)",
        "a sequence counts a row's `id` alone",
    ),
    ("CREATE TABLE t (emb vector)", "`vector` has its dimension"),
    ("CREATE TABLE t (a interval)", "`interval` is no type here"),
    ("CREATE TABLE t (a)", "a column has a type"),
    (
        "CREATE TABLE t (a int GENERATED ALWAYS AS (1) STORED)",
        "no `default`",
    ),
    (
        "CREATE TABLE t (a text COLLATE \"de_DE\")",
        "unknown collation `de_DE`",
    ),
    (
        "CREATE TABLE t AS SELECT * FROM u",
        "a table is made of its columns",
    ),
    ("CREATE TEMP TABLE t (a int)", "kept in the file"),
    (
        "CREATE TABLE t (a int PRIMARY KEY, b int PRIMARY KEY)",
        "a table has one primary key",
    ),
    ("CREATE EXTENSION vector", "no extension to create"),
    // CREATE INDEX with no counterpart
    (
        "CREATE INDEX ON t USING ivfflat (emb vector_l2_ops)",
        "fenecdb's vector index is a graph",
    ),
    (
        "CREATE INDEX ON t (a) WHERE a > 1",
        "a partial index's `where`",
    ),
    ("CREATE UNIQUE INDEX ON t (a) @hash", "not both"),
    (
        "CREATE INDEX ON t USING gist (loc)",
        "an index is `using btree`",
    ),
    (
        "CREATE INDEX ON t USING hash (a, b)",
        "an index is `using btree`",
    ),
    (
        "CREATE INDEX ON t USING hnsw (emb vector_l1_ops)",
        "`vector_l1_ops`",
    ),
    ("CREATE INDEX ON t (lower(name))", "an index is over fields"),
    // pgvector's distance where it ranks nothing
    (
        "SELECT * FROM docs ORDER BY emb <-> $1 DESC LIMIT 3",
        "pgvector's distance orders alone, nearest first",
    ),
    (
        "SELECT * FROM docs ORDER BY emb <=> $1, id LIMIT 3",
        "pgvector's distance orders alone, nearest first",
    ),
    (
        "SELECT id, emb <-> $1 AS d FROM docs ORDER BY emb <-> $1 LIMIT 3",
        "`_score` the L2 distance for `<->`",
    ),
    (
        "SELECT * FROM docs WHERE emb <#> $1 < -0.5",
        "pgvector's distance ranks rows",
    ),
    (
        "UPDATE docs SET n = 1 ORDER BY emb <-> $1 LIMIT 1",
        "pgvector's distance ranks a `get`: `where id in (get ... near",
    ),
    (
        "SELECT * FROM docs WHERE MATCH(body) AGAINST ('rust')",
        "`get docs where ... match body $1`",
    ),
    (
        "SELECT * FROM docs WHERE to_tsvector(body) @@ to_tsquery('rust')",
        "full text is the `match` clause",
    ),
    (
        r"SELECT * FROM users WHERE email ~ '@example\.org$'",
        "not a regular expression",
    ),
    ("get users where name ~ '^Ad'", "not a regular expression"),
    ("get users where name ~ 'A.*a'", "not a regular expression"),
    (
        "get t where lower(a) between 'a' and 'm'",
        "`between` takes a field",
    ),
    ("get t order a nulls last", "a null comes first ascending"),
    ("TRUNCATE t", "`del <collection>`"),
    ("BEGIN", "a `/batch` of statements"),
    ("COMMIT", "a `/batch` of statements"),
];

#[test]
fn what_has_no_exact_meaning_is_refused_saying_what_to_write() {
    let mut wrong = Vec::new();
    for (sql, says) in REFUSED {
        match parse(sql) {
            Ok(s) => wrong.push(format!("{sql} parsed: {s:?}")),
            Err(e) if !e.to_string().contains(says) => wrong.push(format!("{sql}: {e}")),
            Err(_) => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Where a double-quoted text is no column in SQL either, or stands beside
/// a field, it stays text: FenecQL as it was written before.
#[test]
fn double_quoted_text_beside_a_field_stays_text() {
    for q in [
        r#"get t where name = "a""#,
        r#"get t where "a" = name"#,
        r#"get t where $1 = "admin" or owner = $2"#,
        r#"get t where tags has "ai" and title ~ "rust""#,
        r#"get t where name in ["a", "b"]"#,
        r#"get t where name like "a%""#,
        "get t where 'a' = 'a'",
        "get t where 1 = 1",
        r#"get t select coalesce(n, "none") as n, bucket(at, 1d, "Europe/Berlin") as day"#,
        r#"get t select 'x' as k"#,
        r#"get t match body "rust" limit 5"#,
        r#"put t {a: "x"}"#,
        r#"set t {a: "x"} where b = "y""#,
        r#"get t select highlight(body, "<b>", "</b>") match body "x""#,
    ] {
        parse(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

fn run(db: &mut Database, q: &str) -> Response {
    let mut last = Response::Affected(0);
    for s in parse(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        last = db
            .execute_with(&s, &[])
            .unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    last
}

fn texts(db: &mut Database, q: &str) -> Vec<String> {
    let Response::Rows(rs) = run(db, q) else {
        panic!("{q}: no rows")
    };
    rs.rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(t) => t.clone(),
            v => fenec_core::json::to_string(v),
        })
        .collect()
}

fn people() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection p (name text, email text nocase @unique, city text collate tr, \
         age int @sorted, n int)",
    );
    run(
        &mut db,
        "INSERT INTO p (name, email, city, age) VALUES \
         ('Ada', 'Ada@X.com', 'İzmir', 36), \
         ('Bob', 'bob@x.com', 'Ankara', 25), \
         ('O''Brien', 'ob@x.com', 'istanbul', 52), \
         ('Straße', 'st@x.com', null, null), \
         ('50% off', 'off@x.com', 'Ankara', 7), \
         ('naïve_x', 'nv@x.com', 'Iğdır', 19)",
    );
    db
}

/// `like` against what SQL answers: `%` any run, `_` one character -- of
/// several bytes too -- an escape named, a `nocase` field as its `=`, a
/// collated one by its bytes, `ilike` by the case fold.
#[test]
fn like_matches_as_sql_reads_a_pattern() {
    let mut db = people();
    let names = |db: &mut Database, w: &str| {
        texts(db, &format!("SELECT name FROM p WHERE {w} ORDER BY id"))
    };
    assert_eq!(names(&mut db, "name LIKE 'A%'"), ["Ada"]);
    assert_eq!(names(&mut db, "name LIKE 'a%'"), Vec::<String>::new());
    assert_eq!(names(&mut db, "name ILIKE 'a%'"), ["Ada"]);
    assert_eq!(names(&mut db, "name LIKE '_o_'"), ["Bob"]);
    assert_eq!(
        names(&mut db, "name LIKE 'na_ve!_x' ESCAPE '!'"),
        ["naïve_x"]
    );
    assert_eq!(names(&mut db, "name LIKE 'na_ve_x'"), ["naïve_x"]);
    assert_eq!(names(&mut db, "name LIKE '%!%%' ESCAPE '!'"), ["50% off"]);
    assert_eq!(
        names(&mut db, "name LIKE '%\\\\%%' ESCAPE '\\\\'"),
        ["50% off"]
    );
    assert_eq!(names(&mut db, "name LIKE 'O''%'"), ["O'Brien"]);
    assert_eq!(names(&mut db, "name LIKE '%'"), names(&mut db, "true"));
    assert_eq!(names(&mut db, "name LIKE '%e%' AND age > 20"), ["O'Brien"]);
    // `ilike` folds: `ß` is `ss`.
    assert_eq!(names(&mut db, "name ILIKE 'STRASSE'"), ["Straße"]);
    // A `nocase` field compares by its fold, as its `=` does.
    assert_eq!(names(&mut db, "email LIKE 'ada@%'"), ["Ada"]);
    assert_eq!(names(&mut db, "email = 'ADA@x.COM'"), ["Ada"]);
    // A collated field by its bytes, as its `=` does: `İ` is not `i`.
    assert_eq!(names(&mut db, "city LIKE 'i%'"), ["O'Brien"]);
    assert_eq!(names(&mut db, "city = 'izmir'"), Vec::<String>::new());
    // A null is no text: `like` leaves it out, `not like` -- `not` over a
    // test FenecQL answers false -- takes it in.
    assert_eq!(names(&mut db, "city LIKE '%'").len(), 5);
    assert_eq!(names(&mut db, "city NOT LIKE '%'"), ["Straße"]);
    // In a select list too, a null for a null.
    let Response::Rows(rs) = run(&mut db, "SELECT city LIKE 'A%' AS a FROM p ORDER BY id") else {
        panic!()
    };
    let got: Vec<Value> = rs.rows.iter().map(|r| r.values[0].clone()).collect();
    use Value::{Bool, Null};
    assert_eq!(
        got,
        [
            Bool(false),
            Bool(true),
            Bool(false),
            Null,
            Bool(true),
            Bool(false)
        ]
    );
}

#[test]
fn like_refuses_what_it_cannot_read() {
    let mut db = people();
    for (q, says) in [
        ("get p where age like '1%'", "`like` takes text, not int"),
        (
            "get p where name like 'a!' escape '!'",
            "ends in its escape",
        ),
        (
            "get p where name like 'a' escape '!!'",
            "takes one character",
        ),
        ("get p where name ilike 'a' escape 'X'", "with no case"),
        ("get p where email like 'a' escape 'X'", "with no case"),
    ] {
        let st = parse_one(q).unwrap();
        let e = db.execute_with(&st, &[]).unwrap_err().to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
    // Plain text takes a letter for its escape, before any character.
    let names = |db: &mut Database, w: &str| texts(db, &format!("get p select name where {w}"));
    assert_eq!(names(&mut db, "name like 'BXob' escape 'X'"), ["Bob"]);
    assert_eq!(
        names(&mut db, "name like 'BoX_' escape 'X'"),
        Vec::<String>::new()
    );
    assert_eq!(names(&mut db, "name like '%X%%' escape 'X'"), ["50% off"]);
}

/// The spellings run end to end, each answer what its FenecQL answers.
#[test]
fn sql_writes_and_reads_run() {
    let mut db = people();
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age BETWEEN 25 AND 36 ORDER BY age DESC"
        ),
        ["Ada", "Bob"]
    );
    // Inclusive both ends, through the ordered index as through a scan.
    let Response::Rows(rs) = run(&mut db, "explain get p where age between 25 and 36") else {
        panic!()
    };
    let plan = format!("{:?}", rs.rows);
    assert!(plan.contains("the ordered index on age"), "{plan}");
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age NOT BETWEEN 20 AND 40 ORDER BY id"
        ),
        ["O'Brien", "Straße", "50% off", "naïve_x"]
    );
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age IN (7, 52) ORDER BY id"
        ),
        ["O'Brien", "50% off"]
    );
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE age NOT IN (7, 52, 25, 36, 19) ORDER BY id"
        ),
        ["Straße"]
    );
    assert_eq!(
        texts(
            &mut db,
            "SELECT name FROM p WHERE id IN (SELECT id FROM p WHERE city = 'Ankara') ORDER BY id"
        ),
        ["Bob", "50% off"]
    );
    // An upsert, `excluded` the row offered, the collection's name the row
    // held.
    let up = "INSERT INTO p (email, n) VALUES ('ADA@x.com', 2) \
              ON CONFLICT DO UPDATE SET n = coalesce(p.n, 0) + excluded.n RETURNING name, n";
    assert_eq!(texts(&mut db, up), ["Ada"]);
    run(&mut db, up);
    assert_eq!(texts(&mut db, "SELECT n FROM p WHERE name = 'Ada'"), ["4"]);
    assert!(matches!(
        run(
            &mut db,
            "INSERT INTO p (email) VALUES ('bob@X.com') ON CONFLICT DO NOTHING"
        ),
        Response::Affected(0)
    ));
    assert_eq!(
        texts(
            &mut db,
            "UPDATE p SET n = 1, name = name || '!' WHERE city = 'Ankara' RETURNING name"
        ),
        ["Bob!", "50% off!"]
    );
    assert!(matches!(
        run(&mut db, "DELETE FROM p WHERE age < 20"),
        Response::Affected(2)
    ));
    // A json field's list of numbers in a row, read as written through
    // `parse_for`, as a server reads a text.
    run(&mut db, "alter collection p add field meta json");
    let st = fenec_ql::parse_one_for(
        &db,
        "INSERT INTO p (email, meta) VALUES ('m@x', {k: [1.5, 2]}) RETURNING meta",
    )
    .unwrap();
    let Response::Rows(rs) = db.execute_with(&st, &[]).unwrap() else {
        panic!()
    };
    assert_eq!(
        fenec_core::json::to_string(&rs.rows[0].values[0]),
        r#"{"k":[1.5,2]}"#
    );
}

/// `count(x)` counts the rows where `x` is not null, as SQL's does; and
/// what an agent reaches for that is not here is refused naming what is.
#[test]
fn count_of_a_value_and_the_names_that_are_not_fields() {
    let mut db = people();
    let Response::Rows(rs) = run(
        &mut db,
        "SELECT count(*) AS all_rows, count(age), count(city), count(distinct city) FROM p",
    ) else {
        panic!()
    };
    assert_eq!(
        rs.rows[0].values,
        [Value::Int(6), Value::Int(5), Value::Int(5), Value::Int(4)]
    );
    assert_eq!(
        rs.columns,
        [
            "all_rows",
            "count(age)",
            "count(city)",
            "count(distinct city)"
        ]
    );
    let Response::Rows(rs) = run(
        &mut db,
        "SELECT city, count(age) AS n FROM p GROUP BY city HAVING count(age) > 1 ORDER BY city",
    ) else {
        panic!()
    };
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(
        rs.rows[0].values,
        [Value::Text("Ankara".into()), Value::Int(2)]
    );
    for (q, says) in [
        ("SELECT rowid FROM p", "field `rowid`: a row's id is `id`"),
        ("get p where _id = 1", "field `_id`: a row's id is `id`"),
        (
            "SELECT * FROM p WHERE p.age > 30",
            "`p.age` reads into a json field, and a field is named bare",
        ),
        (
            "get p where contains(name, 'a')",
            "text is matched by `like '%a%'`",
        ),
    ] {
        let st = parse_one(q).unwrap();
        let e = db.execute_with(&st, &[]).unwrap_err().to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
}

/// A `like` in a filter is bound to its field (`Test::Pattern`), and
/// compared with `true` worked out by `eval`: over generated texts and
/// patterns, in a plain field, a `nocase` one, a Turkish `nocase` one and
/// a json one, with and without an escape, the two keep the same rows, and
/// a refusal is the same refusal.
#[test]
fn a_bound_like_answers_as_its_column_does() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection w (a text, b text nocase, c text collate tr nocase, j json)",
    );
    let pieces = [
        "a", "A", "ß", "SS", "İ", "i", "ı", "I", "ç", "%", "_", "!", "x", "𝔸",
    ];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut pick = move |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let mut word = |wild: bool| {
        let mut w = String::new();
        for _ in 0..pick(5) {
            match (wild, pick(6)) {
                (true, 0) => w.push('%'),
                (true, 1) => w.push('_'),
                _ => w.push_str(pieces[pick(pieces.len())]),
            }
        }
        w
    };
    let put = parse_one("put w {a: $1, b: $2, c: $3, j: $1}").unwrap();
    for _ in 0..300 {
        let row = [word(false), word(false), word(false)].map(Value::Text);
        db.execute_with(&put, &row).unwrap();
    }
    db.execute(&parse_one("put w {a: null}").unwrap()).unwrap();
    db.execute(&parse_one("put w {j: 7}").unwrap()).unwrap();
    let mut checked = 0;
    for _ in 0..150 {
        let pat = [Value::Text(word(true))];
        for f in ["a", "b", "c", "j"] {
            for op in ["like", "ilike", "not like"] {
                for esc in ["", " escape '!'", " escape 'ß'"] {
                    let cond = format!("{f} {op} $1{esc}");
                    let bound = parse_one(&format!("get w select id where {cond}")).unwrap();
                    let evaluated =
                        parse_one(&format!("get w select id where ({cond}) = true")).unwrap();
                    match (db.query(&bound, &pat), db.query(&evaluated, &pat)) {
                        (Ok(Response::Rows(k)), Ok(Response::Rows(e))) => {
                            assert_eq!(k.rows, e.rows, "{cond} {pat:?}");
                            checked += 1;
                        }
                        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{cond}"),
                        (a, b) => panic!("{cond} {pat:?}: {a:?} against {b:?}"),
                    }
                }
            }
        }
    }
    assert!(checked > 2_500, "{checked}");
}

/// What the agent eval found answered before the hint it needed, each
/// answered with that hint now.
#[test]
fn the_hint_an_agent_needs_comes_first() {
    let says = |q: &str, what: &str| {
        let e = parse(q)
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|e| e.to_string());
        assert!(e.contains(what), "{q}: {e}");
    };
    // A join the eval refused for its grouping, read now; one over both
    // tables' columns says what to write.
    for read in [
        "SELECT * FROM orders JOIN customers ON orders.cid = customers.id",
        "SELECT DISTINCT c.name FROM orders o JOIN customers c ON o.cid = c.id",
        "SELECT * FROM orders o, customers c WHERE o.cid = c.id",
        "SELECT u.city, count(*) FROM users u GROUP BY u.city",
    ] {
        parse(read).unwrap_or_else(|e| panic!("{read}: {e}"));
    }
    says(
        "SELECT c.name, count(*) FROM orders o JOIN customers c ON o.cid = c.id GROUP BY c.name",
        "`get o select k, count(*) as n group k lookup c on id = k select name`",
    );
    // PostgreSQL's `->>`.
    says(
        "SELECT * FROM t WHERE meta->>'source' = 'web'",
        "a json field's member is read by a path: `meta.source`",
    );
    // In an `in`'s query `distinct` changes nothing.
    assert_eq!(
        parse_one("get t where x in (select distinct a from u where b = 1)").unwrap(),
        parse_one("get t where x in (get u select a where b = 1)").unwrap()
    );
    // SQL's list refused for its own fault, a `from` ahead: read as
    // FenecQL's `select <collection>`, it was answered that FenecQL has
    // no `join`. And `->>` in the list as in a `where`.
    says(
        "SELECT status, sum(case when discount is null then 1 else 0 end) FROM orders GROUP BY \
         status",
        "answers under a name: `<expr> as <name>`",
    );
    says(
        "SELECT meta->>'source' AS s, count(*) AS n FROM t GROUP BY meta->>'source'",
        "a json field's member is read by a path: `meta.source`",
    );
}

/// `on conflict (k)` names the one unique key a row is found by, which the
/// engine holds it to: taken where it is the collection's one, refused
/// where the collection has another or a document names an id.
#[test]
fn a_conflict_target_is_the_one_unique_key() {
    let st = parse_one("INSERT INTO t (k) VALUES (1) ON CONFLICT (k) DO NOTHING").unwrap();
    let Statement::Put {
        pick, if_absent, ..
    } = &st
    else {
        panic!()
    };
    assert!(if_absent);
    assert_eq!(pick.as_ref().unwrap().conflict, Some(vec!["k".to_string()]));
    let mut db = Database::new();
    run(
        &mut db,
        "create collection p (sku text @unique, n int); create collection two (a text @unique, \
         b text @unique, n int); create collection plain (n int)",
    );
    let up = "INSERT INTO p (sku, n) VALUES ('x', 1) ON CONFLICT (sku) DO UPDATE SET n = p.n + \
              excluded.n RETURNING n";
    assert_eq!(texts(&mut db, up), ["1"]);
    assert_eq!(texts(&mut db, up), ["2"]);
    assert!(matches!(
        run(
            &mut db,
            "insert into plain (id, n) values (1, 1) on conflict (id) do nothing"
        ),
        Response::Affected(1)
    ));
    for (q, says) in [
        (
            "insert into p (sku) values ('y') on conflict (n) do nothing",
            "its rows are found by (sku)",
        ),
        (
            "insert into two (a, b) values ('a', 'b') on conflict (a) do nothing",
            "its rows are found by (a), (b)",
        ),
        (
            "insert into p (id, sku) values (9, 'z') on conflict (sku) do nothing",
            "(sku), (id)",
        ),
    ] {
        let e = db
            .execute_with(&parse_one(q).unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
}

/// A field a `lookup` level does not hold says the clauses after it are
/// that collection's -- where agents put the rows above's `where`.
#[test]
fn a_field_after_lookup_is_the_looked_up_collections() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection orders (total int); create collection items (order_id int @hash, \
         qty int); put orders {total: 10}; put items {order_id: 1, qty: 3}",
    );
    for q in [
        "get items lookup orders on id = order_id where qty > 1",
        "get items lookup orders on id = order_id select qty",
    ] {
        let e = db
            .execute_with(&parse_one(q).unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the clauses after `lookup` are the looked-up collection's"),
            "{q}: {e}"
        );
    }
    // And `json_extract`, the engine's to answer.
    let e = db
        .execute_with(
            &parse_one("get items where json_extract(qty, '$.a') = 1").unwrap(),
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("`meta.source`"), "{e}");
}

/// pgvector's `order by emb <op> $1 limit k` is `near`, its operator's
/// distance held to the index's: the same rows where they agree, refused
/// naming the operator to write where they do not. A row's `_score` is the
/// L2 distance `<->` orders by, 1 minus the cosine distance `<=>` does,
/// and the inner product `<#>` orders by turned round.
#[test]
fn pgvector_s_distance_is_near_where_the_index_measures_it() {
    use fenec_core::schema::Metric;
    for (sql, metric, fenecql) in [
        (
            "SELECT id FROM docs ORDER BY emb <-> $1 LIMIT 5",
            Metric::L2,
            "get docs select id near emb $1 limit 5",
        ),
        (
            "select * from docs where k = 1 order by $1 <=> emb asc limit 3",
            Metric::Cosine,
            "get docs where k = 1 near emb $1 limit 3",
        ),
        (
            "SELECT id FROM docs ORDER BY emb <#> '[1, 2.5, -3]' LIMIT 2",
            Metric::Dot,
            "get docs select id near emb [1, 2.5, -3] limit 2",
        ),
    ] {
        let Statement::Select(mut s) = parse_one(sql).unwrap() else {
            panic!()
        };
        let near = s.near.as_mut().unwrap();
        assert_eq!(near.metric.take(), Some(metric), "{sql}");
        assert_eq!(Statement::Select(s), parse_one(fenecql).unwrap(), "{sql}");
    }
    let mut db = Database::new();
    run(
        &mut db,
        "create collection d (emb vector<2> @hnsw(cosine), s sparse<4> @inverted, loc geo, k int)",
    );
    let put = fenec_ql::parse_one_for(
        &db,
        "put d [{emb: [1, 0], s: '{1:1}/4', loc: [0, 0], k: 1}, {emb: [0.6, 0.8], s: '{2:2}/4', \
         loc: [1, 1], k: 2}, {emb: [0, 1], s: '{1:0.5,2:1}/4', loc: [2, 2], k: 3}]",
    )
    .unwrap();
    db.execute(&put).unwrap();
    assert_eq!(
        texts(&mut db, "SELECT k FROM d ORDER BY emb <=> '[0, 1]' LIMIT 3"),
        texts(&mut db, "get d select k near emb [0, 1] limit 3")
    );
    assert_eq!(
        texts(&mut db, "SELECT k FROM d ORDER BY s <#> '{2:1}/4' LIMIT 2"),
        texts(&mut db, "get d select k near s '{2:1}/4' limit 2")
    );
    for (q, says) in [
        (
            "SELECT k FROM d ORDER BY emb <-> $1 LIMIT 3",
            "`emb <->` orders by the l2 distance, and its index measures cosine: `<=>`",
        ),
        (
            "SELECT k FROM d ORDER BY s <=> $1 LIMIT 3",
            "its index measures dot: `<#>`",
        ),
        (
            "SELECT k FROM d ORDER BY loc <-> $1 LIMIT 3",
            "`loc` holds points",
        ),
        (
            "SELECT k FROM d ORDER BY k <-> $1 LIMIT 3",
            "no vector index",
        ),
    ] {
        let st = parse_one(q).unwrap();
        let point = Value::List(vec![Value::Float(0.0), Value::Float(1.0)]);
        let v = match q.contains("loc") {
            true => point,
            false => Value::Vector(vec![0.0, 1.0]),
        };
        let e = db.execute_with(&st, &[v]).unwrap_err().to_string();
        assert!(e.contains(says), "{q}: {e}");
    }
}

/// SQL's `not null` is `required`: a write must give the field a value,
/// and a `null` given is none -- as `fenec types` and the declared
/// schema's `.notNull()` read it.
#[test]
fn a_required_field_takes_no_null() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (a text NOT NULL, b int); insert t {a: 'x', b: 1}",
    );
    for q in [
        "insert t {b: 2}",
        "insert t {a: null, b: 2}",
        "INSERT INTO t (a, b) VALUES (NULL, 3)",
        "UPDATE t SET a = NULL WHERE b = 1",
        "put t {a: null}",
    ] {
        let e = db
            .execute_with(&parse_one(q).unwrap(), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("field `a` is required"), "{q}: {e}");
    }
    assert_eq!(texts(&mut db, "get t select a"), ["x"]);
}

/// `call` and `statements` begin named statements and are no reserved
/// words: SQL reserves neither (PostgreSQL's `CALL` is a word any alias
/// takes), so each stays the alias it was before named statements, in a
/// table's place and a column's, with or without the `statements` feature.
#[test]
fn call_and_statements_are_still_aliases() {
    for word in ["call", "statements"] {
        for text in [
            "select a from t {w}",
            "select a {w} from t",
            "select count(*) {w} from t",
            "select c.a, {w}.b from t c join calls {w} on {w}.id = c.id",
            "select a from t {w} where a > 1",
        ] {
            let read = |w: &str| parse(&text.replace("{w}", w)).map(|s| format!("{s:?}"));
            let (with, other) = (read(word), read("other"));
            let other = other.unwrap_or_else(|e| panic!("{text} with `other`: {e}"));
            let with = with.unwrap_or_else(|e| panic!("{text} with `{word}`: {e}"));
            assert_eq!(with, other.replace("other", word), "{text} with `{word}`");
            assert_eq!(parse(&text.replace("{w}", word)).unwrap().len(), 1);
        }
    }
}
