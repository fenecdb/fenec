// `connect(url, { reads: 'nearest' })` against real servers: a tenant at
// home in eu, its read replica in us, a router of the global directory in
// each region -- the client in us asks the one there.
//
// What must hold: the builder's reads, and a text the caller marks, are
// answered by the replica near the client -- shown by an answer while home
// cannot be reached, which only the replica could give -- and each is sent
// after the client's last write, so it never reads older than what it
// wrote; writes, and every request of a client reading home, go home. A
// live query's streams stay home, and each of its runs is a read near
// sent after the change a stream told of, which the replica waits for.
//
// Skipped when the `fenec-server` or `fenec-shard` binary (cargo build) is
// missing: nothing here needs the module.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { access, mkdtemp, rm } from 'node:fs/promises';
import { createServer, connect as dial } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { connect, FenecError } from './client.js';

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
const skip = !bin
  ? 'no fenec-server binary (cargo build)'
  : !shardBin
    ? 'no fenec-shard binary (cargo build)'
    : false;

const alive = new Set();
process.on('exit', () => {
  for (const p of alive) p.kill('SIGKILL');
});

/** Spawns a server binary and reads its address off its `listening on:` line. */
async function listening(path, args, name) {
  const proc = spawn(path, args, { stdio: ['ignore', 'ignore', 'pipe'] });
  alive.add(proc);
  const url = await new Promise((res, rej) => {
    let buf = '';
    // A bound only on a server that never starts, which is killed then.
    const timer = setTimeout(() => {
      alive.delete(proc);
      proc.kill('SIGKILL');
      rej(new Error(`${name} did not start: ${buf}`));
    }, 30000);
    proc.stderr.on('data', (d) => {
      buf += d;
      const m = buf.match(new RegExp(`${name} \\S+ listening on: (http://\\S+)\\s+\\[[^\\]]*\\]`));
      if (m) {
        clearTimeout(timer);
        res(m[1]);
      }
    });
    proc.on('exit', (c) => {
      clearTimeout(timer);
      rej(new Error(`${name} exited with ${c}: ${buf}`));
    });
  });
  const close = () => {
    alive.delete(proc);
    proc.kill('SIGKILL');
  };
  return { url, close };
}

