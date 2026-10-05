// RS256 JSON Web Tokens with node:crypto alone: signing the access tokens
// fenec-server checks (it holds only the public key, from the JWKS file
// --jwt-keys reads), and verifying the identity provider's ID tokens.
import { createPrivateKey, createPublicKey, generateKeyPairSync, sign, verify, type KeyObject } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const b64 = (b: Buffer | string) => Buffer.from(b).toString('base64url');

export interface SigningKey {
  kid: string;
  key: KeyObject;
}

export function signJwt(claims: Record<string, unknown>, k: SigningKey): string {
  const head = b64(JSON.stringify({ alg: 'RS256', typ: 'JWT', kid: k.kid }));
  const body = b64(JSON.stringify(claims));
  const sig = sign('sha256', Buffer.from(`${head}.${body}`), k.key);
  return `${head}.${body}.${b64(sig)}`;
}

export interface Jwk {
  kty?: string;
  n?: string;
  e?: string;
  kid?: string;
  alg?: string;
  use?: string;
}

/**
 * A key pair in `dir`: the private key as PEM (0600) and the public one as
 * a JWKS file. Made once and kept, so tokens survive a restart; a key named
 * `kid` is made if the file has none.
 */
export function loadOrMakeKey(dir: string, name: string, kid: string): { signing: SigningKey; jwks: { keys: Jwk[] } } {
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  const pem = join(dir, `${name}.key.pem`);
  const jwksFile = join(dir, `${name}.jwks.json`);
  if (!existsSync(pem)) {
    const { privateKey, publicKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
    writeFileSync(pem, privateKey.export({ type: 'pkcs8', format: 'pem' }), { mode: 0o600 });
    const jwk = { ...(publicKey.export({ format: 'jwk' }) as Jwk), kid, alg: 'RS256', use: 'sig' };
    writeFileSync(jwksFile, JSON.stringify({ keys: [jwk] }, null, 2));
  }
  const jwks = JSON.parse(readFileSync(jwksFile, 'utf8')) as { keys: Jwk[] };
  return { signing: { kid: jwks.keys[0].kid!, key: createPrivateKey(readFileSync(pem)) }, jwks };
}

export class JwtError extends Error {}

/**
 * Verifies an RS256 token against a key set: the algorithm is the key's,
 * never the token's (so `alg: none` and an HS256 token "signed" with the
 * public key are refused), a `kid` picks its key and an unknown one is
 * refused, and `exp` is required.
 */
export function verifyJwt(
  token: string,
  keys: Jwk[],
  want: { iss?: string; aud?: string; now?: number; leeway?: number } = {},
): Record<string, unknown> {
  const parts = token.split('.');
  if (parts.length !== 3) throw new JwtError('not a JWT');
  let head: { alg?: string; kid?: string };
  let claims: Record<string, unknown>;
  try {
    head = JSON.parse(Buffer.from(parts[0], 'base64url').toString());
    claims = JSON.parse(Buffer.from(parts[1], 'base64url').toString());
  } catch {
    throw new JwtError('not a JWT');
  }
  if (head.alg !== 'RS256') throw new JwtError(`algorithm ${String(head.alg)} refused`);
  const rsa = keys.filter((k) => k.kty === 'RSA' && (!k.alg || k.alg === 'RS256'));
  const candidates = head.kid ? rsa.filter((k) => k.kid === head.kid) : rsa;
  if (!candidates.length) throw new JwtError('no key for this token');
  const data = Buffer.from(`${parts[0]}.${parts[1]}`);
  const sig = Buffer.from(parts[2], 'base64url');
  const ok = candidates.some((k) => verify('sha256', data, createPublicKey({ key: k, format: 'jwk' }), sig));
  if (!ok) throw new JwtError('bad signature');
  const now = want.now ?? Math.floor(Date.now() / 1000);
  const leeway = want.leeway ?? 30;
  if (typeof claims.exp !== 'number') throw new JwtError('no exp');
  if (claims.exp + leeway < now) throw new JwtError('expired');
  if (typeof claims.nbf === 'number' && claims.nbf - leeway > now) throw new JwtError('not yet valid');
  if (want.iss !== undefined && claims.iss !== want.iss) throw new JwtError('wrong issuer');
  if (want.aud !== undefined) {
    const aud = claims.aud;
    if (!(aud === want.aud || (Array.isArray(aud) && aud.includes(want.aud)))) throw new JwtError('wrong audience');
  }
  return claims;
}

/** The claims of a token, unverified: for display and tests only. */
export function peek(token: string): Record<string, unknown> {
  return JSON.parse(Buffer.from(token.split('.')[1] ?? '', 'base64url').toString() || '{}');
}
