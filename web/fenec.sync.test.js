// End-to-end tests for the sync layer: a **real** server, real wasm.
//
// No mocks. Only a real stream can answer the questions asked here: does an
// optimistic row reconcile with the server id, is a row that leaves the
// shape deleted, is the local side really rolled back when the server
// rejects a write.
//
// Skipped when `web/fenec.wasm` (make wasm) or the `fenec-pg` binary
// (cargo build) is missing.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, access } from 'node:fs/promises';
import { Fenec, sync, FenecError } from './fenec.js';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);
const bin = await (async () => {
  for (const p of ['../target/debug/fenec-pg', '../target/release/fenec-pg']) {
    const url = new URL(p, import.meta.url);
    try {
      await access(url);
      return url.pathname;
    } catch {
      /* not there */
    }
  }
  return null;
})();

const skip = !wasm
  ? 'no web/fenec.wasm (make wasm)'
  : !bin
    ? 'no fenec-pg binary (cargo build)'
    : false;

// A test that times out never reaches its `finally`; that server would be
// left behind. Live ones are tracked here and killed for certain as the
// process exits.
const alive = new Set();
process.on('exit', () => {
  for (const p of alive) p.kill('SIGKILL');
});

/** Brings up an in-memory `fenec-pg --http`; reads the address off stderr. */
async function server(extra = []) {
  const proc = spawn(bin, ['--listen', '127.0.0.1:0', '--http', '127.0.0.1:0', ...extra], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  alive.add(proc);
  const url = await new Promise((res, rej) => {
    let buf = '';
    const timer = setTimeout(() => rej(new Error(`server did not start: ${buf}`)), 10000);
    proc.stderr.on('data', (d) => {
      buf += d;
      // A pattern that guarantees the line is **complete**. Matching only
      // `(http:\/\/\S+)` would match a half-written address
      // (`http://127.0.0.`) when stderr arrives in pieces -- and the URL
      // parser silently turns that into `127.0.0.0:80`, followed by a 10 s
      // connection timeout. The closing bracket says the line has ended.
      const m = buf.match(/fenec-http \S+ listening on: (http:\/\/\S+)\s+\[[^\]]*\]/);
      if (m) {
        clearTimeout(timer);
        res(m[1]);
      }
    });
    proc.on('exit', (c) => {
      clearTimeout(timer);
      rej(new Error(`server exited with ${c}: ${buf}`));
    });
  });

  const run = async (query, params = []) => {
    const res = await fetch(`${url}/query`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ query, params }),
    });
    const body = await res.json();
    if (!res.ok) throw new Error(body.error);
    return body;
  };

  await run(
    'create collection tasks (key text @hash, title text, status text @hash, priority int)',
  );
  await run(
    `put tasks [
       {key: "a", title: "one",   status: "open",   priority: 1},
       {key: "b", title: "two",   status: "open",   priority: 5},
       {key: "c", title: "three", status: "closed", priority: 3}
     ]`,
  );
  return {
    url,
    run,
    close: () => {
      alive.delete(proc);
      proc.kill('SIGKILL');
    },
  };
}

async function open(url, opts = {}) {
  return sync({
    url,
    local: await Fenec.open(wasm),
    leader: false, // Node has no BroadcastChannel/locks; keep it simple
    shapes: [{ collection: 'tasks', where: { status: 'open' }, key: 'key' }],
    ...opts,
  });
}

/** Waits until the condition holds; fails with the last value if it never does. */
async function until(fn, what, ms = 5000) {
  const end = Date.now() + ms;
  let last;
  for (;;) {
    last = await fn();
    if (last) return last;
    if (Date.now() > end) assert.fail(`never happened: ${what} (last: ${JSON.stringify(last)})`);
    await new Promise((r) => setTimeout(r, 25));
  }
}

const opts = { skip, concurrency: false };

test('the seed fills the local replica with the shape', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    const rows = await db.from('tasks').order('priority').rows();
    assert.deepEqual(
      rows.map((r) => r.title),
      ['one', 'two'],
      'only what matches the shape (status = open)',
    );
    // Reads are local: they work with the server down too.
    s.close();
    assert.equal(await db.from('tasks').count(), 2);
  } finally {
    db.close();
    s.close();
  }
});

