// "Continue with single sign-on": the authorization code flow with PKCE
// against an OpenID Connect provider, and its ID token verified here --
// RS256 against the provider's published keys, the issuer, the audience,
// the expiry and the nonce this server sent. The provider's keys are
// fetched from its JWKS and again when a token names a key not seen yet
// (the provider rotated), at most once a minute.
import { createHash, randomBytes } from 'node:crypto';
import { JwtError, verifyJwt, type Jwk } from './jwt.ts';

interface Pending {
  nonce: string;
  verifier: string;
  at: number;
}

export class Oidc {
  #pending = new Map<string, Pending>();
  #keys: Jwk[] = [];
  #fetchedAt = 0;

  constructor(
    readonly issuer: string,
    readonly clientId: string,
    readonly redirect: string,
  ) {}

  /** Where to send the browser, and the state to keep in its cookie. */
  start(): { location: string; state: string } {
    const state = randomBytes(18).toString('base64url');
    const nonce = randomBytes(18).toString('base64url');
    const verifier = randomBytes(32).toString('base64url');
    const now = Date.now();
    for (const [k, p] of this.#pending) if (now - p.at > 600_000) this.#pending.delete(k);
    this.#pending.set(state, { nonce, verifier, at: now });
    const u = new URL(`${this.issuer}/authorize`);
    u.search = new URLSearchParams({
      response_type: 'code',
      client_id: this.clientId,
      redirect_uri: this.redirect,
      scope: 'openid email profile',
      state,
      nonce,
      code_challenge: createHash('sha256').update(verifier).digest('base64url'),
      code_challenge_method: 'S256',
    }).toString();
    return { location: u.toString(), state };
  }

  /**
   * The code exchanged and the ID token verified. `cookieState` is what the
   * browser's cookie holds: a callback whose state is not the one this
   * browser started is refused, so nobody can sign a victim into the
   * attacker's account by sending them a link.
   */
  async finish(code: string, state: string, cookieState: string | undefined): Promise<{ sub: string; email: string; verified: boolean; name: string }> {
    const p = this.#pending.get(state);
    this.#pending.delete(state);
    if (!p || state !== cookieState || Date.now() - p.at > 600_000) throw new JwtError('state does not match');
    const res = await fetch(`${this.issuer}/token`, {
      method: 'POST',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({ grant_type: 'authorization_code', code, redirect_uri: this.redirect, client_id: this.clientId, code_verifier: p.verifier }),
    });
    if (!res.ok) throw new JwtError(`token endpoint: ${res.status}`);
    const { id_token } = (await res.json()) as { id_token?: string };
    if (!id_token) throw new JwtError('no ID token');
    const claims = await this.verify(id_token);
    if (claims.nonce !== p.nonce) throw new JwtError('nonce does not match');
    return {
      sub: String(claims.sub),
      email: String(claims.email ?? ''),
      verified: claims.email_verified === true,
      name: String(claims.name ?? ''),
    };
  }

  async verify(idToken: string): Promise<Record<string, unknown>> {
    if (!this.#keys.length) await this.#fetchKeys();
    try {
      return verifyJwt(idToken, this.#keys, { iss: this.issuer, aud: this.clientId });
    } catch (e) {
      if (e instanceof JwtError && e.message === 'no key for this token' && Date.now() - this.#fetchedAt > 60_000) {
        await this.#fetchKeys();
        return verifyJwt(idToken, this.#keys, { iss: this.issuer, aud: this.clientId });
      }
      throw e;
    }
  }

  async #fetchKeys(): Promise<void> {
    const res = await fetch(`${this.issuer}/jwks`);
    if (!res.ok) throw new JwtError(`jwks: ${res.status}`);
    this.#keys = ((await res.json()) as { keys: Jwk[] }).keys;
    this.#fetchedAt = Date.now();
  }
}
