// node-postgres over fenec-pg's pg wire: parameters and typed rows, in the
// text format it reads every row in (its parser has no binary mode: with
// `binary: true` it reads a binary cell as UTF-8, from PostgreSQL too).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import pg from 'pg';

const url = process.env.FENEC_PG_URL;

test('parameters go in and typed rows come back', { skip: !url && 'FENEC_PG_URL is not set' }, async (t) => {
  const c = new pg.Client({ connectionString: url });
  await c.connect();
  t.after(() => c.end());
  const coll = `nd_${Date.now()}_${Math.floor(Math.random() * 1e6)}`;
  await c.query(`create collection ${coll} (name text, n int, score float, ok bool, at timestamp)`);
  const at = new Date(Date.UTC(2026, 8, 28, 12, 30));
  await c.query(`put ${coll} {name: $1, n: $2, score: $3, ok: $4, at: $5}`, ['a', 7, 0.25, true, at]);
  const r = await c.query(`get ${coll} select name, n, score, ok, at where n = $1`, [7]);
  assert.deepEqual(r.rows, [{ name: 'a', n: '7', score: 0.25, ok: true, at }]);
  const count = await c.query(`get ${coll} count`);
  assert.deepEqual(count.rows, [{ count: '1' }]);
});
