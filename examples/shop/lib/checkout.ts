// Checkout: stock reserved, an order written and the cart emptied as one
// block, which cannot oversell.
//
// Stock is a counter per sku (`inventory.available`). Each line of the
// order takes its units with
//
//   set inventory {available: available - $1, reserved: reserved + $1}
//     where sku = $2 and available >= $1 require 1
//
// The condition is the guard and `require 1` makes a miss an error: the
// write that finds too few units writes nothing, the statement is refused
// (412), and the whole `/batch` -- every line's reservation, the order and
// its lines -- is put back. One writer holds the lock for the block, so two
// shoppers after the last unit cannot both pass the guard. Without
// `require`, the miss was `{"affected": 0}` and the order landed anyway.
//
// The order is a hold: `reserved` until paid, cancelled when payment fails
// or its hold lapses (`reap`), each of those a guarded change of status
// (`where status = "reserved" require 1`) in the same block as the stock it
// moves back, so a payment and a lapse racing for one order cannot both win.
//
// The batch carries an `Idempotency-Key` from the checkout form, and the
// order's number is derived from it: a retry after a timeout, or a second
// click, is answered with the first answer and makes no second order.
import 'server-only';
import { createHash } from 'node:crypto';
import { as, batch, db, query, type Statement } from './db';
import { readCart } from './cart';
import { orders, orderLines } from './schema';
import { capture } from './payments';

/** How long a reservation waits for payment. */
export const HOLD_MS = 15 * 60 * 1000;

export interface Address {
  name: string;
  line1: string;
  city: string;
  postcode: string;
  country: string;
}

export type CheckoutResult =
  | { ok: true; number: string; replayed: boolean }
  | { ok: false; reason: 'empty' | 'stock' | 'changed' | 'invalid' | 'failed'; message: string; sku?: string };

export function orderNumber(owner: string, key: string): string {
  const h = createHash('sha256').update(`${owner}\u0000${key}`).digest('hex').toUpperCase();
  return `SG-${h.slice(0, 4)}-${h.slice(4, 10)}`;
}

const text = (v: unknown, max: number) => (typeof v === 'string' ? v.trim().slice(0, max) : '');

export function readAddress(form: Record<string, unknown>): { email: string; address: Address } | null {
  const email = text(form.email, 200);
  const address = {
    name: text(form.name, 120),
    line1: text(form.line1, 200),
    city: text(form.city, 120),
    postcode: text(form.postcode, 20),
    country: text(form.country, 60),
  };
  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email) || Object.values(address).some((v) => !v)) return null;
  return { email, address };
}

export async function placeOrder(owner: string, key: string, form: Record<string, unknown>): Promise<CheckoutResult> {
  if (!/^[A-Za-z0-9_-]{16,64}$/.test(key)) return { ok: false, reason: 'invalid', message: 'The checkout form is out of date; open it again.' };
  const number = orderNumber(owner, key);
  // Sent twice, the second request finds the order the first one made.
  const existing = await as(owner).from(orders).select('number').where('number', number).first();
  if (existing) return { ok: true, number, replayed: true };

  const who = readAddress(form);
  if (!who) return { ok: false, reason: 'invalid', message: 'Fill in an email address and every line of the address.' };
  const cart = await readCart(owner);
  if (!cart.lines.length) return { ok: false, reason: 'empty', message: 'Your cart is empty.' };

  const statements: Statement[] = cart.lines.map((l) => [
    'set inventory {available: available - $1, reserved: reserved + $1} where sku = $2 and available >= $1 require 1',
    [l.qty, l.sku],
  ]);
  // Each line's price as the cart read it, held inside the block: a price
  // changed since is a 412 here, and nothing of the order lands.
  for (const l of cart.lines) {
    statements.push(['get products select sku where sku = $1 and price = $2 limit 1 require 1', [l.sku, l.price]]);
  }
  statements.push([
    'insert orders {number: $1, owner: $2, status: "reserved", subtotal: $3, shipping: $4, total: $5, email: $6, address: $7, at: now(), holdUntil: now() + $8}',
    [number, owner, cart.subtotal, cart.shipping, cart.total, who.email, who.address, HOLD_MS],
  ]);
  for (const l of cart.lines) {
    statements.push([
      'insert order_lines {orderNumber: $1, owner: $2, sku: $3, name: $4, qty: $5, price: $6}',
      [number, owner, l.sku, l.name, l.qty, l.price],
    ]);
  }
  statements.push(['del carts where owner = $1', [owner]]);

  const res = await batch(statements, { key: `checkout:${owner}:${key}` });
  if (res.status === 200) return { ok: true, number, replayed: res.replayed };
  if (res.status === 412 && res.at !== undefined && res.at < cart.lines.length) {
    const l = cart.lines[res.at];
    return { ok: false, reason: 'stock', sku: l.sku, message: `Only ${await availableOf(l.sku)} of ${l.name} left; change the quantity and try again.` };
  }
  if (res.status === 412 && res.at !== undefined && res.at < 2 * cart.lines.length) {
    const l = cart.lines[res.at - cart.lines.length];
    return { ok: false, reason: 'changed', sku: l.sku, message: `The price of ${l.name} changed; review your cart and place the order again.` };
  }
  if (res.status === 422) return { ok: false, reason: 'changed', message: 'Your cart changed while the order was being placed; review it and place the order again.' };
  return { ok: false, reason: 'failed', message: res.error ?? `The order could not be placed (${res.status}).` };
}

