// Pays for a reserved order through the mock provider (lib/payments.ts).
// The amount is the order's, read from the database; nothing in the
// request says what to charge.
import type { NextRequest } from 'next/server';
import { pay } from '../../../lib/checkout';
import { shopper } from '../../../lib/session';
import { back, crossSite, json, readBody } from '../../../lib/http';

export async function POST(req: NextRequest) {
  if (crossSite(req)) return json({ error: 'cross-site request refused' }, 403);
  const s = await shopper();
  const { body, form } = await readBody(req);
  const number = String(body.number ?? '');
  if (!s || !/^SG-[0-9A-F]{4}-[0-9A-F]{6}$/.test(number)) return form ? back(req, '/account') : json({ error: 'no such order' }, 404);
  const r = await pay(s.owner, number, String(body.card ?? ''));
  if (form) return back(req, `/orders/${number}${r.ok ? '?paid=1' : `?error=${r.reason}`}`);
  return json(r, r.ok ? 200 : r.reason === 'gone' ? 404 : r.reason === 'declined' ? 402 : 502);
}
