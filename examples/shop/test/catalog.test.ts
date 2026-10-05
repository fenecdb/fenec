// The category page's filters: a brand chosen still lists the other brands
// (`facet brand disjunctive`), and a price band is a range of the price
// (`facet price ranges`), each held to what fenec-server counts itself.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SHOP, rows } from './helpers';

async function html(path: string) {
  const res = await fetch(`${SHOP}${path}`);
  assert.equal(res.status, 200, path);
  return res.text();
}

/** A facet section's entries: each label and count, as the page draws them. */
function facet(page: string, label: string): { label: string; count: number }[] {
  const at = page.indexOf(`aria-label="${label}"`);
  assert.ok(at >= 0, `no ${label} facet`);
  const section = page.slice(at, page.indexOf('</section>', at));
  return [...section.matchAll(/<span>([^<]*)<\/span><span class="count">(\d+)<\/span>/g)].map((m) => ({
    label: m[1].replace(/&amp;/g, '&').replace(/&#x27;/g, "'"),
    count: Number(m[2]),
  }));
}

async function count(where: string): Promise<number> {
  return (await rows<{ count: number }>(`get products where category = "tents" and ${where} count`))[0].count;
}

test('a brand chosen still lists every brand, each counted as though none were', async () => {
  const brands = (
    await rows<Record<string, string | number>>('get products select brand, count(*) where category = "tents" group brand')
  ).map((b) => ({ brand: String(b.brand), n: Number(Object.values(b).find((v) => typeof v === 'number')) }));
  assert.ok(brands.length > 1);
  const chosen = brands[0].brand;
  const page = await html(`/c/tents?brand=${encodeURIComponent(chosen)}`);
  const listed = facet(page, 'Brand');
  assert.equal(listed.length, Math.min(brands.length, 12));
  for (const b of listed) {
    const want = brands.find((x) => x.brand === b.label);
    assert.ok(want, b.label);
    assert.equal(b.count, want.n, b.label);
  }
  // The other facets count the brand's rows alone.
  const colours = facet(page, 'Colour').reduce((n, f) => n + f.count, 0);
  assert.ok(colours <= (await count(`brand = "${chosen}"`)));
});

test('a price band is the range of the price it names, counted by range', async () => {
  // A band the tents have rows in, and one they may not.
  const bands = [
    { slug: 'under-25', label: 'Under $25', where: 'price < 2500' },
    { slug: '25-50', label: '$25 to $50', where: 'price >= 2500 and price < 5000' },
    { slug: '50-100', label: '$50 to $100', where: 'price >= 5000 and price < 10000' },
    { slug: '100-250', label: '$100 to $250', where: 'price >= 10000 and price < 25000' },
    { slug: '250-plus', label: '$250 and over', where: 'price >= 25000' },
  ];
  const counts = await Promise.all(bands.map((b) => count(b.where)));
  const i = counts.findIndex((n) => n > 0);
  const page = await html(`/c/tents?price=${bands[i].slug}`);
  const drawn = facet(page, 'Price');
  // Disjunctive: every band with rows, as though none were chosen, in order.
  assert.deepEqual(
    drawn,
    bands.flatMap((b, k) => (counts[k] > 0 ? [{ label: b.label, count: counts[k] }] : [])),
  );
  const api = (await (await fetch(`${SHOP}/api/c/tents?price=${bands[i].slug}`)).json()) as { total: number; cards: { price: number }[] };
  assert.equal(api.total, counts[i]);
  // Two bands are either one.
  const two = (await (await fetch(`${SHOP}/api/c/tents?price=under-25&price=250-plus`)).json()) as { total: number };
  assert.equal(two.total, counts[0] + counts[4]);
});
