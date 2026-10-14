// Rows a Durable Object writes to keep a database: `make cloudflare-bench`.
// An object is billed by the row written, $1 a million past the 50 million
// a month included, so this counts them under `wrangler dev` -- workerd, as
// Cloudflare runs it -- through the example Worker with its storage counted
// (bench/worker.js): a single-row `put` a request from 1, 8 and 32 writers
// at once, each request's write kept before its answer, then a checkpoint
// of 10 000 and 50 000 rows of 128 dimensions. Then maintenance, the alarm
// 300 ms after the write that brings it due: bursts of writes over 10 000
// rows of 128 dimensions with quiet moments between them, in which an
// alarm may ring and write an image, and rows that expire swept. ROWS_CONFIG
// names another directory of the counted Worker -- one made from another
// commit's example -- to count its rows the same way. Needs web/fenec.wasm
// (make wasm) and `npm ci` here.

import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { serve as dev } from './dev.mjs';

const here = new URL('.', import.meta.url).pathname;
const config = process.env.ROWS_CONFIG ?? join(here, 'bench');
const WRITES = 256;
const DIM = 128;

async function serve(state, vars = {}) {
  const w = await dev(config, state, vars);
  return {
    async query(tenant, sql, params = []) {
      const res = await fetch(`${w.url}/t/${tenant}/query`, { method: 'POST', body: JSON.stringify({ sql, params }) });
      const out = await res.json();
      if (out.error) throw new Error(out.error);
      return out;
    },
    async checkpoint(tenant) {
      return (await (await fetch(`${w.url}/t/${tenant}/checkpoint`, { method: 'POST' })).json()).bytes;
    },
    async rows(tenant, reset = false) {
      return (await fetch(`${w.url}/t/${tenant}/rows${reset ? '?reset' : ''}`)).json();
    },
    stop: w.stop,
  };
}

