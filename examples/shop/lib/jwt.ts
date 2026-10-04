// HS256 JSON Web Tokens with node:crypto alone, signed with the secret
// fenec-server checks them with (`--jwt-secret-file`). Every token names an
// `exp`: the server refuses one without.
import { createHmac, timingSafeEqual } from 'node:crypto';

const SECRET = process.env.FENEC_JWT_SECRET ?? 'shop-dev-jwt-secret-of-at-least-32-bytes!';

const b64 = (b: Buffer | string) => Buffer.from(b).toString('base64url');

export function mint(claims: Record<string, unknown>, seconds: number, secret = SECRET): string {
  const now = Math.floor(Date.now() / 1000);
  const head = b64(JSON.stringify({ alg: 'HS256', typ: 'JWT' }));
  const body = b64(JSON.stringify({ iat: now, exp: now + seconds, ...claims }));
  const sig = createHmac('sha256', secret).update(`${head}.${body}`).digest('base64url');
  return `${head}.${body}.${sig}`;
}

/** A value signed with the shop's own secret, for its cookies. */
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
