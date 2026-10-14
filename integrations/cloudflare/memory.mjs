// What a restore holds at its peak: `make cloudflare-bench`. A Worker's
// isolate has 128 MB, the module's memory in it, so a restore that reads
// the image into the page whole -- then joined, then copied into the
// module -- needs room for it two and three times over. Here, in Node: a
// database of rows of a text, an int and a 128-dim vector under HNSW,
// written 1 000 a persist (an image, then the writes after it, as an
// object's requests leave them), kept in the stand-ins of fakes.js -- a
// SQLite-backed object's rows in a node:sqlite file, a key-value backed
// object's values in a file read back in -- and then restored, each in a
// process of its own: the module's memory after it, and the most the
// JavaScript heap and the array buffers held above where they stood
// before, what is still reachable at every copy of a part
// (`Uint8Array.prototype.set`), and the most the process held resident.
// ROWS sets the rows (50 000 by default, an image of 27.6 MB). Then
// maintenance, in a process of its own: the same rows with three in five
// past their time, swept by `maintain` 10 000 an alarm, an image written
// once half of storage is dead; then the rows left written over 1 000 a
// request, `maintain` run whenever it comes due, as an alarm would, until
// it asks for a reload, and a new module restored -- the module's memory
// after each step, and the most the JavaScript heap and the array buffers
// held at any storage call of it. Needs web/fenec.wasm (make wasm).

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { serialize, deserialize } from 'node:v8';
import { DatabaseSync } from 'node:sqlite';
import * as kept from './index.js';
import { Rows, Values } from './fakes.js';

const web = new URL('../../web/', import.meta.url);
const ROWS = Number(process.env.ROWS ?? 50_000);
const DIM = 128;
const MB = 1e6;

// The module's memory, which `Fenec` keeps to itself: the last one made.
let memory = null;
const instantiate = WebAssembly.instantiate;
WebAssembly.instantiate = async (...a) => {
  const out = await instantiate(...a);
  memory = (out instanceof WebAssembly.Instance ? out : out.instance).exports.memory;
  return out;
};
const { Fenec } = await import(new URL('fenec.js', web));
const wasm = readFileSync(new URL('fenec.wasm', web));

const [, , phase, kind, dir] = process.argv;

