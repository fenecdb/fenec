// SEO: what a crawler that runs no JavaScript gets -- titles, canonicals,
// Open Graph, JSON-LD that parses and has what Google asks for, sitemaps
// that list every product, robots.txt -- all in the HTML the server sends.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SHOP, rows } from './helpers';
import { CATEGORIES } from '../lib/catalog-data';
import { CATEGORY_SHAPES } from '../lib/category-shapes';

const html = async (path: string) => {
  const r = await fetch(SHOP + path);
  assert.equal(r.status, 200, path);
  return r.text();
};
const ld = (page: string) => [...page.matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)].map((m) => JSON.parse(m[1]));
const canonical = (page: string) => /<link rel="canonical" href="([^"]+)"/.exec(page)?.[1];
const meta = (page: string, name: string) => new RegExp(`<meta (?:name|property)="${name}" content="([^"]*)"`).exec(page)?.[1];

test('a product page: title, description, canonical, Open Graph, and Product with Offer and rating', async () => {
  const [p] = await rows<{ slug: string; name: string; price: number; sku: string; reviews: number }>(
    'get products select slug, name, price, sku, reviews where reviews > 10 order id limit 1',
  );
  const page = await html(`/p/${p.slug}`);
  assert.match(page, new RegExp(`<title>${p.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')} \\| Sandgrouse</title>`));
  assert.ok((meta(page, 'description') ?? '').length > 50);
  assert.equal(canonical(page), `${SHOP}/p/${p.slug}`);
  assert.equal(meta(page, 'og:title'), p.name);
  assert.ok(meta(page, 'og:image')?.endsWith(`/img/p/${p.sku}.svg`));
  const blocks = ld(page);
  const product = blocks.find((b) => b['@type'] === 'Product');
  assert.ok(product, 'a Product block');
  assert.equal(product.name, p.name);
  assert.equal(product.sku, p.sku);
  assert.ok(product.image && product.description && product.brand?.name);
  assert.equal(product.offers['@type'], 'Offer');
  assert.equal(product.offers.price, (p.price / 100).toFixed(2));
  assert.equal(product.offers.priceCurrency, 'USD');
  assert.match(product.offers.availability, /^https:\/\/schema\.org\/(InStock|OutOfStock)$/);
  assert.equal(product.aggregateRating['@type'], 'AggregateRating');
  assert.equal(product.aggregateRating.reviewCount, p.reviews);
  assert.ok(product.aggregateRating.ratingValue >= 1 && product.aggregateRating.ratingValue <= 5);
  const crumbs = blocks.find((b) => b['@type'] === 'BreadcrumbList');
  assert.equal(crumbs.itemListElement.length, 3);
  assert.equal(crumbs.itemListElement[2].item, `${SHOP}/p/${p.slug}`);
  // The content is in the HTML, not added by a script.
  assert.ok(page.includes(`>${p.sku}<`));
  assert.ok(page.includes('Add to cart') || page.includes('Sold out'));
  assert.match(page, /<h1>/);
  // Its picture, as a file, for the JSON-LD and Open Graph.
  const img = await fetch(`${SHOP}/img/p/${p.sku}.svg`);
  assert.equal(img.headers.get('content-type'), 'image/svg+xml');
});

test('a product with no reviews has no aggregate rating', async () => {
  const [p] = await rows<{ slug: string }>('get products select slug where reviews = 0 order id limit 1');
  const product = ld(await html(`/p/${p.slug}`)).find((b) => b['@type'] === 'Product');
  assert.equal(product.aggregateRating, undefined);
});

test('a category page: ItemList of its page, canonical by page, filtered views kept out of the index', async () => {
  const page = await html('/c/tents');
  assert.equal(canonical(page), `${SHOP}/c/tents`);
  const list = ld(page).find((b) => b['@type'] === 'ItemList');
  assert.equal(list.itemListElement.length, 24);
  assert.equal(list.itemListElement[0].position, 1);
  assert.ok(list.itemListElement.every((i: { url: string }) => i.url.startsWith(`${SHOP}/p/`)));
  assert.equal(ld(page).find((b) => b['@type'] === 'BreadcrumbList').itemListElement.length, 3);
  assert.ok(!/<meta name="robots"/.test(page));
  // The next page is a link a crawler follows.
  assert.match(page, /<a href="\/c\/tents\?page=2" rel="next">/);
  const two = await html('/c/tents?page=2');
  assert.equal(canonical(two), `${SHOP}/c/tents?page=2`);
  assert.equal(ld(two).find((b) => b['@type'] === 'ItemList').itemListElement[0].position, 25);
  const filtered = await html('/c/tents?sort=price-asc');
  assert.match(filtered, /<meta name="robots" content="noindex, follow"/);
  assert.equal(canonical(filtered), `${SHOP}/c/tents`);
  assert.equal((await fetch(`${SHOP}/c/no-such-thing`)).status, 404);
});

test('the sitemaps list every product and every category; robots.txt names them', async () => {
  const index = await html('/sitemap.xml');
  const files = [...index.matchAll(/<loc>([^<]+)<\/loc>/g)].map((m) => m[1]);
  assert.ok(files.length >= 3);
  const urls = new Set<string>();
  for (const f of files) {
    const body = await (await fetch(f.replace(/^https?:\/\/[^/]+/, SHOP))).text();
    for (const m of body.matchAll(/<loc>([^<]+)<\/loc>/g)) urls.add(m[1]);
  }
  const all = await rows<{ slug: string }>('get products select slug limit 100000');
  assert.ok(all.length >= 1000);
  for (const p of all) assert.ok(urls.has(`${SHOP}/p/${p.slug}`), `missing ${p.slug}`);
  for (const c of CATEGORIES) assert.ok(urls.has(`${SHOP}/c/${c.slug}`), `missing ${c.slug}`);
  const robots = await html('/robots.txt');
  assert.match(robots, new RegExp(`Sitemap: ${SHOP}/sitemap.xml`));
  assert.match(robots, /Disallow: \/api\//);
});

test('the home page and search have titles and canonicals; search stays out of the index', async () => {
  const home = await html('/');
  assert.match(home, /<title>Sandgrouse: desert and travel gear<\/title>/);
  assert.ok([SHOP, `${SHOP}/`].includes(canonical(home)!));
  assert.equal(ld(home)[0]['@type'], 'WebSite');
  const search = await html('/search?q=tent');
  assert.match(search, /<meta name="robots" content="noindex, follow"/);
});

test('every category draws with the silhouette the catalog names', () => {
  for (const c of CATEGORIES) assert.equal(CATEGORY_SHAPES[c.slug], c.shape, c.slug);
  assert.equal(Object.keys(CATEGORY_SHAPES).length, CATEGORIES.length);
});
