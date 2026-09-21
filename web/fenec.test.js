// Generation tests for the query builder: `node --test web/` (no deps).
//
// No wasm runs here -- the builder is transport independent, so the FenecQL
// text it generates and its parameter array can be checked on their own.
// The engine side is tested in Rust (`crates/fenec-ql/tests/e2e.rs`).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { from, or, not, raw, Query, FenecError } from './fenec.js';

const q = () => from('articles');

// ----------------------------------------------------------------- reading

test('bare get', () => {
  assert.deepEqual(q().toFenecQL(), ['get articles', []]);
});

test('select, limit, offset', () => {
  const [sql, p] = q().select('title', 'year').limit(10).offset(5).toFenecQL();
  assert.equal(sql, 'get articles select title, year limit 10 offset 5');
  assert.deepEqual(p, []);
});

test('select with an array and with a star', () => {
  assert.equal(q().select(['a', 'b']).toFenecQL()[0], 'get articles select a, b');
  assert.equal(q().select('*').toFenecQL()[0], 'get articles');
});

test('three-argument where', () => {
  const [sql, p] = q().where('year', '>=', 2024).toFenecQL();
  assert.equal(sql, 'get articles where year >= $1');
  assert.deepEqual(p, [2024]);
});

test('two-argument where is equality', () => {
  const [sql, p] = q().where('category', 'book').toFenecQL();
  assert.equal(sql, 'get articles where category = $1');
  assert.deepEqual(p, ['book']);
});

test('word operators match the symbols', () => {
  assert.equal(q().where('year', 'gte', 1).toFenecQL()[0], q().where('year', '>=', 1).toFenecQL()[0]);
  assert.equal(q().where('b', 'like', 'x').toFenecQL()[0], 'get articles where b ~ $1');
});

test('an object condition joins with and', () => {
  const [sql, p] = q().where({ year: { gte: 2024 }, tags: { has: 'rust' } }).toFenecQL();
  assert.equal(sql, 'get articles where year >= $1 and tags has $2');
  assert.deepEqual(p, [2024, 'rust']);
});

test('successive where calls join with and', () => {
  const [sql, p] = q().where('a', 1).where('b', 2).toFenecQL();
  assert.equal(sql, 'get articles where a = $1 and b = $2');
  assert.deepEqual(p, [1, 2]);
});

test('two bounds on the same field', () => {
  const [sql, p] = q().where({ year: { gte: 2020, lt: 2030 } }).toFenecQL();
  assert.equal(sql, 'get articles where year >= $1 and year < $2');
  assert.deepEqual(p, [2020, 2030]);
});

test('an and inside an or is parenthesised, and the other way round', () => {
  const [sql] = q().where(or({ a: 1, b: 2 }, { c: 3 })).toFenecQL();
  assert.equal(sql, 'get articles where (a = $1 and b = $2) or c = $3');
});

test('an or inside an and is parenthesised', () => {
  const [sql] = q().where('year', 2024).where(or({ a: 1 }, { b: 2 })).toFenecQL();
  assert.equal(sql, 'get articles where year = $1 and (a = $2 or b = $3)');
});

test('orWhere covers everything conditioned so far', () => {
  const [sql] = q().where('a', 1).where('b', 2).orWhere({ c: 3 }).toFenecQL();
  assert.equal(sql, 'get articles where (a = $1 and b = $2) or c = $3');
});

test('not', () => {
  assert.equal(q().where(not({ a: 1 })).toFenecQL()[0], 'get articles where not (a = $1)');
});

test('in', () => {
  const [sql, p] = q().where('year', 'in', [2023, 2024]).toFenecQL();
  assert.equal(sql, 'get articles where year in [$1, $2]');
  assert.deepEqual(p, [2023, 2024]);
});

test('null becomes is null, not = null', () => {
  assert.equal(q().where({ summary: null }).toFenecQL()[0], 'get articles where summary is null');
  assert.equal(q().where('summary', '=', null).toFenecQL()[0], 'get articles where summary is null');
  assert.equal(q().where('summary', '!=', null).toFenecQL()[0], 'get articles where summary is not null');
  assert.equal(q().where({ summary: { not: null } }).toFenecQL()[0], 'get articles where summary is not null');
});

