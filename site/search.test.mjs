// The site's search finds what a reader would look for, first.
//
//   node --test site/search.test.mjs
//
// `build.py` runs it on the image it has just written (FENEC_SEARCH_INDEX)
// and fails the build when a query does not put its page first; run alone,
// it takes the image site/dist serves, gzipped. The queries go through
// `search`, the function the dialog calls, over the image the page loads.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { gunzipSync } from 'node:zlib';
import { Fenec } from '../web/fenec.js';
import { search, grouped } from './search-query.js';
import { asker } from './search-meaning.js';
import { fixtureVector } from './search-fixture.mjs';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
/** An image: the one build.py names, or the one site/dist serves. */
const imageOf = (env, stem) => (process.env[env] ? readFileSync(process.env[env]) : (() => {
  const name = readdirSync(here('./dist/')).find((n) => new RegExp(`^${stem}\\.[0-9a-f]+\\.fenec\\.gz$`).test(n));
  if (!name) throw new Error('no search index in site/dist: run python3 site/build.py');
  return gunzipSync(readFileSync(here(`./dist/${name}`)));
})());

const module = readFileSync(process.env.FENEC_WASM ?? here('../web/fenec.wasm'));
const db = await Fenec.open(module);
db.load(imageOf('FENEC_SEARCH_INDEX', 'search'));
// The image the page loads once the search by meaning answers: the same
// sections, each with its vector.
const dbv = await Fenec.open(module);
dbv.load(imageOf('FENEC_SEARCH_VECTORS', 'search-vectors'));

const first = (q, section) => search(db, q, section).hits[0]?.url;

test('the index holds every page', () => {
  const n = db.run('get docs count').rows[0].count;
  assert.ok(n > 100, `only ${n} sections`);
  const pages = new Set(db.run('get docs select url limit 10000').rows.map((r) => r.url.split('#')[0]));
  for (const p of ['', 'playground', 'docs/', 'docs/fenecql', 'docs/server', 'docs/redis', 'docs/benchmarks', 'docs/vs-sqlite']) {
    assert.ok(pages.has(p), `no section of ${JSON.stringify(p)}`);
  }
});

