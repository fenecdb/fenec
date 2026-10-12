// The site end to end, against the cluster and the Next.js server that
// scripts/ci.sh starts (PLOVER_SITE): sign-in by an email typed in another
// case, each role's pages, a customer's order placed and followed, a
// courier's claim, pickup and delivery through the phone's routes, and the
// refusals -- another customer's order, a write from another site.
import { before, test } from 'node:test';
import assert from 'node:assert/strict';
import { demoAccounts } from '../lib/demo';
import { DEMO_PASSWORD, COMPANIES, people } from '../lib/world';
import { routeTo } from '../lib/db';
import { ROUTER_URL } from '../lib/config';

const site = process.env.PLOVER_SITE;
const skip = site ? false : 'PLOVER_SITE is not set: scripts/ci.sh runs this against a site it starts';
const demo = demoAccounts().filter((d) => d.company === 'tandem');
const as = (role: string) => demo.find((d) => d.role === role)!;

before(() => routeTo(ROUTER_URL));

async function signIn(email: string, password = DEMO_PASSWORD): Promise<{ cookie: string; to: string }> {
  const res = await fetch(`${site}/api/signin`, {
    method: 'POST',
    redirect: 'manual',
    headers: { 'content-type': 'application/x-www-form-urlencoded', origin: site! },
    body: new URLSearchParams({ email, password }),
  });
  assert.equal(res.status, 303);
  const cookie = (res.headers.get('set-cookie') ?? '').split(';')[0];
  return { cookie, to: new URL(res.headers.get('location')!).pathname + new URL(res.headers.get('location')!).search };
}

const page = (path: string, cookie: string) => fetch(`${site}${path}`, { headers: { cookie }, redirect: 'manual' });
const api = (path: string, cookie: string, body: object) =>
  fetch(`${site}${path}`, { method: 'POST', headers: { cookie, 'content-type': 'application/json', origin: site! }, body: JSON.stringify(body) });

test('sign in with the email in another case; a wrong password is refused', { skip }, async () => {
  const d = as('dispatcher');
  assert.notEqual(d.email, d.email.toLowerCase(), 'the demo types it in another case');
  const { cookie, to } = await signIn(d.email);
  assert.equal(to, '/dispatch');
  assert.ok(cookie.startsWith('plover='));
  const wrong = await signIn(d.email, 'not the password');
  assert.equal(wrong.to, '/signin?failed=1');
});

test("a dispatcher's map and insights; another role's page sends them home", { skip }, async () => {
  const { cookie } = await signIn(as('dispatcher').email);
  const map = await page('/dispatch', cookie);
  assert.equal(map.status, 200);
  assert.match(await map.text(), /couriers on shift/);
  const ins = await page('/insights', cookie);
  assert.equal(ins.status, 200);
  assert.match(await ins.text(), /Deliveries an hour/);
  const other = await page('/courier', cookie);
  assert.equal(other.status, 307);
  assert.equal(new URL(other.headers.get('location')!, site).pathname, '/dispatch');
  const token = await (await page('/api/token', cookie)).json();
  assert.match(token.url, /\/t\/co-tandem$/);
});

test("a customer orders, follows it, and cannot open another customer's", { skip }, async () => {
  const { cookie } = await signIn(as('customer').email);
  const list = await page('/orders', cookie);
  assert.equal(list.status, 200);
  const html = await list.text();
  const merchant = /name="merchant" value="(M\d+)"/.exec(html)?.[1];
  assert.ok(merchant, 'a merchant near them');
  const placed = await fetch(`${site}/api/orders`, {
    method: 'POST',
    redirect: 'manual',
    headers: { cookie, 'content-type': 'application/x-www-form-urlencoded', origin: site! },
    body: new URLSearchParams({ merchant }),
  });
  assert.equal(placed.status, 303);
  const at = new URL(placed.headers.get('location')!).pathname;
  assert.match(at, /^\/orders\/\d+$/);
  assert.equal((await page(at, cookie)).status, 200);
  // Another customer of the same company, the same order: not there for them.
  const other = people(COMPANIES[0]).filter((p) => p.role === 'customer')[5];
  const theirs = await signIn(other.email);
  assert.equal((await page(at, theirs.cookie)).status, 404);
});

test("a courier's phone claims, picks up and delivers through the site", { skip }, async () => {
  // An order placed at a merchant, and the courier standing at its door.
  const k = await signIn(as('customer').email);
  const html = await (await page('/orders', k.cookie)).text();
  const merchant = /name="merchant" value="(M\d+)"/.exec(html)![1];
  const shop = (await import('../lib/world')).merchants(COMPANIES[0]).find((m) => m.code === merchant)!;
  await fetch(`${site}/api/orders`, {
    method: 'POST',
    redirect: 'manual',
    headers: { cookie: k.cookie, 'content-type': 'application/x-www-form-urlencoded', origin: site! },
    body: new URLSearchParams({ merchant }),
  });
  const c = await signIn(as('courier').email);
  const claimed = await (await api('/api/courier/claim', c.cookie, { at: shop.loc })).json();
  assert.ok(claimed.job, 'an order near them');
  const job = { id: claimed.job.id, claim: claimed.job.claim };
  assert.deepEqual(await (await api('/api/courier/beat', c.cookie, { at: shop.loc, state: 'pickup', job })).json(), { kept: true });
  assert.deepEqual(await (await api('/api/courier/pickup', c.cookie, job)).json(), { ok: true });
  assert.deepEqual(await (await api('/api/courier/deliver', c.cookie, { ...job, driven: 1500 })).json(), { ok: true });
  // Delivered once: a second ack finds the order no longer theirs to ack.
  assert.deepEqual(await (await api('/api/courier/deliver', c.cookie, { ...job, driven: 1500 })).json(), { ok: false });
  const shift = await page('/courier', c.cookie);
  assert.match(await shift.text(), new RegExp(shop.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
});

test('a write from another site is refused', { skip }, async () => {
  const c = await signIn(as('courier').email);
  const res = await fetch(`${site}/api/courier/beat`, {
    method: 'POST',
    headers: { cookie: c.cookie, 'content-type': 'application/json', origin: 'https://elsewhere.example' },
    body: JSON.stringify({ at: [27, 38.36], state: 'idle' }),
  });
  assert.equal(res.status, 403);
});
