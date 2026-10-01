// pgvector-node over fenec-server's pg wire: registerTypes finds the types by
// name, and node-postgres then reads a vector as an array and a sparse
// vector as a SparseVector -- from their text, as against PostgreSQL.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import pg from 'pg';
import pgvector from 'pgvector/pg';
import { SparseVector } from 'pgvector';

const url = process.env.FENEC_PG_URL;

test('pgvector-node reads and writes the three types', { skip: !url && 'FENEC_PG_URL is not set' }, async (t) => {
  const c = new pg.Client({ connectionString: url });
  await c.connect();
  t.after(() => c.end());
  const coll = `pn_${Date.now()}_${Math.floor(Math.random() * 1e6)}`;
  await c.query(`create collection ${coll} (name text, e vector<3> @hnsw(cosine), h vector<3, f16>, s sparse<5> @inverted)`);
  await pgvector.registerTypes(c);
  await c.query(`put ${coll} {name: $1, e: $2, h: $3, s: $4}`,
    ['a', pgvector.toSql([1, 2, 3]), pgvector.toSql([1.5, 2, 3]), pgvector.toSql(new SparseVector([1, 0, 0, 0.5, 0]))]);
  const r = await c.query(`get ${coll} select e, h, s where name = $1`, ['a']);
  assert.deepEqual(r.rows[0].e, [1, 2, 3]);
  assert.deepEqual(r.rows[0].h, [1.5, 2, 3]);
  assert.deepEqual([r.rows[0].s.dimensions, r.rows[0].s.indices, r.rows[0].s.values], [5, [0, 3], [1, 0.5]]);
  const hit = await c.query(`get ${coll} select name near e $1 limit 1`, [pgvector.toSql([1, 2, 3])]);
  assert.equal(hit.rows[0].name, 'a');
});
