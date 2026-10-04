// The catalog's reads: what each page asks fenec-server, through the query
// builder of `@fenecdb/web/client`. Every value a shopper types or clicks
// goes in as a parameter ($1, $2, ...), never into the text, so a search
// for `"; del products; --` is a search for those characters.
import 'server-only';
import { cache } from 'react';
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
 * A page of a category or a search, with each facet counted over every
 * row it matches. A facet a filter is set on is counted again without its
 * own filter -- so "Brand" still lists the other brands to add -- each
 * such count a query of its own beside the page, run at once.
 */
export async function listing(opts: { category?: string; q?: string; filters: Filters; sort: Sort; page: number }) {
  const shop = await db();
  const base = (skip?: FilterKey) => {
    let q = shop.from(products);
    if (opts.category) q = q.where('category', opts.category);
    for (const [k, values] of Object.entries(opts.filters) as [FilterKey, string[]][]) {
      if (k === skip || !values.length) continue;
      const field = FILTERS[k].field;
      q = values.length === 1 ? q.where(field, values[0]) : q.where(field, 'in', values);
    }
    return q;
  };
  let page = base().select(...CARD);
  if (opts.q) {
    page = page.highlight('name').snippet('description', 26, { ellipsis: '…' }).match('description', opts.q);
  } else {
    const s = SORTS[opts.sort];
    page = page.order(s.field, s.dir).order('id');
  }
  page = page
    .facet('brand', { top: 40 })
    .facet('priceBand')
    .facet('colour')
    .facet('material', { top: 20 });
  if (opts.q && !opts.category) page = page.facet('category');
  page = page.offset((opts.page - 1) * PAGE_SIZE).limit(PAGE_SIZE);

  const again = (Object.keys(opts.filters) as FilterKey[]).map(async (k) => {
    let q = base(k);
    if (opts.q) q = q.match('description', opts.q);
    const rows = await q.facet(FILTERS[k].field, { top: 40 }).limit(0).rows();
    return [FILTERS[k].field, (rows.facets?.[FILTERS[k].field] ?? []) as Facet[]] as const;
  });
  const [got, ...recounted] = await Promise.all([page.rows(), ...again]);
  // `page` was built in steps, so its type no longer knows it asked for facets.
  const rows = got as typeof got & { facets?: Record<string, Facet[]> };
  const facets: Record<string, Facet[]> = { ...((rows.facets ?? {}) as Record<string, Facet[]>) };
  for (const [field, counts] of recounted) facets[field] = counts;
  // Every row has a brand, so the brands' counts add up to the matches.
  const total = ((rows.facets?.brand ?? []) as Facet[]).reduce((n, f) => n + f.count, 0);
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
