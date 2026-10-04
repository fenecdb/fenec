// Security: a shopper reaches only its own cart and orders, through the
// shop or straight at fenec-server with its token; a guest's cart cannot be
// had by guessing; prices are the server's; search text is a parameter.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FENEC, SHOP, Shopper, fenec, key, rows, someProduct, stock, tokenAs } from './helpers';

test('one shopper cannot read or change another shopper’s cart or orders through the shop', async () => {
  const p = await someProduct(10);
  const q = await someProduct(11);
  await stock(p.sku, 50);
  await stock(q.sku, 50);
  const alice = await Shopper.signedUp('alice');
  const bob = await Shopper.signedUp('bob');
  await alice.add(p.sku, 2);
  const order = await alice.checkout(key());
  assert.equal(order.status, 200);
  await alice.add(q.sku, 1);

  // Alice's order is hers alone: by the page, by the JSON route, by paying for it.
  assert.equal((await bob.get(`/api/orders/${order.body.number}`)).status, 404);
  assert.equal((await bob.req(`/orders/${order.body.number}`)).status, 404);
  assert.equal((await bob.post('/api/pay', { number: order.body.number, card: '4242 4242 4242 4242' })).status, 404);
  assert.equal((await new Shopper().get(`/api/orders/${order.body.number}`)).status, 404);
  assert.equal((await alice.get(`/api/orders/${order.body.number}`)).status, 200);

  // Bob changing "the line of q" changes nothing of Alice's.
  assert.equal((await bob.post('/api/cart', { action: 'set', sku: q.sku, qty: 9 })).status, 404);
  assert.equal((await bob.post('/api/cart', { action: 'remove', sku: q.sku })).status, 404);
  const hers = (await alice.get('/api/cart')).body;
  assert.equal(hers.lines.length, 1);
  assert.equal(hers.lines[0].qty, 1);
  assert.equal((await bob.get('/api/cart')).body.lines.length, 0);

  // A signed session cookie altered to name Alice is no session at all.
  const forged = new Shopper();
  const [value, sig] = bob.jar.get('sg_session')!.split('.');
  const claims = JSON.parse(Buffer.from(value, 'base64url').toString());
  forged.jar.set('sg_session', `${Buffer.from(JSON.stringify({ ...claims, uid: claims.uid - 1 })).toString('base64url')}.${sig}`);
  assert.equal((await forged.get('/api/cart')).body.lines.length, 0);
  assert.equal((await forged.get(`/api/orders/${order.body.number}`)).status, 404);
});

test('a shopper’s own token, sent straight to fenec-server, reaches only its own rows', async () => {
  const p = await someProduct(12);
  await stock(p.sku, 50);
  const alice = new Shopper();
  await alice.add(p.sku, 1);
  const o = await alice.checkout(key());
  const [mine] = await rows<{ owner: string }>('get orders select owner where number = $1', [o.body.number]);
  const mallory = tokenAs('g:mallory');

  // Reads: the policy's filter is in every read, lookups and counts too.
  assert.deepEqual(await fenec('get orders where owner = $1', [mine.owner], mallory), { status: 200, body: [] });
  assert.deepEqual(await fenec('get orders where number = $1', [o.body.number], mallory), { status: 200, body: [] });
  assert.deepEqual(await fenec('get order_lines where orderNumber = $1', [o.body.number], mallory), { status: 200, body: [] });
  assert.deepEqual((await fenec('get carts count', [], mallory)).body, [{ count: 0 }]);
  // Writes: a set or a del touches only Mallory's rows, and a row written
  // for someone else is refused.
  assert.deepEqual((await fenec('set carts {qty: 99} where owner = $1', [mine.owner], mallory)).body, { affected: 0 });
  assert.deepEqual((await fenec('del carts', [], mallory)).body, { affected: 0 });
  assert.equal((await fenec('put carts {owner: $1, line: "x", sku: "y", qty: 1}', [mine.owner], mallory)).status, 403);
  assert.equal((await fenec('set orders {status: "paid"} where number = $1', [o.body.number], mallory)).status, 403);
  assert.equal((await fenec('set inventory {available: 1000}', [], mallory)).status, 404);
  // Collections no rule lets it read do not exist for it.
  assert.equal((await fenec('get users', [], mallory)).status, 404);
  assert.equal((await fenec('get payments', [], mallory)).status, 404);
  // The browser's stock token reads stock and nothing else.
  const viewer = tokenAs('stock-viewer', 'stock');
  assert.equal((await fenec('get inventory where sku = $1', [p.sku], viewer)).status, 200);
  assert.equal((await fenec('set inventory {available: 0} where sku = $1', [p.sku], viewer)).status, 403);
  assert.deepEqual((await fenec('get carts', [], viewer)).body, []);
  assert.deepEqual((await fenec('get orders count', [], viewer)).body, [{ count: 0 }]);
  // A token with no exp, or signed with another secret, is refused.
  assert.equal((await fenec('get products limit 1', [], 'not.a.token')).status, 401);
  const res = await fetch(`${FENEC}/query`, { method: 'POST', headers: { authorization: 'Bearer ' }, body: '{"query":"get users"}' });
  assert.equal(res.status, 401);
});

