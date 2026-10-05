// The catalog's reads: what each page asks fenec-server, through the query
// builder of `@fenecdb/web/client`. Every value a shopper types or clicks
// goes in as a parameter ($1, $2, ...), never into the text, so a search
// for `"; del products; --` is a search for those characters.
import 'server-only';
import { cache } from 'react';
import { and, or } from '@fenecdb/web/client';
import { db } from './db';
import { products, categories, inventory } from './schema';
import { PRICE_BANDS } from './generate';
import { FILTERS, PAGE_SIZE, SORTS, type FilterKey, type Filters, type Sort } from './catalog-params';

export * from './catalog-params';


/** What a listing shows of a product: no description, no vector. */
const CARD = ['id', 'sku', 'slug', 'name', 'brand', 'category', 'price', 'colour', 'rating', 'reviews'] as const;

export type Card = {
  id: number;
  sku: string;
  slug: string;
  name: string;
  brand: string;
  category: string;
  price: number;
  colour: string | null;
  rating: number | null;
  reviews: number | null;
  /** Where the words of a search are in the name, UTF-16 offsets. */
  marks?: [number, number][];
  snippet?: { text: string; marks: [number, number][] } | null;
};

export interface Facet {
  value: string | null;
  count: number;
}

export const categoryList = cache(async () => {
  const shop = await db();
  return shop.from(categories).order('position').limit(100).rows();
});

export const category = cache(async (slug: string) => {
  const shop = await db();
  return shop.from(categories).where('slug', slug).first();
});

/**
 * The bounds of the price facet's ranges, in cents: each band from its
 * floor to the next, the last up to the largest whole number a bound
 * takes exactly.
 */
const BOUNDS = [0, ...PRICE_BANDS.map((b) => Math.min(b.max, Number.MAX_SAFE_INTEGER))];

/** The rows of the price bands a query string names: `price >= floor and price < next`, a band each. */
function bandsWhere(slugs: string[]) {
  const conds = PRICE_BANDS.flatMap((b, i) =>
    slugs.includes(b.slug) ? [and({ price: { gte: BOUNDS[i] } }, { price: { lt: BOUNDS[i + 1] } })] : [],
  );
  return conds.length === 1 ? conds[0] : or(...conds);
}

/**
 * A page of a category or a search, with each facet counted over every
 * row it matches, in one statement. A facet a filter is set on is
 * `disjunctive`: counted without its own filter, so "Brand" still lists
 * the other brands to add -- where it was a query of its own beside the
 * page. The price is counted by ranges of the `@sorted` field: the bands
 * are no field of their own.
 */
export async function listing(opts: { category?: string; q?: string; filters: Filters; sort: Sort; page: number }) {
  const shop = await db();
  const on = (k: FilterKey) => (opts.filters[k]?.length ?? 0) > 0;
  // The rows the page and its facets are of.
  let base = shop.from(products);
  if (opts.category) base = base.where('category', opts.category);
  for (const [k, values] of Object.entries(opts.filters) as [FilterKey, string[]][]) {
    if (!values.length) continue;
    if (k === 'price') {
      if (PRICE_BANDS.some((b) => values.includes(b.slug))) base = base.where(bandsWhere(values));
      continue;
    }
    const field = FILTERS[k].field;
    base = values.length === 1 ? base.where(field, values[0]) : base.where(field, 'in', values);
  }
  if (opts.q) base = base.match('description', opts.q);

  let page = base.select(...CARD);
  if (opts.q) {
    page = page.highlight('name').snippet('description', 26, { ellipsis: '…' });
  } else {
    const s = SORTS[opts.sort];
    page = page.order(s.field, s.dir).order('id');
  }
  // `category` is asked on a category page too, where nothing chooses it:
  // one value, from its index, whose count is the matches.
  page = page
    .facet('brand', { top: 40, disjunctive: on('brand') })
    .facet('price', { ranges: BOUNDS, disjunctive: on('price') })
    .facet('colour', { disjunctive: on('colour') })
    .facet('material', { top: 20, disjunctive: on('material') })
    .facet('category', { disjunctive: on('category') })
    .offset((opts.page - 1) * PAGE_SIZE)
    .limit(PAGE_SIZE);

  const rows = await page.rows();
  const facets: Record<string, Facet[]> = {};
  for (const [field, counts] of Object.entries(rows.facets ?? {})) {
    facets[field] =
      field === 'price'
        ? counts.flatMap((f, i) => (f.count > 0 ? [{ value: PRICE_BANDS[i].slug, count: f.count }] : []))
        : (counts as Facet[]);
  }
  // The matches: the counts of a facet no filter of its own leaves out add
  // up to them -- every product has a category and a colour or none -- and
  // with each of those chosen, a count of their own.
  const sum = (counts: { count: number }[] | undefined) => (counts ?? []).reduce((n, f) => n + f.count, 0);
  const total = !on('category')
    ? sum(rows.facets?.category)
    : !on('colour')
      ? sum(rows.facets?.colour)
      : sum((await base.facet('colour').limit(0).rows()).facets.colour);
  const cards: Card[] = rows.map((r) => {
    const row = r as unknown as Card & { 'highlight(name)'?: [number, number][]; 'snippet(description)'?: Card['snippet'] };
    return { ...row, marks: row['highlight(name)'] ?? undefined, snippet: row['snippet(description)'] ?? null };
  });
  return { cards, facets, total };
}

