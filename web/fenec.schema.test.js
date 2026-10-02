// A schema declared in code (`schema.js`): what each declaration makes,
// what it is refused at declaration, and the check at an open -- what is
// applied, what is refused and how the migrations run -- through the module,
// which decides it (`fenec_core::declared`). The `fenec types --schema`
// round trip and the type equality's source file need the `fenec` binary
// (cargo build), and are skipped without it.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFile, writeFile, access, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Fenec, FenecError, from } from './fenec.js';
import {
  fenecTable,
  collection,
  text,
  integer,
  doublePrecision,
  boolean,
  timestamp,
  bytea,
  json,
  vector,
  halfvec,
  sparsevec,
  index,
  uniqueIndex,
  defineRelations,
  describe,
  toFenecQL,
  getColumns,
  rename,
  drop,
  dropTable,
  rebuild,
} from './schema.js';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);
const skip = wasm ? false : 'no web/fenec.wasm (make wasm)';
const fenec = await (async () => {
  for (const p of ['../target/debug/fenec', '../target/release/fenec']) {
    const url = new URL(p, import.meta.url);
    if (await access(url).then(() => true, () => false)) return url.pathname;
  }
  return null;
})();

/** A schema and its migrations as the engine takes them: FenecQL, as an open hands them over. */
const desc = (schema, migrations = []) => ({ format: 1, fenecql: toFenecQL(schema), migrations });

/** The statements an empty database would be made with. */
async function made(schema) {
  const db = await Fenec.open(wasm);
  try {
    return db.checkSchema(desc(schema), 'plan').statements;
  } finally {
    db.close();
  }
}

test('every column and modifier declares what FenecQL does', { skip }, async () => {
  const all = fenecTable(
    'all',
    {
      a: text().notNull().collate('tr'),
      b: integer(),
      c: doublePrecision(),
      d: boolean(),
      e: timestamp(),
      f: bytea(),
      g: json(),
      h: halfvec({ dimensions: 4 }),
      i: sparsevec({ dimensions: 100 }),
      j: text().array(),
      k: integer().array().notNull(),
      u: text().unique(),
      v: vector({ dimensions: 3 }),
      w: text().collate('und').array(),
    },
    (t) => [
      index('all_a').using('bm25', t.a).with({ prefix: 6, chars: true }),
      index('all_b').using('hash', t.b),
      index('all_c').on(t.c),
      index('all_e').on(t.e).ttl('90m'),
      index('all_h').using('hnsw', t.h.op('halfvec_l2_ops')).with({ m: 8, ef_construction: 50, ef_search: 33, quant: 'int8' }),
      index('all_i').using('inverted', t.i),
      index('all_j').using('hash', t.j),
      index('all_v').using('hnsw', t.v.op('vector_ip_ops')),
      index('all_lang').using('hash', t.g.path('lang')),
      uniqueIndex('all_slug').on(t.g.path('meta.slug')),
      index('all_rank').on(t.g.path('src.rank')),
    ],
  );
  assert.deepEqual(await made({ all }), [
    'create collection all (a text required collate tr @text(k1=0.9, b=0.4, prefix=6, chars), ' +
      'b int @hash, c float @sorted, d bool, e timestamp @ttl(90m), f bytes, g json, ' +
      'h vector<4, f16> @hnsw(l2, m=8, ef_construction=50, ef_search=33, quant=int8), ' +
      'i sparse<100> @inverted, j [text] @hash, k [int] required, u text @unique, ' +
      'v vector<3> @hnsw(dot, m=16, ef_construction=200, ef_search=100), w [text] collate und)',
    'create index on all (g.lang) @hash',
    'create index on all (g.meta.slug) @unique',
    'create index on all (g.src.rank) @sorted',
  ]);
  // The description writes what the declaration says, and no default.
  assert.deepEqual(JSON.parse(JSON.stringify(all)).fields.slice(0, 2), [
    { name: 'a', type: 'text', required: true, collate: 'tr', index: { kind: 'text', prefix: 6, chars: true } },
    { name: 'b', type: 'int', index: { kind: 'hash' } },
  ]);
  // A quantized bit index searches the wider beam by default; a bm25's k1
  // and b are its own when given.
  const q = fenecTable('q', { v: vector({ dimensions: 8 }), s: text() }, (t) => [
    index().using('hnsw', t.v).with({ quant: 'bit' }),
    index().using('bm25', t.s).with({ k1: 1.2, b: 0.75, prefix: 5, prefix_min: 4 }),
  ]);
  assert.deepEqual(await made({ q }), [
    'create collection q (v vector<8> @hnsw(cosine, m=16, ef_construction=200, ef_search=200, quant=bit), ' +
      's text @text(k1=1.2, b=0.75, prefix=5, prefix_min=4))',
  ]);
  assert.equal(collection, fenecTable);
  assert.deepEqual(Object.keys(getColumns(all)), 'abcdefghijkuvw'.split(''));
});

