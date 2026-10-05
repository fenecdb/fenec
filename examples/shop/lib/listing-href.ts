// A listing's address with one thing changed: shared by the pages, which
// write links, and the JSON route, which says where the next page is.
import { FILTERS, type FilterKey, type Filters, type Sort } from './catalog-params';

export interface ListingState {
  base: string;
  filters: Filters;
  sort: Sort;
  page: number;
  q?: string;
}

/** The address of this listing with something changed. */
export function hrefOf(s: ListingState, change: { filters?: Filters; sort?: Sort; page?: number }): string {
  const p = new URLSearchParams();
  if (s.q) p.set('q', s.q);
  const filters = change.filters ?? s.filters;
  for (const k of Object.keys(FILTERS) as FilterKey[]) for (const v of filters[k] ?? []) p.append(k, v);
  const sort = change.sort ?? s.sort;
  if (sort !== 'featured') p.set('sort', sort);
  const page = change.page ?? (change.filters || change.sort ? 1 : s.page);
  if (page > 1) p.set('page', String(page));
  const qs = p.toString();
  return qs ? `${s.base}?${qs}` : s.base;
}

