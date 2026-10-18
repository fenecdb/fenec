// JavaScript through @fenecdb/web's HTTP client: the query builder the
// browser module runs, sent to a server. Imported here from the repository
// (FENEC_WEB, ../../web/fenec.js); a project imports '@fenecdb/web'.
// Run by ../run-tests.sh.
import assert from 'node:assert/strict';

const web = process.env.FENEC_WEB ?? new URL('../../../web/fenec.js', import.meta.url).href;
const { connect } = await import(web);

const db = connect(process.env.FENEC_URL ?? 'http://127.0.0.1:8080', {
  token: process.env.FENEC_TOKEN,
});

await db.run('create collection if not exists docs (title text @text, year int @sorted)');
await db.from('docs').insert({ title: 'Dunes', year: 2021 });
await db.from('docs').insert({ title: 'Night at the oasis', year: 2024 });

const rows = await db.from('docs').select('title', 'year').where('year', '>=', 2020).order('year', 'desc').limit(5).rows();
assert.deepEqual([...rows], [
  { title: 'Night at the oasis', year: 2024 },
  { title: 'Dunes', year: 2021 },
]);

const hits = await db.from('docs').select('title').match('title', 'oasys', { fuzzy: true }).rows();
assert.deepEqual(hits.map((h) => h.title), ['Night at the oasis']);

// The builder's statement is the one written by hand, and so is its answer.
const [text, params] = db.from('docs').select('title', 'year').where('year', '>=', 2020).order('year', 'desc').limit(5).toFenecQL();
assert.equal(text, 'get docs select title, year where year >= $1 order year desc limit $2');
assert.deepEqual(params, [2020, 5]);
assert.deepEqual(await db.rows(text, params), [...rows]);
assert.equal(await db.from('docs').where('title', '~', 'Dunes').count(), 1);

await assert.rejects(db.run('get nowhere'), /nowhere/);

console.log('javascript (@fenecdb/web): ok');
