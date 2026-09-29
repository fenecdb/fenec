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

test('a string is the text it is, for a text field', { skip: !url && 'FENEC_PG_URL is not set' }, async (t) => {
  // node-postgres names no type for a parameter and asks for no Describe
  // before its Bind: fenec-pg reads each value as the field its place
  // names, where by its look "t" was a boolean and "42" a number.
  const c = new pg.Client({ connectionString: url });
  await c.connect();
  t.after(() => c.end());
  const coll = `ns_${Date.now()}_${Math.floor(Math.random() * 1e6)}`;
  await c.query(`create collection ${coll} (name text, n int)`);
  for (const [i, name] of ['t', '42', 'false'].entries()) {
    await c.query(`put ${coll} {name: $1, n: $2}`, [name, i]);
  }
  const r = await c.query(`get ${coll} select name order name`);
  assert.deepEqual(r.rows.map((x) => x.name), ['42', 'false', 't']);
  const hit = await c.query(`get ${coll} select n where name = $1`, ['42']);
  assert.deepEqual(hit.rows, [{ n: '1' }]);
});

test('a list field is an array', { skip: !url && 'FENEC_PG_URL is not set' }, async (t) => {
  // node-postgres parses an array's text by its type and sends a JS array
  // as one; as `text` a list was a string to it.
  const c = new pg.Client({ connectionString: url });
  await c.connect();
  t.after(() => c.end());
  const coll = `na_${Date.now()}_${Math.floor(Math.random() * 1e6)}`;
  await c.query(`create collection ${coll} (name text, tags [text], ns [int], oks [bool])`);
  const tags = ['plain', 'a "q"', 'b\\s', 'x,y', '', 'NULL'];
  await c.query(`put ${coll} {name: $1, tags: $2, ns: $3, oks: $4}`, ['a', tags, [1, -2, 3], [true, false]]);
  const r = await c.query(`get ${coll} select tags, ns, oks where name = $1`, ['a']);
  assert.deepEqual(r.rows, [{ tags, ns: ['1', '-2', '3'], oks: [true, false] }]);
});