let seed = 7;
const rand = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31) - 0.5;
const vector = (d) => Array.from({ length: d }, rand);
const median = (xs) => [...xs].sort((a, b) => a - b)[xs.length >> 1];
const total = (c) => c.sql + c.kv + c.alarms;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const state = await mkdtemp(join(tmpdir(), 'fenecdb-rows-'));
let w = await serve(state);
try {
  console.log('writers  writes  rows: sql  values  alarms  a write  p50 ms');
  for (const writers of [1, 8, 32]) {
    const t = `w${writers}`;
    await w.query(t, 'create collection docs (n int, title text, e vector<8> @hnsw(cosine))');
    await w.query(t, 'put docs $1', [Array.from({ length: 100 }, (_, n) => ({ n, title: `doc ${n}`, e: vector(8) }))]);
    await w.rows(t, true);
    const took = [];
    let n = 1000;
    await Promise.all(
      Array.from({ length: writers }, async () => {
        for (let i = 0; i < WRITES / writers; i++) {
          const t0 = performance.now();
          await w.query(t, 'put docs {n: $1, title: "w", e: $2}', [n++, vector(8)]);
          took.push(performance.now() - t0);
        }
      }),
    );
    const c = await w.rows(t);
    console.log(
      `${String(writers).padStart(7)}  ${String(WRITES).padStart(6)}  ${String(c.sql).padStart(9)}  ${String(c.kv).padStart(6)}  ${String(c.alarms).padStart(6)}  ${(total(c) / WRITES).toFixed(3).padStart(7)}  ${median(took).toFixed(2).padStart(6)}`,
    );
  }

  console.log('\nrows x 128  image bytes  checkpoint rows: sql  values  alarms');
  for (const size of [10_000, 50_000]) {
    const t = `c${size}`;
    await w.query(t, `create collection docs (n int, title text, e vector<${DIM}> @hnsw(cosine))`);
    for (let i = 0; i < size; i += 1000) {
      const rows = Array.from({ length: Math.min(1000, size - i) }, (_, j) => ({ n: i + j, title: `doc ${i + j}`, e: vector(DIM) }));
      await w.query(t, 'put docs $1', [rows]);
    }
    // One write after whatever image the load left, so the checkpoint has
    // writes to fold.
    await w.query(t, 'put docs {n: -1, title: "last", e: $1}', [vector(DIM)]);
    await w.rows(t, true);
    const bytes = await w.checkpoint(t);
    const c = await w.rows(t);
    console.log(`${String(size).padStart(10)}  ${String(bytes).padStart(11)}  ${String(c.sql).padStart(20)}  ${String(c.kv).padStart(6)}  ${String(c.alarms).padStart(6)}`);
  }
  await w.stop();

  // Maintenance: the alarm 300 ms after the write that brings it due, a row
  // swept as soon as it is past its time, a second between sweeps. An
  // example from before maintenance sets an alarm after the first write
  // since the last and checkpoints once the writes since reach 64 KiB.
  w = await serve(state, { QUIET: 300, GRACE: 0, EVERY: 1000 });
  const load = async (t) => {
    await w.query(t, `create collection docs (n int, title text, e vector<${DIM}> @hnsw(cosine))`);
    for (let i = 0; i < 10_000; i += 1000) {
      await w.query(t, 'put docs $1', [Array.from({ length: 1000 }, (_, j) => ({ n: i + j, title: `doc ${i + j}`, e: vector(DIM) }))]);
    }
    // Whatever the load brought due rings before the counting starts.
    await sleep(1500);
    await w.rows(t, true);
  };
  console.log('\nmaintenance, 10 000 x 128          writes  rows: sql  alarms  rang  a write');
  const report = (what, writes, c) =>
    console.log(
      `${what.padEnd(32)}  ${String(writes).padStart(6)}  ${String(c.sql + c.kv).padStart(9)}  ${String(c.alarms).padStart(6)}  ${String(c.rang ?? '-').padStart(4)}  ${(total(c) / writes).toFixed(3).padStart(7)}`,
    );
  // Bursts of single-row writes, a second's quiet after each.
  await load('m1');
  let n = 100_000;
  for (let b = 0; b < 30; b++) {
    for (let i = 0; i < 20; i++) await w.query('m1', 'put docs {n: $1, title: "w", e: $2}', [n++, vector(DIM)]);
    await sleep(1000);
  }
  report('30 bursts of 20 single rows', 600, await w.rows('m1'));
  // Bursts of 500 rows in a request.
  await load('m2');
  for (let b = 0; b < 12; b++) {
    await w.query('m2', 'put docs $1', [Array.from({ length: 500 }, () => ({ n: n++, title: 'w', e: vector(DIM) }))]);
    await sleep(1000);
  }
  report('12 bursts of 500 rows', 12, await w.rows('m2'));
  // Rows that expire: 20 000 at once, then 2 000 a second for 10 s, each
  // second's written in a request. Not on an example from before, which
  // never deletes them.
  if ((await w.rows('m1')).rang !== undefined) {
    await w.query('m3', 'create collection s (n int, at timestamp @ttl(2s), title text)');
    await w.rows('m3', true);
    let at = new Date().toISOString();
    for (let i = 0; i < 20_000; i += 1000) {
      await w.query('m3', 'put s $1', [Array.from({ length: 1000 }, (_, j) => ({ n: i + j, at, title: `session ${i + j}` }))]);
    }
    // The sweep alone: the writes set the first alarm, uncounted here.
    await w.rows('m3', true);
    await sleep(5000);
    report('then 20 000 swept, 10 000 a call', 1, await w.rows('m3', true));
    for (let s = 0; s < 10; s++) {
      at = new Date().toISOString();
      await w.query('m3', 'put s $1', [Array.from({ length: 2000 }, (_, j) => ({ n: s * 2000 + j, at, title: `session ${j}` }))]);
      await sleep(1000);
    }
    await sleep(5000);
    report('2 000 expiring a second for 10 s', 10, await w.rows('m3'));
  }
} finally {
  await w.stop();
  await rm(state, { recursive: true, force: true });
}