test('a write on the server lands on the subscriber', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    await s.run('put tasks {key: "d", title: "four", status: "open", priority: 9}');
    await until(async () => (await db.from('tasks').count()) === 3, '3 rows');
    const row = await db.from('tasks').where('key', 'd').first();
    assert.equal(row.title, 'four');
  } finally {
    db.close();
    s.close();
  }
});

test('a row that leaves the shape is deleted locally', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    await s.run('set tasks {status: "closed"} where key = "a"');
    await until(async () => (await db.from('tasks').count()) === 1, 'the row left the shape');
    assert.equal(await db.from('tasks').where('key', 'a').first(), null);
  } finally {
    db.close();
    s.close();
  }
});

test('an optimistic insert shows up at once, then reconciles with the server id', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    const p = db.from('tasks').insert({ title: 'new', status: 'open', priority: 2 });

    // It must already be in the local store, without waiting for the server.
    const immediately = await db.from('tasks').where('title', 'new').first();
    assert.ok(immediately, 'the optimistic row must show up at once');
    assert.ok(immediately.id >= 2 ** 52, 'the temporary id is far from the server range');

    await p;
    // Once the real row arrives over the subscription, the temporary one
    // must drop: a single copy.
    const reconciled = await until(async () => {
      const rows = await db.from('tasks').where('title', 'new').rows();
      return rows.length === 1 && rows[0].id < 2 ** 52 ? rows : false;
    }, 'the temporary id was replaced by the server id');
    assert.equal(reconciled.length, 1, 'no duplicate must remain');
    assert.ok(reconciled[0].key, 'the key must have been generated on its own');
  } finally {
    db.close();
    s.close();
  }
});

test('when the server rejects, the local side is rolled back exactly', opts, async () => {
  const s = await server();
  // A transport that rejects writes: the only honest way to force the error
  // path *without* breaking the server.
  let reject = false;
  const db = await open(s.url, {
    fetch: (u, init) => {
      if (reject && init?.method === 'POST') {
        return Promise.resolve(
          new Response(JSON.stringify({ error: 'nope' }), {
            status: 503,
            headers: { 'content-type': 'application/json' },
          }),
        );
      }
      return fetch(u, init);
    },
  });
  try {
    await db.ready();
    reject = true;

    await assert.rejects(
      () => db.from('tasks').where('key', 'a').update({ title: 'BROKEN' }),
      /nope/,
    );
    assert.equal(
      (await db.from('tasks').where('key', 'a').first()).title,
      'one',
      'the update must be rolled back',
    );

    await assert.rejects(() => db.from('tasks').where('key', 'b').delete(), /nope/);
    assert.equal(await db.from('tasks').count(), 2, 'the delete must be rolled back');

    await assert.rejects(
      () => db.from('tasks').insert({ title: 'ghost', status: 'open' }),
      /nope/,
    );
    assert.equal(
      await db.from('tasks').where('title', 'ghost').first(),
      null,
      'the optimistic insert must be rolled back',
    );
  } finally {
    db.close();
    s.close();
  }
});

test('a live query re-runs on every change', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    const seen = [];
    const stop = db.live(db.from('tasks').order('priority'), (rows) =>
      seen.push(rows.map((r) => r.title)),
    );
    await db.flush();
    assert.deepEqual(seen.at(-1), ['one', 'two'], 'the first value arrives right away');

    await s.run('put tasks {key: "d", title: "four", status: "open", priority: 0}');
    await until(
      () => seen.at(-1)?.length === 3,
      'the live query re-ran on the remote change',
    );
    assert.deepEqual(seen.at(-1), ['four', 'one', 'two'], 'the ordering is kept');

    stop();
    const count = seen.length;
    await s.run('put tasks {key: "e", title: "five", status: "open", priority: 7}');
    await until(async () => (await db.from('tasks').count()) === 4, 'the new row landed');
    await db.flush();
    assert.equal(seen.length, count, 'not called after the subscription ends');
  } finally {
    db.close();
    s.close();
  }
});