test('near with ef and exact', () => {
  const v = [0.1, 0.2];
  const [sql, p] = q().near('embed', v, { ef: 128 }).limit(5).toFenecQL();
  assert.equal(sql, 'get articles near embed $1 ef 128 limit 5');
  assert.deepEqual(p, [v]);
  assert.equal(q().near('embed', v, { exact: true }).toFenecQL()[0], 'get articles near embed $1 exact');
});

test('order adds a key on each successive call', () => {
  const [sql] = q().order('year', 'desc').order('title').toFenecQL();
  assert.equal(sql, 'get articles order year desc, title asc');
});

test('count is generated as a clause', async () => {
  const seen = [];
  const exec = (sql, p) => (seen.push([sql, p]), { columns: ['count'], rows: [{ count: 7 }] });
  const n = await q().where('year', '>=', 2024).bind(exec).count();
  assert.deepEqual(seen[0], ['get articles where year >= $1 count', [2024]]);
  assert.equal(n, 7);
});

test('count does not combine with the other clauses', async () => {
  const exec = () => ({ rows: [{ count: 0 }] });
  for (const base of [
    q().limit(5),
    q().offset(1),
    q().select('title'),
    q().order('year'),
    q().near('embed', [1, 2]),
  ]) {
    await assert.rejects(() => base.bind(exec).count(), FenecError);
  }
});

test('match, on its own and with a filter', () => {
  const [sql, p] = q().match('body', 'business trip').limit(10).toFenecQL();
  assert.equal(sql, 'get articles match body $1 limit 10');
  assert.deepEqual(p, ['business trip']);

  const [sql2, p2] = q()
    .select('title')
    .where('year', '>=', 2023)
    .match('body', 'rust')
    .toFenecQL();
  assert.equal(sql2, 'get articles select title where year >= $1 match body $2');
  assert.deepEqual(p2, [2023, 'rust']);
});

test('rerank rides on match and takes a candidate budget', () => {
  const [sql, p] = q()
    .match('body', 'rust')
    .rerank('embed', [1, 0, 0], { candidates: 500 })
    .limit(5)
    .toFenecQL();
  assert.equal(
    sql,
    'get articles match body $1 rerank embed $2 candidates 500 limit 5',
  );
  assert.deepEqual(p, ['rust', [1, 0, 0]]);

  // Without a budget the engine's default applies and nothing is emitted.
  assert.equal(
    q().match('body', 'x').rerank('embed', [1, 0, 0]).toFenecQL()[0],
    'get articles match body $1 rerank embed $2',
  );
});

// The engine refuses these too; the builder fails before a query is sent.
test('match and rerank reject the combinations that contradict', async () => {
  assert.throws(() => q().rerank('embed', [1, 0, 0]).toFenecQL(), FenecError);
  assert.throws(
    () => q().match('body', 'x').near('embed', [1, 0, 0]).toFenecQL(),
    FenecError,
  );
  // `count` runs the query, so it rejects rather than throwing.
  await assert.rejects(
    () => q().match('body', 'x').count(),
    /count cannot be used with `match`/,
  );
  assert.throws(
    () => q().match('body', 'x').toInsert({ title: 'a' }),
    /insert cannot be used with `match`/,
  );
});

test('near together with where and order', () => {
  const [sql, p] = q()
    .select('title')
    .where('year', '>=', 2024)
    .near('embed', [1, 2], { ef: 64 })
    .order('year', 'desc')
    .limit(10)
    .toFenecQL();
  assert.equal(
    sql,
    'get articles select title where year >= $1 near embed $2 ef 64 order year desc limit 10',
  );
  assert.deepEqual(p, [2024, [1, 2]]);
});

test('Float32Array becomes a plain array', () => {
  const [, p] = q().near('embed', new Float32Array([1, 2, 3])).toFenecQL();
  assert.deepEqual(p, [[1, 2, 3]]);
  assert.equal(JSON.stringify(p), '[[1,2,3]]');
});

