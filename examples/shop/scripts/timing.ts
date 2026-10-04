// How long the shop's server takes to answer each kind of page, with the
// data in fenec-server: one request at a time, from the first byte sent to
// the last received, on this machine (no network between). Each page kind
// is asked 200 times over different addresses -- 30 categories and their
// pages and filters, 40 searches, 200 products -- after 20 to warm up.
// A product page is measured twice: made (the first request for it) and
// kept (incremental static regeneration answering from its cache).
//
//   npm run timing            SHOP_URL, FENEC_URL as the tests read them
import { pct, ms, timed } from './stats';
import { CATEGORIES, COLOURS } from '../lib/catalog-data';

const SHOP = (process.env.SHOP_URL ?? 'http://localhost:3000').replace(/\/$/, '');
const N = Number(process.env.TIMING_N ?? 200);
const WORDS = ['tent', 'titanium stove', 'shemagh', 'merino socks', 'solar', 'water filter', 'wide brim hat', 'sand', 'down quilt',
  'headlamp red light', 'canvas duffel', 'polarised', 'cooler', 'trekking pole tent', 'insulated bottle', 'first aid', 'knife',
  'camp chair', 'lantern', 'power bank', 'sun shirt', 'linen', 'boots suede', 'sandals', 'gaiters', 'tarp', 'pad', 'pack 30 l',
  'hydration vest', 'cubes', 'kettle', 'compass', 'gps', 'rain jacket', 'windproof', 'trousers', 'zip-off', 'olive', 'indigo', 'saffron'];

async function slugs(n: number): Promise<string[]> {
  const out: string[] = [];
  for (let page = 1; out.length < n; page++) {
    const c = CATEGORIES[page % CATEGORIES.length].slug;
    const r = (await (await fetch(`${SHOP}/api/c/${c}?sort=newest&page=${1 + Math.floor(page / CATEGORIES.length)}`)).json()) as { cards: { slug: string }[] };
    out.push(...r.cards.map((x) => x.slug));
  }
  return out.slice(0, n);
}

const kinds: Record<string, (i: number) => string> = {
  home: () => '/',
  category: (i) => {
    const c = CATEGORIES[i % CATEGORIES.length].slug;
    const page = 1 + (i % 4);
    return i % 3 === 0 ? `/c/${c}?colour=${encodeURIComponent(COLOURS[i % COLOURS.length])}&sort=price-asc` : `/c/${c}?page=${page}`;
  },
  search: (i) => `/search?q=${encodeURIComponent(WORDS[i % WORDS.length])}${i % 2 ? '&page=2' : ''}`,
};

const rows: string[] = [];
async function measure(name: string, urlOf: (i: number) => string, warm = 20) {
  for (let i = 0; i < warm; i++) await timed(SHOP + urlOf(i + 1000));
  const t: number[] = [];
  let bytes = 0;
  for (let i = 0; i < N; i++) {
    const r = await timed(SHOP + urlOf(i));
    if (r.status !== 200) throw new Error(`${urlOf(i)}: ${r.status}`);
    t.push(r.ms);
    bytes += r.bytes;
  }
  t.sort((a, b) => a - b);
  rows.push(`| ${name} | ${ms(pct(t, 50))} ms | ${ms(pct(t, 95))} ms | ${ms(pct(t, 99))} ms | ${(bytes / N / 1024).toFixed(1)} KB |`);
  console.error(`${name}: done`);
}

const products = await slugs(N + 20);
await measure('home (kept, regenerated every 5 min)', kinds.home);
await measure('category: a page, a filter, an order', kinds.category);
await measure('search: match, highlight, snippet, facets', kinds.search);
// Made: products no one asked for since the build.
const fresh = products.slice(0, N);
let k = 0;
await measure('product, made on its first request', () => `/p/${fresh[k++ % fresh.length]}`, 0);
await measure('product, kept (ISR)', (i) => `/p/${fresh[i % fresh.length]}`, 0);

console.log('| page | p50 | p95 | p99 | HTML |');
console.log('| --- | --- | --- | --- | --- |');
for (const r of rows) console.log(r);