test('what fenecdb cannot hold is refused as it is declared', () => {
  const refused = (fn, why) => assert.throws(fn, (e) => e instanceof FenecError && why.test(e.message), String(why));
  refused(() => fenecTable('t', { a: text().unique() }, (t) => [index().on(t.a)]), /two indexes: fenecdb takes one a field/);
  refused(() => fenecTable('t', { a: text() }, (t) => [index('x').on(t.a), index('x').using('hash', t.a)]), /names two indexes x/);
  refused(() => fenecTable('t', { id: integer() }), /an `id` of its own/);
  refused(() => fenecTable('t', { 'a b': integer() }), /not a FenecQL name/);
  refused(() => fenecTable('t', {}), /declares no column/);
  refused(() => text('title'), /takes no name/);
  refused(() => integer().default(0), /no column defaults/);
  refused(() => integer().primaryKey(), /an `id` of its own/);
  refused(() => integer().references(), /no foreign keys/);
  refused(() => integer().collate('tr'), /orders text/);
  refused(() => text().collate('fr'), /unknown collation/);
  refused(() => json().array(), /json column holding a list/);
  refused(() => vector({ dimensions: 0 }), /dimensions/);
  refused(() => vector({ dimensions: 3 }).op('vector_hamming_ops'), /unknown operator class/);
  refused(() => fenecTable('t', { a: text(), b: text() }, (t) => [index().on(t.a, t.b)]), /one field/);
  refused(() => fenecTable('t', { a: text() }, (t) => [index().using('gin', t.a)]), /unknown index method/);
  refused(() => fenecTable('t', { a: text() }, (t) => [index().using('hash', t.a).ttl('1h')]), /ttl\(\) expires/);
  refused(() => fenecTable('t', { a: timestamp() }, (t) => [index().on(t.a).ttl('soon')]), /a duration/);
  refused(() => fenecTable('t', { a: text() }, (t) => [index().using('bm25', t.a).with({ m: 3 })]), /take no m/);
  // A relation lookup cannot answer: the child's key has no index.
  const p = fenecTable('p', { name: text() });
  const c = fenecTable('c', { pid: integer() });
  refused(() => defineRelations({ p, c }, (r) => ({ p: { c: r.many.c({ from: r.p.id, to: r.c.pid }) } })), /no hash or unique index/);
  refused(() => rename('t', 'a', 'b c'), /not a FenecQL name/);
});

test('a relation is a lookup, its key and kind its own', () => {
  const products = fenecTable('products', { name: text(), sku: text().unique() });
  const reviews = fenecTable('reviews', { productId: integer(), sku: text(), stars: integer() }, (t) => [
    index().using('hash', t.productId),
  ]);
  const relations = defineRelations({ products, reviews }, (r) => ({
    products: { reviews: r.many.reviews({ from: r.products.id, to: r.reviews.productId }) },
    reviews: { product: r.one.products({ from: r.reviews.sku, to: r.products.sku }) },
  }));
  const q = (t) => from(t).toFenecQL()[0];
  // The builder takes a table wherever it takes a name.
  assert.equal(q(products), 'get products');
  const db = { relations };
  void db;
  assert.deepEqual(relations, {
    products: { reviews: { collection: 'reviews', on: 'productId' } },
    reviews: { product: { collection: 'products', on: 'sku', parentKey: 'sku', limit: 1 } },
  });
});

