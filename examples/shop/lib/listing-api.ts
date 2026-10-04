// A page of a listing as JSON, for "Show more" and the load test: the same
// query the page runs (`listing`), and where the next page is.
import 'server-only';
import type { NextRequest } from 'next/server';
import { listing, readParams, PAGE_SIZE } from './catalog';
import { hrefOf } from './listing-href';
import { json } from './http';

export async function listingJson(req: NextRequest, base: string, api: string, category?: string) {
  const sp: Record<string, string | string[]> = {};
  for (const k of new Set(req.nextUrl.searchParams.keys())) {
    const all = req.nextUrl.searchParams.getAll(k);
    sp[k] = all.length > 1 ? all : all[0];
  }
  const { filters, sort, page, q } = readParams(sp);
  if (!category && !q) return json({ error: 'search needs q' }, 400);
  const { cards, total } = await listing({ category, q: category ? undefined : q, filters, sort, page });
  const s = { base, filters, sort, page, q: category ? undefined : q };
  const more = page * PAGE_SIZE < total;
  const nextHref = hrefOf(s, { page: page + 1 });
  return json({
    cards,
    total,
    next: more ? { api: `${api}${nextHref.slice(base.length)}`, next: nextHref } : null,
  });
}
