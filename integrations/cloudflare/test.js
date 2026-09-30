// persist and restore: `npm test` here. Against a stand-in for a Durable
// Object's storage that holds to its limits -- a value of 128 KiB at most,
// 128 keys a call, values cloned on the way in and out -- and can be made
// to fail part way. Needs web/fenec.wasm (make wasm), and skips without it.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { checkpoint, persist, restore, PIECE } from './index.js';

const wasm = await readFile(new URL('../../web/fenec.wasm', import.meta.url)).catch(() => null);
const skip = wasm ? false : 'no web/fenec.wasm (make wasm)';
const { Fenec } = await import('../../web/fenec.js');

/** A Durable Object's storage, in memory. `failAfter` puts, it throws. */
class Storage {
  map = new Map();
  puts = 0;
  failAfter = Infinity;
  async get(key) {
    return structuredClone(this.map.get(key));
  }
  async put(key, value) {
    if (++this.puts > this.failAfter) throw new Error('storage went away');
    const v = structuredClone(value);
    if (v instanceof Uint8Array) assert.ok(v.length <= 128 * 1024, `${key}: ${v.length} bytes`);
    this.map.set(key, v);
  }
  async delete(keys) {
    assert.ok(keys.length <= 128);
    for (const k of keys) this.map.delete(k);
  }
  async list({ prefix }) {
    const keys = [...this.map.keys()].filter((k) => k.startsWith(prefix)).sort();
    return new Map(keys.map((k) => [k, structuredClone(this.map.get(k))]));
  }
}

async function open() {
  return Fenec.open(wasm);
}

/** Every row of `t`, as the database answers them. */
const rows = (db) => db.rows('get t select n, s, e order n');

test('a database comes back as it was kept', { skip }, async () => {
  const storage = new Storage();
  const db = await open();
  assert.equal(await restore(db, storage), false);
  db.run('create collection t (n int, s text, e vector<8> @hnsw(cosine))');
  // An image of many pieces, then writes appended after it.
  const big = Array.from({ length: 3000 }, (_, i) => `{n: ${i}, s: "${'x'.repeat(100)}", e: [${[1, i, 2, 3, 4, 5, 6, 7]}]}`);
  db.run(`put t [${big.join(', ')}]`);
  assert.ok((await persist(db, storage)) > 2 * PIECE);
  for (let i = 0; i < 20; i++) {
    db.run(`put t {n: ${3000 + i}, s: "late", e: [0, 1, ${i}, 0, 0, 0, 0, 1]}`);
    db.run(`set t {s: "changed"} where n = ${i}`);
    assert.ok((await persist(db, storage)) > 0);
  }
  assert.equal(await persist(db, storage), 0, 'nothing written, nothing stored');

  const back = await open();
  assert.equal(await restore(back, storage), true);
  assert.deepEqual(rows(back), rows(db));
  assert.deepEqual(
    back.rows('get t select n near e [0, 1, 7, 0, 0, 0, 0, 1] limit 1'),
    db.rows('get t select n near e [0, 1, 7, 0, 0, 0, 0, 1] limit 1'),
  );
  // And it goes on from there: a write after the restore is kept too.
  back.run('put t {n: -1, s: "after", e: [1, 1, 1, 1, 1, 1, 1, 1]}');
  await persist(back, storage);
  const third = await open();
  await restore(third, storage);
  assert.deepEqual(rows(third), rows(back));
});

test('a new image lets the one before go', { skip }, async () => {
  const storage = new Storage();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run('put t {n: 0, s: "a", e: [1, 0, 0, 0, 0, 0, 0, 0]}');
  await persist(db, storage);
  const first = (await storage.get('fenec:meta')).gen;
  // Writes past half the image fold into a new one.
  for (let i = 1; i < 40; i++) {
    db.run(`put t {n: ${i}, s: "${'y'.repeat(200)}", e: [0, 1, 0, 0, 0, 0, 0, 0]}`);
    await persist(db, storage);
  }
  const gen = (await storage.get('fenec:meta')).gen;
  assert.ok(gen > first);
  const gens = new Set([...storage.map.keys()].filter((k) => k !== 'fenec:meta').map((k) => k.split(':')[1]));
  assert.deepEqual([...gens], [String(gen)], 'only the last generation is kept');
  const back = await open();
  await restore(back, storage);
  assert.deepEqual(rows(back), rows(db));
});

