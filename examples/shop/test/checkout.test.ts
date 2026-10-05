// Correctness: checkout never sells what is not there, a retried checkout
// is one order, payment moves stock once, and a cart's totals are the
// catalog's prices summed.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ADMIN, FENEC, ROOT, SHOP, Shopper, key, rows, someProduct, stock } from './helpers';

type Inv = { available: number; reserved: number; sold: number };
const inv = async (sku: string) => (await rows<Inv>('get inventory select available, reserved, sold where sku = $1', [sku]))[0];

/** Units each order of a sku holds, by the order's status. */
async function held(sku: string) {
  const lines = await rows<{ orderNumber: string; qty: number }>('get order_lines select orderNumber, qty where sku = $1 limit 10000', [sku]);
  const by = { reserved: 0, paid: 0, cancelled: 0 } as Record<string, number>;
  for (const l of lines) {
    const [o] = await rows<{ status: string }>('get orders select status where number = $1', [l.orderNumber]);
    by[o.status] += l.qty;
  }
  return by;
}

test('N shoppers buying the last M units: exactly M are sold, never more', async () => {
  const p = await someProduct(3);
  const M = 5;
  const N = 24;
  const before = await held(p.sku); // orders a run before this one made
  await stock(p.sku, N); // enough for every cart to take one
  const shoppers = await Promise.all(
    Array.from({ length: N }, async () => {
      const s = new Shopper();
      assert.equal((await s.add(p.sku, 1)).status, 200);
      return s;
    }),
  );
  await stock(p.sku, M); // and now only M are left
  const results = await Promise.all(shoppers.map((s) => s.checkout(key())));
  const won = results.filter((r) => r.status === 200);
  const lost = results.filter((r) => r.status !== 200);
  assert.equal(won.length, M, `${won.length} orders for ${M} units`);
  for (const r of lost) assert.equal(r.body.reason, 'stock');
  const after = await inv(p.sku);
  assert.deepEqual(after, { available: 0, reserved: M, sold: 0 });
  const h = await held(p.sku);
  assert.equal(h.reserved - before.reserved, M);
  // A lost checkout keeps its cart: it can change the quantity and retry.
  const cart = await shoppers[results.indexOf(lost[0])].get('/api/cart');
  assert.equal(cart.body.lines.length, 1);
});

test('mixed quantities from many shoppers: stock is what was stocked less every order line', async () => {
  const p = await someProduct(4);
  const M = 11;
  const before = await held(p.sku);
  await stock(p.sku, 60);
  const want = Array.from({ length: 16 }, (_, i) => 1 + (i % 3));
  const shoppers = await Promise.all(
    want.map(async (q) => {
      const s = new Shopper();
      assert.equal((await s.add(p.sku, q)).status, 200);
      return s;
    }),
  );
  await stock(p.sku, M);
  const results = await Promise.all(shoppers.map((s) => s.checkout(key())));
  const sold = results.reduce((n, r, i) => n + (r.status === 200 ? want[i] : 0), 0);
  assert.ok(sold <= M, `sold ${sold} of ${M}`);
  assert.ok(sold >= M - 2, `a quantity that fit was refused: sold ${sold} of ${M}`);
  const after = await inv(p.sku);
  assert.equal(after.available, M - sold);
  assert.equal(after.reserved + after.sold, sold);
  const h = await held(p.sku);
  assert.equal(h.reserved + h.paid - before.reserved - before.paid, sold, 'order lines account for every unit taken');
});

test('a retried checkout -- in turn and at once -- is one order', async () => {
  const p = await someProduct(5);
  await stock(p.sku, 10);
  const s = new Shopper();
  await s.add(p.sku, 2);
  const k = key();
  const [a, b] = await Promise.all([s.checkout(k), s.checkout(k)]);
  const c = await s.checkout(k);
  assert.equal(a.status, 200);
  assert.equal(b.status, 200);
  assert.equal(c.status, 200);
  assert.equal(a.body.number, b.body.number);
  assert.equal(a.body.number, c.body.number);
  assert.equal((await rows('get orders where number = $1', [a.body.number])).length, 1);
  assert.equal((await rows('get order_lines where orderNumber = $1', [a.body.number])).length, 1);
  assert.deepEqual(await inv(p.sku), { available: 8, reserved: 2, sold: 0 });
});

