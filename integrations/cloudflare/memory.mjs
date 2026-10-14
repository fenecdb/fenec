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
// ROWS sets the rows (50 000 by default, an image of 27.6 MB). Needs
// web/fenec.wasm (make wasm).

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

if (phase === 'measure') {
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
}