test('Date becomes ISO text', () => {
  const d = new Date('2026-09-19T12:34:56.000Z');
  const [, p] = q().where('t', '>=', d).toFenecQL();
  assert.deepEqual(p, ['2026-09-19T12:34:56.000Z']);
});

test('raw escape hatch', () => {
  const [sql, p] = q().where(raw('cosine(embed, ?) > ?', [0.1], 0.5)).toFenecQL();
  assert.equal(sql, 'get articles where cosine(embed, $1) > $2');
  assert.deepEqual(p, [[0.1], 0.5]);
});

// ----------------------------------------------------------------- writing

test('insert a single document', async () => {
  const seen = [];
  const r = await q().bind((sql, p) => (seen.push([sql, p]), { kind: 'affected', count: 1 }))
    .insert({ title: 'a', year: 2024 });
  assert.deepEqual(seen[0], ['put articles {title: $1, year: $2}', ['a', 2024]]);
  assert.equal(r, 1);
});

test('insert in bulk', async () => {
  const seen = [];
  await q().bind((sql, p) => (seen.push([sql, p]), { count: 2 }))
    .insert([{ title: 'a' }, { title: 'b' }]);
  assert.deepEqual(seen[0], ['put articles [{title: $1}, {title: $2}]', ['a', 'b']]);
});

test('update with a filter', async () => {
  const seen = [];
  await q().where('id', 3).bind((sql, p) => (seen.push([sql, p]), { count: 1 }))
    .update({ title: 'new' });
  assert.deepEqual(seen[0], ['set articles {title: $1} where id = $2', ['new', 3]]);
});

test('delete with a filter', async () => {
  const seen = [];
  await q().where('year', '<', 2000).bind((sql, p) => (seen.push([sql, p]), { count: 7 }))
    .delete();
  assert.deepEqual(seen[0], ['del articles where year < $1', [2000]]);
});

test('an unfiltered delete is rejected, all: true lets it through', async () => {
  const exec = () => ({ count: 0 });
  await assert.rejects(() => q().bind(exec).delete(), FenecError);
  await assert.rejects(() => q().bind(exec).update({ a: 1 }), FenecError);
  const seen = [];
  await q().bind((sql) => (seen.push(sql), { count: 0 })).delete({ all: true });
  assert.equal(seen[0], 'del articles');
});

test('write statements do not accept limit/near', async () => {
  const exec = () => ({ count: 0 });
  await assert.rejects(() => q().limit(1).bind(exec).delete({ all: true }), FenecError);
  await assert.rejects(() => q().where('a', 1).bind(exec).insert({ a: 1 }), FenecError);
});

// --------------------------------------------------------------- execution

test('rows, first, count', async () => {
  const data = { columns: ['id'], rows: [{ id: 1 }, { id: 2 }] };
  const db = { run: (sql) => (sql.endsWith(' count') ? { rows: [{ count: 2 }] } : data) };
  assert.deepEqual(await from('t').bind(db).rows(), data.rows);
  assert.deepEqual(await from('t').bind(db).first(), { id: 1 });
  assert.equal(await from('t').bind(db).count(), 2);
  assert.equal(await from('t').bind(() => ({ columns: [], rows: [] })).first(), null);
});

test('first adds limit 1', async () => {
  const seen = [];
  await from('t').bind((sql) => (seen.push(sql), { rows: [] })).first();
  assert.equal(seen[0], 'get t limit 1');
});

test('an unbound query cannot run but still generates text', async () => {
  assert.equal(from('t').toFenecQL()[0], 'get t');
  await assert.rejects(() => from('t').rows(), FenecError);
});

// ---------------------------------------------------------------- security

test('names are validated -- the injection boundary', () => {
  assert.throws(() => from('t; drop collection x'), FenecError);
  assert.throws(() => q().select('a, b'), FenecError);
  assert.throws(() => q().where('a or 1=1', 1), FenecError);
  assert.throws(() => q().order('a desc'), FenecError);
  assert.throws(() => q().near('a b', [1]), FenecError);
});

