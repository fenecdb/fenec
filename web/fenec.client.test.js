// `@fenecdb/web/client` (web/client.js): the HTTP client and the builder
// without the engine. A bundler's output for a page that imports it is the
// modules it reaches, so those are held to the three of the client --
// none of them the module's glue, persistence, files or the sync layer --
// and its live queries to a server's subscription stream, scripted here
// (web/fenec.sync.test.js runs them against a real fenec-server).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import * as client from './client.js';
import * as fenec from './fenec.js';

/** Every module `entry` reaches through its static imports, by file name. */
async function graph(entry) {
  const seen = new Map();
  const visit = async (name) => {
    if (seen.has(name)) return;
    const text = await readFile(new URL(`./${name}`, import.meta.url), 'utf8');
    seen.set(name, text);
    for (const m of text.matchAll(/^(?:import|export)\b[^;]*?\bfrom\s+'\.\/([^']+)'/gm)) await visit(m[1]);
    for (const m of text.matchAll(/\bimport\s*\(\s*['"]\.\/([^'"]+)['"]/g)) await visit(m[1]);
  };
  await visit(entry);
  return seen;
}

test('the client reaches the builder and the HTTP client, and nothing of the engine', async () => {
  const modules = await graph('client.js');
  assert.deepEqual([...modules.keys()].sort(), ['builder.js', 'client.js', 'http.js']);
  for (const [name, text] of modules) {
    // Comments may name them; the code may not.
    const code = text.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');
    for (const word of ['WebAssembly', 'fenec_query', 'fenec_open', 'FenecSync', 'indexedDB', 'getDirectory', 'fenec.wasm', 'class Fenec ']) {
      assert.ok(!code.includes(word), `${name} holds ${word}`);
    }
  }
  // And fenec.js reaches the same two, so the classes are one.
  assert.deepEqual([...(await graph('fenec.js')).keys()].sort(), ['builder.js', 'fenec.js', 'http.js']);
});

test('the client exports what fenec.js does of it, the same objects', () => {
  assert.deepEqual(Object.keys(client).sort(), [
    'FenecError', 'FenecHttp', 'Query', 'and', 'bucket', 'connect', 'countDistinct', 'expr', 'first', 'from', 'inc', 'last', 'not', 'or', 'raw',
  ]);
  for (const [name, value] of Object.entries(client)) assert.equal(value, fenec[name], name);
  assert.equal(client.from('docs').where('year', 2024).toFenecQL()[0], 'get docs where year = $1');
});

/**
 * A server's subscription streams, scripted: `fetch` answers `/query` from
 * `rows()`, and each `GET /<c>/changes` with a stream `emit` writes events
 * into. A `this` other than the global object is refused, as a browser's
 * own `fetch` refuses it.
 */
function scripted(rows) {
  const streams = [];
  const queries = [];
  function fetch(url, init = {}) {
    if (this !== undefined && this !== globalThis) throw new TypeError('Illegal invocation');
    const u = new URL(url);
    if (init.method === 'POST') {
      queries.push(JSON.parse(init.body));
      return Promise.resolve(new Response(JSON.stringify(rows()), { status: 200 }));
    }
    let push;
    const body = new ReadableStream({
      start(c) {
        push = c;
      },
    });
    const s = {
      path: u.pathname,
      search: u.search,
      auth: init.headers?.authorization,
      emit: (name, data) => push.enqueue(new TextEncoder().encode(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`)),
      end: () => push.close(),
    };
    init.signal?.addEventListener('abort', () => {
      s.aborted = true;
      try {
        push.error(new DOMException('aborted', 'AbortError'));
      } catch {
        /* closed */
      }
    });
    streams.push(s);
    return Promise.resolve(new Response(body, { status: 200, headers: { 'content-type': 'text/event-stream' } }));
  }
  return { fetch, streams, queries };
}

const tick = () => new Promise((r) => setTimeout(r, 5));
const until = async (f) => {
  for (let i = 0; i < 200 && !f(); i++) await tick();
  assert.ok(f(), 'timed out');
};

test('a live query over HTTP runs once every stream is seeded, and again on a write', async () => {
  let n = 1;
  const server = scripted(() => [{ id: 1, n }]);
  const db = client.connect('http://db.test/', { token: 't', fetch: server.fetch });
  const seen = [];
  const q = db.from('docs').where('n', '>', 0).lookup('notes', { on: 'doc' });
  const stop = db.live(q, (rows) => seen.push(rows[0].n));
  await until(() => server.streams.length === 2);
  // A stream a collection the query reads, of a shape that holds nothing.
  assert.deepEqual(server.streams.map((s) => s.path).sort(), ['/docs/changes', '/notes/changes']);
  for (const s of server.streams) {
    assert.equal(s.search, '?select=id&where=false');
    assert.equal(s.auth, 'Bearer t');
  }
  // The first rows wait for both seeds.
  server.streams[0].emit('seed', { seq: 1, rows: [] });
  await tick();
  assert.deepEqual(seen, []);
  server.streams[1].emit('seed', { seq: 1, rows: [] });
  await until(() => seen.length === 1);
  // A write to either runs it again; a burst while it runs, once more.
  n = 2;
  server.streams[1].emit('change', { seq: 2, puts: [], dels: [7], schema: false });
  await until(() => seen.length === 2);
  n = 3;
  server.streams[0].emit('change', { seq: 3, puts: [], dels: [1], schema: false });
  server.streams[0].emit('change', { seq: 4, puts: [], dels: [2], schema: false });
  server.streams[1].emit('change', { seq: 5, puts: [], dels: [3], schema: false });
  await until(() => seen.at(-1) === 3);
  await tick();
  assert.ok(seen.length <= 4, `a burst ran it ${seen.length - 2} times`);
  assert.equal(server.queries[0].query, q.toFenecQL()[0]);
  stop();
  await until(() => server.streams.every((s) => s.aborted));
  const before = seen.length;
  n = 4;
  server.queries.length = 0;
  await tick();
  assert.equal(seen.length, before);
});

test('a stream that ends is opened again, and its seed runs the query', async () => {
  let n = 1;
  const server = scripted(() => [{ id: 1, n }]);
  const db = client.connect('http://db.test', { fetch: server.fetch });
  const seen = [];
  const errors = [];
  const stop = db.live('get docs', (rows) => seen.push(rows[0].n), { collections: ['docs'], onError: (e) => errors.push(e) });
  await until(() => server.streams.length === 1);
  server.streams[0].emit('seed', { seq: 1, rows: [] });
  await until(() => seen.length === 1);
  // The server says why it ends a stream: that goes to onError, and the
  // stream is opened again, its seed running the query once more.
  server.streams[0].emit('error', { error: 'the tenant was closed on this node' });
  await until(() => errors.length === 1 && server.streams.length === 2);
  assert.match(errors[0].message, /tenant was closed/);
  n = 2;
  server.streams[1].emit('seed', { seq: 9, rows: [] });
  await until(() => seen.at(-1) === 2);
  stop();
});

// The server ends a stream at its token's `exp` with a 401: the live query
// says so as an auth error and stops -- opened again with the same token
// it was refused at every attempt, each refusal waiting longer -- for the
// app to open it again with a fresh one.
test("a stream ended at its token's exp is an auth error, and is not opened again", async () => {
  const server = scripted(() => [{ id: 1 }]);
  const db = client.connect('http://db.test', { token: 'old', fetch: server.fetch });
  const errors = [];
  db.live('get docs', () => {}, { collections: ['docs'], onError: (e) => errors.push(e) });
  await until(() => server.streams.length === 1);
  server.streams[0].emit('seed', { seq: 1, rows: [] });
  server.streams[0].emit('error', { error: 'the token has expired', status: 401 });
  await until(() => errors.length === 1);
  assert.ok(errors[0] instanceof client.FenecError);
  assert.equal(errors[0].status, 401);
  assert.match(errors[0].message, /expired/);
  await new Promise((r) => setTimeout(r, 400));
  assert.equal(server.streams.length, 1, 'opened again with the token refused');
});

test('a text says what it reads, or a live query over HTTP is refused', () => {
  const db = client.connect('http://db.test', { fetch: scripted(() => []).fetch });
  assert.throws(() => db.live('get docs', () => {}), /name the collections a text reads/);
  assert.throws(() => db.live(client.from('docs').where(client.raw('id in (get other select id)')), () => {}), /name the collections/);
  assert.throws(() => db.live(db.from('docs'), null), /cb must be a function/);
});

test('a query from an endpoint carries it, for useLiveQuery to find', () => {
  const db = client.connect('http://db.test', { fetch: scripted(() => []).fetch });
  assert.equal(db.from('docs').context, db);
});
