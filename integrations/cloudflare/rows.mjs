// Rows a Durable Object writes to keep a database: `make cloudflare-bench`.
// An object is billed by the row written, $1 a million past the 50 million
// a month included, so this counts them under `wrangler dev` -- workerd, as
// Cloudflare runs it -- through the example Worker with its storage counted
// (bench/worker.js): a single-row `put` a request from 1, 8 and 32 writers
// at once, each request's write kept before its answer, then a checkpoint
// of 10 000 and 50 000 rows of 128 dimensions. Needs web/fenec.wasm (make
// wasm) and `npm ci` here.

import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';

const here = new URL('.', import.meta.url).pathname;
const config = process.env.ROWS_CONFIG ?? join(here, 'bench');
const WRITES = 256;
const DIM = 128;

const port = () =>
  new Promise((res) => {
    const s = createServer().listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => res(port));
    });
  });

async function serve(state) {
  const p = await port();
  const child = spawn(
    join(here, 'node_modules/.bin/wrangler'),
    ['dev', '--config', 'wrangler.jsonc', '--port', String(p), '--ip', '127.0.0.1', '--persist-to', state, '--show-interactive-dev-session=false'],
    { cwd: config, stdio: ['ignore', 'pipe', 'pipe'], detached: true, env: { ...process.env, WRANGLER_SEND_METRICS: 'false' } },
  );
  let log = '';
  child.stdout.on('data', (d) => (log += d));
  child.stderr.on('data', (d) => (log += d));
  while (!log.includes('Ready on')) {
    if (child.exitCode !== null) throw new Error(log);
    await new Promise((r) => setTimeout(r, 50));
  }
  const url = `http://127.0.0.1:${p}`;
  const stopped = new Promise((r) => child.once('exit', r));
  return {
    async query(tenant, sql, params = []) {
      const res = await fetch(`${url}/t/${tenant}/query`, { method: 'POST', body: JSON.stringify({ sql, params }) });
      const out = await res.json();
      if (out.error) throw new Error(out.error);
      return out;
    },
    async checkpoint(tenant) {
      return (await (await fetch(`${url}/t/${tenant}/checkpoint`, { method: 'POST' })).json()).bytes;
    },
    async rows(tenant, reset = false) {
      return (await fetch(`${url}/t/${tenant}/rows${reset ? '?reset' : ''}`)).json();
    },
    async stop() {
      process.kill(-child.pid, 'SIGINT');
      await stopped;
    },
  };
}

let seed = 7;
const rand = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31) - 0.5;
const vector = (d) => Array.from({ length: d }, rand);
const median = (xs) => [...xs].sort((a, b) => a - b)[xs.length >> 1];
const total = (c) => c.sql + c.kv + c.alarms;

const state = await mkdtemp(join(tmpdir(), 'fenecdb-rows-'));
const w = await serve(state);
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
} finally {
  await w.stop();
  await rm(state, { recursive: true, force: true });
}