test('unicode identifiers are valid', () => {
  // The lexer accepts any Unicode letter, not just ASCII.
  assert.equal(from('páginas').where('año', 2024).toFenecQL()[0],
    'get páginas where año = $1');
  assert.equal(from('livres').select('résumé').toFenecQL()[0], 'get livres select résumé');
});

test('a text value is never embedded in the query text', () => {
  const [sql, p] = q().where('title', '"; del articles; --').toFenecQL();
  assert.equal(sql, 'get articles where title = $1');
  assert.deepEqual(p, ['"; del articles; --']);
});

test('limit must be a whole number', () => {
  assert.throws(() => q().limit(-1), FenecError);
  assert.throws(() => q().limit(1.5), FenecError);
  assert.throws(() => q().near('e', [1], { ef: -3 }), FenecError);
});

test('undefined does not silently become null', () => {
  assert.throws(() => q().where('a', undefined).toFenecQL(), FenecError);
});

test('an empty in is rejected', () => {
  assert.throws(() => q().where('a', 'in', []), FenecError);
});

// -------------------------------------------------------------- immutability

test('a query is immutable and can be branched', () => {
  const base = q().where('year', '>=', 2024);
  const a = base.where('tags', 'has', 'rust');
  const b = base.limit(3);
  assert.equal(base.toFenecQL()[0], 'get articles where year >= $1');
  assert.equal(a.toFenecQL()[0], 'get articles where year >= $1 and tags has $2');
  assert.equal(b.toFenecQL()[0], 'get articles where year >= $1 limit 3');
  assert.ok(a instanceof Query);
});

test('the conditional filter pattern', () => {
  const build = (f) => {
    let x = q().select('title');
    if (f.year) x = x.where('year', '>=', f.year);
    if (f.tag) x = x.where('tags', 'has', f.tag);
    return x.limit(20).toFenecQL();
  };
  assert.deepEqual(build({}), ['get articles select title limit 20', []]);
  assert.deepEqual(build({ year: 2024 }), [
    'get articles select title where year >= $1 limit 20', [2024],
  ]);
  assert.deepEqual(build({ year: 2024, tag: 'rust' }), [
    'get articles select title where year >= $1 and tags has $2 limit 20',
    [2024, 'rust'],
  ]);
});

// ---------------------------------------------------------------- end to end
//
// Checks that the generated text really parses and gives the right answer.
// Skipped when `web/fenec.wasm` is missing (`make wasm` produces it).

import { readFile } from 'node:fs/promises';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);

test('end to end on wasm', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run(`create collection articles (
            title text, tags [text], year int @hash,
            summary text, embed vector<3> @hnsw(cosine))`);

  const n = await db.from('articles').insert([
    { title: 'rust', tags: ['rust', 'db'], year: 2024, summary: 'a', embed: new Float32Array([1, 0, 0]) },
    { title: 'zig', tags: ['zig'], year: 2023, embed: new Float32Array([0, 1, 0]) },
    { title: 'old', tags: [], year: 1999, summary: 'c', embed: new Float32Array([0, 0, 1]) },
  ]);
  assert.equal(n, 3);

  const docs = db.from('articles');

  assert.deepEqual(
    (await docs.select('title').where('year', '>=', 2024).rows()),
    [{ title: 'rust' }],
  );
  assert.equal(await docs.where({ tags: { has: 'rust' } }).count(), 1);
  assert.equal(await docs.where({ year: { gte: 2000, lt: 2025 } }).count(), 2);
  assert.equal(await docs.where('year', 'in', [1999, 2023]).count(), 2);
  assert.equal(await docs.where({ summary: null }).count(), 1);
  assert.equal(await docs.where({ summary: { not: null } }).count(), 2);
  assert.equal(await docs.where(or({ year: 1999 }, { year: 2024 })).count(), 2);
  assert.equal(await docs.where('year', 2024).where(or({ year: 1999 }, { year: 2024 })).count(), 1);
  assert.equal(await docs.where(not({ year: 1999 })).count(), 2);
  assert.equal(await docs.where('title', '~', 'RU').count(), 1);

  // near: the document closest to the query vector comes first, `_score` is returned.
  const near = await docs.near('embed', new Float32Array([0.9, 0.1, 0]), { ef: 64 }).limit(2).rows();
  assert.equal(near[0].title, 'rust');
  assert.equal(typeof near[0]._score, 'number');

  // filter and near together
  const filtered = await docs.where('year', '<', 2024).near('embed', [1, 0, 0]).limit(5).rows();
  assert.equal(filtered.length, 2);
  assert.ok(!filtered.some((r) => r.title === 'rust'));

  // order -- single key and multi key
  const sorted = await docs.select('year').order('year', 'desc').rows();
  assert.deepEqual(sorted.map((r) => r.year), [2024, 2023, 1999]);

  await docs.insert({ title: 'same year', year: 2023, embed: new Float32Array([0, 0, 1]) });
  const twoKeys = await docs.select('year', 'title').order('year', 'asc').order('title', 'desc').rows();
  assert.deepEqual(
    twoKeys.map((r) => [r.year, r.title]),
    [[1999, 'old'], [2023, 'zig'], [2023, 'same year'], [2024, 'rust']],
  );
  await docs.where('title', 'same year').delete();

  // raw: a call to a built-in function
  assert.equal(await docs.where(raw('lower(title) = ?', 'rust')).count(), 1);

  // update / delete
  assert.equal(await docs.where('year', 1999).update({ title: 'refreshed' }), 1);
  assert.equal((await docs.where('year', 1999).first()).title, 'refreshed');
  assert.equal(await docs.where('year', '<', 2000).delete(), 1);
  assert.equal(await docs.count(), 2);

  db.close();
});

