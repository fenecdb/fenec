// persist and restore: `npm test` here. Against stand-ins for a Durable
// Object's storage (fakes.js) that hold to its limits and can be made to
// fail part way: a SQLite-backed object's `sql` over node:sqlite, and a
// key-value backed object's values. Needs web/fenec.wasm (make wasm), and
// skips without it.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { checkpoint, due, maintain, persist, restore, schedule, PIECE, ROW } from './index.js';
import { Rows, Values } from './fakes.js';

const wasm = await readFile(new URL('../../web/fenec.wasm', import.meta.url)).catch(() => null);
const skip = wasm ? false : 'no web/fenec.wasm (make wasm)';
const { Fenec } = await import('../../web/fenec.js');
const sqlite = await import('node:sqlite').catch(() => null);

/** Each storage a test runs over: `sql` skipped where node:sqlite is missing. */
const kinds = {
  sql: { make: () => new Rows(sqlite.DatabaseSync), skip: skip || (!sqlite && 'no node:sqlite') },
  values: { make: () => new Values(), skip },
};

/** `name` over each kind of storage. */
function each(name, fn) {
  for (const [kind, { make, skip }] of Object.entries(kinds)) test(`${name} (${kind})`, { skip }, () => fn(make, kind));
}

async function open() {
  return Fenec.open(wasm);
}

/** Every row of `t`, as the database answers them. */
const rows = (db) => db.rows('get t select n, s, e order n');

/** A database restored from `storage`, as an object woken would have it. */
async function back(storage, options) {
  const db = await open();
  assert.equal(await restore(db, storage, options), true);
  return db;
}

const many = (n, from, s, len = 150) =>
  `put t [${Array.from({ length: n }, (_, i) => `{n: ${from + i}, s: "${s.repeat(len)}", e: [${[i, 1, 0, 0, 0, 0, 0, 0]}]}`).join(', ')}]`;

each('a database comes back as it was kept', async (make) => {
  const storage = make();
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

  const again = await back(storage);
  assert.deepEqual(rows(again), rows(db));
  assert.deepEqual(
    again.rows('get t select n near e [0, 1, 7, 0, 0, 0, 0, 1] limit 1'),
    db.rows('get t select n near e [0, 1, 7, 0, 0, 0, 0, 1] limit 1'),
  );
  // And it goes on from there: a write after the restore is kept too.
  again.run('put t {n: -1, s: "after", e: [1, 1, 1, 1, 1, 1, 1, 1]}');
  await persist(again, storage);
  assert.deepEqual(rows(await back(storage)), rows(again));
});

each('a write is one row, and a new image its rows', async (make, kind) => {
  const storage = make();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(20000, 0, 'z'));
  let before = storage.rows;
  const image = await persist(db, storage);
  // The image's rows, and on a SQLite-backed object the table, made in two.
  const pieces = Math.ceil(image / (kind === 'sql' ? ROW : PIECE));
  if (kind === 'sql') assert.equal(storage.rows - before, pieces + 2);
  else assert.ok(storage.rows - before >= pieces + 1, `${storage.rows - before} rows for ${pieces} pieces`);
  before = storage.rows;
  db.run('put t {n: -1, s: "one", e: [1, 0, 0, 0, 0, 0, 0, 0]}');
  await persist(db, storage);
  // The write's row; a record naming it beside, as values.
  assert.equal(storage.rows - before, kind === 'sql' ? 1 : 2);
});

each('writes that land while a flush is on its way share the next', async (make, kind) => {
  const storage = make();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(100, 0, 'a'));
  await persist(db, storage);
  // Its flush let go of, as a request's answer would be by the next.
  await new Promise(setImmediate);
  const before = storage.rows;
  // As requests a Durable Object takes in while one waits: each its
  // statement, then `persist`, none waited for before the next.
  const waits = [];
  for (let i = 0; i < 32; i++) {
    db.run(`put t {n: ${1000 + i}, s: "g", e: [0, 1, 0, 0, 0, 0, 0, 0]}`);
    waits.push(persist(db, storage));
  }
  // A read among them waits for the writes it may have read.
  let read = false;
  const reading = persist(db, storage).then((n) => (read = n === 0));
  const wrote = await Promise.all(waits);
  await reading;
  assert.ok(read, 'the read resolved to 0, after the writes');
  assert.ok(wrote.every((n) => n > 0));
  // The first went out at once, and the 31 after it in one flush.
  assert.equal(storage.rows - before, kind === 'sql' ? 2 : 4, 'two flushes for the 32 writes');
  assert.deepEqual(rows(await back(storage)), rows(db));
});