test('a batch goes in a single round trip', opts, async () => {
  const s = await server();
  let posts = 0;
  const db = await open(s.url, {
    fetch: (u, init) => {
      if (init?.method === 'POST') posts++;
      return fetch(u, init);
    },
  });
  try {
    await db.ready();
    posts = 0;
    const n = await db.batch(async (t) => {
      await t.from('tasks').insert({ title: 'x', status: 'open', priority: 1 });
      await t.from('tasks').insert({ title: 'y', status: 'open', priority: 2 });
      await t.from('tasks').where('key', 'a').update({ title: 'ONE' });
    });
    assert.equal(n, 3);
    assert.equal(posts, 1, 'three statements, one request');
    await until(async () => (await db.from('tasks').count()) === 4, 'two rows were added');
    assert.equal((await db.from('tasks').where('key', 'a').first()).title, 'ONE');
  } finally {
    db.close();
    s.close();
  }
});

test('on error a batch rolls the local side back from end to start', opts, async () => {
  const s = await server();
  // Local and remote are the same engine: a statement that passes locally
  // passes on the server too. So the honest way to force "the server
  // rejected it" is the transport -- not breaking the server.
  let reject = false;
  const db = await open(s.url, {
    fetch: (u, init) => {
      if (reject && String(u).endsWith('/batch')) {
        return Promise.resolve(
          new Response(JSON.stringify({ error: 'nope', completed: 1 }), {
            status: 503,
            headers: { 'content-type': 'application/json' },
          }),
        );
      }
      return fetch(u, init);
    },
  });
  try {
    await db.ready();
    const before = (await db.from('tasks').order('key').rows()).map((r) => r.title);
    reject = true;

    await assert.rejects(
      () =>
        db.batch(async (t) => {
          await t.from('tasks').where('key', 'a').update({ title: 'CHANGED' });
          await t.from('tasks').insert({ title: 'new', status: 'open', priority: 1 });
          await t.from('tasks').where('key', 'b').delete();
        }),
      /nope/,
    );

    const after = (await db.from('tasks').order('key').rows()).map((r) => r.title);
    assert.deepEqual(after, before, 'the whole batch must be rolled back');
  } finally {
    db.close();
    s.close();
  }
});

test('a half-finished batch reports how many were applied', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    // The second statement fails on the server (a field that does not
    // exist); the batch stops there and reports that the first one was
    // applied -- it is not rolled back, because fenecdb has no transaction
    // to roll back.
    const res = await fetch(`${s.url}/batch`, {
      method: 'POST',
      headers: { 'content-type': 'application/x-ndjson' },
      body: [
        JSON.stringify({ query: 'put tasks {title: $1, status: $2}', params: ['x', 'open'] }),
        JSON.stringify({ query: 'put tasks {nosuchfield: $1}', params: [1] }),
      ].join('\n'),
    });
    const body = await res.json();
    assert.equal(res.ok, false);
    assert.equal(body.completed, 1, 'how many were applied must be in the response');
    await until(async () => (await db.from('tasks').where('title', 'x').first()) !== null,
      'the first statement persists');
  } finally {
    db.close();
    s.close();
  }
});

test('a collection without a shape cannot be queried locally', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    assert.throws(() => db.from('missing'), FenecError);
    assert.throws(() => db.from('missing'), /no shape for/);
  } finally {
    db.close();
    s.close();
  }
});

test('the cursor state is reported', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    const [st] = db.status();
    assert.equal(st.collection, 'tasks');
    assert.equal(st.seeded, true);
    assert.ok(st.cursor > 0, 'the cursor must have advanced');
    assert.equal(st.pending, 0);
  } finally {
    db.close();
    s.close();
  }
});

test('a dropped connection resumes from the cursor, it does not reseed', opts, async () => {
  const s = await server();
  let seeds = 0;
  const db = await open(s.url, {
    fetch: async (u, init) => {
      if (String(u).includes('/changes')) seeds += String(u).includes('since=') ? 0 : 1;
      return fetch(u, init);
    },
  });
  try {
    await db.ready();
    assert.equal(seeds, 1, 'the first connection asks for a seed');

    // Cut the stream from outside: the client must back off and come back
    // with `since`.
    db.local.run('put tasks {key: "z", title: "z", status: "open"}');
    await s.run('put tasks {key: "d", title: "four", status: "open", priority: 9}');
    await until(async () => (await db.from('tasks').where('key', 'd').first()) !== null, 'd landed');
    assert.equal(seeds, 1, 'no reseed happened');
  } finally {
    db.close();
    s.close();
  }
});