// ----------------------------------------------------------- HTTP endpoint
//
// Tests against a real server live on the Rust side (`crates/fenec-http/tests`).
// What is checked here is what the client sends and how it normalises the
// response; node's own HTTP server is enough for that.

import { createServer } from 'node:http';
import { connect, FenecHttp } from './fenec.js';

/** A small server that records requests and returns a canned response. */
async function stub(handler) {
  const seen = [];
  const server = createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      seen.push({ method: req.method, url: req.url, headers: req.headers, body: JSON.parse(body || 'null') });
      const [status, payload] = handler(seen.at(-1));
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(payload));
    });
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { url: `http://127.0.0.1:${server.address().port}`, seen, close: () => server.close() };
}

test('the HTTP transport sends the text the builder generated', async () => {
  const s = await stub(() => [200, [{ title: 'a' }]]);
  try {
    const db = connect(s.url);
    const rows = await db.from('articles')
      .select('title')
      .where('year', '>=', 2024)
      .near('embed', new Float32Array([1, 2]))
      .limit(5)
      .rows();

    assert.deepEqual(rows, [{ title: 'a' }]);
    assert.equal(s.seen[0].method, 'POST');
    assert.equal(s.seen[0].url, '/query');
    assert.deepEqual(s.seen[0].body, {
      query: 'get articles select title where year >= $1 near embed $2 limit 5',
      params: [2024, [1, 2]],
    });
  } finally {
    s.close();
  }
});

test('HTTP and wasm generate the same query text', async () => {
  const s = await stub(() => [200, []]);
  try {
    const q = (db) => db.from('t').where('a', 1).where({ b: { has: 'x' } }).order('c', 'desc').limit(3);
    const local = q({ from: (n) => from(n) }).toFenecQL();
    await q(connect(s.url)).rows();
    assert.equal(s.seen[0].body.query, local[0]);
    assert.deepEqual(s.seen[0].body.params, local[1]);
  } finally {
    s.close();
  }
});

test('HTTP response shapes are normalised', async () => {
  const s = await stub((req) =>
    req.body.query.startsWith('put') ? [200, { affected: 2 }] : [200, [{ count: 7 }]],
  );
  try {
    const db = connect(s.url);
    assert.equal(await db.from('t').insert([{ a: 1 }, { a: 2 }]), 2);
    assert.equal(await db.from('t').where('a', 1).count(), 7);
  } finally {
    s.close();
  }
});

