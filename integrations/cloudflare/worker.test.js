// The example Worker under `wrangler dev`, workerd running it as Cloudflare
// does: each tenant's database in a Durable Object, written, then read back
// after the server is stopped and started again over the same storage.
// Needs web/fenec.wasm (make wasm) and `npm ci` here, which brings wrangler.

import { after, test } from 'node:test';
import assert from 'node:assert/strict';
import { access, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { readyUrl, running, serve as dev } from './dev.mjs';

const ready = await Promise.all([
  access(new URL('../../web/fenec.wasm', import.meta.url)),
  access(new URL('node_modules/.bin/wrangler', import.meta.url)),
]).then(() => true, () => false);
const skip = ready ? false : 'needs web/fenec.wasm (make wasm) and wrangler (npm ci)';

// A test cut short by its timeout runs no `finally`, and the wrangler it
// started held the test process open for good.
after(() => {
  for (const child of running) process.kill(-child.pid, 'SIGKILL');
});

/**
 * `wrangler dev` over `state` (dev.mjs), until it says it is ready; `stop()`
 * ends it. `dir` is the Worker's: `bench` is the example with its storage
 * counted, which answers the alarm set and the generations kept too. `vars`
 * its environment.
 */
async function serve(state, { dir = 'example', vars = {} } = {}) {
  const w = await dev(dir, state, vars);
  return {
    async query(tenant, sql, params = []) {
      const res = await fetch(`${w.url}/t/${tenant}/query`, {
        method: 'POST',
        body: JSON.stringify({ sql, params }),
      });
      return res.json();
    },
    /** The bench Worker's counts: rows written, alarms set and rung, the alarm set and the generations. */
    async kept(tenant) {
      return (await fetch(`${w.url}/t/${tenant}/rows`)).json();
    },
    stop: w.stop,
  };
}

test('the ready line is read through the colours wrangler may write it in', () => {
  assert.equal(readyUrl('[wrangler:info] Ready on http://127.0.0.1:57886\n'), 'http://127.0.0.1:57886');
  assert.equal(readyUrl('[wrangler:info] Ready on \x1b[32mhttp://127.0.0.1:57886\x1b[39m\n'), 'http://127.0.0.1:57886');
  // Not before the line is whole: a port cut short is another port.
  assert.equal(readyUrl('[wrangler:info] Ready on http://127.0.0.1:57'), null);
});

test('wrangler dev that ends before it is ready fails the start, and one asked for colours starts', { skip, timeout: 120_000 }, async () => {
  const state = await mkdtemp(join(tmpdir(), 'fenecdb-dev-'));
  try {
    // A directory with no Worker in it: wrangler gives up, and so does the start.
    await assert.rejects(dev(state, state), /ended before it was ready/);
    const colour = process.env.FORCE_COLOR;
    process.env.FORCE_COLOR = '1';
    let w;
    try {
      w = await dev('example', state);
    } finally {
      if (colour === undefined) delete process.env.FORCE_COLOR;
      else process.env.FORCE_COLOR = colour;
    }
    const res = await fetch(`${w.url}/t/a/query`, { method: 'POST', body: JSON.stringify({ sql: 'create collection d (n int)' }) });
    assert.equal(res.status, 200);
    await w.stop();
  } finally {
    for (const child of running) process.kill(-child.pid, 'SIGKILL');
    await rm(state, { recursive: true, force: true });
  }
});

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
    assert.match((await w.query('initech', 'get docs count')).error, /not found/);
    // A cold object sent many writes at once: each lands, and comes back.
    // Opened by whichever request found it cold, two requests could each
    // restore a database of their own and append as if alone.
    const burst = await Promise.all(
      Array.from({ length: 32 }, (_, i) => w.query('globex', 'put docs {n: $1}', [i])),
    );
    assert.ok(burst.every((r) => r.kind === 'affected' && r.count === 1), JSON.stringify(burst));
    assert.deepEqual((await w.query('globex', 'get docs count')).rows, [{ count: 32 }]);
    await w.stop();

    w = await serve(state);
    assert.deepEqual((await w.query('globex', 'get docs count')).rows, [{ count: 32 }]);
    assert.deepEqual(await w.query('acme', 'get docs count'), count);
    await w.stop();
  } finally {
    // A failed assertion leaves a server running, which holds the test
    // process open: it waited out every timeout.
    for (const child of running) process.kill(-child.pid, 'SIGKILL');
    await rm(state, { recursive: true, force: true });
  }
});

