// The studio's two ways to a database answer alike: a fenec-server over
// HTTP, and the browser module in the page (engine.js, which the
// playground's worker runs). The same statements go to both, from the same
// empty database, and every answer -- status, body and the change a write
// left the database at -- is held equal, so no view of the studio has to
// ask which it has. A shape's subscription too: its seed and each change.
//
//   node --test studio/test/transport.test.mjs       (make test-js)
//
// Needs web/fenec.wasm (`make wasm`) and the debug fenec-server (`cargo
// build -p fenec-server`); skipped without them, and under CI a failure.

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const WASM = join(ROOT, 'web', 'fenec.wasm');
const SERVER = process.env.FENEC_SERVER ?? join(ROOT, 'target', 'debug', 'fenec-server');
const missing = !existsSync(WASM) ? 'web/fenec.wasm: make wasm' : !existsSync(SERVER) ? 'the debug fenec-server: cargo build -p fenec-server' : null;
if (missing && process.env.CI) throw new Error(`the transport test needs ${missing}`);

const { Engine } = await import('../engine.js');
const { Fenec, connect } = await import('../../web/fenec.js');

let server;
let engine;
let token;

test.before(async () => {
  if (missing) return;
  const harness = await import('./harness.mjs');
  token = harness.TOKEN;
  server = await harness.startServer({ studio: false });
  engine = new Engine(await Fenec.open(readFileSync(WASM)));
});

test.after(() => server?.stop());

/** `{status, json, seq}` from the server. */
async function remote(method, path, body = null, type = 'application/json') {
  const headers = { authorization: `Bearer ${token}` };
  if (body !== null) headers['content-type'] = type;
  const res = await fetch(`${server.url}${path}`, { method, headers, body });
  const text = await res.text();
  const seq = res.headers.get('fenec-seq');
  return { status: res.status, json: text ? JSON.parse(text) : null, seq: seq === null ? null : Number(seq) };
}

/** `{status, json, seq}` from the module. */
async function local(method, path, body = null) {
  const { status, json, seq } = await engine.request(method, path, body);
  return { status, json, seq };
}

/** The same request to both, held equal, and the answer. */
async function same(method, path, body = null, type) {
  const [a, b] = [await remote(method, path, body, type), await local(method, path, body)];
  assert.deepEqual(b, a, `${method} ${path} ${body ?? ''}`);
  return a;
}

const q = (text, params = []) => same('POST', '/query', JSON.stringify({ query: text, params }));
const batch = (...items) => same('POST', '/batch', items.map(([query, params = []]) => JSON.stringify({ query, params })).join('\n'), 'application/x-ndjson');

const skip = missing ? `needs ${missing}` : false;

test('statements: the same rows, counts, messages and refusals', { skip }, async () => {
  // A schema of every kind of field, index and option the views read.
  await q('create collection docs (title text required @text, lang text @hash, code text @unique, n int @sorted, at timestamp @ttl(30d), tags [text], meta json, name text collate tr, emb vector<4> @hnsw(cosine, quant=int8))');
  await q('create index on docs (meta.src) @hash');
  await q('create collection notes (doc int @hash, body text)');
  const rows = Array.from({ length: 30 }, (_, i) => ({
    title: `document ${i} about ${['deserts', 'foxes', 'vectors'][i % 3]}`,
    lang: ['en', 'tr', 'de'][i % 3],
    code: `D-${i}`,
    n: i,
    at: new Date(Date.UTC(2030, 0, 1) + i * 3_600_000).toISOString(),
    tags: ['a', 'b'].slice(0, (i % 2) + 1),
    meta: { src: i % 2 ? 'web' : 'app', items: [i, 0.5] },
    name: ['çay', 'ırmak', 'Istanbul', 'ilk'][i % 4],
    emb: [Math.cos(i), Math.sin(i), 0.5, i % 3],
  }));
  assert.deepEqual((await q('put docs $1', [rows])).json, { affected: 30 });
  await q('insert notes {doc: $1, body: $2}', [1, 'a note']);
  await q('insert notes {doc: $1, body: $2}', [1, 'another']);

  for (const [text, params] of [
    ['get docs limit 5', []],
    ['get docs select title, n, meta where n > $1 and lang = $2 order n desc limit 3', [10, 'tr']],
    ['get docs select title match title "foxes" limit 4', []],
    ['get docs select code near emb $1 limit 3', [[1, 0, 0.5, 1]]],
    ['get docs select code match title "deserts" near emb $1 fuse limit 3', [[1, 0, 0.5, 1]]],
    ['get docs where n < 20 limit 0 facet lang, tags top 3', []],
    ['get docs select name order name collate tr limit 4', []],
    ['get docs select lang, count(*) as c, avg(n) as mean group lang order c desc', []],
    ['get docs select bucket(at, 1d) as day, approx_count_distinct(lang) as langs group day having count(*) > 5', []],
    ['get docs count', []],
    ['get docs select code where id <= 2 lookup notes on doc select body', []],
    ['get docs where meta.src = "web" count', []],
    ['explain get docs where lang = "en" and n >= 3 order n limit 5', []],
    ['collections', []],
    ['describe docs', []],
  ]) {
    await q(text, params);
  }

  // Writes: the count, and the change they left the database at.
  const w = await q('set docs {n: n + 100} where lang = $1', ['de']);
  assert.ok(w.seq > 0 && w.json.affected === 10, JSON.stringify(w));
  await q('del notes where body = $1', ['another']);
  await q('alter collection notes add field stars int');
  await q('create index on notes (stars) @sorted');

  // Refusals: their status and their words.
  for (const [text, params, status] of [
    ['get nope limit 1', [], 404],
    ['get docs where', [], 400],
    ['insert docs {title: "x", code: $1}', ['D-1'], 409],
    ['set docs {n: 1} where code = "none" require 1', [], 412],
    ['put docs {n: "not a number"}', [], 400],
    ['create collection docs (a int)', [], 409],
  ]) {
    const r = await q(text, params);
    assert.equal(r.status, status, `${text}: ${JSON.stringify(r)}`);
  }
});