test('an HTTP error becomes a FenecError, the token goes into the header', async () => {
  const s = await stub((req) =>
    req.headers.authorization === 'Bearer secret'
      ? [400, { error: 'not found: field `missing`' }]
      : [401, { error: 'invalid or missing token' }],
  );
  try {
    await assert.rejects(() => connect(s.url).from('t').rows(), /invalid or missing token/);
    await assert.rejects(
      () => connect(s.url, { token: 'secret' }).from('t').rows(),
      /not found: field `missing`/,
    );
  } finally {
    s.close();
  }
});

test('a clear error when fetch is missing', () => {
  const original = globalThis.fetch;
  delete globalThis.fetch;
  try {
    assert.throws(() => new FenecHttp('http://x'), FenecError);
    // A hand-supplied fetch is always valid (a proxied version, for instance).
    assert.ok(new FenecHttp('http://x', { fetch: () => {} }));
  } finally {
    globalThis.fetch = original;
  }
});

// ------------------------------------------------------------------ lookup

test('lookup is terminal: its clauses bind to the child', () => {
  const [sql, p] = from('products')
    .where('price', '>', 100)
    .limit(20)
    .lookup('reviews', {
      on: 'product_id',
      select: ['body', 'stars'],
      where: { stars: { gte: 4 } },
      order: [['created', 'desc']],
      limit: 3,
    })
    .toFenecQL();
  assert.equal(
    sql,
    'get products where price > $1 limit 20 lookup reviews on product_id ' +
      'select body, stars where stars >= $2 order created desc limit 3',
  );
  // The child's parameters follow the parent's, because the clause is last.
  assert.deepEqual(p, [100, 4]);
});

test('lookup defaults the parent key to id, and can name it', () => {
  assert.equal(
    from('products').lookup('reviews', { on: 'product_id' }).toFenecQL()[0],
    'get products lookup reviews on product_id',
  );
  assert.equal(
    from('products')
      .lookup('tags', { on: 'sku', parentKey: 'sku', order: 'label' })
      .toFenecQL()[0],
    'get products lookup tags on sku = sku order label asc',
  );
});

test('lookup refuses what the engine refuses', () => {
  assert.throws(
    () => from('p').near('e', [1, 0]).lookup('r', { on: 'k' }).toFenecQL(),
    /lookup cannot be combined with near/,
  );
  assert.throws(
    () => from('p').match('body', 'x').lookup('r', { on: 'k' }).toFenecQL(),
    /lookup cannot be combined with match/,
  );
  assert.throws(
    () => from('p').lookup('p', { on: 'k' }).toFenecQL(),
    /cannot look itself up/,
  );
  assert.throws(() => from('p').lookup('r', {}), /lookup needs `on`/);
});

test('lookup names go through the same identifier check as every other name', () => {
  assert.throws(() => from('p').lookup('r; del p; --', { on: 'k' }), FenecError);
  assert.throws(() => from('p').lookup('r', { on: 'a.b' }), FenecError);
  assert.throws(
    () => from('p').lookup('r', { on: 'k', order: [['a', 'sideways']] }),
    /asc.*desc/,
  );
});

test('lookup end to end on wasm', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run('create collection products (name text, price int)');
  db.run('create collection reviews (product_id int @hash, stars int, body text)');
  await db.from('products').insert([
    { name: 'Kahve', price: 12000 },
    { name: 'Demlik', price: 34000 },
  ]);
  await db.from('reviews').insert([
    { product_id: 1, stars: 5, body: 'guzel' },
    { product_id: 1, stars: 3, body: 'idare eder' },
    { product_id: 1, stars: 4, body: 'hizli kargo' },
  ]);

  const rows = await db
    .from('products')
    .select('name')
    .lookup('reviews', {
      on: 'product_id',
      select: ['stars'],
      where: { stars: { gte: 4 } },
      order: [['stars', 'desc']],
      limit: 2,
    })
    .rows();

  assert.deepEqual(rows, [
    { name: 'Kahve', reviews: [{ stars: 5 }, { stars: 4 }] },
    // A parent with no matching children keeps its row and an empty group:
    // the page is the parents.
    { name: 'Demlik', reviews: [] },
  ]);
});