test('a relation is looked up by its name, on a database opened with it', { skip }, async () => {
  const products = fenecTable('products', { name: text() });
  const reviews = fenecTable('reviews', { productId: integer(), stars: integer() }, (t) => [index().using('hash', t.productId)]);
  const relations = defineRelations({ products, reviews }, (r) => ({
    products: { reviews: r.many.reviews({ from: r.products.id, to: r.reviews.productId }) },
    reviews: { product: r.one.products({ from: r.reviews.productId, to: r.products.id }) },
  }));
  const db = await Fenec.open(wasm, { schema: { products, reviews }, relations });
  db.run('put products [{name: "a"}, {name: "b"}]; put reviews [{productId: 1, stars: 5}, {productId: 1, stars: 3}]');
  const q = db.from(products).lookup('reviews', { order: [['stars', 'desc']], limit: 1 });
  assert.equal(q.toFenecQL()[0], 'get products lookup reviews on productId order stars desc limit 1');
  assert.deepEqual(await q.rows(), [
    { id: 1, name: 'a', reviews: [{ id: 1, productId: 1, stars: 5 }] },
    { id: 2, name: 'b', reviews: [] },
  ]);
  // `one` is the first child; a relation chains from the level it reaches.
  const back = db.from(reviews).lookup('product');
  assert.equal(back.toFenecQL()[0], 'get reviews lookup products on id = productId limit 1');
  // A table by itself, with its key, as a name would be.
  assert.equal(db.from(products).lookup(reviews, { on: 'productId' }).toFenecQL()[0], 'get products lookup reviews on productId');
  db.close();
});

// -------------------------------------------------------------- the check

const v1 = {
  todos: fenecTable('todos', { title: text().notNull(), done: boolean() }, (t) => [index('todos_done').using('hash', t.done)]),
};

test('an open makes what the code declares, and opened again makes nothing', { skip }, async () => {
  const db = await Fenec.open(wasm, { schema: v1 });
  assert.deepEqual(db.schemas().map((s) => s.name), ['todos']);
  db.run('put todos {title: "a", done: false}');
  const image = db.snapshot();
  const seq = db.changeSeq;
  // A schema already there: compared, and nothing written.
  assert.deepEqual(db.checkSchema(desc(v1), 'apply'), {
    kind: 'schema', applied: false, ran: false, migrations: [], statements: [], refusals: [],
  });
  assert.equal(db.changeSeq, seq);
  db.close();

  // Additions only: a collection, a field, an index on a field and on a path.
  const v2 = {
    todos: fenecTable('todos', { title: text().notNull(), done: boolean(), at: timestamp(), meta: json() }, (t) => [
      index('todos_done').using('hash', t.done),
      index('todos_at').on(t.at),
      index('todos_lang').using('hash', t.meta.path('lang')),
    ]),
    tags: fenecTable('tags', { name: text().unique() }),
  };
  const before = await Fenec.open(wasm);
  before.load(image);
  const plan = before.checkSchema(desc(v2), 'plan');
  assert.deepEqual(plan.statements, [
    'alter collection todos add field at timestamp @sorted',
    'alter collection todos add field meta json',
    'create index on todos (meta.lang) @hash',
    'create collection tags (name text @unique)',
  ]);
  assert.equal(plan.applied, false);
  const out = before.checkSchema(desc(v2), 'apply');
  assert.equal(out.applied, true);
  assert.deepEqual(before.rows('get todos select title, at, meta'), [{ title: 'a', at: null, meta: null }]);
  assert.equal(before.checkSchema(desc(v2), 'apply').applied, false);
  before.close();
});

/**
 * `image` restored into a database opened with `schema`: refused naming
 * `kinds`, and nothing written -- the database is the image as it was.
 */
async function refusedWith(image, schema, kinds, why, migrations) {
  const db = await Fenec.open(wasm, { schema, migrations });
  const { restoreFrom } = await fakeStore(image);
  await assert.rejects(restoreFrom(db), (e) => {
    assert.ok(e instanceof FenecError && e.refusals, String(e));
    assert.deepEqual(e.refusals.map((r) => r.kind), kinds);
    assert.match(e.message, why);
    return true;
  });
  const as = await Fenec.open(wasm);
  as.load(image);
  assert.equal(JSON.stringify(db.schemas()), JSON.stringify(as.schemas()), 'nothing applied');
  assert.equal(db.changeSeq, as.changeSeq, 'nothing written');
  as.close();
  db.close();
}

