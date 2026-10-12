// A customer orders from a merchant: placed under their own token, to
// their own door. The merchant's reference is made here; a merchant's own
// system would send its own, and `@unique(merchant, ref)` keeps a retry one
// order.
import type { NextRequest } from 'next/server';
import { back, crossSite, json, readBody } from '../../../lib/http';
import { current, whoOf } from '../../../lib/session';
import { asUser, FenecError, one } from '../../../lib/db';
import { placeOrder, cancel } from '../../../lib/orders';
import { COMPANIES, people } from '../../../lib/world';
import { metresBetween, type Point } from '../../../lib/city';

export async function POST(req: NextRequest) {
  if (crossSite(req)) return json({ error: 'from Plover itself' }, 403);
  const a = await current();
  if (!a || a.role !== 'customer') return json({ error: 'sign in as a customer' }, 401);
  const who = whoOf(a);
  const { body, form } = await readBody(req);
  if (body.cancel) {
    const ok = await cancel(who, Number(body.cancel));
    return form ? back(req, `/orders/${Number(body.cancel)}${ok ? '' : '?late=1'}`) : json({ ok });
  }
  const m = await one<{ code: string; name: string; loc: Point }>(asUser(who), 'get merchants select code, name, loc where code = $1', [String(body.merchant ?? '')]);
  if (!m) return json({ error: 'no such merchant' }, 404);
  const me = people(COMPANIES.find((c) => c.slug === a.company)!).find((p) => p.uid === a.uid)!;
  const metres = metresBetween(m.loc, me.home.point);
  try {
    const { id } = await placeOrder(who, {
      ref: `${m.code}-${Date.now().toString(36)}`,
      merchant: m,
      dropoff: me.home.point,
      dropoffAddr: me.home.label,
      items: 2,
      fee: 250 + Math.round(metres / 10) * 5,
    });
    return form ? back(req, `/orders/${id}`) : json({ id });
  } catch (e) {
    // A company at its plan's rows takes no new order: say so.
    if (e instanceof FenecError && e.status === 507) return json({ error: 'The delivery company cannot take new orders right now.', quota: e.quota }, 503);
    throw e;
  }
}