each('a new image lets the one before go', async (make, kind) => {
  const storage = make();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(200, 0, 'a'));
  await persist(db, storage);
  const first = kind === 'sql' ? storage.tables() : (await storage.get('fenec:meta')).gen;
  // Writes past half the image, and past 64 KiB, fold into a new one.
  for (let i = 0; i < 20; i++) {
    db.run(many(20, 1000 + 20 * i, 'y', 300));
    await persist(db, storage);
  }
  if (kind === 'sql') {
    const tables = storage.tables();
    assert.equal(tables.length, 1, 'one generation kept');
    assert.notDeepEqual(tables, first);
  } else {
    const gen = (await storage.get('fenec:meta')).gen;
    assert.ok(gen > first);
    const gens = new Set([...storage.map.keys()].filter((k) => k !== 'fenec:meta').map((k) => k.split(':')[1]));
    assert.deepEqual([...gens], [String(gen)], 'only the last generation is kept');
  }
  assert.deepEqual(rows(await back(storage)), rows(db));
});

each('storage that fails part way keeps the last whole database', async (make, kind) => {
  // A row appended after the image, rows enough to have a new image of
  // several rows or pieces written, and a compact's image: stopped after
  // every number of operations each takes.
  const [base, more] = kind === 'sql' ? [12000, 9000] : [2000, 1500];
  const writes = {
    append: ['put t {n: -5, s: "lost?", e: [1, 1, 1, 1, 1, 1, 1, 1]}'],
    image: [many(more, base, 'w')],
    compact: ['del t where n < 500', 'compact'],
  };
  let failed = 0;
  for (const [what, sqls] of Object.entries(writes)) {
    for (let failAfter = 0; failAfter < 12; failAfter++) {
      const storage = make();
      const count = () => (kind === 'sql' ? storage.ops : storage.puts);
      const db = await open();
      db.run('create collection t (n int, s text, e vector<8>)');
      db.run(many(base, 0, 'z'));
      await persist(db, storage);
      const whole = rows(db);
      for (const sql of sqls) db.run(sql);
      storage.failAfter = count() + failAfter;
      const wrote = await persist(db, storage).then(() => true, () => false);
      storage.failAfter = Infinity;
      failed += !wrote;
      const again = await back(storage);
      assert.deepEqual(rows(again), wrote ? rows(db) : whole, `${what}, failing after ${failAfter}`);
      if (!wrote) {
        // The database that failed holds writes its storage does not, and
        // stores nothing more: a new one is restored instead.
        await assert.rejects(persist(db, storage), /open it again and restore/);
        await assert.rejects(checkpoint(db, storage), /open it again and restore/);
      }
      // The database restored goes on from what storage holds.
      again.run('put t {n: -7, s: "then", e: [0, 0, 0, 0, 0, 0, 0, 1]}');
      await persist(again, storage);
      assert.deepEqual(rows(await back(storage)), rows(again), `${what}, after ${failAfter}, then again`);
    }
  }
  assert.ok(failed >= 12, `only ${failed} writes were cut short`);
});

each('a flush that fails fails every write it carried', async (make, kind) => {
  const storage = make();
  const count = () => (kind === 'sql' ? storage.ops : storage.puts);
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(100, 0, 'a'));
  await persist(db, storage);
  await new Promise(setImmediate);
  const whole = rows(db);
  // The first goes out at once and is kept; the eight landing while it is
  // on its way go out together, and storage stops under them: one
  // operation more is the rest of the first flush, a record or a commit.
  db.run('put t {n: -1, s: "first", e: [1, 0, 0, 0, 0, 0, 0, 0]}');
  const first = persist(db, storage);
  const kept = rows(db);
  storage.failAfter = count() + (kind === 'sql' ? 0 : 1);
  const rest = [];
  for (let i = 0; i < 8; i++) {
    db.run(`put t {n: ${2000 + i}, s: "lost", e: [0, 1, 0, 0, 0, 0, 0, 0]}`);
    rest.push(persist(db, storage).then(() => 'stored', () => 'refused'));
  }
  assert.ok((await first) > 0);
  assert.deepEqual(await Promise.all(rest), Array(8).fill('refused'));
  storage.failAfter = Infinity;
  assert.notDeepEqual(kept, whole);
  assert.deepEqual(rows(await back(storage)), kept);
  await assert.rejects(persist(db, storage), /open it again and restore/);
});

