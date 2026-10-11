// End-to-end tests for the sync layer: a **real** server, real wasm.
//
// No mocks. Only a real stream can answer the questions asked here: does an
// optimistic row reconcile with the server id, is a row that leaves the
// shape deleted, is the local side really rolled back when the server
// rejects a write.
//
// Skipped when `web/fenec.wasm` (make wasm) or the `fenec-server` binary
// (cargo build) is missing.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, access, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Fenec, sync, connect, inc, FenecError } from './fenec.js';
import { fenecTable, text, integer, index, rename } from './schema.js';
import { installIndexedDB } from './idb.fake.js';
import * as client from './client.js';

const wasm = await readFile(new URL('./fenec.wasm', import.meta.url)).catch(() => null);
async function binary(name) {
  for (const p of [`../target/debug/${name}`, `../target/release/${name}`]) {
    const url = new URL(p, import.meta.url);
    try {
      await access(url);
      return url.pathname;
    } catch {
      /* not there */
    }
  }
  return null;
}
const bin = await binary('fenec-server');
const shardBin = await binary('fenec-shard');

const skip = !wasm
  ? 'no web/fenec.wasm (make wasm)'
  : !bin
    ? 'no fenec-server binary (cargo build)'
    : false;

// A test that times out never reaches its `finally`; that server would be
// left behind. Live ones are tracked here and killed for certain as the
// process exits.
const alive = new Set();
process.on('exit', () => {
  for (const p of alive) p.kill('SIGKILL');
});

/** Spawns a server binary and reads its HTTP address off stderr. */
async function listening(path, args, name) {
  const proc = spawn(path, args, { stdio: ['ignore', 'ignore', 'pipe'] });
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
      const m = buf.match(new RegExp(`${name} \\S+ listening on: (http://\\S+)\\s+\\[[^\\]]*\\]`));
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
  const close = () => {
    alive.delete(proc);
    proc.kill('SIGKILL');
  };
  return { url, close };
}

