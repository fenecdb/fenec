// What the tests share: a shopper as a browser would be one -- its own
// cookies, sent with every request -- and fenec-server reached directly.
// They run against a shop and a server already started (scripts/ci.sh).
import { mint } from '../lib/jwt';

export const SHOP = (process.env.SHOP_URL ?? 'http://localhost:3000').replace(/\/$/, '');
export const FENEC = (process.env.FENEC_URL ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
export const ROOT = process.env.FENEC_TOKEN ?? 'shop-dev-token';
export const ADMIN = process.env.SHOP_ADMIN_TOKEN ?? 'shop-dev-admin';

export class Shopper {
  jar = new Map<string, string>();

  async req(path: string, init: RequestInit = {}): Promise<Response> {
    const headers = new Headers(init.headers);
    if (this.jar.size) headers.set('cookie', [...this.jar].map(([k, v]) => `${k}=${v}`).join('; '));
    if (init.method && init.method !== 'GET' && !headers.has('origin')) headers.set('origin', SHOP);
    const res = await fetch(SHOP + path, { ...init, headers, redirect: 'manual' });
    for (const c of res.headers.getSetCookie()) {
      const [pair] = c.split(';');
      const eq = pair.indexOf('=');
      const name = pair.slice(0, eq);
      const value = pair.slice(eq + 1);
      if (value === '' || /max-age=0|expires=thu, 01 jan 1970/i.test(c)) this.jar.delete(name);
      else this.jar.set(name, value);
    }
    return res;
  }

  async post(path: string, body: unknown, headers: Record<string, string> = {}) {
    const res = await this.req(path, { method: 'POST', headers: { 'content-type': 'application/json', accept: 'application/json', ...headers }, body: JSON.stringify(body) });
    const text = await res.text();
    return { status: res.status, body: text ? JSON.parse(text) : null };
  }

  async get(path: string) {
    const res = await this.req(path, { headers: { accept: 'application/json' } });
    const text = await res.text();
    return { status: res.status, body: text && res.headers.get('content-type')?.includes('json') ? JSON.parse(text) : text };
  }

  add(sku: string, qty = 1) {
    return this.post('/api/cart', { action: 'add', sku, qty });
  }

  checkout(key: string, extra: Record<string, unknown> = {}) {
    return this.post('/api/checkout', { key, email: 'a@example.com', name: 'Ada Test', line1: '1 Dune Road', city: 'Tozeur', postcode: '2200', country: 'Tunisia', ...extra });
  }

  static async signedUp(name: string): Promise<Shopper> {
    const s = new Shopper();
    const email = `${name}-${crypto.randomUUID()}@example.com`;
    const r = await s.post('/api/account', { action: 'signup', name, email, password: 'correct horse battery' });
    if (r.status !== 200) throw new Error(`sign up: ${r.status} ${JSON.stringify(r.body)}`);
    return s;
  }
}

/** FenecQL straight to fenec-server, with the server's token or a JWT. */
export async function fenec(query: string, params: unknown[] = [], token = ROOT): Promise<{ status: number; body: unknown }> {
  const res = await fetch(`${FENEC}/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
    body: JSON.stringify({ query, params }),
  });
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

export async function rows<T = Record<string, unknown>>(query: string, params: unknown[] = []): Promise<T[]> {
  const r = await fenec(query, params);
  if (r.status !== 200) throw new Error(`${query}: ${r.status} ${JSON.stringify(r.body)}`);
  const b = r.body as T[] | { rows: T[] };
  return Array.isArray(b) ? b : b.rows;
}

/** A token as the shop mints them, for a subject of the test's choosing. */
export function tokenAs(sub: string, role?: string) {
  return mint({ sub, ...(role ? { role } : {}) }, 600, process.env.FENEC_JWT_SECRET ?? 'shop-dev-jwt-secret-of-at-least-32-bytes!');
}

/** A product that is in stock, by sku, with its catalog price. */
export async function someProduct(offset = 0): Promise<{ sku: string; price: number; slug: string; name: string }> {
  const [p] = await rows<{ sku: string; price: number; slug: string; name: string }>(
    `get products select sku, price, slug, name where category = "water-bottles" order id offset ${Math.floor(offset)} limit 1`,
  );
  return p;
}

export async function stock(sku: string, available: number) {
  await rows('set inventory {available: $1, reserved: 0, sold: 0} where sku = $2', [available, sku]);
}

export const key = () => crypto.randomUUID().replace(/-/g, '');