test('cart totals are the catalog prices, summed on the server', async () => {
  const a = await someProduct(6);
  const b = await someProduct(7);
  await stock(a.sku, 50);
  await stock(b.sku, 50);
  const s = new Shopper();
  await s.add(a.sku, 1);
  const one = (await s.get('/api/cart')).body;
  assert.equal(one.subtotal, a.price);
  assert.equal(one.shipping, a.price >= 15000 ? 0 : 995);
  assert.equal(one.total, one.subtotal + one.shipping);
  await s.add(b.sku, 3);
  await s.add(a.sku, 2);
  const cart = (await s.get('/api/cart')).body;
  assert.equal(cart.count, 6);
  assert.equal(cart.subtotal, 3 * a.price + 3 * b.price);
  assert.equal(cart.shipping, cart.subtotal >= 15000 ? 0 : 995);
  assert.equal(cart.total, cart.subtotal + cart.shipping);
  const set = await s.post('/api/cart', { action: 'set', sku: b.sku, qty: 1 });
  assert.equal(set.body.subtotal, 3 * a.price + b.price);
  const gone = await s.post('/api/cart', { action: 'remove', sku: a.sku });
  assert.equal(gone.body.subtotal, b.price);
  // The cap is the write's own condition: 20 of one product at most.
  assert.equal((await s.add(b.sku, 20)).status, 400);
  assert.equal((await s.get('/api/cart')).body.lines[0].qty, 1);
});

test('payment moves reserved units to sold once, and a decline puts them back on sale', async () => {
  const p = await someProduct(8);
  await stock(p.sku, 10);
  const s = new Shopper();
  await s.add(p.sku, 3);
  const o = await s.checkout(key());
  assert.equal(o.status, 200);
  const paid = await s.post('/api/pay', { number: o.body.number, card: '4242 4242 4242 4242' });
  assert.equal(paid.status, 200);
  const again = await s.post('/api/pay', { number: o.body.number, card: '4242 4242 4242 4242' });
  assert.equal(again.status, 200);
  assert.equal(again.body.ref, paid.body.ref, 'one capture, one reference');
  assert.equal((await rows('get payments where orderNumber = $1', [o.body.number])).length, 1);
  assert.deepEqual(await inv(p.sku), { available: 7, reserved: 0, sold: 3 });

  const t = new Shopper();
  await t.add(p.sku, 2);
  const o2 = await t.checkout(key());
  assert.deepEqual(await inv(p.sku), { available: 5, reserved: 2, sold: 3 });
  const declined = await t.post('/api/pay', { number: o2.body.number, card: '4000 0000 0000 0002' });
  assert.equal(declined.status, 402);
  assert.deepEqual(await inv(p.sku), { available: 7, reserved: 0, sold: 3 });
  const [row] = await rows<{ status: string }>('get orders select status where number = $1', [o2.body.number]);
  assert.equal(row.status, 'cancelled');
  // A cancelled order cannot be paid for after all.
  assert.equal((await t.post('/api/pay', { number: o2.body.number, card: '4242 4242 4242 4242' })).status, 404);
});

test('a lapsed reservation is released once, and then cannot be paid for', async () => {
  const p = await someProduct(9);
  await stock(p.sku, 4);
  const s = new Shopper();
  await s.add(p.sku, 4);
  const o = await s.checkout(key());
  assert.deepEqual(await inv(p.sku), { available: 0, reserved: 4, sold: 0 });
  await rows('set orders {holdUntil: now() - 1000} where number = $1', [o.body.number]);
  // The reaper -- run by every checkout, and by a timer through /api/reap.
  const reaped = await fetch(`${SHOP}/api/reap`, { method: 'POST', headers: { authorization: `Bearer ${ADMIN}` } });
  assert.equal(reaped.status, 200);
  const twice = await fetch(`${SHOP}/api/reap`, { method: 'POST', headers: { authorization: `Bearer ${ADMIN}` } });
  assert.equal((await twice.json()).released, 0, 'a release runs once');
  assert.deepEqual(await inv(p.sku), { available: 4, reserved: 0, sold: 0 });
  const late = await s.post('/api/pay', { number: o.body.number, card: '4242 4242 4242 4242' });
  assert.equal(late.status, 404);
  assert.deepEqual(await inv(p.sku), { available: 4, reserved: 0, sold: 0 });
});

// The price guard checkout puts in its block: a line whose price is not the
// catalog's is refused there, at its statement, and nothing before it lands.
test('a checkout block with a price that changed is put back whole', async () => {
  const p = await someProduct(5);
  await stock(p.sku, 10);
  const lines = [
    { query: 'set inventory {available: available - $1, reserved: reserved + $1} where sku = $2 and available >= $1 require 1', params: [1, p.sku] },
    { query: 'get products select sku where sku = $1 and price = $2 limit 1 require 1', params: [p.sku, p.price + 1] },
  ];
  const res = await fetch(`${FENEC}/batch`, {
    method: 'POST',
    headers: { 'content-type': 'application/x-ndjson', authorization: `Bearer ${ROOT}` },
    body: lines.map((l) => JSON.stringify(l)).join('\n'),
  });
  assert.equal(res.status, 412);
  assert.equal(((await res.json()) as { at: number }).at, 1);
  assert.equal((await inv(p.sku)).available, 10);
});