test('what would lose data or could mean two things is refused, listed with its resolution', { skip }, async () => {
  const db = await Fenec.open(wasm, { schema: v1 });
  db.run('put todos {title: "a", done: true}');
  const image = db.snapshot();
  db.close();
  const t = (cols, extra) => ({ todos: fenecTable('todos', cols, extra) });
  const hash = (t) => [index().using('hash', t.done)];

  // Renamed, or dropped and another added? Not guessed.
  await refusedWith(image, t({ name: text(), done: boolean() }, hash), ['field_not_declared'], /if it was renamed to `name`, the migration `alter collection todos rename field title to name`/);
  await refusedWith(image, t({ title: integer().notNull(), done: boolean() }, hash), ['type_changed'], /`todos.title` is text in the database and int in the code/);
  await refusedWith(image, t({ title: text(), done: boolean() }, hash), ['required_changed'], /required in the database and not required in the code/);
  await refusedWith(image, t({ title: text().notNull().collate('tr'), done: boolean() }, hash), ['collate_changed'], /rebuild\('todos', 'title'\)/);
  // An index added to one field would be made; the one taken off another is refused.
  await refusedWith(image, t({ title: text().notNull(), done: boolean() }, (t) => [index().on(t.title)]), ['index_removed'], /`todos.done` has @hash/);
  await refusedWith(image, t({ title: text().notNull(), done: boolean() }), ['index_removed'], /`todos.done` has @hash in the database and no index in the code/);
  await refusedWith(image, t({ title: text().notNull(), done: boolean().unique() }), ['index_changed'], /@hash in the database and @unique in the code/);
  await refusedWith(image, t({ title: text().notNull(), done: boolean(), due: timestamp().notNull() }, hash), ['required_added'], /declare it not required/);
  // Every difference at once, each named.
  await refusedWith(image, t({ name: integer(), done: boolean().notNull() }), ['required_changed', 'index_removed', 'field_not_declared'], /title/);
});

test('migrations run once, in order, recorded; a fresh database records them without running', { skip }, async () => {
  const migrations = [rename('todos', 'title', 'name')];
  const v3 = {
    todos: fenecTable('todos', { name: text().notNull(), done: boolean().unique() }),
  };
  const old = await Fenec.open(wasm, { schema: v1 });
  old.run('put todos [{title: "a", done: true}, {title: "b", done: false}]');
  const image = old.snapshot();
  old.close();

  // The index changes too: a rebuild, in the migrations after the rename.
  const steps = [...migrations, rebuild('todos', 'done')];
  await refusedWith(image, v3, ['index_changed'], /rebuild\('todos', 'done'\)/, migrations);
  const db = await Fenec.open(wasm);
  db.load(image);
  const out = db.checkSchema(desc(v3, steps), 'apply');
  assert.equal(out.applied, true);
  assert.equal(out.ran, true);
  assert.deepEqual(out.migrations, [1, 2]);
  assert.deepEqual(db.rows('get todos select name, done order done'), [
    { name: 'b', done: false },
    { name: 'a', done: true },
  ]);
  assert.deepEqual(
    db.rows('get _migrations select n, text order n'),
    [
      { n: 1, text: 'alter collection todos rename field title to name' },
      {
        n: 2,
        text:
          'alter collection todos add field done__rebuilt bool @unique; set todos {done__rebuilt: done}; ' +
          'alter collection todos drop field done; alter collection todos rename field done__rebuilt to done',
      },
    ],
  );
  // Again: nothing to run, nothing to apply.
  const seq = db.changeSeq;
  assert.equal(db.checkSchema(desc(v3, steps), 'apply').applied, false);
  assert.equal(db.changeSeq, seq);
  // A migration changed after it ran: the list only grows.
  assert.throws(() => db.checkSchema(desc(v3, [drop('todos', 'x'), steps[1]]), 'apply'), /not the one the database applied/);
  db.close();

  // Made from the code, a database holds what the migrations lead to:
  // they are recorded, and none runs.
  const fresh = await Fenec.open(wasm, { schema: v3, migrations: steps });
  assert.deepEqual(fresh.rows('get _migrations select n order n'), [{ n: 1 }, { n: 2 }]);
  fresh.close();
  // A failing migration puts back the ones before it, and says which.
  const bad = await Fenec.open(wasm);
  bad.load(image);
  assert.throws(
    () => bad.checkSchema(desc(v3, [...steps, dropTable('nope')]), 'apply'),
    /migration 3: .*nope/,
  );
  assert.deepEqual(bad.rows('get todos select title order id').map((r) => r.title), ['a', 'b']);
  bad.close();
});

