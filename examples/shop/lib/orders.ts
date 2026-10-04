// A shopper's orders, read under the shopper's token: the policy's
// `owner = $jwt.sub` is in every one of these reads, the lookup of the
// lines included, so another shopper's order number finds nothing.
import 'server-only';
import { as } from './db';
import { orders } from './schema';

export interface OrderView {
  number: string;
  status: string;
  subtotal: number;
  shipping: number;
  total: number;
  email: string;
  at: string;
  holdUntil: string | null;
  payment: string | null;
  address: Record<string, string> | null;
  lines: { sku: string; name: string; qty: number; price: number }[];
}

export async function orderOf(owner: string, number: string): Promise<OrderView | null> {
  const row = await as(owner)
    .from(orders)
    .where('number', number)
    .lookup('order_lines', { on: 'orderNumber', parentKey: 'number', select: ['sku', 'name', 'qty', 'price'], order: [['id', 'asc']], limit: 200 })
    .first();
  if (!row) return null;
  const r = row as unknown as OrderView & { order_lines: OrderView['lines'] };
  return { ...r, lines: r.order_lines ?? [] };
}

export async function ordersOf(owner: string) {
  return as(owner).from(orders).select('number', 'status', 'total', 'at').order('at', 'desc').limit(50).rows();
}