if (phase === 'maintain') {
  // A clock moved by hand: the rows written an hour and a half ago are past
  // their time by more than the grace.
  const clock = { now: Date.now() };
  const storage = new Rows(DatabaseSync);
  const use = () => {
    globalThis.gc();
    const m = process.memoryUsage();
    return m.heapUsed + m.arrayBuffers;
  };
  // The most the JavaScript held at any storage call of a step, above
  // what it held as the step began: the rows of an image pass through it.
  let peak = 0;
  const exec = storage.sql.exec;
  storage.sql.exec = (...a) => {
    peak = Math.max(peak, use());
    return exec(...a);
  };
  let from = use();
  const step = (what, db, extra = '') => {
    console.log(`${what.padEnd(34)}  ${(memory.buffer.byteLength / MB).toFixed(1).padStart(6)} MB  ${(Math.max(0, peak - from) / MB).toFixed(1).padStart(6)} MB  ${extra}`);
    from = peak = use();
  };
  let seed = 7;
  const rand = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31) - 0.5;
  console.log('step                                module    JS peak  done');
  {
    const db = await Fenec.open(wasm);
    db.now = () => clock.now;
    db.run(`create collection docs (n int, at timestamp @ttl(1h), title text, e vector<${DIM}> @hnsw(cosine))`);
    for (let i = 0; i < ROWS; i += 1000) {
      const docs = Array.from({ length: Math.min(1000, ROWS - i) }, (_, j) => ({
        n: i + j,
        at: new Date(clock.now - ((i + j) % 5 < 3 ? 5_400_000 : 0)).toISOString(),
        title: `doc ${i + j}`,
        e: Array.from({ length: DIM }, rand),
      }));
      db.run('put docs $1', [docs]);
      await kept.persist(db, storage);
    }
    db.close();
  }
  /** A new module, restored from what storage holds: what a reload is. */
  const opened = async () => {
    const db = await Fenec.open(wasm);
    db.now = () => clock.now;
    await kept.restore(db, storage);
    return db;
  };
  let db = await opened();
  step(`${ROWS} x ${DIM} restored, 3/5 expired`, db);
  /** `maintain`, as an alarm runs it, and the reload it asks for. */
  const maintained = async (what) => {
    const r = await kept.maintain(db, storage);
    const st = db.stats()[0];
    const done = [
      r.swept && `${r.swept} swept`,
      r.image ? `an image of ${(r.image / MB).toFixed(1)} MB` : 'no image',
      `${(st.dead_bytes / MB).toFixed(1)} MB dead of ${(st.bytes / MB).toFixed(1)}`,
      r.reload && 'a reload',
    ];
    step(what, db, done.filter(Boolean).join(', '));
    if (r.reload) {
      db.close();
      db = await opened();
      step('reloaded: a new module restored', db);
    }
    return r;
  };
  while ((await maintained('maintained')).more);
  // The live rows written over, a request at a time -- 2 500 numbers, of
  // which the sweep left 1 000 -- `maintain` run each time it is due, as an
  // alarm would, until it has asked for a reload.
  let reloads = 0;
  for (let round = 0; reloads === 0; round++) {
    for (let i = 0; i < ROWS && reloads === 0; i += 2500) {
      db.run('set docs {title: $1} where n >= $2 and n < $3', [`${round} `.repeat(10), i, i + 2500]);
      await kept.persist(db, storage);
      if (kept.due(db, storage) > clock.now) continue;
      reloads += (await maintained(`written over ${(round + (i + 2500) / ROWS).toFixed(2)} times`)).reload;
    }
  }
} else if (phase === 'measure') {
  // Each value a buffer of its own, as a value read out of storage is: as
  // deserialized they are views of one, which a clone copies whole.
  const own = (v) => (v instanceof Uint8Array ? v.slice() : v);
  const storage =
    kind === 'sql'
      ? new Rows(DatabaseSync, join(dir, 'rows.sqlite'))
      : Object.assign(new Values(), { map: new Map([...deserialize(readFileSync(join(dir, 'values.bin')))].map(([k, v]) => [k, own(v)])) });
  const db = await Fenec.open(wasm);
  globalThis.gc();
  const use = () => {
    const m = process.memoryUsage();
    return m.heapUsed + m.arrayBuffers;
  };
  const base = use();
  const wasmBefore = memory.buffer.byteLength;
  // What is still reachable, a collection forced first: a part read and
  // copied is garbage, which V8 collects before an isolate runs out --
  // left to it, the rows of one restore piled up to the image's size here.
  let peak = base;
  const set = Uint8Array.prototype.set;
  Uint8Array.prototype.set = function (...a) {
    globalThis.gc();
    peak = Math.max(peak, use());
    return set.apply(this, a);
  };
  const rssBefore = process.memoryUsage().rss;
  if (!(await kept.restore(db, storage))) throw new Error('nothing kept');
  Uint8Array.prototype.set = set;
  peak = Math.max(peak, use());
  const wasmAfter = memory.buffer.byteLength;
  const maxRss = process.resourceUsage().maxRSS * 1024;
  const count = db.rows('get docs count')[0].count;
  console.log(JSON.stringify({ kind, count, wasmBefore, wasmAfter, js: peak - base, rss: maxRss - rssBefore }));
} else {
  const dir = mkdtempSync(join(tmpdir(), 'fenecdb-memory-'));
  try {
    // Kept the way requests keep it, a thousand rows a persist.
    const rows = new Rows(DatabaseSync, join(dir, 'rows.sqlite'));
    const values = new Values();
    let seed = 7;
    const rand = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31) - 0.5;
    const db = await Fenec.open(wasm);
    db.run(`create collection docs (n int, title text, e vector<${DIM}> @hnsw(cosine))`);
    const twin = await Fenec.open(wasm);
    twin.run(`create collection docs (n int, title text, e vector<${DIM}> @hnsw(cosine))`);
    for (let i = 0; i < ROWS; i += 1000) {
      const docs = Array.from({ length: Math.min(1000, ROWS - i) }, (_, j) => ({ n: i + j, title: `doc ${i + j}`, e: Array.from({ length: DIM }, rand) }));
      db.run('put docs $1', [docs]);
      twin.run('put docs $1', [docs]);
      // A version from before the SQL layout has the key-value one alone.
      if (kept.ROW) await kept.persist(db, rows);
      await kept.persist(twin, values);
    }
    const image = db.snapshot().length;
    rows.db.close();
    writeFileSync(join(dir, 'values.bin'), serialize(values.map));
    console.log(`${ROWS} rows x ${DIM}: an image of ${(image / MB).toFixed(1)} MB`);
    console.log('kept as   module before  module after  JS reachable above  max RSS above');
    for (const k of kept.ROW ? ['sql', 'values'] : ['values']) {
      const out = execFileSync(process.execPath, ['--expose-gc', '--no-warnings', new URL(import.meta.url).pathname, 'measure', k, dir], { encoding: 'utf8' });
      const r = JSON.parse(out.trim().split('\n').at(-1));
      if (r.count !== ROWS) throw new Error(`${k}: ${r.count} rows came back`);
      console.log(
        `${k.padEnd(7)}  ${(r.wasmBefore / MB).toFixed(1).padStart(10)} MB  ${(r.wasmAfter / MB).toFixed(1).padStart(9)} MB  ${(r.js / MB).toFixed(1).padStart(15)} MB  ${(r.rss / MB).toFixed(1).padStart(10)} MB`,
      );
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  if (kept.maintain) {
    console.log('');
    execFileSync(process.execPath, ['--expose-gc', '--no-warnings', new URL(import.meta.url).pathname, 'maintain'], { stdio: 'inherit' });
  }
}