test('a database opened with its schema takes a stored one in its place, checked', { skip }, async () => {
  const db = await Fenec.open(wasm, { schema: v1 });
  db.run('put todos {title: "kept"}');
  const image = db.snapshot();
  db.close();
  // As `restore` does: the database the schema made is let go of for the
  // image, and the image checked -- here renamed and a field dropped.
  const other = { todos: fenecTable('todos', { name: text().notNull() }) };
  const migrations = [rename('todos', 'title', 'name'), drop('todos', 'done')];
  const back = await Fenec.open(wasm, { schema: other, migrations });
  assert.deepEqual(back.rows('get _migrations select n'), [{ n: 1 }, { n: 2 }]);
  const { restoreFrom } = await fakeStore(image);
  assert.equal(await restoreFrom(back), true);
  assert.deepEqual(back.rows('get todos select name'), [{ name: 'kept' }]);
  assert.deepEqual(back.rows('get _migrations select n, text order n').map((r) => r.text), migrations);
  back.close();
  // Refused, an open closes what it opened and throws.
  await assert.rejects(Fenec.open(wasm, { schema: other, migrations: [42] }), /a migration is FenecQL text/);
});

/** `restore` over an IndexedDB holding `image` alone, in memory. */
async function fakeStore(image) {
  const records = new Map([['k', image]]);
  const request = (fn) => {
    const req = {};
    queueMicrotask(() => {
      req.result = fn();
      req.onsuccess?.();
    });
    return req;
  };
  const db = {
    transaction() {
      const tx = {
        objectStore: () => ({ get: (k) => request(() => records.get(k)), getAll: () => request(() => []) }),
      };
      setTimeout(() => tx.oncomplete?.());
      return tx;
    },
  };
  globalThis.indexedDB = { open: () => request(() => db) };
  globalThis.IDBKeyRange = { bound: () => ({}) };
  const { restore } = await import('./fenec.js');
  return { restoreFrom: (f) => restore(f, 'k') };
}

test('the open-time check costs an open nothing measurable', { skip }, async () => {
  const schema = {};
  for (let i = 0; i < 10; i++) {
    schema[`c${i}`] = fenecTable(`c${i}`, {
      title: text().notNull(),
      n: integer(),
      at: timestamp(),
      meta: json(),
      embed: vector({ dimensions: 8 }),
    }, (t) => [index().on(t.at), index().using('hash', t.n), index().using('hnsw', t.embed.op('vector_cosine_ops'))]);
  }
  const made = await Fenec.open(wasm, { schema });
  made.run('put c0 {title: "a", n: 1}');
  const image = made.snapshot();
  made.close();
  // An open as `restore` makes one: the module, the image loaded, and with
  // a schema the check over what it loaded.
  const module = await WebAssembly.compile(wasm);
  const time = async (checked) => {
    const t0 = performance.now();
    const db = await Fenec.open(module);
    db.load(image);
    if (checked) assert.equal(db.checkSchema(desc(schema), 'apply').refusals.length, 0);
    const took = performance.now() - t0;
    db.close();
    return took;
  };
  const median = async (checked) => {
    const xs = [];
    for (let i = 0; i < 101; i++) xs.push(await time(checked));
    return xs.sort((a, b) => a - b)[50];
  };
  await median(true);
  const without = await median(false);
  const withSchema = await median(true);
  console.log(`an open of 10 collections: ${without.toFixed(3)} ms, ${withSchema.toFixed(3)} ms checked against their schema`);
});