each('a checkpoint writes the writes since into a new image', async (make, kind) => {
  const storage = make();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8> @hnsw(cosine))');
  db.run(many(500, 0, 'a', 10));
  await persist(db, storage);
  // Nothing since the image: nothing to write.
  let rowsBefore = storage.rows;
  assert.equal(await checkpoint(db, storage), 0);
  assert.equal(storage.rows, rowsBefore);
  db.run('put t {n: 900, s: "b", e: [0, 0, 1, 0, 0, 0, 0, 0]}');
  await persist(db, storage);
  // Under the 64 KiB a checkpoint waits for by default.
  rowsBefore = storage.rows;
  assert.equal(await checkpoint(db, storage), 0);
  assert.equal(storage.rows, rowsBefore);
  const gen = () => (kind === 'sql' ? storage.tables()[0] : storage.get('fenec:meta').then((m) => m.gen));
  const before = await gen();
  assert.ok((await checkpoint(db, storage, { min: 1 })) > 0);
  assert.notEqual(await gen(), before);
  if (kind === 'values') assert.deepEqual((await storage.get('fenec:meta')).log, [0, 0], 'nothing after the new image');
  assert.deepEqual(rows(await back(storage)), rows(db));
  // And writes after it are kept after it.
  db.run('put t {n: 901, s: "c", e: [0, 0, 0, 1, 0, 0, 0, 0]}');
  await persist(db, storage);
  assert.deepEqual(rows(await back(storage)), rows(db));
});

each('databases under two keys are kept apart', async (make) => {
  const storage = make();
  const a = await open();
  const b = await open();
  for (const [db, n] of [[a, 1], [b, 2]]) {
    db.run('create collection t (n int, s text, e vector<8>)');
    db.run(`put t {n: ${n}, s: "k", e: [1, 0, 0, 0, 0, 0, 0, 0]}`);
  }
  await persist(a, storage, { key: 'alpha' });
  await persist(b, storage, { key: 'beta "quoted"' });
  assert.deepEqual(rows(await back(storage, { key: 'beta "quoted"' })), rows(b));
  assert.deepEqual(rows(await back(storage, { key: 'alpha' })), rows(a));
});

// -------------------------------------------------------------- maintenance

const HOUR = 3_600_000;
const GRACE = 60_000;
const T0 = 1_700_000_000_000;
const iso = (ms) => new Date(ms).toISOString();

/** A database whose clock is `clock.now`, as a test moves it. */
async function at(clock) {
  const db = await open();
  db.now = () => clock.now;
  return db;
}

/** `back` at `clock`: a read leaves out the rows past their time by it. */
async function backAt(storage, clock) {
  const db = await at(clock);
  assert.equal(await restore(db, storage), true);
  return db;
}

/** The generation storage holds: an image written is a new one. */
const generation = async (storage, kind) =>
  kind === 'sql' ? storage.tables().join() : (await storage.get('fenec:meta')).gen;

const expired = (db) => db.rows('get s where expired() count')[0].count;