/** Brings up an in-memory `fenec-server --http`; reads the address off stderr. */
async function server(extra = []) {
  const { url, close } = await listening(
    bin,
    ['--http', '127.0.0.1:0', ...extra],
    'fenec-http',
  );

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
  return { url, run, close };
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

test('a collated field syncs in its collation, the replica handed the data it needs', opts, async () => {
  const s = await server();
  await s.run('create collection people (key text @hash, name text collate und @sorted)');
  await s.run('put people [{key: "1", name: "Ζωή"}, {key: "2", name: "anna"}, {key: "3", name: "Борис"}, {key: "4", name: "Bora"}]');
  const fetched = [];
  const db = await sync({
    url: s.url,
    local: await Fenec.open(wasm, {
      collation: (name) => {
        fetched.push(name);
        return readFile(new URL(`./collate/${name}.bin`, import.meta.url));
      },
    }),
    leader: false,
    shapes: [{ collection: 'people', key: 'key' }],
  });
  const names = async () => (await db.from('people').select('name').order('name').rows()).map((r) => r.name);
  try {
    // The seed needed Greek and Cyrillic, and the replica's field orders as
    // the server's does: by the collation, not the bytes.
    await db.ready();
    assert.deepEqual(await names(), ['anna', 'Bora', 'Ζωή', 'Борис']);
    // An optimistic write whose text needs data the replica has not got
    // waits for it, then shows up.
    await db.from('people').insert({ name: '山田' });
    assert.deepEqual(await names(), ['anna', 'Bora', 'Ζωή', 'Борис', '山田']);
    assert.deepEqual(fetched.sort(), ['cyrillic', 'greek', 'han']);
    await until(async () => (await s.run('get people')).length === 5, 'the server has the write');
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

test('when the server refuses, the local side is rolled back exactly', opts, async () => {
  const s = await server();
  // A transport that refuses writes: the only honest way to force the error
  // path *without* breaking the server. A 403, since a 5xx is not a
  // refusal: the write waits and goes again.
  let reject = false;
  const db = await open(s.url, {
    fetch: (u, init) => {
      if (reject && init?.method === 'POST') {
        return Promise.resolve(
          new Response(JSON.stringify({ error: 'nope' }), {
            status: 403,
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

test('a server error keeps the write, and it lands once the server is back', opts, async () => {
  const s = await server();
  let failing = 0;
  const keys = [];
  const db = await open(s.url, {
    fetch: (u, init) => {
      if (init?.method === 'POST') {
        keys.push(init.headers['idempotency-key']);
        if (failing > 0) {
          failing--;
          return Promise.resolve(new Response('{"error":"down"}', { status: 503 }));
        }
      }
      return fetch(u, init);
    },
  });
  try {
    await db.ready();
    failing = 1;
    const p = db.from('tasks').where('key', 'a').update({ title: 'KEPT' });
    assert.equal((await db.from('tasks').where('key', 'a').first()).title, 'KEPT');
    // 250 ms of backoff, then sent again under the same key.
    assert.equal(await p, 1);
    assert.equal(keys.length, 2);
    assert.equal(keys[0], keys[1], 'the retry carries the key it was sent with');
    await until(async () => (await s.run('get tasks where title = "KEPT"')).length === 1, 'the server has it');
    assert.equal((await db.from('tasks').where('key', 'a').first()).title, 'KEPT', 'never put back');
    await db.pushed();
    assert.equal(db.status()[0].pending, 0);
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
            status: 409,
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

test('a batch that fails lands none of it, and says so', opts, async () => {
  const s = await server();
  const db = await open(s.url);
  try {
    await db.ready();
    // The second statement fails on the server (a field that does not
    // exist). A batch is one block: the first one's write is put back, and
    // the response says nothing was applied -- which is what the sync
    // layer's own rollback of the whole batch assumes.
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
    assert.equal(body.completed, 0, 'what was applied must be in the response');
    // A write after it lands, and the first statement's never did.
    await s.run('put tasks {key: $1, title: $2, status: $3, priority: $4}', ['z', 'after', 'open', 1]);
    await until(async () => (await db.from('tasks').where('title', 'after').first()) !== null,
      'the write after it arrives');
    assert.equal(await db.from('tasks').where('title', 'x').first(), null, 'the first statement landed');
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

// ------------------------------------------------------------- outages
//
// A write is kept until the server answers it: no answer and a 5xx send it
// again under its key, which the server answers as the first time.

/** A fetch that fails every POST while `down.on`, the server reached or not first. */
function flaky(down) {
  const keys = [];
  return {
    keys,
    fetch: async (u, init) => {
      if (init?.method !== 'POST') return fetch(u, init);
      keys.push(init.headers['idempotency-key']);
      if (down.on === 'refused') throw new TypeError('fetch failed');
      if (down.on === 'lost') {
        // The server takes it; the answer never comes back.
        await fetch(u, init);
        throw new TypeError('the connection was reset');
      }
      return fetch(u, init);
    },
  };
}

test('a write whose answer was lost lands once, sent again under its key', opts, async () => {
  const s = await server();
  const down = { on: 'lost' };
  const f = flaky(down);
  const db = await open(s.url, { fetch: f.fetch });
  try {
    await db.ready();
    const p = db.from('tasks').insert({ title: 'once', status: 'open' });
    await until(() => f.keys.length >= 1, 'the first send');
    down.on = null;
    assert.equal(await p, 1);
    assert.ok(f.keys.length >= 2 && f.keys.every((k) => k === f.keys[0]), 'one key for every send');
    assert.equal((await s.run('get tasks where title = "once"')).length, 1, 'made once');
    await until(async () => {
      const rows = await db.from('tasks').where('title', 'once').rows();
      return rows.length === 1 && rows[0].id < 2 ** 52;
    }, 'the server copy in its place');
  } finally {
    db.close();
    s.close();
  }
});

test("an update of an unanswered insert reaches the server's copy by its key", opts, async () => {
  const s = await server();
  const down = { on: 'refused' };
  const db = await open(s.url, { fetch: flaky(down).fetch });
  try {
    await db.ready();
    db.from('tasks').insert({ title: 'draft', status: 'open' });
    const row = await db.from('tasks').where('title', 'draft').first();
    assert.ok(row.id >= 2 ** 52);
    // By the temporary id: the server never saw it.
    const p = db.from('tasks').where('id', row.id).update({ title: 'final' });
    down.on = null;
    db.setOnline(true);
    await p;
    const open = await s.run('get tasks where status = "open" select title order title');
    assert.deepEqual(open.map((r) => r.title), ['final', 'one', 'two']);
    await until(async () => (await db.from('tasks').where('title', 'final').first())?.id < 2 ** 52, 'the copy came');
  } finally {
    db.close();
    s.close();
  }
});

test('writes made offline survive a reload of the page, and go once it is back', opts, async () => {
  installIndexedDB();
  const s = await server();
  const first = await open(s.url, { fetch: flaky({ on: 'refused' }).fetch, persist: 'reload' });
  await first.ready();
  first.setOnline(false);
  first.from('tasks').insert({ title: 'kept', status: 'open' }).catch(() => {});
  first.from('tasks').where('key', 'a').update({ title: 'ONE' }).catch(() => {});
  assert.equal(first.status()[0].pending, 2);
  // What a page closed now leaves in IndexedDB.
  await new Promise((r) => setTimeout(r, 50));
  first.close();

  const second = await open(s.url, { persist: 'reload' });
  try {
    assert.equal(second.status()[0].pending, 2, 'the writes came back with the replica');
    assert.ok(await second.from('tasks').where('title', 'kept').first(), 'and so did their rows');
    await second.pushed();
    const open = await s.run('get tasks where status = "open" select title order title');
    assert.deepEqual(open.map((r) => r.title), ['ONE', 'kept', 'two']);
    await until(
      async () => (await second.from('tasks').where('title', 'kept').rows()).every((r) => r.id < 2 ** 52),
      'the copy in place',
    );
    assert.equal((await second.from('tasks').where('title', 'kept').rows()).length, 1);
  } finally {
    second.close();
    s.close();
    delete globalThis.indexedDB;
  }
});

test('two replicas increment offline, and both land: the server holds +2, and both see it', opts, async () => {
  const s = await server();
  const [a, b] = [await open(s.url), await open(s.url)];
  try {
    await a.ready();
    await b.ready();
    a.setOnline(false);
    b.setOnline(false);
    // Each applies its own at once, from the 1 it holds.
    for (const r of [a, b]) {
      r.from('tasks').where('key', 'a').update({ priority: inc(1) }).catch(() => {});
      assert.equal((await r.from('tasks').where('key', 'a').first()).priority, 2);
    }
    a.setOnline(true);
    b.setOnline(true);
    await a.pushed();
    await b.pushed();
    // The server worked each out again over what it held: 1 + 1 + 1.
    assert.deepEqual(await s.run('get tasks select priority where key = "a"'), [{ priority: 3 }]);
    for (const r of [a, b]) {
      await until(
        async () => (await r.from('tasks').where('key', 'a').first()).priority === 3,
        "the server's sum on the replica",
      );
    }
  } finally {
    a.close();
    b.close();
    s.close();
  }
});

test("the leader sends a follower's writes, and the follower is told the answer", opts, async () => {
  const s = await server();
  const locks = fakeLocks();
  const posts = { a: 0, b: 0 };
  let refuse = false;
  const counting = (tab) => (u, init) => {
    if (init?.method === 'POST') {
      posts[tab]++;
      if (refuse) return Promise.resolve(new Response('{"error":"denied"}', { status: 403 }));
    }
    return fetch(u, init);
  };
  const a = await open(s.url, { leader: 'auto', locks, fetch: counting('a') });
  await a.ready();
  const refused = [];
  const b = await open(s.url, { leader: 'auto', locks, fetch: counting('b'), onRefused: (e) => refused.push(e.status) });
  try {
    await b.ready();
    assert.equal(await b.from('tasks').insert({ title: 'from b', status: 'open' }), 1);
    assert.deepEqual(posts, { a: 1, b: 0 }, 'only the leader sends');
    await until(
      async () => (await b.from('tasks').where('title', 'from b').rows()).every((r) => r.id < 2 ** 52),
      "b's row is the server's",
    );
    assert.equal(b.status()[0].pending, 0);

    refuse = true;
    await assert.rejects(b.from('tasks').where('key', 'a').update({ title: 'NOPE' }), /denied/);
    assert.equal((await b.from('tasks').where('key', 'a').first()).title, 'one', 'put back in the follower');
    assert.deepEqual(refused, [403]);
    assert.equal(posts.b, 0);
  } finally {
    a.close();
    b.close();
    s.close();
  }
});

test('a new leader sends what the tabs before it had not', opts, async () => {
  installIndexedDB();
  const s = await server();
  const locks = fakeLocks();
  const a = await open(s.url, { leader: 'auto', locks, fetch: flaky({ on: 'refused' }).fetch, persist: 'tabs' });
  await a.ready();
  const b = await open(s.url, { leader: 'auto', locks, persist: 'tabs' });
  try {
    await b.ready();
    a.from('tasks').insert({ title: 'from a', status: 'open' }).catch(() => {});
    const fromB = b.from('tasks').insert({ title: 'from b', status: 'open' });
    await until(() => a.status()[0].pending === 2, 'the leader holds both writes');
    await new Promise((r) => setTimeout(r, 50));
    // The leader goes, its write unsent: the next one takes over.
    a.close();
    await fromB;
    await b.pushed();
    assert.equal(b.status()[0].leader, true);
    assert.equal((await s.run('get tasks where title = "from a" count'))[0].count, 1);
    assert.equal((await s.run('get tasks where title = "from b" count'))[0].count, 1);
  } finally {
    a.close();
    b.close();
    s.close();
    delete globalThis.indexedDB;
  }
});

// ------------------------------------------------------------------ tenants
//
// The same sync layer through a router: `fenec-shard` in front of a
// `fenec-server --dir` node. The client's base URL gains `/t/<tenant>` and
// nothing else changes -- which is the whole claim of tenant routing.

const shardSkip = skip || (!shardBin ? 'no fenec-shard binary (cargo build)' : false);

/** A node, a router in front of it, and tenant `name` seeded with tasks. */
async function sharded(names) {
  const dir = await mkdtemp(join(tmpdir(), 'fenec-sync-'));
  const node = await listening(
    bin,
    ['--dir', join(dir, 'n1'), '--http', '127.0.0.1:0', '--admin-token', 'adm'],
    'fenec-http',
  );
  const router = await listening(
    shardBin,
    ['--listen', '127.0.0.1:0', '--directory', join(dir, 'shard.fenec')],
    'fenec-shard',
  );
  const call = async (method, path, body) => {
    const res = await fetch(`${router.url}${path}`, {
      method,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(`${method} ${path}: ${res.status} ${text}`);
    return text ? JSON.parse(text) : null;
  };
  await call('PUT', '/_shard/nodes/n1', { addr: new URL(node.url).host, token: 'adm' });
  for (const name of names) {
    await call('PUT', `/_shard/tenants/${name}`);
    await call('POST', `/t/${name}/query`, {
      query: 'create collection tasks (key text @hash, title text, status text @hash, priority int)',
    });
    await call('POST', `/t/${name}/query`, {
      query: `put tasks {key: "a", title: "${name}", status: "open", priority: 1}`,
    });
  }
  return {
    url: (name) => `${router.url}/t/${name}`,
    run: (name, query) => call('POST', `/t/${name}/query`, { query }),
    close: async () => {
      router.close();
      node.close();
      await rm(dir, { recursive: true, force: true });
    },
  };
}

test('a tenant syncs through the router with only the base URL changed', { ...opts, skip: shardSkip }, async () => {
  const c = await sharded(['acme', 'beta']);
  const acme = await open(c.url('acme'));
  const beta = await open(c.url('beta'));
  try {
    await acme.ready();
    await beta.ready();
    assert.deepEqual((await acme.from('tasks').rows()).map((r) => r.title), ['acme']);
    assert.deepEqual((await beta.from('tasks').rows()).map((r) => r.title), ['beta']);

    // A write on one tenant streams to its subscriber and to no other.
    await c.run('acme', 'put tasks {key: "b", title: "acme 2", status: "open", priority: 2}');
    await until(async () => (await acme.from('tasks').count()) === 2, 'acme got its write');
    assert.equal(await beta.from('tasks').count(), 1);

    // An optimistic write goes through the router and reconciles.
    await acme.from('tasks').insert({ key: 'c', title: 'local', status: 'open', priority: 3 });
    await until(
      async () => (await c.run('acme', 'get tasks where key = "c" count'))[0]?.count === 1,
      'the insert reached the node',
    );
  } finally {
    acme.close();
    beta.close();
    await c.close();
  }
});

// ------------------------------------------------------------ schema in code

const tasksTable = fenecTable(
  'tasks',
  { key: text(), title: text(), status: text(), priority: integer() },
  (t) => [index('tasks_key').using('hash', t.key), index('tasks_status').using('hash', t.status)],
);

test('a replica checks the code\'s schema against the server\'s and applies none of it', opts, async () => {
  const s = await server();
  try {
    // The code declares what the server holds: the replica opens, typed by it.
    // The shape names the table, as the code declares it.
    const db = await open(s.url, { schema: { tasks: tasksTable }, shapes: [{ collection: tasksTable, where: { status: 'open' }, key: 'key' }] });
    await db.ready();
    assert.deepEqual((await db.from(tasksTable).order('priority').rows()).map((r) => r.title), ['one', 'two']);
    db.close();

    // A field the server lacks is the server's to add: refused, and the
    // server's schema is as it was.
    const ahead = fenecTable('tasks', { key: text(), title: text(), status: text(), priority: integer(), due: integer() }, (t) => [
      index().using('hash', t.key),
      index().using('hash', t.status),
    ]);
    await assert.rejects(open(s.url, { schema: { tasks: ahead } }), (e) => {
      assert.ok(e instanceof FenecError);
      assert.deepEqual(e.refusals.map((r) => [r.kind, r.field]), [['field_missing', 'due']]);
      assert.match(e.message, /the server's is the one that counts/);
      return true;
    });
    const fields = (await s.run('collections'))[0].fields.map((f) => f.name);
    assert.deepEqual(fields, ['key', 'title', 'status', 'priority']);

    // What the server holds beyond the code is its own: a replica of fewer
    // fields opens.
    const fewer = fenecTable('tasks', { key: text(), title: text() }, (t) => [index().using('hash', t.key)]);
    const narrow = await open(s.url, { schema: { tasks: fewer } });
    narrow.close();
  } finally {
    s.close();
  }
});

test('connect compares, and migrates only when asked', opts, async () => {
  const s = await server();
  try {
    const http = await connect(s.url, { schema: { tasks: tasksTable } });
    assert.equal((await http.from(tasksTable).where('status', 'open').rows()).length, 2);

    const renamed = fenecTable('tasks', { key: text(), name: text(), status: text(), priority: integer() }, (t) => [
      index().using('hash', t.key),
      index().using('hash', t.status),
    ]);
    const migrations = [rename('tasks', 'title', 'name')];
    // Not asked: the server's `title` is not the code's `name`, and nothing runs.
    await assert.rejects(connect(s.url, { schema: { tasks: renamed }, migrations }), /`tasks.name` is in the code and not in the database/);
    // Asked: the migration runs on the server, once, recorded there.
    const migrated = await connect(s.url, { schema: { tasks: renamed }, migrations, migrate: true });
    assert.deepEqual((await migrated.from(renamed).select('name').order('name').rows()).map((r) => r.name), ['one', 'three', 'two']);
    assert.deepEqual(await s.run('get _migrations select n, text'), [{ n: 1, text: 'alter collection tasks rename field title to name' }]);
    await connect(s.url, { schema: { tasks: renamed }, migrations, migrate: true });
    assert.equal((await s.run('get _migrations count'))[0].count, 1);
  } finally {
    s.close();
  }
});

// `@fenecdb/web/client`: no module at all, the queries run on the server
// and a live query follows its subscriptions.
test("a live query over HTTP follows the server's writes, with no module", { skip: bin ? false : 'no fenec-server binary (cargo build)', concurrency: false }, async () => {
  const s = await server();
  const db = client.connect(s.url);
  const seen = [];
  const stop = db.live(db.from('tasks').where('status', 'open').order('priority', 'desc').limit(2), (rows) =>
    seen.push(rows.map((r) => r.title)),
  );
  try {
    await until(() => seen.length === 1, 'the first rows');
    assert.deepEqual(seen[0], ['two', 'one']);
    await s.run('put tasks {key: "d", title: "four", status: "open", priority: 9}');
    await until(() => seen.at(-1)?.[0] === 'four', 'the rows again after a write');
    assert.deepEqual(seen.at(-1), ['four', 'two']);
    // A write to a collection the query does not read runs nothing.
    await s.run('create collection other (n int)');
    await new Promise((r) => setTimeout(r, 150));
    const count = seen.length;
    await s.run('put other {n: 1}');
    await new Promise((r) => setTimeout(r, 150));
    assert.equal(seen.length, count);
    stop();
    await s.run('put tasks {key: "e", title: "five", status: "open", priority: 10}');
    await new Promise((r) => setTimeout(r, 150));
    assert.equal(seen.length, count, 'nothing after the stop');
  } finally {
    stop();
    s.close();
  }
});

// A polled live query holds no stream: its rows again only once what it
// reads was written, the rounds between answered 304.
test('a polled live query over HTTP runs again only after a write to what it reads', { skip: bin ? false : 'no fenec-server binary (cargo build)', concurrency: false }, async () => {
  const s = await server();
  await s.run('create collection other (n int)');
  const statuses = [];
  const fetchSeen = (url, init) =>
    fetch(url, init).then((r) => {
      statuses.push(r.status);
      return r;
    });
  const db = client.connect(s.url, { fetch: fetchSeen });
  const seen = [];
  const stop = db.live(db.from('tasks').where('status', 'open').order('priority', 'desc').limit(2), (rows) => seen.push(rows.map((r) => r.title)), { poll: 100 });
  try {
    await until(() => seen.length === 1, 'the first rows');
    assert.deepEqual(seen[0], ['two', 'one']);
    await s.run('put other {n: 1}');
    await until(() => statuses.filter((x) => x === 304).length >= 3, 'rounds answered 304');
    assert.equal(seen.length, 1, 'a write elsewhere runs nothing');
    await s.run('put tasks {key: "d", title: "four", status: "open", priority: 9}');
    await until(() => seen.length === 2, 'the rows again after a write');
    assert.deepEqual(seen[1], ['four', 'two']);
    assert.throws(() => db.live('get tasks', () => {}, { poll: 10 }), /100 ms/);
  } finally {
    stop();
    s.close();
  }
});

/** An HS256 token for `claims`, signed with `secret`. */
async function jwt(secret, claims) {
  const { createHmac } = await import('node:crypto');
  const b64 = (s) => Buffer.from(s).toString('base64url');
  const body = `${b64('{"alg":"HS256","typ":"JWT"}')}.${b64(JSON.stringify(claims))}`;
  return `${body}.${createHmac('sha256', secret).update(body).digest('base64url')}`;
}

// A live query under a scoped token: its stream's shape holds nothing, and
// ANDed with the token's filter it heard nothing, so the query ran once.
// It runs again at a write to a row the token may read, and not at another
// user's -- by stream and by poll alike.
test('a live query over HTTP follows the writes a scoped token may read, and no others', { skip: bin ? false : 'no fenec-server binary (cargo build)', concurrency: false }, async () => {
  const dir = await mkdtemp(join(tmpdir(), 'fenec-scoped-live-'));
  const secret = 'web-tests-jwt-secret-of-32-bytes-or-more';
  const policy = join(dir, 'policy.txt');
  await (await import('node:fs/promises')).writeFile(policy, 'notes read,write where owner = $jwt.sub\n');
  const s = await listening(bin, ['--http', '127.0.0.1:0', '--http-token', 'root', '--jwt-secret', secret, '--policy', policy], 'fenec-http');
  try {
    const root = client.connect(s.url, { token: 'root' });
    await root.run('create collection notes (owner text @hash, title text)');
    const exp = Math.floor(Date.now() / 1000) + 600;
    const alice = client.connect(s.url, { token: await jwt(secret, { sub: 'alice', exp }) });
    const bob = client.connect(s.url, { token: await jwt(secret, { sub: 'bob', exp }) });
    const streamed = [];
    const polled = [];
    const statuses = [];
    const counting = client.connect(s.url, {
      token: await jwt(secret, { sub: 'alice', exp }),
      fetch: (url, init) => fetch(url, init).then((r) => (statuses.push(r.status), r)),
    });
    const q = alice.from('notes').select('title').order('title');
    const stops = [
      alice.live(q, (rows) => streamed.push(rows.map((r) => r.title))),
      counting.live(counting.from('notes').select('title').order('title'), (rows) => polled.push(rows.map((r) => r.title)), { poll: 100 }),
    ];
    try {
      await until(() => streamed.length === 1 && polled.length === 1, 'the first rows');
      await bob.from('notes').insert({ title: 'bob 1' });
      await root.run('put notes {owner: "bob", title: "bob 2"}');
      await until(() => statuses.filter((x) => x === 304).length >= 3, 'polls answered 304 after bob wrote');
      assert.equal(streamed.length, 1, 'bob writing ran alice\'s stream');
      assert.equal(polled.length, 1, 'bob writing ran alice\'s poll');
      await alice.from('notes').insert({ title: 'alice 1' });
      await until(() => streamed.at(-1)?.[0] === 'alice 1' && polled.at(-1)?.[0] === 'alice 1', 'her own write');
      assert.deepEqual(streamed.at(-1), ['alice 1']);
    } finally {
      for (const stop of stops) stop();
    }
  } finally {
    s.close();
    await rm(dir, { recursive: true, force: true });
  }
});

// `/batch` and `Idempotency-Key` through the client: the shop's checkout
// posted them with a `fetch` of its own.
test('a batch over HTTP lands whole, once a key, and names the statement that stopped it', { skip: bin ? false : 'no fenec-server binary (cargo build)', concurrency: false }, async () => {
  const s = await server();
  const db = client.connect(s.url);
  try {
    const tasks = db.from('tasks');
    const take = (key, by) => tasks.where('key', key).where('priority', '>=', by).toUpdate({ priority: inc(-by) }, { require: 1 });
    const got = await db.batch([take('b', 2), tasks.select('priority').where('key', 'b'), 'get tasks count'], { idempotencyKey: 'k-1' });
    assert.deepEqual(got.results, [{ affected: 1 }, { rows: [{ priority: 3 }] }, { rows: [{ count: 3 }] }]);
    assert.equal(got.replayed, false);
    assert.ok(got.seq > 0 && db.seq === got.seq);
    // Sent again with its key: the answer kept, nothing written twice.
    const again = await db.batch([take('b', 2), tasks.select('priority').where('key', 'b'), 'get tasks count'], { idempotencyKey: 'k-1' });
    assert.equal(again.replayed, true);
    assert.deepEqual(again.results, got.results);
    assert.equal((await tasks.where('key', 'b').first()).priority, 3);
    // A `require` not met: 412, the statement's place, and nothing landed.
    const e = await db.batch([take('a', 1), take('b', 9)]).catch((e) => e);
    assert.ok(e instanceof client.FenecError);
    assert.equal(e.status, 412);
    assert.equal(e.at, 1);
    assert.equal(e.completed, 0);
    assert.match(e.message, /require/);
    assert.equal((await tasks.where('key', 'a').first()).priority, 1);
    // The same key with another request is refused.
    await assert.rejects(db.batch(['set tasks {priority: 0} where key = "zz"'], { idempotencyKey: 'k-1' }), (e) => e.status === 422);
    // A single write under a key, and a builder's through a keyed copy.
    const one = await db.run('put tasks {key: "d", title: "four", status: "open", priority: 2}', [], { idempotencyKey: 'k-2' });
    assert.deepEqual([one.count, one.replayed], [1, false]);
    assert.equal((await db.run('put tasks {key: "d", title: "four", status: "open", priority: 2}', [], { idempotencyKey: 'k-2' })).replayed, true);
    const keyed = db.withIdempotencyKey('k-3');
    assert.equal(await keyed.from('tasks').insert({ key: 'e', title: 'five', status: 'open', priority: 4 }), 1);
    assert.equal(await keyed.from('tasks').insert({ key: 'e', title: 'five', status: 'open', priority: 4 }), 1);
    assert.equal(await tasks.count(), 5);
    assert.equal(db.seq, keyed.seq);
    await assert.rejects(db.batch([]), /a list of statements/);
    await assert.rejects(db.batch([42]), /batch statement 0/);
  } finally {
    s.close();
  }
});

// A claim held at the server until a job comes (`{ wait }`, `Fenec-Wait`):
// taken by the builder's update and by `run`, ended on time with no row,
// and given up by an AbortSignal; a database in the page refuses `wait`.
test('a held claim over HTTP takes the job enqueued meanwhile, and a signal gives it up', { skip: bin ? false : 'no fenec-server binary (cargo build)', concurrency: false }, async () => {
  const s = await server();
  const db = client.connect(s.url);
  try {
    await db.run('create collection jobs (kind text, run_at timestamp @sorted, owner text)');
    const ready = () => db.from('jobs').where(client.raw('run_at <= now()')).order('run_at').limit(1);
    const lease = (owner) => ({ owner, run_at: client.expr('now() + ?', 60000) });
    const held = ready().update(lease('w1'), { returning: ['kind'], wait: 10000 });
    await new Promise((r) => setTimeout(r, 200));
    await db.from('jobs').insert({ kind: 'mail', run_at: Date.now() });
    assert.deepEqual(await held, [{ kind: 'mail' }]);
    // Nothing more to take: the wait ends on time, with no row.
    const began = Date.now();
    assert.deepEqual(await ready().update(lease('w2'), { returning: true, wait: 300 }), []);
    assert.ok(Date.now() - began >= 290, `${Date.now() - began} ms`);
    // A signal gives a held claim up, and the client goes on.
    const stop = new AbortController();
    const given = db.run('del jobs where run_at <= now() order run_at limit 1 returning kind', [], { wait: 10000, signal: stop.signal });
    setTimeout(() => stop.abort(), 100);
    await assert.rejects(given, (e) => e.name === 'AbortError');
    assert.equal(await db.from('jobs').count(), 1);
    await assert.rejects(ready().update(lease('w3'), { wait: -1 }), /wait is milliseconds/);
  } finally {
    s.close();
  }
  if (wasm) {
    const local = await Fenec.open(wasm);
    local.run('create collection q (n int)');
    await assert.rejects(
      async () => local.from('q').limit(1).update({ n: 0 }, { wait: 1000 }),
      /a database in the page/,
    );
  }
});

// sync() opens the full module unless given one, and the module it is
// given by URL when asked: here the one without indexes, whose `near`
// measures every vector as `exact` does.
const liteWasm = await readFile(new URL('./fenec-lite.wasm', import.meta.url)).catch(() => null);

test('sync opens the full module by default, and the module it is given', { ...opts, skip: skip || (liteWasm ? false : 'no web/fenec-lite.wasm (make wasm-lite)') }, async () => {
  const s = await server();
  await s.run('create collection docs (key text @hash, title text @text, embed vector<3> @hnsw(cosine))');
  await s.run(`put docs [{key: "a", title: "east", embed: [1.0, 0.0, 0.0]},
                         {key: "b", title: "north", embed: [0.0, 1.0, 0.0]},
                         {key: "c", title: "between", embed: [0.7, 0.7, 0.0]}]`);
  // The module's URL is asked of fetch, as a page asks it; the rest goes
  // to the server.
  const asked = [];
  const real = globalThis.fetch;
  globalThis.fetch = (url, init) => {
    if (typeof url === 'string' && url.endsWith('.wasm')) {
      asked.push(url);
      const bytes = url.endsWith('fenec-lite.wasm') ? liteWasm : wasm;
      return Promise.resolve(new Response(bytes, { headers: { 'content-type': 'application/wasm' } }));
    }
    return real(url, init);
  };
  let db;
  try {
    const shapes = [{ collection: 'docs', key: 'key' }];
    const full = await sync({ url: s.url, leader: false, shapes });
    await full.ready();
    assert.deepEqual(asked, ['./fenec.wasm']);
    full.local.run('create collection own (v vector<2>)');
    full.local.run('create index on own (v) @hnsw(l2)');
    full.close();

    db = await sync({ url: s.url, leader: false, shapes, wasm: './fenec-lite.wasm' });
    await db.ready();
    assert.deepEqual(asked, ['./fenec.wasm', './fenec-lite.wasm']);
    const near = await db.from('docs').near('embed', [0.9, 0.5, 0.0]).limit(2).rows();
    assert.deepEqual(near.map((r) => r.key), ['c', 'a']);
    // No graph in it: an index of one is refused, naming the feature.
    db.local.run('create collection own (v vector<2>)');
    assert.throws(() => db.local.run('create index on own (v) @hnsw(l2)'), /`vector` feature/);
  } finally {
    globalThis.fetch = real;
    db?.close();
    s.close();
  }
});