export function bandLabel(slug: string | null): string {
  return PRICE_BANDS.find((b) => b.slug === slug)?.label ?? 'Other';
}

export const product = cache(async (slug: string) => {
  const shop = await db();
  return shop.from(products).where('slug', slug).first();
});

export async function stockOf(sku: string): Promise<number> {
  const shop = await db();
  const row = await shop.from(inventory).select('available').where('sku', sku).first();
  return row?.available ?? 0;
}

/**
 * "More like this": `near` over the product's feature vector, within its
 * category. The vector holds category, colour, material and price
 * (lib/features.ts), so the nearest are the closest alternatives.
 */
export async function related(p: { id: number; category: string; features: number[] | null }): Promise<Card[]> {
  if (!p.features) return [];
  const shop = await db();
  const rows = await shop
    .from(products)
    .select(...CARD)
    .where('category', p.category)
    .where('id', '!=', p.id)
    .near('features', p.features)
    .limit(4)
    .rows();
  return rows as unknown as Card[];
}

export async function fromBrand(p: { id: number; brand: string; category: string }): Promise<Card[]> {
  const shop = await db();
  const rows = await shop
    .from(products)
    .select(...CARD)
    .where('brand', p.brand)
    .where('category', '!=', p.category)
    .order('rating', 'desc')
    .limit(4)
    .rows();
  return rows as unknown as Card[];
}

export async function newest(limit = 8): Promise<Card[]> {
  const shop = await db();
  return (await shop.from(products).select(...CARD).order('added', 'desc').limit(limit).rows()) as unknown as Card[];
}

export async function bestRated(limit = 8): Promise<Card[]> {
  const shop = await db();
  return (await shop
    .from(products)
    .select(...CARD)
    .where('reviews', '>=', 300)
    .order('rating', 'desc')
    .limit(limit)
    .rows()) as unknown as Card[];
}

/** Typed suggestions under the search box: the name's own index, by prefix. */
export async function suggest(q: string): Promise<Pick<Card, 'slug' | 'name'>[]> {
  const shop = await db();
  return (await shop.from(products).select('slug', 'name').match('name', q).limit(6).rows()) as Pick<Card, 'slug' | 'name'>[];
}

/** Every product's address, for the sitemaps. */
export async function allSlugs(): Promise<{ slug: string; added: string | null }[]> {
  const shop = await db();
  return (await shop.from(products).select('slug', 'added').order('id').limit(100_000).rows()) as { slug: string; added: string | null }[];
}

/**
 * Where semantic search would go. A sentence model's vector of the query
 * -- transformers.js on the server, or an embeddings API -- put beside
 * `match` with `.near('embedding', v).fuse()`, over an `embedding` field
 * filled the same way at seed time. The shop has no model it can run
 * without a download or a key, so it searches by the words alone.
 */
export const searchHook = null;