/** Waits for `check` to hold: a bound on what must happen, never on how soon. */
async function until(check, what) {
  const deadline = Date.now() + 60000;
  while (!(await check())) {
    if (Date.now() > deadline) throw new Error(`never: ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

/**
 * A TCP proxy to the server at `url` that can hold what a primary sends
 * its replicas: a connection whose request line names `/_replication` -- a
 * replica's stream -- has every byte the primary sends it kept while the
 * link is parked, and passed on once released; every other connection
 * passes as it comes. The replica so held cannot have a write made
 * meanwhile until the test says, however soon it would have caught up, and
 * the primary is slowed by nothing: the proxy reads what it sends. What
 * every client sent through it is kept, for `asked` to count.
 */
async function parking(url) {
  const { hostname, port } = new URL(url);
  let parked = false;
  const kept = [];
  const open = new Set();
  let sent = '';
  const server = createServer((client) => {
    const upstream = dial(Number(port), hostname);
    open.add(client).add(upstream);
    client.on('data', (d) => (sent += d.toString('latin1')));
    const end = () => {
      client.destroy();
      upstream.destroy();
      open.delete(client);
      open.delete(upstream);
    };
    for (const s of [client, upstream]) {
      s.on('error', end);
      s.on('close', end);
    }
    let head = Buffer.alloc(0);
    const first = (d) => {
      head = Buffer.concat([head, d]);
      const line = head.indexOf('\r\n');
      if (line < 0) return;
      client.off('data', first);
      upstream.write(head);
      client.pipe(upstream);
      if (!head.toString('latin1', 0, line).includes('/_replication')) return upstream.pipe(client);
      upstream.on('data', (chunk) => (parked ? kept.push([client, chunk]) : client.write(chunk)));
    };
    client.on('data', first);
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return {
    addr: `127.0.0.1:${server.address().port}`,
    park: () => {
      parked = true;
    },
    release: () => {
      parked = false;
      for (const [client, chunk] of kept.splice(0)) if (!client.destroyed) client.write(chunk);
    },
    /** How many requests sent through it name `line`, a request line's start. */
    asked: (line) => sent.split(line).length - 1,
    close: () => {
      server.close();
      for (const s of open) s.destroy();
    },
  };
}

/**
 * eu1 in eu and us1 in us, the global router near eu and its standby near
 * us; tenant `acme` on eu1 with its read replica on us1. `parked`: eu1 is
 * named to the routers by a `parking` proxy in front of it, which the read
 * replica follows it through too (`link`), and us1 by one of its own
 * (`nearby`), which counts what the routers ask it.
 */
async function world({ parked = false } = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'fenec-replicas-'));
  const node = (name) =>
    listening(
      bin,
      ['--dir', join(dir, name), '--http', '127.0.0.1:0', '--admin-token', 'adm', '--replication-token', 'repl'],
      'fenec-http',
    );
  const eu1 = await node('eu1');
  const us1 = await node('us1');
  const link = parked ? await parking(eu1.url) : null;
  const nearby = parked ? await parking(us1.url) : null;
  const router = (file, extra) =>
    listening(
      shardBin,
      ['--listen', '127.0.0.1:0', '--directory', join(dir, file), '--token', 'root', '--replication-token', 'dirs', ...extra],
      'fenec-shard',
    );
  const eu = await router('global.fenec', ['--near', 'eu']);
  const us = await router('standby.fenec', ['--near', 'us', '--replica-of', eu.url]);
  const call = async (base, method, path, body) => {
    const res = await fetch(`${base}${path}`, {
      method,
      headers: { authorization: 'Bearer root' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(`${method} ${path}: ${res.status} ${text}`);
    return text ? JSON.parse(text) : null;
  };
  await call(eu.url, 'PUT', '/_shard/nodes/eu1', { addr: link?.addr ?? new URL(eu1.url).host, token: 'adm', region: 'eu' });
  await call(eu.url, 'PUT', '/_shard/nodes/us1', { addr: nearby?.addr ?? new URL(us1.url).host, token: 'adm', region: 'us' });
  await call(eu.url, 'PUT', '/_shard/tenants/acme', { node: 'eu1', schema: 'create collection notes (body text)' });
  const placed = await call(eu.url, 'PUT', '/_shard/tenants/acme/read-replicas/us');
  assert.equal(placed.node, 'us1');
  await until(async () => {
    const list = await call(us.url, 'GET', '/_shard/tenants/acme/read-replicas');
    return list.read_replicas.some((r) => r.node === 'us1');
  }, "us's router knows of the read replica");
  return {
    eu1,
    us1,
    link,
    nearby,
    call,
    eu: eu.url,
    us: us.url,
    url: `${us.url}/t/acme`,
    close: async () => {
      for (const p of [us, eu, us1, eu1]) p.close();
      link?.close();
      nearby?.close();
      await rm(dir, { recursive: true, force: true });
    },
  };
}

/** A `fetch` that keeps every request's headers. */
function recording() {
  const sent = [];
  const fetchSpy = (url, init) => {
    sent.push({ path: new URL(url).pathname, headers: { ...(init?.headers ?? {}) } });
    return fetch(url, init);
  };
  return { sent, fetch: fetchSpy };
}

test('reads near a read replica are answered there, never older than the client wrote', { skip }, async () => {
  const w = await world();
  try {
    const { sent, fetch: spy } = recording();
    const db = connect(w.url, { reads: 'nearest', fetch: spy });
    const home = connect(w.url);

    // A write goes home, and says where it left the tenant.
    const { seq } = await db.run('put notes {body: "a"}');
    assert.ok(seq > 0, `a write answers its seq: ${seq}`);
    const write = sent.at(-1);
    assert.equal(write.headers['fenec-read'], undefined);
    // The builder's read after it waits for it on the replica.
    assert.deepEqual((await db.from('notes').rows()).map((r) => r.body), ['a']);
    let read = sent.at(-1);
    assert.equal(read.headers['fenec-read'], 'replica');
    assert.equal(read.headers['fenec-after'], String(seq));
    // That change of the history the write named: a node that replicates
    // gives each tenant one of its own.
    assert.match(read.headers['fenec-history'], /^\d+-[0-9a-f]{16}$/);
    // So do its count and a batch of its queries; a text the caller marks;
    // and the furthest write, not the last answered.
    await db.from('notes').insert({ body: 'b' });
    const b = db.seq;
    assert.ok(b > seq);
    assert.equal(await db.from('notes').count(), 2);
    assert.equal(sent.at(-1).headers['fenec-after'], String(b));
    const { results } = await db.batch([db.from('notes').where('body', 'a'), db.from('notes').where('body', 'b')]);
    assert.equal(results.length, 2);
    assert.equal(sent.at(-1).headers['fenec-read'], 'replica');
    const marked = await db.run('get notes count', [], { read: true });
    assert.equal(marked.rows[0].count, 2);
    assert.equal(sent.at(-1).headers['fenec-read'], 'replica');
    // A text not marked, and a batch holding one, go home.
    await db.run('get notes count');
    assert.equal(sent.at(-1).headers['fenec-read'], undefined);
    await db.batch([db.from('notes').where('body', 'a'), 'get notes count']);
    assert.equal(sent.at(-1).headers['fenec-read'], undefined);
    // A write marked as a read by mistake is refused, never landed.
    await assert.rejects(
      () => db.run('put notes {body: "x"}', [], { read: true }),
      (e) => e instanceof FenecError && e.status === 403,
    );

    // Home gone: the reads near are still answered -- by the replica, the
    // one copy left -- and the client reading home is not. The replica
    // asked itself first, until it holds both: a read near above may have
    // been answered home, the replica not caught up yet.
    await until(async () => {
      const res = await fetch(`${w.us1.url}/t/acme/query`, {
        method: 'POST',
        headers: { 'fenec-read': 'replica' },
        body: JSON.stringify({ query: 'get notes count' }),
      });
      return res.ok && (await res.json())[0]?.count === 2;
    }, 'the read replica holds both writes');
    w.eu1.close();
    assert.deepEqual((await db.from('notes').order('body').rows()).map((r) => r.body), ['a', 'b']);
    read = sent.at(-1);
    assert.equal(read.headers['fenec-read'], 'replica');
    await assert.rejects(() => home.from('notes').rows(), (e) => e instanceof FenecError && e.status === 502);
    await assert.rejects(() => db.from('notes').insert({ body: 'c' }), (e) => e instanceof FenecError && e.status === 502);
  } finally {
    await w.close();
  }
});

/** `fenec_router_replica_reads_total` of a router, by outcome. */
async function replicaReads(router) {
  const text = await (await fetch(`${router}/_metrics`, { headers: { authorization: 'Bearer root' } })).text();
  const of = (outcome) =>
    Number(text.match(new RegExp(`fenec_router_replica_reads_total\\{outcome="${outcome}"\\} (\\d+)`))?.[1] ?? NaN);
  return { replica: of('replica'), fallback: of('fallback') };
}

test("a live query's runs read near after the change that ran them, its streams home", { skip }, async () => {
  const w = await world({ parked: true });
  try {
    // A wait for a write (`Fenec-After`) holds one of the tenant's streams
    // where its quota is set: the replica's count of them is what tells
    // the test a run is held there, and the router's of the reads it
    // answered near, by whom.
    await w.call(w.eu, 'PUT', '/_shard/tenants/acme/quota', { streams: 1000 });
    const waiting = async () => {
      const res = await fetch(`${w.us1.url}/_admin/tenants/acme/quota`, { headers: { authorization: 'Bearer adm' } });
      const q = await res.json();
      return q.quota?.streams === 1000 ? q.usage.streams : -1;
    };
    await until(async () => (await waiting()) === 0, 'the quota reaches the read replica');
    // Caught up with its primary before its link is parked: a copy whose
    // primary has not said hello answers no read (503), and the router
    // sends it home -- parked in that moment, the first session's hello
    // was held and every run went home at once.
    await until(async () => {
      const res = await fetch(`${w.us1.url}/t/acme/query`, {
        method: 'POST',
        headers: { 'fenec-read': 'replica' },
        body: JSON.stringify({ query: 'get notes count' }),
      });
      return res.ok;
    }, 'the read replica caught up');
    const { sent, fetch: spy } = recording();
    const db = connect(w.url, { reads: 'nearest', fetch: spy });
    const home = connect(w.url);
    const runs = () => sent.filter((r) => r.path === '/t/acme/query');
    const got = [];
    const errors = [];
    // Until a run is seen held on the replica, no rows may come: a run
    // answered meanwhile was answered by the replica from before the change.
    const held = (n, what) =>
      until(async () => {
        assert.equal(got.length, n, `rows came before the replica had the change: ${JSON.stringify(got.at(-1))}`);
        return (await waiting()) === 1;
      }, what);

    // A write another client made before the live query opened, the
    // replica held back from it: the first rows wait for it there, sent
    // after where home stood as the stream opened -- this client wrote
    // nothing, so nothing else would hold it -- and come once it has it.
    w.link.park();
    const a = (await home.run('put notes {body: "a"}')).seq;
    let before = await replicaReads(w.us);
    const stop = db.live(db.from('notes').order('body'), (rows) => got.push(rows.map((r) => r.body)), {
      onError: (e) => errors.push(e),
    });
    try {
      await held(0, 'the first run waits on the replica');
      assert.equal(runs().length, 1);
      const first = runs()[0].headers;
      assert.equal(first['fenec-read'], 'replica');
      assert.ok(Number(first['fenec-after']) >= a, `sent after ${first['fenec-after']}, the write at ${a}`);
      assert.match(first['fenec-history'], /^\d+-[0-9a-f]{16}$/);
      // The stream stays home: a write is told of as home takes it.
      const streams = sent.filter((r) => r.path === '/t/acme/notes/changes');
      assert.equal(streams.length, 1);
      for (const h of ['fenec-read', 'fenec-after', 'fenec-history']) assert.equal(streams[0].headers[h], undefined);
      w.link.release();
      await until(() => got.length === 1, 'the first rows');
      assert.deepEqual(got[0], ['a']);
      let after = await replicaReads(w.us);
      assert.deepEqual(after, { replica: before.replica + 1, fallback: before.fallback });

      // A change: the run it brings waits on the replica for that very
      // change of that history, and the replica answers it once it has it.
      w.link.park();
      before = after;
      const b = (await home.run('put notes {body: "b"}')).seq;
      await held(1, 'the run a change brought waits on the replica');
      const second = runs().at(-1).headers;
      assert.equal(runs().length, 2);
      assert.equal(second['fenec-read'], 'replica');
      assert.equal(second['fenec-after'], String(b));
      assert.equal(second['fenec-history'], first['fenec-history']);
      w.link.release();
      await until(() => got.length === 2, 'the rows after the change');
      assert.deepEqual(got[1], ['a', 'b']);
      after = await replicaReads(w.us);
      assert.deepEqual(after, { replica: before.replica + 1, fallback: before.fallback });

      // A replica that cannot have the change in its second sends the run
      // home, which has it: never the rows from before it. And the router
      // lets the copy be, so the changes after go home at once: a replica
      // cut off costs one wait, not one a change.
      w.link.park();
      before = after;
      const asked = () => w.nearby.asked('POST /t/acme/query ');
      const reached = asked();
      const c = (await home.run('put notes {body: "c"}')).seq;
      await held(2, 'the run waits on a replica that will not have the change');
      assert.equal(asked(), reached + 1);
      await until(() => got.length === 3, 'the rows after a change the replica never had');
      assert.deepEqual(got[2], ['a', 'b', 'c']);
      assert.equal(runs().at(-1).headers['fenec-after'], String(c));
      assert.equal(runs().at(-1).headers['fenec-wait'], '1000');
      for (const body of ['d', 'e']) {
        await home.run(`put notes {body: "${body}"}`);
        await until(() => got.at(-1).includes(body), `the rows after ${body}`);
      }
      assert.deepEqual(got.at(-1), ['a', 'b', 'c', 'd', 'e']);
      assert.equal(asked(), reached + 1, 'a run after the first asked the replica cut off');
      after = await replicaReads(w.us);
      assert.equal(after.replica, before.replica);
      assert.ok(after.fallback >= before.fallback + 3, `${JSON.stringify(after)} after ${JSON.stringify(before)}`);
      // One stream all along, unmarked.
      assert.equal(sent.filter((r) => r.path === '/t/acme/notes/changes').length, 1);
      assert.deepEqual(errors, []);
    } finally {
      stop();
      w.link.release();
    }
  } finally {
    await w.close();
  }
});

test("runs that reach home after a change it holds take none of the tenant's streams", { skip }, async () => {
  const w = await world();
  try {
    // One stream for the tenant, which the live query's own subscription
    // takes. Its runs -- through eu's router, the tenant's own region, home
    // -- are sent after changes home holds already: each refused 429 for
    // a stream it never waited in, the live query was handed nothing.
    await w.call(w.eu, 'PUT', '/_shard/tenants/acme/quota', { streams: 1 });
    await until(async () => {
      const res = await fetch(`${w.eu1.url}/_admin/tenants/acme/quota`, { headers: { authorization: 'Bearer adm' } });
      return (await res.json()).quota?.streams === 1;
    }, 'the quota reaches home');
    const home = connect(w.url);
    await home.run('put notes {body: "a"}');
    const db = connect(`${w.eu}/t/acme`, { reads: 'nearest' });
    const got = [];
    const errors = [];
    const stop = db.live(db.from('notes').order('body'), (rows) => got.push(rows.map((r) => r.body)), {
      onError: (e) => errors.push(e),
    });
    try {
      await until(() => got.length === 1 || errors.length > 0, 'the first rows');
      assert.deepEqual(errors, []);
      assert.deepEqual(got[0], ['a']);
      await home.run('put notes {body: "b"}');
      await until(() => got.length === 2 || errors.length > 0, 'the rows after a change');
      assert.deepEqual(errors, []);
      assert.deepEqual(got[1], ['a', 'b']);
      // A client's read near after its own write, beside them: home holds
      // it, so it waits for nothing and takes no stream.
      await db.run('put notes {body: "c"}');
      assert.deepEqual((await db.from('notes').order('body').rows()).map((r) => r.body), ['a', 'b', 'c']);
      assert.deepEqual(errors, []);
    } finally {
      stop();
    }
  } finally {
    await w.close();
  }
});

test('a client reads home unless it asks for nearest', async () => {
  const sent = [];
  const answer = (status, body, headers = {}) =>
    new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });
  const replies = [];
  const fake = async (url, init) => {
    sent.push({ ...(init.headers ?? {}) });
    return replies.shift() ?? answer(200, []);
  };
  const plain = connect('http://x', { fetch: fake });
  await plain.from('notes').rows();
  assert.equal(sent.at(-1)['fenec-read'], undefined);
  assert.throws(() => connect('http://x', { reads: 'anywhere', fetch: fake }), FenecError);

  // A read near goes after the position the write named: its change, of
  // its history.
  const db = connect('http://x', { reads: 'nearest', fetch: fake });
  const h1 = '1-00000000000000aa';
  const h2 = '2-00000000000000bb';
  replies.push(answer(200, { affected: 1 }, { 'fenec-seq': '7', 'fenec-history': h1 }));
  await db.run('put notes {body: "a"}');
  // A read near that no copy holds the write for in the wait -- home had
  // none either, the write lost in a failover -- is asked again home with
  // no mark, and the client takes the position home named as its own,
  // lower and of the history home is on now: kept at 7 of the old one,
  // every read near would wait out the replica and home again for good.
  replies.push(answer(504, { error: 'change 7 is not in this copy\'s history', seq: 5, history: h2 }));
  replies.push(answer(200, [{ body: 'a' }]));
  assert.deepEqual(await db.from('notes').rows(), [{ body: 'a' }]);
  assert.equal(sent.at(-2)['fenec-after'], '7');
  assert.equal(sent.at(-2)['fenec-history'], h1);
  assert.equal(sent.at(-1)['fenec-read'], undefined);
  assert.equal(sent.at(-1)['fenec-after'], undefined);
  assert.equal(sent.at(-1)['fenec-history'], undefined);
  await db.from('notes').rows();
  assert.equal(sent.at(-1)['fenec-after'], '5');
  assert.equal(sent.at(-1)['fenec-history'], h2);
  // A read near's own answer moves nothing: it may be a replica's.
  replies.push(answer(200, { results: [] }, { 'fenec-seq': '9', 'fenec-history': h1 }));
  await db.batch([db.from('notes')]);
  // A write answered late, of the history before, is not the furthest; one
  // of the history now is.
  replies.push(answer(200, { affected: 1 }, { 'fenec-seq': '8', 'fenec-history': h1 }));
  await db.run('put notes {body: "b"}');
  await db.from('notes').rows();
  assert.equal(sent.at(-1)['fenec-after'], '5');
  replies.push(answer(200, { affected: 1 }, { 'fenec-seq': '6', 'fenec-history': h2 }));
  await db.run('put notes {body: "c"}');
  // A copy with a key reads near too, and shares what was written.
  await db.withIdempotencyKey('k1').from('notes').rows();
  assert.equal(sent.at(-1)['fenec-after'], '6');
  assert.equal(sent.at(-1)['fenec-history'], h2);
});

test("a read near's 504 sets the position back over its own alone, and never to an earlier term", async () => {
  const sent = [];
  const answer = (status, body, headers = {}) =>
    new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });
  // Each request is answered when the test says: two under way at once.
  const waiting = [];
  const fake = (url, init) => {
    sent.push({ ...(init.headers ?? {}) });
    return new Promise((resolve) => waiting.push(resolve));
  };
  const next = async () => {
    while (waiting.length === 0) await new Promise((r) => setTimeout(r, 1));
    return waiting.shift();
  };
  const h2 = '2-00000000000000bb';
  const h1 = '1-00000000000000aa';
  const db = connect('http://x', { reads: 'nearest', fetch: fake });
  const w1 = db.run('put notes {body: "a"}');
  (await next())(answer(200, { affected: 1 }, { 'fenec-seq': '100', 'fenec-history': h2 }));
  await w1;

  // A read near is sent after 100; a write lands at 101 while it waits;
  // then home answers the read 504 at 100. Taken, the next read near would
  // be sent after 100 and miss the write.
  const read = db.from('notes').rows();
  const readAnswer = await next();
  assert.equal(sent.at(-1)['fenec-after'], '100');
  const w2 = db.run('put notes {body: "b"}');
  (await next())(answer(200, { affected: 1 }, { 'fenec-seq': '101', 'fenec-history': h2 }));
  await w2;
  readAnswer(answer(504, { error: 'not in the wait', seq: 100, history: h2 }));
  (await next())(answer(200, [{ body: 'a' }, { body: 'b' }]));
  await read;
  const after = db.from('notes').rows();
  (await next())(answer(200, [{ body: 'a' }, { body: 'b' }]));
  await after;
  assert.equal(sent.at(-1)['fenec-after'], '101');

  // A 504 naming an earlier term -- the old primary, fenced, reached
  // through a router behind a failover -- is not taken.
  const stale = db.from('notes').rows();
  (await next())(answer(504, { error: 'not in this copy', seq: 140, history: h1 }));
  (await next())(answer(200, []));
  await stale;
  const again = db.from('notes').rows();
  (await next())(answer(200, []));
  await again;
  assert.equal(sent.at(-1)['fenec-after'], '101');
  assert.equal(sent.at(-1)['fenec-history'], h2);
});

test("a tenant made again under its name: home's position of another lineage is taken, its term lower", async () => {
  const sent = [];
  const answers = [];
  const answer = (status, body, headers = {}) =>
    new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });
  const fake = (url, init) => {
    sent.push({ ...(init.headers ?? {}) });
    return Promise.resolve(answers.shift());
  };
  // A tenant failed over twice: term 3 of its lineage, aaaaaaaa.
  const old = '3-aaaaaaaa000000c3';
  const db = connect('http://x', { reads: 'nearest', fetch: fake });
  answers.push(answer(200, { affected: 1 }, { 'fenec-seq': '50', 'fenec-history': old }));
  await db.run('put notes {body: "a"}');

  // Deleted and made again: home names term 1 of another lineage. Refused
  // as an earlier term, every read near waited its whole wait for good.
  const remade = '1-bbbbbbbb00000001';
  answers.push(answer(504, { error: 'not in this copy', seq: 2, history: remade }));
  answers.push(answer(200, []));
  await db.from('notes').rows();
  answers.push(answer(200, []));
  await db.from('notes').rows();
  assert.equal(sent.at(-1)['fenec-after'], '2');
  assert.equal(sent.at(-1)['fenec-history'], remade);

  // And from a write's answer the same: home's word on the tenant now.
  const third = '1-cccccccc00000001';
  answers.push(answer(200, { affected: 1 }, { 'fenec-seq': '1', 'fenec-history': third }));
  await db.run('put notes {body: "b"}');
  answers.push(answer(200, []));
  await db.from('notes').rows();
  assert.equal(sent.at(-1)['fenec-after'], '1');
  assert.equal(sent.at(-1)['fenec-history'], third);
});


/**
 * A scripted server for a live query: each stream opened, by collection,
 * its head the next of `heads`, events sent as the test says; each request
 * to /query held until the test answers it.
 */
function scripted() {
  const tick = () => new Promise((r) => setTimeout(r, 1));
  const heads = { notes: [], tags: [] };
  const streams = { notes: [], tags: [] };
  const requests = [];
  const enc = new TextEncoder();
  const fetch = (url, init) => {
    const c = new URL(url).pathname.match(/^\/(\w+)\/changes$/)?.[1];
    if (!c) return new Promise((resolve) => requests.push({ headers: { ...init.headers }, resolve }));
    let ctl;
    const body = new ReadableStream({ start: (k) => (ctl = k) });
    streams[c].push({
      headers: { ...init.headers },
      send: (name, data) => ctl.enqueue(enc.encode(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`)),
      end: () => {
        try {
          ctl.close();
        } catch {
          /* closed already */
        }
      },
    });
    return Promise.resolve(
      new Response(body, { status: 200, headers: { 'content-type': 'text/event-stream', ...heads[c].shift() } }),
    );
  };
  let taken = 0;
  const got = [];
  return {
    heads,
    streams,
    fetch,
    got,
    /** The next request sent to /query. */
    next: async () => {
      while (requests.length <= taken) await tick();
      return requests[taken++];
    },
    /** The `n`th stream opened of collection `c`. */
    opened: async (c, n) => {
      while (streams[c].length < n) await tick();
      return streams[c][n - 1];
    },
    /** Until the live query has been handed rows `n` times. */
    rows: async (n) => {
      while (got.length < n) await tick();
    },
    end: () => {
      for (const s of [...streams.notes, ...streams.tags]) s.end();
    },
  };
}

