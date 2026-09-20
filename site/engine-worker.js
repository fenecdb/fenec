/* The engine, off the main thread.
   fenecdb's HNSW build is synchronous by design — there is no yielding inside
   it — so running it on the main thread froze the page for as long as the
   build took. Here it holds a worker instead, and the page stays live: the
   scatter keeps drawing, the sky keeps moving, scrolling still works.
   Only rendering data crosses back, never the vectors themselves.

   Two callers share it: the console on the home page (`demo`) and the
   playground (`open`, `exec`, `seed`). One database per worker. */

import { Fenec } from './fenec.js';

let db = null;

const N = 5000;        // documents
const DIM = 128;       // dimensions
const CLUSTERS = 16;   // clustered, the way an embedding model outputs
const PROBES = 40;     // queries measured

const post = (t, d = {}) => self.postMessage({ t, ...d });

self.onmessage = async (e) => {
  const { cmd, id } = e.data || {};
  try {
    if (cmd === 'demo') return await demo();
    if (cmd === 'open') return await open(id);
    if (cmd === 'exec') return exec(id, e.data.sql, e.data.params);
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

function exec(id, sql, params) {
  if (!db) throw new Error('the database is not open yet');
  const t0 = performance.now();
  const res = db.run(sql, params || []);
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

/* ----------------------------------------------------------- the console */

async function demo() {
  post('log', { html: '<i>booting fenec.wasm</i> ' });
  const t0 = performance.now();
  db = await Fenec.open('./fenec.wasm');
  const boot = performance.now() - t0;
  post('amend', { html: `<b>ok</b> <i>in ${boot.toFixed(0)} ms</i>` });
  post('stat', { k: 'boot', v: boot.toFixed(0), unit: 'ms' });

  post('log', { html: '' });
  post('log', { html: '<em>create collection</em> notes (body text, topic int @hash,' });
  post('log', { html: '       embed <span style="color:#C79BF2">vector&lt;128&gt;</span>)' });
  db.run(`create collection notes (body text, topic int @hash, embed vector<${DIM}>)`);

  post('log', { html: '' });
  post('log', { html: `<i>generating ${N.toLocaleString()} vectors in ${CLUSTERS} clusters</i> ` });
  const rand = mulberry32(0x5eed);
  const centres = Array.from({ length: CLUSTERS }, () => unit(DIM, rand));
  const docs = new Array(N);
  for (let i = 0; i < N; i++) {
    const c = i % CLUSTERS;
    docs[i] = { body: `note ${i}`, topic: c, embed: jitter(centres[c], 0.55, rand) };
  }
  post('amend', { html: '<b>ok</b>' });

  // A real 2D shadow of the 128-dimensional data: two fixed random directions,
  // orthonormalised. Nothing is laid out by hand. Only the shadow crosses back.
  const project = makeProjection(DIM, mulberry32(0xd17e));
  const raw = docs.map((d) => project(d.embed));
  const box = bounds(raw);
  const xy = new Float32Array(N * 2);
  const cl = new Uint8Array(N);
  for (let i = 0; i < N; i++) {
    const p = place(raw[i], box);
    xy[i * 2] = p.x; xy[i * 2 + 1] = p.y; cl[i] = i % CLUSTERS;
  }
  post('points', { n: N, dim: DIM, xy, cl }, [xy.buffer, cl.buffer]);

  post('log', { html: '' });
  post('log', { html: '<em>put</em> notes [ … ] ' });
  post('phase', { name: 'scatter' });
  const t1 = performance.now();
  for (let i = 0; i < N; i += 1000) {
    await db.from('notes').insert(docs.slice(i, i + 1000));
    post('amend', { html: '.' });
  }
  const write = performance.now() - t1;
  post('amend', { html: ` <b>${N.toLocaleString()} rows</b> <i>in ${write.toFixed(0)} ms</i>` });
  post('stat', { k: 'rows', v: String(N), unit: ` × ${DIM}` });

  post('log', { html: '' });
  post('log', { html: '<em>create index on</em> notes (embed) <span style="color:#C79BF2">@hnsw</span>(cosine) ' });
  post('phase', { name: 'build' });
  const t2 = performance.now();
  db.run('create index on notes (embed) @hnsw(cosine)');
  const build = performance.now() - t2;
  post('amend', { html: `<b>ok</b> <i>in ${(build / 1000).toFixed(2)} s</i>` });
  post('stat', { k: 'build', v: (build / 1000).toFixed(2), unit: 's' });

  post('log', { html: '' });
  post('log', { html: '<em>get</em> notes <em>near</em> embed $1 <em>limit</em> 10' });

  const probes = Array.from({ length: PROBES }, () =>
    jitter(centres[Math.floor(rand() * CLUSTERS)], 0.55, rand));

  const times = [];
  let hit = 0, total = 0, last = null;
  for (let i = 0; i < PROBES; i++) {
    const t = performance.now();
    const ann = db.run('get notes select body near embed $1 limit 10', [probes[i]]);
    times.push(performance.now() - t);
    if (i % 5 === 0) {
      const exact = db.run('get notes select body near embed $1 exact limit 10', [probes[i]]);
      const truth = new Set(exact.rows.map((r) => r.body));
      hit += ann.rows.filter((r) => truth.has(r.body)).length;
      total += truth.size;
    }
    last = { probe: probes[i], rows: ann.rows };
  }
  times.sort((a, b) => a - b);
  const p50 = times[Math.floor(times.length / 2)];
  const recall = total ? (hit / total) * 100 : 0;
  post('amend', { html: `  <b>10 rows</b> <i>· p50 ${p50.toFixed(3)} ms over ${PROBES} queries</i>` });
  post('stat', { k: 'query', v: p50.toFixed(3), unit: 'ms' });
  post('stat', { k: 'recall', v: recall.toFixed(recall === 100 ? 0 : 1), unit: '%' });

  const q = place(project(last.probe), box, true);
  const idx = last.rows
    .map((r) => Number(String(r.body).replace('note ', '')))
    .filter((i) => Number.isInteger(i) && i >= 0 && i < N);
  post('query', { x: q.x, y: q.y, idx });

  post('log', { html: '' });
  post('log', { html: `<i>recall@10 against an exact scan: </i><b>${recall.toFixed(recall === 100 ? 0 : 1)}%</b>` });
  post('log', { html: '<i>nothing left this tab. no server was contacted.</i>' });
  post('done', { note: `${N.toLocaleString()} × ${DIM}, clustered` });
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

function makeProjection(dim, rand) {
  const a = unit(dim, rand);
  const b = unit(dim, rand);
  let dot = 0;
  for (let i = 0; i < dim; i++) dot += a[i] * b[i];
  let n = 0;
  for (let i = 0; i < dim; i++) { b[i] -= dot * a[i]; n += b[i] * b[i]; }
  n = Math.sqrt(n) || 1;
  for (let i = 0; i < dim; i++) b[i] /= n;
  return (v) => {
    let x = 0, y = 0;
    for (let i = 0; i < dim; i++) { x += v[i] * a[i]; y += v[i] * b[i]; }
    return { x, y };
  };
}

function bounds(raw) {
  let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
  for (const p of raw) {
    if (p.x < x0) x0 = p.x; if (p.x > x1) x1 = p.x;
    if (p.y < y0) y0 = p.y; if (p.y > y1) y1 = p.y;
  }
  return { x0, x1, y0, y1, sx: (x1 - x0) || 1, sy: (y1 - y0) || 1 };
}

function place(p, b, clamp) {
  let x = 0.06 + ((p.x - b.x0) / b.sx) * 0.88;
  let y = 0.08 + ((p.y - b.y0) / b.sy) * 0.8;
  if (clamp) {
    x = Math.min(Math.max(x, 0.02), 0.98);
    y = Math.min(Math.max(y, 0.02), 0.98);
  }
  return { x, y };
}