// ----------------------------------------------------- fenec types --schema

test('fenec types --schema writes tables an open finds no difference with', { skip: skip || (!fenec && 'no fenec binary (cargo build)') }, async () => {
  const dir = await mkdtemp(join(tmpdir(), 'fenec-schema-'));
  try {
    const file = join(dir, 'app.fenec');
    for (const sql of [
      'create collection t (a text required collate tr @text(prefix=6, chars), b int @hash, c float @sorted, d bool, ' +
        'e timestamp @ttl(90m), f bytes, g json, h vector<4, f16> @hnsw(l2, m=8, ef_construction=50, ef_search=33, quant=int8), ' +
        'i sparse<100> @inverted, j [text] @hash, u text @unique, v vector<3> @hnsw(cosine), w [text] collate und, año int)',
      'create index on t (g.lang) @hash',
      'create index on t (g.src.rank) @sorted',
      'create collection text (x int, y vector<8> @hnsw(cosine, quant=bit))',
      'put t {a: "x"}',
    ]) {
      execFileSync(fenec, [file, '-c', sql], { stdio: 'ignore' });
    }
    const out = join(dir, 'schema.ts');
    execFileSync(fenec, ['types', '--schema', file, '--import', new URL('./schema.js', import.meta.url).pathname, '-o', out], { stdio: 'ignore' });
    const source = await readFile(out, 'utf8');
    assert.match(source, /export const textTable = fenecTable\('text'/);
    assert.match(source, /'año': integer\(\)/);
    const tables = await import(out);
    const db = await Fenec.open(wasm);
    db.load(await readFile(file));
    const nothing = { kind: 'schema', applied: false, ran: false, migrations: [], statements: [], refusals: [] };
    assert.deepEqual(db.checkSchema(desc(tables), 'plan'), nothing);
    // And as FenecQL: the text a pull writes is the schema the file holds.
    const text = execFileSync(fenec, ['types', '--fenecql', file], { encoding: 'utf8' });
    assert.deepEqual(db.checkSchema({ format: 1, fenecql: text }, 'plan'), nothing);
    // The tables and the text are one schema: the same statements make it.
    const empty = await Fenec.open(wasm);
    const a = empty.checkSchema(desc(tables), 'plan').statements;
    assert.deepEqual(empty.checkSchema({ format: 1, fenecql: text }, 'plan').statements, a);
    // And `fenec types` reads the text as it reads the database.
    await writeFile(join(dir, 'schema.fenecql'), text);
    assert.equal(
      execFileSync(fenec, ['types', 'schema.fenecql'], { encoding: 'utf8', cwd: dir }).split('\n').slice(3).join('\n'),
      execFileSync(fenec, ['types', 'app.fenec'], { encoding: 'utf8', cwd: dir }).split('\n').slice(3).join('\n'),
    );
    empty.close();
    db.close();
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('the tables types/schema.ts checks make the file fenec types reads its types from', { skip: skip || (!fenec && 'no fenec binary (cargo build)') }, async () => {
  // web/types/schema.ts holds `$inferSelect` equal to what `fenec types`
  // generates for the same collections: that file is made here, again,
  // and must be the one it holds.
  const { tables } = await import('./types/schema-tables.ts');
  const dir = await mkdtemp(join(tmpdir(), 'fenec-types-'));
  try {
    const db = await Fenec.open(wasm, { schema: tables });
    const file = join(dir, 'tables.fenec');
    await writeFile(file, db.snapshot());
    db.close();
    const generated = execFileSync(fenec, ['types', 'tables.fenec'], { encoding: 'utf8', cwd: dir });
    const kept = await readFile(new URL('./types/schema-generated.d.ts', import.meta.url), 'utf8');
    assert.equal(kept, generated, 'web/types/schema-generated.d.ts is not what fenec types makes: write it again (node web/types/check.mjs --schema)');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('the declarations and the plans are what integrations/schema-golden.json says', { skip }, async () => {
  const { text, GOLDEN } = await import('./schema-golden.mjs');
  assert.equal(await readFile(GOLDEN, 'utf8'), await text(), 'make schema-golden');
});