// -------------------------------------------------------------------- tabs
//
// Each tab has its own WASM instance; if each opened its own subscription
// there would be N copies and N connections. Leader election prevents that:
// one stream, distributed over `BroadcastChannel`. The Web Locks API does
// not exist on Node, so the lock manager is injected -- the same seam leaves
// room for another coordination mechanism in the browser.

/** The test stand-in for `navigator.locks`: a serial, exclusive lock. */
function fakeLocks() {
  const queues = new Map();
  return {
    async request(name, _opts, fn) {
      const prev = queues.get(name) ?? Promise.resolve();
      let release;
      const held = new Promise((r) => {
        release = r;
      });
      queues.set(
        name,
        prev.then(() => held),
      );
      await prev;
      try {
        return await fn();
      } finally {
        release();
      }
    },
  };
}

test('multiple tabs: one stream, the others fed by the broadcast', opts, async () => {
  const s = await server();
  const locks = fakeLocks();
  let streams = 0;
  const count = (u, init) => {
    if (String(u).includes('/changes')) streams++;
    return fetch(u, init);
  };

  const a = await open(s.url, { leader: 'auto', locks, fetch: count });
  await a.ready();
  assert.equal(streams, 1, 'the first tab becomes leader and opens the stream');

  const b = await open(s.url, { leader: 'auto', locks, fetch: count });
  try {
    // The second tab does not open its own subscription: it gets the seed
    // from the leader.
    await until(async () => {
      const [st] = b.status();
      return st.seeded;
    }, 'the second tab was seeded by the leader');
    assert.equal(streams, 1, 'no second stream was opened');
    assert.deepEqual(
      (await b.from('tasks').order('key').rows()).map((r) => r.title),
      ['one', 'two'],
    );

    // A remote change lands on the leader, and from there to the second tab.
    await s.run('put tasks {key: "d", title: "four", status: "open", priority: 9}');
    await until(async () => (await b.from('tasks').count()) === 3, 'the broadcast reached the second tab');
    assert.equal(streams, 1, 'still a single stream');
  } finally {
    a.close();
    b.close();
    s.close();
  }
});

test('leader election can be turned off', opts, async () => {
  const s = await server();
  const locks = fakeLocks();
  let streams = 0;
  const count = (u, init) => {
    if (String(u).includes('/changes')) streams++;
    return fetch(u, init);
  };
  const a = await open(s.url, { leader: false, locks, fetch: count });
  const b = await open(s.url, { leader: false, locks, fetch: count });
  try {
    await Promise.all([a.ready(), b.ready()]);
    assert.equal(streams, 2, 'every tab opens its own subscription');
  } finally {
    a.close();
    b.close();
    s.close();
  }
});

test('a tab joining before the leader is seeded is not left hanging', opts, async () => {
  const s = await server();
  const locks = fakeLocks();
  // Slow the leader's seed down: the second tab will join right in that gap.
  const slow = async (u, init) => {
    if (String(u).includes('/changes')) await new Promise((r) => setTimeout(r, 800));
    return fetch(u, init);
  };

  const a = await open(s.url, { leader: 'auto', locks, fetch: slow });
  const b = await open(s.url, { leader: 'auto', locks });
  try {
    // A one-shot `hello` would go unanswered here: the leader is not seeded
    // yet, so it ignores the request.
    await Promise.race([
      b.ready(),
      new Promise((_, rej) => setTimeout(() => rej(new Error('the second tab was not seeded')), 8000)),
    ]);
    assert.deepEqual(
      (await b.from('tasks').order('key').rows()).map((r) => r.title),
      ['one', 'two'],
    );
    assert.equal(b.status()[0].leader, false, 'still a follower');
  } finally {
    a.close();
    b.close();
    s.close();
  }
});
