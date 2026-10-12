// Passwords as scrypt hashes, `scrypt$<N>$<salt>$<hash>`: the database
// keeps the result, never the password.
import { randomBytes, scryptSync, timingSafeEqual } from 'node:crypto';
import { SCRYPT_N } from './config';

export function hashPassword(password: string, n = SCRYPT_N): string {
  const salt = randomBytes(16);
  const hash = scryptSync(password.normalize('NFC'), salt, 32, { N: n, r: 8, p: 1, maxmem: 256 * n * 8 });
  return `scrypt$${n}$${salt.toString('base64url')}$${hash.toString('base64url')}`;
}

export function checkPassword(password: string, stored: string): boolean {
  const [kind, n, salt, hash] = stored.split('$');
  if (kind !== 'scrypt' || !n || !salt || !hash) return false;
  const N = Number(n);
  const want = Buffer.from(hash, 'base64url');
  const got = scryptSync(password.normalize('NFC'), Buffer.from(salt, 'base64url'), want.length, {
    N,
    r: 8,
    p: 1,
    maxmem: 256 * N * 8,
  });
  return got.length === want.length && timingSafeEqual(got, want);
}

/**
 * A hash to check against when no account has the email, so the answer
 * takes as long either way.
 */
export const DUMMY_HASH = hashPassword('no account has this password');
