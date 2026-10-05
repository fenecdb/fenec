// The tokens Trellis signs, RS256 with a key only this process holds; the
// nodes verify them with the public half (`--jwt-keys`), so a node, its
// disk or its backups cannot mint one.
//
// - The app's token, one a tenant (`role: app`, `tenant`): what this
//   server reads and writes with. The policy lets it do what the auth flows
//   need and nothing else -- it cannot rewrite the security log or an
//   organisation's audit trail.
// - A person's access token: their `sub`, the organisation's tenant, their
//   role as a list that holds the roles below it, and the teams they are in
//   (`teams`, a list claim the policy reads with `in $jwt.teams`). It lives
//   five minutes; the browser holds it in memory and reads and writes the
//   database with it directly.
import { randomUUID } from 'node:crypto';
import { signJwt, type SigningKey } from './jwt.ts';

export const ROLES = ['owner', 'admin', 'member', 'guest'] as const;
export type Role = (typeof ROLES)[number];

/** A role and every role below it: what the policy's `for <role>` matches. */
export function roleClaim(role: Role): Role[] {
  return ROLES.slice(ROLES.indexOf(role));
}

export const ISSUER = 'trellis';

export interface Access {
  token: string;
  /** Seconds since the epoch. */
  exp: number;
}

export class Tokens {
  #apps = new Map<string, Access>();

  constructor(
    readonly signing: SigningKey,
    readonly accessTtl: number,
  ) {}

  /**
   * The app's token for one tenant, minted for fifteen minutes and used for
   * ten: fenec-server keeps a verified token by its text, so reusing one
   * skips the RSA check (38 us) for a lookup (0.5 us).
   */
  app(tenant: string): string {
    const now = Math.floor(Date.now() / 1000);
    const kept = this.#apps.get(tenant);
    if (kept && kept.exp - now > 300) return kept.token;
    const exp = now + 900;
    const token = signJwt({ iss: ISSUER, sub: 'trellis-app', role: 'app', tenant, iat: now, exp }, this.signing);
    if (this.#apps.size > 10_000) this.#apps.clear();
    this.#apps.set(tenant, { token, exp });
    return token;
  }

  /** A person's access token for one organisation. */
  user(c: { sub: string; tenant: string; role: Role; teams: string[]; name: string }): Access {
    const now = Math.floor(Date.now() / 1000);
    const exp = now + this.accessTtl;
    const token = signJwt(
      {
        iss: ISSUER,
        sub: c.sub,
        tenant: c.tenant,
        role: roleClaim(c.role),
        teams: [...c.teams].sort(),
        name: c.name,
        iat: now,
        exp,
        jti: randomUUID(),
      },
      this.signing,
    );
    return { token, exp };
  }
}