each('maintain deletes rows past their time, a bounded number a call, and keeps the deletes', async (make, kind) => {
  const storage = make();
  const clock = { now: T0 };
  const db = await at(clock);
  db.run('create collection s (n int, at timestamp @ttl(1h))');
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run('put s $1', [Array.from({ length: 2500 }, (_, n) => ({ n, at: iso(T0 + n) }))]);
  // No time, no expiry.
  db.run('put s $1', [Array.from({ length: 10 }, (_, n) => ({ n: -1 - n }))]);
  // Rows enough beside them that the sweep leaves the image worth keeping.
  db.run(many(4000, 0, 'a', 200));
  await persist(db, storage);
  const first = await generation(storage, kind);
  // Due once the first row is a minute past its time, and set once.
  assert.equal(due(db, storage), T0 + HOUR + GRACE);
  assert.equal(await schedule(db, storage), T0 + HOUR + GRACE);
  db.run('put s {n: 9999, at: $1}', [iso(T0 + 2 * HOUR)]);
  await persist(db, storage);
  assert.equal(await schedule(db, storage), T0 + HOUR + GRACE, 'a row that expires later moves nothing');
  assert.deepEqual([storage.alarm, storage.alarms], [T0 + HOUR + GRACE, 1]);

  // 2 000 rows a minute past their time, 500 past it by less.
  clock.now = T0 + HOUR + GRACE + 1999;
  const rowsBefore = storage.rows;
  let r = await maintain(db, storage, { sweep: 1500 });
  assert.deepEqual([r.swept, r.more, r.next, r.image], [1500, true, clock.now, 0]);
  // Woken to sweep, it stores the deletes and writes no image.
  assert.equal(storage.rows - rowsBefore, kind === 'sql' ? 1 : 2);
  r = await maintain(db, storage, { sweep: 1500 });
  assert.deepEqual([r.swept, r.more, r.image], [500, false, 0]);
  // The next row is due in a millisecond, and swept no sooner than a
  // minute after this sweep.
  assert.equal(r.next, clock.now + 60_000);
  assert.equal(storage.alarm, r.next);
  assert.equal(await generation(storage, kind), first);
  assert.equal(expired(db), 500);

  // Restored, the deletes are there: the rows left past their time are
  // the 500 inside the grace, and the rest reads as it did.
  let again = await backAt(storage, clock);
  assert.equal(expired(again), 500);
  assert.deepEqual(again.rows('get s select n order n'), db.rows('get s select n order n'));
  assert.deepEqual(rows(again), rows(db));

  // And the restored database sweeps the rest, then has nothing due.
  clock.now = T0 + 3 * HOUR + GRACE;
  storage.alarm = clock.now; // the alarm rang
  r = await maintain(again, storage);
  assert.deepEqual([r.swept, r.more, r.next], [501, false, null]);
  assert.equal(due(again, storage), null);
  // The alarm that rang is gone: a row that will expire sets one again.
  storage.alarm = null;
  const alarms = storage.alarms;
  again.run('put s {n: 1, at: $1}', [iso(clock.now)]);
  await persist(again, storage);
  assert.equal(await schedule(again, storage), clock.now + HOUR + GRACE);
  assert.equal(storage.alarms, alarms + 1);
  again = await backAt(storage, clock);
  assert.equal(expired(again), 0);
  assert.deepEqual(again.rows('get s count'), [{ count: 11 }]);
});

each('maintain writes an image by the rule, and not without it', async (make, kind) => {
  const storage = make();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(4000, 0, 'a', 500));
  const image = await persist(db, storage);
  const first = await generation(storage, kind);
  // A tenth of the image written after it: no image owed, no alarm.
  for (let i = 0; i < 4; i++) {
    db.run(many(100, 10_000 + 100 * i, 'b', 500));
    await persist(db, storage);
    assert.equal(due(db, storage), null);
    assert.equal(await schedule(db, storage), null);
  }
  let r = await maintain(db, storage);
  assert.deepEqual([r.image, r.next], [0, null]);
  assert.equal(await generation(storage, kind), first);
  assert.equal(storage.alarms, 0);
  // Past half of it: owed now, and written by `maintain`, not by the
  // request that crossed it -- which folds them at half where no alarm
  // keeps the database, and here only past the whole image.
  for (let i = 4; i < 24; i++) {
    db.run(many(100, 10_000 + 100 * i, 'b', 500));
    assert.notEqual(await persist(db, storage), 0);
  }
  assert.equal(await generation(storage, kind), first, 'no request folded them');
  const before = Date.now();
  const when = due(db, storage);
  assert.ok(when >= before && when <= Date.now(), `due at ${when}`);
  r = await maintain(db, storage);
  assert.ok(r.image > image, `${r.image}`);
  const second = await generation(storage, kind);
  assert.notEqual(second, first);
  assert.equal(due(db, storage), null);
  assert.deepEqual(rows(await back(storage)), rows(db));

  // Past the whole image with no alarm rung, a request folds them still.
  let folded = 0;
  for (let i = 24; i < 100 && !folded; i++) {
    db.run(many(100, 10_000 + 100 * i, 'b', 500));
    if ((await persist(db, storage)) > r.image) folded = i;
  }
  assert.ok(folded > 60, `folded at ${folded}`);
  const third = await generation(storage, kind);
  assert.notEqual(third, second);

  // Deletes write little, and leave the image dead: under a mebibyte and
  // half of what storage holds, not worth an image; past both, worth one.
  db.run('del t where n < 1500');
  await persist(db, storage);
  assert.equal(due(db, storage), null);
  db.run('del t where n < $1', [10_000 + 100 * folded - 200]);
  await persist(db, storage);
  assert.notEqual(due(db, storage), null);
  r = await maintain(db, storage);
  assert.ok(r.image > 0 && r.image < image / 4, `${r.image} of ${image}`);
  assert.notEqual(await generation(storage, kind), third);
  assert.deepEqual(rows(await back(storage)), rows(db));
});

