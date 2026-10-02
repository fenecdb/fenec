// The schema's golden suite: integrations/schema-golden.json, held by every
// SDK that declares a schema in code, as builder-golden.json holds the
// builders. Three parts:
//
// - `declarations`: a declaration, as TypeScript writes it with
//   `@fenecdb/web/schema`, and the description it compiles to -- or the
//   refusal it meets. Another SDK writes each case in its own idiom and must
//   compile it to the same description, to the key: the description is what
//   the engine compares, and the one format every SDK speaks.
// - `texts`: a schema written as FenecQL, as any SDK may hand it over with
//   no builder of its own, and the statements an empty database is made
//   with -- every option written -- or the refusal it meets.
// - `plans`: a database, made by FenecQL statements, a description with its
//   migrations, and what the engine plans for them (`fenec_abi::schema`,
//   writing nothing): the statements, the migrations it would record, the
//   refusals with their messages. The Rust tests run every case
//   (crates/fenec-abi), and this file made them through the browser module:
//   the plan is the engine's, and one engine answers both.
//
//   node web/schema-golden.mjs           writes integrations/schema-golden.json
//   node web/schema-golden.mjs --check   fails if the file is not what it would write

import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Fenec, FenecError } from './fenec.js';
import * as schema from './schema.js';

export const GOLDEN = fileURLToPath(new URL('../integrations/schema-golden.json', import.meta.url));

/** A declaration's source, run with the builders in scope. */
function declare(source) {
  const names = Object.keys(schema);
  return new Function(...names, `return (${source});`)(...names.map((n) => schema[n]));
}

// ------------------------------------------------------------ declarations

const declarations = [];
const d = (name, source) => declarations.push({ name, source });