test('storage that fails part way keeps the last whole database', { skip }, async () => {
  // A row appended after the image, and rows enough to have a new image of
  // many pieces written: stopped after every number of puts either takes.
  const writes = {
    append: ['put t {n: 5000, s: "lost?", e: [1, 1, 1, 1, 1, 1, 1, 1]}'],
    image: [`put t [${Array.from({ length: 1500 }, (_, i) => `{n: ${9000 + i}, s: "${'w'.repeat(150)}", e: [${[i, 2, 0, 0, 0, 0, 0, 0]}]}`).join(', ')}]`],
  };
  let failed = 0;
  for (const [what, sqls] of Object.entries(writes)) {
    for (let failAfter = 0; failAfter < 10; failAfter++) {
      const storage = new Storage();
      const db = await open();
      db.run('create collection t (n int, s text, e vector<8>)');
      db.run(`put t [${Array.from({ length: 2000 }, (_, i) => `{n: ${i}, s: "${'z'.repeat(150)}", e: [${[i, 1, 0, 0, 0, 0, 0, 0]}]}`).join(', ')}]`);
      await persist(db, storage);
      const whole = rows(db);
      for (const sql of sqls) db.run(sql);
      storage.failAfter = storage.puts + failAfter;
      const wrote = await persist(db, storage).then(() => true, () => false);
      storage.failAfter = Infinity;
      failed += !wrote;
      const back = await open();
      await restore(back, storage);
      assert.deepEqual(rows(back), wrote ? rows(db) : whole, `${what}, failing after ${failAfter} puts`);
      // The database that failed stores an image next, and it holds all.
      await persist(db, storage);
      const again = await open();
      await restore(again, storage);
      assert.deepEqual(rows(again), rows(db), `${what}, after ${failAfter}, then again`);
    }
  }
  assert.ok(failed >= 8, `only ${failed} writes were cut short`);
});

test('a checkpoint writes the writes since into a new image', { skip }, async () => {
  const storage = new Storage();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8> @hnsw(cosine))');
  db.run(`put t [${Array.from({ length: 500 }, (_, i) => `{n: ${i}, s: "a", e: [${[1, i, 0, 0, 0, 0, 0, 0]}]}`).join(', ')}]`);
  await persist(db, storage);
  db.run('put t {n: 900, s: "b", e: [0, 0, 1, 0, 0, 0, 0, 0]}');
  await persist(db, storage);
  const before = await storage.get('fenec:meta');
  assert.ok(before.log[0] > 0, 'a write kept after the image');
  assert.ok((await checkpoint(db, storage)) > 0);
  const after = await storage.get('fenec:meta');
  assert.equal(after.gen, before.gen + 1);
  assert.deepEqual(after.log, [0, 0], 'nothing after the new image');
  const back = await open();
  await restore(back, storage);
  assert.deepEqual(rows(back), rows(db));
  // And writes after it are kept after it.
  db.run('put t {n: 901, s: "c", e: [0, 0, 0, 1, 0, 0, 0, 0]}');
  await persist(db, storage);
  const again = await open();
  await restore(again, storage);
  assert.deepEqual(rows(again), rows(db));
});

test('databases under two keys are kept apart', { skip }, async () => {
  const storage = new Storage();
  const a = await open();
  const b = await open();
  for (const [db, n] of [[a, 1], [b, 2]]) {
    db.run('create collection t (n int, s text, e vector<8>)');
    db.run(`put t {n: ${n}, s: "k", e: [1, 0, 0, 0, 0, 0, 0, 0]}`);
  }
  await persist(a, storage, { key: 'alpha' });
  await persist(b, storage, { key: 'beta' });
  const back = await open();
  await restore(back, storage, { key: 'beta' });
  assert.deepEqual(rows(back), rows(b));
});
