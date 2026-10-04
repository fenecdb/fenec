// A cart, read and written under the shopper's own token. The policy holds
// that token to `owner = $jwt.sub` on every read and write, so a line id
// or sku from another shopper reaches nothing: a `set ... require 1` on a
// line the token cannot see writes 0 rows and is refused.
//
// Prices are never taken from the request. A line holds a sku and a
// quantity; what it costs is read from the catalog each time the cart is
// shown or bought.
import 'server-only';
import { as, batch, db, tokenFor } from './db';
import { products, inventory } from './schema';
import { shippingFor } from './money';

import { MAX_QTY } from './cart-limits';

export { MAX_QTY };

export interface CartLine {
  sku: string;
  qty: number;
  slug: string;
  name: string;
  brand: string;
  category: string;
  colour: string | null;
  price: number;
  lineTotal: number;
  available: number;
}

export interface CartView {
  lines: CartLine[];
  count: number;
  subtotal: number;
  shipping: number;
  total: number;
}

const lineOf = (owner: string, sku: string) => `${owner}|${sku}`;

/** The cart with each line's price read from the catalog, totals summed here. */
export async function readCart(owner: string): Promise<CartView> {
  const rows = (await as(owner)
    .from('carts')
    .select('sku', 'qty')
    .order('id')
    .limit(200)
    .lookup('products', { on: 'sku', parentKey: 'sku', select: ['slug', 'name', 'brand', 'category', 'colour', 'price'], limit: 1 })
    .rows()) as unknown as { sku: string; qty: number; products: { slug: string; name: string; brand: string; category: string; colour: string | null; price: number }[] }[];
  const skus = rows.map((r) => r.sku);
  const stock = new Map<string, number>();
  if (skus.length) {
    const shop = await db();
    const inv = await shop.from(inventory).select('sku', 'available').where('sku', 'in', skus).limit(200).rows();
    for (const r of inv) stock.set(r.sku, r.available);
  }
  const lines: CartLine[] = [];
  for (const r of rows) {
    const p = r.products[0];
    if (!p || r.qty <= 0) continue; // a product taken off sale leaves the cart
    lines.push({ sku: r.sku, qty: r.qty, ...p, lineTotal: p.price * r.qty, available: stock.get(r.sku) ?? 0 });
  }
  const subtotal = lines.reduce((n, l) => n + l.lineTotal, 0);
  const shipping = shippingFor(subtotal);
  return { lines, count: lines.reduce((n, l) => n + l.qty, 0), subtotal, shipping, total: subtotal + shipping };
}

/** What went wrong, by a code a page can name without echoing a request's text. */
export const CART_ERRORS = {
  qty: `Choose a quantity from 0 to ${MAX_QTY}.`,
  unknown: 'That product does not exist.',
  limit: `A cart holds at most ${MAX_QTY} of one product.`,
  line: 'That line is not in your cart.',
  soldout: 'That product has sold out.',
  failed: 'The cart could not be changed; try again.',
} as const;
export type CartErrorCode = keyof typeof CART_ERRORS;

export class CartError extends Error {
  constructor(readonly code: CartErrorCode, readonly status = 400) {
    super(CART_ERRORS[code]);
  }
}

function qtyOf(v: unknown): number {
  const n = Number(v);
  if (!Number.isInteger(n) || n < 0 || n > MAX_QTY) throw new CartError('qty');
  return n;
}

async function known(sku: unknown): Promise<string> {
  if (typeof sku !== 'string' || !/^SG-\d{6}$/.test(sku)) throw new CartError('unknown', 404);
  const shop = await db();
  const p = await shop.from(products).select('sku').where('sku', sku).first();
  if (!p) throw new CartError('unknown', 404);
  return sku;
}

/**
 * Adds `qty` of a product: the line made if absent, then its quantity
 * raised, as one block under the shopper's token. The cap is part of the
 * write (`qty + $1 <= MAX`), so two quick clicks cannot pass it.
 */
export async function addToCart(owner: string, skuIn: unknown, qtyIn: unknown) {
  const sku = await known(skuIn);
  const qty = qtyOf(qtyIn);
  if (qty === 0) return;
  // A courtesy, not the guard: stock can still go between now and the
  // checkout, whose `require 1` is what holds.
  const shop = await db();
  const inv = await shop.from(inventory).select('available').where('sku', sku).first();
  if (!inv || inv.available <= 0) throw new CartError('soldout', 409);
  const line = lineOf(owner, sku);
  const res = await batch(
    [
      ['put carts {line: $1, sku: $2, qty: 0, touched: now()} if absent', [line, sku]],
      [`set carts {qty: qty + $1, touched: now()} where line = $2 and qty + $1 <= ${MAX_QTY} require 1`, [qty, line]],
    ],
    { token: tokenFor(owner) },
  );
  if (res.status === 412) throw new CartError('limit');
  if (res.status !== 200) throw new CartError('failed', res.status >= 400 ? res.status : 500);
}

/** Sets a line's quantity; 0 takes the line out. Only the shopper's own line. */
export async function setQuantity(owner: string, skuIn: unknown, qtyIn: unknown) {
  if (typeof skuIn !== 'string') throw new CartError('unknown', 404);
  const qty = qtyOf(qtyIn);
  const line = lineOf(owner, skuIn);
  const res =
    qty === 0
      ? await batch([['del carts where line = $1 require 1', [line]]], { token: tokenFor(owner) })
      : await batch([['set carts {qty: $1, touched: now()} where line = $2 require 1', [qty, line]]], { token: tokenFor(owner) });
  if (res.status === 412) throw new CartError('line', 404);
  if (res.status !== 200) throw new CartError('failed', res.status >= 400 ? res.status : 500);
}
