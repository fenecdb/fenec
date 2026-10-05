// HS256 JSON Web Tokens, signed with the secret fenec-server checks them
// with (`--jwt-secret-file`), with node:crypto alone. Every token names an
// `exp` (the server refuses one without) and a `tenant` (on a --dir node a
// token reaches only the tenant its claim names).
import { createHmac, timingSafeEqual } from 'node:crypto';

export const JWT_SECRET = process.env.FENEC_JWT_SECRET ?? 'ledger-dev-jwt-secret-of-at-least-32-bytes';

const b64 = (b: Buffer | string) => Buffer.from(b).toString('base64url');

export function mint(claims: Record<string, unknown>, seconds: number, secret = JWT_SECRET): string {
  const now = Math.floor(Date.now() / 1000);
  const head = b64(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const body = b64(JSON.stringify({ iat: now, exp: now + seconds, ...claims }));
  const sig = createHmac('sha256', secret).update(`${head}.${body}`).digest('base64url');
  return `${head}.${body}.${sig}`;
}

/**
 * Tokens for one tenant, each minted for ten minutes and used for five:
 * the server keeps a verified token by its text, so reusing one skips its
 * signature check.
 */
export class Tokens {
  #kept = new Map<string, { token: string; until: number }>();

  constructor(readonly tenant: string) {}

  /** The ledger's server: moves money (policy.txt, `for app`). */
  app(): string {
    return this.#get({ sub: 'ledger-app', role: 'app' });
  }

  /** An operator in the console: reads everything, writes nothing. */
  admin(sub: string): string {
    return this.#get({ sub, role: 'admin' });
  }

  /** A customer: the accounts they hold, joint ones included, as a list claim. */
  customer(sub: string, accounts: string[]): string {
    return this.#get({ sub, accounts: [...accounts].sort() });
  }

  #get(claims: Record<string, unknown>): string {
    const full = { ...claims, tenant: this.tenant };
    const key = JSON.stringify(full);
    const now = Date.now();
    const kept = this.#kept.get(key);
    if (kept && kept.until > now) return kept.token;
    const token = mint(full, 600);
    if (this.#kept.size > 10_000) this.#kept.clear();
    this.#kept.set(key, { token, until: now + 300_000 });
    return token;
  }
}

/** A value signed with the console's own secret, for its session cookie. */
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
