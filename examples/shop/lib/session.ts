// Who is shopping. A signed-in shopper is `u:<user id>`, from a signed
// session cookie; a guest is `g:<hash of the guest cookie>`. The guest
// cookie is 128 random bits and httpOnly, so a cart cannot be had by
// guessing an id; and since only its hash is stored, the database never
// holds the value that opens the cart.
import 'server-only';
import { createHash, randomBytes, scryptSync, timingSafeEqual } from 'node:crypto';
import { cookies } from 'next/headers';
import { sign, unsign } from './jwt';

const SECRET = process.env.SHOP_SECRET ?? 'shop-dev-session-secret-change-me';
export const SESSION = 'sg_session';
export const GUEST = 'sg_guest';
const WEEK = 7 * 86_400;

export function guestOwner(token: string): string {
  return `g:${createHash('sha256').update(token).digest('base64url').slice(0, 32)}`;
}

export interface Shopper {
  owner: string;
  userId: number | null;
  name: string | null;
  email: string | null;
}

/** The shopper this request is from, or null when it carries no cookie. */
export async function shopper(): Promise<Shopper | null> {
  const jar = await cookies();
  const session = unsign(jar.get(SESSION)?.value, SECRET);
  if (session) {
    try {
      const s = JSON.parse(Buffer.from(session, 'base64url').toString()) as { uid: number; name: string; email: string; exp: number };
      if (s.exp > Date.now() / 1000) return { owner: `u:${s.uid}`, userId: s.uid, name: s.name, email: s.email };
    } catch {
      // a cookie that does not parse is no session
    }
  }
  const guest = jar.get(GUEST)?.value;
  if (guest && /^[A-Za-z0-9_-]{22}$/.test(guest)) return { owner: guestOwner(guest), userId: null, name: null, email: null };
  return null;
}

/** The shopper, a guest made now if the request has no cookie. Route handlers and actions only. */
export async function shopperOrNew(): Promise<Shopper> {
  const s = await shopper();
  if (s) return s;
  const token = randomBytes(16).toString('base64url');
  (await cookies()).set(GUEST, token, { httpOnly: true, sameSite: 'lax', secure: process.env.NODE_ENV === 'production' && !process.env.SHOP_INSECURE_COOKIES, path: '/', maxAge: WEEK });
  return { owner: guestOwner(token), userId: null, name: null, email: null };
}

export async function signIn(user: { id: number; name: string; email: string }) {
  const value = Buffer.from(JSON.stringify({ uid: user.id, name: user.name, email: user.email, exp: Math.floor(Date.now() / 1000) + WEEK })).toString('base64url');
  (await cookies()).set(SESSION, sign(value, SECRET), { httpOnly: true, sameSite: 'lax', secure: process.env.NODE_ENV === 'production' && !process.env.SHOP_INSECURE_COOKIES, path: '/', maxAge: WEEK });
}

export async function signOut() {
  (await cookies()).delete(SESSION);
}

export function hashPassword(password: string): string {
  const salt = randomBytes(16);
  return `${salt.toString('base64url')}.${scryptSync(password, salt, 32).toString('base64url')}`;
}

export function checkPassword(password: string, stored: string): boolean {
  const [salt, hash] = stored.split('.');
  if (!salt || !hash) return false;
  const got = scryptSync(password, Buffer.from(salt, 'base64url'), 32);
  const want = Buffer.from(hash, 'base64url');
  return got.length === want.length && timingSafeEqual(got, want);
}
