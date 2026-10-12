// Orders coming in: placed by a customer under their own token -- the
// policy pins the customer to the token and the order to `ready` -- with
// the merchant's own reference, which `@unique(merchant, ref)` makes the
// order's key: sent twice after a timeout, it is one order.
import { asUser, FenecError, one, rows } from './db';
import type { Point } from './city';
import type { Who } from './tokens';
import type { Job } from './dispatch';

export interface NewOrder {
  ref: string;
  merchant: { code: string; name: string; loc: Point };
  dropoff: Point;
  dropoffAddr: string;
  items: number;
  fee: number;
  /** Minutes promised. */
  promise?: number;
}

/**
 * Places an order as `who`, a customer. A second send of the same
 * reference answers the order already there rather than a second one.
 *
 * The insert answers the id it made (`returning id`), held to the
 * customer's read rules as written: their own order. (Before `returning`
 * on an insert, the order was read back by its key in the same /batch.)
 */
export async function placeOrder(who: Who, o: NewOrder): Promise<{ id: number; again: boolean }> {
  const db = asUser(who);
  try {
    const made = await one<{ id: number }>(
      db,
      'insert orders {ref: $1, merchant: $2, customer: $3, pickup: $4, pickup_name: $5, dropoff: $6, dropoff_addr: $7, ' +
        'items: $8, fee: $9, status: "ready", run_at: now(), attempts: 0, created: now(), promised: now() + $10} returning id',
      [o.ref, o.merchant.code, who.sub, o.merchant.loc, o.merchant.name, o.dropoff, o.dropoffAddr, o.items, o.fee, (o.promise ?? 45) * 60_000],
    );
    return { id: made!.id, again: false };
  } catch (e) {
    if (!(e instanceof FenecError) || e.status !== 409) throw e;
    const kept = await one<{ id: number }>(db, 'get orders select id where merchant = $1 and ref = $2', [o.merchant.code, o.ref]);
    if (!kept) throw e;
    return { id: kept.id, again: true };
  }
}

/** A customer's own orders, the latest first. */
export function myOrders(who: Who, limit = 20): Promise<Job[]> {
  return rows<Job>(asUser(who), 'get orders where customer = $1 order created desc limit $2', [who.sub, limit]);
}

/** Cancels an order no courier has taken yet; `false` once one has. */
export async function cancel(who: Who, id: number): Promise<boolean> {
  try {
    await asUser(who).run(
      'set orders {status: "cancelled", run_at: null} where id = $1 and status = "ready" and courier is null require 1',
      [id],
    );
    return true;
  } catch (e) {
    if (e instanceof FenecError && e.status === 412) return false;
    throw e;
  }
}