/** Waits for `ok()` to hold, asking every 100 ms; fails with `what` past `ms`. */
async function until(ok, what, ms = 30_000) {
  const end = Date.now() + ms;
  for (;;) {
    const got = await ok();
    if (got) return got;
    if (Date.now() > end) throw new Error(`waited ${ms} ms for ${what}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

test('maintenance runs from alarms set only when it is due', { skip, timeout: 180_000 }, async () => {
  const state = await mkdtemp(join(tmpdir(), 'fenecdb-alarms-'));
  // The alarm a second after the write that brings it due, a row swept as
  // soon as it is past its time, 100 rows an alarm.
  const vars = { QUIET: 1000, GRACE: 0, EVERY: 0, SWEEP: 100 };
  try {
    let w = await serve(state, { dir: 'bench', vars });
    const ok = (r) => assert.ok(!r.error, r.error);

    // An idle object: written, nothing due, no alarm.
    ok(await w.query('idle', 'create collection docs (n int, title text)'));
    for (let i = 0; i < 10; i++) ok(await w.query('idle', 'put docs {n: $1, title: "t"}', [i]));
    let k = await w.kept('idle');
    assert.deepEqual([k.alarms, k.alarm, k.rang], [0, null, 0]);

    // Rows that expire: one alarm set by the writes, and 350 rows swept
    // over four of them, 100 an alarm.
    ok(await w.query('ttl', 'create collection s (n int, at timestamp @ttl(1s))'));
    ok(await w.query('ttl', 'create collection keep (n int)'));
    ok(await w.query('ttl', 'put keep {n: 1}'));
    const now = Date.now();
    ok(await w.query('ttl', 'put s $1', [Array.from({ length: 350 }, (_, n) => ({ n, at: new Date(now).toISOString() }))]));
    k = await w.kept('ttl');
    // Set once, past the rows' time -- unless it rang already.
    if (k.rang === 0) {
      assert.equal(k.alarms, 1);
      assert.ok(k.alarm >= now + 1000, `the alarm at ${k.alarm - now} ms`);
    }
    k = await until(async () => {
      const r = await w.kept('ttl');
      return r.rang >= 4 && r.alarm === null && r;
    }, 'the sweep');
    assert.equal(k.rang, 4);
    assert.deepEqual((await w.query('ttl', 'get s where expired() count')).rows, [{ count: 0 }]);
    assert.deepEqual((await w.query('ttl', 'get keep count')).rows, [{ count: 1 }]);

    // A database written past half its image: an image by the alarm, not
    // by the request. Under it: none, and no alarm.
    ok(await w.query('grow', 'create collection docs (n int, title text)'));
    const page = (from, n) => Array.from({ length: n }, (_, i) => ({ n: from + i, title: `${from + i} ${'x'.repeat(300)}` }));
    ok(await w.query('grow', 'put docs $1', [page(0, 3000)]));
    const first = (await w.kept('grow')).tables;
    for (let i = 0; i < 3; i++) ok(await w.query('grow', 'put docs $1', [page(10_000 + 100 * i, 100)]));
    k = await w.kept('grow');
    assert.deepEqual([k.tables, k.alarm, k.alarms], [first, null, 0]);
    // The write that brings the image due sets the alarm and writes the
    // one row it would have anyway: while no alarm has rung, the
    // generation is the one before. Written until the alarm is set, or has
    // rung already, its image written and none set after it.
    let pages = 3;
    for (; !(k = await w.kept('grow')).alarm && k.rang === 0; pages++) {
      assert.ok(pages < 40, 'no image came due');
      ok(await w.query('grow', 'put docs $1', [page(10_000 + 100 * pages, 100)]));
    }
    if (k.rang === 0) assert.deepEqual(k.tables, first, 'a request folded the writes');
    k = await until(async () => {
      const r = await w.kept('grow');
      return r.rang >= 1 && r.alarm === null && r;
    }, 'the image');
    assert.notDeepEqual(k.tables, first);
    assert.equal(k.tables.length, 1);

    // Every row written over until the module holds more dead records
    // than live: the alarm writes an image without them, and the database
    // is opened again from it, its module's memory given back.
    ok(await w.query('churn', 'create collection docs (n int, title text)'));
    ok(await w.query('churn', 'put docs $1', [page(0, 4000)]));
    for (const c of 'abcd') ok(await w.query('churn', 'set docs {title: $1}', [c.repeat(400)]));
    // Read until a read finds the database opened again: the alarm lets it
    // go once no call holds it, and the next call restores.
    k = await until(async () => {
      const r = await w.query('churn', 'get docs select title where n = 7');
      assert.deepEqual(r.rows, [{ title: 'd'.repeat(400) }]);
      const c = await w.kept('churn');
      return c.opens >= 2 && c;
    }, 'the reload');
    assert.ok(k.rang >= 1);
    await w.stop();

    // Started again: the deletes and the image were kept.
    w = await serve(state, { dir: 'bench', vars });
    assert.deepEqual((await w.query('ttl', 'get s where expired() count')).rows, [{ count: 0 }]);
    assert.deepEqual((await w.query('grow', 'get docs count')).rows, [{ count: 3000 + 100 * pages }]);
    assert.deepEqual((await w.query('idle', 'get docs count')).rows, [{ count: 10 }]);
    assert.deepEqual((await w.query('churn', 'get docs where title = $1 count', ['d'.repeat(400)])).rows, [{ count: 4000 }]);
    k = await w.kept('idle');
    assert.deepEqual([k.alarms, k.alarm], [0, null]);
    await w.stop();
  } finally {
    for (const child of running) process.kill(-child.pid, 'SIGKILL');
    await rm(state, { recursive: true, force: true });
  }
});
