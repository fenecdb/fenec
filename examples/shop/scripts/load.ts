// A short load test of the API routes: many shoppers at once, each route
// for LOAD_SECONDS (10) at LOAD_CLIENTS (16) concurrent clients, every
// request answered by the Next server with its data in fenec-server.
//
//   search     GET  /api/search?q=...        match, highlight, snippet, facets
//   category   GET  /api/c/<slug>?page=...   a page with four facets
//   add        POST /api/cart                a cart line under the shopper's token
//   checkout   POST /api/cart + /api/checkout   reserve, order and empty, one /batch
//
// It prints a Markdown table: requests a second, and the p50 and p99 of a
// request's time. The checkout row counts whole checkouts (an add and the
// order), and checks afterwards that no unit was sold twice.
import { pct, ms } from './stats';
import { CATEGORIES } from '../lib/catalog-data';

const SHOP = (process.env.SHOP_URL ?? 'http://localhost:3000').replace(/\/$/, '');
const FENEC = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const ROOT = process.env.FENEC_TOKEN ?? 'shop-dev-token';
const SECONDS = Number(process.env.LOAD_SECONDS ?? 10);
const CLIENTS = Number(process.env.LOAD_CLIENTS ?? 16);
const WORDS = ['tent', 'titanium', 'shemagh', 'merino', 'solar panel', 'filter', 'brim hat', 'down', 'headlamp', 'duffel', 'cooler', 'stove', 'linen shirt', 'boots', 'tarp'];

async function q(query: string, params: unknown[] = []) {
  const r = await fetch(`${FENEC}/query`, { method: 'POST', headers: { authorization: `Bearer ${ROOT}` }, body: JSON.stringify({ query, params }) });
  if (!r.ok) throw new Error(await r.text());
  return r.json();
}

class Jar {
  cookie = '';
  async send(path: string, body?: unknown) {
    const headers: Record<string, string> = { accept: 'application/json', origin: SHOP };
    if (this.cookie) headers.cookie = this.cookie;
    if (body) headers['content-type'] = 'application/json';
    const res = await fetch(SHOP + path, { method: body ? 'POST' : 'GET', headers, body: body ? JSON.stringify(body) : undefined });
    const set = res.headers.getSetCookie().map((c) => c.split(';')[0]);
    if (set.length) this.cookie = set.join('; ');
    await res.arrayBuffer();
    return res.status;
  }
}

async function run(name: string, once: (client: number, i: number) => Promise<boolean>) {
  const times: number[] = [];
  let failed = 0;
  const until = performance.now() + SECONDS * 1000;
  await Promise.all(
    Array.from({ length: CLIENTS }, async (_, c) => {
      for (let i = 0; performance.now() < until; i++) {
        const t = performance.now();
        const ok = await once(c, i);
        times.push(performance.now() - t);
        if (!ok) failed++;
      }
    }),
  );
  times.sort((a, b) => a - b);
  console.error(`${name}: ${times.length} in ${SECONDS} s`);
  return `| ${name} | ${(times.length / SECONDS).toFixed(0)}/s | ${ms(pct(times, 50))} ms | ${ms(pct(times, 99))} ms | ${failed} |`;
}

// Stock for the checkouts: ten skus with more than the run can sell.
const skus = ((await q('get products select sku where category = "packing-cubes" order id limit 10')) as { sku: string }[]).map((r) => r.sku);
for (const s of skus) await q('set inventory {available: 1000000, reserved: 0, sold: 0} where sku = $1', [s]);

const out: string[] = [];
out.push(await run('search', async (c, i) => (await new Jar().send(`/api/search?q=${encodeURIComponent(WORDS[(c + i) % WORDS.length])}`)) === 200));
out.push(
  await run('category', async (c, i) => {
    const cat = CATEGORIES[(c * 7 + i) % CATEGORIES.length].slug;
    return (await new Jar().send(`/api/c/${cat}?page=${1 + (i % 5)}`)) === 200;
  }),
);
const shoppers = Array.from({ length: CLIENTS }, () => new Jar());
// A line reaching the cap of 20 is taken out and started again.
out.push(
  await run('add to cart', async (c, i) => {
    const sku = skus[i % skus.length];
    const status = await shoppers[c].send('/api/cart', { action: 'add', sku, qty: 1 });
    if (status === 400) return (await shoppers[c].send('/api/cart', { action: 'remove', sku })) === 200;
    return status === 200;
  }),
);
let orders = 0;
out.push(
  await run('checkout (add, then order)', async (c, i) => {
    const s = new Jar();
    if ((await s.send('/api/cart', { action: 'add', sku: skus[(c + i) % skus.length], qty: 1 })) !== 200) return false;
    const ok = (await s.send('/api/checkout', { key: crypto.randomUUID().replace(/-/g, ''), email: 'load@example.com', name: 'Load Test', line1: '1 Erg Way', city: 'Merzouga', postcode: '52202', country: 'Morocco' })) === 200;
    if (ok) orders++;
    return ok;
  }),
);

// Nothing sold twice: each sku's units taken equal its order lines.
let mismatched = 0;
for (const s of skus) {
  const [inv] = (await q('get inventory select available, reserved, sold where sku = $1', [s])) as { available: number; reserved: number; sold: number }[];
  const lines = (await q('get order_lines select qty where sku = $1 limit 1000000', [s])) as { qty: number }[];
  const taken = lines.reduce((n, l) => n + l.qty, 0);
  if (inv.reserved + inv.sold !== taken || inv.available + taken !== 1000000) mismatched++;
}

console.log(`${CLIENTS} clients, ${SECONDS} s a route\n`);
console.log('| route | rate | p50 | p99 | failed |');
console.log('| --- | --- | --- | --- | --- |');
for (const r of out) console.log(r);
console.log(`\n${orders} orders placed; skus whose stock and order lines disagree: ${mismatched}`);
if (mismatched) process.exit(1);
