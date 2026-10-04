// Places an order from the cart (lib/checkout.ts). `key` comes from the
// checkout form, made when the form was drawn, so the same form sent twice
// is one order.
import type { NextRequest } from 'next/server';
import { placeOrder, reap } from '../../../lib/checkout';
import { shopper } from '../../../lib/session';
import { back, crossSite, json, readBody } from '../../../lib/http';

export async function POST(req: NextRequest) {
  if (crossSite(req)) return json({ error: 'cross-site request refused' }, 403);
  const s = await shopper();
  const { body, form } = await readBody(req);
  if (!s) return form ? back(req, '/cart') : json({ error: 'no cart' }, 400);
  // Lapsed reservations go back on sale before this one takes its units.
  await reap().catch(() => 0);
  const r = await placeOrder(s.owner, String(body.key ?? ''), body);
  if (form) return r.ok ? back(req, `/orders/${r.number}`) : back(req, `/checkout?error=${r.reason}${r.sku ? `&sku=${encodeURIComponent(r.sku)}` : ''}`);
  return json(r, r.ok ? 200 : r.reason === 'stock' ? 409 : r.reason === 'failed' ? 502 : 400);
}
