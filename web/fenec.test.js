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

test('order takes a collation, checked against the ones there are', () => {
  const [sql] = q().order('title', 'desc', { collate: 'tr' }).order('year').toFenecQL();
  assert.equal(sql, 'get articles order title collate tr desc, year asc');
  const [child] = q()
    .lookup('remarks', { on: 'article_id', order: [['body', 'asc', { collate: 'tr' }], 'id'] })
    .toFenecQL();
  assert.equal(child, 'get articles lookup remarks on article_id order body collate tr asc, id asc');
  // Spliced into the text, so only a name on the list gets through.
  assert.throws(() => q().order('title', 'asc', { collate: 'tr desc; del articles' }), FenecError);
  assert.throws(() => q().order('title', 'asc', { collate: 'de' }), FenecError);
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

test('fuse ranks by match and near together', () => {
  const [sql, p] = q()
    .match('body', 'rust wasm')
    .near('embed', [1, 0, 0])
    .fuse({ k: 20, candidates: 50 })
    .limit(10)
    .toFenecQL();
  assert.equal(sql, 'get articles near embed $1 match body $2 fuse k 20 candidates 50 limit 10');
  assert.deepEqual(p, [[1, 0, 0], 'rust wasm']);
  assert.throws(() => q().match('body', 'x').near('embed', [1]).toFenecQL(), FenecError);
  assert.throws(() => q().match('body', 'x').fuse().toFenecQL(), FenecError);
});

test('aggregates go in the select list, grouped or whole', () => {
  const [sql] = from('orders')
    .select('status', 'count(*)', 'SUM(total)', 'avg(total)')
    .where('year', 2024)
    .group('status')
    .order('sum(total)', 'desc')
    .limit(3)
    .toFenecQL();
  assert.equal(
    sql,
    'get orders select status, count(*), sum(total), avg(total) where year = $1 ' +
      'group status order sum(total) desc limit 3',
  );
  assert.equal(
    from('orders').select('min(at)', 'max(at)').toFenecQL()[0],
    'get orders select min(at), max(at)',
  );
});

test('aggregates refuse what the engine would', () => {
  const agg = () => from('orders').select('count(*)');
  assert.throws(() => from('orders').select('status').group('status').toFenecQL(), FenecError);
  assert.throws(() => agg().limit(3).toFenecQL(), FenecError);
  assert.throws(() => agg().near('v', [1, 0]).toFenecQL(), FenecError);
  assert.throws(() => from('orders').select('median(total)'), FenecError);
  assert.throws(() => from('orders').select('sum(a b)'), FenecError);
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

  // `explain` names the path: an equality over `@hash` is answered by the index.
  assert.deepEqual(await docs.where('year', 2024).explain(), [
    'filter: the hash index on year, 1 rows, which is the answer',
    'rows: 1',
  ]);

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

// A module made without the indexes (`make wasm-lite`, which is
// `make wasm FEATURES=none`) opens what the full one wrote, and the full
// one what it wrote: a page can load the smaller module over a store the
// other filled, and hand it back.
const lite = await readFile(new URL('./fenec-lite.wasm', import.meta.url)).catch(() => null);

test('the module made without indexes and the full one open each other\'s files', {
  skip: wasm && lite ? false : 'no web/fenec.wasm and web/fenec-lite.wasm (make wasm wasm-lite)',
}, async () => {
  const { Fenec } = await import('./fenec.js');
  const cat = (...parts) => {
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    parts.reduce((at, p) => (out.set(p, at), at + p.length), 0);
    return out;
  };
  const full = await Fenec.open(wasm);
  full.run(`create collection d (year int @sorted, title text @text, tag text,
            embed vector<3> @hnsw(cosine), s sparse<10> @inverted)`);
  full.run(`put d [
    {year: 2021, title: "rust", tag: "b", embed: [1.0, 0.0, 0.0], s: "{1:0.5}/10"},
    {year: 1999, title: "wasm", tag: "a", embed: [0.0, 1.0, 0.0], s: "{2:0.5}/10"},
    {year: 2010, title: "search", tag: "c", embed: [0.0, 0.0, 1.0], s: "{3:0.5}/10"}]`);
  // The image holds the graph; the writes after it are what `persist`
  // stores as chunks, an index made over the documents among them.
  const image = full.snapshot();
  full.journal();
  full.run('create index on d (tag) @sorted');
  full.run('put d {year: 2024, title: "zig", tag: "d", embed: [0.6, 0.8, 0.0]}');
  const written = full.drain();
  assert.equal(written.replace, false);

  const small = await Fenec.open(lite);
  small.load(cat(image, written.bytes));
  const years = (db) => db.rows('get d select year order year').map((r) => r.year);
  assert.deepEqual(years(small), [1999, 2010, 2021, 2024]);
  assert.deepEqual(small.rows('get d select tag where tag > "b" order tag desc').map((r) => r.tag), ['d', 'c']);
  for (const [sql, feature] of [
    ['get d near embed [1.0, 0.0, 0.0] limit 1', 'vector'],
    ['get d match title "rust"', 'text'],
    ['get d near s "{1:0.5}/10" limit 1', 'sparse'],
    ['create index on d (title) @sorted', 'sorted'],
  ]) {
    assert.throws(() => small.run(sql), new RegExp(`\`${feature}\` feature, which this build was made without`), sql);
  }

  // It writes all the same, a vector and a text among what it changes ...
  small.journal();
  small.run('put d {year: 2030, title: "lite", tag: "e", embed: [0.0, 0.0, 1.0], s: "{4:0.5}/10"}');
  small.run('set d {embed: [0.0, 0.6, 0.8], title: "moved"} where year = 1999');
  small.run('del d where year = 2010');
  const tail = small.drain();
  assert.equal(tail.replace, false);

  // ... and the full module builds every index over it, from the small
  // one's image and from the full one's image with the small one's writes
  // after it, whose graph the writes are applied to.
  const answers = (db) => ({
    years: years(db),
    tags: db.rows('get d select tag where tag >= "d" order tag').map((r) => r.tag),
    near: db.rows('get d near embed [0.0, 0.0, 1.0] limit 2').map((r) => r.title),
    match: db.rows('get d match title "moved"').map((r) => r.year),
    sparse: db.rows('get d near s "{4:0.5}/10" limit 1').map((r) => r.title),
  });
  const want = {
    years: [1999, 2021, 2024, 2030],
    tags: ['d', 'e'],
    near: ['lite', 'moved'],
    match: [1999],
    sparse: ['lite'],
  };
  const fromImage = await Fenec.open(wasm);
  fromImage.load(small.snapshot());
  assert.deepEqual(answers(fromImage), want);
  const fromTail = await Fenec.open(wasm);
  fromTail.load(cat(image, written.bytes, tail.bytes));
  assert.deepEqual(answers(fromTail), want);
  for (const db of [full, small, fromImage, fromTail]) db.close();
});

test('fuse end to end on wasm', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run('create collection notes (body text @text, embed vector<2> @hnsw(cosine))');
  await db.from('notes').insert([
    { body: 'rust in the browser', embed: [0, 1] },
    { body: 'garbage collection', embed: [1, 0] },
    { body: 'rust and wasm', embed: [0.9, 0.1] },
  ]);
  // By text: 3 then 1. By vector: 2, 3, 1. The one on top of neither list
  // but high on both comes first; the one only the vector found, last.
  const rows = await db
    .from('notes')
    .select('body')
    .match('body', 'rust')
    .near('embed', [1, 0])
    .fuse()
    .rows();
  assert.deepEqual(
    rows.map((r) => r.body),
    ['rust and wasm', 'rust in the browser', 'garbage collection'],
  );
  assert.ok(Math.abs(rows[0]._score - (1 / 61 + 1 / 62)) < 1e-6, String(rows[0]._score));
  db.close();
});

test('collate tr on wasm is Intl.Collator("tr")', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  // Words over what the table covers -- Latin-1, Latin Extended-A to -C,
  // the IPA letters, combining marks, punctuation, currency -- each written
  // a second time with its case, its composition or an ignorable soft
  // hyphen changed, so the comparison reaches the accents and the case and
  // not only the letters. What the table leaves to ICU's normalisation stays
  // out: a second mark on one letter, and a case partner outside the table.
  const ranges = [[0x0, 0x370], [0x2000, 0x2070], [0x20a0, 0x20c1], [0x2c60, 0x2c80]];
  const range = (a, b) => Array.from({ length: b - a }, (_, i) => String.fromCodePoint(a + i));
  const covered = (s) => [...s].every((c) => ranges.some(([lo, hi]) => c.codePointAt(0) >= lo && c.codePointAt(0) < hi));
  const pools = [
    [...'abcçdefgğhıijklmnoöprsştuüvyzABCÇDEFGĞHIİJKLMNOÖPRSŞTUÜVYZâîûÂÎÛ'],
    range(0x20, 0x7f),
    [...range(0xa0, 0x300), ...range(0x2c60, 0x2c80)],
    [...range(0x2000, 0x2070), ...range(0x20a0, 0x20c1), '\t', '­'],
  ];
  const marks = range(0x300, 0x370);
  // `Math.imul`: a plain `*` loses the low bits past 2^53, and the
  // generator falls into a cycle too short to make 3000 words.
  let seed = 7;
  const rnd = (n) => {
    seed = (Math.imul(seed, 1103515245) + 12345) & 0x7fffffff;
    return seed % n;
  };
  const pick = (a) => a[rnd(a.length)];
  const words = new Set();
  while (words.size < 3000) {
    const pool = pools[rnd(pools.length)];
    let w = '';
    for (let i = rnd(6); i >= 0; i--) {
      const c = pick(rnd(4) ? pools[0] : pool);
      w += c;
      if (rnd(10) === 0 && c.normalize('NFD').length === 1) w += pick(marks);
    }
    words.add(w);
    const twin = [...w]
      .map((c) => {
        const t = [c.toUpperCase(), c.toLowerCase(), c.normalize('NFD'), c + '­', c][rnd(5)];
        return covered(t) ? t : c;
      })
      .join('');
    words.add(twin);
  }
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run('create collection words (w text)');
  await db.from('words').insert([...words].map((w) => ({ w })));
  const got = (await db.from('words').select('w').order('w', 'asc', { collate: 'tr' }).rows()).map(
    (r) => r.w,
  );
  // What ICU calls equal goes by its bytes, as a deterministic PostgreSQL
  // collation has it.
  const icu = new Intl.Collator('tr');
  const bytes = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
  assert.deepEqual(got, [...words].sort((a, b) => icu.compare(a, b) || bytes(a, b)));
  db.close();
});

test('a collate tr field pages by its last row on wasm, in Intl.Collator("tr") order', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  // A field in the collation compares in it too: `where name > $1` after
  // the last row of a page is the next page, as keyset paging wants, over
  // the scan and over a `@sorted` index alike.
  const letters = [...'abcçdefgğhıijklmnoöprsştuüvyzABCÇDEFGĞHIİJKLMNOÖPRSŞTUÜVYZâîûÂÎÛ'];
  // The high bits: an LCG's low ones cycle, the lowest six every 64 steps,
  // and `% 64` over them never made 600 names.
  let seed = 11;
  const rnd = (n) => {
    seed = (Math.imul(seed, 1103515245) + 12345) & 0x7fffffff;
    return (seed >>> 12) % n;
  };
  const names = new Set();
  while (names.size < 600) {
    let w = '';
    for (let i = rnd(5); i >= 0; i--) w += letters[rnd(letters.length)];
    names.add(w);
  }
  const icu = new Intl.Collator('tr');
  const bytes = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
  const want = [...names].sort((a, b) => icu.compare(a, b) || bytes(a, b));
  const { Fenec } = await import('./fenec.js');
  for (const index of ['', ' @sorted']) {
    const db = await Fenec.open(wasm);
    db.run(`create collection people (name text collate tr${index})`);
    await db.from('people').insert([...names].map((name) => ({ name })));
    const paged = [];
    for (;;) {
      let page = db.from('people').select('name').order('name').limit(25);
      if (paged.length) page = page.where('name', '>', paged[paged.length - 1]);
      const rows = (await page.rows()).map((r) => r.name);
      if (!rows.length) break;
      paged.push(...rows);
    }
    assert.deepEqual(paged, want, index || 'scan');
    db.close();
  }
});

test('collate und on wasm is Intl.Collator("und") in every script, handed the data a statement needs', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  // Words in a script each, now and then two scripts in one, with their
  // case changed or an accent composed another way: the comparison reaches
  // the accents and the case, and every chunk of the root's data but the
  // Latin one the module carries. The Han are common ones and one of
  // another plane: a character whose radical or strokes Unicode revised
  // since ICU 76 would move in Intl's ICU and not here.
  const scripts = [
    'aábcçdeéèêfghiíjklmnñoóöpqrsßtuúüvwxyzAÁBCÇDEÉFGHIÍJKLMNÑOÓÖPRSTUÚÜVWXYZăâđêôơưĂÂĐÊÔƠƯ',
    'αάβγδεέζηθικλμνξοπρσςτυφχψωΑΆΒΓΔΕΖΗΘΙΚΛΜΝΞΟΠΡΣΤΥΦΧΨΩ',
    'абвгдеёжзийклмнопрстуфхцчшщъыьэюяіїєґАБВГДЕЁЖЗИЙКЛМНОПРСТУФХЦЧШЩЭЮЯІЇЄҐ',
    'աբգդեզէըթժիլխծկհձղճմյնշոչպջռսվտրցւփքօֆԱԲԳԴԵԶ',
    'אבגדהוזחטיכךלמםנןסעפףצץקרשת',
    'ابتثجحخدذرزسشصضطظعغفقكلمنهويءآأؤإئةىپچژکگی',
    'अआइईउऊएऐओऔकखगघङचछजझञटठडढणतथदधनपफबभमयरलवशषसह्ािीुूेैोौंः',
    'অআইঈউএকখগঘচছজঝটঠডঢণতথদধনপফবভমযরলশষসহািীুূেৈোৌ',
    'அஆஇஈஉஊஎஏஐஒஓகஙசஞடணதநபமயரலவழளறன்ாிீுூெேைொோ',
    'กขคฆงจฉชซฌญฎฏฐฑฒณดตถทธนบปผฝพฟภมยรลวศษสหฬอฮะัาำิีึืุูเแโใไ่้๊๋',
    'ກຂຄງຈຊຍດຕຖທນບປຜຝພຟມຢຣລວສຫອຮະັາິີຶືຸູເແໂໃໄ່້',
    'აბგდევზთიკლმნოპჟრსტუფქღყშჩცძწჭხჯჰ',
    'ሀለሐመሠረሰሸቀበተቸኀነኘአከኸወዐዘዠየደጀገጠጨጰጸፀፈፐ',
    '가각간갈감갑강개객거건걸검게격견결경고곡공과관광교구국군굴궁권귀규그극근글금기긴길김까꽃나남너노누눈느늘다단달담대더도동두드들등디따때또뜻라람러로루르를리마만말맛머먹면명모목무문물미민바반발밤방배백버번법벽변별병보복본봄부북분불비빛사산살삼상새색생서석선설성세소속손수숙순술숨쉬스시식신실심십아안알암앞애야약양어언얼엄업없여역연열염영예오온올옷와왕외요용우운울원월위유육으은을음응의이인일임입자작잔장재저적전절점정제조족종좌주죽준중즈즉증지직진질짐집차참창책처천철첫청초촌총최추축출충치친칠침카커코크키타터토통투트특파판팔패퍼편평포표품프피필하학한할함합항해행향허현형호혼화확환활황회효후훈휘흐흑흔흘흙흥희흰히힘',
    'あいうえおかがきぎくぐけげこごさざしじすずせぜそぞただちっつてでとどなにぬねのはばぱひびぴふぶぷへべぺほぼぽまみむめもゃやゅゆょよらりるれろわをんアイウエオカガキクケコサシスセソタチッツテトナニヌネノハバパヒビピフブプヘベペホボポマミムメモャヤュユョヨラリルレロワヲンー',
    '一丁七三上下不中九乙二人入八力十千口土大女子小山川工己巾干弓心戈手支文斗方日月木欠止毛水火爪父片牛犬玉瓜瓦甘生用田白皮目矛矢石示禾穴立竹米糸缶羊羽老耳肉臣自至舌舟色艸虫血行衣西見角言谷豆貝赤走足身車辛辰金長門阜隹雨青非面革音頁風飛食首香馬骨高鬼魚鳥鹿麦麻黄黒鼻齒學國語𠀀',
    '0123456789٠١٢٣٤٥٦٧٨٩०१२३४५६७८९๐๑๒๓๔５６７',
    '!"#$%&()*+,-./:;<=>?@[]^_{|}~¡¿§¶©®°±×÷€£¥₺₹←→↑↓∀∂∑√∞≈≠≤≥★☆♠♣♥♦♪☀☁✓✗😀😂🍕🎉👍🚀🌍',
    '𞤀𞤁𞤂𞤃𞤄𞤅𞤢𞤣𞤤𞤥𞤦𞤧',
  ].map((s) => [...s]);
  let seed = 23;
  const rnd = (n) => {
    seed = (Math.imul(seed, 1103515245) + 12345) & 0x7fffffff;
    return (seed >>> 12) % n;
  };
  const word = (letters) => {
    let w = '';
    for (let i = rnd(4); i >= 0; i--) w += letters[rnd(letters.length)];
    return w;
  };
  const words = new Set();
  while (words.size < 2000) {
    let w = word(scripts[rnd(scripts.length)]);
    if (rnd(6) === 0) w += ' ' + word(scripts[rnd(scripts.length)]);
    words.add(w);
    const twin = [w.toUpperCase(), w.toLowerCase(), w.normalize('NFD'), w.normalize('NFC')][rnd(4)];
    if (twin.normalize('NFD') === twin || twin.normalize('NFC') === twin) words.add(twin);
  }
  const icu = new Intl.Collator('und');
  const bytes = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
  const want = [...words].sort((a, b) => icu.compare(a, b) || bytes(a, b));

  const { readFile } = await import('node:fs/promises');
  const fetched = [];
  const collation = (name) => {
    fetched.push(name);
    return readFile(new URL(`./collate/${name}.bin`, import.meta.url));
  };
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm, { collation });
  db.run('create collection words (w text collate und @sorted)');
  // A write whose collated text needs data the module has not got is
  // refused before it changes anything, the data named.
  assert.throws(() => db.run('put words {w: $1}', ['Ελλάδα']), (e) => e.collation?.includes('greek'));
  assert.equal(db.rows('get words').length, 0);
  // The builder fetches what a statement needs and runs it again, each
  // chunk once.
  await db.from('words').insert([...words].map((w) => ({ w })));
  assert.equal(new Set(fetched).size, fetched.length, fetched.join());
  assert.ok(fetched.length >= 10, fetched.join());
  const got = (await db.from('words').select('w').order('w').rows()).map((r) => r.w);
  assert.deepEqual(got, want);

  // An image whose collated text needs data a module has not got loads
  // once handed it: its `@sorted` index was built without it.
  const image = db.snapshot();
  const again = await Fenec.open(wasm, { collation });
  assert.throws(() => again.load(image), (e) => e.collation?.includes('han'));
  await again.loadAsync(image);
  assert.deepEqual((await again.from('words').select('w').order('w').rows()).map((r) => r.w), want);

  // A read that orders text of no collation in one is refused too, and
  // answered once the module has the data.
  const plain = await Fenec.open(wasm, { collation });
  plain.run('create collection t (w text)');
  plain.run('put t [{w: "б"}, {w: "a"}, {w: "β"}]');
  assert.throws(() => plain.run('get t order w collate und'), (e) => e.collation?.length === 2);
  assert.deepEqual((await plain.query('get t order w collate und')).rows.map((r) => r.w), ['a', 'β', 'б']);

  // Data of other tables -- another version's -- is refused.
  const stale = await Fenec.open(wasm, {
    collation: async (name) => {
      const b = new Uint8Array(await readFile(new URL(`./collate/${name}.bin`, import.meta.url)));
      b[8] ^= 1;
      return b;
    },
  });
  await assert.rejects(stale.collation('greek'), /not this module's collation data/);
  // Without a source, the error says where the data would come from.
  const bare = await Fenec.open(wasm);
  await assert.rejects(bare.collation('greek'), /open the module with \{ collation \}/);
  for (const d of [db, again, plain, stale, bare]) d.close();
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

test('required goes into the clause and changes what count is allowed to do', () => {
  assert.equal(
    from('products')
      .lookup('reviews', { on: 'product_id', required: true, where: { stars: { gte: 4 } } })
      .toFenecQL()[0],
    'get products lookup reviews on product_id required where stars >= $1',
  );
});

test('count needs a required lookup, because there is nothing to attach to', async () => {
  const exec = { run: () => ({ columns: ['count'], rows: [{ count: 2 }] }) };
  // `count` collapses the rows children hang from -- unless they are only
  // deciding who is counted.
  await assert.rejects(
    from('p').lookup('r', { on: 'k' }).bind(exec).count(),
    /count cannot be used with lookup unless it is required/,
  );
  assert.equal(await from('p').lookup('r', { on: 'k', required: true }).bind(exec).count(), 2);
});

test('lookup names go through the same identifier check as every other name', () => {
  assert.throws(() => from('p').lookup('r; del p; --', { on: 'k' }), FenecError);
  assert.throws(() => from('p').lookup('r', { on: 'a.b' }), FenecError);
  assert.throws(
    () => from('p').lookup('r', { on: 'k', order: [['a', 'sideways']] }),
    /asc.*desc/,
  );
});

test('a text of several writes lands whole or not at all', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  // Schema changes cannot be put back: a text of them runs each on its own.
  db.run('create collection a (n int); create collection b (n int)');
  assert.throws(() => db.run('put a {n: 1}; put b {n: 2}; put a {nofield: 3}'), /nofield/);
  assert.deepEqual(db.run('get a').rows, []);
  assert.deepEqual(db.run('get b').rows, []);
  db.run('put a {n: 1}; put b {n: 2}');
  assert.deepEqual(db.run('get a select n').rows, [{ n: 1 }]);
  assert.deepEqual(db.run('get b select n').rows, [{ n: 2 }]);
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

test('a second lookup chains onto the first instead of replacing it', () => {
  const [sql, p] = from('shops')
    .limit(20)
    .lookup('orders', { on: 'shop_id', where: { paid: true }, limit: 3 })
    .lookup('lines', { on: 'order_id', select: 'item', limit: 5 })
    .toFenecQL();
  assert.equal(
    sql,
    'get shops limit 20 lookup orders on shop_id where paid = $1 limit 3 ' +
      'lookup lines on order_id select item limit 5',
  );
  assert.deepEqual(p, [true]);
});

test('a chain refuses a repeated collection and a chain too deep', () => {
  // A collection may appear once in a query: two levels down the driving
  // one would be back in scope, and `on child = parent` could mean either.
  assert.throws(
    () =>
      from('shops')
        .lookup('orders', { on: 'shop_id' })
        .lookup('shops', { on: 'id', parentKey: 'shop_id' })
        .toFenecQL(),
    /shops cannot look itself up/,
  );
  let q = from('shops');
  for (let i = 0; i < 9; i++) q = q.lookup(`c${i}`, { on: 'k' });
  assert.throws(() => q.toFenecQL(), /chained too deep/);
});

test('aggregates end to end on wasm', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run('create collection orders (status text @hash, total int)');
  await db.from('orders').insert([
    { status: 'paid', total: 30 },
    { status: 'paid', total: 12 },
    { status: 'open', total: 7 },
    { status: 'open' },
  ]);
  const rows = await db
    .from('orders')
    .select('status', 'count(*)', 'sum(total)', 'avg(total)')
    .group('status')
    .order('sum(total)', 'desc')
    .rows();
  assert.deepEqual(rows, [
    { status: 'paid', count: 2, 'sum(total)': 42, 'avg(total)': 21 },
    { status: 'open', count: 2, 'sum(total)': 7, 'avg(total)': 7 },
  ]);
  const [whole] = await db.from('orders').select('min(total)', 'max(total)').rows();
  assert.deepEqual(whole, { 'min(total)': 7, 'max(total)': 30 });
});

test('lookup chain end to end on wasm', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run('create collection shops (name text)');
  db.run('create collection orders (shop_id int @hash, code text)');
  db.run('create collection lines (order_id int @hash, item text)');
  await db.from('shops').insert([{ name: 'Merkez' }, { name: 'Depo' }]);
  await db.from('orders').insert([
    { shop_id: 1, code: 'A' },
    { shop_id: 1, code: 'B' },
  ]);
  await db.from('lines').insert([
    { order_id: 1, item: 'kahve' },
    { order_id: 1, item: 'demlik' },
  ]);

  const rows = await db
    .from('shops')
    .select('name')
    .lookup('orders', { on: 'shop_id', select: 'code' })
    .lookup('lines', { on: 'order_id', select: 'item' })
    .rows();

  assert.deepEqual(rows, [
    {
      name: 'Merkez',
      orders: [
        { code: 'A', lines: [{ item: 'kahve' }, { item: 'demlik' }] },
        // Order B has no lines: an empty array, not a missing key.
        { code: 'B', lines: [] },
      ],
    },
    { name: 'Depo', orders: [] },
  ]);
});

// The distance kernels sum in one fixed order -- eight running sums, reduced
// pairwise -- and the SIMD build must keep it: a graph built in the browser
// is then the graph built natively. This is that order written out with
// `Math.fround`, so a kernel that drifts from it fails here, SIMD or not.
function sum8(a, b, step) {
  const acc = new Float32Array(8);
  const whole = a.length - (a.length % 8);
  for (let i = 0; i < whole; i += 8) {
    for (let k = 0; k < 8; k++) acc[k] = Math.fround(acc[k] + step(a[i + k], b[i + k]));
  }
  const f = Math.fround;
  let s = f(f(f(acc[0] + acc[1]) + f(acc[2] + acc[3])) + f(f(acc[4] + acc[5]) + f(acc[6] + acc[7])));
  for (let i = whole; i < a.length; i++) s = f(s + step(a[i], b[i]));
  return s;
}

test('distance kernels keep their summation order', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  let x = 0x2545f491;
  const r = () => {
    x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0;
    return Math.fround((x / 2 ** 32) * 2 - 1);
  };
  const mul = (p, q) => Math.fround(p * q);
  const diffSq = (p, q) => { const d = Math.fround(p - q); return Math.fround(d * d); };
  // 37 dimensions: four full strips and a tail, so both paths are covered.
  for (const [metric, score] of [
    ['dot', (v, q) => sum8(v, q, mul)],
    ['l2', (v, q) => Math.fround(Math.sqrt(sum8(v, q, diffSq)))],
  ]) {
    db.run(`create collection k_${metric} (e vector<37> @hnsw(${metric}))`);
    const docs = Array.from({ length: 64 }, () => ({ e: Array.from({ length: 37 }, r) }));
    await db.from(`k_${metric}`).insert(docs);
    for (let t = 0; t < 8; t++) {
      const query = Array.from({ length: 37 }, r);
      const rows = db.rows(`get k_${metric} near e $1 exact limit 64`, [query]);
      assert.equal(rows.length, 64);
      for (const row of rows) {
        assert.equal(Math.fround(row._score), score(row.e.map(Math.fround), query), metric);
      }
    }
  }
});

// The brief at /llms.txt is what a coding agent copies from, so every example
// in it has to run as written. They live in site/build.py, which renders it.
test('every example in the llms brief runs', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const src = await readFile(new URL('../site/build.py', import.meta.url), 'utf8');
  const brief = src.slice(src.indexOf('LLMS_BRIEF = """'));
  const block = brief.match(/```fenecql\n([\s\S]*?)```/)[1];
  const db = await Fenec.open(wasm);
  db.run('create collection comments (article_id int @hash, score int, published timestamp)');
  const examples = block.split('\n').filter(Boolean);
  assert.ok(examples.length >= 10, `only ${examples.length} examples found`);
  for (const sql of examples) {
    assert.doesNotThrow(() => db.run(sql, sql.includes('$1') ? [[0.1, 0.2, 0.3, 0.4]] : []), sql);
  }
});
