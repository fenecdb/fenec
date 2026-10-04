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

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
const image = process.env.FENEC_SEARCH_INDEX ? readFileSync(process.env.FENEC_SEARCH_INDEX) : (() => {
  const name = readdirSync(here('./dist/')).find((n) => /^search\.[0-9a-f]+\.fenec\.gz$/.test(n));
  if (!name) throw new Error('no search index in site/dist: run python3 site/build.py');
  return gunzipSync(readFileSync(here(`./dist/${name}`)));
})();

const db = await Fenec.open(readFileSync(process.env.FENEC_WASM ?? here('../web/fenec.wasm')));
db.load(image);

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
