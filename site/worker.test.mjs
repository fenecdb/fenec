// The embedding endpoint's refusals and limits (site/worker.js), with its
// bindings faked: the model, the rate limit, the budget's Durable Object
// and the cache. No network and no Cloudflare account.
//
//   node --test site/worker.test.mjs

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { handle, Budget, dailyTokens, tokensOf } from './worker.js';
import { EMBED_DIM, MAX_QUERY } from './search-vectors.js';

const SITE = 'https://fenecdb.com';

/** A fresh set of bindings; `calls` counts what reached the model. */
function bindings(over = {}) {
  const calls = [];
  const storage = new Map();
  const budget = new Budget({ storage: {
    get: async (k) => storage.get(k),
    put: async (o) => { for (const [k, v] of Object.entries(o)) storage.set(k, v); },
  } }, { DAILY_NEURONS: over.DAILY_NEURONS ?? '5000' });
  const hits = new Map();
  let allowed = over.perIp ?? Infinity;
  const env = {
    SEMANTIC: 'on',
    AI_GATEWAY: 'fenecdb-site',
    AI: {
      run: async (model, input, opts) => {
        calls.push({ model, input, opts });
        if (over.ai) return over.ai(model, input, opts);
        return { data: [Array.from({ length: EMBED_DIM }, (_, i) => Math.sin(i + input.text[0].length))] };
      },
    },
    PER_IP: { limit: async () => ({ success: allowed-- > 0 }) },
    BUDGET: { idFromName: () => 'id', get: () => ({ fetch: (u, init) => budget.fetch(new Request(u, init)) }) },
    ...over.env,
  };
  const cache = {
    match: async (r) => hits.get(r.url)?.clone(),
    put: async (r, res) => { hits.set(r.url, res); },
  };
  const ctx = { waitUntil: (p) => p };
  return { env, cache, ctx, calls };
}

function ask(body, headers = {}) {
  return new Request(`${SITE}/api/embed`, {
    method: 'POST',
    headers: { origin: SITE, referer: `${SITE}/docs/server`, 'sec-fetch-site': 'same-origin', 'content-type': 'application/json', ...headers },
    body: typeof body === 'string' ? body : JSON.stringify(body),
  });
}

const run = async (b, req) => {
  const res = await handle(req, b.env, b.ctx, b.cache);
  return { status: res.status, body: await res.json(), headers: res.headers };
};

test('a query comes back as one vector of the model, and nothing else', async () => {
  const b = bindings();
  const r = await run(b, ask({ q: 'how do I stop my file from growing' }));
  assert.equal(r.status, 200);
  assert.deepEqual(Object.keys(r.body), ['vector']);
  assert.equal(r.body.vector.length, EMBED_DIM);
  assert.equal(b.calls.length, 1);
  assert.equal(b.calls[0].model, '@cf/baai/bge-m3');
  assert.equal(b.calls[0].opts.gateway.id, 'fenecdb-site');
  assert.equal(r.headers.get('access-control-allow-origin'), null);
});

test('the same words, however spelt, are one cache entry and one call', async () => {
  const b = bindings();
  await run(b, ask({ q: 'Keep data on a phone' }));
  const again = await run(b, ask({ q: '  keep   DATA on a phone ' }));
  assert.equal(again.status, 200);
  assert.equal(b.calls.length, 1);
  assert.deepEqual(b.calls[0].input, { text: ['keep data on a phone'] });
});

test('off until the owner turns it on', async () => {
  const b = bindings({ env: { SEMANTIC: 'off' } });
  assert.equal((await run(b, ask({ q: 'x' }))).status, 404);
  assert.equal(b.calls.length, 0);
});

test('another site, a form or a cross-site fetch is refused', async () => {
  const b = bindings();
  for (const h of [
    { origin: 'https://evil.example' },
    { origin: null },
    { referer: 'https://evil.example/' },
    { 'sec-fetch-site': 'cross-site' },
    { 'content-type': 'text/plain' },
  ]) {
    const headers = Object.fromEntries(Object.entries(h).filter(([, v]) => v !== null));
    const req = ask({ q: 'compact' }, headers);
    if (h.origin === null) req.headers.delete('origin');
    const r = await run(b, req);
    assert.ok([403, 415].includes(r.status), `${JSON.stringify(h)} answered ${r.status}`);
  }
  const get = await run(b, new Request(`${SITE}/api/embed`, { headers: { origin: SITE } }));
  assert.equal(get.status, 405);
  assert.equal(b.calls.length, 0);
});

test('only {q}, a string of at most MAX_QUERY characters', async () => {
  const b = bindings();
  for (const body of ['nope', '[]', '{}', { q: 3 }, { q: 'a', model: '@cf/meta/llama-3.3-70b' }, { q: '   ' }]) {
    assert.equal((await run(b, ask(body))).status, 400, JSON.stringify(body));
  }
  assert.equal((await run(b, ask({ q: 'x'.repeat(MAX_QUERY + 1) }))).status, 413);
  assert.equal((await run(b, ask('{"q":"' + 'x'.repeat(2000) + '"}'))).status, 413);
  assert.equal((await run(b, ask({ q: 'ğ'.repeat(MAX_QUERY) }))).status, 200);
  assert.equal(b.calls.length, 1);
});

test('an address past its limit is told to wait; a cached query still answers', async () => {
  const b = bindings({ perIp: 2 });
  assert.equal((await run(b, ask({ q: 'one' }))).status, 200);
  assert.equal((await run(b, ask({ q: 'two' }))).status, 200);
  const third = await run(b, ask({ q: 'three' }));
  assert.equal(third.status, 429);
  assert.equal(third.headers.get('retry-after'), '60');
  assert.equal((await run(b, ask({ q: 'one' }))).status, 200);
  assert.equal(b.calls.length, 2);
});

test('the day\'s budget, once spent, is a 429 until midnight', async () => {
  // A cap of 0.05 neurons is 46 tokens: two queries of 20 characters.
  const b = bindings({ DAILY_NEURONS: '0.05' });
  assert.equal(dailyTokens({ DAILY_NEURONS: '0.05' }), 46);
  assert.equal(tokensOf('x'.repeat(40)), 22);
  assert.equal((await run(b, ask({ q: 'a'.repeat(40) }))).status, 200);
  assert.equal((await run(b, ask({ q: 'b'.repeat(40) }))).status, 200);
  const over = await run(b, ask({ q: 'c'.repeat(40) }));
  assert.equal(over.status, 429);
  assert.equal(over.body.error, 'budget');
  assert.ok(Number(over.headers.get('retry-after')) <= 86400);
  assert.equal(b.calls.length, 2);
});

test('a model that fails, is slow or answers nonsense is a 503, never a vector', async () => {
  for (const ai of [
    () => { throw new Error('down'); },
    () => ({ data: [[1, 2, 3]] }),
    () => ({ data: [Array(EMBED_DIM).fill(NaN)] }),
    () => new Promise(() => {}),
  ]) {
    const b = bindings({ ai });
    const r = await run(b, ask({ q: 'compact' }));
    assert.equal(r.status, 503);
  }
});

test('nothing but /api/embed runs here', async () => {
  const b = bindings({ env: { ASSETS: { fetch: async () => new Response('asset') } } });
  const res = await handle(new Request(`${SITE}/api/other`), b.env, b.ctx, b.cache);
  assert.equal(await res.text(), 'asset');
});
