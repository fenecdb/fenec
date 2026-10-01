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

await db.run('create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))');
await db.from('docs').insert({ title: 'Night at the oasis', embed: [0.1, 0.2, 0.3] });
await db.from('docs').insert({ title: 'Dunes', embed: [0.9, 0.1, 0.0] });

const rows = await db.from('docs').select('title').near('embed', [0.1, 0.2, 0.3]).limit(5).rows();
assert.deepEqual(rows.map((r) => r.title), ['Night at the oasis', 'Dunes']);

await assert.rejects(db.run('get nowhere'), /nowhere/);

console.log('javascript (@fenecdb/web): ok');