const answer = (status, body, headers = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });
const unmarked = (headers) => {
  for (const h of ['fenec-read', 'fenec-after', 'fenec-history', 'fenec-wait']) assert.equal(headers[h], undefined, h);
};

test("a live query's runs go after the furthest its streams named, a 504 sending them home", async () => {
  const s = scripted();
  /** The next run: near, after `seq` of `history`; answered, its rows the `n`th. */
  const run = async (seq, history, n, head = {}) => {
    const r = await s.next();
    assert.equal(r.headers['fenec-read'], 'replica');
    assert.equal(r.headers['fenec-after'], seq);
    assert.equal(r.headers['fenec-history'], history);
    // A second on a replica behind, then home.
    assert.equal(r.headers['fenec-wait'], '1000');
    r.resolve(answer(200, [], head));
    await s.rows(n);
  };
  const h1 = '1-00000000000000aa';
  const h2 = '2-00000000000000aa';
  const h3 = '3-00000000000000aa';
  const h4 = '4-00000000000000aa';

  const db = connect('http://x', { reads: 'nearest', fetch: s.fetch });
  const wrote = async (seq, history) => {
    const w = db.run('put notes {body: "x"}');
    (await s.next()).resolve(answer(200, { affected: 1 }, { 'fenec-seq': seq, 'fenec-history': history }));
    await w;
  };
  await wrote('7', h1);
  s.heads.notes.push({ 'fenec-seq': '5', 'fenec-history': h1 });
  s.heads.tags.push({ 'fenec-seq': '9', 'fenec-history': h1 });
  const stop = db.live('get notes', (r) => s.got.push(r), { collections: ['notes', 'tags'] });
  const notes = await s.opened('notes', 1);
  const tags = await s.opened('tags', 1);
  // The streams go home: no mark, no position.
  unmarked(notes.headers);
  unmarked(tags.headers);
  // The first rows: after the furthest of the client's write (7) and the
  // streams' seeds, of their history -- tags' 9.
  notes.send('seed', { seq: 5, rows: [] });
  tags.send('seed', { seq: 9, rows: [] });
  await run('9', h1, 1);
  // A change: after it. A change one stream names behind another's: never
  // back.
  notes.send('change', { seq: 12, puts: [1], dels: [] });
  await run('12', h1, 2);
  tags.send('change', { seq: 11, puts: [2], dels: [] });
  await run('12', h1, 3);
  // The client's own write further than any change: after it.
  await wrote('15', h1);
  notes.send('change', { seq: 13, puts: [3], dels: [] });
  await run('15', h1, 4);

  // A failover: notes' stream ends and is opened again on the promoted
  // copy, whose head names the later term at a lower change -- taken over
  // the client's write of the term before. The old primary's stream,
  // fenced, still names its term: it moves nothing.
  s.heads.notes.push({ 'fenec-seq': '14', 'fenec-history': h2 });
  notes.end();
  const again = await s.opened('notes', 2);
  unmarked(again.headers);
  again.send('seed', { seq: 14, rows: [] });
  await run('14', h2, 5);
  tags.send('change', { seq: 16, puts: [4], dels: [] });
  await run('14', h2, 6);

  // A run home answers 504 -- the change lost in another failover -- is
  // asked near again after where home stands, which the live query takes
  // as its own; that answer's position the next run goes after.
  again.send('change', { seq: 17, puts: [5], dels: [] });
  const lost = await s.next();
  assert.equal(lost.headers['fenec-after'], '17');
  lost.resolve(answer(504, { error: 'not in this copy', seq: 3, history: h3 }));
  await run('3', h3, 7, { 'fenec-seq': '4', 'fenec-history': h3 });
  tags.send('change', { seq: 18, puts: [6], dels: [] });
  await run('4', h3, 8);
  // One asked near again and answered 504 once more goes home unmarked.
  tags.send('change', { seq: 19, puts: [7], dels: [] });
  (await s.next()).resolve(answer(504, { error: 'not in this copy', seq: 2, history: h4 }));
  const twice = await s.next();
  assert.equal(twice.headers['fenec-after'], '2');
  twice.resolve(answer(504, { error: 'not in this copy', seq: 1, history: h4 }));
  const home = await s.next();
  unmarked(home.headers);
  home.resolve(answer(200, []));
  await s.rows(9);
  stop();
  s.end();
  // The client's own position is its own: a read near goes after its
  // write still, not after what another's change set back.
  const read = db.from('notes').rows();
  const r = await s.next();
  assert.equal(r.headers['fenec-after'], '15');
  assert.equal(r.headers['fenec-history'], h1);
  r.resolve(answer(200, [], { 'fenec-seq': '30', 'fenec-history': h1 }));
  await read;
  // ...and a read near's answer moves nothing of it.
  const after = db.from('notes').rows();
  const r2 = await s.next();
  assert.equal(r2.headers['fenec-after'], '15');
  r2.resolve(answer(200, []));
  await after;
});