async function availableOf(sku: string): Promise<number> {
  const rows = await query<{ available: number }>('get inventory select available where sku = $1', [sku]);
  return rows[0]?.available ?? 0;
}

async function linesOf(number: string) {
  const shop = await db();
  return shop.from(orderLines).select('sku', 'qty').where('orderNumber', number).limit(200).rows();
}

/**
 * Gives a reservation's units back and cancels it -- only while it is still
 * reserved, so a paid order is never released and a release runs once.
 */
export async function release(number: string): Promise<boolean> {
  const lines = await linesOf(number);
  const statements: Statement[] = [['set orders {status: "cancelled"} where number = $1 and status = "reserved" require 1', [number]]];
  for (const l of lines) {
    statements.push(['set inventory {available: available + $1, reserved: reserved - $1} where sku = $2 and reserved >= $1 require 1', [l.qty, l.sku]]);
  }
  const res = await batch(statements, { key: `release:${number}` });
  return res.status === 200 && !res.replayed;
}

/** Releases every reservation whose hold has lapsed. */
export async function reap(): Promise<number> {
  const due = await query<{ number: string }>('get orders select number where status = "reserved" and holdUntil <= now() limit 200');
  let n = 0;
  for (const o of due) if (await release(o.number)) n++;
  return n;
}

export type PayResult = { ok: true; ref: string } | { ok: false; reason: 'declined' | 'gone' | 'failed'; message: string };

/**
 * Pays for a reserved order: the provider's capture first, idempotent by
 * the order's number, then in one block the payment recorded (a second is
 * a clash on `payments.orderNumber`), the order marked paid -- only from
 * `reserved` -- and its units moved from reserved to sold.
 */
export async function pay(owner: string, number: string, card: string): Promise<PayResult> {
  const order = await as(owner).from(orders).select('number', 'status', 'total').where('number', number).first();
  if (!order) return { ok: false, reason: 'gone', message: 'There is no such order.' };
  if (order.status === 'paid') {
    const shop = await db();
    const p = await shop.from('payments').select('ref').where('orderNumber', number).first();
    return { ok: true, ref: String(p?.ref ?? '') };
  }
  if (order.status !== 'reserved') return { ok: false, reason: 'gone', message: 'This order was cancelled: its reservation lapsed or its payment was declined.' };

  const charge = await capture({ amount: order.total, card, key: `order:${number}` });
  if (!charge.ok) {
    await release(number);
    return { ok: false, reason: 'declined', message: `The card was declined (${charge.reason}). The items have been put back on sale; your cart is empty, so add them again to retry.` };
  }
  const lines = await linesOf(number);
  const statements: Statement[] = [
    ['insert payments {orderNumber: $1, ref: $2, amount: $3, at: now()}', [number, charge.ref, order.total]],
    ['set orders {status: "paid", payment: $2} where number = $1 and status = "reserved" require 1', [number, charge.ref]],
  ];
  for (const l of lines) {
    statements.push(['set inventory {reserved: reserved - $1, sold: sold + $1} where sku = $2 and reserved >= $1 require 1', [l.qty, l.sku]]);
  }
  const res = await batch(statements, { key: `capture:${number}` });
  if (res.status === 200) return { ok: true, ref: charge.ref };
  if (res.status === 409) return { ok: true, ref: charge.ref }; // paid by a request beside this one
  if (res.status === 412) {
    // The hold lapsed between the read and the capture: the money goes back.
    await capture({ amount: -order.total, card, key: `refund:${number}` });
    return { ok: false, reason: 'gone', message: 'The reservation lapsed before payment completed; you have not been charged.' };
  }
  return { ok: false, reason: 'failed', message: res.error ?? `Payment could not be recorded (${res.status}).` };
}