d('every type', `fenecTable('t', {
  a: text(), b: integer(), c: doublePrecision(), d: boolean(), e: timestamp(), f: bytea(), g: json(),
  h: vector({ dimensions: 768 }), i: halfvec({ dimensions: 384 }), j: sparsevec({ dimensions: 30522 }),
  k: text().array(), l: integer().array(), m: timestamp().array(),
})`);
d('a required field', `fenecTable('t', { title: text().notNull(), n: integer().notNull() })`);
d('a collation', `fenecTable('t', { a: text().collate('tr'), b: text().collate('und').notNull(), c: text().collate('und').array() })`);
d('a unique column', `fenecTable('t', { email: text().unique().notNull() })`);
d('an ordered index', `fenecTable('t', { at: timestamp() }, (t) => [index('t_at').on(t.at)])`);
d('a btree is ordered', `fenecTable('t', { n: integer() }, (t) => [index('t_n').using('btree', t.n)])`);
d('a hash index', `fenecTable('t', { n: integer() }, (t) => [index('t_n').using('hash', t.n)])`);
d('a unique index', `fenecTable('t', { s: text() }, (t) => [uniqueIndex('t_s').on(t.s)])`);
d('an expiry', `fenecTable('t', { seen: timestamp() }, (t) => [index('t_seen').on(t.seen).ttl('30m')])`);
d('an expiry in days', `fenecTable('t', { seen: timestamp() }, (t) => [index().on(t.seen).ttl('7d')])`);
d('an expiry in hours', `fenecTable('t', { seen: timestamp() }, (t) => [index().on(t.seen).ttl('12h')])`);
d('an expiry in seconds', `fenecTable('t', { seen: timestamp() }, (t) => [index().on(t.seen).ttl('45s')])`);
d('an expiry in milliseconds', `fenecTable('t', { seen: timestamp() }, (t) => [index().on(t.seen).ttl('1500ms')])`);
d('an expiry written in a smaller unit', `fenecTable('t', { seen: timestamp() }, (t) => [index().on(t.seen).ttl('120m')])`);
d('hnsw by cosine', `fenecTable('t', { e: vector({ dimensions: 3 }) }, (t) => [index('t_e').using('hnsw', t.e.op('vector_cosine_ops'))])`);
d('hnsw by l2, its options', `fenecTable('t', { e: vector({ dimensions: 3 }) }, (t) => [
  index('t_e').using('hnsw', t.e.op('vector_l2_ops')).with({ m: 8, ef_construction: 64, ef_search: 40 }),
])`);
d('hnsw by inner product over halfvec, int8', `fenecTable('t', { e: halfvec({ dimensions: 3 }) }, (t) => [
  index('t_e').using('hnsw', t.e.op('halfvec_ip_ops')).with({ quant: 'int8' }),
])`);
d('hnsw over bit codes', `fenecTable('t', { e: vector({ dimensions: 8 }) }, (t) => [index().using('hnsw', t.e).with({ quant: 'bit' })])`);
d('bm25', `fenecTable('t', { body: text() }, (t) => [index('t_body').using('bm25', t.body)])`);
d('bm25, its options', `fenecTable('t', { body: text() }, (t) => [
  index('t_body').using('bm25', t.body).with({ k1: 1.2, b: 0.75, prefix: 6, prefix_min: 4, chars: true }),
])`);
d('an inverted index', `fenecTable('t', { s: sparsevec({ dimensions: 100 }) }, (t) => [index('t_s').using('inverted', t.s)])`);
d('indexes on paths', `fenecTable('docs', { meta: json() }, (t) => [
  index('docs_lang').using('hash', t.meta.path('lang')),
  uniqueIndex('docs_slug').on(t.meta.path('slug')),
  index('docs_rank').on(t.meta.path('source.rank')),
])`);
d('a name past ASCII', `fenecTable('kişiler', { 'ad': text(), 'yaş': integer() })`);
d('two indexes on a field', `fenecTable('t', { a: text().unique() }, (t) => [index().on(t.a)])`);
d('two indexes of one name', `fenecTable('t', { a: text(), b: text() }, (t) => [index('x').on(t.a), index('x').on(t.b)])`);
d('an id declared', `fenecTable('t', { id: integer() })`);
d('a name FenecQL does not read', `fenecTable('t', { 'a-b': integer() })`);
d('no column', `fenecTable('t', {})`);
d('a default', `fenecTable('t', { n: integer().default(0) })`);
d('a primary key', `fenecTable('t', { n: integer().primaryKey() })`);
d('a collation of a number', `fenecTable('t', { n: integer().collate('tr') })`);
d('a list of json', `fenecTable('t', { m: json().array() })`);
d('an index on two columns', `fenecTable('t', { a: text(), b: text() }, (t) => [index().on(t.a, t.b)])`);
d('an expiry of a hash', `fenecTable('t', { at: timestamp() }, (t) => [index().using('hash', t.at).ttl('1h')])`);
d('an expiry of a number', `fenecTable('t', { n: integer() }, (t) => [index().on(t.n).ttl('1h')])`);
d('hnsw over text', `fenecTable('t', { a: text() }, (t) => [index().using('hnsw', t.a)])`);
d('bm25 over a number', `fenecTable('t', { n: integer() }, (t) => [index().using('bm25', t.n)])`);
d('an inverted index over a vector', `fenecTable('t', { e: vector({ dimensions: 3 }) }, (t) => [index().using('inverted', t.e)])`);
d('a bad duration', `fenecTable('t', { at: timestamp() }, (t) => [index().on(t.at).ttl('soon')])`);
d('a collation there is not', `fenecTable('t', { a: text().collate('fr') })`);
d('a dimension of none', `fenecTable('t', { e: vector({ dimensions: 0 }) })`);
d('an option hnsw takes not', `fenecTable('t', { e: vector({ dimensions: 3 }) }, (t) => [index().using('hnsw', t.e).with({ prefix: 3 })])`);

// ------------------------------------------------------------------- texts

const texts = [];
const x = (name, fenecql) => texts.push({ name, fenecql });
x('a collection', 'create collection t (a text required, b int @hash)');
x('every index, options defaulted', `create collection t (
  a text @text, b int @sorted, c text @unique, d timestamp @ttl(1h), e vector<3> @hnsw(dot), f sparse<9> @inverted, g json
)
create index on t (g.lang) @hash`);
x('two collections, comments between', `-- the schema
create collection a (x int)
-- and another
create collection b (y text collate und)`);
x('an index after its collection', 'create collection t (a int)\ncreate index on t (a) @sorted');
x('a collection twice', 'create collection t (a int)\ncreate collection t (b int)');
x('a field twice', 'create collection t (a int, a text)');
x('an id declared', 'create collection t (id int)');
x('two indexes on a field', 'create collection t (a int @hash)\ncreate index on t (a) @sorted');
x('an index on no collection', 'create index on t (a) @hash');
x('an index on no field', 'create collection t (a int)\ncreate index on t (b) @hash');
x('an expiry of a number', 'create collection t (a int @ttl(1h))');
x('hnsw over text', 'create collection t (a text @hnsw(cosine))');
x('a bad duration', 'create collection t (a timestamp @ttl(soon))');
x('a collation there is not', 'create collection t (a text collate fr)');
x('a write in a schema', 'create collection t (a int)\nput t {a: 1}');
x('a path into a number', 'create collection t (a int)\ncreate index on t (a.b) @hash');

