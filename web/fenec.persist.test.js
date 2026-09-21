// Incremental persistence: `persist` stores the image once and then only the
// writes since, and `restore` reads the image and its chunks back as one
// file. Node has no IndexedDB, so a small in-memory one stands in for it --
// the part of the API `persist`, `restore` and the sync layer's cursors use,
// keys kept sorted as the real one keeps them.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);

class KeyRange {
  constructor(lo, hi) {
    this.lo = lo;
    this.hi = hi;
  }
  static bound(lo, hi) {
    return new KeyRange(lo, hi);
  }
  has(k) {
    return k >= this.lo && k <= this.hi;
  }
}

/** One store's records, and every transaction that wrote to them. */
function fakeIndexedDB() {
  const records = new Map();
  const log = [];
  const request = (fn) => {
    const req = {};
    queueMicrotask(() => {
      req.result = fn();
      req.onsuccess?.();
    });
    return req;
  };
  const store = (writes) => ({
    put(value, key) {
      writes.push(['put', key, value.length ?? 0]);
      records.set(key, value);
    },
    delete(range) {
      writes.push(['delete', `${range.lo}..`]);
      for (const k of [...records.keys()]) if (range.has(k)) records.delete(k);
    },
    get: (key) => request(() => records.get(key)),
    getAll: (range) =>
      request(() =>
        [...records.keys()]
          .filter((k) => range.has(k))
          .sort()
          .map((k) => records.get(k)),
      ),
  });
  const db = {
    transaction() {
      const writes = [];
      const tx = { objectStore: () => store(writes) };
      setTimeout(() => {
        if (writes.length) log.push(writes);
        tx.oncomplete?.();
      });
      return tx;
    },
  };
  return {
    records,
    log,
    open() {
      const req = {};
      queueMicrotask(() => {
        req.result = db;
        req.onsuccess?.();
      });
      return req;
    },
  };
}

test('persist stores the image once, then only what was written since', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const idb = fakeIndexedDB();
  globalThis.indexedDB = idb;
  globalThis.IDBKeyRange = KeyRange;
  const { Fenec, persist, restore } = await import('./fenec.js');

  const db = await Fenec.open(wasm);
  db.run('create collection t (n int, body text)');
  const filler = 'x'.repeat(200);
  for (let i = 0; i < 500; i++) db.run('put t {n: $1, body: $2}', [i, filler]);

  const first = await persist(db, 'k');
  assert.ok(first > 100_000, `the image first: ${first} bytes`);
  assert.equal(idb.log.at(-1)[0][0], 'put');

  // One row: a chunk of that row's size, not the image again.
  db.run('put t {n: 9999, body: "one"}');
  const one = await persist(db, 'k');
  assert.ok(one > 0 && one < 200, `one row wrote ${one} bytes`);
  assert.deepEqual(idb.log.at(-1).map((w) => w[1]), ['k#000000001']);
  assert.equal(await persist(db, 'k'), 0, 'nothing written, nothing stored');

  db.run('set t {body: "changed"} where n = 3');
  db.run('del t where n = 4');
  await persist(db, 'k');
  assert.deepEqual(idb.log.at(-1).map((w) => w[1]), ['k#000000002']);

  // Image and chunks read back as one file give the same database.
  const back = await Fenec.open(wasm);
  assert.equal(await restore(back, 'k'), true);
  const rows = (d) => d.rows('get t select n, body order n');
  assert.deepEqual(rows(back), rows(db));

  // A restored database goes on adding chunks where the stored ones end.
  back.run('put t {n: 10000}');
  await persist(back, 'k');
  assert.deepEqual(idb.log.at(-1).map((w) => w[1]), ['k#000000003']);
  const again = await Fenec.open(wasm);
  await restore(again, 'k');
  assert.deepEqual(rows(again), rows(back));

  // `compact` rewrites the image: it replaces what was stored, chunks gone.
  back.run('compact');
  back.run('put t {n: 10001}');
  await persist(back, 'k');
  assert.deepEqual(idb.log.at(-1).map((w) => w[0]), ['put', 'delete']);
  assert.ok(![...idb.records.keys()].some((k) => k.startsWith('k#')));
  const third = await Fenec.open(wasm);
  await restore(third, 'k');
  assert.deepEqual(rows(third), rows(back));

  // Once the chunks outgrow half the image, a new image replaces them.
  for (let i = 0; i < 300; i++) {
    third.run('put t {n: $1, body: $2}', [20000 + i, filler]);
    await persist(third, 'k');
  }
  assert.ok(idb.log.some((w) => w[0][0] === 'put' && w[0][1] === 'k' && w[1]?.[0] === 'delete'));
  const fourth = await Fenec.open(wasm);
  await restore(fourth, 'k');
  assert.deepEqual(rows(fourth), rows(third));
});

test('drain hands over frames, or an image after a rewrite', { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
  const { Fenec } = await import('./fenec.js');
  const db = await Fenec.open(wasm);
  db.run('create collection t (n int)');
  // No journal started: nothing is kept.
  db.run('put t {n: 1}');
  assert.deepEqual(db.drain(), { replace: false, bytes: new Uint8Array(0) });

  db.journal();
  const base = db.snapshot();
  db.run('put t {n: 2}');
  const { replace, bytes } = db.drain();
  assert.equal(replace, false);
  const file = new Uint8Array(base.length + bytes.length);
  file.set(base);
  file.set(bytes, base.length);
  const copy = await Fenec.open(wasm);
  copy.load(file);
  assert.deepEqual(copy.rows('get t select n order n'), db.rows('get t select n order n'));
  assert.equal(db.drain().bytes.length, 0, 'drained once');

  db.run('compact');
  db.run('put t {n: 3}');
  const after = db.drain();
  assert.equal(after.replace, true);
  const fresh = await Fenec.open(wasm);
  fresh.load(after.bytes);
  assert.deepEqual(fresh.rows('get t select n order n'), db.rows('get t select n order n'));
});