test('a guest cart is behind 128 random bits: a guessed cookie finds nothing', async () => {
  const p = await someProduct(13);
  await stock(p.sku, 50);
  const guest = new Shopper();
  await guest.add(p.sku, 1);
  const cookie = guest.jar.get('sg_guest')!;
  assert.match(cookie, /^[A-Za-z0-9_-]{22}$/, 'a guest cookie is 16 random bytes');
  for (const guess of ['1', 'admin', cookie.slice(0, 21) + (cookie[21] === 'A' ? 'B' : 'A'), 'A'.repeat(22)]) {
    const g = new Shopper();
    g.jar.set('sg_guest', guess);
    assert.equal((await g.get('/api/cart')).body.lines.length, 0, `guessed ${guess}`);
  }
  // The database holds a hash of the cookie, never the cookie.
  assert.equal((await rows('get carts where owner = $1', [`g:${cookie}`])).length, 0);
  const res = await guest.req('/api/cart', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ action: 'add', sku: p.sku, qty: 1 }) });
  assert.ok(!/sg_guest/.test(res.headers.get('set-cookie') ?? ''), 'a guest keeps its cookie');
  const made = await new Shopper().req('/api/cart', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ action: 'add', sku: p.sku, qty: 1 }) });
  assert.match(made.headers.get('set-cookie') ?? '', /HttpOnly/i);
  assert.match(made.headers.get('set-cookie') ?? '', /SameSite=Lax/i);
});

test('prices come from the catalog: a price, a total or a discount in the request is ignored', async () => {
  const p = await someProduct(14);
  await stock(p.sku, 50);
  const s = new Shopper();
  const add = await s.post('/api/cart', { action: 'add', sku: p.sku, qty: 2, price: 1, lineTotal: 2, total: 1 });
  assert.equal(add.status, 200);
  assert.equal(add.body.lines[0].price, p.price);
  assert.equal(add.body.subtotal, 2 * p.price);
  const o = await s.checkout(key(), { subtotal: 1, total: 1, shipping: 0, price: 1 });
  assert.equal(o.status, 200);
  const [order] = await rows<{ subtotal: number; total: number; shipping: number }>('get orders select subtotal, total, shipping where number = $1', [o.body.number]);
  assert.equal(order.subtotal, 2 * p.price);
  assert.equal(order.total, order.subtotal + order.shipping);
  const [line] = await rows<{ price: number }>('get order_lines select price where orderNumber = $1', [o.body.number]);
  assert.equal(line.price, p.price);
  // A quantity outside 1..20, or not a whole number, is refused.
  for (const qty of [-1, 0.5, 21, '3; del carts', 1e9]) {
    assert.equal((await s.post('/api/cart', { action: 'add', sku: p.sku, qty })).status, 400, `qty ${qty}`);
  }
  assert.equal((await s.post('/api/cart', { action: 'add', sku: 'SG-999999', qty: 1 })).status, 404);
});

test('search and filters are parameters: an injection is a search for its characters', async () => {
  const [{ count: before }] = await rows<{ count: number }>('get products count');
  const attacks = [
    '"; del products; --',
    "') or 1=1 --",
    '$1',
    'tent" or brand != "',
    'match description "x"',
    '\\"\\n del products',
    '<script>alert(1)</script>',
  ];
  for (const q of attacks) {
    const r = await fetch(`${SHOP}/api/search?q=${encodeURIComponent(q)}`);
    assert.equal(r.status, 200, q);
    const page = await (await fetch(`${SHOP}/search?q=${encodeURIComponent(q)}`)).text();
    assert.ok(!page.includes('<script>alert(1)</script>'), 'the query is escaped in the page');
  }
  // A filter value that tries to widen the filter matches no brand at all.
  const r = await (await fetch(`${SHOP}/api/c/tents?brand=${encodeURIComponent('x" or brand != "')}`)).json();
  assert.equal(r.total, 0);
  const [{ count: tents }] = await rows<{ count: number }>('get products where category = "tents" count');
  assert.equal((await (await fetch(`${SHOP}/api/c/tents?sort=${encodeURIComponent('price desc; del products')}`)).json()).total, tents);
  const [{ count: after }] = await rows<{ count: number }>('get products count');
  assert.equal(after, before);
});

test('a write sent from another site is refused', async () => {
  const p = await someProduct(15);
  const s = new Shopper();
  const r = await s.post('/api/cart', { action: 'add', sku: p.sku, qty: 1 }, { origin: 'https://evil.example' });
  assert.equal(r.status, 403);
  for (const path of ['/api/checkout', '/api/pay', '/api/account']) {
    assert.equal((await s.post(path, {}, { origin: 'https://evil.example' })).status, 403, path);
  }
  assert.equal((await fetch(`${SHOP}/api/reap`, { method: 'POST' })).status, 401);
});
