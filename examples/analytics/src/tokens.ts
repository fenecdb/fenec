// HS256 JSON Web Tokens, signed with the secret fenec-server checks them
// with (`--jwt-secret-file`), with node:crypto alone. Every token names an
// `exp` (the server refuses one without) and a `tenant`: on a --dir node a
// token reaches only the tenant its claim names.
import { createHmac, timingSafeEqual } from 'node:crypto';
import { JWT_SECRET } from './config.ts';

const b64 = (b: Buffer | string) => Buffer.from(b).toString('base64url');

export function mint(claims: Record<string, unknown>, seconds: number, secret = JWT_SECRET): string {
  const now = Math.floor(Date.now() / 1000);
  const head = b64(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const body = b64(JSON.stringify({ iat: now, exp: now + seconds, ...claims }));
  const sig = createHmac('sha256', secret).update(`${head}.${body}`).digest('base64url');
  return `${head}.${body}.${sig}`;
}

export type Role = 'viewer' | 'ingest' | 'feed' | 'app';

/**
 * Tokens minted for ten minutes and used for five: the server keeps a
 * verified token by its text, so reusing one skips its signature check.
 */
export class Tokens {
  #kept = new Map<string, { token: string; until: number }>();

  /** `role` on `tenant`, for `sub`. */
  get(tenant: string, role: Role, sub = `kestrel-${role}`): string {
    const claims = { sub, role, tenant };
    const key = JSON.stringify(claims);
    const now = Date.now();
    const kept = this.#kept.get(key);
    if (kept && kept.until > now) return kept.token;
    const token = mint(claims, 600);
    if (this.#kept.size > 10_000) this.#kept.clear();
    this.#kept.set(key, { token, until: now + 300_000 });
    return token;
  }
}

export const tokens = new Tokens();

/** A value signed with the dashboard's own secret, for its session cookie. */
export function sign(value: string, secret: string): string {
  return `${value}.${createHmac('sha256', secret).update(value).digest('base64url')}`;
}

export function unsign(signed: string | undefined, secret: string): string | null {
  if (!signed) return null;
  const dot = signed.lastIndexOf('.');
  if (dot < 0) return null;
  const value = signed.slice(0, dot);
  const want = Buffer.from(createHmac('sha256', secret).update(value).digest('base64url'));
  const got = Buffer.from(signed.slice(dot + 1));
  return want.length === got.length && timingSafeEqual(want, got) ? value : null;
}