// ------------------------------------------------------------------- plans

const plans = [];
const p = (name, db, source, migrations = []) => plans.push({ name, db, source, migrations });
const TODOS = 'create collection todos (title text required, done bool @hash)';
const todos = `fenecTable('todos', { title: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)])`;

p('an empty database is made', [], `{ todos: ${todos} }`);
p('the same schema plans nothing', [TODOS], `{ todos: ${todos} }`);
p('a collection is added', [TODOS], `{ todos: ${todos}, tags: fenecTable('tags', { name: text().unique() }) }`);
p('a field and its indexes are added', [TODOS], `{ todos: fenecTable('todos', {
  title: text().notNull(), done: boolean(), at: timestamp(), meta: json(),
}, (t) => [index().using('hash', t.done), index().on(t.at), index().using('hash', t.meta.path('lang'))]) }`);
p('an index is added where there is none', [TODOS], `{ todos: fenecTable('todos', { title: text().notNull(), done: boolean() }, (t) => [
  index().using('hash', t.done), index().using('bm25', t.title),
]) }`);
p('a field the code lacks', [TODOS], `{ todos: fenecTable('todos', { title: text().notNull() }) }`);
p('a field renamed, it seems', [TODOS], `{ todos: fenecTable('todos', { name: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`);
p('a rename, as a migration', [TODOS, 'put todos {title: "a"}'], `{ todos: fenecTable('todos', { name: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, [
  "rename('todos', 'title', 'name')",
]);
p('a type changed', [TODOS], `{ todos: fenecTable('todos', { title: integer().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`);
p('a field made required', ['create collection todos (title text, done bool @hash)'], `{ todos: ${todos} }`);
p('a field no longer required', [TODOS], `{ todos: fenecTable('todos', { title: text(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`);
p('a required field added', [TODOS], `{ todos: fenecTable('todos', { title: text().notNull(), done: boolean(), due: timestamp().notNull() }, (t) => [index().using('hash', t.done)]) }`);
p('a collation changed', [TODOS], `{ todos: fenecTable('todos', { title: text().notNull().collate('tr'), done: boolean() }, (t) => [index().using('hash', t.done)]) }`);
p('an index taken off', [TODOS], `{ todos: fenecTable('todos', { title: text().notNull(), done: boolean() }) }`);
p('an index changed', [TODOS], `{ todos: fenecTable('todos', { title: text().notNull(), done: boolean().unique() }) }`);
p('an index changed, rebuilt', [TODOS, 'put todos [{title: "a", done: true}, {title: "b", done: false}]'], `{ todos: fenecTable('todos', { title: text().notNull(), done: boolean().unique() }) }`, [
  "rebuild('todos', 'done')",
]);
p('an expiry changed in place', ['create collection s (seen timestamp @ttl(30m))'], `{ s: fenecTable('s', { seen: timestamp() }, (t) => [index().on(t.seen).ttl('1h')]) }`);
p('hnsw options changed', ['create collection v (e vector<3> @hnsw(cosine))'], `{ v: fenecTable('v', { e: vector({ dimensions: 3 }) }, (t) => [index().using('hnsw', t.e.op('vector_cosine_ops')).with({ m: 32 })]) }`);
p('a path index taken off', ['create collection docs (meta json)', 'create index on docs (meta.lang) @hash'], `{ docs: fenecTable('docs', { meta: json() }) }`);
p('a collection the code lacks is left as it is', [TODOS, 'create collection gone (x int)'], `{ todos: ${todos} }`);
p('a dropped collection, as a migration', [TODOS, 'create collection gone (x int)'], `{ todos: ${todos} }`, ["dropTable('gone')"]);
p('the database\'s own collections are its own', [TODOS, 'create collection _idempotency (key text)'], `{ todos: ${todos} }`);
p('a field dropped before is declared again, a new field', [TODOS, 'alter collection todos drop field done'], `{ todos: ${todos} }`);
p('a field is loosened: a rebuild', [TODOS, 'put todos {title: "a"}'], `{ todos: fenecTable('todos', { title: text(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, ["rebuild('todos', 'title')"]);
p('migrations run in order', [TODOS, 'put todos {title: "a"}'], `{ todos: fenecTable('todos', { label: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, [
  "rename('todos', 'title', 'name')",
  "rename('todos', 'name', 'label')",
]);
p('a migration is refused', [TODOS], `{ todos: ${todos} }`, ["'compact'"]);
p('a migration fails, and none of them lands', [TODOS], `{ todos: fenecTable('todos', { name: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, [
  "rename('todos', 'title', 'name')",
  "dropTable('nothing')",
]);
const RECORDED = [
  'create collection _migrations (n int @unique, text text, at timestamp)',
  'put _migrations [{n: 1, text: "alter collection todos rename field title to name"}, {n: 2, text: "drop collection gone"}]',
];
p('an older app opens what a newer one migrated', ['create collection todos (name text required, done bool @hash)', ...RECORDED], `{ todos: fenecTable('todos', { name: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, [
  "rename('todos', 'title', 'name')",
]);
p('a migration edited after it ran', ['create collection todos (name text required, done bool @hash)', ...RECORDED], `{ todos: fenecTable('todos', { name: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, [
  "rename('todos', 'title', 'label')",
  "dropTable('gone')",
]);
p('the migrations recorded, nothing runs', ['create collection todos (name text required, done bool @hash)', ...RECORDED], `{ todos: fenecTable('todos', { name: text().notNull(), done: boolean() }, (t) => [index().using('hash', t.done)]) }`, [
  "rename('todos', 'title', 'name')",
  "dropTable('gone')",
]);
p('an empty database records its migrations, and runs none', [], `{ todos: ${todos} }`, ["rename('todos', 'name', 'title')", "dropTable('gone')"]);
p('an empty database, several collections', [], `{ a: fenecTable('a', { x: integer() }), b: fenecTable('b', { y: text().notNull() }, (t) => [index().on(t.y)]) }`);
p('an empty database holding only its own', ['create collection _idempotency (key text)'], `{ todos: ${todos} }`);
p('every difference at once', [TODOS, 'create collection gone (x int)'], `{ todos: fenecTable('todos', { name: integer(), done: boolean().notNull() }) }`);

// ------------------------------------------------------------------ writing

async function generate() {
  const out = { declarations: [], plans: [] };
  for (const { name, source } of declarations) {
    try {
      const table = declare(source);
      out.declarations.push({ name, source, collection: JSON.parse(JSON.stringify(table)), fenecql: table.toFenecQL() });
    } catch (e) {
      if (!(e instanceof FenecError)) throw e;
      out.declarations.push({ name, source, error: e.message });
    }
  }
  const wasm = await readFile(new URL('./fenec.wasm', import.meta.url));
  // The plan an empty or a made database gets, or the refusal: written
  // without `kind` and `applied`, which a plan always says alike.
  const planned = async (statements, description) => {
    const db = await Fenec.open(wasm);
    try {
      for (const sql of statements) db.run(sql);
      const { kind, applied, ...plan } = db.checkSchema(description, 'plan');
      void kind;
      void applied;
      return { plan };
    } catch (e) {
      if (!(e instanceof FenecError)) throw e;
      return { error: e.message };
    } finally {
      db.close();
    }
  };
  out.texts = [];
  for (const { name, fenecql } of texts) {
    const r = await planned([], { format: 1, fenecql });
    out.texts.push({ name, fenecql, ...(r.plan ? { statements: r.plan.statements } : r) });
  }
  for (const { name, db: statements, source, migrations } of plans) {
    const tables = declare(source);
    const description = schema.describe(tables, migrations.map((m) => declare(m)));
    // The module reads a schema as FenecQL; the Rust tests read both.
    const fenecql = schema.toFenecQL(tables);
    const r = await planned(statements, { ...description, collections: undefined, fenecql });
    out.plans.push({ name, db: statements, source, migrations, description, fenecql, ...r });
  }
  return out;
}

/** The file's text: one case a line, so a change reads as the cases it moved. */
export function format(out) {
  const part = (list) => `[\n${list.map((k) => `    ${JSON.stringify(k)}`).join(',\n')}\n  ]`;
  return `{\n  "format": 1,\n  "declarations": ${part(out.declarations)},\n  "texts": ${part(out.texts)},\n  "plans": ${part(out.plans)}\n}\n`;
}

export async function text() {
  for (const list of [declarations, texts, plans]) {
    if (new Set(list.map((k) => k.name)).size !== list.length) throw new Error('two cases share a name');
  }
  return format(await generate());
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const made = await text();
  if (process.argv.includes('--check')) {
    const have = await readFile(GOLDEN, 'utf8').catch(() => '');
    if (have !== made) {
      console.error(`${GOLDEN} is not what the declarations and the engine make: make schema-golden`);
      process.exit(1);
    }
  } else {
    await writeFile(GOLDEN, made);
    console.log(`${declarations.length} declarations, ${texts.length} texts, ${plans.length} plans -> ${GOLDEN}`);
  }
}
