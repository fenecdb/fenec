/* The engine, off the main thread.
   fenecdb's HNSW build is synchronous by design — there is no yielding inside
   it — so running it on the main thread froze the page for as long as the
   build took. Here it holds a worker instead, and the page stays live:
   the sky keeps moving, scrolling still works. Only the answers cross back.

   The playground calls it (`open`, `exec`, `seed`). One database per
   worker. */

import { Fenec } from './fenec.js';

let db = null;

const post = (t, d = {}) => self.postMessage({ t, ...d });

self.onmessage = async (e) => {
  const { cmd, id } = e.data || {};
  try {
    if (cmd === 'open') return await open(id);
    if (cmd === 'exec') return await exec(id, e.data.sql, e.data.params);
    if (cmd === 'seed') return await seed(id, e.data.n, e.data.dim);
    if (cmd === 'schema') return schema(id);
  } catch (err) {
    const message = String(err && err.message ? err.message : err);
    if (id != null) post('result', { id, error: message });
    else post('failed', { message });
  }
};

/* ------------------------------------------------------------ playground */

async function open(id) {
  const t0 = performance.now();
  db = await Fenec.open('./fenec.wasm');
  post('result', { id, ok: true, ms: performance.now() - t0 });
}

async function exec(id, sql, params) {
  if (!db) throw new Error('the database is not open yet');
  const t0 = performance.now();
  // `query` rather than `run`: a statement over text of a script whose
  // collation data the module has not got fetches it and runs again.
  const res = await db.query(sql, params || []);
  const ms = performance.now() - t0;
  // A read comes back as {columns, rows}; `collections` and `describe` as
  // {kind:'schemas', collections}; a write as {kind:'affected', count}.
  post('result', {
    id, ms,
    kind: res.kind ?? 'rows',
    rows: res.rows ?? null,
    columns: res.columns ?? null,
    collections: res.collections ?? null,
    count: res.count ?? null,
  });
  schema();
}

function schema(id) {
  if (!db) return;
  let list = [];
  try {
    list = db.run('collections').collections ?? [];
  } catch { /* an empty file has none yet */ }
  post(id != null ? 'result' : 'schema', { id, schema: list });
}

/* Sample data so the playground has something to query on arrival. */
async function seed(id, n = 400, dim = 64) {
  if (!db) throw new Error('the database is not open yet');
  const rand = mulberry32(0xbeef);
  const CL = 8;
  const centres = Array.from({ length: CL }, () => unit(dim, rand));
  const topics = ['storage', 'vectors', 'protocol', 'wasm', 'indexing', 'sync', 'codec', 'limits'];
  const t0 = performance.now();
  db.run(`create collection if not exists notes (
            body text, topic text @hash, year int, embed vector<${dim}>)`);
  for (let i = 0; i < n; i += 200) {
    const batch = [];
    for (let k = i; k < Math.min(i + 200, n); k++) {
      const c = k % CL;
      batch.push({
        body: `${topics[c]} note ${k}`,
        topic: topics[c],
        year: 2021 + (k % 5),
        embed: jitter(centres[c], 0.55, rand),
      });
    }
    await db.from('notes').insert(batch);
  }
  db.run('create index on notes (embed) @hnsw(cosine)');
  post('result', { id, ok: true, n, dim, ms: performance.now() - t0 });
  schema();
}

/* ------------------------------------------------------------------ maths */

function mulberry32(a) {
  return function () {
    a |= 0; a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function gauss(rand) {
  const u = Math.max(rand(), 1e-9), v = rand();
  return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v);
}

function unit(dim, rand) {
  const v = new Array(dim);
  let n = 0;
  for (let i = 0; i < dim; i++) { const g = gauss(rand); v[i] = g; n += g * g; }
  n = Math.sqrt(n) || 1;
  for (let i = 0; i < dim; i++) v[i] /= n;
  return v;
}

function jitter(centre, spread, rand) {
  const d = centre.length, v = new Array(d);
  let n = 0;
  for (let i = 0; i < d; i++) {
    const g = centre[i] + gauss(rand) * spread / Math.sqrt(d);
    v[i] = g; n += g * g;
  }
  n = Math.sqrt(n) || 1;
  for (let i = 0; i < d; i++) v[i] /= n;
  return v;
}
