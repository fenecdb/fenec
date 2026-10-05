// What a listing's query string can say -- filters, an order, a page, a
// search -- read with every value bounded. No server code: the pages, the
// JSON route and the browser's "Show more" share it.

export const PAGE_SIZE = 24;

export const SORTS = {
  featured: { label: 'Best rated', field: 'rating', dir: 'desc' },
  'price-asc': { label: 'Price, low to high', field: 'price', dir: 'asc' },
  'price-desc': { label: 'Price, high to low', field: 'price', dir: 'desc' },
  newest: { label: 'Newest', field: 'added', dir: 'desc' },
} as const;
export type Sort = keyof typeof SORTS;

/** The facets a listing filters by, as a query string names them. */
export const FILTERS = {
  category: { field: 'category', label: 'Category' },
  brand: { field: 'brand', label: 'Brand' },
  price: { field: 'price', label: 'Price' },
  colour: { field: 'colour', label: 'Colour' },
  material: { field: 'material', label: 'Material' },
} as const;
export type FilterKey = keyof typeof FILTERS;
export type Filters = Partial<Record<FilterKey, string[]>>;

/** The query string read into filters, a sort and a page, every value bounded. */
export function readParams(sp: Record<string, string | string[] | undefined>) {
  const list = (v: string | string[] | undefined) =>
    (Array.isArray(v) ? v : v === undefined ? [] : [v]).filter((x) => x.length > 0 && x.length <= 80).slice(0, 12);
  const filters: Filters = {};
  for (const k of Object.keys(FILTERS) as FilterKey[]) {
    const v = list(sp[k]);
    if (v.length) filters[k] = v;
  }
  const sortRaw = list(sp.sort)[0];
  const sort: Sort = sortRaw && sortRaw in SORTS ? (sortRaw as Sort) : 'featured';
  const pageRaw = Number(list(sp.page)[0] ?? 1);
  const page = Number.isInteger(pageRaw) && pageRaw >= 1 && pageRaw <= 500 ? pageRaw : 1;
  const q = (list(sp.q)[0] ?? '').trim().slice(0, 120);
  return { filters, sort, page, q };
}