test("a live query's rows never go back to before a run answered from home", async () => {
  // A replica behind home, as a router sees it: a run after a change the
  // replica holds is answered there, as it stands; one after a change it
  // lacks, from home, which went on meanwhile. Rows of the notes `a`, `b`,
  // `b2`, `c` at changes 1 to 4.
  const s = scripted();
  const h = '1-00000000000000aa';
  const notes = ['a', 'b', 'b2', 'c'];
  let replica = 1;
  const home = 4;
  const answered = async () => {
    const r = await s.next();
    const after = Number(r.headers['fenec-after']);
    const at = after <= replica ? replica : home;
    r.resolve(answer(200, notes.slice(0, at).map((body) => ({ body })), { 'fenec-seq': String(at), 'fenec-history': h }));
    await s.rows(s.got.length + 1);
  };
  const db = connect('http://x', { reads: 'nearest', fetch: s.fetch });
  s.heads.notes.push({ 'fenec-seq': '1', 'fenec-history': h });
  const stop = db.live('get notes', (r) => s.got.push(r.map((n) => n.body)), { collections: ['notes'] });
  const stream = await s.opened('notes', 1);
  stream.send('seed', { seq: 1, rows: [] });
  await answered();
  // `b` told of: the replica lacks it, so home answers -- with `b2` and
  // `c` too, written meanwhile, their events still on their way.
  stream.send('change', { seq: 2, puts: [2], dels: [] });
  await answered();
  assert.deepEqual(s.got.at(-1), notes);
  // The replica catches up to `b2`, not `c`; `b2`'s event comes. Sent
  // after it alone, the run was answered by the replica without `c`.
  replica = 3;
  stream.send('change', { seq: 3, puts: [3], dels: [] });
  await answered();
  stream.send('change', { seq: 4, puts: [4], dels: [] });
  replica = 4;
  await answered();
  for (let i = 1; i < s.got.length; i++) {
    assert.ok(s.got[i].length >= s.got[i - 1].length, `rows went back: ${JSON.stringify(s.got)}`);
  }
  stop();
  s.end();
});