test('a known query finds its page first', () => {
  assert.equal(first('compact'), 'docs/server#compaction');
  assert.match(first('insert if absent'), /^docs\/(redis|fenecql)(#|$)/);
  assert.equal(first('facet'), 'docs/fenecql#facets');
  assert.equal(first('highlight snippet'), 'docs/fenecql#highlights');
  assert.match(first('replica failover'), /^docs\/(replication|sharding)(#|$)/);
});

test('a word being typed finds the words it begins', () => {
  assert.equal(first('compa'), 'docs/server#compaction');
});

test('Turkish and other scripts: İ and ı fold, and a run of Han is searched', () => {
  // A Turkish keyboard capitalises i as İ and writes ı for a dotless i.
  assert.equal(first('COMPACTİON'), 'docs/server#compaction');
  assert.equal(first('compactıon'), 'docs/server#compaction');
  assert.ok(search(db, '東京').hits.length > 0, 'no section holds 東京');
  assert.ok(search(db, 'kitapların').hits.length > 0, 'no section holds kitapların');
});

test('a section narrows the list, and the counts stay those of every group', () => {
  const all = search(db, 'replica');
  const operate = search(db, 'replica', 'Operate');
  assert.ok(operate.hits.length > 0);
  assert.ok(operate.hits.every((h) => h.section === 'Operate'));
  assert.deepEqual(operate.facets, all.facets);
  const counted = all.facets.find((f) => f.value === 'Operate').count;
  assert.ok(counted >= operate.hits.length);
});

test('marks fall on the words, and a snippet is text', () => {
  const [h] = search(db, 'compaction').hits;
  assert.deepEqual(h.headingMarks.map(([a, b]) => h.heading.slice(a, b).toLowerCase()), ['compaction']);
  assert.ok(h.snippet.marks.length > 0);
  for (const [a, b] of h.snippet.marks) assert.match(h.snippet.text.slice(a, b), /^compact/i);
});

test('nothing to match is no results, not an error', () => {
  for (const q of ['', '   ', '?!', '"', '$1', 'get docs; drop collection docs']) {
    assert.doesNotThrow(() => search(db, q));
  }
  assert.ok(db.run('get docs count').rows[0].count > 0);
});

test('hits are grouped by page, the best page first', () => {
  const { hits } = search(db, 'replication');
  const groups = grouped(hits);
  assert.equal(groups.flatMap((g) => g.hits).length, hits.length);
  assert.equal(groups[0].hits[0], hits[0]);
  assert.ok(hits.length <= 10);
});

/* ---------------------------------------------------------- by meaning */

/** Where the first hit for `url` stands, from 1, or 0: words, then meaning. */
function standing(q, url) {
  const at = (hits) => hits.findIndex((h) => h.url === url || h.url.startsWith(url + '#')) + 1;
  const v = fixtureVector(q);
  assert.ok(v, `the fixture holds no vector for ${JSON.stringify(q)}`);
  return [at(search(db, q).hits), at(search(dbv, q, null, 10, v).hits)];
}

test('the vector image holds every section, nearly all with a vector', () => {
  // A section new since the cache was committed, built where nothing could
  // embed it, has none, and is found by its words; most have one.
  const n = db.run('get docs count').rows[0].count;
  assert.equal(dbv.run('get docs count').rows[0].count, n);
  assert.ok(dbv.run('get docs where embed is null count').rows[0].count <= n / 10);
});

test('a question the words miss finds its page by its meaning', () => {
  // Each: the words leave the page off the ten shown, the meaning puts it there.
  for (const [q, url, within] of [
    ['how do I stop my file from growing', 'docs/server#compaction', 10],
    ['keep data on a phone', 'docs/mobile', 1],
    ['telefonda veri saklamak', 'docs/mobile', 3],
    ['dosyanın büyümesini durdurmak', 'docs/server#compaction', 3],
  ]) {
    const [words, meaning] = standing(q, url);
    assert.ok(words === 0, `${q}: the words alone found ${url} at ${words}`);
    assert.ok(meaning >= 1 && meaning <= within, `${q}: ${url} at ${meaning} by meaning, within ${within} wanted`);
  }
});

test('what the words found, the meaning keeps', () => {
  for (const [q, url, within] of [
    ['count things atomically', 'docs/redis', 5],
    ['what happens to writes when the primary dies', 'docs/replication', 3],
    ['stop a retried request from writing twice', 'docs/http#idempotency', 1],
    ['run it in docker', 'docs/server#containers', 1],
    ['sort names the way Turkish does', 'docs/fenecql#collation', 1],
    ['work offline and catch up later', 'docs/sync', 1],
  ]) {
    const [, meaning] = standing(q, url);
    assert.ok(meaning >= 1 && meaning <= within, `${q}: ${url} at ${meaning} by meaning, within ${within} wanted`);
  }
});

test('a section found by meaning alone is shown with its opening, nothing marked', () => {
  // Words that match nothing, and a phone's question's vector.
  const { hits } = search(dbv, 'qqzxv', null, 10, fixtureVector('keep data on a phone'));
  assert.ok(hits.length > 0);
  assert.equal(hits[0].page, 'docs/mobile');
  for (const h of hits) {
    assert.ok(h.byMeaning);
    assert.deepEqual(h.snippet.marks, []);
    assert.ok(h.snippet.text.length > 0);
  }
});

test('the endpoint absent, refusing, slow or wrong: the words alone, and asked no more than it should be', async () => {
  const answer = (status, body, headers = {}) => async () => new Response(JSON.stringify(body), { status, headers });
  const Q = 'how do I stop my file from growing';
  const v = fixtureVector(Q);
  // A word or two is looked up by the words, and the endpoint not asked.
  let calls = 0;
  let ask = asker({ fetch: async () => { calls++; return answer(200, { vector: v })(); } });
  assert.equal(await ask('compact'), null);
  assert.equal(await ask('replica failover'), null);
  assert.equal(calls, 0);
  // Absent (a local server, or off): null, and never asked again.
  ask = asker({ fetch: async (...a) => { calls++; return answer(404, { error: 'off' })(...a); } });
  assert.equal(await ask(Q), null);
  assert.equal(await ask('keep data on a phone'), null);
  assert.equal(calls, 1);
  // A budget spent: null, and not asked again until the time it gave.
  let now = 0;
  calls = 0;
  ask = asker({ now: () => now, fetch: async (...a) => { calls++; return answer(429, { error: 'budget' }, { 'retry-after': '120' })(...a); } });
  assert.equal(await ask(Q), null);
  assert.equal(await ask('keep data on a phone'), null);
  now = 121_000;
  assert.equal(await ask('keep data on a phone'), null);
  assert.equal(calls, 2);
  // Slow: given up at the timeout, null.
  ask = asker({ timeout: 50, fetch: (_, init) => new Promise((_, no) => init.signal.addEventListener('abort', () => no(init.signal.reason))) });
  const t = Date.now();
  assert.equal(await ask(Q), null);
  assert.ok(Date.now() - t < 1000);
  // No network, a 503, a vector of the wrong width: null.
  for (const f of [async () => { throw new TypeError('offline'); }, answer(503, {}), answer(200, { vector: [1, 2, 3] }), answer(200, { nope: 1 })]) {
    assert.equal(await asker({ fetch: f })(Q), null);
  }
  // A vector, asked once for one spelling of the query.
  calls = 0;
  ask = asker({ fetch: async (...a) => { calls++; return answer(200, { vector: v })(...a); } });
  assert.deepEqual(await ask('How do I  stop my file from growing '), v);
  assert.deepEqual(await ask(Q), v);
  assert.equal(calls, 1);
  // With no vector the search is the words' alone, hit for hit.
  assert.deepEqual(search(dbv, 'compact', null, 10, null).hits.map((h) => h.url), search(db, 'compact').hits.map((h) => h.url));
});
