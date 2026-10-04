// The cart: GET is the cart with its totals, worked out here from the
// catalog's prices; POST adds, sets or takes out a line. A price sent with
// a request is never read -- the body's fields are action, sku and qty.
import type { NextRequest } from 'next/server';
import { addToCart, CartError, readCart, setQuantity } from '../../../lib/cart';
import { shopper, shopperOrNew } from '../../../lib/session';
import { back, crossSite, json, readBody } from '../../../lib/http';

export async function GET() {
  const s = await shopper();
  if (!s) return json({ lines: [], count: 0, subtotal: 0, shipping: 0, total: 0 });
  return json(await readCart(s.owner));
}

export async function POST(req: NextRequest) {
  if (crossSite(req)) return json({ error: 'cross-site request refused' }, 403);
  const { body, form } = await readBody(req);
  const s = await shopperOrNew();
  try {
    if (body.action === 'add') await addToCart(s.owner, body.sku, body.qty ?? 1);
    else if (body.action === 'set') await setQuantity(s.owner, body.sku, body.qty);
    else if (body.action === 'remove') await setQuantity(s.owner, body.sku, 0);
    else return json({ error: 'action is add, set or remove' }, 400);
  } catch (e) {
    if (!(e instanceof CartError)) throw e;
    if (form) return back(req, `/cart?error=${e.code}`);
    return json({ error: e.message, code: e.code }, e.status);
  }
  if (form) return back(req, body.action === 'add' ? '/cart?added=1' : '/cart');
  return json(await readCart(s.owner));
}
