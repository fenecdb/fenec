// The global entry (entry/) under `wrangler dev`, in front of real
// fenec-shard routers and fenec-server nodes (entry/cluster.mjs): two
// regions keeping their own directory, us with a standby router, behind a
// global router. What it holds: a tenant goes to its home region, learnt
// from the routers' answers, and to its new one after a move; a tenant not
// yet heard reaches its home past a region that is down; a stream and a
// held write pass through whole, and a stream lets go when its client does;
// the admin paths stay private however a path is spelled; the routers'
// marks never come from a client, and the client is named by the entry's
// mark, bound to its address; a router gone -- or one a tunnel's connector
// cannot reach -- is passed over for its standby, and a known home gone is
// a 503 to come back from, never another region. Needs `npm ci` here
// (wrangler) and the debug binaries (cargo build -p fenec-server -p
// fenec-shard).

import { after, before, describe, test } from 'node:test';
import assert from 'node:assert/strict';
import { createHmac } from 'node:crypto';
import { access, mkdtemp, rm } from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { running, serve } from './dev.mjs';
import * as cluster from './entry/cluster.mjs';
import { BUFFER_AT, State, clientMark, configure, corsHeaders, entry, judged, markKey, percentDecode, segments, sentHeaders } from './entry/entry.js';

const { DATA, ORIGIN, ROOT, query, request, shard, until } = cluster;

const ready = (await cluster.built()) && (await access(new URL('node_modules/.bin/wrangler', import.meta.url)).then(() => true, () => false));
const skip = ready ? false : 'needs wrangler (npm ci) and fenec-server and fenec-shard (cargo build -p fenec-server -p fenec-shard)';
// CI builds both and installs wrangler (make cloudflare-test): skipped
// there, these would pass unrun.
if (skip && process.env.CI) throw new Error(`entry.test.js cannot run: ${skip}`);

after(() => {
  cluster.killAll();
  for (const child of running) process.kill(-child.pid, 'SIGKILL');
});

const SECRET = 'entry-secret';
/** A client's mark as a router would work it out, here with node:crypto. */
const markOf = (address, seconds) =>
  createHmac('sha256', SECRET).update(`fenec-client ${address} ${seconds}`).digest().subarray(0, 16).toString('base64url');

/** Holds a `Fenec-Client` value to `address`: its time within a minute of now, its mark the router's. */
function namesClient(value, address) {
  const [ip, seconds, mark] = value.split(' ');
  assert.equal(ip, address);
  assert.ok(Math.abs(Number(seconds) - Date.now() / 1000) < 60, value);
  assert.equal(mark, markOf(address, seconds));
}

/**
 * A router gone, as the entry meets one: a port held by a listener that
 * resets every connection it takes -- held, so no one else's server can
 * take the port meanwhile, as a port found free and let go of could be.
 */
