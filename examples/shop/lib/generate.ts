// The catalog, generated: the same 10 000 products from the same seed on
// every machine, so a test can name a product and a measurement can be
// repeated. Nothing here touches the network.
import { BRANDS, CATEGORIES, COLOURS, MODELS, type CategoryDef } from './catalog-data';
import { featuresOf } from './features';

/** mulberry32: a small, fast, seeded generator; good enough for a catalog. */
export function rng(seed: number) {
  let a = seed >>> 0;
  const next = () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  return {
    next,
    int: (lo: number, hi: number) => lo + Math.floor(next() * (hi - lo + 1)),
    pick: <T>(xs: readonly T[]): T => xs[Math.floor(next() * xs.length)],
  };
}

export function slugify(s: string): string {
  return s
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .toLowerCase()
    .replace(/&/g, 'and')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '');
}

/** Price bands the category pages filter by, in cents. */
export const PRICE_BANDS = [
  { slug: 'under-25', label: 'Under $25', max: 2500 },
  { slug: '25-50', label: '$25 to $50', max: 5000 },
  { slug: '50-100', label: '$50 to $100', max: 10000 },
  { slug: '100-250', label: '$100 to $250', max: 25000 },
  { slug: '250-plus', label: '$250 and over', max: Infinity },
] as const;

export function bandOf(cents: number): string {
  return PRICE_BANDS.find((b) => cents < b.max)!.slug;
}

export interface GeneratedProduct {
  sku: string;
  slug: string;
  name: string;
  description: string;
  category: string;
  brand: string;
  price: number;
  priceBand: string;
  colour: string;
  material: string;
  size: string;
  weight: number;
  rating: number | null;
  reviews: number;
  added: string;
  features: number[];
  stock: number;
}

function fill(template: string, v: Record<string, string>): string {
  return template.replace(/\{(\w+)\}/g, (_, k: string) => v[k] ?? '');
}

/** A price that looks like a price: log-uniform in range, ending in 9. */
function priceIn(r: ReturnType<typeof rng>, [lo, hi]: [number, number]): number {
  const p = Math.exp(Math.log(lo) + r.next() * (Math.log(hi) - Math.log(lo)));
  return Math.max(99, Math.round(p / 100) * 100 - 1);
}

function grams(g: number): string {
  return g >= 1000 ? `${(g / 1000).toFixed(g % 1000 === 0 ? 0 : 2).replace(/0$/, '')} kg` : `${g} g`;
}

const EPOCH = Date.UTC(2026, 0, 1);

export function generate(count = 10_000, seed = 0x5a4d6721): { products: GeneratedProduct[]; categories: CategoryDef[] } {
  const r = rng(seed);
  const brandsOf = new Map<string, string[]>();
  for (const c of CATEGORIES) {
    const pool = [...BRANDS];
    const mine: string[] = [];
    for (let i = 0; i < 8; i++) mine.push(pool.splice(Math.floor(r.next() * pool.length), 1)[0]);
    brandsOf.set(c.slug, mine);
  }
  const products: GeneratedProduct[] = [];
  const names = new Set<string>();
  for (let i = 0; i < count; i++) {
    const c = CATEGORIES[i % CATEGORIES.length];
    let brand: string, model: string, noun: string, colour: string, name: string;
    // A name once: brand, model, noun and colour together.
    do {
      brand = r.pick(brandsOf.get(c.slug)!);
      model = r.pick(MODELS);
      noun = r.pick(c.nouns);
      colour = r.pick(COLOURS);
      name = `${brand} ${model} ${noun}, ${colour}`;
    } while (names.has(name) || brand.includes(model)); // no "Seif Seif ..."
    names.add(name);
    const material = r.pick(c.materials);
    const size = r.pick(c.sizes);
    const weight = Math.round(r.int(c.weight[0], c.weight[1]) / 10) * 10;
    const price = priceIn(r, c.price);
    // Most products are rated, around 4.2; a few have no reviews yet.
    const reviews = r.next() < 0.08 ? 0 : Math.floor(Math.pow(r.next(), 2.2) * 900) + 1;
    const rating = reviews === 0 ? null : Math.min(5, Math.max(2.4, Math.round((4.25 + (r.next() + r.next() + r.next() - 1.5) * 0.9) * 10) / 10));
    const v = { model, material, colour, size };
    const description = [
      fill(r.pick(c.openers), v),
      r.pick(c.uses),
      `${noun[0].toUpperCase()}${noun.slice(1)} in ${colour}, ${size}, ${material}; weighs ${grams(weight)}.`,
    ].join(' ');
    const sku = `SG-${String(CATEGORIES.indexOf(c) + 1).padStart(2, '0')}${String(Math.floor(i / CATEGORIES.length)).padStart(4, '0')}`;
    const added = new Date(EPOCH + Math.floor(r.next() * 270 * 86_400_000)).toISOString();
    const stock = r.next() < 0.05 ? 0 : r.int(1, 60);
    products.push({
      sku,
      slug: `${slugify(`${brand} ${model} ${noun} ${colour}`)}-${sku.slice(3).toLowerCase()}`,
      name,
      description,
      category: c.slug,
      brand,
      price,
      priceBand: bandOf(price),
      colour,
      material,
      size,
      weight,
      rating,
      reviews,
      added,
      features: featuresOf({ category: c.slug, colour, material, price, rating }),
      stock,
    });
  }
  return { products, categories: CATEGORIES };
}