test('batches: each answer, and the statement that stops one', { skip }, async () => {
  await q('create collection b (k text @unique, v int)');
  const ok = await batch(['insert b {k: $1, v: $2}', ['a', 1]], ['insert b {k: $1, v: $2}', ['b', 2]], ['get b select k, v order k'], ['get b count']);
  assert.equal(ok.json.results.length, 4);
  // A read alone: under the read lock on a server, the same answer.
  await batch(['get b select k'], ['get b where v > $1 count', [1]]);
  // The second statement refused: nothing of the batch lands.
  const stopped = await batch(['insert b {k: $1, v: $2}', ['c', 3]], ['insert b {k: $1, v: $2}', ['a', 9]], ['get b count']);
  assert.equal(stopped.status, 409);
  assert.equal(stopped.json.at, 1);
  await batch(['set b {v: 0} where k = $1 require 1', ['zz']]);
  assert.deepEqual((await q('get b select k order k')).json, [{ k: 'a' }, { k: 'b' }]);
  // A statement that does not parse refuses the batch before it runs.
  assert.equal((await batch(['get b'], ['get b where'])).status, 400);
  // A compact runs each statement on its own: the ones before stay.
  const compacted = await batch(['insert b {k: $1}', ['d']], ['compact'], ['get nope']);
  assert.equal(compacted.json.completed, 2);
});

test('the schema: described, as FenecQL, and the plan of a change', { skip }, async () => {
  const described = (await same('GET', '/_schema')).json;
  assert.ok(described.collections.some((c) => c.name === 'docs'));
  assert.match((await same('GET', '/_schema?as=fenecql')).json.fenecql, /create collection docs \(title text required @text/);
  const docs = described.collections.find((c) => c.name === 'docs');
  const changed = structuredClone(docs);
  changed.fields.find((f) => f.name === 'lang').index = { kind: 'sorted' };
  changed.fields.push({ name: 'extra', type: 'float', index: { kind: 'sorted' } });
  await same('POST', '/_schema/plan', JSON.stringify({ format: 1, collections: [changed] }));
  await same('POST', '/_schema/plan', JSON.stringify({ format: 1, collections: [{ ...docs, fields: docs.fields.slice(1) }] }));
  await same('POST', '/_schema/plan', 'not json');
  // What only a server has is not faked: a 404, as an unknown path.
  assert.equal((await local('GET', '/_metrics')).status, 404);
});

test('a subscription: its seed, then each change, alike', { skip }, async () => {
  await q('create collection live (t text @hash, n int)');
  await q('put live [{t: "a", n: 1}, {t: "b", n: 2}, {t: "a", n: 3}]');
  const got = { remote: [], local: [] };
  const http = connect(server.url, { token });
  const stopRemote = http.subscribe('live', { where: 't = "a"' }, (ev) => got.remote.push(ev));
  engine.subscribe(1, 'live', { where: 't = "a"' }, (ev) => got.local.push(ev));
  const reached = async (seq) => {
    for (let i = 0; i < 500 && !got.remote.some((e) => e.seq >= seq); i++) await new Promise((r) => setTimeout(r, 10));
    // The page's live query looks after the task that wrote: a few ticks.
    await new Promise((r) => setTimeout(r, 30));
  };
  await reached(0);
  // Each write on its own, as the views send them, both streams caught up.
  for (const [text, params] of [
    ['insert live {t: $1, n: $2}', ['a', 4]],
    ['set live {n: 30} where n = 3', []],
    ['set live {t: "b"} where n = 1', []],
    ['insert live {t: $1, n: $2}', ['b', 5]],
    ['del live where n = 4', []],
    ['set live {t: "a"} where n = 2', []],
  ]) {
    await reached((await q(text, params)).seq);
  }
  stopRemote();
  engine.unsubscribe(1);
  // A server names as deleted a row written outside the shape that it
  // never sent (the insert of a "b"); a view passes over such an id, and
  // the page names only the rows its shape held.
  const held = new Set();
  const seen = (events) =>
    events.flatMap((e) => {
      if (e.type === 'seed') {
        e.rows.forEach((r) => held.add(r.id));
        return [e];
      }
      const dels = e.dels.filter((id) => held.has(id));
      dels.forEach((id) => held.delete(id));
      e.puts.forEach((r) => held.add(r.id));
      return e.puts.length || dels.length || e.schema ? [{ ...e, dels }] : [];
    });
  const remote = seen(got.remote);
  held.clear();
  assert.deepEqual(seen(got.local), remote);
  assert.equal(remote[0].type, 'seed');
  assert.deepEqual(remote[0].rows.map((r) => r.n), [1, 3]);
  assert.equal(remote.length, 6, JSON.stringify(remote));
});