test('a live query reads home while a stream names no history, and polls home', async () => {
  const s = scripted();
  const h = '1-00000000000000aa';
  const db = connect('http://x', { reads: 'nearest', fetch: s.fetch });
  // One head naming no history -- a server from before heads named one --
  // beside one that does: the runs go home.
  s.heads.notes.push({ 'fenec-seq': '20' });
  s.heads.tags.push({ 'fenec-seq': '20', 'fenec-history': h });
  const stop = db.live('get notes', (r) => s.got.push(r), { collections: ['notes', 'tags'] });
  const bare = await s.opened('notes', 1);
  bare.send('seed', { seq: 20, rows: [] });
  (await s.opened('tags', 1)).send('seed', { seq: 20, rows: [] });
  const run = await s.next();
  unmarked(run.headers);
  run.resolve(answer(200, []));
  await s.rows(1);
  // Its stream opened again on a server that names one -- upgraded -- and
  // the runs read near again, after that history's change.
  s.heads.notes.push({ 'fenec-seq': '21', 'fenec-history': h });
  bare.end();
  (await s.opened('notes', 2)).send('seed', { seq: 21, rows: [] });
  const near = await s.next();
  assert.equal(near.headers['fenec-read'], 'replica');
  assert.equal(near.headers['fenec-after'], '21');
  assert.equal(near.headers['fenec-history'], h);
  near.resolve(answer(200, []));
  await s.rows(2);
  stop();
  s.end();

  // A poll goes home, marked by nothing: a replica would answer 304 to a
  // tag home has moved past.
  const quit = db.live(db.from('notes'), (r) => s.got.push(r), { poll: 100 });
  const poll = await s.next();
  unmarked(poll.headers);
  assert.equal(poll.headers['if-none-match'], '"0"');
  quit();
  poll.resolve(answer(200, []));

  // A client reading home: its runs unmarked, whatever its streams name.
  s.heads.notes.push({ 'fenec-seq': '20', 'fenec-history': '1-00000000000000aa' });
  const plain = connect('http://x', { fetch: s.fetch });
  const end = plain.live('get notes', (r) => s.got.push(r), { collections: ['notes'] });
  (await s.opened('notes', 3)).send('seed', { seq: 20, rows: [] });
  const first = await s.next();
  unmarked(first.headers);
  first.resolve(answer(200, []));
  await s.rows(3);
  end();
  s.end();
});