async function refusing() {
  const server = net.createServer((socket) => socket.resetAndDestroy());
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { url: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

test("the client's mark is HMAC-SHA256 of its address and time, 16 bytes, base64url", async () => {
  const key = markKey(SECRET);
  assert.equal(await clientMark(key, '203.0.113.7', 1_800_000_000), markOf('203.0.113.7', 1_800_000_000));
  assert.equal((await clientMark(key, '203.0.113.7', 1_800_000_000)).length, 22);
  // Bound to both: a mark seen names no other address, and no other time.
  assert.notEqual(await clientMark(key, '203.0.113.8', 1_800_000_000), markOf('203.0.113.7', 1_800_000_000));
  assert.notEqual(await clientMark(key, '203.0.113.7', 1_800_000_001), markOf('203.0.113.7', 1_800_000_000));
});

test('a path is read as fenec-http reads one: decoded, then cut at its slashes', () => {
  // fenec-http's own cases (http.rs, `percent_decode`).
  assert.equal(percentDecode('a%20b'), 'a b');
  assert.equal(percentDecode('a+b'), 'a b');
  assert.equal(percentDecode('%C3%BCst'), 'üst');
  assert.equal(percentDecode('%zz'), '%zz');
  assert.equal(percentDecode('100%'), '100%');
  assert.equal(percentDecode('%4'), '%4');
  for (const p of ['/t/acme/%5Freplication', '/t/acme/%5freplication', '/t/acme%2F_replication', '/t/acme/%2F_replication', '//t//acme//_replication']) {
    assert.deepEqual(segments(p), ['t', 'acme', '_replication'], p);
  }
  assert.deepEqual(segments('/%74/%61cme/query'), ['t', 'acme', 'query']);
});

test('the configuration is read, and refused where it names nothing to send to', () => {
  const cfg = configure({
    REGIONS: JSON.stringify({ us: { routers: ['https://r1.us.example/', 'http://r2.us.example:8090'], continents: ['na'] }, eu: ['https://r1.eu.example'] }),
    DEFAULT_REGION: 'eu',
  });
  assert.deepEqual(cfg.regions.get('us').routers, ['https://r1.us.example', 'http://r2.us.example:8090']);
  assert.equal(cfg.byContinent.get('NA').name, 'us');
  assert.equal(cfg.fallback.name, 'eu');
  assert.equal(cfg.admin, null);
  assert.equal(cfg.share, true);
  assert.throws(() => configure({}), /names no region/);
  assert.throws(() => configure({ REGIONS: '{"us": {"routers": []}}' }), /names no router/);
  assert.throws(() => configure({ REGIONS: '{"us": ["ftp://x"]}' }), /not http/);
  assert.throws(() => configure({ REGIONS: '{"us": ["https://x"]}', DEFAULT_REGION: 'ap' }), /not one of REGIONS/);
  assert.throws(() => configure({ REGIONS: '{"us": ["https://x"]}', HOMES: '-1' }), /count/);
});

test('a home is forgotten first-heard first past the bound, and moved on a change', () => {
  const s = new State();
  assert.equal(s.learn('a', 'us', 2), true);
  assert.equal(s.learn('a', 'us', 2), false);
  s.learn('b', 'us', 2);
  s.learn('c', 'eu', 2);
  assert.deepEqual([...s.homes], [['b', 'us'], ['c', 'eu']]);
  // A change puts the tenant last: it is not the next forgotten.
  s.learn('b', 'eu', 2);
  s.learn('d', 'us', 2);
  assert.deepEqual([...s.homes], [['b', 'eu'], ['d', 'us']]);
  // Bound 0: nothing kept.
  assert.equal(s.learn('e', 'us', 0), true);
  assert.equal(s.homes.has('e'), false);
});

test('a request goes on less its hops, the marks and every address a client claims', () => {
  const client = `203.0.113.7 1800000000 ${markOf('203.0.113.7', 1_800_000_000)}`;
  const sent = sentHeaders(
    new Headers({
      Authorization: 'Bearer t',
      'Idempotency-Key': 'k',
      'Fenec-Wait': '100',
      traceparent: '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
      'Fenec-Router': '9.9.9.9 forged',
      'Fenec-Global': '1',
      'Fenec-Verified': '1',
      'Fenec-Client': '8.8.8.8 forged',
      'X-Forwarded-For': '6.6.6.6',
      'CF-Connecting-IP': '203.0.113.7',
      'CF-Ray': '8f1e2d3c4b5a6978-FRA',
    }),
    client,
  );
  assert.deepEqual(Object.fromEntries(sent), {
    authorization: 'Bearer t',
    'idempotency-key': 'k',
    'fenec-wait': '100',
    traceparent: '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
    'fenec-home': '1',
    'fenec-client': client,
    'x-request-id': '8f1e2d3c4b5a6978-FRA',
  });
  // No mark, no client named: the address alone would be believed of anyone.
  assert.equal(sentHeaders(new Headers({ 'CF-Connecting-IP': '203.0.113.7' }), null).has('fenec-client'), false);
});

test("an answer is a router's where it carries the router's mark, as every answer a router writes does", () => {
  const answer = (status, headers = {}) => new Response(null, { status, headers });
  assert.equal(judged(answer(200)), 'router');
  assert.equal(judged(answer(502, { 'X-Request-Id': 'r', 'Fenec-Shard': '1' })), 'router');
  assert.equal(judged(answer(503, { 'Fenec-Shard': '1' })), 'router');
  // A load balancer's 502 with a request id of its own is not a router's.
  assert.equal(judged(answer(502, { 'X-Request-Id': 'lb-1' })), 'unknown');
  // A tunnel's connector that could not reach the router, Cloudflare timing out.
  for (const s of [502, 503, 504, 520, 522, 524]) assert.equal(judged(answer(s)), 'unknown', String(s));
  for (const s of [521, 523, 525, 526, 530]) assert.equal(judged(answer(s)), 'unrun', String(s));
});

/**
 * A stream's text, read as it arrives and kept from its first byte:
 * `until(done)` waits for the text read so far to hold. One reader for the
 * stream's life: a `data` listener taken off between two waits leaves the
 * stream flowing, and what came meanwhile -- the change a test wrote while
 * the stream was not listened to -- was dropped (CI: a stream's change
 * reached it before its write's answer reached the test, and was never
 * read).
 */
function reading(res) {
  let text = '';
  let ended = false;
  const waiting = new Set();
  res.setEncoding('utf8');
  res.on('data', (d) => {
    text += d;
    for (const w of waiting) w();
  });
  res.once('end', () => {
    ended = true;
    for (const w of waiting) w();
  });
  // An abort ends it with an error nobody waits for.
  res.on('error', () => {});
  return {
    until(done, ms = 30_000) {
      return new Promise((ok, fail) => {
        const timer = setTimeout(() => {
          waiting.delete(check);
          fail(new Error(`waited ${ms} ms for the stream; read: ${text}`));
        }, ms);
        const check = () => {
          if (!done(text) && !ended) return;
          clearTimeout(timer);
          waiting.delete(check);
          if (done(text)) ok(text);
          else fail(new Error(`the stream ended; read: ${text}`));
        };
        waiting.add(check);
        check();
      });
    },
  };
}

/** A router's count of the tenant requests it answered. */
async function forwarded(router) {
  const m = await shard(router.url, 'GET', '/_metrics');
  return m.text
    .split('\n')
    .filter((l) => l.startsWith('fenec_router_requests_total{route="tenant"'))
    .reduce((n, l) => n + Number(l.split(' ').pop()), 0);
}

const SCHEMA =
  'create collection notes (title text); create collection jobs (n int, run_at timestamp @sorted, owner text)';

describe('the entry in front of two regions', { skip, timeout: 300_000 }, () => {
  let w;
  let front;
  let shared;
  let guess;
  let gone;
  let state;

  before(async () => {
    w = await cluster.world({
      tenants: [
        ['acme', 'us'],
        ['beta', 'eu'],
        ['mover', 'us'],
        ['live', 'us'],
        ['feed', 'us'],
        ['fresh1', 'us'],
        ['fresh2', 'us'],
        ['fresh3', 'us'],
        ['fresh4', 'us'],
        ['fresh5', 'us'],
        ['fresh6', 'us'],
      ],
      schema: SCHEMA,
      // A silent stream writes a keep-alive every second, so a router and
      // a node learn of a client gone within one; one stream a tenant, so
      // a stream still held is a refused one.
      node: ['--http-keepalive', '1', '--http-max-streams', '1'],
    });
    state = await mkdtemp(join(tmpdir(), 'fenecdb-entry-state-'));
    const regions = { us: { routers: [w.us.url, w.usStandby.url] }, eu: { routers: [w.eu.url] } };
    front = await serve('entry', state, {
      REGIONS: JSON.stringify(regions),
      DEFAULT_REGION: 'eu',
      CORS: ORIGIN,
      ENTRY_TOKEN: SECRET,
    });
    // An isolate that keeps no home of its own: every request one the
    // colo's cache answers, or a relay.
    shared = await serve('entry', join(state, 'shared'), {
      REGIONS: JSON.stringify(regions),
      DEFAULT_REGION: 'eu',
      HOMES: '0',
    });
    // One whose default region is down: eu's router is a port nothing
    // listens on.
    guess = await serve('entry', join(state, 'guess'), {
      REGIONS: JSON.stringify({ eu: { routers: [(gone = await refusing()).url] }, us: regions.us }),
      DEFAULT_REGION: 'eu',
      HOMES_CACHE: 'off',
      // A region found down is asked first again at once by a read: a
      // write must reach the home whatever the time since.
      DOWN_MS: '0',
    });
  });

  after(async () => {
    await front?.stop();
    await shared?.stop();
    await guess?.stop();
    gone?.close();
    await w?.stop();
    if (state) await rm(state, { recursive: true, force: true });
  });

  test('a tenant goes to its home region, learnt from the routers', async () => {
    const [eu, us, standby] = [await forwarded(w.eu), await forwarded(w.us), await forwarded(w.usStandby)];
    // acme is us's; the entry has not seen it, and sends it to eu first,
    // whose router relays it home -- and the answer names the home, to the
    // entry alone.
    for (let i = 0; i < 5; i++) {
      const r = await query(front.url, 'acme', 'put notes {title: $1}', [`n${i}`]);
      assert.equal(r.status, 200, r.text);
      assert.equal(r.header('fenec-home'), undefined);
    }
    for (let i = 0; i < 3; i++) assert.equal((await query(front.url, 'beta', 'get notes count')).status, 200);
    // eu answered acme's first request alone, and beta's; us all of acme's.
    assert.equal(await forwarded(w.eu), eu + 1 + 3);
    assert.equal(await forwarded(w.us), us + 5);
    assert.equal(await forwarded(w.usStandby), standby);
    const count = await query(w.us.url, 'acme', 'get notes count');
    assert.deepEqual(count.json(), [{ count: 5 }]);
  });

  test("a tenant's home is told nobody, a request refused for its token least of all", async () => {
    // The router names it on the refusal too: the entry drops it.
    const r = await request(`${front.url}/t/acme/query`, { method: 'POST', body: '{"query":"get notes count"}' });
    assert.equal(r.status, 401, r.text);
    assert.equal(r.header('fenec-home'), undefined);
    const asked = await query(front.url, 'acme', 'get notes count', undefined, { 'Fenec-Home': '1' });
    assert.equal(asked.status, 200);
    assert.equal(asked.header('fenec-home'), undefined);
  });

  test('a tenant moved across regions is relayed once, and then sent to its new home', async () => {
    for (let i = 0; i < 2; i++) assert.equal((await query(front.url, 'mover', 'put notes {title: "before"}')).status, 200);
    const moved = await shard(w.global.url, 'POST', '/_shard/tenants/mover/move', { region: 'eu' });
    assert.equal(moved.status, 200, moved.text);
    // The home the entry has is us: us's router answers it moving (503,
    // until its copy of the global directory has the move) or relays it to
    // eu, whose answer names eu.
    await until(async () => {
      const r = await query(front.url, 'mover', 'get notes count');
      assert.ok(r.status === 200 || r.status === 503, `${r.status} ${r.text}`);
      return r.status === 200;
    }, 'mover answered');
    const [eu, us] = [await forwarded(w.eu), await forwarded(w.us)];
    for (let i = 0; i < 4; i++) assert.equal((await query(front.url, 'mover', 'put notes {title: "after"}')).status, 200);
    assert.equal(await forwarded(w.eu), eu + 4);
    assert.equal(await forwarded(w.us), us, 'nothing more by us');
    assert.deepEqual((await query(w.eu.url, 'mover', 'get notes count')).json(), [{ count: 6 }]);
  });

  test('a tenant not yet heard reaches its home past a region that is down', async () => {
    const auth = { Authorization: `Bearer ${DATA}` };
    // eu, the region guessed first, does not answer: the read goes on to us
    // in the same request, whose router places it.
    const read = await request(`${guess.url}/t/fresh1/notes`, { headers: auth });
    assert.equal(read.status, 200, read.text);
    // A write, which may not go to a second router once it may have reached
    // the first, goes to us first while eu has not answered since -- each
    // one, however long after: it lands in us, none told it may have run.
    const us = await forwarded(w.us);
    for (const t of ['fresh2', 'fresh3', 'fresh4', 'fresh5']) {
      const wrote = await query(guess.url, t, 'put notes {title: "past eu"}');
      assert.equal(wrote.status, 200, `${t}: ${wrote.text}`);
      assert.deepEqual((await query(w.us.url, t, 'get notes count')).json(), [{ count: 1 }], t);
    }
    // A read asks eu first again, and still goes on.
    assert.equal((await request(`${guess.url}/t/fresh6/notes`, { headers: auth })).status, 200);
    assert.equal(await forwarded(w.us), us + 4 + 4 + 1);
  });

  test('a subscription streams through change by change, and lets go with its client', async () => {
    const auth = { Authorization: `Bearer ${DATA}` };
    const ac = new AbortController();
    const s = await request(`${front.url}/t/live/notes/changes`, { headers: auth, signal: ac.signal, stream: true });
    assert.equal(s.status, 200);
    assert.match(s.header('content-type'), /text\/event-stream/);
    const events = reading(s.res);
    await events.until((t) => t.includes('event: seed'));
    // A change written elsewhere reaches the open stream: nothing waits for
    // the stream to end. It may reach it before the write's answer reaches
    // the test, which the reader keeps.
    assert.equal((await query(w.us.url, 'live', 'put notes {title: "streamed"}')).status, 200);
    await events.until((t) => t.includes('streamed'));
    ac.abort();
    // One stream a tenant: the node takes another once the entry's has
    // gone -- the entry's fetch cancelled, the router's write and then the
    // node's failing at the next keep-alive.
    await until(async () => {
      const ac2 = new AbortController();
      const r = await request(`${w.us.url}/t/live/notes/changes`, { headers: auth, signal: ac2.signal, stream: true }).catch(() => null);
      r?.res.on('error', () => {});
      ac2.abort();
      return r?.status === 200;
    }, 'the stream let go of at the node', 30_000);
  });

  test('a held claim waits at the node until a job comes, and /_changes until a write', async () => {
    const claim = 'set jobs {owner: $1, run_at: now() + 60000} where run_at <= now() order run_at limit 1 returning n';
    const answer = query(front.url, 'live', claim, ['w1'], { 'Fenec-Wait': '20000' });
    // The job put once the claim is held at the node, as its
    // `fenec_held_requests` says: the answer is the held claim's.
    const held = async () => {
      const m = await request(`${w.nodes.us.url}/_metrics`, { headers: { Authorization: 'Bearer adm-us' } });
      return m.text.split('\n').includes('fenec_held_requests 1');
    };
    await until(held, 'the claim held at the node');
    const put = await query(w.us.url, 'live', 'put jobs {n: 7, run_at: now(), owner: ""}');
    assert.equal(put.status, 200, put.text);
    const r = await answer;
    assert.equal(r.status, 200, r.text);
    assert.deepEqual(r.json(), [{ n: 7 }]);

    // A change feed's wait, the same way: the write made once the wait is
    // held at the node, which counts it among the tenant's streams where
    // the tenant has a quota of them.
    const admin = { Authorization: 'Bearer adm-us', 'Content-Type': 'application/json' };
    const set = await request(`${w.nodes.us.url}/_admin/tenants/feed/quota`, { method: 'PUT', headers: admin, body: '{"streams": 100}' });
    assert.ok(set.status < 300, `${set.status} ${set.text}`);
    const first = await query(w.us.url, 'feed', 'put notes {title: "first"}');
    const seq = Number(first.header('fenec-seq'));
    const changes = request(`${front.url}/t/feed/_changes?since=${seq}&wait=20000`, { headers: { Authorization: `Bearer ${DATA}` } });
    const waiting = async () => {
      const q = await request(`${w.nodes.us.url}/_admin/tenants/feed/quota`, { headers: admin });
      return q.json().usage.streams === 1;
    };
    await until(waiting, 'the wait held at the node');
    assert.equal((await query(w.us.url, 'feed', 'put notes {title: "fed"}')).status, 200);
    const fed = await changes;
    assert.equal(fed.status, 200, fed.text);
    assert.match(fed.text, /fed/);
  });

  test('the admin paths are not served, however the path is spelled', async () => {
    const plain = ['/_shard/tenants', '/_admin/tenants', '/_replication', '/_metrics', '/_studio/', '/', '/t', '/t/acme/_replication', '/t/Acme/query'];
    // As the router and the node decode them, each of these is one of the
    // above: the entry decodes them the same way first.
    const spelled = [
      '/t/acme/%5Freplication',
      '/t/acme/%5freplication',
      '/t/acme%2F_replication',
      '/t/acme/%2F_replication',
      '/t//acme//_replication',
      '/t/%61cme/_repl%69cation',
      '/%5Freplication',
      '/%5Fshard/tenants',
      '/%5Fadmin/tenants',
      '/%74/acme/%5Freplication',
    ];
    for (const path of [...plain, ...spelled]) {
      // A tenant's replication token, which its stream would take.
      const ac = new AbortController();
      const r = await request(`${front.url}${path}`, {
        headers: { Authorization: path.includes('repl') ? 'Bearer tenant-replication' : `Bearer ${ROOT}` },
        signal: ac.signal,
        stream: true,
      });
      r.res.on('error', () => {});
      ac.abort();
      assert.equal(r.status, 404, path);
      assert.match(r.header('content-type'), /application\/json/, path);
      // Its own answers carry the routers' CORS headers, once.
      assert.deepEqual(r.all('access-control-allow-origin'), [ORIGIN], path);
    }
  });

  test("a preflight is answered here as a router answers it, and a router's answer carries its CORS headers once", async () => {
    const pre = { Origin: ORIGIN, 'Access-Control-Request-Method': 'POST', 'Access-Control-Request-Headers': 'authorization' };
    const [mine, router] = [
      await request(`${front.url}/t/acme/query`, { method: 'OPTIONS', headers: pre }),
      await request(`${w.us.url}/t/acme/query`, { method: 'OPTIONS', headers: pre }),
    ];
    assert.equal(mine.status, 204);
    assert.equal(router.status, 204);
    const cors = (r) => Object.fromEntries(r.raw.filter(([k]) => k.startsWith('access-control-') || k === 'vary'));
    assert.deepEqual(cors(mine), cors(router));
    assert.deepEqual(
      cors(mine),
      Object.fromEntries(Object.entries(corsHeaders(ORIGIN)).map(([k, v]) => [k.toLowerCase(), v])),
    );
    const r = await query(front.url, 'acme', 'get notes count', undefined, { Origin: ORIGIN });
    assert.equal(r.status, 200);
    assert.deepEqual(r.all('access-control-allow-origin'), [ORIGIN]);
  });

  test("an isolate that has not heard a tenant's home reads it from the colo's cache", async () => {
    // The first request relayed by eu, whose answer names us, kept in the
    // cache once the answer is out (`waitUntil`): until then a request may
    // be relayed again.
    await until(async () => {
      const eu = await forwarded(w.eu);
      assert.equal((await query(shared.url, 'acme', 'get notes count')).status, 200);
      return (await forwarded(w.eu)) === eu;
    }, 'a request sent to us');
    const [eu, us] = [await forwarded(w.eu), await forwarded(w.us)];
    for (let i = 0; i < 4; i++) assert.equal((await query(shared.url, 'acme', 'get notes count')).status, 200);
    assert.equal(await forwarded(w.eu), eu);
    assert.equal(await forwarded(w.us), us + 4);
  });

  // Last: it ends us's routers.
  test('a router gone is passed over for its standby, and a known home gone is a 503 -- never another region', async () => {
    assert.equal((await query(front.url, 'acme', 'get notes count')).status, 200);
    const eu = await forwarded(w.eu);
    const auth = { Authorization: `Bearer ${DATA}` };
    await w.us.kill();
    // A read goes on to the standby in the same request.
    const read = await request(`${front.url}/t/acme/notes`, { headers: auth });
    assert.equal(read.status, 200, read.text);
    // And so do writes, the standby asked first from then on.
    const wrote = await query(front.url, 'acme', 'put notes {title: "on the standby"}');
    assert.equal(wrote.status, 200, wrote.text);

    await w.usStandby.kill();
    // A write that may have reached a router is not sent to another: the
    // client is told so, and decides.
    const once = await query(front.url, 'acme', 'put notes {title: "maybe"}');
    assert.equal(once.status, 502, once.text);
    assert.equal(once.header('retry-after'), undefined);
    assert.match(once.json().error, /may have run/);
    // A read, or a write under its key, tries every router and is told
    // when to come back.
    for (const r of [
      await request(`${front.url}/t/acme/notes`, { headers: auth }),
      await query(front.url, 'acme', 'put notes {title: "keyed"}', undefined, { 'Idempotency-Key': 'k-1' }),
    ]) {
      assert.equal(r.status, 503, r.text);
      assert.equal(r.header('retry-after'), '1');
      assert.equal(r.json().retry_after, 1);
      assert.match(r.json().error, /region `us`'s routers did not answer/);
      // The routers' hostnames are the operator's: logged, not told.
      for (const router of [w.us, w.usStandby]) assert.ok(!r.text.includes(router.addr), r.text);
    }
    // eu was never asked for us's tenant: its writes stay in us.
    assert.equal(await forwarded(w.eu), eu);
  });
});

/**
 * A stand-in router: each request recorded with its headers, answered by
 * `answer(req, res, one)`; each record's `closed` turns true as its
 * connection closes.
 */
function standIn(answer) {
  const seen = [];
  const server = http.createServer((req, res) => {
    const one = { method: req.method, url: req.url, headers: req.headers, closed: false };
    seen.push(one);
    res.on('close', () => (one.closed = true));
    let body = '';
    req.on('data', (d) => (body += d));
    req.on('end', () => {
      one.body = body;
      answer(req, res, one);
    });
  });
  return new Promise((ok) =>
    server.listen(0, '127.0.0.1', () => ok({ url: `http://127.0.0.1:${server.address().port}`, seen, close: () => server.close() })),
  );
}

/**
 * The handler in Node, Node's fetch for Cloudflare's, over `env`: what
 * `wrangler dev` cannot show -- a client gone before its answer's head
 * never reaches a Worker there, workerd noticing a client only as it
 * writes, where Cloudflare aborts `request.signal` -- and what is plainest
 * to hold without it.
 */
function inNode(env) {
  const worker = entry();
  return (path, init) => worker.fetch(new Request(`http://entry.test${path}`, init), env, { waitUntil() {} });
}

test('a request held at its router is let go of with its client', async () => {
  const stub = await standIn(() => {});
  try {
    const fetchOf = inNode({ REGIONS: JSON.stringify({ stub: [stub.url] }), HOMES_CACHE: 'off' });
    const ac = new AbortController();
    const answered = fetchOf('/t/held/query', {
      method: 'POST',
      body: '{"query":"set jobs {owner: 1} limit 1"}',
      headers: { 'Fenec-Wait': '30000' },
      signal: ac.signal,
    });
    const at = await until(() => stub.seen[0], 'the request at the router', 10_000);
    assert.equal(at.closed, false);
    ac.abort();
    await until(() => at.closed, 'the request let go of at the router', 10_000);
    assert.equal((await answered).status, 499);
  } finally {
    stub.close();
  }
});

test("a guessed region's router that cannot reach the home is passed for a read, and is the answer to a write", async () => {
  // cut's router is cut off from the tenant's home: it answers 502, its
  // own, naming no home. home's router places the tenant.
  const cut = await standIn((req, res) => {
    res.writeHead(502, { 'Content-Type': 'application/json', 'X-Request-Id': 'c1', 'Fenec-Shard': '1' });
    res.end('{"error":"region `home`\'s routers did not answer"}');
  });
  const home = await standIn((req, res) => {
    res.writeHead(200, { 'Content-Type': 'application/json', 'X-Request-Id': 'h1', 'Fenec-Shard': '1', 'Fenec-Home': 'home' });
    res.end('[]');
  });
  // A home's own failure names the home: the answer, not passed.
  const failing = await standIn((req, res) => {
    res.writeHead(502, { 'Content-Type': 'application/json', 'X-Request-Id': 'f1', 'Fenec-Shard': '1', 'Fenec-Home': 'cut' });
    res.end('{"error":"node `n1` did not answer"}');
  });
  try {
    const regions = (first) => JSON.stringify({ cut: [first.url], home: [home.url] });
    let fetchOf = inNode({ REGIONS: regions(cut), DEFAULT_REGION: 'cut', HOMES_CACHE: 'off' });
    const read = await fetchOf('/t/acme/notes');
    assert.equal(read.status, 200);
    assert.equal(home.seen.length, 1);
    // A write a router answered may have been relayed: its answer is the client's.
    const write = await fetchOf('/t/other/query', { method: 'POST', body: '{"query":"put"}' });
    assert.equal(write.status, 502);
    assert.match(await write.text(), /did not answer/);
    assert.equal(home.seen.length, 1);
    fetchOf = inNode({ REGIONS: regions(failing), DEFAULT_REGION: 'cut', HOMES_CACHE: 'off' });
    const homes = await fetchOf('/t/acme/notes');
    assert.equal(homes.status, 502);
    assert.equal(home.seen.length, 1, 'no other region asked');
  } finally {
    for (const s of [cut, home, failing]) s.close();
  }
});

test('a body sent with no length is read up to what is kept for a retry, and refused past it', async () => {
  const stub = await standIn((req, res) => res.end('{}'));
  try {
    const fetchOf = inNode({ REGIONS: JSON.stringify({ stub: [stub.url] }), HOMES_CACHE: 'off' });
    /** A body of `n` bytes as a stream, so with no length. */
    const streamed = (n) => {
      let left = n;
      return new ReadableStream({
        pull(c) {
          if (left === 0) return c.close();
          const k = Math.min(left, 64 << 10);
          left -= k;
          c.enqueue(new Uint8Array(k).fill(0x20));
        },
      });
    };
    const small = await fetchOf('/t/acme/query', { method: 'POST', body: streamed(1000), duplex: 'half' });
    assert.equal(small.status, 200);
    // Sent on with its length: a router takes no chunked body.
    assert.equal(stub.seen.at(-1).headers['content-length'], '1000');
    const seen = stub.seen.length;
    const big = await fetchOf('/t/acme/query', { method: 'POST', body: streamed(BUFFER_AT + 1), duplex: 'half' });
    assert.equal(big.status, 411);
    assert.match((await big.json()).error, /Content-Length/);
    assert.equal(stub.seen.length, seen, 'nothing sent');
  } finally {
    stub.close();
  }
});

test('the routers sending CORS headers to an entry with no CORS is said, once', async () => {
  const stub = await standIn((req, res) => {
    res.writeHead(200, { 'Access-Control-Allow-Origin': ORIGIN });
    res.end('{}');
  });
  const said = [];
  const warn = console.warn;
  console.warn = (...a) => said.push(a.join(' '));
  try {
    const fetchOf = inNode({ REGIONS: JSON.stringify({ stub: [stub.url] }), HOMES_CACHE: 'off' });
    for (let i = 0; i < 3; i++) assert.equal((await fetchOf('/t/acme/notes')).status, 200);
    assert.equal(said.filter((s) => s.includes('CORS is not set')).length, 1, said.join('\n'));
  } finally {
    console.warn = warn;
    stub.close();
  }
});

describe('the entry in front of stand-in routers', { skip, timeout: 180_000 }, () => {
  let stub;
  let unreached;
  let connector;
  let timedOut;
  let failing;
  let balancer;
  let w;
  let front;
  let state;

  before(async () => {
    stub = await standIn((req, res, e) => {
      if (req.url.startsWith('/t/stream/')) {
        res.writeHead(200, { 'Content-Type': 'text/event-stream' });
        res.write('event: seed\ndata: {}\n\n');
        // A keep-alive a second, as a node's stream writes one: a client
        // gone is noticed as one is written, by workerd and by a router.
        const beat = setInterval(() => res.write(': keepalive\n\n'), 1000);
        res.on('close', () => clearInterval(beat));
        return;
      }
      // A tenant named `to-<region>` lives in <region>, as a router says.
      const home = /^\/t\/to-([a-z0-9]+)\//.exec(req.url)?.[1];
      res.writeHead(200, {
        'Content-Type': 'application/json',
        'X-Request-Id': 'stub',
        'Fenec-Shard': '1',
        'Fenec-Verified': '1',
        'Fenec-Seq': '5',
        ...(home ? { 'Fenec-Home': home } : {}),
      });
      res.end(JSON.stringify({ body: e.body }));
    });
    // What Cloudflare answers a Worker for a router behind a tunnel with no
    // connector: the request never reached it.
    unreached = await standIn((req, res) => {
      res.writeHead(530, { 'Content-Type': 'text/html' });
      res.end('error code: 1033');
    });
    // A tunnel's connector that cannot reach its router (Cloudflare's
    // "502 Bad Gateway: Unable to reach the origin service"): the request
    // may have reached it.
    connector = await standIn((req, res) => {
      res.writeHead(502, { 'Content-Type': 'text/plain; charset=utf-8' });
      res.end('Bad Gateway');
    });
    // Cloudflare's 522: the connection timed out, an acknowledgement of the
    // request's bytes among what it counts.
    timedOut = await standIn((req, res) => {
      res.writeHead(522, { 'Content-Type': 'text/html' });
      res.end('error code: 522');
    });
    // A router's own 502 -- a node it could not reach -- is an answer.
    failing = await standIn((req, res) => {
      res.writeHead(502, { 'Content-Type': 'application/json; charset=utf-8', 'X-Request-Id': 'r1', 'Fenec-Shard': '1' });
      res.end('{"error":"node `n1` did not answer"}');
    });
    // A load balancer in front of a router it cannot reach: a 502 with a
    // request id of its own, and no router's mark.
    balancer = await standIn((req, res) => {
      res.writeHead(502, { 'Content-Type': 'text/html', 'X-Request-Id': 'lb-1' });
      res.end('<h1>502 Bad Gateway</h1>');
    });
    // The global router, for /_shard/ when it is let through.
    w = await cluster.world({});
    state = await mkdtemp(join(tmpdir(), 'fenecdb-entry-stub-'));
    front = await serve('entry', state, {
      REGIONS: JSON.stringify({
        stub: [stub.url],
        far: [unreached.url, stub.url],
        far2: [unreached.url, stub.url],
        tunnel: [connector.url, stub.url],
        tunnel2: [connector.url, stub.url],
        slow: [timedOut.url, stub.url],
        own: [failing.url, stub.url],
        balanced: [balancer.url, stub.url],
      }),
      DEFAULT_REGION: 'stub',
      ADMIN: JSON.stringify([w.global.url]),
      ENTRY_TOKEN: SECRET,
    });
  });

  after(async () => {
    await front?.stop();
    await w?.stop();
    for (const s of [stub, unreached, connector, timedOut, failing, balancer]) s?.close();
    if (state) await rm(state, { recursive: true, force: true });
  });

  /** A tenant of `region`, heard by the entry: its first request's answer names the region. */
  async function homed(region) {
    const r = await request(`${front.url}/t/to-${region}/query`, { method: 'POST', body: '{}' });
    assert.equal(r.status, 200, r.text);
    assert.equal(r.header('fenec-home'), undefined);
    return `/t/to-${region}`;
  }

  test('the headers a client may send go on, the marks and its claimed addresses do not, and the client is named', async () => {
    const r = await request(`${front.url}/t/acme/query`, {
      method: 'POST',
      headers: {
        Authorization: 'Bearer t',
        'Content-Type': 'application/json',
        'Idempotency-Key': 'k-2',
        'Fenec-Wait': '5',
        'Fenec-After': '3',
        traceparent: '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01',
        tracestate: 'a=b',
        'X-Request-Id': 'rid-1',
        'If-None-Match': '"e"',
        'Fenec-Router': '9.9.9.9 forged',
        'Fenec-Global': '1',
        'Fenec-Verified': '1',
        'Fenec-Client': '8.8.8.8 forged',
        'Fenec-Home': 'eu',
        'X-Forwarded-For': '6.6.6.6',
        'X-Real-IP': '6.6.6.6',
        Forwarded: 'for=6.6.6.6',
        'True-Client-IP': '6.6.6.6',
        // What Cloudflare puts on a request; `wrangler dev` passes the
        // client's through, which stands in for it here.
        'CF-Connecting-IP': '203.0.113.7',
      },
      body: '{"query":"get notes count"}',
    });
    assert.equal(r.status, 200, r.text);
    const seen = stub.seen.at(-1);
    assert.equal(seen.body, '{"query":"get notes count"}');
    const h = seen.headers;
    for (const [k, v] of [
      ['authorization', 'Bearer t'],
      ['content-type', 'application/json'],
      ['idempotency-key', 'k-2'],
      ['fenec-wait', '5'],
      ['fenec-after', '3'],
      ['traceparent', '00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01'],
      ['tracestate', 'a=b'],
      ['x-request-id', 'rid-1'],
      ['if-none-match', '"e"'],
      ['fenec-home', '1'],
    ]) {
      assert.equal(h[k], v, k);
    }
    namesClient(h['fenec-client'], '203.0.113.7');
    for (const k of ['fenec-router', 'fenec-global', 'fenec-verified', 'x-real-ip', 'forwarded', 'true-client-ip']) assert.equal(h[k], undefined, k);
    // Nothing the client claimed reaches the router, under any name.
    const all = JSON.stringify(h);
    for (const ip of ['6.6.6.6', '8.8.8.8', '9.9.9.9']) assert.ok(!all.includes(ip), `${ip} in ${all}`);
    // The answer: the router's, less whether a node verified a token.
    assert.equal(r.header('fenec-verified'), undefined);
    assert.equal(r.header('fenec-seq'), '5');
    // No CORS configured: a preflight goes on to the router.
    const pre = await request(`${front.url}/t/acme/query`, { method: 'OPTIONS', headers: { Origin: ORIGIN } });
    assert.equal(pre.status, 200);
    assert.equal(stub.seen.at(-1).method, 'OPTIONS');
  });

  test('a stream is let go of at the router as its client goes', async () => {
    const ac = new AbortController();
    const s = await request(`${front.url}/t/stream/notes/changes`, { signal: ac.signal, stream: true });
    await reading(s.res).until((t) => t.includes('event: seed'));
    const streamed = stub.seen.at(-1);
    assert.equal(streamed.closed, false);
    ac.abort();
    await until(() => streamed.closed, 'the stream closed at the router', 10_000);
  });

  test('a router Cloudflare could not reach is passed over, a write too: it never ran there', async () => {
    // far's first router is unreached; the entry's next request after
    // hearing the home goes to it, then on.
    const path = await homed('far');
    const before = unreached.seen.length;
    const r = await request(`${front.url}${path}/query`, { method: 'POST', body: '{"query":"put"}' });
    assert.equal(r.status, 200, r.text);
    assert.equal(unreached.seen.length, before + 1);
    assert.equal(stub.seen.at(-1).body, '{"query":"put"}');
  });

  test("a router a tunnel's connector cannot reach is passed over for a read, and a write is told it may have run", async () => {
    // tunnel: a read goes on to the standby in the same request.
    let path = await homed('tunnel');
    let before = connector.seen.length;
    const read = await request(`${front.url}${path}/notes`);
    assert.equal(read.status, 200, read.text);
    assert.equal(connector.seen.length, before + 1);
    assert.equal(stub.seen.at(-1).url, `${path}/notes`);
    // tunnel2: a write is not sent again, and the next goes to the standby.
    path = await homed('tunnel2');
    before = stub.seen.length;
    const once = await request(`${front.url}${path}/query`, { method: 'POST', body: '{"query":"put"}' });
    assert.equal(once.status, 502, once.text);
    assert.match(once.json().error, /may have run/);
    assert.equal(stub.seen.length, before, 'not sent again');
    const next = await request(`${front.url}${path}/query`, { method: 'POST', body: '{"query":"put"}' });
    assert.equal(next.status, 200, next.text);
  });

  test("a 522 may have reached the router: a write is not sent again, a router's own 502 is its answer", async () => {
    let path = await homed('slow');
    let before = stub.seen.length;
    const once = await request(`${front.url}${path}/query`, { method: 'POST', body: '{"query":"put"}' });
    assert.equal(once.status, 502, once.text);
    assert.match(once.json().error, /may have run/);
    assert.equal(stub.seen.length, before, 'not sent again');
    // A router's 502, with its request id: passed on, the standby not asked.
    path = await homed('own');
    before = stub.seen.length;
    const r = await request(`${front.url}${path}/notes`);
    assert.equal(r.status, 502);
    assert.equal(r.header('x-request-id'), 'r1');
    assert.match(r.text, /node `n1` did not answer/);
    assert.equal(stub.seen.length, before);
  });

  test("a 502 with a request id but no router's mark is not the router's: a read goes on", async () => {
    const path = await homed('balanced');
    const before = balancer.seen.length;
    const r = await request(`${front.url}${path}/notes`);
    assert.equal(r.status, 200, r.text);
    assert.equal(balancer.seen.length, before + 1);
    assert.equal(stub.seen.at(-1).url, `${path}/notes`);
  });

  test('a body past what is kept for a retry goes to one router, its length kept', async () => {
    // far2's first router is unreached, as Cloudflare says: a body of 2
    // MiB is streamed to it and not kept, so it cannot go on to the next --
    // the client is told to come back, which it may: it never ran.
    const path = await homed('far2');
    const big = JSON.stringify({ query: 'put', pad: 'x'.repeat(2 << 20) });
    const r = await request(`${front.url}${path}/query`, { method: 'POST', body: big });
    assert.equal(r.status, 503, r.text);
    assert.equal(r.json().retry_after, 1);
    // The next goes to the next router, whole.
    const ok = await request(`${front.url}${path}/query`, { method: 'POST', body: big });
    assert.equal(ok.status, 200, ok.text);
    const seen = stub.seen.at(-1);
    assert.equal(seen.headers['content-length'], String(Buffer.byteLength(big)));
    assert.equal(seen.headers['transfer-encoding'], undefined);
    assert.equal(seen.body.length, big.length);
  });

  test('a home in a region the entry does not know is not one it sends to', async () => {
    const before = stub.seen.length;
    for (let i = 0; i < 2; i++) {
      const r = await request(`${front.url}/t/to-mars/query`, { method: 'POST', body: '{}' });
      assert.equal(r.status, 200);
    }
    // Both to the default region's router, the stand-in.
    assert.equal(stub.seen.length, before + 2);
  });

  test('/_shard/ goes to the global router where it is let through', async () => {
    const ok = await request(`${front.url}/_shard/tenants`, { headers: { Authorization: `Bearer ${ROOT}` } });
    assert.equal(ok.status, 200, ok.text);
    assert.ok(Array.isArray(ok.json()));
    const refused = await request(`${front.url}/_shard/tenants`);
    assert.equal(refused.status, 401);
    // Still not the rest.
    assert.equal((await request(`${front.url}/_replication`)).status, 404);
  });
});
