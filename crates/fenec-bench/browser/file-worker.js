// What keeping a database costs a page, measured where a page keeps it: in a
// dedicated worker (`make file-bench`). A database of 61 000 rows with
// 128-dim vectors, a 32 MB image, kept in IndexedDB by `persist` and in a
// file of the origin private file system by `openFile`: the first image,
// one new row stored, thirty times, and the database opened again.
// Both stores are emptied before and after.

import { Fenec, persist, restore, openFile } from '/web/fenec.js';

const ROWS = 61000;
const DIM = 128;
const WRITES = 30;
const MODULE = '/web/fenec.wasm';
const FILE = 'file-bench.fenec';

const say = (msg) => postMessage({ msg });
const now = () => performance.now();

function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

// The mean is of the thirty together, which a coarse clock does not round.
function summary(ms) {
  const s = [...ms].sort((a, b) => a - b);
  const at = (q) => s[Math.min(s.length - 1, Math.floor(q * s.length))];
  return { p50: at(0.5), mean: s.reduce((a, b) => a + b, 0) / s.length, max: s[s.length - 1], runs: s.length };
}

/** The smallest step `performance.now()` takes. */
function tick() {
  let step = Infinity;
  for (let i = 0; i < 50; i++) {
    const a = now();
    let b = now();
    while (b === a) b = now();
    step = Math.min(step, b - a);
  }
  return step;
}

async function empty() {
  await new Promise((res) => {
    const req = indexedDB.deleteDatabase('fenecdb');
    req.onsuccess = req.onerror = req.onblocked = () => res();
  });
  const root = await navigator.storage.getDirectory();
  for (const name of [FILE, `${FILE}~`, 'small.fenec', 'small.fenec~']) {
    await root.removeEntry(name).catch(() => {});
  }
}

/** The database the stores are handed: its image. */
async function image() {
  const db = await Fenec.open(MODULE);
  db.run(`create collection docs (title text, v vector<${DIM}>)`);
  const r = rng(1);
  for (let at = 0; at < ROWS; at += 500) {
    const rows = [];
    for (let i = at; i < Math.min(ROWS, at + 500); i++) {
      const v = Array.from({ length: DIM }, () => (r() * 2 - 1).toFixed(4));
      rows.push(`{title: "row ${i}", v: [${v.join(',')}]}`);
    }
    db.run(`put docs [${rows.join(',')}]`);
  }
  const bytes = db.snapshot();
  db.close();
  return bytes;
}

async function loaded(bytes) {
  const db = await Fenec.open(MODULE);
  db.load(bytes);
  return db;
}

const put = (db, r, i) =>
  db.run('put docs {title: $1, v: $2}', [`new ${i}`, Array.from({ length: DIM }, () => r() * 2 - 1)]);

async function measure() {
  await empty();
  say('building the database');
  const bytes = await image();
  const out = {
    agent: navigator.userAgent,
    isolated: globalThis.crossOriginIsolated ?? false,
    tick: tick(),
    rows: ROWS,
    dim: DIM,
    image_bytes: bytes.length,
  };

  say('a row, stored nowhere');
  {
    const db = await loaded(bytes);
    const r = rng(2);
    const ms = [];
    for (let i = 0; i < WRITES; i++) {
      const t = now();
      put(db, r, i);
      ms.push(now() - t);
    }
    out.statement = summary(ms);
    db.close();
  }

  say('IndexedDB: persist and restore');
  {
    const db = await loaded(bytes);
    let t = now();
    await persist(db, 'bench');
    out.idb_image = now() - t;
    const r = rng(2);
    const ms = [];
    for (let i = 0; i < WRITES; i++) {
      put(db, r, i);
      t = now();
      await persist(db, 'bench');
      ms.push(now() - t);
    }
    out.idb_row = summary(ms);
    db.close();
    const again = await Fenec.open(MODULE);
    t = now();
    await restore(again, 'bench');
    out.idb_open = now() - t;
    out.idb_rows = again.rows('get docs count')[0].count;
    again.close();
  }

  say('OPFS: openFile, a row a statement, and open again');
  {
    const db = await loaded(bytes);
    let t = now();
    const file = await openFile(db, FILE);
    out.file_image = now() - t;
    const r = rng(2);
    const ms = [];
    for (let i = 0; i < WRITES; i++) {
      t = now();
      put(db, r, i);
      ms.push(now() - t);
    }
    // The statement and its flush: `run` answers once the file holds it.
    out.file_row = summary(ms);
    out.file_bytes = file.size;
    file.close();
    db.close();
    const again = await Fenec.open(MODULE);
    t = now();
    const reopened = await openFile(again, FILE);
    out.file_open = now() - t;
    out.file_rows = again.rows('get docs count')[0].count;
    reopened.close();
    again.close();
  }

  say('a small database: an append against a new image');
  {
    const db = await Fenec.open(MODULE);
    const file = await openFile(db, 'small.fenec');
    db.run('create collection docs (title text, v vector<8>)');
    const r = rng(3);
    const append = [];
    const image = [];
    for (let i = 0; i < WRITES; i++) {
      let t = now();
      db.run('put docs {title: $1, v: $2}', [`row ${i}`, Array.from({ length: 8 }, () => r())]);
      append.push(now() - t);
      // A compact writes a new image as a fold does: beside, over, emptied.
      t = now();
      db.run('compact');
      image.push(now() - t);
    }
    out.small_append = summary(append);
    out.small_image = summary(image);
    out.small_bytes = file.size;
    file.close();
    db.close();
  }
  await empty();
  return out;
}

measure().then(
  (result) => postMessage({ result }),
  (e) => postMessage({ result: { error: String(e?.stack ?? e) } }),
);
