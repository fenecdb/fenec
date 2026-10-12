// HS256 JSON Web Tokens with node:crypto alone, signed with the secret the
// nodes check them with (`--jwt-secret-file`). Every token names its
// tenant -- a company's `co-<slug>`, or `plover` for the app's own -- and an
// `exp`: the nodes refuse a token without one, and one naming another tenant
// is 403 by every route.
import { createHmac, timingSafeEqual } from 'node:crypto';
import { JWT_SECRET, PLATFORM, tenantOf } from './config';

const b64 = (b: Buffer | string) => Buffer.from(b).toString('base64url');

export function mint(claims: Record<string, unknown>, seconds: number, secret = JWT_SECRET): string {
  const now = Math.floor(Date.now() / 1000);
  const head = b64(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const body = b64(JSON.stringify({ iat: now, exp: now + seconds, ...claims }));
  const sig = createHmac('sha256', secret).update(`${head}.${body}`).digest('base64url');
  return `${head}.${body}.${sig}`;
}

/** A value signed with a secret, for the session cookie. */
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

export type Role = 'dispatcher' | 'courier' | 'customer' | 'app';

/** Who a token speaks for: a person in their company, or the app. */
export interface Who {
  sub: string;
  role: Role;
  /** The company's slug; none for the app's token on the platform. */
  company?: string;
}

// A token is minted for ten minutes and used for five: the node keeps a
// verified token by its text, so reusing one skips its signature check.
const kept = new Map<string, { token: string; until: number }>();

export function tokenFor(who: Who, seconds = 600): string {
  const tenant = who.company ? tenantOf(who.company) : PLATFORM;
  const key = `${tenant}\u0000${who.sub}\u0000${who.role}\u0000${seconds}`;
  const now = Date.now();
  const hit = kept.get(key);
  if (hit && hit.until > now) return hit.token;
  const token = mint({ sub: who.sub, role: who.role, tenant }, seconds);
  if (kept.size > 20_000) kept.clear();
  kept.set(key, { token, until: now + (seconds * 1000) / 2 });
  return token;
}

/** The app's own token in a company's tenant, or on the platform. */
export const appToken = (company?: string) => tokenFor({ sub: 'plover-app', role: 'app', company });
