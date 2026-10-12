// Who is signed in: a signed cookie holding the account's id, looked up in
// the platform's tenant. Every page and route reaches the company's data
// with that person's own token (lib/db.ts `asUser`), so the database --
// not this code -- holds them to their role's rows.
import 'server-only';
import { cookies } from 'next/headers';
import { redirect } from 'next/navigation';
import { COOKIE_SECRET, INSECURE_COOKIES, PUBLIC_ROUTER_URL, tenantOf } from './config';
import { one, platformDb, routeTo } from './db';
import { checkPassword, DUMMY_HASH } from './passwords';
import { sign, tokenFor, unsign, type Who } from './tokens';
import type { User } from './schema';
import { ROUTER_URL } from './config';

routeTo(ROUTER_URL);

export const COOKIE = 'plover';

export interface Account {
  uid: string;
  email: string;
  name: string;
  company: string;
  role: 'dispatcher' | 'courier' | 'customer';
  code: string | null;
}

const kept = new Map<string, { a: Account; until: number }>();

async function account(uid: string): Promise<Account | null> {
  const hit = kept.get(uid);
  if (hit && hit.until > Date.now()) return hit.a;
  const u = await one<User>(platformDb(), 'get users select uid, email, name, company, role, code where uid = $1', [uid]);
  if (!u) return null;
  const a = u as unknown as Account;
  kept.set(uid, { a, until: Date.now() + 30_000 });
  return a;
}

/**
 * An account by its email and password. The email is compared by its case
 * fold (`email text nocase @unique`), so `Sami.Lindqvist@Tandem.example`
 * finds `sami.lindqvist@tandem.example`; an unknown email is checked against
 * a dummy hash, so the answer takes as long either way.
 */
export async function signIn(email: string, password: string): Promise<Account | null> {
  const u = await one<User>(platformDb(), 'get users where email = $1', [email.trim().normalize('NFC')]);
  const ok = checkPassword(password, u?.password ?? DUMMY_HASH);
  return u && ok ? (u as unknown as Account) : null;
}

export async function startSession(a: Account): Promise<void> {
  (await cookies()).set(COOKIE, sign(a.uid, COOKIE_SECRET), {
    httpOnly: true,
    sameSite: 'lax',
    secure: !INSECURE_COOKIES,
    path: '/',
    maxAge: 12 * 3600,
  });
}

export async function endSession(): Promise<void> {
  (await cookies()).delete(COOKIE);
}

export async function current(): Promise<Account | null> {
  const uid = unsign((await cookies()).get(COOKIE)?.value, COOKIE_SECRET);
  return uid ? account(uid) : null;
}

/** The signed-in account in `role`, or off to sign in. */
export async function need(role: Account['role']): Promise<Account> {
  const a = await current();
  if (!a) redirect('/signin');
  if (a.role !== role) redirect(home(a));
  return a;
}

export const whoOf = (a: Account): Who => ({ sub: a.uid, role: a.role, company: a.company });

export const home = (a: Account) => ({ dispatcher: '/dispatch', courier: '/courier', customer: '/orders' })[a.role];

/**
 * What a page's script needs to reach the router itself: a token of five
 * minutes for the person, and where their company's tenant is. The page
 * asks for another before it lapses (`/api/token`).
 */
export function browserAccess(a: Account): { token: string; url: string; expires: number } {
  return {
    token: tokenFor(whoOf(a), 300),
    url: `${PUBLIC_ROUTER_URL}/t/${tenantOf(a.company)}`,
    expires: Date.now() + 150_000,
  };
}
