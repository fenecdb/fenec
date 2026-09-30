// The example Worker under `wrangler dev`, workerd running it as Cloudflare
// does: each tenant's database in a Durable Object, written, then read back
// after the server is stopped and started again over the same storage.
// Needs web/fenec.wasm (make wasm) and `npm ci` here, which brings wrangler.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { access, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';

const here = new URL('.', import.meta.url).pathname;
const ready = await Promise.all([
  access(new URL('../../web/fenec.wasm', import.meta.url)),
  access(new URL('node_modules/.bin/wrangler', import.meta.url)),
]).then(() => true, () => false);
const skip = ready ? false : 'needs web/fenec.wasm (make wasm) and wrangler (npm ci)';

function freePort() {
  return new Promise((res) => {
    const s = createServer().listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => res(port));
    });
  });
}

/** Every `wrangler dev` started, for a failed test to end them all. */
const running = new Set();

/** `wrangler dev` over `state`, until it answers; `stop()` ends it. */
async function serve(state) {
  const port = await freePort();
  const child = spawn(
    join(here, 'node_modules/.bin/wrangler'),
    ['dev', '--config', 'wrangler.jsonc', '--port', String(port), '--ip', '127.0.0.1', '--persist-to', state, '--show-interactive-dev-session=false'],
    // A group of its own: wrangler starts workerd beside it, and killing
    // wrangler alone left those holding its output open, and the test
    // process with them.
    { cwd: join(here, 'example'), stdio: ['ignore', 'pipe', 'pipe'], detached: true, env: { ...process.env, WRANGLER_SEND_METRICS: 'false' } },
  );
  running.add(child);
  child.once('exit', () => running.delete(child));
  let log = '';
  child.stdout.on('data', (d) => (log += d));
  child.stderr.on('data', (d) => (log += d));
  const url = `http://127.0.0.1:${port}`;
  for (let i = 0; ; i++) {
    try {
      await fetch(url);
      break;
    } catch {
      if (i > 600 || child.exitCode !== null) throw new Error(`wrangler dev did not start:\n${log}`);
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  const stopped = new Promise((r) => child.once('exit', r));
  return {
    async query(tenant, sql, params = []) {
      const res = await fetch(`${url}/t/${tenant}/query`, {
        method: 'POST',
        body: JSON.stringify({ sql, params }),
      });
      return res.json();
    },
    async stop() {
      process.kill(-child.pid, 'SIGINT');
      await stopped;
    },
  };
}

test('a tenant database lives in a Durable Object and comes back', { skip, timeout: 180_000 }, async () => {
  const state = await mkdtemp(join(tmpdir(), 'fenecdb-worker-'));
  try {
    let w = await serve(state);
    assert.deepEqual(
      await w.query('acme', 'create collection docs (n int, title text, e vector<8> @hnsw(cosine))'),
      { kind: 'ok', message: 'collection `docs` created' },
    );
    // An image of several pieces, then writes appended after it.
    const rows = Array.from({ length: 2000 }, (_, i) => `{n: ${i}, title: "doc ${i} ${'x'.repeat(100)}", e: [${[1, i, 2, 3, 4, 5, 6, 7]}]}`);
    assert.deepEqual(await w.query('acme', `put docs [${rows.join(', ')}]`), { kind: 'affected', count: 2000 });
    for (let i = 0; i < 5; i++) {
      const put = await w.query('acme', 'put docs {n: $1, title: "late", e: $2}', [2000 + i, [0, 1, 0, 0, 0, 0, 0, i]]);
      assert.deepEqual(put, { kind: 'affected', count: 1 });
    }
    await w.query('globex', 'create collection docs (n int)');
    const count = await w.query('acme', 'get docs count');
    const near = await w.query('acme', 'get docs select n near e $1 limit 3', [[0, 1, 0, 0, 0, 0, 0, 4]]);
    assert.deepEqual(count.rows, [{ count: 2005 }]);
    assert.equal(near.rows[0].n, 2004);
    await w.stop();

    // Started again over the same storage: every object opens afresh.
    w = await serve(state);
    assert.deepEqual(await w.query('acme', 'get docs count'), count);
    assert.deepEqual(await w.query('acme', 'get docs select n near e $1 limit 3', [[0, 1, 0, 0, 0, 0, 0, 4]]), near);
    assert.deepEqual((await w.query('globex', 'get docs count')).rows, [{ count: 0 }]);
    assert.match((await w.query('initech', 'get docs count')).error, /not found/);
    await w.stop();
  } finally {
    // A failed assertion leaves a server running, which holds the test
    // process open: it waited out every timeout.
    for (const child of running) process.kill(-child.pid, 'SIGKILL');
    await rm(state, { recursive: true, force: true });
  }
});