each('an object with nothing due sets no alarm', async (make) => {
  const storage = make();
  const clock = { now: T0 };
  const db = await at(clock);
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(100, 0, 'a'));
  await persist(db, storage);
  for (let i = 0; i < 20; i++) {
    db.run(`put t {n: ${1000 + i}, s: "w", e: [0, 1, 0, 0, 0, 0, 0, 0]}`);
    await persist(db, storage);
    assert.equal(await schedule(db, storage), null);
  }
  assert.deepEqual([storage.alarms, storage.alarm], [0, null]);
  const r = await maintain(db, storage);
  assert.deepEqual([r.swept, r.image, r.next], [0, 0, null]);
  assert.equal(storage.alarms, 0);
  // A row that will expire brings the alarm, once.
  db.run('create collection s (n int, at timestamp @ttl(1m))');
  for (let i = 0; i < 5; i++) {
    db.run('put s {n: $1, at: $2}', [i, iso(T0 + i * 1000)]);
    await persist(db, storage);
    assert.equal(await schedule(db, storage), T0 + 60_000 + GRACE);
  }
  assert.equal(storage.alarms, 1);
});

each('maintain asks for a reload once the module holds more dead than live', async (make) => {
  const storage = make();
  const db = await open();
  db.run('create collection t (n int, s text, e vector<8>)');
  db.run(many(4000, 0, 'a', 500));
  await persist(db, storage);
  // Every row written over, three times: the module keeps every version.
  for (const s of ['b', 'c', 'd']) {
    db.run('set t {s: $1}', [s.repeat(500)]);
    await persist(db, storage);
  }
  const held = db.stats()[0];
  assert.ok(held.dead_bytes > held.bytes / 2, JSON.stringify(held));
  assert.notEqual(due(db, storage), null);
  const r = await maintain(db, storage);
  assert.equal(r.reload, true);
  // Asked once: the database is to be let go, not asked again and again.
  assert.equal(due(db, storage), null);
  assert.equal((await maintain(db, storage)).reload, false);
  // Opened again, it holds the live records alone.
  const again = await back(storage);
  const fresh = again.stats()[0];
  assert.equal(fresh.dead_bytes, 0);
  assert.ok(fresh.bytes < held.bytes / 3, `${fresh.bytes} of ${held.bytes}`);
  assert.deepEqual(rows(again), rows(db));
});

each('storage that fails part way through maintenance keeps the last whole database', async (make, kind) => {
  const count = (storage) => (kind === 'sql' ? storage.ops : storage.puts);
  let failed = 0;
  // The deletes kept as writes after the image, and enough of them to be
  // kept as a new image: stopped after every number of operations each
  // takes.
  for (const [what, size, beside] of [['deletes', 200, 4000], ['image', 10_000, 2000]]) {
    for (let failAfter = 0; failAfter < 10; failAfter++) {
      const storage = make();
      const clock = { now: T0 };
      const db = await at(clock);
      db.run('create collection s (n int, at timestamp @ttl(1h), s text)');
      db.run('create collection t (n int, s text, e vector<8>)');
      if (beside) db.run(many(beside, 0, 'a', 200));
      db.run('put s $1', [Array.from({ length: size }, (_, n) => ({ n, at: iso(T0 + n), s: 'x'.repeat(150) }))]);
      await persist(db, storage);
      clock.now = T0 + HOUR + GRACE + size;
      storage.failAfter = count(storage) + failAfter;
      const done = await maintain(db, storage).then((r) => r, () => null);
      storage.failAfter = Infinity;
      failed += !done;
      if (done) assert.equal(done.swept, size);
      let again = await backAt(storage, clock);
      assert.equal(expired(again), done ? 0 : size, `${what}, failing after ${failAfter}`);
      assert.deepEqual(rows(again), rows(db));
      if (!done) {
        await assert.rejects(maintain(db, storage), /open it again and restore/);
        await assert.rejects(persist(db, storage), /open it again and restore/);
      }
      // The database restored sweeps what storage still holds.
      await maintain(again, storage);
      again = await backAt(storage, clock);
      assert.equal(expired(again), 0, `${what}, after ${failAfter}, then again`);
    }
  }
  // The deletes' row and its commit, or their piece and the record; the
  // image's table, row, drop and commit, or its four pieces and the record.
  assert.ok(failed >= 7, `only ${failed} maintenances were cut short`);
});
