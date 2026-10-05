// Passwords and the secrets mailed to people, all handled here: fenecdb
// stores what this produces and never sees a password.
import { createHash, randomBytes, scrypt as scryptCb, timingSafeEqual } from 'node:crypto';
import { promisify } from 'node:util';

const scrypt = promisify(scryptCb) as (pw: string, salt: Buffer, len: number, opts: object) => Promise<Buffer>;

/**
 * scrypt with r = 8, p = 1 and a 16-byte salt, kept as
 * `scrypt$<log2 N>$<salt>$<hash>` so the cost can rise later and old hashes
 * still verify. On the libuv thread pool: a sign-in does not hold the
 * event loop for its 50 ms.
 */
export async function hashPassword(password: string, N: number): Promise<string> {
  const salt = randomBytes(16);
  const out = await scrypt(password.normalize('NFC'), salt, 32, { N, r: 8, p: 1, maxmem: 256 * N * 8 + 1024 * 1024 });
  return `scrypt$${Math.log2(N)}$${salt.toString('base64url')}$${out.toString('base64url')}`;
}

/** Whether `password` is the one `stored` was made from, in constant time. */
export async function checkPassword(password: string, stored: string): Promise<boolean> {
  const [kind, logN, salt, want] = stored.split('$');
  if (kind !== 'scrypt' || !salt || !want) return false;
  const N = 2 ** Number(logN);
  const got = await scrypt(password.normalize('NFC'), Buffer.from(salt, 'base64url'), 32, {
    N,
    r: 8,
    p: 1,
    maxmem: 256 * N * 8 + 1024 * 1024,
  });
  const w = Buffer.from(want, 'base64url');
  return w.length === got.length && timingSafeEqual(got, w);
}

/**
 * An email as the unique index will hold it. `@unique` compares bytes, so
 * "Alice@X.io" and "alice@x.io" would be two accounts unless they are made
 * one here: Unicode NFC (an é typed as e and a combining accent is the é
 * typed whole), surrounding space trimmed, and lowercased.
 */
export function normaliseEmail(raw: string): string | null {
  const e = raw.normalize('NFC').trim().toLowerCase();
  if (e.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/u.test(e)) return null;
  return e;
}

export function passwordProblem(password: string): string | null {
  if (password.length < 10) return 'Use at least 10 characters.';
  if (password.length > 512) return 'Use at most 512 characters.';
  return null;
}

/** A secret to mail or set in a cookie: 32 random bytes. */
export function secret(): string {
  return randomBytes(32).toString('base64url');
}

/** What the database keeps of a secret: its SHA-256, so a copy of the file signs nobody in. */
export function digest(s: string): string {
  return createHash('sha256').update(s).digest('base64url');
}

/** An identifier: a prefix and 12 random bytes. */
export function newId(prefix: string): string {
  return `${prefix}_${randomBytes(12).toString('base64url').replace(/[-_]/g, 'x')}`;
}
