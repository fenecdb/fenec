// What a Durable Object's first answer costs after it was evicted: `make
// cloudflare-bench`. The example Worker under `wrangler dev` -- workerd, as
// Cloudflare runs it -- is loaded with rows of a text, an int and a 128-dim
// vector under HNSW, 1 000 a request, each kept (`persist`) before it is
// answered; then the server is stopped and started again over the same
// storage, and the first `near` is timed -- the object made, the module
// instantiated, the database restored from its storage, the query run --
// against the ones after it -- as the writes left the storage, an image and
// the writes kept after it, and after a checkpoint wrote a new image with
// the graph. Median of five restarts a size. Needs
// web/fenec.wasm (make wasm) and `npm ci` here.

import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';

const here = new URL('.', import.meta.url).pathname;
const DIM = 128;
const SIZES = (process.env.SIZES ?? '1000,10000,50000').split(',').map(Number);
const RESTARTS = 5;

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
    { cwd: join(here, 'example'), stdio: ['ignore', 'pipe', 'pipe'], detached: true, env: { ...process.env, WRANGLER_SEND_METRICS: 'false' } },
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
      await fetch(`${url}/t/${tenant}/checkpoint`, { method: 'POST' });
    },
    async stop() {
      process.kill(-child.pid, 'SIGINT');
      await stopped;
    },
  };
}

let seed = 7;
const rand = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31) - 0.5;
const vector = () => Array.from({ length: DIM }, rand);
const median = (xs) => [...xs].sort((a, b) => a - b)[xs.length >> 1];

console.log(`rows x ${DIM}  load (rows/s)  first near: as written  after a checkpoint  near after it`);
for (const n of SIZES) {
  const state = await mkdtemp(join(tmpdir(), 'fenecdb-cold-'));
  try {
    let w = await serve(state);
    await w.query('t', `create collection docs (n int, title text, e vector<${DIM}> @hnsw(cosine))`);
    const t0 = performance.now();
    for (let i = 0; i < n; i += 1000) {
      const rows = Array.from({ length: Math.min(1000, n - i) }, (_, j) => `{n: ${i + j}, title: "doc ${i + j}", e: [${vector().map((x) => x.toFixed(5))}]}`);
      await w.query('t', `put docs [${rows.join(', ')}]`);
    }
    const load = n / ((performance.now() - t0) / 1000);
    await w.stop();
    const firsts = {};
    const warm = [];
    for (const when of ['as the writes left it', 'after a checkpoint']) {
    if (when === 'after a checkpoint') {
      w = await serve(state);
      await w.checkpoint('t');
      await w.stop();
    }
    const first = (firsts[when] = []);
    for (let r = 0; r < RESTARTS; r++) {
      w = await serve(state);
      const q = vector();
      let t = performance.now();
      const got = await w.query('t', 'get docs select n near e $1 limit 10', [q]);
      first.push(performance.now() - t);
      if (got.rows.length !== 10) throw new Error('fewer than ten rows');
      for (let k = 0; k < 20; k++) {
        t = performance.now();
        await w.query('t', 'get docs select n near e $1 limit 10', [vector()]);
        warm.push(performance.now() - t);
      }
      await w.stop();
    }
    }
    console.log(
      `${String(n).padStart(6)}  ${Math.round(load).toString().padStart(12)}  ${median(firsts['as the writes left it']).toFixed(0).padStart(14)} ms  ${median(firsts['after a checkpoint']).toFixed(0).padStart(16)} ms  ${median(warm).toFixed(2).padStart(10)} ms`,
    );
  } finally {
    await rm(state, { recursive: true, force: true });
  }
}
