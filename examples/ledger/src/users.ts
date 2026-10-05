// Who signs in to the console. A password is kept as scrypt's output and
// its salt; a check takes the same time whether or not the name exists.
import { randomBytes, scryptSync, timingSafeEqual } from 'node:crypto';
import type { Store } from './store.ts';

export interface User {
  name: string;
  display: string;
  role: 'admin' | 'customer';
}

export function hashPassword(password: string): string {
  const salt = randomBytes(16);
  return `${salt.toString('base64url')}.${scryptSync(password, salt, 32).toString('base64url')}`;
}

const DUMMY = hashPassword('not a password anyone has');

export async function signIn(store: Store, name: string, password: string): Promise<User | null> {
  const [u] = await store.rows<User & { hash: string }>('get users where name = $1 limit 1', [name]);
  const [salt, want] = (u?.hash ?? DUMMY).split('.');
  const got = scryptSync(password, Buffer.from(salt, 'base64url'), 32);
  const ok = timingSafeEqual(got, Buffer.from(want, 'base64url'));
  return u && ok ? { name: u.name, display: u.display, role: u.role } : null;
}

/** The accounts a customer holds, alone or jointly: the list claim of their token. */
export async function holdings(store: Store, name: string): Promise<string[]> {
  const rows = await store.rows<{ ext: string }>('get accounts select ext where holders has $1', [name]);
  return rows.map((r) => r.ext);
}
